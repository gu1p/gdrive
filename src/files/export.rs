mod formats;

use std::{
    error,
    fmt::{Display, Formatter},
    io,
    ops::Not,
    path::{Path, PathBuf},
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use mime::Mime;

pub(crate) use formats::ExportFormats;

use crate::{
    common::{
        drive_file::{DocType, FileExtension},
        hub_helper::{get_hub, GetHubError},
        media_client::{MediaClient, MediaError},
        parse_md5_digest,
        retry::{is_reason, StopReason},
    },
    files::{
        self,
        download::{
            resume::{MediaSource, ResumableDownload},
            Expected,
        },
    },
    hub::Hub,
};

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) file_id: String,
    pub(crate) file_path: PathBuf,
    pub(crate) existing_file_action: ExistingFileAction,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum ExistingFileAction {
    Abort,
    Overwrite,
}

pub(crate) async fn export(config: Config) -> Result<(), Error> {
    let hub = get_hub().await.map_err(Error::Hub)?;
    export_with_hub(&hub, config).await
}

async fn export_with_hub(hub: &Hub, config: Config) -> Result<(), Error> {
    if config.file_path.exists() && config.existing_file_action == ExistingFileAction::Abort {
        return Err(Error::FileExists(config.file_path));
    }

    let file = files::info::get_file(hub, &config.file_id)
        .await
        .map_err(|err| Error::GetFile(Box::new(err)))?;

    let drive_mime = file.mime_type.ok_or(Error::MissingDriveMime)?;
    let Some(doc_type) = DocType::from_mime_type(&drive_mime) else {
        return Err(Error::UnsupportedDriveMime(drive_mime));
    };

    let extension = FileExtension::from_path(&config.file_path)
        .ok_or(Error::UnsupportedExportExtension(doc_type))?;

    if doc_type.can_export_to(extension).not() {
        return Err(Error::UnsupportedExportExtension(doc_type));
    }

    let mime_type = extension.get_export_mime();

    // Escaped: the Drive name goes to the terminal.
    let label = format!(
        "export of '{}'",
        file.name.as_deref().unwrap_or_default().escape_debug()
    );
    let source = export_content(hub, &config.file_id, mime_type, label)
        .map_err(|err| Error::ExportFile(Box::new(err)))?;
    // No md5 or size to check for an export; it always starts over, so it needs no identity.
    let expected = Expected {
        md5: file.md5_checksum.as_deref().and_then(parse_md5_digest),
        size: None,
    };
    let mut download = ResumableDownload::new(source, hub.retrier().state(), expected);
    let first = download.next().await.map_err(|err| {
        if is_export_too_large(&err) {
            Error::TooLarge(ExportTooLarge(Box::new(err)))
        } else {
            Error::ExportFile(Box::new(err))
        }
    })?;

    println!(
        "Exporting {} '{}' to {}",
        doc_type,
        file.name.unwrap_or_default(),
        config.file_path.display()
    );

    files::download::save_to_file(download, first, &config.file_path, &config.file_id)
        .await
        .map_err(Error::SaveFile)?;

    println!("Successfully exported {}", config.file_path.display());

    Ok(())
}

/// The export of a Workspace document as `mime_type` (`files.export`). Exports ignore `Range`:
/// a download that breaks off starts over, through the same path as file downloads.
pub(crate) fn export_content<'a>(
    hub: &'a Hub,
    file_id: &str,
    mime_type: &Mime,
    label: String,
) -> Result<MediaSource<'a>, MediaError> {
    let client = MediaClient::new(hub);
    let url = client.drive_url(
        &["files", file_id, "export"],
        &[("mimeType", mime_type.as_ref())],
    )?;
    Ok(MediaSource::Drive { client, url, label })
}

/// The local name of the export of a Drive file named `name` as `format`, before it goes through
/// `local_name` like any Drive name: `name` with the format's extension appended, unless it ends
/// with that extension already (ASCII case-insensitively). So `report` exported as docx is
/// `report.docx`, `notes.md` exported as md stays `notes.md`, and as docx it is `notes.md.docx`.
#[must_use]
pub(crate) fn export_file_name(name: &str, format: FileExtension) -> String {
    let extension = format!(".{format}");
    let has_extension = name.len() > extension.len()
        && name
            .get(name.len() - extension.len()..)
            .is_some_and(|end| end.eq_ignore_ascii_case(&extension));
    if has_extension {
        name.to_owned()
    } else {
        format!("{name}{extension}")
    }
}

/// The `reason` of Drive's 403 for an export over the `files.export` size limit.
const EXPORT_SIZE_LIMIT_REASON: &str = "exportSizeLimitExceeded";

/// Whether Drive refused an export because the result is larger than `files.export` returns.
#[must_use]
pub(crate) fn is_export_too_large(error: &MediaError) -> bool {
    matches!(
        error,
        MediaError::Status {
            stop: Some(StopReason::Forbidden { reason: Some(reason) }),
            ..
        } if is_reason(reason, EXPORT_SIZE_LIMIT_REASON)
    )
}

/// Drive refused an export over the 10 MB limit of `files.export`; the source is Drive's reply.
#[derive(Debug)]
pub(crate) struct ExportTooLarge(pub(crate) Box<MediaError>);

impl Display for ExportTooLarge {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the exported file would be larger than the 10 MB that the Drive API exports; try \
            another format, or download the file from drive.google.com",
        )
    }
}

impl error::Error for ExportTooLarge {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// Sets the modification time of the file at `path` to `time`, the Drive file's `modifiedTime`.
pub(crate) async fn set_modified_time(path: &Path, time: DateTime<Utc>) -> io::Result<()> {
    let path = path.to_owned();
    let time = SystemTime::from(time);
    // As tokio::fs does: the blocking calls on the blocking pool. Windows needs write access to
    // change the time.
    tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_modified(time)
    })
    .await
    .map_err(io::Error::other)?
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    FileExists(PathBuf),
    GetFile(Box<google_drive3::Error>),
    ExportFile(Box<MediaError>),
    TooLarge(ExportTooLarge),
    MissingDriveMime,
    UnsupportedDriveMime(String),
    UnsupportedExportExtension(DocType),
    SaveFile(files::download::errors::SaveBodyToFile),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::FileExists(path) => {
                write!(
                    f,
                    "file '{}' already exists, use --overwrite to overwrite it",
                    path.display()
                )
            }
            Error::GetFile(_) => f.write_str("unable to get file"),
            Error::ExportFile(_) | Error::TooLarge(_) => f.write_str("unable to export file"),
            Error::MissingDriveMime => f.write_str("drive file does not have a mime type"),
            Error::UnsupportedDriveMime(mime) => {
                write!(f, "mime type on drive file '{mime}' is not supported")
            }
            Error::UnsupportedExportExtension(doc_type) => {
                write!(
                    f,
                    "export of a {doc_type} to this file type is not supported, supported file \
                    types are: "
                )?;

                let mut types = doc_type.supported_export_types().iter();
                if let Some(ty) = types.next() {
                    write!(f, "{ty}")?;
                    for ty in types {
                        write!(f, ", {ty}")?;
                    }
                } else {
                    f.write_str("[NONE]")?;
                }
                Ok(())
            }
            Error::SaveFile(_) => {
                write!(f, "failed to save file")
            }
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::SaveFile(source) => Some(source),
            Error::FileExists(_)
            | Error::MissingDriveMime
            | Error::UnsupportedDriveMime(_)
            | Error::UnsupportedExportExtension(_) => None,
            Error::GetFile(source) => Some(source),
            Error::ExportFile(source) => Some(source),
            Error::TooLarge(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{export_with_hub, Config, ExistingFileAction};
    use crate::common::{
        drive_file::MIME_TYPE_DRIVE_DOCUMENT,
        mock_server::{MockServer, Response},
    };

    #[tokio::test]
    async fn a_broken_export_starts_over_through_the_download_path() {
        let content = "exported document text\n".repeat(2000);
        let server = MockServer::start(vec![
            Response::json(
                200,
                &json!({"id": "doc", "name": "Notes", "mimeType": MIME_TYPE_DRIVE_DOCUMENT}),
            ),
            Response::raw(200, &content).cut_after(5000),
            // Exports ignore Range and send everything again.
            Response::raw(200, &content),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");

        export_with_hub(
            &server.hub(),
            Config {
                file_id: String::from("doc"),
                file_path: path.clone(),
                existing_file_action: ExistingFileAction::Abort,
            },
        )
        .await
        .unwrap();

        crate::files::download::assert_content(&path, &content);
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        for export in &requests[1..] {
            assert_eq!(export.path, "/drive/v3/files/doc/export");
            assert_eq!(export.query_param("mimeType"), Some("text%2Fplain"));
        }
        assert_eq!(requests[1].header("range"), None);
        assert_eq!(requests[2].header("range"), Some("bytes=5000-"));
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "no temporary file is left");
    }

    #[tokio::test]
    async fn an_export_over_the_size_limit_says_so() {
        let message = "This file is too large to be exported.";
        let server = MockServer::start(vec![
            Response::json(
                200,
                &json!({"id": "doc", "name": "Big", "mimeType": MIME_TYPE_DRIVE_DOCUMENT}),
            ),
            Response::json(
                403,
                &json!({"error": {"code": 403, "message": message, "errors": [
                    {"domain": "global", "reason": "exportSizeLimitExceeded", "message": message}
                ]}}),
            ),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();

        let error = export_with_hub(
            &server.hub(),
            Config {
                file_id: String::from("doc"),
                file_path: dir.path().join("big.pdf"),
                existing_file_action: ExistingFileAction::Abort,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            crate::error_message(&error),
            "Error: unable to export file: the exported file would be larger than the 10 MB that \
            the Drive API exports; try another format, or download the file from \
            drive.google.com: Drive answered 403 Forbidden: This file is too large to be exported."
        );
        assert_eq!(server.requests().len(), 2, "not repeated");
    }

    #[test]
    fn export_names_get_the_extension_once() {
        use crate::common::drive_file::FileExtension;

        assert_eq!(
            super::export_file_name("report", FileExtension::Docx),
            "report.docx"
        );
        assert_eq!(
            super::export_file_name("notes.MD", FileExtension::Md),
            "notes.MD"
        );
        assert_eq!(
            super::export_file_name("notes.md", FileExtension::Docx),
            "notes.md.docx"
        );
        assert_eq!(super::export_file_name(".md", FileExtension::Md), ".md.md");
        assert_eq!(
            super::export_file_name("é.pdf", FileExtension::Pdf),
            "é.pdf"
        );
    }
}
