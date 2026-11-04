use std::{error::Error, fmt::Display, io, path::PathBuf};

use md5::Digest;

use crate::{
    common::{
        file_tree_drive::{self, errors::FileIdentifier},
        hub_helper::GetHubError,
        local_name::{self, UnusableName},
        media_client::MediaError,
    },
    files::export::ExportTooLarge,
    transfer::scheduler::{Incomplete, OutputError},
};

#[derive(Debug)]
pub(crate) enum Download {
    Hub(GetHubError),
    GetFile(Box<google_drive3::Error>),
    #[expect(
        clippy::enum_variant_names,
        reason = "this is the actual download operation"
    )]
    DownloadFile(Box<MediaError>),
    /// The export of a Workspace document failed.
    ExportFile(Box<MediaError>),
    /// Drive refused an export over its size limit.
    ExportTooLarge(ExportTooLarge),
    MissingFileName(FileIdentifier),
    UnusableName(UnusableName),
    FileExists(FileIdentifier),
    LocalFileExists(PathBuf),
    IsDirectory(FileIdentifier),
    CreateDirectory(PathBuf, io::Error),
    CreateFileTree(file_tree_drive::errors::FileTreeDrive),
    DestinationPathDoesNotExist(PathBuf),
    DestinationPathNotADirectory(PathBuf),
    CanonicalizeDestinationPath(PathBuf, io::Error),
    MissingShortcutTarget(FileIdentifier),
    IsShortcut(FileIdentifier),
    StdoutNotValidDestination,
    SaveBodyToStdout(SaveBodyToStdout),
    SaveBodyToFile {
        path: PathBuf,
        source: SaveBodyToFile,
    },
    /// A file of a recursive download failed; `path` is relative to the destination.
    RecursiveFile {
        path: PathBuf,
        source: Box<Download>,
    },
    /// A Workspace document of a recursive download could not be exported; `path` is the local
    /// path of the export, relative to the destination.
    RecursiveExport {
        path: PathBuf,
        source: Box<Download>,
    },
    /// Files or folders of a recursive download failed, or it was interrupted.
    Incomplete(Incomplete),
    /// Drive reported `incompleteSearch` while listing a recursive download.
    IncompleteListing,
    /// stdout or stderr of a recursive download could not be written.
    WriteOutput(OutputError),
}

impl Display for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Download::Hub(_) => f.write_str("unable to get drive hub"),
            Download::GetFile(_) => f.write_str("unable to get file info"),
            Download::DownloadFile(_) => f.write_str("unable to download file from drive"),
            Download::ExportFile(_) => f.write_str("unable to export the file from drive"),
            // The message of the source, which the error message then leaves out.
            Download::ExportTooLarge(source) => source.fmt(f),
            Download::MissingFileName(identifier) => {
                write!(f, "file{} does not have a name", identifier.display())
            }
            Download::UnusableName(_) => {
                f.write_str("unable to map the drive name to a local file name")
            }
            Download::FileExists(identifier) => write!(
                f,
                "file{} already exists, use --overwrite to overwrite it",
                identifier.display()
            ),
            Download::LocalFileExists(path) => write!(
                f,
                "file '{}' already exists, use --overwrite to overwrite it",
                path.display()
            ),
            Download::IsDirectory(identifier) => write!(
                f,
                "file{} is a directory, use --recursive to download directories",
                identifier.display()
            ),
            Download::CreateDirectory(path, _) => {
                write!(f, "unable to create directory '{}'", path.display())
            }
            Download::CreateFileTree(_) => f.write_str("unable to create file tree"),
            Download::DestinationPathDoesNotExist(path) => {
                write!(f, "destination path '{}' does not exist", path.display())
            }
            Download::DestinationPathNotADirectory(path) => write!(
                f,
                "destination path '{}' is not a directory",
                path.display()
            ),
            Download::CanonicalizeDestinationPath(path, _) => write!(
                f,
                "unable to canoicalize destination path '{}'",
                path.display()
            ),
            Download::MissingShortcutTarget(identifier) => {
                write!(f, "shortcut{} does not have a target", identifier.display())
            }
            Download::IsShortcut(identifier) => write!(
                f,
                "file{} is a shortcut, use --follow-shortcuts to download the file it points to",
                identifier.display()
            ),
            Download::StdoutNotValidDestination => {
                f.write_str("stdout is not a valid destination for this combination of options")
            }
            Download::SaveBodyToStdout(_) => f.write_str("unabl to save body to stdout"),
            Download::SaveBodyToFile { path, source: _ } => {
                write!(f, "unable to save body to file '{}'", path.display())
            }
            Download::RecursiveFile { path, source: _ } => {
                write!(f, "unable to download '{}'", local_name::tree_path(path))
            }
            Download::RecursiveExport { path, source: _ } => {
                write!(f, "unable to export '{}'", path.display())
            }
            Download::Incomplete(_) => f.write_str("the recursive download is incomplete"),
            Download::WriteOutput(_) => {
                f.write_str("unable to write the output of the recursive download")
            }
            Download::IncompleteListing => f.write_str(
                "Drive did not search everything while listing the folders, some files may be \
                missing; use --continue-on-error to accept an incomplete listing",
            ),
        }
    }
}

impl Error for Download {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Download::Hub(get_hub_error) => Some(get_hub_error),
            Download::GetFile(error) => Some(error),
            Download::DownloadFile(error) | Download::ExportFile(error) => Some(error),
            Download::ExportTooLarge(source) => Some(source),
            Download::CreateFileTree(error) => Some(error),
            Download::UnusableName(source) => Some(source),
            Download::MissingFileName(_)
            | Download::FileExists(_)
            | Download::LocalFileExists(_)
            | Download::IsDirectory(_)
            | Download::DestinationPathDoesNotExist(_)
            | Download::DestinationPathNotADirectory(_)
            | Download::MissingShortcutTarget(_)
            | Download::IsShortcut(_)
            | Download::StdoutNotValidDestination
            | Download::IncompleteListing => None,
            Download::CreateDirectory(_, source)
            | Download::CanonicalizeDestinationPath(_, source) => Some(source),
            Download::SaveBodyToStdout(save_body_to_stdout) => Some(save_body_to_stdout),
            Download::SaveBodyToFile { source, .. } => Some(source),
            Download::RecursiveFile { source, .. } | Download::RecursiveExport { source, .. } => {
                Some(source)
            }
            Download::Incomplete(source) => Some(source),
            Download::WriteOutput(source) => Some(source),
        }
    }
}

impl From<SaveBodyToStdout> for Download {
    fn from(value: SaveBodyToStdout) -> Self {
        Download::SaveBodyToStdout(value)
    }
}

#[derive(Debug)]
pub(crate) enum SaveBodyToStdout {
    ReadChunk(hyper::Error),
    WriteChunk(io::Error),
    /// The request to resume a broken download failed.
    Request(Box<MediaError>),
    /// The server sent the content from the start again, which stdout cannot take back.
    CannotRestart,
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    Md5Mismatch {
        expected: Digest,
        actual: Digest,
    },
}

impl Display for SaveBodyToStdout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let operation = match self {
            SaveBodyToStdout::ReadChunk(_) => "read",
            SaveBodyToStdout::WriteChunk(_) => "write",
            SaveBodyToStdout::Request(_) => return f.write_str("unable to resume the download"),
            SaveBodyToStdout::CannotRestart => {
                return f.write_str(
                    "the download broke off and must start over (the file changed on Drive or \
                     the server sent it from the start again), which cannot be undone on stdout; \
                     download to a file instead",
                )
            }
            SaveBodyToStdout::SizeMismatch { expected, actual } => {
                return write!(
                    f,
                    "size mismatches (expected {expected} bytes, received {actual})"
                )
            }
            SaveBodyToStdout::Md5Mismatch { expected, actual } => {
                return write!(
                    f,
                    "md5 mismatches (expected {expected:x}, actual is {actual:x})"
                )
            }
        };

        write!(f, "unable to {operation} chunk of bytes")
    }
}

impl Error for SaveBodyToStdout {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SaveBodyToStdout::ReadChunk(source) => Some(source),
            SaveBodyToStdout::WriteChunk(source) => Some(source),
            SaveBodyToStdout::Request(source) => Some(source),
            SaveBodyToStdout::CannotRestart
            | SaveBodyToStdout::SizeMismatch { .. }
            | SaveBodyToStdout::Md5Mismatch { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum SaveBodyToFile {
    CreateFile(io::Error),
    ReadChunk(hyper::Error),
    WriteChunk(io::Error),
    Flush(io::Error),
    /// The request to resume a broken download failed.
    Request(Box<MediaError>),
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    Md5Mismatch {
        expected: Digest,
        actual: Digest,
    },
    RenameFile(io::Error),
}

impl Display for SaveBodyToFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveBodyToFile::CreateFile(_) => f.write_str("unable to create file"),
            SaveBodyToFile::ReadChunk(_) => f.write_str("unable to read chunk of bytes"),
            SaveBodyToFile::WriteChunk(_) => f.write_str("unable to write chunk of bytes"),
            SaveBodyToFile::Flush(_) => f.write_str("unable to finish writing the file"),
            SaveBodyToFile::Request(_) => f.write_str("unable to resume the download"),
            SaveBodyToFile::SizeMismatch { expected, actual } => write!(
                f,
                "size mismatches (expected {expected} bytes, received {actual})"
            ),
            SaveBodyToFile::Md5Mismatch { expected, actual } => {
                write!(
                    f,
                    "md5 mismatches (expected {expected:x}, actual is {actual:x})"
                )
            }
            SaveBodyToFile::RenameFile(_) => f.write_str("unable to rename file"),
        }
    }
}

impl Error for SaveBodyToFile {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SaveBodyToFile::ReadChunk(source) => Some(source),
            SaveBodyToFile::Request(source) => Some(source),
            SaveBodyToFile::Md5Mismatch { .. } | SaveBodyToFile::SizeMismatch { .. } => None,
            SaveBodyToFile::CreateFile(source)
            | SaveBodyToFile::WriteChunk(source)
            | SaveBodyToFile::Flush(source)
            | SaveBodyToFile::RenameFile(source) => Some(source),
        }
    }
}
