use std::{
    error,
    fmt::{Display, Formatter},
    ops::Not,
    path::PathBuf,
};

use mime::Mime;

use crate::{
    common::{
        drive_file::{DocType, FileExtension},
        hub_helper::{get_hub, GetHubError},
        media_client::{MediaClient, MediaError},
        parse_md5_digest,
    },
    files::{
        self,
        download::{
            resume::{MediaSource, ResumableDownload},
            Expected,
        },
    },
    hub::Hub,
    transfer::progress::{self, ProgressDisplay},
};

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) file_id: String,
    pub(crate) file_path: PathBuf,
    pub(crate) existing_file_action: ExistingFileAction,
    /// Whether the export shows its progress on stderr.
    pub(crate) progress: progress::Mode,
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
    let first = download
        .next()
        .await
        .map_err(|err| Error::ExportFile(Box::new(err)))?;

    println!(
        "Exporting {} '{}' to {}",
        doc_type,
        file.name.unwrap_or_default(),
        config.file_path.display()
    );

    let (display, _progress) = ProgressDisplay::single(config.progress, false, download.total());
    let saved =
        files::download::save_to_file(download, first, &config.file_path, &config.file_id).await;
    display.finish();
    saved.map_err(Error::SaveFile)?;

    println!("Successfully exported {}", config.file_path.display());

    Ok(())
}

/// The export of a Workspace document as `mime_type` (`files.export`). Exports ignore `Range`:
/// a download that breaks off starts over, through the same path as file downloads.
fn export_content<'a>(
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

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    FileExists(PathBuf),
    GetFile(Box<google_drive3::Error>),
    ExportFile(Box<MediaError>),
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
            Error::ExportFile(_) => f.write_str("unable to export file"),
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
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{export_with_hub, progress, Config, ExistingFileAction};
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
                progress: progress::Mode::Never,
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
}
