use std::{
    error,
    fmt::{Display, Formatter},
    fs, io,
    path::{Path, PathBuf},
};

use mime::Mime;

use crate::{
    common::{
        drive_file::{DocType, FileExtension},
        file_info::{self, FileInfo},
        hub_helper::{get_hub, GetHubError},
    },
    files::{self, info::DisplayConfig},
    transfer::{
        progress::{self, ConfirmedTotal, ProgressDisplay},
        upload::{
            session::{account_session_dir, account_session_identity},
            upload_path, UploadError, UploadOptions, UploadTarget,
        },
    },
};

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) file_path: PathBuf,
    pub(crate) parents: Option<Vec<String>>,
    pub(crate) print_only_id: bool,
    /// Whether the import shows its progress on stderr.
    pub(crate) progress: progress::Mode,
}

pub(crate) async fn import(config: Config) -> Result<(), Box<Error>> {
    let hub = get_hub().await.map_err(Error::Hub)?;
    let options = UploadOptions {
        session_dir: account_session_dir(),
        session_identity: account_session_identity(),
        ..UploadOptions::default()
    };

    let (doc_type, source_mime_type) =
        import_types(&config.file_path).ok_or(Error::UnsupportedFileType)?;
    let mime_type = doc_type.mime();

    let file = match fs::File::open(&config.file_path) {
        Ok(file) => file,
        Err(err) => return Err(Box::new(Error::OpenFile(config.file_path, err))),
    };

    let file_info = match FileInfo::from_file(
        &file,
        file_info::Config {
            file_path: &config.file_path,
            mime_type: Some(mime_type),
            parents: config.parents.clone(),
        },
    ) {
        Ok(file_info) => file_info,
        Err(source) => {
            return Err(Box::new(Error::FileInfo {
                path: config.file_path,
                source,
            }))
        }
    };
    // The uploader opens the file itself.
    drop(file);
    let target = import_target(
        file_info.name.into_owned(),
        file_info.parents.unwrap_or_default(),
        doc_type,
        source_mime_type,
    );

    if !config.print_only_id {
        println!("Importing {} as a {}", config.file_path.display(), doc_type);
    }

    let (display, progress) = ProgressDisplay::single(config.progress, false, Some(file_info.size));
    let confirmed = ConfirmedTotal::new(progress);
    let report = |held| confirmed.held(held);
    let uploaded = upload_path(&hub, &config.file_path, &target, &options, Some(&report)).await;
    display.finish();
    let file = uploaded.map_err(|err| Error::UploadFile(Box::new(err)))?;

    if config.print_only_id {
        print!("{}", file.id.unwrap_or_default());
    } else {
        println!("File successfully imported");
        files::info::print_file_info(&file, &DisplayConfig::default());
    }

    Ok(())
}

/// The upload of an import: the Drive metadata asks for the Google type while the content goes
/// up as what it is (e.g. text/markdown), the pair that makes Drive convert the file.
fn import_target(
    name: String,
    parents: Vec<String>,
    doc_type: DocType,
    source_mime_type: &Mime,
) -> UploadTarget {
    UploadTarget {
        parents,
        name,
        mime: Some(source_mime_type.clone()),
        convert_to: Some(doc_type.mime().clone()),
        ..UploadTarget::default()
    }
}

/// The Google Workspace type a local file is imported as, and the media type of its content,
/// both from the file extension.
fn import_types(path: &Path) -> Option<(DocType, &'static Mime)> {
    let doc_type = DocType::from_file_path(path)?;
    let source_mime_type = FileExtension::from_path(path)?.get_export_mime();
    Some((doc_type, source_mime_type))
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    OpenFile(PathBuf, io::Error),
    FileInfo {
        path: PathBuf,
        source: file_info::FromFileError,
    },
    UploadFile(Box<UploadError>),
    UnsupportedFileType,
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::FileInfo { source, .. } => Some(source),
            Error::UploadFile(source) => Some(source.as_ref()),
            // The message of the I/O error is part of the error's own.
            Error::OpenFile(..) | Error::UnsupportedFileType => None,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::OpenFile(path, err) => {
                write!(f, "Failed to open file '{}': {}", path.display(), err)
            }
            Error::FileInfo { path, source: _ } => {
                write!(f, "unable to get file info for '{}'", path.display())
            }
            Error::UploadFile(_) => f.write_str("Failed to upload file"),
            Error::UnsupportedFileType => {
                const _: () = const {
                    assert!(
                        !DocType::SUPPORTED_INPUT_TYPES.is_empty(),
                        "DocType::SUPPORTED_INPUT_TYPES should not be empty"
                    );
                };

                f.write_str("Unsupported file type, supported file types: ")?;
                let mut supported_types = DocType::SUPPORTED_INPUT_TYPES.iter();
                write!(f, "{}", supported_types.next().unwrap())?;
                for supported_type in supported_types {
                    write!(f, ", {supported_type}")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{import_target, import_types};
    use crate::{
        common::{
            drive_file::{DocType, MIME_TYPE_DRIVE_DOCUMENT},
            mock_server::{MockServer, Response},
        },
        transfer::upload::{upload_path, UploadOptions},
    };

    #[test]
    fn markdown_is_imported_as_a_document_from_text_markdown() {
        let (doc_type, source_mime_type) = import_types(Path::new("notes.md")).unwrap();
        assert_eq!(doc_type, DocType::Document);
        assert_eq!(doc_type.mime().essence_str(), MIME_TYPE_DRIVE_DOCUMENT);
        assert_eq!(source_mime_type.essence_str(), "text/markdown");
    }

    #[test]
    fn source_type_follows_the_extension() {
        let (doc_type, source_mime_type) = import_types(Path::new("data.csv")).unwrap();
        assert_eq!(doc_type, DocType::Spreadsheet);
        assert_eq!(source_mime_type.essence_str(), "text/csv");

        let (doc_type, source_mime_type) = import_types(Path::new("deck.pptx")).unwrap();
        assert_eq!(doc_type, DocType::Presentation);
        assert_eq!(
            source_mime_type.essence_str(),
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        );

        assert!(import_types(Path::new("archive.zip")).is_none());
        assert!(import_types(Path::new("no-extension")).is_none());
    }

    /// `notes.md` with `content` in a temporary directory.
    fn notes(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn import_upload_sends_source_type_as_content_type() {
        let server = MockServer::start(vec![Response::json(
            200,
            &serde_json::json!({"id": "new-id", "mimeType": MIME_TYPE_DRIVE_DOCUMENT}),
        )])
        .await;
        let (_dir, path) = notes(b"# Title");
        let (doc_type, source_mime_type) = import_types(&path).unwrap();
        let target = import_target(
            String::from("notes.md"),
            Vec::new(),
            doc_type,
            source_mime_type,
        );

        upload_path(
            &server.hub(),
            &path,
            &target,
            &UploadOptions::default(),
            None,
        )
        .await
        .unwrap();

        // No files.generateIds: Drive takes no id for a conversion.
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let upload = &requests[0];
        assert_eq!(upload.method, "POST");
        assert_eq!(upload.path, "/upload/drive/v3/files");
        assert_eq!(upload.query_param("uploadType"), Some("multipart"));

        let body = upload.body_text();
        let metadata_type = format!("\"mimeType\":\"{MIME_TYPE_DRIVE_DOCUMENT}\"");
        assert!(body.contains(&metadata_type), "{body}");
        assert!(
            body.to_ascii_lowercase()
                .contains("content-type: text/markdown"),
            "{body}"
        );
        assert!(body.contains("# Title"), "{body}");
    }

    #[tokio::test]
    async fn resumable_import_sends_source_type_and_target_mime() {
        // Above the multipart threshold the upload is resumable: the initiation request carries
        // the metadata and announces the content type, then the content follows in chunks.
        const MIB: usize = 1 << 20;
        let content = vec![b'#'; MIB + 1];
        let total = content.len();

        let server = MockServer::start(vec![
            Response::json(200, &serde_json::json!({}))
                .with_header("Location", "{root}upload-session"),
            Response::json(308, &serde_json::json!({}))
                .with_header("Range", &format!("bytes=0-{}", MIB - 1)),
            Response::json(
                200,
                &serde_json::json!({"id": "new-id", "mimeType": MIME_TYPE_DRIVE_DOCUMENT}),
            ),
        ])
        .await;
        let (_dir, path) = notes(&content);
        let (doc_type, source_mime_type) = import_types(&path).unwrap();
        let target = import_target(
            String::from("notes.md"),
            Vec::new(),
            doc_type,
            source_mime_type,
        );
        let options = UploadOptions {
            chunk_size: MIB as u64,
            multipart_threshold: MIB as u64,
            ..UploadOptions::default()
        };

        upload_path(&server.hub(), &path, &target, &options, None)
            .await
            .unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 3);

        let initiation = &requests[0];
        assert_eq!(initiation.method, "POST");
        assert_eq!(initiation.path, "/upload/drive/v3/files");
        assert_eq!(initiation.query_param("uploadType"), Some("resumable"));
        assert_eq!(
            initiation.header("x-upload-content-type"),
            Some("text/markdown")
        );
        let mut metadata = initiation.body_json();
        let marker = metadata
            .as_object_mut()
            .unwrap()
            .remove("appProperties")
            .expect("an import is marked, to find it if its reply is lost");
        assert!(marker["gdriveUploadId"].is_string(), "{marker}");
        assert_eq!(
            metadata,
            serde_json::json!({"name": "notes.md", "mimeType": MIME_TYPE_DRIVE_DOCUMENT})
        );

        let chunks = &requests[1..];
        let ranges: Vec<_> = chunks
            .iter()
            .map(|chunk| chunk.header("content-range").unwrap_or_default())
            .collect();
        assert_eq!(
            ranges,
            [
                format!("bytes 0-{}/{total}", MIB - 1),
                format!("bytes {MIB}-{MIB}/{total}"),
            ]
        );
        for chunk in chunks {
            assert_eq!(chunk.method, "PUT");
            assert_eq!(chunk.path, "/upload-session");
            assert_eq!(chunk.header("content-type"), Some("text/markdown"));
        }
        let uploaded: usize = chunks.iter().map(|chunk| chunk.body.len()).sum();
        assert_eq!(uploaded, total);
    }
}
