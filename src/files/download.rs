pub(crate) mod errors;
#[cfg(test)]
mod media_tests;

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
    io::{self, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
};

use crate::{
    common::{
        drive_file,
        file_tree_drive::{self, errors::FileIdentifier, FileTreeDrive},
        hub_helper::get_hub,
        local_name::{self, NameChange, UnusableName},
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
    /// Download files Google flagged as malware or spam (owner or shared drive organizer only).
    pub(crate) acknowledge_abuse: bool,
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

pub(crate) async fn download(config: Config) -> Result<(), errors::Download> {
    let hub = get_hub().await.map_err(errors::Download::Hub)?;
    download_with_hub(&hub, config).await
}

#[async_recursion]
async fn download_with_hub(hub: &Hub, config: Config) -> Result<(), errors::Download> {
    use errors::Download as E;

    let file = files::info::get_file(hub, &config.file_id)
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

        download_with_hub(hub, Config { file_id, ..config }).await?;
    } else if drive_file::is_directory(&file) {
        if !config.download_directories {
            return Err(E::IsDirectory(FileIdentifier::from(file)));
        }

        download_directory(hub, file, &config).await?;
    } else {
        download_regular(hub, &file, &config).await?;
    }

    Ok(())
}

async fn download_regular(
    hub: &Hub,
    file: &google_drive3::api::File,
    config: &Config,
) -> Result<(), errors::Download> {
    use errors::Download as E;

    let body = download_file(hub, &config.file_id, config.acknowledge_abuse)
        .await
        .map_err(E::DownloadFile)?;

    if config.destination == Destination::Stdout {
        save_body_to_stdout(body).await?;
    } else {
        let file_name = local_file_name(file, config)?;
        warn_if_renamed(file, config, &file_name);
        let abs_file_path =
            local_name::safe_join(&config.canonical_destination_root()?, &file_name);

        println!("Downloading {file_name}");
        let md5_checksum = file.md5_checksum.as_deref().and_then(parse_md5_digest);
        let file_id = file.id.as_deref().unwrap_or(&config.file_id);
        if let Err(source) =
            save_body_to_file(body, &abs_file_path, file_id, md5_checksum.as_ref()).await
        {
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

            err_if_local_file_exists(&abs_file_path, config.existing_file_action).await?;

            if local_file_is_identical(&abs_file_path, &file).await {
                continue;
            }

            let body = download_file(hub, &file.drive_id, config.acknowledge_abuse)
                .await
                .map_err(E::DownloadFile)?;

            println!("Downloading file '{}'", file_path.display());
            if let Err(source) =
                save_body_to_file(body, &abs_file_path, &file.drive_id, file.md5.as_ref()).await
            {
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

async fn download_file(
    hub: &Hub,
    file_id: &str,
    acknowledge_abuse: bool,
) -> Result<Body, Box<google_drive3::Error>> {
    let (response, _) = hub
        .files()
        .get(file_id)
        .supports_all_drives(true)
        .acknowledge_abuse(acknowledge_abuse)
        .param("alt", "media")
        .add_scope(google_drive3::api::Scope::Full)
        .doit()
        .await?;

    Ok(response.into_body())
}

/// How many temporary file names [`create_temp_file`] tries before it gives up.
const MAX_TEMP_FILE_ATTEMPTS: u32 = 100;

/// Saves `body` to `file_path` through a temporary file named after the Drive file id.
///
/// The temporary file is created next to `file_path` with `create_new`, so it is never shared
/// with another download, removed if anything fails, and renamed to `file_path` only after the
/// whole body is written and its md5 matches.
// TODO: move to common
pub(crate) async fn save_body_to_file(
    body: Body,
    file_path: &Path,
    file_id: &str,
    expected_md5: Option<&Digest>,
) -> Result<(), errors::SaveBodyToFile> {
    let (tmp_file_path, file) = create_temp_file(file_path, file_id).await?;

    let result = write_temp_file(body, file, &tmp_file_path, file_path, expected_md5).await;
    if result.is_err() {
        remove_temp_file(&tmp_file_path).await;
    }

    result
}

/// Creates the temporary file for `file_path`. It refuses a `file_path` that has the name of a
/// temporary file, and a temporary path that exists but is not a regular file: in both cases the
/// temporary file could alias a final destination.
async fn create_temp_file(
    file_path: &Path,
    file_id: &str,
) -> Result<(PathBuf, File), errors::SaveBodyToFile> {
    let refuse = |message: String| {
        errors::SaveBodyToFile::CreateFile(io::Error::new(io::ErrorKind::InvalidInput, message))
    };

    let file_name = file_path.file_name().map(|name| name.to_string_lossy());
    if file_name.is_some_and(|name| local_name::is_reserved_temp_name(&name)) {
        return Err(refuse(format!(
            "refusing to write '{}': the name is reserved for temporary download files",
            file_path.display()
        )));
    }

    let dir = file_path.parent().unwrap_or_else(|| Path::new(""));

    for attempt in 0..MAX_TEMP_FILE_ATTEMPTS {
        let tmp_file_path =
            local_name::safe_join(dir, &local_name::temp_file_name(file_id, attempt));
        let created = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_file_path)
            .await;

        match created {
            Ok(file) => return Ok((tmp_file_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                // A regular file is a stale or concurrent temporary file: try the next name.
                let existing = fs::symlink_metadata(&tmp_file_path).await;
                if existing.is_ok_and(|metadata| !metadata.is_file()) {
                    return Err(refuse(format!(
                        "refusing to use '{}' as a temporary file: it exists and is not a \
                        regular file",
                        tmp_file_path.display()
                    )));
                }
            }
            Err(err) => return Err(errors::SaveBodyToFile::CreateFile(err)),
        }
    }

    Err(errors::SaveBodyToFile::CreateFile(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "all temporary file names for file {file_id} are taken, remove the stale \
            '.gdrive-*.part' files next to '{}'",
            file_path.display()
        ),
    )))
}

async fn remove_temp_file(path: &Path) {
    if let Err(err) = fs::remove_file(path).await {
        if err.kind() != io::ErrorKind::NotFound {
            eprintln!(
                "Warning: unable to remove temporary file '{}': {err}",
                path.display()
            );
        }
    }
}

/// Streams `body` into the temporary file, checks the md5 and renames the file to `file_path`.
async fn write_temp_file(
    body: Body,
    file: File,
    tmp_file_path: &Path,
    file_path: &Path,
    expected_md5: Option<&Digest>,
) -> Result<(), errors::SaveBodyToFile> {
    use errors::SaveBodyToFile as E;

    // Wrap file in writer that calculates md5
    let mut writer = Md5Writer::new(file);

    let mut body = body.into_stream();

    // Read chunks from stream and write to file
    while let Some(chunk_result) = body.next().await {
        let chunk = chunk_result.map_err(E::ReadChunk)?;
        if let Some(data) = chunk.data_ref() {
            writer.write_all(data).await.map_err(E::WriteChunk)?;
        }
    }

    // Check md5
    let md5_digest = writer.md5();
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

    Ok(())
}

/// Safe local name of a single downloaded file or of the root of a recursive download.
fn local_file_name(
    file: &google_drive3::api::File,
    config: &Config,
) -> Result<String, errors::Download> {
    let name = file
        .name
        .as_deref()
        .ok_or_else(|| errors::Download::MissingFileName(FileIdentifier::from(file)))?;
    let id = file.id.as_deref().unwrap_or(&config.file_id);

    local_name::local_name(name, id)
        .map_err(|source| errors::Download::UnusableName(UnusableName::new(name, id, source)))
}

fn warn_if_renamed(file: &google_drive3::api::File, config: &Config, local_name: &str) {
    let Some(name) = file.name.as_deref() else {
        return;
    };

    if local_name::is_renamed(name, local_name) {
        let change = NameChange::Sanitized {
            id: file.id.clone().unwrap_or_else(|| config.file_id.clone()),
            drive_name: name.to_owned(),
            local_name: local_name.to_owned(),
        };
        eprintln!("Warning: {change}");
    }
}

fn err_if_file_exists(
    file: &google_drive3::api::File,
    config: &Config,
) -> Result<(), errors::Download> {
    // A followed shortcut is checked under the name of its target.
    if drive_file::is_shortcut(file) {
        return Ok(());
    }

    let destination = match &config.destination {
        Destination::CurrentDir => Path::new("."),
        Destination::Path(path) => path.as_path(),
        Destination::Stdout => return Ok(()),
    };

    let path = local_name::safe_join(destination, &local_file_name(file, config)?);

    // symlink_metadata: a dangling symlink is an existing entry too.
    if config.existing_file_action == ExistingFileAction::Abort
        && std::fs::symlink_metadata(&path).is_ok()
    {
        Err(errors::Download::FileExists(FileIdentifier::from(file)))
    } else {
        Ok(())
    }
}

/// The `--overwrite` check for each file of a recursive download.
async fn err_if_local_file_exists(
    path: &Path,
    existing_file_action: ExistingFileAction,
) -> Result<(), errors::Download> {
    if existing_file_action == ExistingFileAction::Abort && fs::symlink_metadata(path).await.is_ok()
    {
        Err(errors::Download::LocalFileExists(path.to_path_buf()))
    } else {
        Ok(())
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
    use std::path::Path;

    use bytes::Bytes;
    use google_apis_common::Body;
    use http_body_util::{BodyExt, Full};

    use super::{
        create_temp_file, err_if_file_exists, err_if_local_file_exists, errors, save_body_to_file,
        write_temp_file, Config, Destination, ExistingFileAction,
    };
    use crate::common::{
        drive_file::{MIME_TYPE_DRIVE_FOLDER, MIME_TYPE_DRIVE_SHORTCUT},
        local_name,
    };

    fn is_refusal(result: &Result<(), errors::SaveBodyToFile>) -> bool {
        matches!(
            result,
            Err(errors::SaveBodyToFile::CreateFile(err)) if err.kind() == std::io::ErrorKind::InvalidInput
        )
    }

    fn body(content: &'static [u8]) -> Body {
        Full::new(Bytes::from_static(content))
            .map_err(|never| match never {})
            .boxed()
    }

    /// A response body that yields `partial` and then fails: an in-process server promises more
    /// bytes than it sends and closes the connection.
    async fn truncated_body(partial: &'static [u8]) -> Body {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buf).await.unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..read]);
            }

            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                partial.len() + 100
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(partial).await.unwrap();
            socket.flush().await.unwrap();
            // Let the client read the bytes before the connection drops.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });

        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http::<http_body_util::Empty<Bytes>>();
        let response = client
            .get(format!("http://{addr}/").parse().unwrap())
            .await
            .unwrap();
        response.into_body().boxed()
    }

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn config(destination: &Path, existing_file_action: ExistingFileAction) -> Config {
        Config {
            file_id: "file-id".to_string(),
            existing_file_action,
            follow_shortcuts: false,
            download_directories: true,
            destination: Destination::Path(destination.to_path_buf()),
            acknowledge_abuse: false,
        }
    }

    fn drive_file(name: &str, mime_type: Option<&str>) -> google_drive3::api::File {
        google_drive3::api::File {
            id: Some("file-id".to_string()),
            name: Some(name.to_string()),
            mime_type: mime_type.map(str::to_string),
            ..google_drive3::api::File::default()
        }
    }

    #[test]
    fn existing_file_aborts_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"old").unwrap();
        let file = drive_file("a.txt", None);

        let abort = config(dir.path(), ExistingFileAction::Abort);
        assert!(matches!(
            err_if_file_exists(&file, &abort),
            Err(errors::Download::FileExists(_))
        ));

        let overwrite = config(dir.path(), ExistingFileAction::Overwrite);
        assert!(err_if_file_exists(&file, &overwrite).is_ok());

        let other = drive_file("b.txt", None);
        assert!(err_if_file_exists(&other, &abort).is_ok());
    }

    #[test]
    fn existing_directory_aborts_recursive_download() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("folder")).unwrap();
        let folder = drive_file("folder", Some(MIME_TYPE_DRIVE_FOLDER));

        let abort = config(dir.path(), ExistingFileAction::Abort);
        assert!(matches!(
            err_if_file_exists(&folder, &abort),
            Err(errors::Download::FileExists(_))
        ));
    }

    #[test]
    fn existence_check_uses_the_sanitized_name() {
        let dest = tempfile::tempdir().unwrap();
        let inner = dest.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        // `../victim` must be checked as `inner/.._victim`, never as `dest/victim`.
        std::fs::write(dest.path().join("victim"), b"keep").unwrap();
        let abort = config(&inner, ExistingFileAction::Abort);

        assert!(err_if_file_exists(&drive_file("../victim", None), &abort).is_ok());

        std::fs::write(inner.join(".._victim"), b"").unwrap();
        assert!(matches!(
            err_if_file_exists(&drive_file("../victim", None), &abort),
            Err(errors::Download::FileExists(_))
        ));
    }

    #[test]
    fn unusable_name_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let abort = config(dir.path(), ExistingFileAction::Abort);
        assert!(matches!(
            err_if_file_exists(&drive_file("..", None), &abort),
            Err(errors::Download::UnusableName(_))
        ));
    }

    #[test]
    fn shortcut_is_checked_under_its_target_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("link"), b"").unwrap();
        let abort = config(dir.path(), ExistingFileAction::Abort);
        let shortcut = drive_file("link", Some(MIME_TYPE_DRIVE_SHORTCUT));
        assert!(err_if_file_exists(&shortcut, &abort).is_ok());
    }

    #[test]
    fn stdout_destination_skips_the_check() {
        let mut stdout = config(Path::new("."), ExistingFileAction::Abort);
        stdout.destination = Destination::Stdout;
        assert!(err_if_file_exists(&drive_file("..", None), &stdout).is_ok());
    }

    #[tokio::test]
    async fn recursive_files_honor_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("a.txt");
        std::fs::write(&existing, b"old").unwrap();

        assert!(matches!(
            err_if_local_file_exists(&existing, ExistingFileAction::Abort).await,
            Err(errors::Download::LocalFileExists(path)) if path == existing
        ));
        assert!(
            err_if_local_file_exists(&existing, ExistingFileAction::Overwrite)
                .await
                .is_ok()
        );
        assert!(
            err_if_local_file_exists(&dir.path().join("new.txt"), ExistingFileAction::Abort)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn save_renames_temp_file_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.pdf");
        let md5 = md5::compute(b"content");

        save_body_to_file(body(b"content"), &target, "id-a", Some(&md5))
            .await
            .unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"content");
        assert_eq!(dir_entries(dir.path()), ["a.pdf"]);
    }

    #[tokio::test]
    async fn files_differing_in_extension_get_distinct_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let (pdf_tmp, _pdf) = create_temp_file(&dir.path().join("a.pdf"), "id-pdf")
            .await
            .unwrap();
        let (docx_tmp, _docx) = create_temp_file(&dir.path().join("a.docx"), "id-docx")
            .await
            .unwrap();

        assert_ne!(pdf_tmp, docx_tmp);
        assert_eq!(pdf_tmp, dir.path().join(".gdrive-id-pdf.part"));
        assert_eq!(docx_tmp, dir.path().join(".gdrive-id-docx.part"));
    }

    #[tokio::test]
    async fn concurrent_downloads_do_not_share_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("a.pdf");
        let docx = dir.path().join("a.docx");
        let pdf_md5 = md5::compute(b"pdf bytes");
        let docx_md5 = md5::compute(b"docx bytes");

        let (first, second) = tokio::join!(
            save_body_to_file(body(b"pdf bytes"), &pdf, "id-pdf", Some(&pdf_md5)),
            save_body_to_file(body(b"docx bytes"), &docx, "id-docx", Some(&docx_md5)),
        );
        first.unwrap();
        second.unwrap();

        assert_eq!(std::fs::read(&pdf).unwrap(), b"pdf bytes");
        assert_eq!(std::fs::read(&docx).unwrap(), b"docx bytes");
        assert_eq!(dir_entries(dir.path()), ["a.docx", "a.pdf"]);
    }

    #[tokio::test]
    async fn existing_temp_file_is_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join(".gdrive-id-a.part");
        std::fs::write(&stale, b"belongs to someone else").unwrap();

        let (tmp, _file) = create_temp_file(&dir.path().join("a.pdf"), "id-a")
            .await
            .unwrap();
        assert_eq!(tmp, dir.path().join(".gdrive-id-a-1.part"));
        assert_eq!(std::fs::read(&stale).unwrap(), b"belongs to someone else");
    }

    #[tokio::test]
    async fn failed_download_removes_temp_file_and_keeps_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.pdf");
        std::fs::write(&target, b"old").unwrap();
        let wrong_md5 = md5::compute(b"something else");

        let result = save_body_to_file(body(b"new"), &target, "id-a", Some(&wrong_md5)).await;

        assert!(matches!(
            result,
            Err(errors::SaveBodyToFile::Md5Mismatch { .. })
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(dir_entries(dir.path()), ["a.pdf"]);
    }

    #[tokio::test]
    async fn temp_file_name_does_not_grow_long_names() {
        let dir = tempfile::tempdir().unwrap();
        // 255 bytes: the longest valid name. `<stem>.incomplete` would have been 262.
        let name = format!("{}.pdf", "x".repeat(251));
        let target = dir.path().join(&name);

        save_body_to_file(body(b"long"), &target, "id-long", None)
            .await
            .unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"long");
        assert_eq!(dir_entries(dir.path()), [name]);
    }

    #[tokio::test]
    async fn drive_file_named_like_a_temp_file_cannot_alias_it() {
        let dir = tempfile::tempdir().unwrap();

        // Download B is staging its bytes.
        let b_target = dir.path().join("b.bin");
        let (b_tmp, b_file) = create_temp_file(&b_target, "idB").await.unwrap();
        assert_eq!(b_tmp, dir.path().join(".gdrive-idB.part"));

        // Download A is a Drive file whose name is B's temporary file name.
        let a_name = local_name::local_name(".gdrive-idB.part", "idA").unwrap();
        let a_target = local_name::safe_join(dir.path(), &a_name);
        assert_ne!(a_target, b_tmp);
        save_body_to_file(body(b"A bytes"), &a_target, "idA", None)
            .await
            .unwrap();

        // B finishes with its own bytes in its own destination.
        let b_md5 = md5::compute(b"B bytes");
        write_temp_file(body(b"B bytes"), b_file, &b_tmp, &b_target, Some(&b_md5))
            .await
            .unwrap();

        assert_eq!(std::fs::read(&a_target).unwrap(), b"A bytes");
        assert_eq!(std::fs::read(&b_target).unwrap(), b"B bytes");
        assert_eq!(dir_entries(dir.path()), [".gdrive-idB.part_", "b.bin"]);
    }

    #[tokio::test]
    async fn target_with_temp_file_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".gdrive-x.part");

        let result = save_body_to_file(body(b"data"), &target, "x", None).await;

        assert!(is_refusal(&result), "{result:?}");
        assert_eq!(dir_entries(dir.path()), Vec::<String>::new());
    }

    #[tokio::test]
    async fn temp_path_that_is_not_a_regular_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".gdrive-id-a.part")).unwrap();

        let result =
            save_body_to_file(body(b"data"), &dir.path().join("a.bin"), "id-a", None).await;

        assert!(is_refusal(&result), "{result:?}");
        assert_eq!(dir_entries(dir.path()), [".gdrive-id-a.part"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn temp_path_that_is_a_symlink_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join(".gdrive-id-a.part")).unwrap();

        let result =
            save_body_to_file(body(b"data"), &dir.path().join("a.bin"), "id-a", None).await;

        assert!(is_refusal(&result), "{result:?}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        assert!(!dir.path().join("a.bin").exists());
    }

    #[tokio::test]
    async fn truncated_body_yields_bytes_then_fails() {
        let mut body = truncated_body(b"partial bytes").await;
        let mut received = Vec::new();
        loop {
            match body.frame().await {
                Some(Ok(frame)) => received.extend_from_slice(frame.data_ref().unwrap()),
                Some(Err(_)) => break,
                None => panic!("body ended without an error"),
            }
        }
        assert_eq!(received, b"partial bytes");
    }

    #[tokio::test]
    async fn stream_failure_after_bytes_removes_temp_file_and_keeps_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.bin");
        std::fs::write(&target, b"old").unwrap();

        let body = truncated_body(b"partial bytes").await;
        let result = save_body_to_file(body, &target, "id-a", None).await;

        assert!(
            matches!(result, Err(errors::SaveBodyToFile::ReadChunk(_))),
            "{result:?}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(dir_entries(dir.path()), ["a.bin"]);
    }

    #[tokio::test]
    async fn rename_failure_removes_temp_file_and_keeps_target() {
        let dir = tempfile::tempdir().unwrap();
        // A non-empty directory where the file should go: the final rename fails.
        let target = dir.path().join("a.bin");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep.txt"), b"keep").unwrap();

        let result = save_body_to_file(body(b"new"), &target, "id-a", None).await;

        assert!(
            matches!(result, Err(errors::SaveBodyToFile::RenameFile(_))),
            "{result:?}"
        );
        assert_eq!(std::fs::read(target.join("keep.txt")).unwrap(), b"keep");
        assert_eq!(dir_entries(dir.path()), ["a.bin"]);
        assert_eq!(dir_entries(&target), ["keep.txt"]);
    }
}
