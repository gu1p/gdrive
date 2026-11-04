//! The errors of `files sync`.

use std::{
    error,
    fmt::{self, Display, Formatter},
    io,
    path::PathBuf,
};

use crate::{
    common::{file_tree, hub_helper::GetHubError, id_gen},
    files,
    transfer::scheduler::{plural, OutputError},
};

/// Why a sync failed or did not do everything it planned.
#[derive(Debug)]
pub(crate) enum Sync {
    Hub(GetHubError),
    GetFolder(Box<google_drive3::Error>),
    /// The id of `sync` is not a folder.
    NotAFolder(String),
    FolderTrashed(String),
    /// `sync up` from a path that does not exist or is not a directory.
    LocalDirMissing(PathBuf),
    LocalNotADirectory(PathBuf),
    CreateLocalDir(PathBuf, io::Error),
    CanonicalizeLocalDir(PathBuf, io::Error),
    LocalTree(file_tree::errors::FileTree),
    /// The local walk task ended without a result (a panic, which is printed already).
    LocalWalk,
    GenerateIds(id_gen::NextError),
    /// Ctrl-C before the transfers started: nothing was changed.
    Interrupted,
    WriteOutput(OutputError),
    /// Some of the plan was not done; the counts are on stderr.
    Incomplete(Incomplete),
}

/// What a sync did not do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about one run, each a part of the message"
)]
pub(crate) struct Incomplete {
    pub(crate) failed: u64,
    pub(crate) cancelled: u64,
    pub(crate) interrupted: bool,
    /// Local entries that could not be read, folders that could not be listed, an incomplete
    /// Drive search: the inventory problems.
    pub(crate) inventory_problems: u64,
    /// `--delete` was given and nothing was deleted.
    pub(crate) deletion_skipped: bool,
    pub(crate) stopped: bool,
    pub(crate) output_failed: bool,
}

impl Display for Incomplete {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.interrupted {
            parts.push(String::from("interrupted"));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", plural(self.failed, "item", "items")));
        }
        if self.cancelled > 0 {
            parts.push(format!("{} cancelled", self.cancelled));
        }
        if self.inventory_problems > 0 {
            parts.push(format!(
                "the inventory is incomplete ({})",
                plural(self.inventory_problems, "problem", "problems")
            ));
        }
        if self.output_failed && parts.is_empty() {
            parts.push(String::from("the output could not be written"));
        }
        if self.deletion_skipped {
            parts.push(String::from("nothing was deleted"));
        }
        f.write_str(&parts.join(", "))?;
        if self.stopped && !self.interrupted {
            f.write_str("; use --continue-on-error to finish the other transfers after a failure")?;
        }
        Ok(())
    }
}

impl error::Error for Incomplete {}

impl Display for Sync {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Sync::Hub(_) => f.write_str("unable to get drive hub"),
            Sync::GetFolder(_) => f.write_str("unable to get the Drive folder"),
            Sync::NotAFolder(id) => write!(
                f,
                "file '{id}' is not a folder; `files sync` syncs a folder, use `files upload` or \
                `files download` for a single file"
            ),
            Sync::FolderTrashed(id) => write!(
                f,
                "folder '{id}' is in the trash, restore it with `gdrive files untrash {id}`"
            ),
            Sync::LocalDirMissing(path) => {
                write!(f, "local directory '{}' does not exist", path.display())
            }
            Sync::LocalNotADirectory(path) => {
                write!(f, "'{}' is not a directory", path.display())
            }
            Sync::CreateLocalDir(path, _) => {
                write!(f, "unable to create directory '{}'", path.display())
            }
            Sync::CanonicalizeLocalDir(path, _) => {
                write!(f, "unable to resolve directory '{}'", path.display())
            }
            Sync::LocalTree(_) => f.write_str("unable to read the local directory"),
            Sync::LocalWalk => f.write_str("the walk of the local directory failed"),
            Sync::GenerateIds(_) => f.write_str("unable to get ids for the new files"),
            Sync::Interrupted => f.write_str("interrupted before anything was changed"),
            Sync::WriteOutput(_) => f.write_str("unable to write the output of the sync"),
            Sync::Incomplete(_) => f.write_str("the sync is incomplete"),
        }
    }
}

impl error::Error for Sync {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Sync::Hub(source) => Some(source),
            Sync::GetFolder(source) => Some(source.as_ref()),
            Sync::CreateLocalDir(_, source) | Sync::CanonicalizeLocalDir(_, source) => Some(source),
            Sync::LocalTree(source) => Some(source),
            Sync::GenerateIds(source) => Some(source),
            Sync::WriteOutput(source) => Some(source),
            Sync::Incomplete(source) => Some(source),
            Sync::NotAFolder(_)
            | Sync::FolderTrashed(_)
            | Sync::LocalDirMissing(_)
            | Sync::LocalNotADirectory(_)
            | Sync::LocalWalk
            | Sync::Interrupted => None,
        }
    }
}

/// A local entry that the inventory could not read.
#[derive(Debug)]
pub(crate) struct LocalEntry {
    pub(crate) path: PathBuf,
    pub(crate) source: file_tree::errors::Folder,
}

impl Display for LocalEntry {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unable to read '{}' from the local disk, it is not synced",
            self.path.display()
        )
    }
}

impl error::Error for LocalEntry {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.source)
    }
}

/// One item of a sync that failed: its path below the synced folders, and why.
#[derive(Debug)]
pub(crate) struct Item {
    pub(crate) label: String,
    pub(crate) source: ItemError,
}

impl Display for Item {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "unable to sync '{}'", self.label)
    }
}

impl error::Error for Item {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
pub(crate) enum ItemError {
    Upload(Box<files::upload::Error>),
    CreateDir(PathBuf, io::Error),
    Hash(PathBuf, io::Error),
    Trash(Box<google_drive3::Error>),
    /// The local entry is not what the inventory found any more.
    Changed(PathBuf),
    /// The local trash `.gdrive-trash` is not a plain directory.
    TrashNotADirectory(PathBuf),
    MoveToTrash {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },
    TrashTaken(PathBuf),
    Remove(PathBuf, io::Error),
}

impl Display for ItemError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ItemError::Upload(_) => f.write_str("the upload failed"),
            ItemError::CreateDir(path, _) => {
                write!(f, "unable to create directory '{}'", path.display())
            }
            ItemError::Hash(path, _) => write!(f, "unable to read '{}'", path.display()),
            ItemError::Trash(_) => f.write_str("unable to move it to the Drive trash"),
            ItemError::Changed(path) => write!(
                f,
                "'{}' changed since the inventory, it is left alone; run the sync again",
                path.display()
            ),
            ItemError::TrashNotADirectory(path) => write!(
                f,
                "'{}' is not a directory, so nothing can be moved into it; move it away",
                path.display()
            ),
            ItemError::MoveToTrash { from, to, .. } => write!(
                f,
                "unable to move '{}' to '{}'",
                from.display(),
                to.display()
            ),
            ItemError::TrashTaken(path) => write!(
                f,
                "'{}' exists already in the local trash, it is left alone",
                path.display()
            ),
            ItemError::Remove(path, _) => write!(f, "unable to remove '{}'", path.display()),
        }
    }
}

impl error::Error for ItemError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            ItemError::Upload(source) => Some(source.as_ref()),
            ItemError::CreateDir(_, source)
            | ItemError::Hash(_, source)
            | ItemError::MoveToTrash { source, .. }
            | ItemError::Remove(_, source) => Some(source),
            ItemError::Trash(source) => Some(source.as_ref()),
            ItemError::Changed(_) | ItemError::TrashNotADirectory(_) | ItemError::TrashTaken(_) => {
                None
            }
        }
    }
}
