use std::{
    error,
    fmt::{Display, Formatter},
    fs, io,
    ops::Not,
    path::{Path, PathBuf},
};

use mime::Mime;

use crate::{
    common::{
        delegate::{ChunkSize, MultipartThreshold, UploadDelegateConfig},
        file_helper,
        file_info::{self, FileInfo},
        file_tree::{self, FileTree},
        hub_helper::{get_hub, GetHubError},
        human_size::human_size,
        id_gen::IdGen,
        retry::RetryPolicy,
        FileTreeLike, FolderLike,
    },
    files::{self, info::DisplayConfig, mkdir, FileResult},
    hub::Hub,
    transfer::upload::{
        session::{account_session_dir, account_session_identity},
        upload_path, upload_reader, UploadError, UploadOptions, UploadTarget,
    },
};

#[expect(
    clippy::struct_excessive_bools,
    reason = "they are orthogonal one each other"
)]
pub(crate) struct Config {
    pub(crate) file_path: Option<PathBuf>,
    /// The name on Drive (`--name`): instead of the local file name, or of the random name of
    /// an upload from stdin.
    pub(crate) name: Option<String>,
    pub(crate) mime_type: Option<Mime>,
    pub(crate) parents: Option<Vec<String>>,
    pub(crate) chunk_size: ChunkSize,
    pub(crate) multipart_threshold: MultipartThreshold,
    pub(crate) print_chunk_errors: bool,
    pub(crate) print_chunk_info: bool,
    pub(crate) upload_directories: bool,
    pub(crate) print_only_id: bool,
}

impl Config {
    /// The common part, the path, the configuration of `upload_file` and the uploader's
    /// options (which record sessions in the account directory).
    fn split(
        self,
    ) -> (
        CommonConfig,
        Option<PathBuf>,
        UploadDelegateConfig,
        UploadOptions,
    ) {
        let Self {
            file_path,
            name,
            mime_type,
            parents,
            chunk_size,
            multipart_threshold,
            print_chunk_errors,
            print_chunk_info,
            upload_directories,
            print_only_id,
        } = self;
        let options = UploadOptions {
            chunk_size: chunk_size.in_bytes(),
            multipart_threshold: multipart_threshold.in_bytes(),
            verify_md5: true,
            print_chunk_errors,
            print_chunk_info,
            session_dir: account_session_dir(),
            session_identity: account_session_identity(),
        };
        (
            CommonConfig {
                name,
                mime_type,
                parents,
                upload_directories,
                print_only_id,
            },
            file_path,
            UploadDelegateConfig {
                chunk_size,
                multipart_threshold,
                // Bounded: a chunk the server accepts starts a new budget, so a long upload
                // survives many transient errors, but a dead connection ends it (it used to be
                // 100,000 retries).
                retry_policy: RetryPolicy::default(),
                print_chunk_errors,
                print_chunk_info,
            },
            options,
        )
    }
}

pub(crate) async fn upload(config: Config) -> Result<(), Error> {
    let hub = get_hub().await.map_err(Error::Hub)?;

    let (common, file_path, delegate_config, options) = config.split();

    if let Some(file_path) = file_path {
        if file_path.is_dir() && common.upload_directories.not() {
            return Err(Error::IsDirectory(file_path));
        }

        if file_path.is_dir() {
            upload_directory(
                &hub,
                UploadDirectoryConfig { file_path, common },
                &delegate_config,
            )
            .await?;
        } else {
            upload_regular(
                &hub,
                UploadRegularConfig {
                    file_path: FilePath::Regular(file_path),
                    common,
                },
                &options,
            )
            .await?;
        }
    } else {
        let tmp_file = file_helper::stdin_to_file().map_err(Error::StdinToFile)?;

        upload_regular(
            &hub,
            UploadRegularConfig {
                file_path: FilePath::Temp(tmp_file),
                common,
            },
            &options,
        )
        .await?;
    }

    Ok(())
}

#[derive(Debug)]
enum FilePath {
    Regular(PathBuf),
    Temp(mktemp::Temp),
}

impl FilePath {
    fn as_path(&self) -> &Path {
        match self {
            FilePath::Regular(path_buf) => path_buf.as_path(),
            FilePath::Temp(temp) => temp.as_path(),
        }
    }

    fn into_path_buf(self) -> PathBuf {
        match self {
            FilePath::Regular(path_buf) => path_buf,
            FilePath::Temp(temp) => temp.release(),
        }
    }
}

#[derive(Debug)]
struct CommonConfig {
    name: Option<String>,
    mime_type: Option<Mime>,
    parents: Option<Vec<String>>,
    upload_directories: bool,
    print_only_id: bool,
}

#[derive(Debug)]
struct UploadRegularConfig {
    file_path: FilePath,
    common: CommonConfig,
}

async fn upload_regular(
    hub: &Hub,
    config: UploadRegularConfig,
    options: &UploadOptions,
) -> Result<(), Error> {
    let file = match fs::File::open(config.file_path.as_path()) {
        Ok(file) => file,
        Err(err) => {
            return Err(Error::OpenFile(config.file_path.into_path_buf(), err));
        }
    };

    let file_info = match FileInfo::from_file(
        &file,
        file_info::Config {
            file_path: config.file_path.as_path(),
            name: config.common.name.as_deref(),
            mime_type: config.common.mime_type.as_ref(),
            parents: config.common.parents,
        },
    ) {
        Ok(file_info) => file_info,
        Err(source) => {
            return Err(Error::FileInfo {
                path: config.file_path.into_path_buf(),
                source,
            })
        }
    };
    // The uploader opens the file itself.
    drop(file);

    let target = UploadTarget {
        parents: file_info.parents.unwrap_or_default(),
        name: Some(file_info.name.into_owned()),
        mime: Some(file_info.mime_type.into_owned()),
        ..UploadTarget::default()
    };
    // A spool of stdin has a new name every time: there is no earlier run to continue.
    let options = match config.file_path {
        FilePath::Regular(_) => options.clone(),
        FilePath::Temp(_) => UploadOptions {
            session_dir: None,
            ..options.clone()
        },
    };

    if !config.common.print_only_id {
        println!("Uploading {}", config.file_path.as_path().display());
    }

    let file = upload_path(hub, config.file_path.as_path(), &target, &options, None)
        .await
        .map_err(|err| Error::Transfer(Box::new(err)))?;

    if config.common.print_only_id {
        print!("{}", file.id.unwrap_or_default());
    } else {
        println!("File successfully uploaded");
        files::info::print_file_info(&file, &DisplayConfig::default());
    }

    Ok(())
}

#[derive(Debug)]
struct UploadDirectoryConfig {
    file_path: PathBuf,
    common: CommonConfig,
}

async fn upload_directory(
    hub: &Hub,
    config: UploadDirectoryConfig,
    delegate_config: &UploadDelegateConfig,
) -> Result<(), Error> {
    let mut ids = IdGen::new(hub);
    let tree = FileTree::from_path(&config.file_path, &mut ids)
        .await
        .map_err(Error::CreateFileTree)?;

    let tree_info = tree.info();

    if !config.common.print_only_id {
        println!(
            "Found {} files in {} directories with a total size of {}",
            tree_info.file_count,
            tree_info.folder_count,
            human_size(tree_info.total_file_size),
        );
    }

    for folder in &tree.folders() {
        let folder_parents = folder
            .info
            .parent
            .as_ref()
            .map(|p| vec![p.drive_id.clone()])
            .or_else(|| config.common.parents.clone());

        if !config.common.print_only_id {
            println!(
                "Creating directory '{}' with id: {}",
                folder.relative_path().display(),
                folder.info.drive_id
            );
        }

        let drive_folder = mkdir::create_directory(
            hub,
            &mkdir::Config {
                id: Some(folder.info.drive_id.clone()),
                name: folder.info.name.clone(),
                parents: folder_parents,
                print_only_id: false,
            },
        )
        .await
        .map_err(|err| Error::Mkdir(Box::new(err)))?;

        if config.common.print_only_id {
            println!(
                "{}: {}",
                folder.relative_path().display(),
                folder.info.drive_id
            );
        }

        let folder_id = drive_folder.id.ok_or(Error::DriveFolderMissingId)?;
        let parents = Some(vec![folder_id.clone()]);

        for file in folder.files() {
            let os_file = match fs::File::open(&file.path) {
                Ok(os_file) => os_file,
                Err(err) => {
                    return Err(Error::OpenFile(file.path.clone(), err));
                }
            };

            let file_info = file.info(parents.clone());

            if !config.common.print_only_id {
                println!(
                    "Uploading file '{}' with id: {}",
                    file.relative_path().display(),
                    file.drive_id
                );
            }

            upload_file(
                hub,
                os_file,
                Some(file.drive_id.clone()),
                file_info,
                delegate_config,
            )
            .await
            .map_err(|err| Error::Upload(Box::new(err)))?;

            if config.common.print_only_id {
                println!("{}: {}", file.relative_path().display(), file.drive_id);
            }
        }
    }

    if !config.common.print_only_id {
        println!(
            "Uploaded {} files in {} directories with a total size of {}",
            tree_info.file_count,
            tree_info.folder_count,
            human_size(tree_info.total_file_size),
        );
    }

    Ok(())
}

/// Uploads `src_file` as a new file with the pre-generated id `file_id`, if given, through
/// gdrive's uploader (multipart up to the configured threshold, resumable above, md5 checked;
/// sessions are not kept across runs). The content is all of `src_file`, measured when the upload starts, not
/// `file_info.size` (which may be older); a size change during the upload is an error. A `file_info.source_mime_type` makes it a
/// conversion to `file_info.mime_type`. Errors are google-drive3's, as they were when
/// google-drive3 made the upload (see [`UploadError::into_drive_error`]).
///
/// The bridge for callers that have a reader rather than a path; with a path,
/// `transfer::upload::upload_path` reads the file asynchronously and continues interrupted
/// uploads.
#[expect(
    clippy::result_large_err,
    reason = "Ok variant is bigger, see test next to FileResult"
)]
pub(crate) async fn upload_file<RS>(
    hub: &Hub,
    src_file: RS,
    file_id: Option<String>,
    file_info: FileInfo<'_>,
    delegate_config: &UploadDelegateConfig,
) -> FileResult
where
    RS: google_apis_common::ReadSeek,
{
    let FileInfo {
        name,
        mime_type,
        source_mime_type,
        parents,
        size: _,
    } = file_info;
    let (mime, convert_to) = match source_mime_type {
        Some(source_mime_type) => (source_mime_type.clone(), Some(mime_type.into_owned())),
        None => (mime_type.into_owned(), None),
    };
    let target = UploadTarget {
        parents: parents.unwrap_or_default(),
        name: Some(name.into_owned()),
        mime: Some(mime),
        existing_file_id: None,
        convert_to,
        new_file_id: file_id,
    };

    upload_reader(
        hub,
        src_file,
        &target,
        &delegate_config.options(),
        delegate_config.retry_policy,
        None,
    )
    .await
    .map_err(UploadError::into_drive_error)
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    FileInfo {
        path: PathBuf,
        source: file_info::FromFileError,
    },
    OpenFile(PathBuf, io::Error),
    StdinToFile(file_helper::StdinToFileError),
    Upload(Box<google_drive3::Error>),
    Transfer(Box<UploadError>),
    IsDirectory(PathBuf),
    DriveFolderMissingId,
    CreateFileTree(file_tree::errors::FileTree),
    Mkdir(Box<google_drive3::Error>),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::FileInfo { source, .. } => Some(source),
            Error::OpenFile(_, source) => Some(source),
            Error::StdinToFile(source) => Some(source),
            Error::Upload(source) | Error::Mkdir(source) => Some(source),
            Error::Transfer(source) => Some(source.as_ref()),
            Error::IsDirectory(_) | Error::DriveFolderMissingId => None,
            Error::CreateFileTree(file_tree) => Some(file_tree),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::FileInfo { path, source: _ } => {
                write!(f, "unable to get file info for '{}'", path.display())
            }
            Error::OpenFile(path, _) => {
                write!(f, "unable to open file '{}'", path.display())
            }
            Error::StdinToFile(_) => f.write_str("unable to write stdin to file"),
            Error::Upload(_) | Error::Transfer(_) => f.write_str("unable to upload file"),
            Error::IsDirectory(path) => write!(
                f,
                "'{}' is a directory, use --recursive to upload directories",
                path.display()
            ),
            Error::DriveFolderMissingId => {
                f.write_str("folder created on drive does not have an id")
            }
            Error::CreateFileTree(_) => f.write_str("unable to create file tree"),
            Error::Mkdir(_) => f.write_str("unable to create directory"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, io::Cursor, path::PathBuf};

    use serde_json::json;

    use crate::transfer::upload::UploadOptions;

    use super::{upload_file, upload_regular, Config, FilePath, UploadRegularConfig};
    use crate::common::{
        delegate::UploadDelegateConfig,
        file_info::FileInfo,
        mock_server::{MockServer, Response},
    };

    /// The metadata part of a multipart upload body.
    fn multipart_metadata(body: &str) -> serde_json::Value {
        let start = body.find('{').expect("a metadata part");
        let end = start
            + body[start..]
                .find("}\r\n")
                .expect("the end of the metadata")
            + 1;
        serde_json::from_str(&body[start..end]).unwrap()
    }

    /// `gdrive files upload --print-only-id [--name NAME] [FILE_PATH]`, multipart up to 5 MiB.
    fn single_file_config(file_path: Option<PathBuf>, name: Option<&str>) -> Config {
        Config {
            file_path,
            name: name.map(String::from),
            mime_type: None,
            parents: None,
            chunk_size: "1".parse().unwrap(),
            multipart_threshold: "5".parse().unwrap(),
            print_chunk_errors: false,
            print_chunk_info: false,
            upload_directories: false,
            print_only_id: true,
        }
    }

    /// `printf hello | gdrive files upload --print-only-id [--name NAME]`: the metadata Drive
    /// got, and the name of the spool of stdin.
    async fn upload_from_stdin(name: Option<&str>) -> (serde_json::Value, String) {
        let content = b"hello";
        let server = MockServer::start(vec![
            Response::json(
                200,
                &json!({"kind": "drive#generatedIds", "ids": ["new-id"]}),
            ),
            Response::json(
                200,
                &json!({
                    "id": "new-id",
                    "size": "5",
                    "md5Checksum": format!("{:x}", md5::compute(content)),
                }),
            ),
        ])
        .await;
        let spool = mktemp::Temp::new_file().unwrap();
        std::fs::write(&spool, content).unwrap();
        let spool_name = spool.file_name().unwrap().to_string_lossy().into_owned();
        let (common, file_path, _, options) = single_file_config(None, name).split();
        assert_eq!(file_path, None);

        upload_regular(
            &server.hub(),
            UploadRegularConfig {
                file_path: FilePath::Temp(spool),
                common,
            },
            &options,
        )
        .await
        .unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 2, "generateIds and the upload");
        let upload = &requests[1];
        assert_eq!(upload.path, "/upload/drive/v3/files");
        assert_eq!(upload.query_param("uploadType"), Some("multipart"));
        assert!(upload.body_text().contains("\r\n\r\nhello\r\n"));
        (multipart_metadata(&upload.body_text()), spool_name)
    }

    #[tokio::test]
    async fn an_upload_from_stdin_gets_the_name_given() {
        let (metadata, _) = upload_from_stdin(Some("notes.txt")).await;
        // The type follows the name, as the spool has no extension.
        assert_eq!(
            metadata,
            json!({"id": "new-id", "name": "notes.txt", "mimeType": "text/plain"})
        );
    }

    #[tokio::test]
    async fn an_upload_from_stdin_without_a_name_gets_a_random_one() {
        let (metadata, spool_name) = upload_from_stdin(None).await;
        assert_eq!(
            metadata,
            json!({"id": "new-id", "name": spool_name, "mimeType": "application/octet-stream"})
        );
    }

    #[tokio::test]
    async fn the_name_given_replaces_the_local_file_name() {
        let content = b"a,b\n";
        let server = MockServer::start(vec![
            Response::json(
                200,
                &json!({"kind": "drive#generatedIds", "ids": ["new-id"]}),
            ),
            Response::json(
                200,
                &json!({
                    "id": "new-id",
                    "size": "4",
                    "md5Checksum": format!("{:x}", md5::compute(content)),
                }),
            ),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("table.csv");
        std::fs::write(&path, content).unwrap();
        let (common, _, _, options) =
            single_file_config(Some(path.clone()), Some("Q3 numbers")).split();

        upload_regular(
            &server.hub(),
            UploadRegularConfig {
                file_path: FilePath::Regular(path),
                common,
            },
            &UploadOptions {
                session_dir: None,
                ..options
            },
        )
        .await
        .unwrap();

        // The type still follows the local file.
        assert_eq!(
            multipart_metadata(&server.requests()[1].body_text()),
            json!({"id": "new-id", "name": "Q3 numbers", "mimeType": "text/csv"})
        );
    }

    fn file_info(size: u64) -> FileInfo<'static> {
        FileInfo {
            name: Cow::Borrowed("a.txt"),
            mime_type: Cow::Owned(mime::TEXT_PLAIN),
            source_mime_type: None,
            parents: Some(vec![String::from("folder-id")]),
            size,
        }
    }

    #[tokio::test]
    async fn upload_file_creates_the_file_with_the_given_id() {
        let content = b"hello";
        let server = MockServer::start(vec![Response::json(
            200,
            &json!({
                "id": "pre-id",
                "name": "a.txt",
                "size": "5",
                "md5Checksum": format!("{:x}", md5::compute(content)),
            }),
        )])
        .await;

        let file = upload_file(
            &server.hub(),
            Cursor::new(content.to_vec()),
            Some(String::from("pre-id")),
            file_info(5),
            &UploadDelegateConfig::default(),
        )
        .await
        .unwrap();

        assert_eq!(file.id.as_deref(), Some("pre-id"));
        // No files.generateIds: the id was given.
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/upload/drive/v3/files");
        assert_eq!(requests[0].query_param("uploadType"), Some("multipart"));
        let body = requests[0].body_text();
        assert!(
            body.contains(r#""id":"pre-id""#) && body.contains(r#""parents":["folder-id"]"#),
            "{body}"
        );
        assert!(body.contains("\r\n\r\nhello\r\n"), "{body}");
    }

    /// `files upload --recursive --multipart-threshold 0 --chunk-size 1`: the configuration
    /// the recursive upload gets.
    fn recursive_config() -> UploadDelegateConfig {
        let (_, _, delegate_config, _) = Config {
            file_path: Some(PathBuf::from("dir")),
            name: None,
            mime_type: None,
            parents: None,
            chunk_size: "1".parse().unwrap(),
            multipart_threshold: "0".parse().unwrap(),
            print_chunk_errors: false,
            print_chunk_info: false,
            upload_directories: true,
            print_only_id: false,
        }
        .split();
        delegate_config
    }

    #[test]
    fn the_recursive_upload_gets_the_selected_threshold() {
        let options = recursive_config().options();
        assert_eq!(options.multipart_threshold, 0);
        assert_eq!(options.chunk_size, 1 << 20);
    }

    #[tokio::test]
    async fn upload_file_follows_the_selected_threshold() {
        // A 5-byte file is resumable with a threshold of 0.
        let content = b"hello";
        let server = MockServer::start(vec![
            Response::json(200, &json!({})).with_header("Location", "{root}upload-session"),
            Response::json(
                200,
                &json!({
                    "id": "pre-id",
                    "size": "5",
                    "md5Checksum": format!("{:x}", md5::compute(content)),
                }),
            ),
        ])
        .await;

        upload_file(
            &server.hub(),
            Cursor::new(content.to_vec()),
            Some(String::from("pre-id")),
            file_info(5),
            &recursive_config(),
        )
        .await
        .unwrap();

        let requests = server.requests();
        assert_eq!(requests[0].query_param("uploadType"), Some("resumable"));
        assert_eq!(requests[1].header("content-range"), Some("bytes 0-4/5"));
    }

    /// The recursive upload measures its files while it discovers them, earlier than it uploads
    /// them: a file that grew in between goes up whole.
    #[tokio::test]
    async fn upload_file_sends_what_the_file_holds_when_it_is_uploaded() {
        let content = b"hello, world";
        let server = MockServer::start(vec![Response::json(
            200,
            &json!({
                "id": "pre-id",
                "size": "12",
                "md5Checksum": format!("{:x}", md5::compute(content)),
            }),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, &content[..5]).unwrap();
        let discovered = file_info(std::fs::metadata(&path).unwrap().len());
        std::fs::write(&path, content).unwrap();

        upload_file(
            &server.hub(),
            std::fs::File::open(&path).unwrap(),
            Some(String::from("pre-id")),
            discovered,
            &UploadDelegateConfig::default(),
        )
        .await
        .unwrap();

        let body = server.requests()[0].body_text();
        assert!(body.contains("\r\n\r\nhello, world\r\n"), "{body}");
    }

    #[tokio::test]
    async fn upload_file_reports_drive_errors_as_google_drive3_did() {
        let error = json!({"error": {
            "code": 403,
            "message": "The user's Drive storage quota has been exceeded.",
            "errors": [{"domain": "usageLimits", "reason": "storageQuotaExceeded"}],
        }});
        let server = MockServer::start(vec![Response::json(403, &error)]).await;

        let result = upload_file(
            &server.hub(),
            Cursor::new(b"hello".to_vec()),
            Some(String::from("pre-id")),
            file_info(5),
            &UploadDelegateConfig::default(),
        )
        .await;

        assert!(
            matches!(&result, Err(google_drive3::Error::BadRequest(body)) if *body == error),
            "{result:?}"
        );
    }
}
