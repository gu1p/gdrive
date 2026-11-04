//! The retry policy of every Drive request (#36, #25, #78): which failures are worth another
//! attempt, how long to wait before it, and a [`Throttle`] that holds back every request that
//! shares it once Google reports a rate limit.
//!
//! - The classifiers ([`classify_status`], [`classify_drive_error`], [`classify_transport`],
//!   [`classify_body_error`], [`classify_io`]) turn one failed attempt into a [`Decision`]:
//!   [`Decision::Retry`] with the delay the server asked for (`Retry-After`, zero without one),
//!   or [`Decision::Stop`] with the reason.
//! - [`RetryPolicy`] is the budget and the backoff. [`RetryState`] applies it to one operation:
//!   it turns a classifier's `Retry` into the delay to actually wait (backoff with jitter, at
//!   least the `Retry-After`, at least the shared throttle), or into a `Stop` once the budget is
//!   used up. Progress ([`RetryState::progress`]) starts a new budget.
//! - [`Throttle`] is shared by every request of a hub, and later of every scheduler worker.
//! - [`retry_api`] runs a google-drive3 metadata call under all of the above. The upload
//!   delegate (`delegate.rs`) and the media client (`media_client.rs`) use the parts directly.
//!
//! Google's guidance: <https://developers.google.com/workspace/drive/api/guides/handle-errors>.
//!
//! This module only uses `super::` paths and dependencies of the crate, so that
//! `tests/resumable_upload.rs` can compile it next to `delegate.rs`.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    future::Future,
    io,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime},
};

use hyper::{
    header::{HeaderMap, RETRY_AFTER},
    StatusCode,
};
use tokio::time::Instant;

/// What to do about a 401, in the style of the auth errors.
const UNAUTHORIZED_HINT: &str =
    "; run 'gdrive account reauth' to authorize the current account again";

/// A longer `Retry-After` is cut down to this.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

/// 403 reasons that mean "slow down", which Google documents as retryable with backoff.
const RATE_LIMIT_REASONS: [&str; 3] = [
    "rateLimitExceeded",
    "userRateLimitExceeded",
    "sharingRateLimitExceeded",
];

/// Reasons that do not go away within a retry budget: storage full, daily limits.
const QUOTA_REASONS: [&str; 6] = [
    "storageQuotaExceeded",
    "quotaExceeded",
    "dailyLimitExceeded",
    "dailyLimitExceededUnreg",
    "downloadQuotaExceeded",
    "teamDriveFileLimitExceeded",
];

/// How the backoff delay is randomized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Jitter {
    /// Exactly the backoff delay.
    None,
    /// A uniformly random delay between zero and the backoff delay ("full jitter"), so that
    /// workers that failed together do not retry together.
    Full,
}

/// The retry budget and backoff of one operation: one metadata call, one upload, one download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryPolicy {
    /// Attempts, the first one included, without progress in between.
    pub(crate) max_attempts: u32,
    /// No further attempt starts once this much time has passed since the first failure
    /// without progress in between.
    pub(crate) max_elapsed: Duration,
    /// The backoff before the first retry (before jitter).
    pub(crate) base_delay: Duration,
    /// The backoff grows by this factor with each further retry.
    pub(crate) multiplier: u32,
    /// The backoff never grows past this.
    pub(crate) max_delay: Duration,
    pub(crate) jitter: Jitter,
}

impl Default for RetryPolicy {
    /// Up to 10 attempts and 10 minutes, waiting 1 s doubling to 64 s, with full jitter.
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 10,
            max_elapsed: Duration::from_secs(10 * 60),
            base_delay: Duration::from_secs(1),
            multiplier: 2,
            max_delay: Duration::from_secs(64),
            jitter: Jitter::Full,
        }
    }
}

impl RetryPolicy {
    /// One attempt, no retries.
    #[must_use]
    pub(crate) fn no_retries() -> Self {
        RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        }
    }

    /// A policy with millisecond delays, for tests against local mock servers.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_tests() -> Self {
        RetryPolicy {
            max_attempts: 4,
            max_elapsed: Duration::from_secs(30),
            base_delay: Duration::from_millis(2),
            multiplier: 2,
            max_delay: Duration::from_millis(10),
            jitter: Jitter::Full,
        }
    }

    /// The backoff before retry number `retry` (1 for the first retry), before jitter:
    /// `base_delay * multiplier^(retry - 1)`, at most `max_delay`.
    #[must_use]
    pub(crate) fn backoff_ceiling(&self, retry: u32) -> Duration {
        let factor = self
            .multiplier
            .max(1)
            .saturating_pow(retry.saturating_sub(1));
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }

    /// The delay before retry number `retry`, jittered with `rng`.
    pub(crate) fn backoff(&self, retry: u32, rng: &mut fastrand::Rng) -> Duration {
        let ceiling = self.backoff_ceiling(retry);
        match self.jitter {
            Jitter::None => ceiling,
            Jitter::Full => {
                let nanos = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX);
                Duration::from_nanos(rng.u64(0..=nanos))
            }
        }
    }
}

/// What to do after a failed attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Try again after `after`. From a classifier, `after` is what the server asked for
    /// (`Retry-After`, zero without one); from [`RetryState::next`], the delay to wait.
    Retry {
        after: Duration,
        reason: RetryReason,
    },
    /// Give up.
    Stop { reason: StopReason },
}

/// Why a failure is worth another attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetryReason {
    /// 408, 500, 502, 503 or 504.
    Status(StatusCode),
    /// 429, or a 403 with a rate-limit reason. These also hold back every request that shares
    /// the [`Throttle`].
    RateLimited {
        status: StatusCode,
        reason: Option<String>,
    },
    /// No reply: connection refused or reset, timeout, HTTP/2 GOAWAY or stream reset.
    Transport,
    /// The reply body broke off.
    BodyRead,
    /// A resumable upload chunk answered with a 308 that took none of it.
    NoProgress,
    /// A reply to a `Range` request that does not continue where the download stopped.
    BadRange,
}

impl RetryReason {
    #[must_use]
    pub(crate) fn is_rate_limit(&self) -> bool {
        matches!(self, RetryReason::RateLimited { .. })
    }
}

impl Display for RetryReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            RetryReason::Status(status) => write!(f, "HTTP {status}"),
            RetryReason::RateLimited { status, reason } => {
                write!(f, "rate limited (HTTP {status}")?;
                if let Some(reason) = reason {
                    write!(f, " {reason}")?;
                }
                f.write_str(")")
            }
            RetryReason::Transport => f.write_str("connection error"),
            RetryReason::BodyRead => f.write_str("the reply broke off"),
            RetryReason::NoProgress => f.write_str("the server accepted none of the chunk"),
            RetryReason::BadRange => {
                f.write_str("the reply does not continue where the transfer stopped")
            }
        }
    }
}

/// Why a failure is not retried (any more).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// 401: the access token was not accepted.
    Unauthorized,
    /// 403 for another reason than a rate limit: no permission, a policy, an abusive file.
    Forbidden { reason: Option<String> },
    /// 404.
    NotFound,
    /// Storage full or a daily limit: waiting minutes does not help.
    Quota { reason: String },
    /// Another status that is not worth repeating (400, 409, 411, 501, ...).
    Status(StatusCode),
    /// The TLS connection could not be set up, for example because the certificate is not
    /// trusted.
    Tls,
    /// No access token could be obtained; the token source reports why.
    Token,
    /// A local problem: file I/O, or a request gdrive could not build.
    Local,
    /// A reply that could not be decoded.
    InvalidReply,
    /// The retry budget is used up.
    GaveUp {
        attempts: u32,
        elapsed: Duration,
        last: RetryReason,
    },
}

impl Display for StopReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            StopReason::Unauthorized => {
                write!(
                    f,
                    "Google did not accept the access token{UNAUTHORIZED_HINT}"
                )
            }
            StopReason::Forbidden {
                reason: Some(reason),
            } => {
                write!(f, "access denied ({reason})")
            }
            StopReason::Forbidden { reason: None } => f.write_str("access denied"),
            StopReason::NotFound => f.write_str("not found"),
            StopReason::Quota { reason } => write!(f, "quota exceeded ({reason})"),
            StopReason::Status(status) => write!(f, "HTTP {status}"),
            StopReason::Tls => f.write_str("the secure connection could not be set up"),
            StopReason::Token => f.write_str("no access token"),
            StopReason::Local => f.write_str("local error"),
            StopReason::InvalidReply => f.write_str("the reply could not be decoded"),
            StopReason::GaveUp {
                attempts,
                elapsed,
                last,
            } => write!(
                f,
                "gave up after {attempts} attempts in {}, the last one failed with: {last}",
                DisplayDuration(*elapsed)
            ),
        }
    }
}

/// A duration as `850ms`, `12.5s` or `3m 20s`.
pub(crate) struct DisplayDuration(pub(crate) Duration);

impl Display for DisplayDuration {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let duration = self.0;
        if duration < Duration::from_secs(1) {
            write!(f, "{}ms", duration.as_millis())
        } else if duration < Duration::from_secs(60) {
            write!(f, "{:.1}s", duration.as_secs_f64())
        } else {
            let seconds = duration.as_secs();
            write!(f, "{}m {}s", seconds / 60, seconds % 60)
        }
    }
}

/// The retry state of one operation under a [`RetryPolicy`].
#[derive(Debug, Clone)]
pub(crate) struct RetryState {
    policy: RetryPolicy,
    throttle: Option<Throttle>,
    /// Failed attempts since the start or the last progress.
    failures: u32,
    /// When the first of them failed.
    first_failure: Option<Instant>,
    rng: fastrand::Rng,
}

impl RetryState {
    /// A state that signals rate limits to `throttle` and waits for it, if given.
    #[must_use]
    pub(crate) fn new(policy: RetryPolicy, throttle: Option<Throttle>) -> Self {
        RetryState {
            policy,
            throttle,
            failures: 0,
            first_failure: None,
            rng: fastrand::Rng::new(),
        }
    }

    #[must_use]
    pub(crate) fn policy(&self) -> &RetryPolicy {
        &self.policy
    }

    /// Failed attempts since the start or the last progress.
    #[must_use]
    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }

    /// The operation got further (an upload chunk was accepted, download bytes arrived): the
    /// next failure starts a new budget.
    pub(crate) fn progress(&mut self) {
        self.failures = 0;
        self.first_failure = None;
    }

    /// Counts a failed attempt and decides what follows: a classifier's `Stop` stays; its
    /// `Retry` becomes the delay to wait (the jittered backoff, at least the server's
    /// `Retry-After`, at least what the shared throttle still holds back), or a
    /// [`StopReason::GaveUp`] once the attempts or the elapsed time are used up. A rate limit is
    /// signaled to the throttle, so that every request sharing it waits as well.
    pub(crate) fn next(&mut self, decision: Decision) -> Decision {
        let Decision::Retry {
            after: requested,
            reason,
        } = decision
        else {
            return decision;
        };

        let now = Instant::now();
        let first_failure = *self.first_failure.get_or_insert(now);
        let elapsed = now.saturating_duration_since(first_failure);
        self.failures = self.failures.saturating_add(1);

        let give_up = |attempts, last| Decision::Stop {
            reason: StopReason::GaveUp {
                attempts,
                elapsed,
                last,
            },
        };

        if self.failures >= self.policy.max_attempts {
            return give_up(self.failures, reason);
        }

        let mut after = self
            .policy
            .backoff(self.failures, &mut self.rng)
            .max(requested);
        if let Some(throttle) = &self.throttle {
            if reason.is_rate_limit() {
                throttle.signal(after);
            }
            after = after.max(throttle.remaining());
        }

        if elapsed.saturating_add(after) > self.policy.max_elapsed {
            return give_up(self.failures, reason);
        }

        Decision::Retry { after, reason }
    }
}

/// The earliest time at which the next request may start, shared by every request (and every
/// worker) that holds a clone. A rate limit seen by one of them holds back all of them, so that
/// concurrent workers do not keep hitting the limit while one of them backs off.
#[derive(Debug, Clone, Default)]
pub(crate) struct Throttle {
    next_request: Arc<Mutex<Option<Instant>>>,
}

impl Throttle {
    #[must_use]
    pub(crate) fn new() -> Self {
        Throttle::default()
    }

    /// Holds back every request for at least `delay` from now. An earlier hold that ends later
    /// is kept.
    pub(crate) fn signal(&self, delay: Duration) {
        let now = Instant::now();
        let until = now.checked_add(delay.min(MAX_RETRY_AFTER)).unwrap_or(now);
        let mut next_request = lock(&self.next_request);
        if next_request.is_none_or(|current| current < until) {
            *next_request = Some(until);
        }
    }

    /// How long requests are still held back; zero when they are not.
    #[must_use]
    pub(crate) fn remaining(&self) -> Duration {
        lock(&self.next_request).map_or(Duration::ZERO, |until| {
            until.saturating_duration_since(Instant::now())
        })
    }

    /// Waits until requests may start again. Another request may extend the hold meanwhile,
    /// which is waited for as well.
    pub(crate) async fn wait(&self) {
        loop {
            let remaining = self.remaining();
            if remaining.is_zero() {
                return;
            }
            tokio::time::sleep(remaining).await;
        }
    }
}

/// The policy and throttle of a hub: [`Retrier::state`] starts the retry state of one
/// operation.
#[derive(Debug, Clone, Default)]
pub(crate) struct Retrier {
    pub(crate) policy: RetryPolicy,
    pub(crate) throttle: Throttle,
}

impl Retrier {
    #[must_use]
    pub(crate) fn new(policy: RetryPolicy, throttle: Throttle) -> Self {
        Retrier { policy, throttle }
    }

    /// The retry state of a new operation, sharing this throttle.
    #[must_use]
    pub(crate) fn state(&self) -> RetryState {
        RetryState::new(self.policy, Some(self.throttle.clone()))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // The guarded values stay valid whatever a panicking holder did.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `Retry-After` of a reply, capped at [`MAX_RETRY_AFTER`].
#[must_use]
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value, SystemTime::now())
}

/// A `Retry-After` value, delay seconds (`120`) or an HTTP date
/// (`Wed, 21 Oct 2015 07:28:00 GMT`, a date in the past meaning now), capped at
/// [`MAX_RETRY_AFTER`].
#[must_use]
pub(crate) fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    let delay = if let Ok(seconds) = value.parse::<u64>() {
        Duration::from_secs(seconds)
    } else {
        let date = httpdate::parse_http_date(value).ok()?;
        date.duration_since(now).unwrap_or_default()
    };
    Some(delay.min(MAX_RETRY_AFTER))
}

/// The `reason`s of a Google JSON error body: `error.errors[].reason`, and
/// `error.details[].reason` of the newer format.
fn error_reasons(error: Option<&serde_json::Value>) -> Vec<String> {
    let Some(error) = error.and_then(|body| body.get("error")) else {
        return Vec::new();
    };
    ["errors", "details"]
        .iter()
        .filter_map(|key| error.get(key)?.as_array())
        .flatten()
        .filter_map(|entry| entry.get("reason")?.as_str())
        .map(str::to_owned)
        .collect()
}

/// `rateLimitExceeded` and `RATE_LIMIT_EXCEEDED` are the same reason.
fn is_reason(reason: &str, known: &str) -> bool {
    let normalize = |s: &str| {
        s.chars()
            .filter(|c| *c != '_')
            .map(|c| c.to_ascii_lowercase())
            .collect::<String>()
    };
    normalize(reason) == normalize(known)
}

fn is_any_reason(reason: &str, known: &[&str]) -> bool {
    known.iter().any(|known| is_reason(reason, known))
}

/// The HTTP status of a Google JSON error body (`error.code`).
#[must_use]
pub(crate) fn status_of_error_body(error: &serde_json::Value) -> Option<StatusCode> {
    let code = error.get("error")?.get("code")?.as_u64()?;
    StatusCode::from_u16(u16::try_from(code).ok()?).ok()
}

/// The message of a Google JSON error body (`error.message`).
#[must_use]
pub(crate) fn message_of_error_body(error: &serde_json::Value) -> Option<&str> {
    error.get("error")?.get("message")?.as_str()
}

fn stop(reason: StopReason) -> Decision {
    Decision::Stop { reason }
}

/// A reply with a failure status. `headers` gives the `Retry-After`, `error` the JSON error body
/// with the reasons that decide a 403.
#[must_use]
pub(crate) fn classify_status(
    status: StatusCode,
    headers: Option<&HeaderMap>,
    error: Option<&serde_json::Value>,
) -> Decision {
    let reasons = error_reasons(error);
    if let Some(reason) = reasons
        .iter()
        .find(|reason| is_any_reason(reason, &QUOTA_REASONS))
    {
        return stop(StopReason::Quota {
            reason: reason.clone(),
        });
    }

    let retry = |reason| Decision::Retry {
        after: headers.and_then(retry_after).unwrap_or_default(),
        reason,
    };

    match status {
        StatusCode::UNAUTHORIZED => stop(StopReason::Unauthorized),
        StatusCode::FORBIDDEN => {
            match reasons
                .iter()
                .find(|reason| is_any_reason(reason, &RATE_LIMIT_REASONS))
            {
                Some(reason) => retry(RetryReason::RateLimited {
                    status,
                    reason: Some(reason.clone()),
                }),
                None => stop(StopReason::Forbidden {
                    reason: reasons.into_iter().next(),
                }),
            }
        }
        StatusCode::NOT_FOUND => stop(StopReason::NotFound),
        StatusCode::TOO_MANY_REQUESTS => retry(RetryReason::RateLimited {
            status,
            reason: reasons.into_iter().next(),
        }),
        StatusCode::REQUEST_TIMEOUT
        | StatusCode::INTERNAL_SERVER_ERROR
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => retry(RetryReason::Status(status)),
        _ => stop(StopReason::Status(status)),
    }
}

/// The error and its sources.
fn chain<'a>(error: &'a (dyn Error + 'static)) -> impl Iterator<Item = &'a (dyn Error + 'static)> {
    std::iter::successors(Some(error), |&error| error.source())
}

/// hyper's "the request is invalid" errors are gdrive's own bugs: repeating does not help.
fn is_user_error(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<hyper::Error>()
        .is_some_and(hyper::Error::is_user)
}

/// A request that got no reply: retried (connection refused or reset, timeout, HTTP/2 GOAWAY or
/// stream reset, DNS), except for TLS setup failures and requests hyper refused to send.
#[must_use]
pub(crate) fn classify_transport(error: &hyper_util::client::legacy::Error) -> Decision {
    if chain(error).any(is_user_error) {
        return stop(StopReason::Local);
    }

    // TLS errors (an untrusted certificate, a missing CA bundle) reach the connector as
    // `InvalidData`; they do not go away by waiting.
    let tls_failure = error.is_connect()
        && chain(error).any(|error| {
            error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::InvalidData)
        });
    if tls_failure {
        return stop(StopReason::Tls);
    }

    Decision::Retry {
        after: Duration::ZERO,
        reason: RetryReason::Transport,
    }
}

/// A reply body that broke off (connection reset, HTTP/2 GOAWAY or stream reset): retried.
#[must_use]
pub(crate) fn classify_body_error(error: &hyper::Error) -> Decision {
    if error.is_user() {
        stop(StopReason::Local)
    } else {
        Decision::Retry {
            after: Duration::ZERO,
            reason: RetryReason::BodyRead,
        }
    }
}

/// A local I/O error (reading the source file, writing the destination): never retried.
#[must_use]
pub(crate) fn classify_io(_error: &io::Error) -> Decision {
    stop(StopReason::Local)
}

/// Status and headers of a failed reply, which google-drive3's `Error::BadRequest` drops.
#[derive(Debug, Clone)]
pub(crate) struct FailedReply {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
}

/// A failed google-drive3 call. `reply` is what [`ApiDelegate`] kept of the failed reply.
#[must_use]
pub(crate) fn classify_drive_error(
    error: &google_drive3::Error,
    reply: Option<&FailedReply>,
) -> Decision {
    use google_drive3::Error as E;

    match error {
        E::HttpError(error) => classify_transport(error),
        E::BadRequest(body) => {
            let status = reply
                .map(|reply| reply.status)
                .or_else(|| status_of_error_body(body))
                .unwrap_or(StatusCode::BAD_REQUEST);
            classify_status(status, reply.map(|reply| &reply.headers), Some(body))
        }
        E::Failure(response) => classify_status(response.status(), Some(response.headers()), None),
        E::MissingToken(_) => stop(StopReason::Token),
        // A 2xx whose body could not be read most likely did what it was asked to; repeating a
        // create would make a duplicate, and google-drive3 does not tell a cut body from a
        // malformed one.
        E::JsonDecodeError(..) => stop(StopReason::InvalidReply),
        E::Io(_)
        | E::UploadSizeLimitExceeded(..)
        | E::MissingAPIKey
        | E::Cancelled
        | E::FieldClash(_) => stop(StopReason::Local),
    }
}

/// The google-drive3 delegate of [`retry_api`]. It never retries by itself (google-drive3's
/// own loop would bypass the budget and the throttle); it keeps the status and headers of a
/// failed reply for [`classify_drive_error`].
#[derive(Debug, Clone, Default)]
pub(crate) struct ApiDelegate {
    failed_reply: Arc<Mutex<Option<FailedReply>>>,
}

impl google_apis_common::Delegate for ApiDelegate {
    fn http_failure(
        &mut self,
        response: &google_apis_common::Response,
        _error: Option<&serde_json::Value>,
    ) -> google_apis_common::Retry {
        *lock(&self.failed_reply) = Some(FailedReply {
            status: response.status(),
            headers: response.headers().clone(),
        });
        google_apis_common::Retry::Abort
    }
}

/// A 401 from Drive, returned by [`retry_api`] as `Error::MissingToken`, the error the auth code
/// and `gdrive`'s error output treat as an authorization failure, with the command to run.
#[derive(Debug)]
pub(crate) struct AccessTokenRejected {
    message: Option<String>,
}

impl Display for AccessTokenRejected {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("Google did not accept the access token")?;
        if let Some(message) = &self.message {
            write!(f, " ({message})")?;
        }
        f.write_str(UNAUTHORIZED_HINT)
    }
}

impl Error for AccessTokenRejected {}

/// Runs a google-drive3 call, and repeats it per the policy of `retrier` while it fails with a
/// retryable error ([`classify_drive_error`]).
///
/// `attempt` builds and sends the call; it gets the [`ApiDelegate`] of the attempt, which it must
/// set with `.delegate(&mut delegate)` so that the `Retry-After` and status of a failed reply are
/// seen. Each attempt first waits for the shared throttle. A retry is announced on stderr with
/// `call` (for example `files.list`). The error of the last attempt is returned unchanged, except
/// for a 401, which becomes `Error::MissingToken` with a hint to authorize again.
///
/// Media upload bodies do not belong here: their retries are the upload delegate's.
#[expect(
    clippy::result_large_err,
    reason = "the error of the wrapped call, unchanged; callers box it where they keep it"
)]
pub(crate) async fn retry_api<T, F, Fut>(
    retrier: &Retrier,
    call: &str,
    mut attempt: F,
) -> google_drive3::Result<T>
where
    F: FnMut(ApiDelegate) -> Fut,
    Fut: Future<Output = google_drive3::Result<T>>,
{
    let mut state = retrier.state();
    loop {
        retrier.throttle.wait().await;

        let delegate = ApiDelegate::default();
        let failed_reply = Arc::clone(&delegate.failed_reply);
        let error = match attempt(delegate).await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let failed_reply = lock(&failed_reply).take();

        match state.next(classify_drive_error(&error, failed_reply.as_ref())) {
            Decision::Retry { after, reason } => {
                eprintln!(
                    "Warning: {call} failed ({reason}), retrying in {} (attempt {} of {})",
                    DisplayDuration(after),
                    state.failures() + 1,
                    state.policy().max_attempts,
                );
                tokio::time::sleep(after).await;
            }
            Decision::Stop {
                reason: StopReason::Unauthorized,
            } => {
                let message = match &error {
                    google_drive3::Error::BadRequest(body) => {
                        message_of_error_body(body).map(str::to_owned)
                    }
                    _ => None,
                };
                return Err(google_drive3::Error::MissingToken(Box::new(
                    AccessTokenRejected { message },
                )));
            }
            Decision::Stop { .. } => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use hyper::{header::HeaderValue, HeaderMap, StatusCode};
    use serde_json::json;

    use super::{
        classify_body_error, classify_drive_error, classify_io, classify_status, parse_retry_after,
        retry_after, Decision, FailedReply, Jitter, RetryPolicy, RetryReason, RetryState,
        StopReason, Throttle, MAX_RETRY_AFTER,
    };

    fn google_error(code: u16, reason: &str) -> serde_json::Value {
        json!({"error": {
            "code": code,
            "message": "message",
            "errors": [{"domain": "usageLimits", "reason": reason, "message": "message"}],
        }})
    }

    fn is_retry(decision: &Decision) -> bool {
        matches!(decision, Decision::Retry { .. })
    }

    fn stop_reason(decision: Decision) -> StopReason {
        match decision {
            Decision::Stop { reason } => reason,
            Decision::Retry { .. } => panic!("expected a stop, got {decision:?}"),
        }
    }

    #[test]
    fn transient_statuses_are_retried() {
        for status in [408, 429, 500, 502, 503, 504] {
            let status = StatusCode::from_u16(status).unwrap();
            assert!(
                is_retry(&classify_status(status, None, None)),
                "{status} should be retried"
            );
        }
    }

    #[test]
    fn other_statuses_stop() {
        for status in [400, 401, 403, 404, 409, 410, 411, 416, 501, 505] {
            let status = StatusCode::from_u16(status).unwrap();
            assert!(
                !is_retry(&classify_status(status, None, None)),
                "{status} should not be retried"
            );
        }
        assert_eq!(
            stop_reason(classify_status(StatusCode::UNAUTHORIZED, None, None)),
            StopReason::Unauthorized
        );
        assert_eq!(
            stop_reason(classify_status(StatusCode::NOT_FOUND, None, None)),
            StopReason::NotFound
        );
    }

    #[test]
    fn rate_limit_403_is_retried_and_other_403_stop() {
        for reason in [
            "rateLimitExceeded",
            "userRateLimitExceeded",
            "sharingRateLimitExceeded",
        ] {
            let body = google_error(403, reason);
            let decision = classify_status(StatusCode::FORBIDDEN, None, Some(&body));
            assert_eq!(
                decision,
                Decision::Retry {
                    after: Duration::ZERO,
                    reason: RetryReason::RateLimited {
                        status: StatusCode::FORBIDDEN,
                        reason: Some(reason.to_owned()),
                    },
                },
            );
        }

        for reason in ["insufficientFilePermissions", "cannotDownloadAbusiveFile"] {
            let body = google_error(403, reason);
            assert_eq!(
                stop_reason(classify_status(StatusCode::FORBIDDEN, None, Some(&body))),
                StopReason::Forbidden {
                    reason: Some(reason.to_owned())
                }
            );
        }

        assert_eq!(
            stop_reason(classify_status(StatusCode::FORBIDDEN, None, None)),
            StopReason::Forbidden { reason: None }
        );
    }

    #[test]
    fn quota_reasons_stop_whatever_the_status() {
        for (status, reason) in [
            (403, "storageQuotaExceeded"),
            (403, "dailyLimitExceeded"),
            (403, "downloadQuotaExceeded"),
            (429, "dailyLimitExceeded"),
        ] {
            let body = google_error(status, reason);
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(
                stop_reason(classify_status(status, None, Some(&body))),
                StopReason::Quota {
                    reason: reason.to_owned()
                }
            );
        }
    }

    #[test]
    fn reasons_of_the_newer_error_format_count() {
        let body = json!({"error": {"code": 403, "details": [
            {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED"}
        ]}});
        assert!(is_retry(&classify_status(
            StatusCode::FORBIDDEN,
            None,
            Some(&body)
        )));
    }

    #[test]
    fn retry_after_comes_from_the_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("7"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(7)));

        let decision = classify_status(StatusCode::SERVICE_UNAVAILABLE, Some(&headers), None);
        assert_eq!(
            decision,
            Decision::Retry {
                after: Duration::from_secs(7),
                reason: RetryReason::Status(StatusCode::SERVICE_UNAVAILABLE),
            }
        );
    }

    #[test]
    fn retry_after_seconds_and_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_480);
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
        // 1_445_412_480 is Wed, 21 Oct 2015 07:28:00 GMT.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:29:30 GMT", now),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", now),
            Some(Duration::ZERO),
            "a date in the past means now"
        );
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("-5", now), None);
        assert_eq!(parse_retry_after("1.5", now), None);
    }

    #[test]
    fn retry_after_is_capped_at_five_minutes() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_480);
        assert_eq!(parse_retry_after("86400", now), Some(MAX_RETRY_AFTER));
        assert_eq!(
            parse_retry_after("Thu, 22 Oct 2015 07:28:00 GMT", now),
            Some(MAX_RETRY_AFTER)
        );
        assert_eq!(MAX_RETRY_AFTER, Duration::from_secs(300));
    }

    #[test]
    fn drive_errors_use_the_kept_reply() {
        // google-drive3 drops the reply of a JSON error; the delegate keeps its status and
        // headers.
        let body = google_error(403, "userRateLimitExceeded");
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("3"));
        let reply = FailedReply {
            status: StatusCode::FORBIDDEN,
            headers,
        };
        let error = google_drive3::Error::BadRequest(body.clone());
        assert_eq!(
            classify_drive_error(&error, Some(&reply)),
            Decision::Retry {
                after: Duration::from_secs(3),
                reason: RetryReason::RateLimited {
                    status: StatusCode::FORBIDDEN,
                    reason: Some(String::from("userRateLimitExceeded")),
                },
            }
        );

        // Without it, the status comes from the body.
        let error = google_drive3::Error::BadRequest(google_error(404, "notFound"));
        assert_eq!(
            stop_reason(classify_drive_error(&error, None)),
            StopReason::NotFound
        );
        let error = google_drive3::Error::BadRequest(google_error(503, "backendError"));
        assert!(is_retry(&classify_drive_error(&error, None)));
    }

    #[test]
    fn local_and_decode_errors_stop() {
        let io = std::io::Error::other("disk full");
        assert_eq!(stop_reason(classify_io(&io)), StopReason::Local);

        let decode = serde_json::from_str::<serde_json::Value>("").unwrap_err();
        let error = google_drive3::Error::JsonDecodeError(String::new(), decode);
        assert_eq!(
            stop_reason(classify_drive_error(&error, None)),
            StopReason::InvalidReply
        );
        assert_eq!(
            stop_reason(classify_drive_error(&google_drive3::Error::Cancelled, None)),
            StopReason::Local
        );
    }

    /// A connection that is accepted and closed before the reply.
    async fn transport_error() -> hyper_util::client::legacy::Error {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            drop(socket);
        });
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http::<http_body_util::Empty<bytes::Bytes>>();
        client
            .get(format!("http://{addr}/").parse().unwrap())
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn transport_and_body_errors_are_retried() {
        let error = transport_error().await;
        assert_eq!(
            super::classify_transport(&error),
            Decision::Retry {
                after: Duration::ZERO,
                reason: RetryReason::Transport,
            }
        );
        let error = google_drive3::Error::HttpError(error);
        assert!(is_retry(&classify_drive_error(&error, None)));

        // A body that ends before its Content-Length.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            let _ = socket.read(&mut buf).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc")
                .await
                .unwrap();
        });
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http::<http_body_util::Empty<bytes::Bytes>>();
        let response = client
            .get(format!("http://{addr}/").parse().unwrap())
            .await
            .unwrap();
        let error = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap_err();
        assert_eq!(
            classify_body_error(&error),
            Decision::Retry {
                after: Duration::ZERO,
                reason: RetryReason::BodyRead,
            }
        );
    }

    fn no_jitter() -> RetryPolicy {
        RetryPolicy {
            jitter: Jitter::None,
            ..RetryPolicy::default()
        }
    }

    #[test]
    fn backoff_doubles_up_to_the_maximum() {
        let policy = no_jitter();
        let ceilings: Vec<u64> = (1..=9)
            .map(|retry| policy.backoff_ceiling(retry).as_secs())
            .collect();
        assert_eq!(ceilings, [1, 2, 4, 8, 16, 32, 64, 64, 64]);
        // No overflow, however many retries.
        assert_eq!(policy.backoff_ceiling(u32::MAX), Duration::from_secs(64));
        let mut rng = fastrand::Rng::with_seed(1);
        assert_eq!(policy.backoff(3, &mut rng), Duration::from_secs(4));
    }

    #[test]
    fn full_jitter_stays_within_zero_and_the_ceiling() {
        let policy = RetryPolicy::default();
        let mut rng = fastrand::Rng::with_seed(42);
        for retry in 1..=12 {
            let ceiling = policy.backoff_ceiling(retry);
            let delays: Vec<Duration> = (0..200).map(|_| policy.backoff(retry, &mut rng)).collect();
            assert!(delays.iter().all(|delay| *delay <= ceiling), "{retry}");
            let distinct: std::collections::HashSet<_> = delays.iter().collect();
            assert!(distinct.len() > 100, "the delays must be spread out");
            let mean = delays.iter().sum::<Duration>() / 200;
            assert!(
                mean > ceiling / 4 && mean < ceiling * 3 / 4,
                "uniform over the range: mean {mean:?} for ceiling {ceiling:?}"
            );
        }
    }

    fn transient() -> Decision {
        Decision::Retry {
            after: Duration::ZERO,
            reason: RetryReason::Status(StatusCode::SERVICE_UNAVAILABLE),
        }
    }

    #[tokio::test]
    async fn the_budget_bounds_the_attempts_and_progress_resets_it() {
        let mut state = RetryState::new(no_jitter(), None);
        // Attempts 1 to 9 fail and are retried, the 10th failure gives up.
        let delays: Vec<u64> = (0..9)
            .map(|_| match state.next(transient()) {
                Decision::Retry { after, .. } => after.as_secs(),
                Decision::Stop { reason } => panic!("stopped early: {reason}"),
            })
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 64, 64, 64]);
        assert!(matches!(
            stop_reason(state.next(transient())),
            StopReason::GaveUp { attempts: 10, .. }
        ));

        state.progress();
        assert_eq!(state.failures(), 0);
        assert_eq!(
            state.next(transient()),
            Decision::Retry {
                after: Duration::from_secs(1),
                reason: RetryReason::Status(StatusCode::SERVICE_UNAVAILABLE),
            },
            "after progress, the backoff starts over"
        );
    }

    #[tokio::test]
    async fn a_stop_is_not_counted() {
        let mut state = RetryState::new(no_jitter(), None);
        let decision = state.next(Decision::Stop {
            reason: StopReason::NotFound,
        });
        assert_eq!(stop_reason(decision), StopReason::NotFound);
        assert_eq!(state.failures(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_elapsed_budget_ends_retries() {
        let policy = RetryPolicy {
            max_attempts: 100,
            max_elapsed: Duration::from_secs(60),
            ..no_jitter()
        };
        let mut state = RetryState::new(policy, None);
        let mut waited = Duration::ZERO;
        loop {
            match state.next(transient()) {
                Decision::Retry { after, .. } => {
                    tokio::time::sleep(after).await;
                    waited += after;
                }
                Decision::Stop { reason } => {
                    assert!(matches!(reason, StopReason::GaveUp { .. }), "{reason}");
                    break;
                }
            }
        }
        // 1 + 2 + 4 + 8 + 16 = 31 s; the next 32 s would end after 63 s.
        assert_eq!(waited, Duration::from_secs(31));
    }

    #[tokio::test]
    async fn retry_after_raises_the_delay_and_can_exceed_the_budget() {
        let mut state = RetryState::new(no_jitter(), None);
        let decision = state.next(Decision::Retry {
            after: Duration::from_secs(30),
            reason: RetryReason::Status(StatusCode::SERVICE_UNAVAILABLE),
        });
        assert!(
            matches!(decision, Decision::Retry { after, .. } if after == Duration::from_secs(30))
        );

        let policy = RetryPolicy {
            max_elapsed: Duration::from_secs(10),
            ..no_jitter()
        };
        let mut state = RetryState::new(policy, None);
        let decision = state.next(Decision::Retry {
            after: Duration::from_secs(30),
            reason: RetryReason::Status(StatusCode::SERVICE_UNAVAILABLE),
        });
        assert!(matches!(
            stop_reason(decision),
            StopReason::GaveUp { attempts: 1, .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_holds_back_every_clone() {
        let throttle = Throttle::new();
        let worker = throttle.clone();
        assert_eq!(worker.remaining(), Duration::ZERO);

        throttle.signal(Duration::from_secs(10));
        assert_eq!(worker.remaining(), Duration::from_secs(10));
        // A shorter hold does not shorten a longer one.
        worker.signal(Duration::from_secs(2));
        assert_eq!(throttle.remaining(), Duration::from_secs(10));

        let start = tokio::time::Instant::now();
        tokio::join!(worker.wait(), throttle.wait());
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        assert_eq!(throttle.remaining(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_wait_follows_an_extended_hold() {
        let throttle = Throttle::new();
        throttle.signal(Duration::from_secs(1));
        let extender = throttle.clone();
        let start = tokio::time::Instant::now();
        tokio::join!(throttle.wait(), async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            extender.signal(Duration::from_secs(5));
        });
        assert_eq!(start.elapsed(), Duration::from_millis(5500));
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_in_one_operation_holds_back_the_others() {
        let throttle = Throttle::new();
        let mut first = RetryState::new(no_jitter(), Some(throttle.clone()));
        let mut second = RetryState::new(no_jitter(), Some(throttle.clone()));

        let rate_limited = Decision::Retry {
            after: Duration::from_secs(20),
            reason: RetryReason::RateLimited {
                status: StatusCode::TOO_MANY_REQUESTS,
                reason: None,
            },
        };
        assert!(
            matches!(first.next(rate_limited), Decision::Retry { after, .. } if after == Duration::from_secs(20))
        );
        assert_eq!(throttle.remaining(), Duration::from_secs(20));

        // Another operation that fails for another reason waits for the hold too.
        assert!(
            matches!(second.next(transient()), Decision::Retry { after, .. } if after == Duration::from_secs(20))
        );

        // A plain 503 does not hold back anyone.
        let other = Throttle::new();
        let mut third = RetryState::new(no_jitter(), Some(other.clone()));
        let _ = third.next(transient());
        assert_eq!(other.remaining(), Duration::ZERO);
    }

    #[test]
    fn stop_reasons_read_well() {
        assert_eq!(
            StopReason::GaveUp {
                attempts: 10,
                elapsed: Duration::from_secs(200),
                last: RetryReason::Status(StatusCode::SERVICE_UNAVAILABLE),
            }
            .to_string(),
            "gave up after 10 attempts in 3m 20s, the last one failed with: HTTP 503 Service \
             Unavailable"
        );
        assert!(StopReason::Unauthorized
            .to_string()
            .contains("gdrive account reauth"));
    }
}
