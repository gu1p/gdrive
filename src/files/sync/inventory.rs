//! Phase 1 of a sync, the inventory: both trees, complete, before anything is compared.
//!
//! The Drive folder is listed by the walk ([`crate::transfer::walk`]) and kept whole as a
//! [`RemoteFolder`] tree; every [`FolderError`] and `incompleteSearch` is recorded on the folder
//! it concerns and as an inventory problem, which keeps the deletion phase from running. The
//! local directory is walked by [`FileTree`] (symlinks followed, cycles left out, unreadable
//! entries reported) on the blocking pool, without asking Drive for ids, and kept as a
//! [`LocalNode`] tree that also says which entries are symlinks.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use google_drive3::api::File;

use super::errors;
use crate::{
    common::{file_tree::FileTree, file_tree_like::Node, id_gen::IdGen, parse_md5_digest},
    hub::Hub,
    transfer::{
        scheduler::Printer,
        walk::{
            Children, EntryKind, FolderError, FolderKey, Walk, WalkItem, WalkOptions, WalkRoot,
        },
    },
};

/// The directory below the local root where `sync down --delete` moves what it removes. It is
/// never synced in either direction.
pub(crate) const LOCAL_TRASH: &str = ".gdrive-trash";

/// Whether a folder's listing is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Listing {
    Complete,
    /// Drive reported `incompleteSearch` for it: some children may be missing.
    Incomplete,
    /// The folder could not be listed (its children so far are kept, but not trusted).
    Failed,
}

/// A listed Drive folder.
#[derive(Debug, Clone)]
pub(crate) struct RemoteFolder {
    pub(crate) id: String,
    pub(crate) children: Vec<RemoteNode>,
    pub(crate) listing: Listing,
}

/// One Drive file of the inventory.
#[derive(Debug, Clone)]
pub(crate) struct RemoteNode {
    pub(crate) name: Option<String>,
    pub(crate) id: Option<String>,
    pub(crate) kind: RemoteKind,
}

#[derive(Debug, Clone)]
pub(crate) enum RemoteKind {
    /// `None`: not listed, a folder that is its own ancestor or has no id.
    Folder(Option<RemoteFolder>),
    Blob {
        size: Option<u64>,
        md5: Option<md5::Digest>,
        modified: Option<DateTime<Utc>>,
    },
    /// A Google Workspace document.
    Native {
        mime: Option<String>,
        modified: Option<DateTime<Utc>>,
    },
    Shortcut,
}

/// One entry of the local directory.
#[derive(Debug, Clone)]
pub(crate) struct LocalNode {
    /// The file name; not valid UTF-8 names are converted lossily, see `utf8`.
    pub(crate) name: String,
    pub(crate) utf8: bool,
    /// Below the canonical local root, as walked (not canonicalized).
    pub(crate) path: PathBuf,
    /// The entry itself is a symlink (followed by the walk).
    pub(crate) symlink: bool,
    pub(crate) kind: LocalKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LocalKind {
    Dir(Vec<LocalNode>),
    File {
        size: u64,
        modified: Option<SystemTime>,
    },
    /// The walk could not read it: unknown content.
    Unreadable {
        is_dir: bool,
    },
    /// A directory left out as a symlink cycle.
    Cycle,
}

/// The local side of the inventory.
#[derive(Debug, Clone)]
pub(crate) struct LocalTree {
    /// The canonical local root, or the path given when it does not exist yet.
    pub(crate) root: PathBuf,
    /// `None` when the local root does not exist (a first `sync down`).
    pub(crate) children: Option<Vec<LocalNode>>,
}

/// Both trees and what keeps the inventory from being complete.
#[derive(Debug, Clone)]
pub(crate) struct Inventory {
    pub(crate) remote: RemoteFolder,
    pub(crate) local: LocalTree,
    /// Lines for stderr, one per problem: unlisted folders, incomplete searches, unreadable
    /// local entries. Any problem suppresses deletion and fails the sync.
    pub(crate) problems: Vec<String>,
}

/// Lists the Drive folder `root` completely, `listing_jobs` requests at once. Folder errors and
/// incomplete searches are printed as they come and recorded as problems.
pub(crate) async fn remote(
    hub: &Hub,
    root: File,
    listing_jobs: usize,
    printer: &Printer,
    problems: &mut Vec<String>,
) -> RemoteFolder {
    let root_id = root.id.clone().unwrap_or_default();
    let walk_root = WalkRoot {
        id: root_id.clone(),
        name: root.name.clone().unwrap_or_default(),
        drive_id: root.drive_id.clone(),
        resource_key: root.resource_key.clone(),
    };
    let options = WalkOptions {
        listing_jobs,
        ..WalkOptions::default()
    };
    let mut walk = Walk::start(Arc::new(hub.clone()), walk_root, options);

    let mut folders = HashMap::from([(FolderKey::ROOT, Building::new(root_id))]);
    while let Some(item) = walk.next().await {
        match item {
            WalkItem::Entry(entry) => {
                let listed = match entry.children {
                    Children::Listed(key) => {
                        let id = entry.file.id.clone().unwrap_or_default();
                        folders.insert(key, Building::new(id));
                        Some(key)
                    }
                    Children::None | Children::Cycle => None,
                };
                let node = Pending::of(entry.kind, &entry.file, listed);
                folders.entry(entry.parent).or_default().children.push(node);
            }
            WalkItem::FolderDone(key) => folders.entry(key).or_default().done = true,
            WalkItem::FolderError(error) => {
                folders.entry(error.folder.key).or_default().listing = Listing::Failed;
                folder_error(&error, printer, problems).await;
            }
            WalkItem::IncompleteSearch(incomplete) => {
                for folder in &incomplete.folders {
                    let state = folders.entry(folder.key).or_default();
                    if state.listing == Listing::Complete {
                        state.listing = Listing::Incomplete;
                    }
                    let line = format!(
                        "Warning: Drive did not search everything while listing '{}', some files \
                        may be missing",
                        folder.display_path()
                    );
                    printer.err(line.clone()).await;
                    problems.push(line);
                }
            }
        }
    }
    assemble(FolderKey::ROOT, &mut folders)
}

async fn folder_error(error: &FolderError, printer: &Printer, problems: &mut Vec<String>) {
    let message = crate::failure_message(error);
    printer.err(message.clone()).await;
    problems.push(message);
}

/// A folder while the walk runs.
#[derive(Debug)]
struct Building {
    id: String,
    children: Vec<Pending>,
    done: bool,
    listing: Listing,
}

impl Default for Building {
    fn default() -> Self {
        Building::new(String::new())
    }
}

impl Building {
    fn new(id: String) -> Self {
        Building {
            id,
            children: Vec::new(),
            done: false,
            listing: Listing::Complete,
        }
    }
}

/// A child whose folder, if it is one, is assembled after the walk.
#[derive(Debug)]
struct Pending {
    name: Option<String>,
    id: Option<String>,
    kind: PendingKind,
}

#[derive(Debug)]
enum PendingKind {
    Folder(Option<FolderKey>),
    Other(RemoteKind),
}

impl Pending {
    fn of(kind: EntryKind, file: &File, listed: Option<FolderKey>) -> Self {
        let kind = match kind {
            EntryKind::Folder => PendingKind::Folder(listed),
            EntryKind::Blob => PendingKind::Other(RemoteKind::Blob {
                size: file.size.and_then(|size| u64::try_from(size).ok()),
                md5: file.md5_checksum.as_deref().and_then(parse_md5_digest),
                modified: file.modified_time,
            }),
            EntryKind::Native => PendingKind::Other(RemoteKind::Native {
                mime: file.mime_type.clone(),
                modified: file.modified_time,
            }),
            EntryKind::Shortcut => PendingKind::Other(RemoteKind::Shortcut),
        };
        Pending {
            name: file.name.clone(),
            id: file.id.clone(),
            kind,
        }
    }
}

/// The tree below `key`, from the folders of the walk. A folder the walk did not finish counts
/// as failed.
fn assemble(key: FolderKey, folders: &mut HashMap<FolderKey, Building>) -> RemoteFolder {
    let building = folders.remove(&key).unwrap_or_default();
    let listing = if building.done || building.listing == Listing::Failed {
        building.listing
    } else {
        Listing::Failed
    };
    let children = building
        .children
        .into_iter()
        .map(|child| {
            let kind = match child.kind {
                PendingKind::Folder(listed) => {
                    RemoteKind::Folder(listed.map(|key| assemble(key, folders)))
                }
                PendingKind::Other(kind) => kind,
            };
            RemoteNode {
                name: child.name,
                id: child.id,
                kind,
            }
        })
        .collect();
    RemoteFolder {
        id: building.id,
        children,
        listing,
    }
}

/// Walks the local directory `root` (canonical) on the blocking pool, leaving out
/// [`LOCAL_TRASH`]. Unreadable entries are printed and recorded as problems.
pub(crate) async fn local(
    root: PathBuf,
    printer: &Printer,
    problems: &mut Vec<String>,
) -> Result<Vec<LocalNode>, errors::Sync> {
    let walked = tokio::task::spawn_blocking(move || walk_local(&root))
        .await
        .map_err(|_| errors::Sync::LocalWalk)??;
    for line in walked.problems {
        printer.err(line.clone()).await;
        problems.push(line);
    }
    Ok(walked.children)
}

struct Walked {
    children: Vec<LocalNode>,
    problems: Vec<String>,
}

fn walk_local(root: &Path) -> Result<Walked, errors::Sync> {
    // The ids are never sent to Drive: a sync creates under ids it gets when it transfers.
    let tree = futures::executor::block_on(FileTree::from_path_except(
        root,
        Some(LOCAL_TRASH),
        &mut IdGen::sequential(),
    ))
    .map_err(errors::Sync::LocalTree)?;
    let canonical_root = tree.root.info.path.clone();
    let mut children = convert_children(tree.root.children);

    let mut problems = Vec::new();
    // Relative to the parent of the root, as file_tree reports them.
    let base = canonical_root.parent().unwrap_or(&canonical_root);
    for entry in tree.unreadable {
        let path = if entry.relative_path.is_absolute() {
            entry.relative_path.clone()
        } else {
            base.join(&entry.relative_path)
        };
        problems.push(crate::failure_message(&errors::LocalEntry {
            path: path.clone(),
            source: entry.error,
        }));
        let node = node(
            path,
            false,
            LocalKind::Unreadable {
                is_dir: entry.is_dir,
            },
        );
        insert(&mut children, &canonical_root, node);
    }
    for path in tree.cycles {
        let node = node(path, true, LocalKind::Cycle);
        insert(&mut children, &canonical_root, node);
    }
    Ok(Walked { children, problems })
}

fn convert_children(children: Vec<Node<crate::common::file_tree::Folder>>) -> Vec<LocalNode> {
    children
        .into_iter()
        .map(|child| match child {
            Node::File(file) => {
                let modified = std::fs::metadata(&file.path)
                    .and_then(|metadata| metadata.modified())
                    .ok();
                let symlink = is_symlink(&file.path);
                node(
                    file.path,
                    symlink,
                    LocalKind::File {
                        size: file.size,
                        modified,
                    },
                )
            }
            Node::Folder(folder) => {
                let path = folder.info.path.clone();
                let symlink = is_symlink(&path);
                node(
                    path,
                    symlink,
                    LocalKind::Dir(convert_children(folder.children)),
                )
            }
        })
        .collect()
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn node(path: PathBuf, symlink: bool, kind: LocalKind) -> LocalNode {
    let file_name = path.file_name().unwrap_or_default();
    LocalNode {
        name: file_name.to_string_lossy().into_owned(),
        utf8: file_name.to_str().is_some(),
        path,
        symlink,
        kind,
    }
}

/// Puts `node` into the directory of `children` (at `dir`) or the one below it that contains it.
fn insert(children: &mut Vec<LocalNode>, dir: &Path, node: LocalNode) {
    if node.path.parent() != Some(dir) {
        let below = children.iter_mut().find(|child| {
            matches!(child.kind, LocalKind::Dir(_)) && node.path.starts_with(&child.path)
        });
        if let Some(child) = below {
            let child_path = child.path.clone();
            if let LocalKind::Dir(grandchildren) = &mut child.kind {
                insert(grandchildren, &child_path, node);
                return;
            }
        }
    }
    children.push(node);
}

/// Whether the local modification time `local` is Drive's `modifiedTime` exactly.
#[must_use]
pub(crate) fn same_time(local: Option<SystemTime>, remote: Option<DateTime<Utc>>) -> bool {
    match (local, remote) {
        (Some(local), Some(remote)) => DateTime::<Utc>::from(local) == remote,
        _ => false,
    }
}
