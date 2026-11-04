//! Authenticated Drive requests outside google-drive3's generated calls: media downloads and
//! exports now, gdrive's own resumable uploader later. They go through the hub's HTTP client
//! (and so its connection pool), get their access token from the hub's token source the way
//! google-drive3 does, and follow the hub's retry policy and throttle (`retry.rs`).

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
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
        classify_status, classify_transport, message_of_error_body, Decision, DisplayDuration,
        RetryState, StopReason,
    },
    hub::Hub,
};

const USER_AGENT_VALUE: &str = concat!("gdrive/", env!("CARGO_PKG_VERSION"));

/// The most of an error reply that is read for its JSON error.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Sends requests with the hub's client and credentials.
#[derive(Clone)]
pub(crate) struct MediaClient<'a> {
    hub: &'a Hub,
}

impl<'a> MediaClient<'a> {
    #[must_use]
    pub(crate) fn new(hub: &'a Hub) -> Self {
        MediaClient { hub }
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
                if let Some(message) = message {
                    write!(f, ": {}", message.trim_end())?;
                }
                match stop {
                    Some(StopReason::Unauthorized) => {
                        write!(f, " ({})", StopReason::Unauthorized)
                    }
                    Some(reason @ StopReason::GaveUp { .. }) => write!(f, " ({reason})"),
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

    use super::{MediaClient, MediaError};
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
