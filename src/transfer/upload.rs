//! gdrive's own uploader (milestone 2, item 1 of docs/triage/README.md): Google's upload protocol
//! spoken directly over the hub's HTTP client, replacing google-apis-common's resumable helper.
//! The protocol: <https://developers.google.com/workspace/drive/api/guides/manage-uploads>.
//!
//! - A file of at most [`UploadOptions::multipart_threshold`] bytes goes up in one multipart
//!   request with its metadata. A larger one goes through a resumable session: a POST (a PATCH
//!   for an update) with the metadata starts it, then the content follows in chunks, each a PUT
//!   with `Content-Range`, all but the last a multiple of 256 KiB.
//! - Bodies are streamed from the source (`body.rs`): a chunk is read piece by piece while it is
//!   sent and never held whole; a chunk sent again is read again from its offset.
//! - The server decides where the next chunk starts: after a 308, at the end of its
//!   `Range: bytes=0-N` (N + 1), at 0 without a `Range`. A 308 that did not get past the start of
//!   the chunk counts as a failed attempt.
//! - Failures (408, 429, 5xx, a rate-limit 403, no reply, a reply that breaks off) go through the
//!   retry policy of the hub (`retry.rs`), with `Retry-After` honored, and the hub's throttle is
//!   waited for before every request. After a failed chunk the session is asked where it stands
//!   (an empty PUT with `Content-Range: bytes */TOTAL`) and the upload continues there. A lost
//!   final reply is resolved the same way: the finished session answers with the file.
//! - A 404 or 410 from the session means it expired: a new session starts from byte 0, at most
//!   [`MAX_SESSIONS`] in all.
//! - A new file gets its id from `files.generateIds` before the upload starts (Drive does not
//!   take one for a conversion). With it, a create whose reply is lost is looked up instead of
//!   repeated, and a repeated create fails with 409 instead of making a second file.
//! - The md5 of the content is computed while it is read, in order, and compared with the
//!   `md5Checksum` Drive returns.
//! - With [`UploadOptions::session_dir`], the session of a resumable upload is recorded there
//!   (`session.rs`), and a later run of the same upload of the unchanged file continues it.
//!
//! Requests to the session URL carry no access token: the URL authorizes them, so an upload that
//! outlasts the token does not depend on refreshing it.

mod body;
#[cfg(test)]
mod mock_drive;
pub(crate) mod session;
#[cfg(test)]
mod tests;

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::{self, Read, Seek},
    path::{Path, PathBuf},
};

use bytes::Bytes;
use error_trace::ErrorTrace;
use http_body_util::BodyExt;
use hyper::{
    body::Incoming,
    header::{HeaderMap, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, RANGE},
    http::request::Builder,
    Method, Request, Response, StatusCode,
};
use mime::Mime;
use tokio::sync::mpsc;

use crate::{
    common::{
        human_size::human_size,
        media_client::{MediaClient, MediaError},
        parse_md5_digest,
        retry::{
            classify_body_error, classify_drive_error, classify_status, classify_transport,
            retry_read, Decision, DisplayDuration, RetryPolicy, RetryReason, RetryState,
            StopReason,
        },
    },
    files::generate_ids,
    hub::Hub,
};

use body::{FileSource, Md5Progress, PipeBody, ReaderSource, Segment, Source};
use session::{RecordFile, RecordTarget, SessionRecord, SourceIdentity};

const MIB: u64 = 1 << 20;

/// Every chunk but the last must be a multiple of this (Google's rule).
pub(crate) const CHUNK_ALIGNMENT: u64 = 256 * 1024;

/// The chunk size of `--chunk-size`'s default, 32 MiB.
pub(crate) const DEFAULT_CHUNK_SIZE: u64 = 32 * MIB;

/// Files of at most this size go up in one multipart request: 5 MiB, Google's recommendation.
pub(crate) const DEFAULT_MULTIPART_THRESHOLD: u64 = 5 * MIB;

/// Upload sessions per upload: the first one and the ones started after it expired.
pub(crate) const MAX_SESSIONS: u32 = 4;

/// The fields of the file resource an upload returns, the ones `files upload` prints.
pub(crate) const FILE_FIELDS: &str = "id,name,size,createdTime,modifiedTime,md5Checksum,\
    mimeType,parents,shared,description,webContentLink,webViewLink";

/// The most of a final reply (the file resource) that is read.
const MAX_REPLY_BODY: usize = 1 << 20;
/// The most of an error reply that is read for its JSON error.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// The Drive file an upload creates or updates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UploadTarget {
    /// The folders of a new file; none for the top folder of My Drive. Not used by an update.
    pub(crate) parents: Vec<String>,
    /// The name of the file on Drive.
    pub(crate) name: String,
    /// The media type of the content, which is also the type of the Drive file unless
    /// `convert_to` is set. `None`: guessed from the source's file name.
    pub(crate) mime: Option<Mime>,
    /// Update this file (a new revision of it, same id) instead of creating one.
    pub(crate) existing_file_id: Option<String>,
    /// Convert the content to this Google Workspace type (an import): the metadata asks for it
    /// while the content goes up as `mime`.
    pub(crate) convert_to: Option<Mime>,
    /// The id the new file gets, from `files.generateIds`. `None`: the upload generates one.
    /// Not used by an update, nor by a conversion (Drive takes no id for those).
    pub(crate) new_file_id: Option<String>,
}

/// How an upload is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UploadOptions {
    /// Bytes per resumable chunk (`--chunk-size`). Rounded down to a multiple of 256 KiB, at
    /// least 256 KiB.
    pub(crate) chunk_size: u64,
    /// Files of at most this many bytes go up in one multipart request, larger ones through a
    /// resumable session (`--multipart-threshold`).
    pub(crate) multipart_threshold: u64,
    /// Compare the md5 Drive computed with the md5 of the local content.
    pub(crate) verify_md5: bool,
    /// Print each failed attempt with its details (`--print-chunk-errors`).
    pub(crate) print_chunk_errors: bool,
    /// Print a line before each chunk is sent (`--print-chunk-info`).
    pub(crate) print_chunk_info: bool,
    /// Where the sessions of resumable uploads are recorded, so that a later run continues an
    /// interrupted one (`uploads/` in the account directory). `None`: an interrupted upload
    /// starts over.
    pub(crate) session_dir: Option<PathBuf>,
}

impl Default for UploadOptions {
    fn default() -> Self {
        UploadOptions {
            chunk_size: DEFAULT_CHUNK_SIZE,
            multipart_threshold: DEFAULT_MULTIPART_THRESHOLD,
            verify_md5: true,
            print_chunk_errors: false,
            print_chunk_info: false,
            session_dir: None,
        }
    }
}

impl UploadOptions {
    /// The chunk size used: `chunk_size` rounded down to a multiple of 256 KiB, at least that.
    #[must_use]
    pub(crate) fn chunk_len(&self) -> u64 {
        (self.chunk_size - self.chunk_size % CHUNK_ALIGNMENT).max(CHUNK_ALIGNMENT)
    }
}

/// Uploads the file at `source` to `target` and returns the Drive file. `progress`, if given, is
/// called with the number of bytes Drive confirmed holding, after every chunk it accepted and
/// at the end. With `opts.session_dir`, an upload of the same file to the same target that an
/// earlier run left unfinished is continued.
pub(crate) async fn upload_path(
    hub: &Hub,
    source: &Path,
    target: &UploadTarget,
    opts: &UploadOptions,
    progress: Option<&(dyn Fn(u64) + Send + Sync)>,
) -> Result<google_drive3::api::File, UploadError> {
    let open_error = |error| UploadError::Open {
        path: source.to_path_buf(),
        source: error,
    };
    let file = tokio::fs::File::open(source).await.map_err(open_error)?;
    let metadata = file.metadata().await.map_err(open_error)?;
    if !metadata.is_file() {
        return Err(UploadError::NotAFile(source.to_path_buf()));
    }

    let media_type = target
        .mime
        .clone()
        .unwrap_or_else(|| mime_guess::from_path(source).first_or_octet_stream());
    let mut upload = Upload::new(
        hub,
        hub.retrier().state(),
        target,
        opts,
        progress,
        metadata.len(),
        media_type,
    );
    if let Some(dir) = &opts.session_dir {
        upload.record_in(dir, SourceIdentity::of(source, &metadata).await);
    }
    upload.run(&mut FileSource::new(file)).await
}

/// [`upload_path`] for a seekable reader of `size` bytes, with the retry policy `policy` (and the
/// hub's throttle). The content type is `target.mime`, `application/octet-stream` without one.
pub(crate) async fn upload_reader<R: Read + Seek + Send>(
    hub: &Hub,
    reader: R,
    size: u64,
    target: &UploadTarget,
    opts: &UploadOptions,
    policy: RetryPolicy,
    progress: Option<&(dyn Fn(u64) + Send + Sync)>,
) -> Result<google_drive3::api::File, UploadError> {
    let media_type = target
        .mime
        .clone()
        .unwrap_or(mime::APPLICATION_OCTET_STREAM);
    let retry = RetryState::new(policy, Some(hub.retrier().throttle.clone()));
    let upload = Upload::new(hub, retry, target, opts, progress, size, media_type);
    upload.run(&mut ReaderSource::new(reader)).await
}

/// The request of an upload that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The request that starts a resumable session.
    Start,
    /// A chunk of a resumable session.
    Chunk,
    /// A status request to a resumable session.
    Status,
    /// A multipart upload.
    Multipart,
}

impl Display for Step {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Step::Start => "starting the upload",
            Step::Chunk => "sending a chunk",
            Step::Status => "asking for the upload status",
            Step::Multipart => "the upload request",
        })
    }
}

/// A failed attempt.
#[derive(Debug)]
pub(crate) enum Failure {
    /// No reply: connection refused or reset, timeout, HTTP/2 GOAWAY or stream reset.
    Transport(hyper_util::client::legacy::Error),
    /// A reply with a failure status. `error` is its JSON error body as google-drive3 reports
    /// one (`BadRequest`), which is what the account hints of `gdrive` look for.
    Status {
        status: StatusCode,
        headers: HeaderMap,
        text: String,
        error: Option<Box<google_drive3::Error>>,
    },
    /// The reply broke off.
    BodyRead(hyper::Error),
    /// A 308 that took none of the chunk.
    NoProgress,
}

impl Failure {
    fn json(&self) -> Option<&serde_json::Value> {
        match self {
            Failure::Status { error, .. } => match error.as_deref() {
                Some(google_drive3::Error::BadRequest(json)) => Some(json),
                _ => None,
            },
            _ => None,
        }
    }

    fn classify(&self) -> Decision {
        match self {
            Failure::Transport(error) => classify_transport(error),
            Failure::Status {
                status, headers, ..
            } => classify_status(*status, Some(headers), self.json()),
            Failure::BodyRead(error) => classify_body_error(error),
            Failure::NoProgress => Decision::Retry {
                after: std::time::Duration::ZERO,
                reason: RetryReason::NoProgress,
            },
        }
    }

    /// The `--print-chunk-errors` line of gdrive 3.x.
    fn print_details(&self, step: Step) {
        let what = if step == Step::Chunk {
            String::from("upload chunk")
        } else {
            step.to_string()
        };
        match self {
            Failure::Transport(error) => {
                eprintln!("Warning: Failed attempt to {what}: {}", error.trace());
            }
            Failure::Status { status, text, .. } => {
                eprintln!(
                    "Warning: Failed attempt to {what}. Status code: {status}, body: {text:?}"
                );
            }
            Failure::BodyRead(error) => {
                eprintln!(
                    "Warning: Failed attempt to {what}, the reply broke off: {}",
                    error.trace()
                );
            }
            Failure::NoProgress => {
                eprintln!("Warning: Failed attempt to {what}: the server accepted none of it");
            }
        }
    }
}

impl Display for Failure {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Transport(_) => f.write_str("the request got no reply"),
            Failure::Status { status, .. } => write!(f, "Drive answered {status}"),
            Failure::BodyRead(_) => f.write_str("the reply broke off"),
            Failure::NoProgress => f.write_str("the server accepted none of the chunk"),
        }
    }
}

/// A failed upload.
#[derive(Debug)]
pub(crate) enum UploadError {
    /// The source could not be opened or examined.
    Open { path: PathBuf, source: io::Error },
    /// The source is not a regular file.
    NotAFile(PathBuf),
    /// Reading the source failed, or it got shorter during the upload.
    Read(io::Error),
    /// No id could be generated for the new file.
    GenerateId(Box<google_drive3::Error>),
    /// A request could not be made: no access token, an invalid URL or header.
    Client(Box<MediaError>),
    /// A request failed and is not tried again; `reason` says why.
    Failed {
        step: Step,
        failure: Box<Failure>,
        reason: StopReason,
    },
    /// A reply that does not follow the protocol.
    InvalidReply { step: Step, problem: &'static str },
    /// The reply to a conversion broke off: the file may have been created, and repeating the
    /// request could create a second one.
    LostReply,
    /// The upload sessions kept expiring.
    SessionsExpired { sessions: u32 },
    /// Looking up whether the file had been created failed.
    Reconcile(Box<google_drive3::Error>),
    /// Drive's md5 of the uploaded file differs from the local content's.
    Md5Mismatch {
        file_id: String,
        local: String,
        remote: String,
    },
}

impl UploadError {
    /// The error as google-drive3 reports one, for callers that return its errors
    /// (`files::upload::upload_file`): a Drive error reply stays the `BadRequest` with its JSON
    /// body, a request without reply the `HttpError`, a missing token `MissingToken`; anything
    /// else becomes an `Io` error with this error inside.
    #[must_use]
    pub(crate) fn into_drive_error(self) -> google_drive3::Error {
        match self {
            UploadError::Failed {
                step,
                failure,
                reason,
            } => match *failure {
                Failure::Status {
                    error: Some(error), ..
                } => *error,
                Failure::Transport(error) => google_drive3::Error::HttpError(error),
                failure => google_drive3::Error::Io(io::Error::other(UploadError::Failed {
                    step,
                    failure: Box::new(failure),
                    reason,
                })),
            },
            UploadError::Client(error) => match *error {
                MediaError::Token(source) => google_drive3::Error::MissingToken(source),
                error => {
                    google_drive3::Error::Io(io::Error::other(UploadError::Client(Box::new(error))))
                }
            },
            UploadError::GenerateId(error) | UploadError::Reconcile(error) => *error,
            error => google_drive3::Error::Io(io::Error::other(error)),
        }
    }
}

impl Display for UploadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            UploadError::Open { path, .. } => {
                write!(f, "unable to open file '{}'", path.display())
            }
            UploadError::NotAFile(path) => write!(
                f,
                "'{}' is not a regular file, only a regular file can be uploaded",
                path.display()
            ),
            UploadError::Read(_) => f.write_str("unable to read the file"),
            UploadError::GenerateId(_) => f.write_str("unable to generate an id for the file"),
            UploadError::Client(_) => f.write_str("unable to send the request"),
            UploadError::Failed {
                step,
                failure,
                reason,
            } => {
                write!(f, "{step} failed: {failure}")?;
                match reason {
                    // Already said by the status.
                    StopReason::NotFound | StopReason::Status(_) => Ok(()),
                    reason => write!(f, " ({reason})"),
                }
            }
            UploadError::InvalidReply { step, problem } => {
                write!(f, "{step}: unexpected reply, {problem}")
            }
            UploadError::LostReply => f.write_str(
                "the reply to the upload broke off; the file may have been created, check \
                 with `gdrive files list` before uploading it again",
            ),
            UploadError::SessionsExpired { sessions } => {
                write!(f, "the upload session expired {sessions} times, giving up")
            }
            UploadError::Reconcile(_) => {
                f.write_str("unable to check whether the upload created the file")
            }
            UploadError::Md5Mismatch {
                file_id,
                local,
                remote,
            } => write!(
                f,
                "the uploaded file {file_id} does not match the local file: Drive's md5 is \
                 {remote}, the local content's is {local}; the local file may have changed \
                 during the upload"
            ),
        }
    }
}

impl Error for UploadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            UploadError::Open { source, .. } | UploadError::Read(source) => Some(source),
            // The boxed errors themselves, not their boxes: callers downcast to them.
            UploadError::GenerateId(source) | UploadError::Reconcile(source) => {
                Some(source.as_ref())
            }
            UploadError::Client(source) => Some(source.as_ref()),
            UploadError::Failed { failure, .. } => match failure.as_ref() {
                Failure::Transport(source) => Some(source),
                Failure::Status {
                    error: Some(source),
                    ..
                } => Some(source.as_ref()),
                Failure::BodyRead(source) => Some(source),
                Failure::Status { error: None, .. } | Failure::NoProgress => None,
            },
            UploadError::NotAFile(_)
            | UploadError::InvalidReply { .. }
            | UploadError::LostReply
            | UploadError::SessionsExpired { .. }
            | UploadError::Md5Mismatch { .. } => None,
        }
    }
}

/// What a request got.
enum Outcome {
    Reply(Response<Incoming>),
    /// No reply.
    Transport(hyper_util::client::legacy::Error),
}

/// How a request is authorized.
#[derive(Clone, Copy)]
enum Auth {
    /// With the access token.
    Token,
    /// By the session URL it goes to.
    Session,
}

/// Where a resumable session stands.
enum Status {
    /// The session holds the bytes before this offset.
    Incomplete(u64),
    /// The upload is complete.
    Complete(Box<google_drive3::api::File>),
    /// The session expired (404 or 410).
    Gone,
}

/// How a session ended.
enum SessionEnd {
    Complete(Box<google_drive3::api::File>),
    /// It expired (404 or 410).
    Gone,
}

/// How starting a session ended.
enum Started {
    Session(String),
    /// The file already exists: an earlier attempt with the same id created it.
    Exists(Box<google_drive3::api::File>),
}

/// The session record of an upload.
struct Recording {
    file: RecordFile,
    dir: PathBuf,
    source: SourceIdentity,
    /// The record of the current session, once there is one.
    record: Option<SessionRecord>,
}

/// What an earlier run left.
enum Resumed {
    /// A session to continue, holding this many bytes.
    Session(String, u64),
    /// A session that completed.
    Complete(Box<google_drive3::api::File>),
}

/// A final reply that could not be used.
enum ReplyError {
    /// It broke off.
    Broken(hyper::Error),
    /// It is not a file resource.
    Invalid,
}

/// One upload, from the first request to the verified file.
struct Upload<'a> {
    hub: &'a Hub,
    client: MediaClient<'a>,
    retry: RetryState,
    target: &'a UploadTarget,
    options: &'a UploadOptions,
    progress: Option<&'a (dyn Fn(u64) + Send + Sync)>,
    /// Names the upload in warnings.
    label: String,
    /// The size of the content.
    total: u64,
    /// The media type of the content.
    media_type: Mime,
    /// The id of a new file (pre-generated), or of the file an update replaces.
    file_id: Option<String>,
    /// The md5 of the content read so far, unless md5 verification is off.
    hasher: Option<Md5Progress>,
    /// The most bytes the server held so far: only bytes past it are progress.
    high_water: u64,
    /// What `progress` was last told.
    reported: Option<u64>,
    /// Where the previous chunk request started, for `--print-chunk-info`.
    previous_chunk: Option<u64>,
    /// The session record, if sessions are recorded.
    recording: Option<Recording>,
}

impl<'a> Upload<'a> {
    fn new(
        hub: &'a Hub,
        retry: RetryState,
        target: &'a UploadTarget,
        options: &'a UploadOptions,
        progress: Option<&'a (dyn Fn(u64) + Send + Sync)>,
        total: u64,
        media_type: Mime,
    ) -> Self {
        // Drive takes no id for a conversion.
        let new_file_id = target
            .new_file_id
            .clone()
            .filter(|_| target.convert_to.is_none());
        let file_id = target.existing_file_id.clone().or(new_file_id);
        Upload {
            hub,
            client: MediaClient::new(hub),
            retry,
            target,
            options,
            progress,
            label: format!("upload of '{}'", target.name),
            total,
            media_type,
            file_id,
            hasher: options.verify_md5.then(Md5Progress::new),
            high_water: 0,
            reported: None,
            previous_chunk: None,
            recording: None,
        }
    }

    /// Records the sessions of the upload of `source` in `dir`.
    fn record_in(&mut self, dir: &Path, source: SourceIdentity) {
        let file = RecordFile::new(dir, &source, &self.record_target());
        self.recording = Some(Recording {
            file,
            dir: dir.to_path_buf(),
            source,
            record: None,
        });
    }

    /// The target as a session record has it.
    fn record_target(&self) -> RecordTarget {
        RecordTarget {
            parents: self.target.parents.clone(),
            name: self.target.name.clone(),
            mime: self.media_type.to_string(),
            existing_file_id: self.target.existing_file_id.clone(),
            convert_to: self.target.convert_to.as_ref().map(ToString::to_string),
            file_id: self.file_id.clone(),
        }
    }

    /// The session an earlier run of this upload recorded, if it can be continued: the
    /// session's URL and the bytes it holds, or the file if it completed. A record that cannot
    /// be used is deleted. The session's file id becomes this upload's.
    async fn resume(&mut self) -> Result<Option<Resumed>, UploadError> {
        let Some(recording) = &self.recording else {
            return Ok(None);
        };
        let Some(record) = recording.file.load().await else {
            return Ok(None);
        };
        if let Some(why) = record.mismatch(&recording.source, &self.record_target()) {
            eprintln!(
                "Warning: {}: not continuing the session of an earlier run, {why}",
                self.label
            );
            self.forget().await;
            return Ok(None);
        }

        if !self.is_update() {
            self.file_id.clone_from(&record.target.file_id);
        }
        match self.query_status(&record.session_url).await {
            Ok(Status::Incomplete(offset)) => {
                eprintln!(
                    "Continuing the {} from an earlier run at {} of {}",
                    self.label,
                    human_size(offset),
                    human_size(self.total)
                );
                self.high_water = offset;
                self.report(offset);
                let url = record.session_url.clone();
                if let Some(recording) = &mut self.recording {
                    recording.record = Some(record);
                }
                Ok(Some(Resumed::Session(url, offset)))
            }
            Ok(Status::Complete(file)) => {
                self.forget().await;
                Ok(Some(Resumed::Complete(file)))
            }
            // The network or Drive is down: the next run tries again.
            Err(
                error @ (UploadError::Failed {
                    reason: StopReason::GaveUp { .. },
                    ..
                }
                | UploadError::Client(_)),
            ) => Err(error),
            // The session is gone, or answers what no session should: start again, with the
            // same id. If that session had completed after all, the new one gets a 409 and
            // the file is looked up.
            Ok(Status::Gone) | Err(_) => {
                self.forget().await;
                Ok(None)
            }
        }
    }

    /// Records the session just started at `url`.
    async fn record_session(&mut self, url: &str) {
        let target = self.record_target();
        let chunk_size = self.options.chunk_len();
        if let Some(recording) = &mut self.recording {
            session::prune(&recording.dir).await;
            let record =
                SessionRecord::new(url.to_owned(), recording.source.clone(), target, chunk_size);
            recording.file.save(&record).await;
            recording.record = Some(record);
        }
    }

    /// Records that the session holds `offset` bytes.
    async fn checkpoint(&mut self, offset: u64) {
        if let Some(Recording {
            file,
            record: Some(record),
            ..
        }) = &mut self.recording
        {
            if record.offset != offset {
                record.offset = offset;
                file.save(record).await;
            }
        }
    }

    /// Deletes the record: the session is over.
    async fn forget(&mut self) {
        if let Some(recording) = &mut self.recording {
            recording.record = None;
            recording.file.remove().await;
        }
    }

    async fn run<S: Source>(
        mut self,
        source: &mut S,
    ) -> Result<google_drive3::api::File, UploadError> {
        let file = if self.total <= self.options.multipart_threshold {
            self.ensure_id().await?;
            self.multipart(source).await?
        } else {
            self.resumable(source).await?
        };
        self.report(self.total);
        self.verify(source, &file).await?;
        Ok(file)
    }

    fn is_update(&self) -> bool {
        self.target.existing_file_id.is_some()
    }

    /// The pre-generated id of a new file.
    fn created_id(&self) -> Option<&str> {
        if self.is_update() {
            None
        } else {
            self.file_id.as_deref()
        }
    }

    /// Whether a request whose reply was lost may be sent again: a create with an id (a
    /// repeat fails with 409 instead of creating a second file) or an update (a repeat is one
    /// more revision of the same content).
    fn replay_safe(&self) -> bool {
        self.is_update() || self.created_id().is_some()
    }

    /// Gets an id for a new file from Drive, unless it has one or is a conversion.
    async fn ensure_id(&mut self) -> Result<(), UploadError> {
        if self.file_id.is_some() || self.target.convert_to.is_some() {
            return Ok(());
        }
        let ids = generate_ids(self.hub, 1)
            .await
            .map_err(UploadError::GenerateId)?;
        self.file_id = ids.into_iter().next();
        Ok(())
    }

    /// The metadata JSON: the name for an update (the file keeps its type and folders), the
    /// id, name, type and folders for a new file.
    fn metadata(&self) -> String {
        let mut metadata = serde_json::Map::new();
        if let Some(id) = self.created_id() {
            metadata.insert("id".into(), id.into());
        }
        metadata.insert("name".into(), self.target.name.as_str().into());
        if !self.is_update() {
            let mime_type = self.target.convert_to.as_ref().unwrap_or(&self.media_type);
            metadata.insert("mimeType".into(), mime_type.as_ref().into());
            if !self.target.parents.is_empty() {
                metadata.insert("parents".into(), self.target.parents.clone().into());
            }
        }
        serde_json::Value::Object(metadata).to_string()
    }

    fn method(&self) -> Method {
        if self.is_update() {
            Method::PATCH
        } else {
            Method::POST
        }
    }

    fn upload_url(&self, upload_type: &str) -> Result<url::Url, UploadError> {
        let mut segments = vec!["files"];
        if let Some(id) = &self.target.existing_file_id {
            segments.push(id);
        }
        self.client
            .upload_url(
                &segments,
                &[
                    ("uploadType", upload_type),
                    ("supportsAllDrives", "true"),
                    ("fields", FILE_FIELDS),
                ],
            )
            .map_err(|error| UploadError::Client(Box::new(error)))
    }

    /// Tells `progress` and the session record that the server holds `offset` bytes, and
    /// starts a new retry budget if that is more than it ever held.
    async fn accepted(&mut self, offset: u64) {
        if offset > self.high_water {
            self.high_water = offset;
            self.retry.progress();
        }
        self.report(offset);
        self.checkpoint(offset).await;
    }

    fn report(&mut self, offset: u64) {
        if self.reported != Some(offset) {
            self.reported = Some(offset);
            if let Some(progress) = self.progress {
                progress(offset);
            }
        }
    }

    /// Counts a failed attempt: waits before the next one, or returns the error once the
    /// failure is not worth repeating or the retry budget is used up.
    async fn backoff(&mut self, step: Step, failure: Failure) -> Result<(), UploadError> {
        match self.retry.next(failure.classify()) {
            Decision::Retry { after, reason } => {
                if self.options.print_chunk_errors {
                    failure.print_details(step);
                }
                eprintln!(
                    "Warning: {}: {step} failed ({reason}), retrying in {} (attempt {} of {})",
                    self.label,
                    DisplayDuration(after),
                    self.retry.failures() + 1,
                    self.retry.policy().max_attempts,
                );
                tokio::time::sleep(after).await;
                Ok(())
            }
            Decision::Stop { reason } => Err(UploadError::Failed {
                step,
                failure: Box::new(failure),
                reason,
            }),
        }
    }

    /// Sends a request with a small body, after waiting for the hub's throttle.
    async fn send_bytes(
        &self,
        request: Builder,
        body: Bytes,
        auth: Auth,
    ) -> Result<Outcome, UploadError> {
        self.hub.retrier().throttle.wait().await;
        let request = request
            .header(CONTENT_LENGTH, body.len())
            .body(google_apis_common::to_body(Some(body)))
            .map_err(|error| UploadError::Client(Box::new(MediaError::Request(error))))?;
        dispatch(&self.client, request, auth).await
    }

    /// Sends a request whose body is `segment`, streamed from `source`, after waiting for the
    /// hub's throttle. A read error of the source ends the request and is returned.
    async fn send_streamed<S: Source>(
        &mut self,
        request: Builder,
        source: &mut S,
        segment: &Segment,
        auth: Auth,
    ) -> Result<Outcome, UploadError> {
        self.hub.retrier().throttle.wait().await;
        let (pieces, received) = mpsc::channel(body::DEPTH);
        let len = segment.body_len();
        let request = request
            .header(CONTENT_LENGTH, len)
            .body(PipeBody::new(received, len).boxed())
            .map_err(|error| UploadError::Client(Box::new(MediaError::Request(error))))?;

        let send = dispatch(&self.client, request, auth);
        let pump = body::pump(source, self.hasher.as_mut(), segment, pieces);
        tokio::pin!(send, pump);
        let mut pumped = false;
        loop {
            tokio::select! {
                outcome = &mut send => return outcome,
                read = &mut pump, if !pumped => {
                    pumped = true;
                    read.map_err(UploadError::Read)?;
                }
            }
        }
    }

    /// The Drive file of a resumable upload: a session started, its chunks sent, again in a
    /// new session when one expires.
    async fn resumable<S: Source>(
        &mut self,
        source: &mut S,
    ) -> Result<google_drive3::api::File, UploadError> {
        let mut resumed = match self.resume().await? {
            Some(Resumed::Complete(file)) => return Ok(*file),
            Some(Resumed::Session(url, offset)) => Some((url, offset)),
            None => None,
        };
        self.ensure_id().await?;
        let mut sessions = 0;
        loop {
            sessions += 1;
            let (url, offset) = match resumed.take() {
                Some(session) => session,
                None => match self.start().await? {
                    Started::Session(url) => {
                        self.record_session(&url).await;
                        (url, 0)
                    }
                    Started::Exists(file) => return Ok(*file),
                },
            };
            let end = self.send_chunks(source, &url, offset).await?;
            self.forget().await;
            match end {
                SessionEnd::Complete(file) => return Ok(*file),
                SessionEnd::Gone => {}
            }
            if let Some(file) = self.reconcile().await? {
                return Ok(file);
            }
            if sessions >= MAX_SESSIONS {
                return Err(UploadError::SessionsExpired { sessions });
            }
            eprintln!(
                "Warning: {}: the upload session expired, starting a new one",
                self.label
            );
        }
    }

    /// Starts a resumable session.
    async fn start(&mut self) -> Result<Started, UploadError> {
        let metadata = Bytes::from(self.metadata());
        loop {
            let url = self.upload_url("resumable")?;
            let request = Request::builder()
                .method(self.method())
                .uri(url.as_str())
                .header(CONTENT_TYPE, "application/json; charset=UTF-8")
                .header("X-Upload-Content-Type", self.media_type.as_ref())
                .header("X-Upload-Content-Length", self.total);
            let failure = match self
                .send_bytes(request, metadata.clone(), Auth::Token)
                .await?
            {
                Outcome::Reply(response) if response.status().is_success() => {
                    return match response
                        .headers()
                        .get(LOCATION)
                        .and_then(|value| value.to_str().ok())
                    {
                        Some(location) => Ok(Started::Session(location.to_owned())),
                        None => Err(UploadError::InvalidReply {
                            step: Step::Start,
                            problem: "it has no session URL (Location)",
                        }),
                    };
                }
                Outcome::Reply(response) => {
                    if response.status() == StatusCode::CONFLICT {
                        if let Some(file) = self.reconcile().await? {
                            return Ok(Started::Exists(Box::new(file)));
                        }
                    }
                    read_failure(response).await
                }
                Outcome::Transport(error) => Failure::Transport(error),
            };
            self.backoff(Step::Start, failure).await?;
        }
    }

    /// Sends the content from `offset` on to the session at `url`, until the session is
    /// complete or gone.
    async fn send_chunks<S: Source>(
        &mut self,
        source: &mut S,
        url: &str,
        mut offset: u64,
    ) -> Result<SessionEnd, UploadError> {
        let chunk_len = self.options.chunk_len();
        loop {
            if offset >= self.total {
                // The server holds every byte but sent no final reply: ask for it.
                match self.query_status(url).await? {
                    Status::Incomplete(next) if next < self.total => {
                        offset = next;
                        continue;
                    }
                    Status::Incomplete(_) => {
                        return Err(UploadError::InvalidReply {
                            step: Step::Status,
                            problem: "the session holds every byte but does not finish",
                        })
                    }
                    Status::Complete(file) => return Ok(SessionEnd::Complete(file)),
                    Status::Gone => return Ok(SessionEnd::Gone),
                }
            }

            let len = chunk_len.min(self.total - offset);
            self.print_chunk_info(offset, len);
            if let Some(hasher) = &mut self.hasher {
                hasher
                    .catch_up(source, offset)
                    .await
                    .map_err(UploadError::Read)?;
            }
            let request = Request::put(url)
                .header(
                    "Content-Range",
                    format!("bytes {offset}-{}/{}", offset + len - 1, self.total),
                )
                .header(CONTENT_TYPE, self.media_type.as_ref());
            let segment = Segment::content(offset, len);
            let failure = match self
                .send_streamed(request, source, &segment, Auth::Session)
                .await?
            {
                Outcome::Reply(response) => match response.status() {
                    StatusCode::PERMANENT_REDIRECT => {
                        let next = self.next_offset(response.headers(), Step::Chunk)?;
                        if next > offset {
                            self.accepted(next).await;
                            offset = next;
                            continue;
                        }
                        // The server lost bytes or took none: follow it, after a backoff.
                        offset = next;
                        Failure::NoProgress
                    }
                    status if status.is_success() => match read_file(response.into_body()).await {
                        Ok(file) => return Ok(SessionEnd::Complete(Box::new(file))),
                        Err(ReplyError::Broken(error)) => Failure::BodyRead(error),
                        Err(ReplyError::Invalid) => {
                            return Err(UploadError::InvalidReply {
                                step: Step::Chunk,
                                problem: "the final reply is not a file resource",
                            })
                        }
                    },
                    StatusCode::NOT_FOUND | StatusCode::GONE => return Ok(SessionEnd::Gone),
                    _ => read_failure(response).await,
                },
                Outcome::Transport(error) => Failure::Transport(error),
            };

            let ask = !matches!(failure, Failure::NoProgress);
            self.backoff(Step::Chunk, failure).await?;
            if ask {
                match self.query_status(url).await? {
                    Status::Incomplete(next) => {
                        self.accepted(next).await;
                        offset = next;
                    }
                    Status::Complete(file) => return Ok(SessionEnd::Complete(file)),
                    Status::Gone => return Ok(SessionEnd::Gone),
                }
            }
        }
    }

    /// Asks the session at `url` where it stands.
    async fn query_status(&mut self, url: &str) -> Result<Status, UploadError> {
        loop {
            let request =
                Request::put(url).header("Content-Range", format!("bytes */{}", self.total));
            let failure = match self
                .send_bytes(request, Bytes::new(), Auth::Session)
                .await?
            {
                Outcome::Reply(response) => match response.status() {
                    StatusCode::PERMANENT_REDIRECT => {
                        let next = self.next_offset(response.headers(), Step::Status)?;
                        return Ok(Status::Incomplete(next));
                    }
                    status if status.is_success() => match read_file(response.into_body()).await {
                        Ok(file) => return Ok(Status::Complete(Box::new(file))),
                        Err(ReplyError::Broken(error)) => Failure::BodyRead(error),
                        Err(ReplyError::Invalid) => {
                            return Err(UploadError::InvalidReply {
                                step: Step::Status,
                                problem: "the final reply is not a file resource",
                            })
                        }
                    },
                    StatusCode::NOT_FOUND | StatusCode::GONE => return Ok(Status::Gone),
                    _ => read_failure(response).await,
                },
                Outcome::Transport(error) => Failure::Transport(error),
            };
            self.backoff(Step::Status, failure).await?;
        }
    }

    /// The offset a 308 says to continue at.
    fn next_offset(&self, headers: &HeaderMap, step: Step) -> Result<u64, UploadError> {
        next_offset(headers, self.total).ok_or(UploadError::InvalidReply {
            step,
            problem: "the Range of the 308 reply is not one of the content",
        })
    }

    fn print_chunk_info(&mut self, offset: u64, len: u64) {
        if self.options.print_chunk_info {
            let action = if self.previous_chunk == Some(offset) {
                "Retrying"
            } else {
                "Uploading"
            };
            println!(
                "Info: {action} {} chunk ({offset}-{} of {})",
                human_size(len),
                offset + len - 1,
                self.total
            );
        }
        self.previous_chunk = Some(offset);
    }

    /// The Drive file of a multipart upload: the metadata and the content in one request.
    async fn multipart<S: Source>(
        &mut self,
        source: &mut S,
    ) -> Result<google_drive3::api::File, UploadError> {
        let boundary = format!(
            "gdrive-{:016x}{:016x}",
            fastrand::u64(..),
            fastrand::u64(..)
        );
        let prefix = format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{}\r\n\
             --{boundary}\r\nContent-Type: {}\r\n\r\n",
            self.metadata(),
            self.media_type
        );
        let segment = Segment {
            offset: 0,
            len: self.total,
            prefix: Bytes::from(prefix),
            suffix: Bytes::from(format!("\r\n--{boundary}--\r\n")),
        };
        let content_type = format!("multipart/related; boundary={boundary}");
        loop {
            let url = self.upload_url("multipart")?;
            let request = Request::builder()
                .method(self.method())
                .uri(url.as_str())
                .header(CONTENT_TYPE, content_type.as_str());
            let failure = match self
                .send_streamed(request, source, &segment, Auth::Token)
                .await?
            {
                Outcome::Reply(response) if response.status().is_success() => {
                    match read_file(response.into_body()).await {
                        Ok(file) => return Ok(file),
                        Err(ReplyError::Broken(error)) if self.replay_safe() => {
                            Failure::BodyRead(error)
                        }
                        Err(ReplyError::Broken(_)) => return Err(UploadError::LostReply),
                        Err(ReplyError::Invalid) => {
                            return Err(UploadError::InvalidReply {
                                step: Step::Multipart,
                                problem: "it is not a file resource",
                            })
                        }
                    }
                }
                Outcome::Reply(response) => {
                    if response.status() == StatusCode::CONFLICT {
                        if let Some(file) = self.reconcile().await? {
                            return Ok(file);
                        }
                    }
                    read_failure(response).await
                }
                Outcome::Transport(error) => Failure::Transport(error),
            };
            self.backoff(Step::Multipart, failure).await?;
        }
    }

    /// The new file, if an earlier attempt created it: looked up by its pre-generated id.
    async fn reconcile(&self) -> Result<Option<google_drive3::api::File>, UploadError> {
        let Some(id) = self.created_id() else {
            return Ok(None);
        };
        let hub = self.hub;
        let result = retry_read(hub.retrier(), "files.get", |mut delegate| async move {
            hub.files()
                .get(id)
                .param("fields", FILE_FIELDS)
                .supports_all_drives(true)
                .add_scope(google_drive3::api::Scope::Full)
                .delegate(&mut delegate)
                .doit()
                .await
        })
        .await;
        match result {
            Ok((_, file)) => Ok(Some(file)),
            Err(error)
                if classify_drive_error(&error, None)
                    == (Decision::Stop {
                        reason: StopReason::NotFound,
                    }) =>
            {
                Ok(None)
            }
            Err(error) => Err(UploadError::Reconcile(Box::new(error))),
        }
    }

    /// Compares Drive's md5 of `file` with the md5 of the local content. Files without one
    /// (Google Workspace files, conversions) are not checked.
    async fn verify<S: Source>(
        &mut self,
        source: &mut S,
        file: &google_drive3::api::File,
    ) -> Result<(), UploadError> {
        let (Some(hasher), Some(remote)) = (&mut self.hasher, file.md5_checksum.as_deref()) else {
            return Ok(());
        };
        hasher
            .catch_up(source, self.total)
            .await
            .map_err(UploadError::Read)?;
        let local = hasher.digest();
        if parse_md5_digest(remote) == Some(local) {
            Ok(())
        } else {
            Err(UploadError::Md5Mismatch {
                file_id: file.id.clone().unwrap_or_default(),
                local: format!("{local:x}"),
                remote: remote.to_owned(),
            })
        }
    }
}

/// Sends `request` with the access token or without (to a session URL).
async fn dispatch(
    client: &MediaClient<'_>,
    request: Request<google_apis_common::Body>,
    auth: Auth,
) -> Result<Outcome, UploadError> {
    let result = match auth {
        Auth::Token => client.send(request).await,
        Auth::Session => client.send_without_token(request).await,
    };
    match result {
        Ok(response) => Ok(Outcome::Reply(response)),
        Err(MediaError::Transport { source, .. }) => Ok(Outcome::Transport(source)),
        Err(error) => Err(UploadError::Client(Box::new(error))),
    }
}

/// The offset to continue at after a 308 Resume Incomplete: after the last byte of its
/// `Range: bytes=0-N`, or 0 without a `Range` (the server holds nothing). `None` for a `Range`
/// that cannot be parsed, does not start at 0, or ends past `total`.
fn next_offset(headers: &HeaderMap, total: u64) -> Option<u64> {
    let Some(value) = headers.get(RANGE) else {
        return Some(0);
    };
    let value = value.to_str().ok()?.trim();
    let range = value
        .strip_prefix("bytes=")
        .or_else(|| value.strip_prefix("bytes "))?;
    let (first, last) = range.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    let next = last.checked_add(1)?;
    (first == 0 && next <= total).then_some(next)
}

/// The file resource of a final reply.
async fn read_file(body: Incoming) -> Result<google_drive3::api::File, ReplyError> {
    let bytes = match http_body_util::Limited::new(body, MAX_REPLY_BODY)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return Err(match error.downcast::<hyper::Error>() {
                Ok(error) => ReplyError::Broken(*error),
                Err(_) => ReplyError::Invalid,
            })
        }
    };
    serde_json::from_slice(&bytes).map_err(|_| ReplyError::Invalid)
}

/// The failure a reply with a failure status is.
async fn read_failure(response: Response<Incoming>) -> Failure {
    let (parts, body) = response.into_parts();
    let bytes = http_body_util::Limited::new(body, MAX_ERROR_BODY)
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    let error = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .map(|json| Box::new(google_drive3::Error::BadRequest(json)));
    Failure::Status {
        status: parts.status,
        headers: parts.headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
        error,
    }
}
