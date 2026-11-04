//! Copies the content of a Drive file of one account into a new file of another account,
//! streaming: no local file, memory bounded.
//!
//! - A blob is read from the source account (`files.get?alt=media`) by [`DriveSource`], the
//!   [`Source`] of gdrive's uploader (`upload.rs`), which sends it to the destination account
//!   in a resumable session (or one multipart request under
//!   [`UploadOptions::multipart_threshold`]). The source's reply is read as the upload asks for
//!   the bytes: the uploader holds at most a few pieces of 256 KiB, and the source's body at
//!   most one frame, so the memory of a relay does not depend on the size of the file.
//! - When the upload asks for bytes the open reply is not at (a chunk is sent again after the
//!   destination failed, from the offset the session holds), the content is requested again
//!   from that offset with `Range` and, with a strong `ETag`, `If-Range`. A reply that breaks
//!   off is requested again the same way, within the source hub's retry policy. Before each such
//!   request, the source's metadata is fetched again: a file whose size or `md5Checksum`
//!   changed (its `version` and `modifiedTime` when it has no md5) fails with
//!   [`RelayError::SourceChanged`] rather than mixing two revisions in the copy.
//! - A resume continues only content that is provably the same. A reply with the whole content
//!   to a `Range` request says that the content is not the one of the `If-Range` (or that the
//!   server ignores `Range`): the metadata is fetched again, and only a source with an
//!   `md5Checksum` whose md5 and size are unchanged is read on, up to the offset when that is
//!   cheap ([`DISCARD_LIMIT`]), else asked for again without `If-Range`. A source without an
//!   md5 has nothing to prove that: it fails with [`RelayError::SourceChanged`], as does any
//!   resume of a source with neither an md5 nor a strong `ETag`.
//! - A file shared by link only is read with its `resourceKey` in every request
//!   (`X-Goog-Drive-Resource-Keys`): metadata, content, resumes and exports.
//! - The md5 Drive computes for the copy is compared with the source's `md5Checksum`, on top of
//!   the uploader's own check of the bytes it sent.
//! - The copy gets the source's `modifiedTime` and `createdTime`.
//! - A Google Workspace file is exported (Docs as docx, Sheets as xlsx, Slides as pptx) into
//!   memory, at most [`EXPORT_LIMIT`] bytes, and uploaded with conversion back to its type.
//!   Drawings are not supported: Drive imports no format it exports them as (its
//!   `importFormats` turn images into Documents), so a copy would not be a drawing.
//! - With [`UploadOptions::session_dir`], the session of a resumable relay is recorded with the
//!   identity of the source ([`SourceIdentity::remote`]: the source's account, file id, size
//!   and md5), and a later relay of the same source to the same target continues it, or takes
//!   the file of a session that completed, without reading the bytes the session holds again.
//!   A source without an md5 is not recorded (nothing would prove the content the same), and
//!   neither are exports: an export is not the same bytes twice.

#[cfg(test)]
mod tests;

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::{self, Cursor},
    time::Duration,
};

use bytes::{Buf, Bytes};
use http_body_util::BodyExt;
use hyper::{Request, StatusCode};
use mime::Mime;

use crate::{
    common::{
        drive_file::{
            DocType, MIME_TYPE_DOCX_MIME, MIME_TYPE_DRIVE_SHORTCUT, MIME_TYPE_PPTX_MIME,
            MIME_TYPE_XLSX_MIME,
        },
        media_client::{get_file, MediaClient, MediaError},
        parse_md5_digest,
        retry::{
            classify_body_error, retry_read, Decision, DisplayDuration, RetryReason, RetryState,
        },
    },
    files::{
        download::resume::{
            content_length, content_range, strong_etag, Identity, MediaSource, Revision,
        },
        export::is_export_too_large,
    },
    hub::Hub,
    // Warnings go above the progress bars.
    transfer::{
        progress::eprintln,
        upload::{
            session::SourceIdentity, FileTimes, ReaderSource, Source, Upload, UploadError,
            UploadOptions, UploadTarget,
        },
    },
};

const MIB: u64 = 1 << 20;

/// Drive's limit on the size of an export (the only buffer the relay holds in memory).
pub(crate) const EXPORT_LIMIT: u64 = 10 * MIB;

/// The most bytes of a reply that are read and dropped to reach an offset, rather than asked
/// for again from that offset.
const DISCARD_LIMIT: u64 = 4 * MIB;

/// The fields of the source a relay uses, fetched when the listing lacks some.
const SOURCE_FIELDS: &str =
    "id,name,mimeType,size,md5Checksum,modifiedTime,createdTime,version,resourceKey";

const GOOGLE_APPS_PREFIX: &str = "application/vnd.google-apps.";

/// Why a file cannot be relayed, known from its metadata alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unsupported {
    /// `application/vnd.google-apps.shortcut`: Drive has no content for it.
    Shortcut,
    /// A Google Workspace type without an export format (forms, sites, maps, jamboard, ...),
    /// or whose exports Drive cannot convert back (drawings).
    NativeType(String),
}

impl Display for Unsupported {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Unsupported::Shortcut => f.write_str("shortcuts have no content"),
            Unsupported::NativeType(mime) => {
                let kind = mime.strip_prefix(GOOGLE_APPS_PREFIX).unwrap_or(mime);
                match kind {
                    "folder" => f.write_str("folders have no content"),
                    "drawing" => f.write_str(
                        "Google Drawings cannot be copied: Drive converts no export back to a \
                         drawing",
                    ),
                    "form" => f.write_str("Google Forms cannot be exported"),
                    "site" => f.write_str("Google Sites cannot be exported"),
                    "map" => f.write_str("Google My Maps cannot be exported"),
                    "jam" => f.write_str("Jamboard files cannot be exported"),
                    "script" => f.write_str("Apps Script projects cannot be exported"),
                    _ => write!(f, "files of type {mime} cannot be exported"),
                }
            }
        }
    }
}

/// How the content of a file is relayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Its bytes, as they are.
    Blob,
    /// An export of a Workspace file.
    Export(Export),
}

/// The export of a Workspace file, which the destination converts back to `doc_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Export {
    doc_type: DocType,
    /// The format of the export.
    format: &'static Mime,
}

/// The export of a Workspace file of type `mime`: the Office format Drive converts back to
/// that type. Drawings have none: Drive exports them as images and PDF, and imports images as
/// Documents only.
fn export_of(mime: &str) -> Option<Export> {
    let doc_type = DocType::from_mime_type(mime)?;
    let format: &'static Mime = match doc_type {
        DocType::Document => &MIME_TYPE_DOCX_MIME,
        DocType::Spreadsheet => &MIME_TYPE_XLSX_MIME,
        DocType::Presentation => &MIME_TYPE_PPTX_MIME,
        DocType::Drawing => return None,
    };
    Some(Export { doc_type, format })
}

/// How a file of type `mime` is relayed. Without a type, it is taken for a blob: the relay
/// fetches the type before it decides.
fn plan(mime: Option<&str>) -> Result<Plan, Unsupported> {
    let Some(mime) = mime else {
        return Ok(Plan::Blob);
    };
    if mime == MIME_TYPE_DRIVE_SHORTCUT {
        return Err(Unsupported::Shortcut);
    }
    if let Some(export) = export_of(mime) {
        return Ok(Plan::Export(export));
    }
    // Folders too: `application/vnd.google-apps.folder`.
    if mime.starts_with(GOOGLE_APPS_PREFIX) {
        return Err(Unsupported::NativeType(mime.to_owned()));
    }
    Ok(Plan::Blob)
}

/// From metadata alone (`mimeType` is required): Ok for a blob or an exportable Workspace file.
pub(crate) fn check_supported(file: &google_drive3::api::File) -> Result<(), Unsupported> {
    plan(file.mime_type.as_deref()).map(|_| ())
}

/// A failed relay.
#[derive(Debug)]
pub(crate) enum RelayError {
    Unsupported(Unsupported),
    /// Reading the source's content failed for good (after the hub's retry policy).
    Source(MediaError),
    /// A Workspace file's export is larger than `EXPORT_LIMIT`.
    ExportTooLarge {
        limit: u64,
    },
    Upload(UploadError),
    /// Drive's md5 of the new file differs from the source's `md5Checksum`.
    Md5Mismatch {
        expected: String,
        actual: String,
    },
    /// The source changed (size, md5Checksum or version) while it was being read.
    SourceChanged,
    /// A metadata request about the source failed (files.get for missing fields).
    SourceMetadata(Box<google_drive3::Error>),
}

impl RelayError {
    /// Why the file cannot be relayed at all, if that is why it failed.
    #[must_use]
    pub(crate) fn unsupported(&self) -> Option<&Unsupported> {
        match self {
            RelayError::Unsupported(unsupported) => Some(unsupported),
            _ => None,
        }
    }

    /// The relay error of a failed upload: a failed read of the source is the source's.
    fn of_upload(error: UploadError) -> Self {
        match error {
            UploadError::Read(error) => match SourceFailure::take(error) {
                Ok(SourceFailure::Media(error)) => RelayError::Source(error),
                Ok(SourceFailure::Changed) => RelayError::SourceChanged,
                Err(error) => RelayError::Upload(UploadError::Read(error)),
            },
            // The uploader's own check: the bytes it read are not what Drive holds.
            UploadError::Md5Mismatch { local, remote, .. } => RelayError::Md5Mismatch {
                expected: local,
                actual: remote,
            },
            UploadError::SourceChanged { .. } => RelayError::SourceChanged,
            error => RelayError::Upload(error),
        }
    }

    /// The source's metadata lacks something the relay needs.
    fn incomplete_metadata(what: &str) -> Self {
        RelayError::SourceMetadata(Box::new(google_drive3::Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Drive reports no {what} for the source file"),
        ))))
    }
}

impl Display for RelayError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            RelayError::Unsupported(unsupported) => unsupported.fmt(f),
            RelayError::Source(_) => f.write_str("unable to read the source file"),
            RelayError::ExportTooLarge { limit } => write!(
                f,
                "the exported file is larger than the {} that the Drive API exports",
                crate::common::human_size::human_size(*limit)
            ),
            RelayError::Upload(_) => f.write_str("unable to upload the copy"),
            RelayError::Md5Mismatch { expected, actual } => write!(
                f,
                "the copy does not match the source: its md5 is {actual}, the source's is \
                 {expected}"
            ),
            RelayError::SourceChanged => {
                f.write_str("the source file changed while it was being copied; copy it again")
            }
            RelayError::SourceMetadata(_) => {
                f.write_str("unable to read the metadata of the source file")
            }
        }
    }
}

impl Error for RelayError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            RelayError::Source(source) => Some(source),
            RelayError::Upload(source) => Some(source),
            // The boxed error itself, not its box: callers downcast to it.
            RelayError::SourceMetadata(source) => Some(source.as_ref()),
            RelayError::Unsupported(_)
            | RelayError::ExportTooLarge { .. }
            | RelayError::Md5Mismatch { .. }
            | RelayError::SourceChanged => None,
        }
    }
}

/// Relays `source_file` from the account of `source` to a NEW file of the account of `dest`.
/// `source_file` comes from a listing with at least `id, name, mimeType`; `size`,
/// `md5Checksum`, `modifiedTime`, `createdTime`, `version` are used when present and fetched
/// with files.get when a blob lacks `size` or `md5Checksum`. The new file goes to
/// `target.parents`, is named `target.name` (the source's name when None) and gets the source's
/// type: `target.mime`, `target.convert_to` and `target.existing_file_id` are ignored;
/// `target.new_file_id` (pre-generated) is honoured for blobs. The source's `modifiedTime` (and
/// `createdTime`) are set on the new file. Blobs stream in pieces straight into the resumable
/// (or multipart, under `opts.multipart_threshold`) upload: nothing is written to disk and
/// memory is a few MiB per call. A Workspace file is exported (docx/xlsx/pptx/... for its type)
/// into memory (at most `EXPORT_LIMIT`) and uploaded with conversion back to the same Workspace
/// type. `progress` gets the bytes Drive confirmed, as `upload_path` does. With
/// `opts.session_dir`, an interrupted relay of the same source (id, size,
/// md5Checksum/version) to the same target is continued by a later run. The returned file has
/// `upload::FILE_FIELDS`.
///
/// The md5 checks follow `opts.verify_md5`. A continued session is checked by the md5s alone,
/// without reading the bytes it holds again; a source without `md5Checksum` is copied
/// unchecked end to end, is never recorded, and is resumed only with a strong `ETag`. A
/// `resourceKey` of `source_file` is sent with every request about it.
pub(crate) async fn relay_file(
    source: &Hub,
    dest: &Hub,
    source_file: &google_drive3::api::File,
    target: &UploadTarget,
    opts: &UploadOptions,
    progress: Option<&(dyn Fn(u64) + Send + Sync)>,
) -> Result<google_drive3::api::File, RelayError> {
    let id = source_file
        .id
        .as_deref()
        .ok_or_else(|| RelayError::incomplete_metadata("id"))?;
    let listed = plan(source_file.mime_type.as_deref()).map_err(RelayError::Unsupported)?;
    let lacks_metadata = source_file.mime_type.is_none()
        || (listed == Plan::Blob
            && (source_file.size.is_none() || source_file.md5_checksum.is_none()));
    let resource_key = source_file.resource_key.as_deref();
    let fetched;
    let file = if lacks_metadata {
        fetched = get_file(source, id, SOURCE_FIELDS, resource_key)
            .await
            .map_err(RelayError::SourceMetadata)?;
        &fetched
    } else {
        source_file
    };
    let plan = plan(file.mime_type.as_deref()).map_err(RelayError::Unsupported)?;

    let name = target.name.clone().or_else(|| file.name.clone());
    let shown = name
        .as_deref()
        .unwrap_or_default()
        .escape_debug()
        .to_string();
    let relay = Relay {
        source,
        dest,
        file,
        id,
        resource_key,
        parents: &target.parents,
        name,
        new_file_id: target.new_file_id.as_deref(),
        times: FileTimes {
            modified: file.modified_time,
            created: file.created_time,
        },
        opts,
        progress,
    };
    match plan {
        Plan::Blob => relay.blob(format!("download of '{shown}'")).await,
        Plan::Export(export) => relay.export(export, format!("export of '{shown}'")).await,
    }
}

/// The Google account `hub` acts as, as Drive names it.
async fn account_of(hub: &Hub) -> Result<Option<String>, Box<google_drive3::Error>> {
    let (_, about) = retry_read(hub.retrier(), "about.get", |mut delegate| async move {
        hub.about()
            .get()
            .param("fields", "user(emailAddress)")
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .doit()
            .await
    })
    .await
    .map_err(Box::new)?;
    Ok(about.user.and_then(|user| user.email_address))
}

/// Whether two md5s in hex are the same.
fn same_md5(one: &str, other: &str) -> bool {
    match (parse_md5_digest(one), parse_md5_digest(other)) {
        (Some(one), Some(other)) => one == other,
        _ => one.eq_ignore_ascii_case(other),
    }
}

/// One relay, once the source's metadata is complete.
struct Relay<'a> {
    source: &'a Hub,
    dest: &'a Hub,
    file: &'a google_drive3::api::File,
    id: &'a str,
    /// The source's `resourceKey`, named in every request about it.
    resource_key: Option<&'a str>,
    parents: &'a [String],
    /// The name of the copy.
    name: Option<String>,
    new_file_id: Option<&'a str>,
    times: FileTimes,
    opts: &'a UploadOptions,
    progress: Option<&'a (dyn Fn(u64) + Send + Sync)>,
}

impl Relay<'_> {
    /// The target of the upload of content of type `mime`, converted to `convert_to`.
    fn target(&self, mime: Mime, convert_to: Option<Mime>) -> UploadTarget {
        UploadTarget {
            parents: self.parents.to_vec(),
            name: self.name.clone(),
            mime: Some(mime),
            // Drive takes no id for a conversion; the uploader leaves it out then.
            new_file_id: self.new_file_id.map(str::to_owned),
            convert_to,
            ..UploadTarget::default()
        }
    }

    /// Streams the bytes of a blob into the upload.
    async fn blob(&self, label: String) -> Result<google_drive3::api::File, RelayError> {
        let size = self
            .file
            .size
            .and_then(|size| u64::try_from(size).ok())
            .ok_or_else(|| RelayError::incomplete_metadata("size"))?;
        let md5 = self.file.md5_checksum.as_deref();
        let media_type = self
            .file
            .mime_type
            .as_deref()
            .and_then(|mime| mime.parse::<Mime>().ok())
            .unwrap_or(mime::APPLICATION_OCTET_STREAM);
        let target = self.target(media_type.clone(), None);
        let check_md5 = self.opts.verify_md5 && md5.is_some();

        let mut upload = Upload::new(
            self.dest,
            self.dest.retrier().state(),
            &target,
            self.opts,
            self.progress,
            size,
            media_type,
        )
        .with_times(self.times);
        if check_md5 {
            upload = upload.trust_continued_content();
        }
        // Only resumable uploads have sessions to record.
        if let Some(dir) = &self.opts.session_dir {
            if size > self.opts.multipart_threshold {
                if let Some(identity) = self.identity(size, &label).await {
                    upload.record_in(dir, identity);
                }
            }
        }

        let mut reader = DriveSource::new(
            self.source,
            self.file,
            (self.id, self.resource_key),
            size,
            label,
        )
        .map_err(RelayError::Source)?;
        // Boxed: the upload's state is large, and the relay's future stays small for callers
        // that hold many of them.
        let copy = Box::pin(upload.run(&mut reader))
            .await
            .map_err(RelayError::of_upload)?;

        if let (true, Some(expected), Some(actual)) = (check_md5, md5, copy.md5_checksum.as_deref())
        {
            if !same_md5(expected, actual) {
                // A source that changed since its listing reads as another content in one go.
                return Err(match reader.identity.changed().await {
                    Ok(true) => RelayError::SourceChanged,
                    _ => RelayError::Md5Mismatch {
                        expected: expected.to_owned(),
                        actual: actual.to_owned(),
                    },
                });
            }
        }
        Ok(copy)
    }

    /// The identity of the source in a session record, if the source has an md5 and its account
    /// is known. Without an md5, nothing would prove that the content a later run reads is the
    /// one the session holds the start of.
    async fn identity(&self, size: u64, label: &str) -> Option<SourceIdentity> {
        let md5 = self.file.md5_checksum.as_deref()?;
        match account_of(self.source).await {
            Ok(Some(account)) => Some(SourceIdentity::remote(
                &account,
                self.id,
                size,
                Some(md5),
                self.file.version,
            )),
            Ok(None) => {
                eprintln!(
                    "Warning: {label}: Drive does not name the account of the source, an \
                     interrupted copy will start over"
                );
                None
            }
            Err(_) => {
                eprintln!(
                    "Warning: {label}: unable to look up the Google account of the source, an \
                     interrupted copy will start over"
                );
                None
            }
        }
    }

    /// Exports a Workspace file and uploads the export, converted back to its type.
    async fn export(
        &self,
        export: Export,
        label: String,
    ) -> Result<google_drive3::api::File, RelayError> {
        let content = read_export(
            self.source,
            self.id,
            self.resource_key,
            export.format,
            &label,
        )
        .await?;
        let target = self.target(export.format.clone(), Some(export.doc_type.mime().clone()));
        let upload = Upload::new(
            self.dest,
            self.dest.retrier().state(),
            &target,
            self.opts,
            self.progress,
            content.len() as u64,
            export.format.clone(),
        )
        .with_times(self.times);
        Box::pin(upload.run(&mut ReaderSource::new(Cursor::new(content))))
            .await
            .map_err(RelayError::of_upload)
    }
}

/// The export of the Workspace file `id` as `format`, read into memory: at most
/// [`EXPORT_LIMIT`] bytes, the reading stops as soon as more arrive. Exports ignore `Range`, so
/// one that breaks off is requested again from the start, within the hub's retry policy.
async fn read_export(
    hub: &Hub,
    id: &str,
    resource_key: Option<&str>,
    format: &Mime,
    label: &str,
) -> Result<Bytes, RelayError> {
    let too_large = || RelayError::ExportTooLarge {
        limit: EXPORT_LIMIT,
    };
    let client = MediaClient::new(hub).with_resource_key(id, resource_key);
    let url = client
        .drive_url(&["files", id, "export"], &[("mimeType", format.as_ref())])
        .map_err(RelayError::Source)?;
    let mut retry = hub.retrier().state();
    loop {
        let response = client
            .send_with_retry(&mut retry, label, || {
                Request::get(url.as_str()).body(google_apis_common::to_body::<String>(None))
            })
            .await
            .map_err(|error| {
                if is_export_too_large(&error) {
                    too_large()
                } else {
                    RelayError::Source(error)
                }
            })?;
        if response.status() != StatusCode::OK {
            return Err(RelayError::Source(MediaError::Status {
                status: response.status(),
                message: None,
                stop: None,
            }));
        }
        let announced = content_length(response.headers());
        if announced.is_some_and(|length| length > EXPORT_LIMIT) {
            return Err(too_large());
        }
        let mut content =
            Vec::with_capacity(announced.and_then(|n| usize::try_from(n).ok()).unwrap_or(0));
        let mut body = response.into_body();
        let error = loop {
            match body.frame().await {
                None => return Ok(Bytes::from(content)),
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        if (content.len() + data.len()) as u64 > EXPORT_LIMIT {
                            return Err(too_large());
                        }
                        content.extend_from_slice(&data);
                    }
                }
                Some(Err(error)) => break error,
            }
        };
        match retry.next(classify_body_error(&error)) {
            Decision::Retry { after, reason } => {
                eprintln!(
                    "Warning: {label}: interrupted after {} bytes ({reason}), exporting again in \
                     {} (attempt {} of {})",
                    content.len(),
                    DisplayDuration(after),
                    retry.failures() + 1,
                    retry.policy().max_attempts,
                );
                tokio::time::sleep(after).await;
            }
            Decision::Stop { reason } => {
                return Err(RelayError::Source(MediaError::Resume { reason }));
            }
        }
    }
}

/// Why reading the source failed, carried through the uploader as the `io::Error` of a read.
#[derive(Debug)]
enum SourceFailure {
    Media(MediaError),
    Changed,
}

impl SourceFailure {
    /// The failure inside `error`, or `error` if it is another one.
    fn take(error: io::Error) -> Result<SourceFailure, io::Error> {
        if !error
            .get_ref()
            .is_some_and(<dyn Error + Send + Sync>::is::<SourceFailure>)
        {
            return Err(error);
        }
        let kind = error.kind();
        match error.into_inner() {
            Some(inner) => inner
                .downcast::<SourceFailure>()
                .map(|failure| *failure)
                .map_err(|inner| io::Error::new(kind, inner)),
            None => Err(io::Error::from(kind)),
        }
    }
}

impl Display for SourceFailure {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SourceFailure::Media(error) => write!(f, "unable to read the source file: {error}"),
            SourceFailure::Changed => f.write_str("the source file changed"),
        }
    }
}

impl Error for SourceFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SourceFailure::Media(error) => Some(error),
            SourceFailure::Changed => None,
        }
    }
}

/// An open reply of the source.
struct Stream {
    body: google_apis_common::Body,
    /// The offset of the next byte of the body (of `pending` first).
    position: u64,
    /// What is left of the last frame.
    pending: Bytes,
    /// The offset the reply was asked for: it is read up to there, however far that is.
    asked: u64,
}

impl Stream {
    /// Whether a read at `offset` continues this reply: at its position, or a little (or up to
    /// where it was asked for) after it.
    fn reaches(&self, offset: u64) -> bool {
        offset >= self.position && (offset - self.position <= DISCARD_LIMIT || offset <= self.asked)
    }
}

/// The content of a Drive file as the uploader reads it: from one open reply while the reads
/// follow each other, from a new reply at the offset asked for otherwise.
///
/// A read that is dropped half-way (the uploader's request ended) loses nothing: the reply
/// stays where it was, or is requested again if it was being opened.
pub(super) struct DriveSource<'a> {
    media: MediaSource<'a>,
    /// The source's content as its metadata describes it, compared before every new reply.
    identity: Identity<'a>,
    /// Whether the metadata has an md5, which proves the content the same when it is unchanged.
    has_md5: bool,
    retry: RetryState,
    size: u64,
    label: String,
    /// The strong `ETag` of the content, sent back as `If-Range`.
    etag: Option<String>,
    stream: Option<Stream>,
    /// Whether the content was requested already: the next requests check the metadata first.
    opened: bool,
    /// The furthest any read got: reads past it are progress, which renews the retry budget.
    high_water: u64,
}

impl<'a> DriveSource<'a> {
    /// The content of `file`, `size` bytes, with its id and resource key; warnings name the
    /// transfer `label`.
    fn new(
        hub: &'a Hub,
        file: &google_drive3::api::File,
        (id, resource_key): (&str, Option<&str>),
        size: u64,
        label: String,
    ) -> Result<Self, MediaError> {
        let client = MediaClient::new(hub).with_resource_key(id, resource_key);
        let url = client.drive_url(
            &["files", id],
            &[("alt", "media"), ("supportsAllDrives", "true")],
        )?;
        // With an md5, the content is its md5 and size; the version and modification time
        // also change with the metadata (a rename, a share), which is no reason to fail.
        let mut revision = Revision::of(file);
        if revision.md5.is_some() {
            revision.version = None;
            revision.modified_time = None;
        }
        let has_md5 = revision.md5.is_some();
        Ok(DriveSource {
            media: MediaSource::Drive {
                client,
                url,
                label: label.clone(),
            },
            identity: Identity::new(hub, id, revision).with_resource_key(resource_key),
            has_md5,
            retry: hub.retrier().state(),
            size,
            label,
            etag: None,
            stream: None,
            opened: false,
            high_water: 0,
        })
    }

    async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, SourceFailure> {
        if buf.is_empty() || offset >= self.size {
            return Ok(0);
        }
        loop {
            let Some(stream) = self.stream.as_mut().filter(|stream| stream.reaches(offset)) else {
                self.stream = None;
                self.stream = Some(self.open(offset).await?);
                continue;
            };
            if stream.pending.is_empty() {
                let position = stream.position;
                match next_data(&mut stream.body).await {
                    Ok(Some(data)) if position + data.len() as u64 > self.size => {
                        // More content than its size: not the content the metadata describes.
                        return Err(SourceFailure::Changed);
                    }
                    Ok(Some(data)) => stream.pending = data,
                    Ok(None) => {
                        self.stream = None;
                        self.wait(
                            Decision::Retry {
                                after: Duration::ZERO,
                                reason: RetryReason::BodyRead,
                            },
                            position,
                        )
                        .await?;
                    }
                    Err(error) => {
                        self.stream = None;
                        self.wait(classify_body_error(&error), position).await?;
                    }
                }
                continue;
            }
            if stream.position < offset {
                let skip = usize::try_from(offset - stream.position)
                    .map_or(stream.pending.len(), |skip| skip.min(stream.pending.len()));
                stream.pending.advance(skip);
                stream.position += skip as u64;
                continue;
            }
            let read = buf.len().min(stream.pending.len());
            buf[..read].copy_from_slice(&stream.pending[..read]);
            stream.pending.advance(read);
            stream.position += read as u64;
            let position = stream.position;
            if position > self.high_water {
                self.high_water = position;
                self.retry.progress();
            }
            return Ok(read);
        }
    }

    /// Whether the metadata shows another content now.
    async fn changed(&self) -> Result<bool, SourceFailure> {
        self.identity.changed().await.map_err(SourceFailure::Media)
    }

    /// A reply with the content from `offset` on, or one from the start that is read up to
    /// there. Every request but the first, and a first one past the start (a continued
    /// session), is made only once the metadata shows the same content; a resume of a source
    /// with neither an md5 nor a strong `ETag` is not made at all: nothing would show that the
    /// rest is of the same content as what was read before.
    async fn open(&mut self, offset: u64) -> Result<Stream, SourceFailure> {
        if offset > 0 && !self.has_md5 && self.etag.is_none() {
            return Err(SourceFailure::Changed);
        }
        if (self.opened || offset > 0) && self.changed().await? {
            return Err(SourceFailure::Changed);
        }
        self.opened = true;
        loop {
            let if_range = if offset > 0 { self.etag.clone() } else { None };
            let reply = self
                .media
                .open(offset, if_range.as_deref(), &mut self.retry)
                .await
                .map_err(SourceFailure::Media)?;
            match reply.status {
                StatusCode::OK => {
                    if content_length(&reply.headers).is_some_and(|length| length != self.size) {
                        return Err(SourceFailure::Changed);
                    }
                    // All of the content to a Range request: it is not the content of the
                    // If-Range any more, or the server ignores Range. Only an unchanged md5
                    // proves it the same content as what was read before.
                    if offset > 0 && (!self.has_md5 || self.changed().await?) {
                        return Err(SourceFailure::Changed);
                    }
                    if if_range.is_some() && offset > DISCARD_LIMIT {
                        // The ETag did not match, though the md5 shows the same content: ask
                        // for the range alone rather than read this far again.
                        self.etag = None;
                        self.wait(
                            Decision::Retry {
                                after: Duration::ZERO,
                                reason: RetryReason::BadRange,
                            },
                            offset,
                        )
                        .await?;
                        continue;
                    }
                    self.etag = strong_etag(&reply.headers);
                    return Ok(Stream {
                        body: reply.body,
                        position: 0,
                        pending: Bytes::new(),
                        asked: offset,
                    });
                }
                StatusCode::PARTIAL_CONTENT if offset > 0 => {
                    let range = content_range(&reply.headers);
                    if range
                        .is_some_and(|range| range.total.is_some_and(|total| total != self.size))
                    {
                        return Err(SourceFailure::Changed);
                    }
                    if range.is_some_and(|range| range.first == offset && range.last < self.size) {
                        if let Some(etag) = strong_etag(&reply.headers) {
                            self.etag = Some(etag);
                        }
                        return Ok(Stream {
                            body: reply.body,
                            position: offset,
                            pending: Bytes::new(),
                            asked: offset,
                        });
                    }
                    self.wait(
                        Decision::Retry {
                            after: Duration::ZERO,
                            reason: RetryReason::BadRange,
                        },
                        offset,
                    )
                    .await?;
                }
                // The range starts before the size the metadata gives: the content is shorter.
                StatusCode::RANGE_NOT_SATISFIABLE if offset > 0 => {
                    return Err(SourceFailure::Changed)
                }
                status => {
                    return Err(SourceFailure::Media(MediaError::Status {
                        status,
                        message: None,
                        stop: None,
                    }))
                }
            }
        }
    }

    /// Counts a failed attempt at `position` and waits before the next one, or gives up.
    async fn wait(&mut self, decision: Decision, position: u64) -> Result<(), SourceFailure> {
        match self.retry.next(decision) {
            Decision::Retry { after, reason } => {
                eprintln!(
                    "Warning: {}: interrupted after {position} bytes ({reason}), resuming in {} \
                     (attempt {} of {})",
                    self.label,
                    DisplayDuration(after),
                    self.retry.failures() + 1,
                    self.retry.policy().max_attempts,
                );
                tokio::time::sleep(after).await;
                Ok(())
            }
            Decision::Stop { reason } => Err(SourceFailure::Media(MediaError::Resume { reason })),
        }
    }
}

impl Source for DriveSource<'_> {
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.read(offset, buf).await.map_err(io::Error::other)
    }

    fn size(&mut self) -> impl std::future::Future<Output = io::Result<u64>> {
        std::future::ready(Ok(self.size))
    }
}

/// The next bytes of `body`; `None` at its end. Dropped before it completes, it takes nothing.
async fn next_data(body: &mut google_apis_common::Body) -> Result<Option<Bytes>, hyper::Error> {
    loop {
        match body.frame().await {
            None => return Ok(None),
            Some(Err(error)) => return Err(error),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    if !data.is_empty() {
                        return Ok(Some(data));
                    }
                }
            }
        }
    }
}
