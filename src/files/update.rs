use std::{
    error,
    fmt::{Display, Formatter},
    path::{Path, PathBuf},
};

use mime::Mime;

use crate::{
    common::{
        delegate::{ChunkSize, MultipartThreshold},
        file_helper,
        file_info::{self, FileInfo},
        hub_helper::{get_hub, GetHubError},
        retry::retry_api,
    },
    files::{
        self,
        info::{self, DisplayConfig},
        FileResult,
    },
    hub::Hub,
    transfer::{
        progress::{self, ConfirmedTotal, ProgressDisplay},
        upload::{
            session::{account_session_dir, account_session_identity},
            upload_path, UploadError, UploadOptions, UploadTarget,
        },
    },
};

pub(crate) struct Config {
    pub(crate) file_id: String,
    pub(crate) file_path: Option<PathBuf>,
    /// The new name of the file (`--name`). Without it the file is renamed to the local file
    /// name, or keeps its name when the content comes from stdin.
    pub(crate) name: Option<String>,
    pub(crate) mime_type: Option<Mime>,
    pub(crate) chunk_size: ChunkSize,
    pub(crate) multipart_threshold: MultipartThreshold,
    pub(crate) print_chunk_errors: bool,
    pub(crate) print_chunk_info: bool,
    /// Whether the update shows its progress on stderr.
    pub(crate) progress: progress::Mode,
}

pub(crate) async fn update(config: Config) -> Result<(), Box<Error>> {
    if let Some(file_path) = &config.file_path {
        err_if_not_regular_file(file_path)?;
    }

    let hub = get_hub().await.map_err(Error::Hub)?;

    let options = UploadOptions {
        chunk_size: config.chunk_size.in_bytes(),
        multipart_threshold: config.multipart_threshold.in_bytes(),
        verify_md5: true,
        print_chunk_errors: config.print_chunk_errors,
        print_chunk_info: config.print_chunk_info,
        // A spool of stdin has a new name every time: there is no earlier run to continue.
        session_dir: config
            .file_path
            .as_ref()
            .and_then(|_| account_session_dir()),
        session_identity: account_session_identity(),
    };

    let mut file_helper = match file_helper::open_file(config.file_path.as_deref()) {
        Ok(file_helper) => file_helper,
        Err(err) => {
            return Err(Box::new(Error::OpenFile(
                config.file_path.unwrap_or_else(|| PathBuf::from("<stdin>")),
                err,
            )))
        }
    };

    let (file, file_path) = file_helper.file_mut_and_path();
    let file = update_content(&hub, &config, &options, file, file_path).await?;

    println!("File successfully updated");

    files::info::print_file_info(&file, &DisplayConfig::default());

    Ok(())
}

/// Uploads the content of `file` at `content_path` as a new revision of `config.file_id`.
/// Without `config.file_path`, `content_path` is a spool of stdin.
async fn update_content(
    hub: &Hub,
    config: &Config,
    options: &UploadOptions,
    file: &std::fs::File,
    content_path: &Path,
) -> Result<google_drive3::api::File, Box<Error>> {
    let drive_file = info::get_file(hub, &config.file_id)
        .await
        .map_err(Error::GetFile)?;

    let file_info_config = file_info::Config {
        file_path: content_path,
        name: config.name.as_deref(),
        mime_type: config.mime_type.as_ref(),
        parents: drive_file.parents.clone(),
    };

    let file_info = match FileInfo::from_file(file, file_info_config) {
        Ok(file_info) => file_info,
        Err(source) => {
            return Err(Box::new(Error::FileInfo {
                path: content_path.to_path_buf(),
                source,
            }))
        }
    };

    // The file keeps its folders: an update sends only the name and the content. The name of
    // a spool of stdin means nothing, so without --name the file keeps its name.
    let from_stdin = config.file_path.is_none();
    let name = (config.name.is_some() || !from_stdin).then(|| file_info.name.into_owned());
    let target = UploadTarget {
        name,
        mime: Some(file_info.mime_type.into_owned()),
        existing_file_id: Some(config.file_id.clone()),
        ..UploadTarget::default()
    };

    println!(
        "Updating {} with {}",
        config.file_id,
        content_path.display()
    );

    let (display, progress) = ProgressDisplay::single(config.progress, false, Some(file_info.size));
    let confirmed = ConfirmedTotal::new(progress);
    let report = |held| confirmed.held(held);
    let uploaded = upload_path(hub, content_path, &target, options, Some(&report)).await;
    display.finish();
    uploaded.map_err(|err| Box::new(Error::Update(Box::new(err))))
}

/// The upload reads the path as a seekable byte stream: a directory made the upload library
/// panic (#18) and a FIFO would block, so only regular files (or symlinks to them) are accepted.
fn err_if_not_regular_file(path: &Path) -> Result<(), Box<Error>> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Err(Box::new(Error::IsDirectory(path.to_path_buf()))),
        Ok(metadata) if !metadata.is_file() => {
            Err(Box::new(Error::NotRegularFile(path.to_path_buf())))
        }
        // A missing or unreadable path is reported when the file is opened.
        _ => Ok(()),
    }
}

#[expect(
    clippy::result_large_err,
    reason = "Ok variant is bigger, see test next to FileResult"
)]
pub(crate) async fn update_metadata(hub: &Hub, patch_file: PatchFile) -> FileResult {
    let patch_file = &patch_file;
    let (_, file) = retry_api(hub.retrier(), "files.update", |mut delegate| async move {
        hub.files()
            .update(patch_file.file.clone(), &patch_file.id)
            .param(
                "fields",
                "id,name,size,createdTime,modifiedTime,md5Checksum,mimeType,parents,shared,\
                description,webContentLink,webViewLink",
            )
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .supports_all_drives(true)
            .doit_without_upload()
            .await
    })
    .await?;

    Ok(file)
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    IsDirectory(PathBuf),
    NotRegularFile(PathBuf),
    FileInfo {
        path: PathBuf,
        source: file_info::FromFileError,
    },
    OpenFile(PathBuf, file_helper::OpenFileError),
    GetFile(google_drive3::Error),
    Update(Box<UploadError>),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::IsDirectory(path) => write!(
                f,
                "'{}' is a directory, only a regular file can be updated",
                path.display()
            ),
            Error::NotRegularFile(path) => write!(
                f,
                "'{}' is not a regular file, only a regular file can be updated",
                path.display()
            ),
            Error::FileInfo { path, source: _ } => {
                write!(f, "unable to get file info for '{}'", path.display())
            }
            Error::OpenFile(path, _) => {
                write!(f, "unable to open file '{}'", path.display())
            }
            Error::GetFile(_) => f.write_str("unable to get file"),
            Error::Update(_) => f.write_str("unable to update file"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::IsDirectory(_) | Error::NotRegularFile(_) => None,
            Error::FileInfo { source, .. } => Some(source),
            Error::OpenFile(_, source) => Some(source),
            Error::GetFile(source) => Some(source),
            Error::Update(source) => Some(source.as_ref()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PatchFile {
    id: String,
    file: google_drive3::api::File,
}

impl PatchFile {
    #[must_use]
    pub(crate) fn new(id: String) -> Self {
        Self {
            id,
            file: google_drive3::api::File::default(),
        }
    }

    #[must_use]
    pub(crate) fn with_name(mut self, name: impl Into<String>) -> Self {
        self.file = google_drive3::api::File {
            name: Some(name.into()),
            ..self.file
        };
        self
    }

    #[must_use]
    pub(crate) fn with_trashed(mut self, trashed: bool) -> Self {
        self.file = google_drive3::api::File {
            trashed: Some(trashed),
            ..self.file
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::{err_if_not_regular_file, update_content, Config, Error};
    use crate::{
        common::mock_server::{MockServer, Response},
        transfer::upload::UploadOptions,
    };

    /// `gdrive files update file-id [--name NAME] [FILE_PATH]` with `content_path` as the
    /// content (a spool of stdin without `file_path`): the metadata of the upload.
    async fn update_metadata_of(
        file_path: Option<PathBuf>,
        name: Option<&str>,
        content_path: &Path,
    ) -> serde_json::Value {
        let content = std::fs::read(content_path).unwrap();
        let server = MockServer::start(vec![
            Response::json(
                200,
                &json!({"id": "file-id", "name": "report.txt", "parents": ["folder-id"]}),
            ),
            Response::json(
                200,
                &json!({
                    "id": "file-id",
                    "name": "report.txt",
                    "size": content.len().to_string(),
                    "md5Checksum": format!("{:x}", md5::compute(&content)),
                }),
            ),
        ])
        .await;
        let config = Config {
            file_id: String::from("file-id"),
            file_path,
            name: name.map(String::from),
            mime_type: None,
            chunk_size: "1".parse().unwrap(),
            multipart_threshold: "5".parse().unwrap(),
            print_chunk_errors: false,
            print_chunk_info: false,
            progress: crate::transfer::progress::Mode::Never,
        };
        let file = std::fs::File::open(content_path).unwrap();

        update_content(
            &server.hub(),
            &config,
            &UploadOptions::default(),
            &file,
            content_path,
        )
        .await
        .unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 2, "files.get and the upload");
        let upload = &requests[1];
        assert_eq!(upload.method, "PATCH");
        assert_eq!(upload.path, "/upload/drive/v3/files/file-id");
        assert_eq!(upload.query_param("uploadType"), Some("multipart"));
        let body = upload.body_text();
        let start = body.find('{').unwrap();
        let end = start + body[start..].find("}\r\n").unwrap() + 1;
        serde_json::from_str(&body[start..end]).unwrap()
    }

    /// A spool of stdin: a file with a random name and no extension.
    fn spool(dir: &Path) -> PathBuf {
        let path = dir.join("5f0c4a3e-9d1b-4c47-8a51-2f6f9a0e7b13");
        std::fs::write(&path, b"new content").unwrap();
        path
    }

    #[tokio::test]
    async fn an_update_from_stdin_keeps_the_name_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = update_metadata_of(None, None, &spool(dir.path())).await;
        assert_eq!(metadata, json!({}));
    }

    #[tokio::test]
    async fn an_update_from_stdin_renames_the_file_to_the_name_given() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = update_metadata_of(None, Some("notes.txt"), &spool(dir.path())).await;
        assert_eq!(metadata, json!({"name": "notes.txt"}));
    }

    #[tokio::test]
    async fn an_update_from_a_file_takes_its_name_or_the_name_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local.txt");
        std::fs::write(&path, b"new content").unwrap();

        let metadata = update_metadata_of(Some(path.clone()), None, &path).await;
        assert_eq!(metadata, json!({"name": "local.txt"}));

        let metadata = update_metadata_of(Some(path.clone()), Some("Final"), &path).await;
        assert_eq!(metadata, json!({"name": "Final"}));
    }

    #[test]
    fn directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = err_if_not_regular_file(dir.path()).unwrap_err();
        assert!(matches!(*err, Error::IsDirectory(ref path) if path == dir.path()));
        assert!(err.to_string().contains("is a directory"));
    }

    #[test]
    fn regular_file_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"a").unwrap();
        assert!(err_if_not_regular_file(&file).is_ok());
    }

    #[test]
    fn missing_file_is_left_to_open() {
        let dir = tempfile::tempdir().unwrap();
        assert!(err_if_not_regular_file(&dir.path().join("missing")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn special_file_and_symlinks_are_checked() {
        use std::os::unix::{fs::symlink, net::UnixListener};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let _listener = UnixListener::bind(&socket).unwrap();
        let err = err_if_not_regular_file(&socket).unwrap_err();
        assert!(matches!(*err, Error::NotRegularFile(_)));

        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"a").unwrap();
        symlink(&file, dir.path().join("file-link")).unwrap();
        symlink(dir.path(), dir.path().join("dir-link")).unwrap();
        assert!(err_if_not_regular_file(&dir.path().join("file-link")).is_ok());
        let err = err_if_not_regular_file(&dir.path().join("dir-link")).unwrap_err();
        assert!(matches!(*err, Error::IsDirectory(_)));
    }
}
