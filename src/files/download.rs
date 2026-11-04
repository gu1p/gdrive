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
        let file_name = local_file_name(file, config)?;
        warn_if_renamed(file, config, &file_name);
        let abs_file_path =
            local_name::safe_join(&config.canonical_destination_root()?, &file_name);

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

            err_if_local_file_exists(&abs_file_path, config.existing_file_action).await?;

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

    if name != local_name {
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

    use super::{
        err_if_file_exists, err_if_local_file_exists, errors, Config, Destination,
        ExistingFileAction,
    };
    use crate::common::drive_file::{MIME_TYPE_DRIVE_FOLDER, MIME_TYPE_DRIVE_SHORTCUT};

    fn config(destination: &Path, existing_file_action: ExistingFileAction) -> Config {
        Config {
            file_id: "file-id".to_string(),
            existing_file_action,
            follow_shortcuts: false,
            download_directories: true,
            destination: Destination::Path(destination.to_path_buf()),
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
}
