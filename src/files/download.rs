pub(crate) mod errors;

use std::path::{Path, PathBuf};

use async_recursion::async_recursion;
use bytesize::ByteSize;
use error_trace::ErrorTrace;
use futures::stream::StreamExt;
use google_apis_common::Body;
use http_body_util::BodyExt;
use md5::Digest;
use tokio::{
    fs::{self, File},
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
};

use crate::{
    common::{
        drive_file,
        file_tree_drive::{self, errors::FileIdentifier, FileTreeDrive},
        hub_helper::get_hub,
        md5_writer::Md5Writer,
        parse_md5_digest, FileTreeLike, FolderLike,
    },
    files,
    hub::Hub,
};

pub(crate) struct Config {
    pub(crate) file_id: String,
    pub(crate) existing_file_action: ExistingFileAction,
    pub(crate) follow_shortcuts: bool,
    pub(crate) download_directories: bool,
    pub(crate) destination: Destination,
}

impl Config {
    fn canonical_destination_root(&self) -> Result<PathBuf, errors::Download> {
        use errors::Download as E;

        match &self.destination {
            Destination::CurrentDir => {
                let current_path = PathBuf::from(".");
                let canonical_current_path = current_path
                    .canonicalize()
                    .map_err(|err| E::CanonicalizeDestinationPath(current_path.clone(), err))?;
                Ok(canonical_current_path)
            }

            Destination::Path(path) => {
                if !path.exists() {
                    Err(E::DestinationPathDoesNotExist(path.clone()))
                } else if !path.is_dir() {
                    Err(E::DestinationPathNotADirectory(path.clone()))
                } else {
                    path.canonicalize()
                        .map_err(|err| E::CanonicalizeDestinationPath(path.clone(), err))
                }
            }

            Destination::Stdout => Err(E::StdoutNotValidDestination),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum Destination {
    CurrentDir,
    Path(PathBuf),
    Stdout,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum ExistingFileAction {
    Abort,
    Overwrite,
}

#[async_recursion]
pub(crate) async fn download(config: Config) -> Result<(), errors::Download> {
    use errors::Download as E;

    let hub = get_hub().await.map_err(E::Hub)?;

    let file = files::info::get_file(&hub, &config.file_id)
        .await
        .map_err(|err| E::GetFile(Box::new(err)))?;

    err_if_file_exists(&file, &config)?;

    if drive_file::is_shortcut(&file) {
        if !config.follow_shortcuts {
            return Err(E::IsShortcut(FileIdentifier::from(file)));
        }

        let google_drive3::api::File {
            shortcut_details,
            name,
            id,
            ..
        } = file;
        let target_file_id = shortcut_details.and_then(|details| details.target_id);

        let file_id = target_file_id
            .ok_or_else(|| E::MissingShortcutTarget(FileIdentifier::new(name, id)))?;

        download(Config { file_id, ..config }).await?;
    } else if drive_file::is_directory(&file) {
        if !config.download_directories {
            return Err(E::IsDirectory(FileIdentifier::from(file)));
        }

        download_directory(&hub, file, &config).await?;
    } else {
        download_regular(&hub, &file, &config).await?;
    }

    Ok(())
}

async fn download_regular(
    hub: &Hub,
    file: &google_drive3::api::File,
    config: &Config,
) -> Result<(), errors::Download> {
    use errors::Download as E;

    let body = download_file(hub, &config.file_id)
        .await
        .map_err(E::DownloadFile)?;

    if config.destination == Destination::Stdout {
        save_body_to_stdout(body).await?;
    } else {
        let file_name = file
            .name
            .as_deref()
            .ok_or_else(|| E::MissingFileName(FileIdentifier::from(file)))?;
        let mut abs_file_path = config.canonical_destination_root()?;
        abs_file_path.push(file_name);

        println!("Downloading {file_name}");
        let md5_checksum = file.md5_checksum.as_deref().and_then(parse_md5_digest);
        if let Err(source) = save_body_to_file(body, &abs_file_path, md5_checksum.as_ref()).await {
            return Err(E::SaveBodyToFile {
                path: abs_file_path,
                source,
            });
        }
        println!("Successfully downloaded {file_name}");
    }

    Ok(())
}

async fn download_directory(
    hub: &Hub,
    file: google_drive3::api::File,
    config: &Config,
) -> Result<(), errors::Download> {
    use errors::Download as E;

    let tree = FileTreeDrive::from_file(hub, file)
        .await
        .map_err(E::CreateFileTree)?;

    let tree_info = tree.info();

    println!(
        "Found {} files in {} directories with a total size of {}",
        tree_info.file_count,
        tree_info.folder_count,
        ByteSize::b(tree_info.total_file_size).display().si(),
    );

    let root_path = config.canonical_destination_root()?;

    for folder in &tree.folders() {
        let folder_path = folder.info.relative_path();
        let abs_folder_path = root_path.join(&folder_path);

        println!("Creating directory {}", folder_path.display());
        fs::create_dir_all(&abs_folder_path)
            .await
            .map_err(|err| E::CreateDirectory(abs_folder_path, err))?;

        for file in folder.files() {
            let file_path = file.relative_path();
            let abs_file_path = root_path.join(&file_path);

            if local_file_is_identical(&abs_file_path, &file).await {
                continue;
            }

            let body = download_file(hub, &file.drive_id)
                .await
                .map_err(E::DownloadFile)?;

            println!("Downloading file '{}'", file_path.display());
            if let Err(source) = save_body_to_file(body, &abs_file_path, file.md5.as_ref()).await {
                return Err(E::SaveBodyToFile {
                    path: abs_file_path,
                    source,
                });
            }
        }
    }

    println!(
        "Downloaded {} files in {} directories with a total size of {}",
        tree_info.file_count,
        tree_info.folder_count,
        ByteSize::b(tree_info.total_file_size).display().si()
    );

    Ok(())
}

async fn download_file(hub: &Hub, file_id: &str) -> Result<Body, Box<google_drive3::Error>> {
    let (response, _) = hub
        .files()
        .get(file_id)
        .supports_all_drives(true)
        .param("alt", "media")
        .add_scope(google_drive3::api::Scope::Full)
        .doit()
        .await?;

    Ok(response.into_body())
}

// TODO: move to common
pub(crate) async fn save_body_to_file(
    body: Body,
    file_path: &Path,
    expected_md5: Option<&Digest>,
) -> Result<(), errors::SaveBodyToFile> {
    use errors::SaveBodyToFile as E;

    // Create temporary file
    let tmp_file_path = file_path.with_extension("incomplete");
    let file = File::create(&tmp_file_path).await.map_err(E::CreateFile)?;

    // The file is closed when this returns, before it is renamed
    let md5_digest = write_body(body, file).await?;

    // Check md5
    if let Some(expected_md5) = expected_md5 {
        if *expected_md5 != md5_digest {
            return Err(E::Md5Mismatch {
                expected: *expected_md5,
                actual: md5_digest,
            });
        }
    }

    // Rename temporary file to final file
    fs::rename(&tmp_file_path, file_path)
        .await
        .map_err(E::RenameFile)
}

/// Writes `body` to `writer` and returns the md5 digest of the bytes written.
///
/// The writer is flushed before the digest is taken: `tokio::fs::File` reports the error of a write
/// only on the next write or flush, so without the flush a failure on the last chunk (a full disk,
/// for example) would be lost and the incomplete file renamed as if it were complete.
async fn write_body<W>(body: Body, writer: W) -> Result<Digest, errors::SaveBodyToFile>
where
    W: AsyncWrite + Unpin,
{
    use errors::SaveBodyToFile as E;

    // Wrap the writer in one that calculates md5
    let mut writer = Md5Writer::new(writer);

    let mut body = body.into_stream();

    // Read chunks from stream and write them
    while let Some(chunk_result) = body.next().await {
        let chunk = chunk_result.map_err(E::ReadChunk)?;
        if let Some(data) = chunk.data_ref() {
            writer.write_all(data).await.map_err(E::WriteChunk)?;
        }
    }

    writer.flush().await.map_err(E::Flush)?;

    Ok(writer.md5())
}

// TODO: move to common
async fn save_body_to_stdout(body: Body) -> Result<(), errors::SaveBodyToStdout> {
    let mut stdout = io::stdout();
    let mut body = body.into_stream();

    // Read chunks from stream and write to stdout
    while let Some(chunk_result) = body.next().await {
        let chunk = chunk_result.map_err(errors::SaveBodyToStdout::ReadChunk)?;
        if let Some(data) = chunk.data_ref() {
            stdout
                .write_all(data)
                .await
                .map_err(errors::SaveBodyToStdout::WriteChunk)?;
        }
    }

    // Like a file, stdout reports the error of the last write only on flush
    stdout
        .flush()
        .await
        .map_err(errors::SaveBodyToStdout::WriteChunk)?;

    Ok(())
}

fn err_if_file_exists(
    file: &google_drive3::api::File,
    config: &Config,
) -> Result<(), errors::Download> {
    let file_name = file
        .name
        .as_deref()
        .ok_or_else(|| errors::Download::MissingFileName(FileIdentifier::from(file)))?;

    let file_path = match &config.destination {
        Destination::CurrentDir => Some(Path::new(".").join(file_name)),
        Destination::Path(path) => Some(path.join(file_name)),
        Destination::Stdout => None,
    };

    match file_path {
        Some(path) => {
            if path.exists() && config.existing_file_action == ExistingFileAction::Abort {
                Err(errors::Download::FileExists(FileIdentifier::from(file)))
            } else {
                Ok(())
            }
        }

        None => Ok(()),
    }
}

async fn local_file_is_identical(path: &Path, file: &file_tree_drive::File) -> bool {
    if path.exists() {
        match compute_md5_from_path(path).await {
            Ok(file_md5) => file.md5.as_ref().is_some_and(|md5| md5 == &file_md5),
            Err(err) => {
                eprintln!(
                    "Warning: Error while computing md5 of '{}': {}",
                    path.display(),
                    err.trace(),
                );
                false
            }
        }
    } else {
        false
    }
}

async fn compute_md5_from_path(path: &Path) -> Result<Digest, io::Error> {
    let input = File::open(path).await?;
    let reader = BufReader::new(input);
    compute_md5_from_async_reader(reader).await
}

async fn compute_md5_from_async_reader<R>(mut reader: R) -> Result<Digest, io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut context = md5::Context::new();
    let mut buffer = [0; 4096];

    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => context.consume(&buffer[..count]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }

    Ok(context.compute())
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    use bytes::Bytes;

    use super::*;

    const DATA: &[u8] = b"the last chunk of a download";

    fn body() -> Body {
        google_apis_common::to_body(Some(Bytes::from_static(DATA)))
    }

    /// Accepts every write, as `tokio::fs::File` does while the write runs in the background, and
    /// reports the failure of that write on flush.
    struct FailsOnFlush;

    impl AsyncWrite for FailsOnFlush {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("no space left on device")))
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    #[tokio::test]
    async fn write_body_returns_md5_of_the_written_bytes() {
        let mut written = Vec::new();

        let digest = write_body(body(), &mut written).await.unwrap();

        assert_eq!(written, DATA);
        assert_eq!(digest, md5::compute(DATA));
    }

    #[tokio::test]
    async fn write_body_reports_a_write_error_that_surfaces_on_flush() {
        let result = write_body(body(), FailsOnFlush).await;

        assert!(
            matches!(result, Err(errors::SaveBodyToFile::Flush(_))),
            "{result:?}"
        );
    }

    /// The same with a real `tokio::fs::File`: every write to /dev/full fails with ENOSPC, but
    /// tokio only reports it on the next write or flush.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn write_body_reports_a_full_disk_on_the_last_chunk() {
        let Ok(file) = File::options().write(true).open("/dev/full").await else {
            return;
        };

        let result = write_body(body(), file).await;

        assert!(
            matches!(result, Err(errors::SaveBodyToFile::Flush(_))),
            "{result:?}"
        );
    }
}
