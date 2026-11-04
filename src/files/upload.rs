pub(crate) mod recursive;

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
        delegate::{ChunkSize, MultipartThreshold},
        file_helper,
        file_info::{self, FileInfo},
        file_tree,
        hub_helper::{get_hub, GetHubError},
        local_name,
    },
    files::{self, info::DisplayConfig},
    hub::Hub,
    transfer::{
        scheduler::{self, Printer},
        upload::{
            session::{account_session_dir, account_session_identity},
            upload_path, UploadError, UploadOptions, UploadTarget,
        },
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
    /// How many files a recursive upload transfers at once, and whether it goes on after a
    /// failure.
    pub(crate) transfer: scheduler::Options,
}

impl Config {
    /// The common part, the path and the uploader's options (which record sessions in the
    /// account directory).
    fn split(self) -> (CommonConfig, Option<PathBuf>, UploadOptions) {
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
            transfer,
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
                transfer,
            },
            file_path,
            options,
        )
    }
}

pub(crate) async fn upload(config: Config) -> Result<(), Error> {
    let hub = get_hub().await.map_err(Error::Hub)?;

    let (common, file_path, options) = config.split();

    if let Some(file_path) = file_path {
        if file_path.is_dir() && common.upload_directories.not() {
            return Err(Error::IsDirectory(file_path));
        }

        if file_path.is_dir() {
            upload_directory(&hub, UploadDirectoryConfig { file_path, common }, &options).await?;
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
    transfer: scheduler::Options,
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

/// Uploads the local folder and everything in it: folders are created parent first with
/// pre-generated ids, files are uploaded `common.transfer.jobs` at a time with `options` (see
/// [`recursive`]).
async fn upload_directory(
    hub: &Hub,
    config: UploadDirectoryConfig,
    options: &UploadOptions,
) -> Result<(), Error> {
    let printer = Printer::stdio();
    recursive::upload_directory(hub, config, options, &printer, scheduler::ctrl_c()).await
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
    Transfer(Box<UploadError>),
    IsDirectory(PathBuf),
    DriveFolderMissingId,
    CreateFileTree(file_tree::errors::FileTree),
    Mkdir(Box<google_drive3::Error>),
    /// A file or folder of a recursive upload failed; `path` starts with the uploaded folder.
    RecursiveItem {
        path: PathBuf,
        source: Box<Error>,
    },
    /// Files or folders of a recursive upload failed, or it was interrupted.
    Incomplete(scheduler::Incomplete),
    /// stdout or stderr of a recursive upload could not be written.
    WriteOutput(scheduler::OutputError),
    /// An entry of the local folder of a recursive upload cannot be read.
    ReadLocal(file_tree::errors::Folder),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::FileInfo { source, .. } => Some(source),
            Error::OpenFile(_, source) => Some(source),
            Error::StdinToFile(source) => Some(source),
            Error::Mkdir(source) => Some(source),
            Error::Transfer(source) => Some(source.as_ref()),
            Error::IsDirectory(_) | Error::DriveFolderMissingId => None,
            Error::CreateFileTree(file_tree) => Some(file_tree),
            Error::RecursiveItem { source, .. } => Some(source),
            Error::Incomplete(source) => Some(source),
            Error::WriteOutput(source) => Some(source),
            Error::ReadLocal(source) => Some(source),
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
            Error::Transfer(_) => f.write_str("unable to upload file"),
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
            Error::RecursiveItem { path, source: _ } => {
                write!(f, "unable to upload '{}'", local_name::tree_path(path))
            }
            Error::Incomplete(_) => f.write_str("the recursive upload is incomplete"),
            Error::WriteOutput(_) => {
                f.write_str("unable to write the output of the recursive upload")
            }
            Error::ReadLocal(_) => f.write_str("unable to read it from the local disk"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::{upload_regular, Config, FilePath, UploadRegularConfig};
    use crate::{
        common::mock_server::{MockServer, Response},
        transfer::{scheduler, upload::UploadOptions},
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
            transfer: scheduler::Options::default(),
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
        let (common, file_path, options) = single_file_config(None, name).split();
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
        let (common, _, options) =
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

    /// `files upload --recursive --multipart-threshold 0 --chunk-size 1`: the options the
    /// recursive upload gets.
    #[test]
    fn the_recursive_upload_gets_the_selected_chunk_size_and_threshold() {
        let (common, _, options) = Config {
            file_path: Some(PathBuf::from("dir")),
            name: None,
            mime_type: None,
            parents: None,
            chunk_size: "1".parse().unwrap(),
            multipart_threshold: "0".parse().unwrap(),
            print_chunk_errors: false,
            print_chunk_info: true,
            upload_directories: true,
            print_only_id: false,
            transfer: scheduler::Options::default(),
        }
        .split();

        assert_eq!(options.multipart_threshold, 0);
        assert_eq!(options.chunk_size, 1 << 20);
        assert!(options.print_chunk_info && !options.print_chunk_errors);
        assert!(options.verify_md5);
        assert_eq!(common.transfer, scheduler::Options::default());
    }
}
