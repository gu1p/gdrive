// TEMPORARY STUB: replaced by the implementation of branch migrate-relay at merge.
//! Copies the content of a Drive file of one account into a new file of another account,
//! streaming: no local file, memory bounded.
//!
//! This stub has the API of the real relay but none of its streaming: it reads the whole content
//! of a file into memory (`files.get?alt=media`, or `files.export` for a Workspace file) and
//! uploads it from there with [`upload_reader`]. It is only meant to carry `files migrate` until
//! the real implementation replaces this file.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::Cursor,
};

use google_drive3::api::File;
use http_body_util::BodyExt;
use hyper::Request;
use mime::Mime;

use crate::{
    common::{
        drive_file::{self, DocType},
        human_size::human_size,
        media_client::{MediaClient, MediaError},
        retry::{classify_body_error, retry_read, Decision},
    },
    files::export::{is_export_too_large, ExportFormats},
    hub::Hub,
    transfer::upload::{upload_reader, UploadError, UploadOptions, UploadTarget},
};

/// Why a file cannot be relayed, known from its metadata alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unsupported {
    /// `application/vnd.google-apps.shortcut`: Drive has no content for it.
    Shortcut,
    /// A Google Workspace type without an export format (forms, sites, maps, jamboard, ...).
    NativeType(String),
}

impl Display for Unsupported {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Unsupported::Shortcut => f.write_str("shortcuts have no content"),
            Unsupported::NativeType(mime) => {
                write!(
                    f,
                    "Google Workspace files of type {mime} cannot be migrated"
                )
            }
        }
    }
}

/// Drive's limit on the size of an export (the only buffer the relay holds in memory).
pub(crate) const EXPORT_LIMIT: u64 = 10 * 1024 * 1024;

/// The prefix of the Google Workspace MIME types.
const GOOGLE_APPS_PREFIX: &str = "application/vnd.google-apps.";

/// The Workspace type of a MIME type that is exported, and the format it is exported to.
/// A Drawing exports only to images and PDF, which Drive does not convert back to a Drawing.
fn export_format(mime: &str) -> Option<(DocType, &'static Mime)> {
    let doc_type =
        DocType::from_mime_type(mime).filter(|&doc_type| doc_type != DocType::Drawing)?;
    let format = ExportFormats::DEFAULT.format(doc_type)?;
    Some((doc_type, format.get_export_mime()))
}

/// From metadata alone (`mimeType` is required): Ok for a blob or an exportable Workspace file.
pub(crate) fn check_supported(file: &File) -> Result<(), Unsupported> {
    if drive_file::is_shortcut(file) {
        return Err(Unsupported::Shortcut);
    }
    let mime = file.mime_type.as_deref().unwrap_or_default();
    if mime.starts_with(GOOGLE_APPS_PREFIX) && export_format(mime).is_none() {
        return Err(Unsupported::NativeType(mime.to_owned()));
    }
    Ok(())
}

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
    #[must_use]
    pub(crate) fn unsupported(&self) -> Option<&Unsupported> {
        match self {
            RelayError::Unsupported(unsupported) => Some(unsupported),
            _ => None,
        }
    }
}

impl Display for RelayError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            RelayError::Unsupported(unsupported) => unsupported.fmt(f),
            RelayError::Source(_) => f.write_str("unable to read the source file"),
            RelayError::ExportTooLarge { limit } => write!(
                f,
                "the export is larger than the {} that Drive exports",
                human_size(*limit)
            ),
            RelayError::Upload(_) => f.write_str("unable to upload the file to the destination"),
            RelayError::Md5Mismatch { expected, actual } => write!(
                f,
                "the new file does not match the source: its md5 is {actual}, the source's is \
                 {expected}"
            ),
            RelayError::SourceChanged => f.write_str("the source file changed while it was read"),
            RelayError::SourceMetadata(_) => {
                f.write_str("unable to get the metadata of the source file")
            }
        }
    }
}

impl Error for RelayError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            RelayError::Source(source) => Some(source),
            RelayError::Upload(source) => Some(source),
            RelayError::SourceMetadata(source) => Some(source.as_ref()),
            RelayError::Unsupported(_)
            | RelayError::ExportTooLarge { .. }
            | RelayError::Md5Mismatch { .. }
            | RelayError::SourceChanged => None,
        }
    }
}

/// The fields of the source that a blob's relay checks.
const SOURCE_FIELDS: &str = "id,name,mimeType,size,md5Checksum,modifiedTime,createdTime,version";

/// Relays `source_file` from the account of `source` to a NEW file of the account of `dest`.
/// See the module documentation of the real implementation; this stub buffers the content.
pub(crate) async fn relay_file(
    source: &Hub,
    dest: &Hub,
    source_file: &File,
    target: &UploadTarget,
    opts: &UploadOptions,
    progress: Option<&(dyn Fn(u64) + Send + Sync)>,
) -> Result<File, RelayError> {
    check_supported(source_file).map_err(RelayError::Unsupported)?;
    let id = source_file.id.clone().unwrap_or_default();
    let name = target.name.clone().or_else(|| source_file.name.clone());
    let label = format!("read of '{}'", name.as_deref().unwrap_or(&id));
    let client = MediaClient::new(source);

    let mime = source_file.mime_type.clone().unwrap_or_default();
    if let Some((doc_type, export_mime)) = export_format(&mime) {
        let url = client
            .drive_url(
                &["files", &id, "export"],
                &[("mimeType", export_mime.as_ref())],
            )
            .map_err(RelayError::Source)?;
        let content = match read_all(source, &url, &label, Some(EXPORT_LIMIT)).await {
            Ok(Some(content)) => content,
            Ok(None) => {
                return Err(RelayError::ExportTooLarge {
                    limit: EXPORT_LIMIT,
                })
            }
            Err(error) if is_export_too_large(&error) => {
                return Err(RelayError::ExportTooLarge {
                    limit: EXPORT_LIMIT,
                })
            }
            Err(error) => return Err(RelayError::Source(error)),
        };
        let target = UploadTarget {
            parents: target.parents.clone(),
            name,
            mime: Some(export_mime.clone()),
            convert_to: Some(doc_type.mime().clone()),
            ..UploadTarget::default()
        };
        return upload(dest, content, &target, opts, progress).await;
    }

    let metadata = if source_file.size.is_none() || source_file.md5_checksum.is_none() {
        source_metadata(source, &id).await?
    } else {
        source_file.clone()
    };
    let url = client
        .drive_url(
            &["files", &id],
            &[("alt", "media"), ("supportsAllDrives", "true")],
        )
        .map_err(RelayError::Source)?;
    let content = read_all(source, &url, &label, None)
        .await
        .map_err(RelayError::Source)?
        .unwrap_or_default();
    let size_changed = metadata
        .size
        .and_then(|size| usize::try_from(size).ok())
        .is_some_and(|size| size != content.len());
    let expected = metadata.md5_checksum.clone();
    let content_md5 = format!("{:x}", md5::compute(&content));
    if size_changed || expected.as_ref().is_some_and(|md5| *md5 != content_md5) {
        return Err(RelayError::SourceChanged);
    }
    let target = UploadTarget {
        parents: target.parents.clone(),
        name,
        mime: metadata
            .mime_type
            .as_deref()
            .and_then(|mime| mime.parse().ok()),
        new_file_id: target.new_file_id.clone(),
        ..UploadTarget::default()
    };
    let created = upload(dest, content, &target, opts, progress).await?;
    match (expected, &created.md5_checksum) {
        (Some(expected), Some(actual)) if expected != *actual => Err(RelayError::Md5Mismatch {
            expected,
            actual: actual.clone(),
        }),
        _ => Ok(created),
    }
}

async fn upload(
    dest: &Hub,
    content: Vec<u8>,
    target: &UploadTarget,
    opts: &UploadOptions,
    progress: Option<&(dyn Fn(u64) + Send + Sync)>,
) -> Result<File, RelayError> {
    upload_reader(
        dest,
        Cursor::new(content),
        target,
        opts,
        dest.retrier().policy,
        progress,
    )
    .await
    .map_err(RelayError::Upload)
}

async fn source_metadata(source: &Hub, id: &str) -> Result<File, RelayError> {
    let (_, file) = retry_read(source.retrier(), "files.get", |mut delegate| async move {
        source
            .files()
            .get(id)
            .param("fields", SOURCE_FIELDS)
            .supports_all_drives(true)
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .doit()
            .await
    })
    .await
    .map_err(|error| RelayError::SourceMetadata(Box::new(error)))?;
    Ok(file)
}

/// The whole body of a GET of `url`, requested again from the start when it breaks off (per the
/// hub's retry policy). `None` when it is longer than `limit`.
async fn read_all(
    hub: &Hub,
    url: &url::Url,
    label: &str,
    limit: Option<u64>,
) -> Result<Option<Vec<u8>>, MediaError> {
    let client = MediaClient::new(hub);
    let mut retry = hub.retrier().state();
    loop {
        let response = client
            .send_with_retry(&mut retry, label, || {
                Request::get(url.as_str()).body(google_apis_common::to_body::<String>(None))
            })
            .await?;
        let mut body = response.into_body();
        let mut content = Vec::new();
        let broken = loop {
            match body.frame().await {
                None => break None,
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        content.extend_from_slice(&data);
                        let len = u64::try_from(content.len()).unwrap_or(u64::MAX);
                        if limit.is_some_and(|limit| len > limit) {
                            return Ok(None);
                        }
                    }
                }
                Some(Err(error)) => break Some(error),
            }
        };
        let Some(error) = broken else {
            return Ok(Some(content));
        };
        match retry.next(classify_body_error(&error)) {
            Decision::Retry { after, .. } => tokio::time::sleep(after).await,
            Decision::Stop { reason } => return Err(MediaError::Resume { reason }),
        }
    }
}
