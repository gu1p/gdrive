pub(crate) mod errors;
#[cfg(test)]
mod media_tests;
#[cfg(test)]
pub(crate) use media_tests::assert_content;
pub(crate) mod recursive;
pub(crate) mod resume;

use std::path::{Path, PathBuf};

use async_recursion::async_recursion;
use error_trace::ErrorTrace;
use md5::Digest;
use tokio::{
    fs::{self, File},
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
};

use crate::{
    common::{
        drive_file,
        file_tree_drive::errors::FileIdentifier,
        hub_helper::get_hub,
        local_name::{self, NameChange, UnusableName},
        md5_writer::Md5Writer,
        media_client::{MediaClient, MediaError},
    },
    files,
    hub::Hub,
    transfer::scheduler::{self, Printer},
};
pub(crate) use resume::Expected;
use resume::{copy_body, BodyError, Identity, MediaSource, Next, ResumableDownload, Revision};

pub(crate) struct Config {
    pub(crate) file_id: String,
    pub(crate) existing_file_action: ExistingFileAction,
    pub(crate) follow_shortcuts: bool,
    pub(crate) download_directories: bool,
    pub(crate) destination: Destination,
    /// Download files Google flagged as malware or spam (owner or shared drive organizer only).
    pub(crate) acknowledge_abuse: bool,
    /// How many files a recursive download transfers at once, and whether it goes on after a
    /// failure.
    pub(crate) transfer: scheduler::Options,
    /// The formats a recursive download exports Google Docs files to (`--export`); `None`
    /// skips them.
    pub(crate) export: Option<files::export::ExportFormats>,
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

    let revision = Revision::of(file);
    // Escaped: the Drive name is not sanitized yet and goes to the terminal.
    let label = format!(
        "download of '{}'",
        file.name
            .as_deref()
            .unwrap_or(&config.file_id)
            .escape_debug()
    );
    let source = file_content(hub, &config.file_id, config.acknowledge_abuse, label)
        .map_err(|err| E::DownloadFile(Box::new(err)))?;
    let identity = Identity::new(hub, &config.file_id, revision);
    let mut download = ResumableDownload::new(source, hub.retrier().state(), revision.expected())
        .with_identity(identity);
    let first = download
        .next()
        .await
        .map_err(|err| E::DownloadFile(Box::new(err)))?;

    if config.destination == Destination::Stdout {
        save_to_stdout(download, first).await?;
    } else {
        let file_name = local_file_name(file, config)?;
        warn_if_renamed(file, config, &file_name);
        let abs_file_path =
            local_name::safe_join(&config.canonical_destination_root()?, &file_name);

        println!("Downloading {file_name}");
        let file_id = file.id.as_deref().unwrap_or(&config.file_id);
        if let Err(source) = save_to_file(download, first, &abs_file_path, file_id).await {
            return Err(E::SaveBodyToFile {
                path: abs_file_path,
                source,
            });
        }
        println!("Successfully downloaded {file_name}");
    }

    Ok(())
}

/// Downloads the folder `file` and everything in it: the files are downloaded while the
/// folder is still being listed, `config.transfer.jobs` at a time (see [`recursive`]).
async fn download_directory(
    hub: &Hub,
    file: google_drive3::api::File,
    config: &Config,
) -> Result<(), errors::Download> {
    let printer = Printer::stdio();
    recursive::download_directory(hub, file, config, &printer, scheduler::ctrl_c()).await
}

/// The content of a Drive file: `files.get?alt=media`, which supports `Range` requests.
/// `label` names the download in warnings.
fn file_content<'a>(
    hub: &'a Hub,
    file_id: &str,
    acknowledge_abuse: bool,
    label: String,
) -> Result<MediaSource<'a>, MediaError> {
    let client = MediaClient::new(hub);
    let acknowledge_abuse = if acknowledge_abuse { "true" } else { "false" };
    let url = client.drive_url(
        &["files", file_id],
        &[
            ("alt", "media"),
            ("supportsAllDrives", "true"),
            ("acknowledgeAbuse", acknowledge_abuse),
        ],
    )?;
    Ok(MediaSource::Drive { client, url, label })
}

/// How many temporary file names [`create_temp_file`] tries before it gives up.
const MAX_TEMP_FILE_ATTEMPTS: u32 = 100;

/// Saves a download to `file_path` through a temporary file named after the Drive file id.
/// `first` is what [`ResumableDownload::next`] returned first.
///
/// The temporary file is created next to `file_path` with `create_new`, so it is never shared
/// with another download, removed if anything fails, and renamed to `file_path` only after the
/// whole content is written and its size and md5 match. A body that breaks off is resumed (see
/// [`resume`]); when the server sends the content from the start again, it goes to a new
/// temporary file and the old one is removed.
// TODO: move to common
pub(crate) async fn save_to_file(
    download: ResumableDownload<'_>,
    first: Next,
    file_path: &Path,
    file_id: &str,
) -> Result<(), errors::SaveBodyToFile> {
    save_through(download, first, file_path, file_id, |file| file).await
}

/// [`save_to_file`] writing through `writer(temporary file)`; tests use it to inject write
/// errors, the recursive download to note which temporary files it created.
async fn save_through<W>(
    download: ResumableDownload<'_>,
    first: Next,
    file_path: &Path,
    file_id: &str,
    writer: impl FnMut(File) -> W,
) -> Result<(), errors::SaveBodyToFile>
where
    W: AsyncWrite + Unpin,
{
    save_through_with(download, first, file_path, file_id, Commit::Replace, writer).await
}

/// [`save_through`], taking the place of `file_path` as `commit` says.
async fn save_through_with<W>(
    download: ResumableDownload<'_>,
    first: Next,
    file_path: &Path,
    file_id: &str,
    commit: Commit,
    mut writer: impl FnMut(File) -> W,
) -> Result<(), errors::SaveBodyToFile>
where
    W: AsyncWrite + Unpin,
{
    let (tmp_file_path, file) = create_temp_file(file_path, file_id).await?;
    let mut part = TempFile::new(tmp_file_path, writer(file));
    part.commit = commit;
    write_temp_file(download, first, part, file_path, file_id, writer).await
}

/// How a complete download takes the place of its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Commit {
    /// Whatever is there is replaced: an overwrite, a changed file of a sync.
    Replace,
    /// Nothing that is there is replaced: a new file of a sync, whose destination was free
    /// when the download started. Something that appeared there since fails the download
    /// ([`rename_no_replace`]).
    NoReplace,
}

/// The temporary file a download is written to.
struct TempFile<W> {
    path: PathBuf,
    writer: Md5Writer<W>,
    commit: Commit,
}

impl<W: AsyncWrite + Unpin> TempFile<W> {
    fn new(path: PathBuf, writer: W) -> Self {
        TempFile {
            path,
            writer: Md5Writer::new(writer),
            commit: Commit::Replace,
        }
    }

    /// Replaces the temporary file with a new, empty one, for content that starts over.
    async fn restart(
        &mut self,
        file_path: &Path,
        file_id: &str,
        writer: &mut impl FnMut(File) -> W,
    ) -> Result<(), errors::SaveBodyToFile> {
        let (path, file) = create_temp_file(file_path, file_id).await?;
        let mut new = TempFile::new(path, writer(file));
        new.commit = self.commit;
        let old = std::mem::replace(self, new);
        let TempFile {
            path: old_path,
            writer: old_writer,
            ..
        } = old;
        drop(old_writer);
        remove_temp_file(&old_path).await;
        Ok(())
    }
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
            Err(err) => {
                // Windows reports an existing directory as a permission error rather than
                // AlreadyExists, so inspect the path on any failure.
                let existing = fs::symlink_metadata(&tmp_file_path).await;
                match existing {
                    Ok(metadata) if !metadata.is_file() => {
                        return Err(refuse(format!(
                            "refusing to use '{}' as a temporary file: it exists and is not a \
                            regular file",
                            tmp_file_path.display()
                        )));
                    }
                    // A regular file is a stale or concurrent temporary file: try the next name.
                    Ok(_) if err.kind() == io::ErrorKind::AlreadyExists => {}
                    _ => return Err(errors::SaveBodyToFile::CreateFile(err)),
                }
            }
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

/// Writes the download into `part`, flushes and closes it, checks size and md5 and renames the
/// file to `file_path`. On failure the temporary file is removed.
///
/// The flush comes before the digest is taken: `tokio::fs::File` reports the error of a write only
/// on the next write or flush, so without it a failure on the last chunk (a full disk, for
/// example) would be lost and the incomplete file renamed as if it were complete.
async fn write_temp_file<W>(
    mut download: ResumableDownload<'_>,
    first: Next,
    mut part: TempFile<W>,
    file_path: &Path,
    file_id: &str,
    mut writer: impl FnMut(File) -> W,
) -> Result<(), errors::SaveBodyToFile>
where
    W: AsyncWrite + Unpin,
{
    use errors::SaveBodyToFile as E;

    let written = async {
        receive(
            &mut download,
            first,
            &mut part,
            file_path,
            file_id,
            &mut writer,
        )
        .await?;
        part.writer.flush().await.map_err(E::Flush)
    }
    .await;

    let TempFile {
        path: tmp_file_path,
        writer: md5_writer,
        commit,
    } = part;
    let result = match written {
        Ok(()) => {
            // Taking the digest closes the file, before it is renamed
            let md5_digest = md5_writer.md5();
            check_and_rename(&tmp_file_path, file_path, &download, md5_digest, commit).await
        }
        Err(err) => Err(err),
    };

    if result.is_err() {
        remove_temp_file(&tmp_file_path).await;
    }
    result
}

/// Streams the download into `part` until it is complete, resuming broken bodies.
async fn receive<W>(
    download: &mut ResumableDownload<'_>,
    first: Next,
    part: &mut TempFile<W>,
    file_path: &Path,
    file_id: &str,
    writer: &mut impl FnMut(File) -> W,
) -> Result<(), errors::SaveBodyToFile>
where
    W: AsyncWrite + Unpin,
{
    use errors::SaveBodyToFile as E;

    let mut next = first;
    loop {
        let body = match next {
            Next::Complete => return Ok(()),
            Next::Body { body, restart } => {
                if restart {
                    part.restart(file_path, file_id, writer).await?;
                }
                body
            }
        };

        match copy_body(body, &mut part.writer, download).await {
            Ok(()) => {
                if download
                    .ended()
                    .await
                    .map_err(|err| E::Request(Box::new(err)))?
                {
                    return Ok(());
                }
            }
            Err(BodyError::Read(err)) => download.interrupted(err).await.map_err(E::ReadChunk)?,
            Err(BodyError::Write(err)) => return Err(E::WriteChunk(err)),
        }

        next = download
            .next()
            .await
            .map_err(|err| E::Request(Box::new(err)))?;
    }
}

/// Checks size and md5 against the revision that was downloaded and renames the temporary file
/// as `commit` says.
async fn check_and_rename(
    tmp_file_path: &Path,
    file_path: &Path,
    download: &ResumableDownload<'_>,
    md5_digest: Digest,
    commit: Commit,
) -> Result<(), errors::SaveBodyToFile> {
    use errors::SaveBodyToFile as E;

    let expected = download.expected();

    // The byte count, also when there is no md5 to check (exports, Workspace files)
    let received = download.received();
    if let Some(size) = expected.size.or(download.total()) {
        if size != received {
            return Err(E::SizeMismatch {
                expected: size,
                actual: received,
            });
        }
    }

    // Check md5
    if let Some(expected_md5) = expected.md5 {
        if expected_md5 != md5_digest {
            return Err(E::Md5Mismatch {
                expected: expected_md5,
                actual: md5_digest,
            });
        }
    }

    // Rename temporary file to final file
    match commit {
        Commit::Replace => fs::rename(&tmp_file_path, file_path)
            .await
            .map_err(E::RenameFile),
        Commit::NoReplace => match rename_no_replace(tmp_file_path, file_path).await {
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                Err(E::DestinationAppeared(file_path.to_path_buf()))
            }
            renamed => renamed.map_err(E::RenameFile),
        },
    }
}

/// Renames `from` to `to` unless `to` exists, which fails with `AlreadyExists` and changes
/// nothing.
///
/// `std::fs::rename` replaces `to` (`rename` on Unix, `MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING` on Windows). Here the new name is a hard link, which the system
/// refuses atomically when the name exists, and the old name is removed after it. Where hard
/// links are not supported (FAT, exFAT, some network filesystems), the fallback checks that
/// `to` does not exist and renames: a file created in the instant between the check and the
/// rename is replaced.
async fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match fs::hard_link(from, to).await {
        Ok(()) => {
            // Both names are the same file now; the content is in place either way.
            remove_temp_file(from).await;
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(err),
        Err(_) => match fs::symlink_metadata(to).await {
            Ok(_) => Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => fs::rename(from, to).await,
            Err(err) => Err(err),
        },
    }
}

/// Writes a download to stdout. A body that breaks off is resumed, but content that the server
/// sends from the start again (because the file changed, for example) cannot be taken back from
/// stdout, so that is an error. Size and md5 are checked at the end; a mismatch cannot undo the
/// output either, but makes the command fail.
// TODO: move to common
async fn save_to_stdout(
    mut download: ResumableDownload<'_>,
    first: Next,
) -> Result<(), errors::SaveBodyToStdout> {
    use errors::SaveBodyToStdout as E;

    let mut stdout = Md5Writer::new(io::stdout());
    let mut next = first;
    loop {
        let body = match next {
            Next::Complete => break,
            Next::Body { restart: true, .. } => return Err(E::CannotRestart),
            Next::Body { body, .. } => body,
        };

        match copy_body(body, &mut stdout, &mut download).await {
            Ok(()) => {
                if download
                    .ended()
                    .await
                    .map_err(|err| E::Request(Box::new(err)))?
                {
                    break;
                }
            }
            Err(BodyError::Read(err)) => download.interrupted(err).await.map_err(E::ReadChunk)?,
            Err(BodyError::Write(err)) => return Err(E::WriteChunk(err)),
        }

        next = download
            .next()
            .await
            .map_err(|err| E::Request(Box::new(err)))?;
    }

    // Like a file, stdout reports the error of the last write only on flush
    stdout.flush().await.map_err(E::WriteChunk)?;

    let expected = download.expected();
    if let Some(size) = expected.size.or(download.total()) {
        if size != download.received() {
            return Err(E::SizeMismatch {
                expected: size,
                actual: download.received(),
            });
        }
    }

    let md5_digest = stdout.md5();
    if let Some(expected_md5) = expected.md5 {
        if expected_md5 != md5_digest {
            return Err(E::Md5Mismatch {
                expected: expected_md5,
                actual: md5_digest,
            });
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

/// Whether the local file at `path` has the md5 of the Drive file, `md5`.
async fn local_file_is_identical(path: &Path, md5: Option<&Digest>) -> bool {
    if path.exists() {
        match compute_md5_from_path(path).await {
            Ok(file_md5) => md5.is_some_and(|md5| md5 == &file_md5),
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
        path::Path,
        pin::Pin,
        task::{Context, Poll},
    };

    use bytes::Bytes;
    use google_apis_common::Body;
    use http_body_util::{BodyExt, Full};
    use md5::Digest;
    use tokio::{fs::File, io::AsyncWrite};

    use super::{
        create_temp_file, err_if_file_exists, err_if_local_file_exists, errors, save_through,
        write_temp_file, Config, Destination, ExistingFileAction, Expected, MediaSource,
        ResumableDownload, TempFile,
    };
    use crate::common::{
        drive_file::{MIME_TYPE_DRIVE_FOLDER, MIME_TYPE_DRIVE_SHORTCUT},
        local_name,
        retry::{RetryPolicy, RetryState},
    };

    /// A download of `body` that cannot be resumed, checked against `md5`.
    fn one_shot(body: Body, md5: Option<Digest>) -> ResumableDownload<'static> {
        ResumableDownload::new(
            MediaSource::Body(Some(body)),
            RetryState::new(RetryPolicy::no_retries(), None),
            Expected { md5, size: None },
        )
    }

    /// Saves `body` like a download that cannot be resumed, through `writer(temporary file)`.
    async fn save_body_through<W>(
        body: Body,
        file_path: &Path,
        file_id: &str,
        expected_md5: Option<&Digest>,
        writer: impl FnMut(File) -> W,
    ) -> Result<(), errors::SaveBodyToFile>
    where
        W: AsyncWrite + Unpin,
    {
        let mut download = one_shot(body, expected_md5.copied());
        let first = download
            .next()
            .await
            .map_err(|err| errors::SaveBodyToFile::Request(Box::new(err)))?;
        save_through(download, first, file_path, file_id, writer).await
    }

    async fn save_body_to_file(
        body: Body,
        file_path: &Path,
        file_id: &str,
        expected_md5: Option<&Digest>,
    ) -> Result<(), errors::SaveBodyToFile> {
        save_body_through(body, file_path, file_id, expected_md5, |file| file).await
    }

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
            transfer: crate::transfer::scheduler::Options::default(),
            export: None,
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
    async fn a_no_replace_rename_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join(".gdrive-x.part"), dir.path().join("x"));
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"theirs").unwrap();

        let refused = super::rename_no_replace(&from, &to).await.unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&to).unwrap(), b"theirs");
        assert_eq!(std::fs::read(&from).unwrap(), b"new");

        std::fs::remove_file(&to).unwrap();
        super::rename_no_replace(&from, &to).await.unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        assert_eq!(dir_entries(dir.path()), ["x"]);
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
        let mut download = one_shot(body(b"B bytes"), Some(b_md5));
        let first = download.next().await.unwrap();
        let part = TempFile::new(b_tmp.clone(), b_file);
        write_temp_file(download, first, part, &b_target, "idB", |file| file)
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

    /// Accepts every write, as `tokio::fs::File` does while the write runs in the background, and
    /// reports the failure of that write on flush.
    struct FailsOnFlush;

    impl tokio::io::AsyncWrite for FailsOnFlush {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other("no space left on device")))
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    #[tokio::test]
    async fn write_error_reported_on_flush_fails_and_removes_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.bin");
        std::fs::write(&target, b"old").unwrap();

        let result =
            save_body_through(body(b"new"), &target, "id-a", None, |_file| FailsOnFlush).await;

        assert!(
            matches!(result, Err(errors::SaveBodyToFile::Flush(_))),
            "{result:?}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(dir_entries(dir.path()), ["a.bin"]);
    }

    /// The same with a real `tokio::fs::File`: every write to /dev/full fails with ENOSPC, but
    /// tokio only reports it on the next write or flush.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn full_disk_on_the_last_chunk_fails_and_removes_temp_file() {
        let Ok(full) = tokio::fs::File::options()
            .write(true)
            .open("/dev/full")
            .await
        else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.bin");

        let mut full = Some(full);
        let result = save_body_through(body(b"new"), &target, "id-a", None, |_file| {
            full.take().expect("a single temporary file")
        })
        .await;

        assert!(
            matches!(result, Err(errors::SaveBodyToFile::Flush(_))),
            "{result:?}"
        );
        assert_eq!(dir_entries(dir.path()), Vec::<String>::new());
    }
}
