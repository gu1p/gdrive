use std::{
    error,
    fmt::{Display, Formatter},
    path::{Path, PathBuf},
    time::Duration,
};

use mime::Mime;

use crate::{
    common::{
        delegate::{BackoffConfig, ChunkSize, UploadDelegate, UploadDelegateConfig},
        file_helper,
        file_info::{self, FileInfo},
        hub_helper::{get_hub, GetHubError},
    },
    files::{
        self,
        info::{self, DisplayConfig},
        FileResult,
    },
    hub::Hub,
};

pub(crate) struct Config {
    pub(crate) file_id: String,
    pub(crate) file_path: Option<PathBuf>,
    pub(crate) mime_type: Option<Mime>,
    pub(crate) chunk_size: ChunkSize,
    pub(crate) print_chunk_errors: bool,
    pub(crate) print_chunk_info: bool,
}

pub(crate) async fn update(config: Config) -> Result<(), Box<Error>> {
    if let Some(file_path) = &config.file_path {
        err_if_not_regular_file(file_path)?;
    }

    let hub = get_hub().await.map_err(Error::Hub)?;

    let delegate_config = UploadDelegateConfig {
        chunk_size: config.chunk_size,
        backoff_config: BackoffConfig {
            max_retries: 20,
            min_sleep: Duration::from_secs(1),
            max_sleep: Duration::from_secs(60),
        },
        print_chunk_errors: config.print_chunk_errors,
        print_chunk_info: config.print_chunk_info,
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

    let drive_file = info::get_file(&hub, &config.file_id)
        .await
        .map_err(Error::GetFile)?;

    let (file, file_path) = file_helper.file_mut_and_path();

    let file_info_config = file_info::Config {
        file_path,
        mime_type: config.mime_type.as_ref(),
        parents: drive_file.parents.clone(),
    };

    let file_info = match FileInfo::from_file(file, file_info_config) {
        Ok(file_info) => file_info,
        Err(source) => {
            return Err(Box::new(Error::FileInfo {
                path: file_helper.into_path_buf(),
                source,
            }))
        }
    };

    let reader = std::io::BufReader::new(file);

    println!("Updating {} with {}", config.file_id, file_path.display());

    let file = update_file(&hub, reader, &config.file_id, file_info, &delegate_config)
        .await
        .map_err(Error::Update)?;

    println!("File successfully updated");

    files::info::print_file_info(&file, &DisplayConfig::default());

    Ok(())
}

/// The upload reads the path as a seekable byte stream: a directory makes the upload library
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
async fn update_file<RS>(
    hub: &Hub,
    src_file: RS,
    file_id: &str,
    file_info: FileInfo<'_>,
    delegate_config: &UploadDelegateConfig,
) -> FileResult
where
    RS: google_apis_common::ReadSeek,
{
    let dst_file = google_drive3::api::File {
        name: Some(file_info.name.into_owned()),
        ..google_drive3::api::File::default()
    };

    let mut delegate = UploadDelegate::new(delegate_config);

    let req = hub
        .files()
        .update(dst_file, file_id)
        .param(
            "fields",
            "id,name,size,createdTime,modifiedTime,md5Checksum,mimeType,parents,shared,\
            description,webContentLink,webViewLink",
        )
        .add_scope(google_drive3::api::Scope::Full)
        .delegate(&mut delegate)
        .supports_all_drives(true);

    let (_, file) = if file_info.size > 0 {
        req.upload_resumable(src_file, file_info.mime_type.into_owned())
            .await?
    } else {
        req.upload(src_file, file_info.mime_type.into_owned())
            .await?
    };

    Ok(file)
}

#[expect(
    clippy::result_large_err,
    reason = "Ok variant is bigger, see test next to FileResult"
)]
pub(crate) async fn update_metadata(
    hub: &Hub,
    delegate_config: &UploadDelegateConfig,
    patch_file: PatchFile,
) -> FileResult {
    let mut delegate = UploadDelegate::new(delegate_config);

    let (_, file) = hub
        .files()
        .update(patch_file.file, &patch_file.id)
        .param(
            "fields",
            "id,name,size,createdTime,modifiedTime,md5Checksum,mimeType,parents,shared,\
            description,webContentLink,webViewLink",
        )
        .add_scope(google_drive3::api::Scope::Full)
        .delegate(&mut delegate)
        .supports_all_drives(true)
        .doit_without_upload()
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
    Update(google_drive3::Error),
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
            Error::GetFile(source) | Error::Update(source) => Some(source),
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
    use super::{err_if_not_regular_file, Error};

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
