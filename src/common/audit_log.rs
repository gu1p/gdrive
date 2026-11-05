//! The Drive audit log of a Google Workspace organization: `activities.list` of the Admin SDK
//! Reports API for the application `drive`, which has a record of every view, download and edit
//! per person. `gdrive files activity` reads it.
//!
//! Requests go through the hub's HTTP client (and so its connection pool and proxy), with the
//! account's token for [`auth::AUDIT_LOG_SCOPE`] and gdrive's user agent, under the hub's retry
//! policy and throttle as [`crate::common::media_client`] sends Drive requests: a rate limit
//! holds back every request of the hub, a server error is retried, and Google's JSON error is
//! kept, with a hint for the failures an administrator runs into first (a token without the
//! scope, the API not enabled, an account that is no administrator, a filter Google refuses).
//! Neither the token nor a request URL is ever part of an error.
//!
//! The records are read leniently: a field of an unexpected type is empty rather than an error,
//! and unknown fields and event names are kept or ignored, never refused.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
};

use chrono::{DateTime, SecondsFormat, Utc};
use http_body_util::{BodyExt, Limited};
use hyper::{
    header::{HeaderValue, AUTHORIZATION},
    Request, StatusCode,
};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::{
    auth,
    common::{
        json_error::RedactedJsonError,
        local_name::printable,
        media_client::{MediaClient, MediaError},
        retry::{
            classify_body_error, classify_status, classify_transport, is_reason,
            message_of_error_body, Decision, DisplayDuration, StopReason,
        },
    },
    hub::Hub,
    // Warnings go above the progress bars.
    transfer::progress::eprintln,
};

/// The most records one page holds, and what every request asks for.
pub(crate) const MAX_RESULTS: u32 = 1000;

/// The path of `activities.list` for Drive below the Reports API base URL.
const ACTIVITIES_PATH: [&str; 5] = ["activity", "users", "all", "applications", "drive"];

/// The name of the call in retry warnings.
const CALL: &str = "activities.list";

/// The most of a page that is read: 1000 records are a few MB.
const MAX_PAGE_BODY: usize = 64 << 20;

/// The most of an error reply that is read for its JSON error.
const MAX_ERROR_BODY: usize = 64 << 10;

/// The event parameter that names the item an event is about.
pub(crate) const DOC_ID: &str = "doc_id";

/// The event parameter that is false for the secondary events of one user action.
pub(crate) const PRIMARY_EVENT: &str = "primary_event";

/// The event parameter that names the shared drive of the item.
pub(crate) const SHARED_DRIVE_ID: &str = "shared_drive_id";

/// What Google selects on the server: Drive events of one item, or of one shared drive.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Filter {
    /// `doc_id==<ID>`.
    DocId(String),
    /// `shared_drive_id==<ID>`.
    SharedDriveId(String),
}

impl Display for Filter {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Filter::DocId(id) => write!(f, "{DOC_ID}=={id}"),
            Filter::SharedDriveId(id) => write!(f, "{SHARED_DRIVE_ID}=={id}"),
        }
    }
}

/// One series of `activities.list` requests: the events from `start` to `end`, all of the
/// organization's or those `filter` selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActivityQuery {
    pub(crate) start: DateTime<Utc>,
    pub(crate) end: DateTime<Utc>,
    pub(crate) filter: Option<Filter>,
}

/// One page of records.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Activities {
    /// Google leaves it out of an empty page.
    #[serde(default)]
    pub(crate) items: Vec<Activity>,
    #[serde(default, deserialize_with = "optional_text")]
    pub(crate) next_page_token: Option<String>,
}

/// One record: what one person did at one time, as one or more events.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Activity {
    #[serde(default)]
    pub(crate) id: ActivityId,
    #[serde(default)]
    pub(crate) actor: Actor,
    #[serde(default)]
    pub(crate) events: Vec<Event>,
}

/// What identifies a record: its time and Google's qualifier for records of the same time.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivityId {
    /// RFC 3339.
    #[serde(default, deserialize_with = "text")]
    pub(crate) time: String,
    /// A number, which Google sends as a string.
    #[serde(default, deserialize_with = "text")]
    pub(crate) unique_qualifier: String,
}

/// Who did it.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Actor {
    /// Missing or empty for people who were not signed in, or outside the organization.
    #[serde(default, deserialize_with = "optional_text")]
    pub(crate) email: Option<String>,
}

/// One event of a record.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Event {
    /// `view`, `download`, `edit`, ...; new names appear over time.
    #[serde(default, deserialize_with = "text")]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) parameters: Vec<Parameter>,
}

impl Event {
    fn parameter(&self, name: &str) -> Option<&Parameter> {
        self.parameters
            .iter()
            .find(|parameter| parameter.name == name)
    }

    /// The `value` of the parameter `name`.
    #[must_use]
    pub(crate) fn value(&self, name: &str) -> Option<&str> {
        self.parameter(name)?.value.as_deref()
    }

    /// The `boolValue` of the parameter `name`.
    #[must_use]
    pub(crate) fn bool_value(&self, name: &str) -> Option<bool> {
        self.parameter(name)?.bool_value
    }
}

/// A parameter of an event. Google sends its value in one of `value`, `boolValue`, `intValue`
/// and `multiValue`; the events of interest here use the first two.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Parameter {
    #[serde(default, deserialize_with = "text")]
    pub(crate) name: String,
    #[serde(default, deserialize_with = "optional_text")]
    pub(crate) value: Option<String>,
    #[serde(default, deserialize_with = "optional_bool")]
    pub(crate) bool_value: Option<bool>,
}

/// A string, or a number or boolean as text; empty for anything else.
fn text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(optional_text(deserializer)?.unwrap_or_default())
}

/// A string, or a number or boolean as text; `None` for an empty string and anything else.
fn optional_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) if !text.is_empty() => Some(text),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    })
}

/// A boolean, or `"true"` or `"false"`; `None` for anything else.
fn optional_bool<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Bool(value) => Some(value),
        Value::String(text) => text.parse().ok(),
        _ => None,
    })
}

/// Reads the audit log with the hub of the account `account`.
pub(crate) struct AuditLog<'a> {
    hub: &'a Hub,
    /// As `gdrive account list` prints it, for the hints of errors.
    account: String,
}

impl<'a> AuditLog<'a> {
    #[must_use]
    pub(crate) fn new(hub: &'a Hub, account: &str) -> Self {
        AuditLog {
            hub,
            account: account.to_owned(),
        }
    }

    /// The URL of one page of `query`.
    fn url(
        &self,
        query: &ActivityQuery,
        page_token: Option<&str>,
    ) -> Result<url::Url, AuditLogError> {
        let mut url = url::Url::parse(self.hub.reports_base_url()).map_err(AuditLogError::Url)?;
        url.path_segments_mut()
            .map_err(|()| AuditLogError::Url(url::ParseError::RelativeUrlWithCannotBeABaseBase))?
            .pop_if_empty()
            .extend(ACTIVITIES_PATH);
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("startTime", &rfc3339(query.start))
                .append_pair("endTime", &rfc3339(query.end))
                .append_pair("maxResults", &MAX_RESULTS.to_string());
            if let Some(filter) = &query.filter {
                pairs.append_pair("filters", &filter.to_string());
            }
            if let Some(token) = page_token {
                pairs.append_pair("pageToken", token);
            }
        }
        Ok(url)
    }

    /// One page of `query`: the first without `page_token`, else the one it names. Retried per
    /// the hub's policy while it fails with a retryable error, waiting for the hub's throttle
    /// before each attempt.
    pub(crate) async fn page(
        &self,
        query: &ActivityQuery,
        page_token: Option<&str>,
    ) -> Result<Activities, AuditLogError> {
        let url = self.url(query, page_token)?;
        let client = MediaClient::new(self.hub);
        let mut retry = self.hub.retrier().state();
        loop {
            self.hub.retrier().throttle.wait().await;

            let request = self.authorized(&url).await?;
            let (decision, error) = match client.send_without_token(request).await {
                Ok(response) if response.status().is_success() => {
                    match Limited::new(response.into_body(), MAX_PAGE_BODY)
                        .collect()
                        .await
                    {
                        Ok(body) => {
                            return serde_json::from_slice(&body.to_bytes()).map_err(|error| {
                                AuditLogError::Decode(RedactedJsonError::from(&error))
                            });
                        }
                        Err(source) => {
                            let decision = source.downcast_ref::<hyper::Error>().map_or(
                                Decision::Stop {
                                    reason: StopReason::Local,
                                },
                                classify_body_error,
                            );
                            (decision, AuditLogError::Body { source, stop: None })
                        }
                    }
                }
                Ok(response) => {
                    let (parts, body) = response.into_parts();
                    let json = Limited::new(body, MAX_ERROR_BODY)
                        .collect()
                        .await
                        .ok()
                        .and_then(|body| serde_json::from_slice::<Value>(&body.to_bytes()).ok());
                    let decision =
                        classify_status(parts.status, Some(&parts.headers), json.as_ref());
                    let error = AuditLogError::Status {
                        status: parts.status,
                        message: json
                            .as_ref()
                            .and_then(message_of_error_body)
                            .map(|message| printable(message).into_owned()),
                        stop: None,
                        hint: Hint::of(
                            parts.status,
                            json.as_ref(),
                            query.filter.is_some(),
                            &self.account,
                        ),
                    };
                    (decision, error)
                }
                Err(MediaError::Transport { source, .. }) => (
                    classify_transport(&source),
                    AuditLogError::Transport { source, stop: None },
                ),
                Err(error) => return Err(AuditLogError::Send(Box::new(error))),
            };

            match retry.next(decision) {
                Decision::Retry { after, reason } => {
                    eprintln!(
                        "Warning: {CALL} failed ({reason}), retrying in {} (attempt {} of {})",
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

    /// A GET of `url` with the audit-log token, which is marked sensitive (never logged).
    async fn authorized(
        &self,
        url: &url::Url,
    ) -> Result<Request<google_apis_common::Body>, AuditLogError> {
        let mut request = Request::get(url.as_str())
            .body(google_apis_common::to_body::<String>(None))
            .map_err(AuditLogError::Request)?;
        let token = self
            .hub
            .auth
            .get_token(&[auth::AUDIT_LOG_SCOPE])
            .await
            .map_err(AuditLogError::Token)?;
        if let Some(token) = token {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| AuditLogError::InvalidToken)?;
            value.set_sensitive(true);
            request.headers_mut().insert(AUTHORIZATION, value);
        }
        Ok(request)
    }
}

/// A time as the Reports API takes it: RFC 3339 in UTC with milliseconds.
#[must_use]
pub(crate) fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// What the user can do about a failed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hint {
    /// The token does not carry the audit-log scope: the account was authorized without it.
    AuthorizeAuditLog { account: String },
    /// The Admin SDK API is not enabled in the Cloud project of the OAuth client.
    EnableApi,
    /// The Google account is no administrator with the Reports privilege.
    NotAdministrator,
    /// Google did not accept the token at all.
    Reauthorize { account: String },
    /// Google refused a request with a filter.
    ScanDomain,
}

impl Hint {
    /// The hint for a reply with `status` and the JSON error `error`, to a request with a
    /// `filters` parameter if `filtered`, of the account `account`.
    #[must_use]
    pub(crate) fn of(
        status: StatusCode,
        error: Option<&Value>,
        filtered: bool,
        account: &str,
    ) -> Option<Hint> {
        let reasons = error_reasons(error);
        let has_reason = |known: &[&str]| {
            reasons
                .iter()
                .any(|reason| known.iter().any(|known| is_reason(reason, known)))
        };
        let message = error
            .and_then(message_of_error_body)
            .unwrap_or_default()
            .to_lowercase();
        let account = account.to_owned();
        let denied = status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN;

        if status == StatusCode::FORBIDDEN
            && (has_reason(&["ACCESS_TOKEN_SCOPE_INSUFFICIENT"])
                || message.contains("insufficient authentication scopes"))
        {
            Some(Hint::AuthorizeAuditLog { account })
        } else if status == StatusCode::FORBIDDEN
            && has_reason(&["accessNotConfigured", "SERVICE_DISABLED"])
        {
            Some(Hint::EnableApi)
        } else if denied
            && (message.contains("not authorized to read activity records")
                || message.contains("access denied"))
        {
            Some(Hint::NotAdministrator)
        } else if status == StatusCode::UNAUTHORIZED {
            Some(Hint::Reauthorize { account })
        } else if status == StatusCode::BAD_REQUEST && filtered {
            Some(Hint::ScanDomain)
        } else {
            None
        }
    }
}

impl Display for Hint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Hint::AuthorizeAuditLog { account } => write!(
                f,
                "the account is not authorized to read the audit log; run 'gdrive account reauth \
                 {} --audit-log'",
                printable(account)
            ),
            Hint::EnableApi => f.write_str(
                "enable the Admin SDK API in the Google Cloud project of the account's OAuth \
                 client (Google's message names the project and has the link), wait a few \
                 minutes and run the command again",
            ),
            Hint::NotAdministrator => f.write_str(
                "the Google account must be an administrator of the Google Workspace \
                 organization, with the Reports privilege; personal Gmail accounts have no audit \
                 log",
            ),
            Hint::Reauthorize { account } => write!(
                f,
                "run 'gdrive account reauth {} --audit-log' to authorize the account again",
                printable(account)
            ),
            Hint::ScanDomain => f.write_str(
                "Google refused the filter of this scan; run the command again with --scan domain",
            ),
        }
    }
}

/// The `reason`s of a Google JSON error body: `error.errors[].reason`, and
/// `error.details[].reason` of the newer format.
fn error_reasons(error: Option<&Value>) -> Vec<&str> {
    let Some(error) = error.and_then(|body| body.get("error")) else {
        return Vec::new();
    };
    ["errors", "details"]
        .iter()
        .filter_map(|key| error.get(key)?.as_array())
        .flatten()
        .filter_map(|entry| entry.get("reason")?.as_str())
        .collect()
}

/// A failed request to the audit log.
#[derive(Debug)]
pub(crate) enum AuditLogError {
    /// The request URL could not be built.
    Url(url::ParseError),
    /// The request could not be built.
    Request(hyper::http::Error),
    /// No access token; the source says why (for example that the account has to be authorized
    /// again).
    Token(Box<dyn Error + Send + Sync>),
    /// The access token cannot be sent in a header.
    InvalidToken,
    /// The request could not be sent.
    Send(Box<MediaError>),
    /// The request got no reply. `stop` says why it is not retried (any more).
    Transport {
        source: hyper_util::client::legacy::Error,
        stop: Option<StopReason>,
    },
    /// The reply broke off, or was too large.
    Body {
        source: Box<dyn Error + Send + Sync>,
        stop: Option<StopReason>,
    },
    /// A reply with a failure status. `message` is Google's, control characters escaped.
    Status {
        status: StatusCode,
        message: Option<String>,
        stop: Option<StopReason>,
        hint: Option<Hint>,
    },
    /// The reply is not a page of records.
    Decode(RedactedJsonError),
}

impl AuditLogError {
    fn stopped(self, reason: StopReason) -> Self {
        match self {
            AuditLogError::Transport { source, .. } => AuditLogError::Transport {
                source,
                stop: Some(reason),
            },
            AuditLogError::Body { source, .. } => AuditLogError::Body {
                source,
                stop: Some(reason),
            },
            AuditLogError::Status {
                status,
                message,
                hint,
                ..
            } => AuditLogError::Status {
                status,
                message,
                stop: Some(reason),
                hint,
            },
            other => other,
        }
    }
}

/// ` (gave up after ...)` for a stop after retries, ` (TLS)` for a connection that could not be
/// secured.
fn stop_suffix(f: &mut Formatter<'_>, stop: Option<&StopReason>) -> fmt::Result {
    match stop {
        Some(reason @ (StopReason::GaveUp { .. } | StopReason::Tls)) => write!(f, " ({reason})"),
        _ => Ok(()),
    }
}

impl Display for AuditLogError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            AuditLogError::Url(_) => f.write_str("invalid request URL"),
            AuditLogError::Request(_) => f.write_str("invalid request"),
            AuditLogError::Token(_) => f.write_str("unable to get an access token"),
            AuditLogError::InvalidToken => {
                f.write_str("the access token is not a valid header value")
            }
            AuditLogError::Send(_) => f.write_str("unable to send the request"),
            AuditLogError::Transport { stop, .. } => {
                f.write_str("the request got no reply")?;
                stop_suffix(f, stop.as_ref())
            }
            AuditLogError::Body { stop, .. } => {
                f.write_str("the reply broke off")?;
                stop_suffix(f, stop.as_ref())
            }
            AuditLogError::Status {
                status,
                message,
                stop,
                hint,
            } => {
                write!(f, "Google answered {status}")?;
                if let Some(message) = message {
                    write!(f, ": {}", message.trim_end())?;
                }
                stop_suffix(f, stop.as_ref())?;
                if let Some(hint) = hint {
                    write!(f, "\nHint: {hint}")?;
                }
                Ok(())
            }
            AuditLogError::Decode(error) => {
                write!(f, "the reply is not a page of the audit log ({error})")
            }
        }
    }
}

impl Error for AuditLogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            AuditLogError::Url(source) => Some(source),
            AuditLogError::Request(source) => Some(source),
            AuditLogError::Token(source) | AuditLogError::Body { source, .. } => {
                Some(source.as_ref())
            }
            AuditLogError::Send(source) => Some(source.as_ref()),
            AuditLogError::Transport { source, .. } => Some(source),
            AuditLogError::InvalidToken
            | AuditLogError::Status { .. }
            | AuditLogError::Decode(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use hyper::StatusCode;
    use serde_json::json;

    use super::{Activities, ActivityQuery, AuditLog, Filter, Hint, PRIMARY_EVENT};
    use crate::transfer::fake_drive::{Behavior, FakeDrive};

    fn time(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn google_error(code: u16, message: &str, reason: &str) -> serde_json::Value {
        json!({"error": {
            "code": code,
            "message": message,
            "errors": [{"domain": "global", "reason": reason, "message": message}],
        }})
    }

    #[test]
    fn records_are_read_leniently() {
        let page: Activities = serde_json::from_value(json!({
            "kind": "admin#reports#activities",
            "items": [
                {
                    "id": {"time": "2026-11-04T14:02:11.123Z", "uniqueQualifier": "-123"},
                    "actor": {"email": "ana@example.com", "profileId": "1"},
                    "events": [{"type": "access", "name": "view", "parameters": [
                        {"name": "doc_id", "value": "1AbC"},
                        {"name": "primary_event", "boolValue": true},
                        {"name": "visibility", "value": 7},
                        {"name": "billable", "boolValue": "false"},
                        {"name": "owner_is_team_drive", "multiValue": ["a"]},
                    ]}],
                },
                {"id": {"time": "2026-11-04T14:02:11Z", "uniqueQualifier": 5}, "actor": {"email": ""}},
                {"id": {}, "actor": {"callerType": "KEY"}, "events": [{"parameters": []}]},
            ],
        }))
        .unwrap();
        assert!(page.next_page_token.is_none());
        let [first, second, third] = page.items.as_slice() else {
            panic!("three records");
        };
        assert_eq!(first.id.unique_qualifier, "-123");
        assert_eq!(first.actor.email.as_deref(), Some("ana@example.com"));
        let event = &first.events[0];
        assert_eq!(event.name, "view");
        assert_eq!(event.value("doc_id"), Some("1AbC"));
        assert_eq!(event.bool_value(PRIMARY_EVENT), Some(true));
        assert_eq!(event.value("visibility"), Some("7"));
        assert_eq!(event.bool_value("billable"), Some(false));
        assert_eq!(event.value("missing"), None);
        assert_eq!(second.id.unique_qualifier, "5");
        assert_eq!(second.actor.email, None, "an empty email is anonymous");
        assert!(second.events.is_empty());
        assert_eq!(third.id.time, "");
        assert_eq!(third.events[0].name, "");

        let empty: Activities = serde_json::from_value(json!({"kind": "x"})).unwrap();
        assert!(empty.items.is_empty());
    }

    #[test]
    fn hints_name_what_to_do() {
        let hint = |status: u16, body: &serde_json::Value, filtered: bool| {
            Hint::of(
                StatusCode::from_u16(status).unwrap(),
                Some(body),
                filtered,
                "admin@example.com",
            )
        };
        let account = String::from("admin@example.com");
        assert_eq!(
            hint(
                403,
                &google_error(
                    403,
                    "Request had insufficient authentication scopes.",
                    "insufficientPermissions"
                ),
                false
            ),
            Some(Hint::AuthorizeAuditLog {
                account: account.clone()
            })
        );
        let newer = json!({"error": {"code": 403, "message": "Denied.", "status": "PERMISSION_DENIED",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                         "reason": "ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}});
        assert_eq!(
            hint(403, &newer, false),
            Some(Hint::AuthorizeAuditLog {
                account: account.clone()
            })
        );
        for reason in ["accessNotConfigured", "SERVICE_DISABLED"] {
            assert_eq!(
                hint(
                    403,
                    &google_error(403, "Admin SDK API has not been used.", reason),
                    true
                ),
                Some(Hint::EnableApi),
                "{reason}"
            );
        }
        for status in [401, 403] {
            let body = google_error(
                status,
                "Access denied. You are not authorized to read activity records.",
                "authError",
            );
            assert_eq!(hint(status, &body, false), Some(Hint::NotAdministrator));
        }
        assert_eq!(
            hint(
                401,
                &google_error(401, "Invalid Credentials", "authError"),
                false
            ),
            Some(Hint::Reauthorize { account })
        );
        let invalid = google_error(400, "Invalid value 'filters'", "invalid");
        assert_eq!(hint(400, &invalid, true), Some(Hint::ScanDomain));
        assert_eq!(hint(400, &invalid, false), None);
        assert_eq!(
            hint(403, &google_error(403, "No.", "forbidden"), false),
            None
        );
        assert_eq!(Hint::of(StatusCode::BAD_GATEWAY, None, true, "a"), None);
    }

    #[tokio::test]
    async fn pages_carry_the_audit_token_and_the_query() {
        let record = json!({"id": {"time": "2026-11-02T00:00:00.000Z", "uniqueQualifier": "1"},
                            "events": [{"name": "view", "parameters": [{"name": "doc_id", "value": "a"}]}]});
        let drive = FakeDrive::start(
            Vec::new(),
            Behavior {
                audit_log: vec![record.clone(), record],
                audit_page_size: Some(1),
                audit_rate_limited: 1,
                ..Behavior::default()
            },
        )
        .await;
        let hub = drive.hub();
        let log = AuditLog::new(&hub, "admin@example.com");
        let query = ActivityQuery {
            start: time("2026-11-01T00:00:00Z"),
            end: time("2026-11-03T12:30:00.5Z"),
            filter: Some(Filter::DocId(String::from("a"))),
        };

        let first = log.page(&query, None).await.unwrap();
        assert_eq!(first.items.len(), 1);
        let token = first.next_page_token.unwrap();
        let second = log.page(&query, Some(&token)).await.unwrap();
        assert_eq!(second.items.len(), 1);
        assert!(second.next_page_token.is_none());

        let requests = drive.audit_requests();
        assert_eq!(requests.len(), 3, "the 429 is retried");
        for request in &requests {
            assert_eq!(request.param("startTime"), Some("2026-11-01T00:00:00.000Z"));
            assert_eq!(request.param("endTime"), Some("2026-11-03T12:30:00.500Z"));
            assert_eq!(request.param("maxResults"), Some("1000"));
            assert_eq!(request.param("filters"), Some("doc_id==a"));
            assert_eq!(request.authorization.as_deref(), Some("Bearer mock-token"));
        }
        assert_eq!(requests[2].param("pageToken"), Some(token.as_str()));
        assert_eq!(drive.writes(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn errors_keep_googles_message_and_add_the_hint() {
        let message = "Admin SDK API has not been used in project 123 before or it is disabled. \
                       Enable it by visiting https://console.developers.google.com/apis/api/\
                       admin.googleapis.com/overview?project=123 then retry.";
        let drive = FakeDrive::start(
            Vec::new(),
            Behavior {
                audit_error: Some((403, google_error(403, message, "accessNotConfigured"))),
                ..Behavior::default()
            },
        )
        .await;
        let hub = drive.hub();
        let query = ActivityQuery {
            start: time("2026-11-01T00:00:00Z"),
            end: time("2026-11-02T00:00:00Z"),
            filter: None,
        };
        let error = AuditLog::new(&hub, "admin@example.com")
            .page(&query, None)
            .await
            .unwrap_err();
        assert_eq!(drive.audit_requests().len(), 1, "not retried");
        let text = error.to_string();
        assert_eq!(
            text,
            format!(
                "Google answered 403 Forbidden: {message}\nHint: enable the Admin SDK API in the \
                 Google Cloud project of the account's OAuth client (Google's message names the \
                 project and has the link), wait a few minutes and run the command again"
            )
        );
        assert!(
            !text.contains("mock-token") && !text.contains("startTime"),
            "{text}"
        );
    }
}
