use std::{
    error::Error,
    fmt::{self, Display},
    num::TryFromIntError,
    path::PathBuf,
};

use crate::common::local_name::{self, UnusableName};

#[derive(Debug)]
pub(crate) struct FileTreeDrive(pub(crate) Folder);

impl Display for FileTreeDrive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unable to create a folder from file")
    }
}

impl Error for FileTreeDrive {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// A child of a folder that cannot be downloaded, with the path of the folder below the
/// destination.
#[derive(Debug)]
pub(crate) struct InFolder {
    pub(crate) folder: PathBuf,
    pub(crate) source: Folder,
}

impl Display for InFolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "in folder '{}'", local_name::tree_path(&self.folder))
    }
}

impl Error for InFolder {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum FileIdentifier {
    Name(String),
    Id(String),
    None,
}

impl FileIdentifier {
    #[must_use]
    pub(crate) fn new(name: Option<String>, id: Option<String>) -> Self {
        if let Some(name) = name {
            Self::Name(name)
        } else if let Some(id) = id {
            Self::Id(id)
        } else {
            Self::None
        }
    }

    #[must_use]
    pub(crate) fn display(&self) -> FileIdentifierDisplay<'_> {
        FileIdentifierDisplay(self)
    }
}

impl From<google_drive3::api::File> for FileIdentifier {
    fn from(value: google_drive3::api::File) -> Self {
        Self::new(value.name, value.id)
    }
}

impl From<&google_drive3::api::File> for FileIdentifier {
    fn from(value: &google_drive3::api::File) -> Self {
        if let Some(name) = &value.name {
            Self::Name(name.clone())
        } else if let Some(id) = &value.id {
            Self::Id(id.clone())
        } else {
            Self::None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileIdentifierDisplay<'a>(&'a FileIdentifier);

impl Display for FileIdentifierDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            FileIdentifier::Name(name) => write!(f, " with name '{name}'"),
            FileIdentifier::Id(id) => write!(f, " with id '{id}'"),
            FileIdentifier::None => Ok(()),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Folder {
    MissingFileName,
    NotDirectory,
    MissingFileId,
    File {
        identifier: FileIdentifier,
        source: File,
    },
    UnusableName(UnusableName),
    /// The folder contains one of its ancestors (only a broken listing can say so).
    Cycle(FileIdentifier),
}

impl Display for Folder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Folder::MissingFileName => f.write_str("file name is missing"),
            Folder::NotDirectory => f.write_str("file is not a directory"),
            Folder::MissingFileId => f.write_str("file id is missing"),
            Folder::File {
                identifier,
                source: _,
            } => {
                write!(f, "unable to process file{}", identifier.display())
            }
            Folder::UnusableName(_) => f.write_str("unable to map a drive name to a local name"),
            Folder::Cycle(identifier) => {
                write!(f, "folder{} is its own ancestor", identifier.display())
            }
        }
    }
}

impl Error for Folder {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Folder::MissingFileName
            | Folder::NotDirectory
            | Folder::MissingFileId
            | Folder::Cycle(_) => None,
            Folder::File { source, .. } => Some(source),
            Folder::UnusableName(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub(crate) enum File {
    MissingFileName,
    MissingFileSize,
    InvalidFileSize(TryFromIntError),
    MissingFileId,
}

impl Display for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            File::MissingFileName => "file name is missing",
            File::MissingFileSize => "file size is missing",
            File::InvalidFileSize(_) => "file size is invalid",
            File::MissingFileId => "file id is missing",
        };

        f.write_str(s)
    }
}

impl Error for File {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            File::MissingFileName | File::MissingFileSize | File::MissingFileId => None,
            File::InvalidFileSize(source) => Some(source),
        }
    }
}
