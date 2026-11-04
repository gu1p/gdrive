//! Authenticated Drive requests outside google-drive3's generated calls: media downloads and
//! exports, and gdrive's own uploader (`transfer/upload.rs`). They go through the hub's HTTP
//! client (and so its connection pool), get their access token from the hub's token source the
//! way google-drive3 does, and follow the hub's retry policy and throttle (`retry.rs`).
//!
//! A file shared by link only has a `resourceKey`, which every request about it must name in the
//! `X-Goog-Drive-Resource-Keys` header ([`RESOURCE_KEYS_HEADER`]): a client made
//! [`MediaClient::with_resource_key`] sends it, and [`get_file`] makes `files.get` with it, which
//! google-drive3 cannot.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
};

use google_apis_common::Body;
use http_body_util::BodyExt;
use hyper::{
    body::Incoming,
    header::{HeaderValue, AUTHORIZATION, USER_AGENT},
    Request, Response, StatusCode,
};

use crate::{
    common::retry::{
        classify_status, classify_transport, message_of_error_body, retry_read, ApiDelegate,
        Decision, DisplayDuration, RetryState, StopReason,
    },
    hub::Hub,
    // Warnings go above the progress bars.
    transfer::{progress::eprintln, walk::RESOURCE_KEYS_HEADER},
};

const USER_AGENT_VALUE: &str = concat!("gdrive/", env!("CARGO_PKG_VERSION"));

/// The most of an error reply that is read for its JSON error.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// The 403 reason of a download of a file Google flagged as malware or spam.
const ABUSIVE_FILE_REASON: &str = "cannotDownloadAbusiveFile";

/// What to do about [`ABUSIVE_FILE_REASON`]: only the owner of the file, or an organizer of its
/// shared drive, can acknowledge the risk.
const ABUSIVE_FILE_HINT: &str = "; rerun with --acknowledge-abuse if you own the file";

/// Sends requests with the hub's client and credentials.
#[derive(Clone)]
pub(crate) struct MediaClient<'a> {
    hub: &'a Hub,
    /// The value of [`RESOURCE_KEYS_HEADER`] every authorized request carries, if any.
    resource_keys: Option<HeaderValue>,
}

impl<'a> MediaClient<'a> {
    #[must_use]
    pub(crate) fn new(hub: &'a Hub) -> Self {
        MediaClient {
            hub,
            resource_keys: None,
        }
    }

    /// The client for requests about the file `file_id` with `resource_key`, which it names in
    /// every request it authorizes (not in those to a session URL). Without a key, or with one
    /// that cannot be a header, the client is unchanged.
    #[must_use]
    pub(crate) fn with_resource_key(mut self, file_id: &str, resource_key: Option<&str>) -> Self {
        self.resource_keys = resource_keys_value(file_id, resource_key);
        self
    }

    /// The URL of the path `segments` (percent-encoded) under the hub's Drive base URL, for
    /// example `["files", id]` for `https://www.googleapis.com/drive/v3/files/<id>`, with `query`.
    pub(crate) fn drive_url(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<url::Url, MediaError> {
        let mut url = url::Url::parse(self.hub.drive_base_url()).map_err(MediaError::Url)?;
        url.path_segments_mut()
            .map_err(|()| MediaError::Url(url::ParseError::RelativeUrlWithCannotBeABaseBase))?
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    /// The URL of the path `segments` under the hub's media upload URL, for example `["files"]`
    /// for `https://www.googleapis.com/upload/drive/v3/files`, with `query`.
    pub(crate) fn upload_url(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<url::Url, MediaError> {
        let drive_base_url = self.hub.drive_base_url();
        // Google serves uploads under `upload/` next to the API: `<root>drive/v3/` becomes
        // `<root>upload/drive/v3/`.
        let root = drive_base_url
            .strip_suffix("drive/v3/")
            .unwrap_or(drive_base_url);
        let mut url = url::Url::parse(root)
            .and_then(|root| root.join("upload/drive/v3/"))
            .map_err(MediaError::Url)?;
        url.path_segments_mut()
            .map_err(|()| MediaError::Url(url::ParseError::RelativeUrlWithCannotBeABaseBase))?
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    /// Sends one request with the access token (full Drive scope) and gdrive's user agent.
    /// Every reply is returned, whatever its status; only a request without reply is an error.
    pub(crate) async fn send(
        &self,
        mut request: Request<Body>,
    ) -> Result<Response<Incoming>, MediaError> {
        let scopes = [google_drive3::api::Scope::Full.as_ref()];
        let token = self
            .hub
            .auth
            .get_token(&scopes)
            .await
            .map_err(MediaError::Token)?;
        if let Some(token) = token {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| MediaError::InvalidToken)?;
            value.set_sensitive(true);
            request.headers_mut().insert(AUTHORIZATION, value);
        }
        if let Some(keys) = &self.resource_keys {
            request
                .headers_mut()
                .insert(RESOURCE_KEYS_HEADER, keys.clone());
        }
        self.send_without_token(request).await
    }

    /// Sends one request with gdrive's user agent but without the access token, for a URL that
    /// authorizes the request itself: the session URL of a resumable upload. Such a request
    /// does not depend on the token, so a token that expires during a long upload, or a token
    /// refresh that fails, does not interrupt it.
    pub(crate) async fn send_without_token(
        &self,
        mut request: Request<Body>,
    ) -> Result<Response<Incoming>, MediaError> {
        request
            .headers_mut()
            .entry(USER_AGENT)
            .or_insert(HeaderValue::from_static(USER_AGENT_VALUE));

        self.hub
            .client
            .request(request)
            .await
            .map_err(|source| MediaError::Transport { source, stop: None })
    }

    /// Sends the request `make` builds, again per `retry` while it fails with a retryable error,
    /// waiting for the hub's throttle before each attempt.
    ///
    /// Returned: a 2xx reply, or a 416, which only a `Range` request gets and its caller handles.
    /// A reply with another status is retried or becomes [`MediaError::Status`]; a request
    /// without reply is retried or becomes [`MediaError::Transport`]. `label` names the transfer
    /// in the warning printed before each retry.
    pub(crate) async fn send_with_retry(
        &self,
        retry: &mut RetryState,
        label: &str,
        mut make: impl FnMut() -> Result<Request<Body>, hyper::http::Error>,
    ) -> Result<Response<Incoming>, MediaError> {
        loop {
            self.hub.retrier().throttle.wait().await;

            let request = make().map_err(MediaError::Request)?;
            let (decision, error) = match self.send(request).await {
                Ok(response)
                    if response.status().is_success()
                        || response.status() == StatusCode::RANGE_NOT_SATISFIABLE =>
                {
                    return Ok(response);
                }
                Ok(response) => {
                    let (parts, body) = response.into_parts();
                    let body = read_error_body(body).await;
                    let json = body
                        .as_deref()
                        .and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok());
                    let decision =
                        classify_status(parts.status, Some(&parts.headers), json.as_ref());
                    let message = json
                        .as_ref()
                        .and_then(message_of_error_body)
                        .map(str::to_owned);
                    let error = MediaError::Status {
                        status: parts.status,
                        message,
                        stop: None,
                    };
                    (decision, error)
                }
                Err(MediaError::Transport { source, .. }) => (
                    classify_transport(&source),
                    MediaError::Transport { source, stop: None },
                ),
                Err(error) => return Err(error),
            };

            match retry.next(decision) {
                Decision::Retry { after, reason } => {
                    eprintln!(
                        "Warning: {label}: request failed ({reason}), retrying in {} (attempt {} \
                         of {})",
                        DisplayDuration(after),
                        retry.failures() + 1,
                        retry.policy().max_attempts,
                    );
                    tokio::time::sleep(after).await;
                }
                Decision::Stop { reason } => return Err(error.stopped(reason)),
            }
        }
    }
}

/// The [`RESOURCE_KEYS_HEADER`] value naming `resource_key` of `file_id`: `<id>/<key>`.
fn resource_keys_value(file_id: &str, resource_key: Option<&str>) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!("{file_id}/{}", resource_key?)).ok()
}

/// The most of a `files.get` reply that is read.
const MAX_FILE_BODY: usize = 1 << 20;

/// `files.get` of `file_id` with the `fields` mask (`supportsAllDrives`), through the hub's retry
/// policy as a read ([`retry_read`]). With `resource_key`, the request names it in the
/// [`RESOURCE_KEYS_HEADER`]: it is built on the hub's client and credentials then, with
/// google-drive3's error mapping (a JSON error reply is `BadRequest`), since google-drive3 cannot
/// send the header.
pub(crate) async fn get_file(
    hub: &Hub,
    file_id: &str,
    fields: &str,
    resource_key: Option<&str>,
) -> Result<google_drive3::api::File, Box<google_drive3::Error>> {
    let keys = resource_keys_value(file_id, resource_key);
    let keys = keys.as_ref();
    let result = retry_read(hub.retrier(), "files.get", |mut delegate| async move {
        match keys {
            None => hub
                .files()
                .get(file_id)
                .param("fields", fields)
                .supports_all_drives(true)
                .add_scope(google_drive3::api::Scope::Full)
                .delegate(&mut delegate)
                .doit()
                .await
                .map(|(_, file)| file),
            Some(keys) => get_file_with_keys(hub, file_id, fields, keys, &delegate).await,
        }
    })
    .await;
    result.map_err(Box::new)
}

/// One `files.get` with the [`RESOURCE_KEYS_HEADER`] `keys`. A failed reply's status and headers
/// go to `delegate`, as google-drive3 hands them to its delegate, so that the retry policy sees
/// them.
#[expect(
    clippy::result_large_err,
    reason = "google-drive3's error, as the call it stands in for returns it"
)]
async fn get_file_with_keys(
    hub: &Hub,
    file_id: &str,
    fields: &str,
    keys: &HeaderValue,
    delegate: &ApiDelegate,
) -> google_drive3::Result<google_drive3::api::File> {
    use google_drive3::Error;

    let client = MediaClient {
        hub,
        resource_keys: Some(keys.clone()),
    };
    let url = client
        .drive_url(
            &["files", file_id],
            &[
                ("fields", fields),
                ("supportsAllDrives", "true"),
                ("alt", "json"),
            ],
        )
        .map_err(|error| Error::Io(io::Error::other(error)))?;
    let request = Request::get(url.as_str())
        .body(google_apis_common::to_body::<String>(None))
        .map_err(|error| Error::Io(io::Error::other(error)))?;
    let response = match client.send(request).await {
        Ok(response) => response,
        Err(MediaError::Transport { source, .. }) => return Err(Error::HttpError(source)),
        Err(MediaError::Token(source)) => return Err(Error::MissingToken(source)),
        Err(error) => return Err(Error::Io(io::Error::other(error))),
    };
    let (parts, body) = response.into_parts();
    let bytes = http_body_util::Limited::new(body, MAX_FILE_BODY)
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    if !parts.status.is_success() {
        delegate.record_failure(parts.status, &parts.headers);
        return Err(match serde_json::from_slice(&bytes) {
            Ok(value) => Error::BadRequest(value),
            Err(_) => Error::Failure(Response::from_parts(
                parts,
                google_apis_common::to_body(Some(bytes)),
            )),
        });
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        Error::JsonDecodeError(String::from_utf8_lossy(&bytes).into_owned(), error)
    })
}

/// At most [`MAX_ERROR_BODY`] bytes of an error reply; `None` if it cannot be read.
async fn read_error_body(body: Incoming) -> Option<Vec<u8>> {
    let limited = http_body_util::Limited::new(body, MAX_ERROR_BODY);
    limited
        .collect()
        .await
        .ok()
        .map(|collected| collected.to_bytes().to_vec())
}

/// A failed media request.
#[derive(Debug)]
pub(crate) enum MediaError {
    /// The request URL could not be built.
    Url(url::ParseError),
    /// The request could not be built.
    Request(hyper::http::Error),
    /// No access token; the source says why (for example that the account has to be authorized
    /// again).
    Token(Box<dyn Error + Send + Sync>),
    /// The access token cannot be sent in a header.
    InvalidToken,
    /// The request got no reply. `stop` says why it is not retried (any more).
    Transport {
        source: hyper_util::client::legacy::Error,
        stop: Option<StopReason>,
    },
    /// A reply with a failure status, or one a download cannot use. `message` is Google's,
    /// `stop` says why it is not retried (any more).
    Status {
        status: StatusCode,
        message: Option<String>,
        stop: Option<StopReason>,
    },
    /// A download could not continue where it stopped.
    Resume { reason: StopReason },
    /// The file's metadata could not be fetched to check that it did not change.
    Metadata(Box<google_drive3::Error>),
}

impl MediaError {
    fn stopped(self, reason: StopReason) -> Self {
        match self {
            MediaError::Transport { source, .. } => MediaError::Transport {
                source,
                stop: Some(reason),
            },
            MediaError::Status {
                status, message, ..
            } => MediaError::Status {
                status,
                message,
                stop: Some(reason),
            },
            other => other,
        }
    }
}

impl Display for MediaError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::Url(_) => f.write_str("invalid request URL"),
            MediaError::Request(_) => f.write_str("invalid request"),
            MediaError::Token(_) => f.write_str("unable to get an access token"),
            MediaError::InvalidToken => f.write_str("the access token is not a valid header value"),
            MediaError::Transport { stop, .. } => {
                f.write_str("the request got no reply")?;
                match stop {
                    Some(reason @ (StopReason::GaveUp { .. } | StopReason::Tls)) => {
                        write!(f, " ({reason})")
                    }
                    _ => Ok(()),
                }
            }
            MediaError::Status {
                status,
                message,
                stop,
            } => {
                write!(f, "Drive answered {status}")?;
                let abusive = matches!(
                    stop,
                    Some(StopReason::Forbidden { reason: Some(reason) })
                        if reason == ABUSIVE_FILE_REASON
                );
                if let Some(message) = message {
                    let message = message.trim_end();
                    // The hint follows; Google's message ends with a period.
                    let message = if abusive {
                        message.trim_end_matches('.')
                    } else {
                        message
                    };
                    write!(f, ": {message}")?;
                }
                match stop {
                    Some(StopReason::Unauthorized) => {
                        write!(f, " ({})", StopReason::Unauthorized)
                    }
                    Some(reason @ StopReason::GaveUp { .. }) => write!(f, " ({reason})"),
                    _ if abusive => f.write_str(ABUSIVE_FILE_HINT),
                    _ => Ok(()),
                }
            }
            MediaError::Resume { reason } => {
                write!(f, "the download could not be resumed ({reason})")
            }
            MediaError::Metadata(_) => {
                f.write_str("unable to check whether the file changed on Drive")
            }
        }
    }
}

impl Error for MediaError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            MediaError::Url(source) => Some(source),
            MediaError::Request(source) => Some(source),
            MediaError::Token(source) => Some(source.as_ref()),
            MediaError::Transport { source, .. } => Some(source),
            MediaError::Metadata(source) => Some(source),
            MediaError::InvalidToken | MediaError::Status { .. } | MediaError::Resume { .. } => {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{get_file, MediaClient, MediaError};
    use crate::common::{
        mock_server::{MockServer, Response},
        retry::{RetryPolicy, StopReason},
    };

    fn get(url: &url::Url) -> Result<hyper::Request<google_apis_common::Body>, hyper::http::Error> {
        hyper::Request::get(url.as_str()).body(google_apis_common::to_body::<String>(None))
    }

    #[tokio::test]
    async fn requests_carry_the_token_and_the_user_agent() {
        let server = MockServer::start(vec![Response::raw(200, "content")]).await;
        let hub = server.hub();
        let client = MediaClient::new(&hub);
        let url = client
            .drive_url(&["files", "id/with space"], &[("alt", "media")])
            .unwrap();
        let mut retry = hub.retrier().state();

        let response = client
            .send_with_retry(&mut retry, "test", || get(&url))
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let request = &server.requests()[0];
        assert_eq!(request.path, "/drive/v3/files/id%2Fwith%20space");
        assert_eq!(request.query_param("alt"), Some("media"));
        assert_eq!(request.header("authorization"), Some("Bearer mock-token"));
        assert!(request
            .header("user-agent")
            .is_some_and(|agent| agent.starts_with("gdrive/")));
    }

    #[tokio::test]
    async fn requests_about_a_file_shared_by_link_name_its_resource_key() {
        let server = MockServer::start(vec![
            Response::raw(200, "content"),
            Response::json(
                503,
                &json!({"error": {"code": 503, "message": "Backend Error"}}),
            ),
            Response::json(200, &json!({"id": "f", "size": "7"})),
            Response::json(
                404,
                &json!({"error": {"code": 404, "message": "File not found: f."}}),
            ),
        ])
        .await;
        let hub = server.hub();
        let client = MediaClient::new(&hub).with_resource_key("f", Some("key"));
        let url = client
            .drive_url(&["files", "f"], &[("alt", "media")])
            .unwrap();
        let mut retry = hub.retrier().state();
        client
            .send_with_retry(&mut retry, "test", || get(&url))
            .await
            .unwrap();

        // files.get, which google-drive3 cannot send with the header: retried like its calls.
        let file = get_file(&hub, "f", "id,size", Some("key")).await.unwrap();
        assert_eq!(file.size, Some(7));
        let error = get_file(&hub, "f", "id,size", Some("key"))
            .await
            .unwrap_err();
        assert!(
            matches!(&*error, google_drive3::Error::BadRequest(json)
                if json["error"]["message"] == "File not found: f."),
            "{error:?}"
        );

        let requests = server.requests();
        assert_eq!(requests.len(), 4, "the 503 is retried, the 404 is not");
        for request in &requests {
            assert_eq!(request.header("x-goog-drive-resource-keys"), Some("f/key"));
            assert_eq!(request.header("authorization"), Some("Bearer mock-token"));
        }
        assert_eq!(requests[1].path, "/drive/v3/files/f");
        assert_eq!(requests[1].query_param("supportsAllDrives"), Some("true"));
        assert_eq!(
            requests[1]
                .query_param("fields")
                .map(|fields| fields.replace("%2C", ",")),
            Some(String::from("id,size"))
        );

        // Without a key, no header.
        let plain = MediaClient::new(&hub).with_resource_key("f", None);
        assert!(plain.resource_keys.is_none());
    }

    #[tokio::test]
    async fn transient_failures_are_retried_and_others_are_not() {
        let server = MockServer::start(vec![
            Response::json(
                503,
                &json!({"error": {"code": 503, "message": "Backend Error"}}),
            ),
            Response::raw(200, "content"),
            Response::json(
                404,
                &json!({"error": {"code": 404, "message": "File not found: x."}}),
            ),
        ])
        .await;
        let hub = server.hub();
        let client = MediaClient::new(&hub);
        let url = client.drive_url(&["files", "x"], &[]).unwrap();

        let mut retry = hub.retrier().state();
        let response = client
            .send_with_retry(&mut retry, "test", || get(&url))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let mut retry = hub.retrier().state();
        let error = client
            .send_with_retry(&mut retry, "test", || get(&url))
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                MediaError::Status { status, stop: Some(StopReason::NotFound), .. } if *status == 404
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "Drive answered 404 Not Found: File not found: x."
        );
        assert_eq!(server.requests().len(), 3);
    }

    #[tokio::test]
    async fn the_budget_ends_retries_with_the_last_failure() {
        let replies = (0..10)
            .map(|_| Response::json(503, &json!({"error": {"code": 503, "message": "Busy"}})))
            .collect();
        let server = MockServer::start(replies).await;
        let hub = server.hub();
        let client = MediaClient::new(&hub);
        let url = client.drive_url(&["files", "x"], &[]).unwrap();
        let mut retry = hub.retrier().state();

        let error = client
            .send_with_retry(&mut retry, "test", || get(&url))
            .await
            .unwrap_err();

        let attempts = RetryPolicy::for_tests().max_attempts;
        assert_eq!(server.requests().len(), usize::try_from(attempts).unwrap());
        let message = error.to_string();
        assert!(
            message.starts_with(
                "Drive answered 503 Service Unavailable: Busy (gave up after 4 attempts"
            ),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_file_flagged_as_abusive_points_at_acknowledge_abuse() {
        let server = MockServer::start(vec![Response::json(
            403,
            &json!({"error": {
                "code": 403,
                "message": "This file has been identified as malware or spam and cannot be \
                            downloaded.",
                "errors": [{
                    "domain": "global",
                    "reason": "cannotDownloadAbusiveFile",
                    "message": "This file has been identified as malware or spam and cannot \
                                be downloaded.",
                }],
            }}),
        )])
        .await;
        let hub = server.hub();
        let client = MediaClient::new(&hub);
        let url = client
            .drive_url(&["files", "x"], &[("alt", "media")])
            .unwrap();
        let mut retry = hub.retrier().state();

        let error = client
            .send_with_retry(&mut retry, "test", || get(&url))
            .await
            .unwrap_err();

        assert_eq!(server.requests().len(), 1, "not retried");
        assert_eq!(
            error.to_string(),
            "Drive answered 403 Forbidden: This file has been identified as malware or spam and \
             cannot be downloaded; rerun with --acknowledge-abuse if you own the file"
        );
    }

    #[tokio::test]
    async fn other_refusals_get_no_abuse_hint() {
        let server = MockServer::start(vec![Response::json(
            403,
            &json!({"error": {
                "code": 403,
                "message": "The user does not have sufficient permissions for this file.",
                "errors": [{"domain": "global", "reason": "insufficientFilePermissions"}],
            }}),
        )])
        .await;
        let hub = server.hub();
        let client = MediaClient::new(&hub);
        let url = client.drive_url(&["files", "x"], &[]).unwrap();
        let mut retry = hub.retrier().state();

        let error = client
            .send_with_retry(&mut retry, "test", || get(&url))
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Drive answered 403 Forbidden: The user does not have sufficient permissions for \
             this file."
        );
    }

    #[tokio::test]
    async fn unauthorized_points_at_reauth() {
        let server = MockServer::start(vec![Response::json(
            401,
            &json!({"error": {"code": 401, "message": "Invalid Credentials"}}),
        )])
        .await;
        let hub = server.hub();
        let client = MediaClient::new(&hub);
        let url = client.drive_url(&["files", "x"], &[]).unwrap();
        let mut retry = hub.retrier().state();

        let error = client
            .send_with_retry(&mut retry, "test", || get(&url))
            .await
            .unwrap_err();

        assert_eq!(server.requests().len(), 1);
        assert!(
            error.to_string().contains("gdrive account reauth"),
            "{error}"
        );
    }
}
