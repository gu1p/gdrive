//! Phase 3 of a sync, with `--delete` only: the extras of the plan are removed, after phase 2
//! ended without a failure on a complete inventory (the parent module decides).
//!
//! - Drive: an extra goes to the Drive trash (`files.update` with `trashed`), never
//!   `files.delete`; a folder goes with everything in it, which the plan made sure are all
//!   extras.
//! - Local: an extra is moved to `<LOCAL_DIR>/.gdrive-trash/<time of the run>/`, under its path
//!   below the local root, after checking that it is still exactly what the inventory found
//!   (see below). Nothing is moved over an existing entry, and nothing into a `.gdrive-trash`
//!   that is not a plain directory.
//! - Local with `--delete-permanently`: after the same check, the files the inventory found are
//!   removed one by one, each checked again just before, and then their directories, each when
//!   it is empty.
//!
//! # Local checks
//!
//! Every local path is reached from the local root, one component at a time, with
//! `symlink_metadata`: each directory on the way must be a directory and not a symlink, so a
//! directory that was replaced by a symlink after the inventory (to a place outside the root)
//! stops the removal before anything below it is touched. The root itself must be the directory
//! phase 3 started with. Symlinks are never followed and never removed.
//!
//! Before an extra goes, its whole tree is compared with the inventory: the same entries in
//! every directory (nothing added, nothing missing), each of the same type and identity (device
//! and inode on Unix), and each file of the same size and modification time. Any difference
//! leaves the extra in place and fails it: something changed it since the inventory, and only a
//! new run can tell what it is now.
//!
//! What remains is the race of a check and the operation after it: a process that swaps a
//! directory for a symlink in the instant between them. std offers no `openat`-style removal
//! below a directory handle; the window is one system call wide.

use std::{
    io,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use futures::{stream, Future};
use tokio::{fs, sync::OnceCell};

use super::{
    errors::{self, ItemError},
    inventory::{FileId, LOCAL_TRASH},
    plan::{Extra, ExtraTarget, Snapshot},
};
use crate::{
    files::update::{update_metadata, PatchFile},
    hub::Hub,
    transfer::scheduler::{Job, JobKind, Options, Outcome, Printer, Report, Scheduler},
};

/// How local extras are removed; Drive extras always go to the Drive trash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Trash,
    Permanently,
}

impl Mode {
    /// The action of the line of an extra.
    #[must_use]
    pub(crate) fn action(self) -> &'static str {
        match self {
            Mode::Trash => "extra->trash",
            Mode::Permanently => "extra->delete",
        }
    }
}

/// Removes `extras` per `mode`; returns the report and how many were removed.
pub(crate) async fn run(
    hub: &Hub,
    extras: Vec<Extra>,
    mode: Mode,
    local_root: &Path,
    options: Options,
    printer: &Printer,
    cancel: impl Future<Output = ()> + Send,
) -> (Report, u64) {
    let shared = Arc::new(Shared {
        hub: hub.clone(),
        mode,
        trash: LocalTrash::new(LocalRoot::new(local_root)),
        removed: AtomicU64::new(0),
    });
    let jobs = extras
        .into_iter()
        .map(|extra| shared.job(extra))
        .collect::<Vec<_>>();
    let report = Scheduler::new(options, printer.clone())
        .run(stream::iter(jobs), cancel)
        .await;
    (report, shared.removed.load(Ordering::Relaxed))
}

struct Shared {
    hub: Hub,
    mode: Mode,
    trash: LocalTrash,
    removed: AtomicU64,
}

impl Shared {
    fn job(self: &Arc<Self>, extra: Extra) -> Job {
        let Extra { label, target } = extra;
        let shared = Arc::clone(self);
        let line = format!("{} {label}", self.mode.action());
        Job::new(
            JobKind::Metadata,
            label.clone(),
            move |context| async move {
                context.printer.out(line).await;
                let removed = match target {
                    ExtraTarget::Drive { id } => {
                        let patch = PatchFile::new(id).with_trashed(true);
                        update_metadata(&shared.hub, patch)
                            .await
                            .map(drop)
                            .map_err(|err| ItemError::Trash(Box::new(err)))
                    }
                    ExtraTarget::Local {
                        path,
                        rel,
                        snapshot,
                    } => match shared.mode {
                        Mode::Trash => shared.trash.take(&path, &rel, &snapshot).await,
                        Mode::Permanently => shared.trash.root.remove(&path, &snapshot).await,
                    },
                };
                match removed {
                    Ok(()) => {
                        shared.removed.fetch_add(1, Ordering::Relaxed);
                        Outcome::Transferred { bytes: 0 }.into()
                    }
                    Err(source) => Outcome::failed(errors::Item { label, source }).into(),
                }
            },
        )
    }
}

/// The local root of a sync, the only way phase 3 reaches a local path; see the module
/// documentation.
struct LocalRoot {
    path: PathBuf,
    /// The identity of the root when phase 3 started.
    id: OnceCell<Option<FileId>>,
}

impl LocalRoot {
    fn new(path: &Path) -> Self {
        LocalRoot {
            path: path.to_path_buf(),
            id: OnceCell::new(),
        }
    }

    /// The metadata of the entry at `path` itself (not followed), reached from the root: every
    /// directory on the way is a plain directory, and the root is the one phase 3 started with.
    async fn reach(&self, path: &Path) -> Result<std::fs::Metadata, ItemError> {
        let changed = || ItemError::Changed(path.to_path_buf());
        let root = fs::symlink_metadata(&self.path)
            .await
            .map_err(|_| changed())?;
        if !root.is_dir() {
            return Err(changed());
        }
        let first = *self.id.get_or_init(|| async { FileId::of(&root) }).await;
        if !FileId::same(first, FileId::of(&root)) {
            return Err(changed());
        }

        let rel = path.strip_prefix(&self.path).map_err(|_| changed())?;
        let mut current = self.path.clone();
        let mut components = rel.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(changed());
            };
            current.push(name);
            let metadata = fs::symlink_metadata(&current)
                .await
                .map_err(|_| changed())?;
            if components.peek().is_none() {
                return Ok(metadata);
            }
            // A symlink is not a directory to symlink_metadata.
            if !metadata.is_dir() {
                return Err(changed());
            }
        }
        // The root itself is never removed.
        Err(changed())
    }

    /// Checks that the entry at `path` is what `snapshot` says, reached from the root.
    async fn check(&self, path: &Path, snapshot: &Snapshot) -> Result<(), ItemError> {
        let metadata = self.reach(path).await?;
        if matches(&metadata, snapshot) {
            Ok(())
        } else {
            Err(ItemError::Changed(path.to_path_buf()))
        }
    }

    /// Checks the whole tree at `path` against `snapshot`: every entry, directory before its
    /// contents, and the names in each directory.
    async fn check_tree(&self, path: &Path, snapshot: &Snapshot) -> Result<(), ItemError> {
        let mut pending = vec![(path.to_path_buf(), snapshot)];
        while let Some((path, snapshot)) = pending.pop() {
            self.check(&path, snapshot).await?;
            let Snapshot::Dir { children, .. } = snapshot else {
                continue;
            };
            let mut found = Vec::new();
            let mut entries = fs::read_dir(&path)
                .await
                .map_err(|_| ItemError::Changed(path.clone()))?;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|_| ItemError::Changed(path.clone()))?
            {
                found.push(entry.path());
            }
            let mut expected = children
                .iter()
                .map(|(child, _)| child.clone())
                .collect::<Vec<_>>();
            found.sort();
            expected.sort();
            if found != expected {
                return Err(ItemError::Changed(path));
            }
            pending.extend(
                children
                    .iter()
                    .map(|(child, snapshot)| (child.clone(), snapshot)),
            );
        }
        Ok(())
    }

    /// Removes what the inventory found at `path`, once the whole tree is checked: files first,
    /// each checked again, then their directories, each when it is empty.
    async fn remove(&self, path: &Path, snapshot: &Snapshot) -> Result<(), ItemError> {
        self.check_tree(path, snapshot).await?;
        // Depth first, each directory after its contents.
        let mut order = Vec::new();
        let mut pending = vec![(path.to_path_buf(), snapshot, false)];
        while let Some((path, snapshot, expanded)) = pending.pop() {
            match snapshot {
                Snapshot::Dir { children, .. } if !expanded => {
                    pending.push((path, snapshot, true));
                    for (child, child_snapshot) in children {
                        pending.push((child.clone(), child_snapshot, false));
                    }
                }
                _ => order.push((path, snapshot)),
            }
        }
        for (path, snapshot) in order {
            self.check(&path, snapshot).await?;
            let removed = match snapshot {
                Snapshot::File { .. } => fs::remove_file(&path).await,
                Snapshot::Dir { .. } => fs::remove_dir(&path).await,
            };
            removed.map_err(|err| ItemError::Remove(path.clone(), err))?;
        }
        Ok(())
    }
}

/// Whether `metadata` (of the entry itself, not followed) is the entry of `snapshot`. A symlink
/// is never.
fn matches(metadata: &std::fs::Metadata, snapshot: &Snapshot) -> bool {
    match snapshot {
        Snapshot::File { size, modified, id } => {
            metadata.is_file()
                && metadata.len() == *size
                && metadata.modified().ok() == *modified
                && FileId::same(*id, FileId::of(metadata))
        }
        Snapshot::Dir { id, .. } => metadata.is_dir() && FileId::same(*id, FileId::of(metadata)),
    }
}

/// The local trash of one run, `<root>/.gdrive-trash/<time>/`, created when the first extra
/// goes there.
struct LocalTrash {
    root: LocalRoot,
    /// `20261104T102030Z`: sortable, and valid on every filesystem.
    stamp: String,
    dir: OnceCell<PathBuf>,
}

impl LocalTrash {
    fn new(root: LocalRoot) -> Self {
        LocalTrash {
            root,
            stamp: DateTime::<Utc>::from(SystemTime::now())
                .format("%Y%m%dT%H%M%SZ")
                .to_string(),
            dir: OnceCell::new(),
        }
    }

    async fn dir(&self) -> Result<&PathBuf, ItemError> {
        self.dir.get_or_try_init(|| self.create()).await
    }

    async fn create(&self) -> Result<PathBuf, ItemError> {
        let trash = self.root.path.join(LOCAL_TRASH);
        self.plain_dir(&trash).await?;
        // A run in the same second gets `-1`, `-2`, ...: a run's trash is its own.
        for attempt in 0_u32.. {
            let name = if attempt == 0 {
                self.stamp.clone()
            } else {
                format!("{}-{attempt}", self.stamp)
            };
            let dir = trash.join(name);
            match fs::create_dir(&dir).await {
                Ok(()) => return Ok(dir),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists && attempt < 1000 => {}
                Err(err) => return Err(ItemError::CreateDir(dir, err)),
            }
        }
        unreachable!("the loop returns")
    }

    /// Makes sure `path`, below the root, is a plain directory, creating it if it is missing.
    async fn plain_dir(&self, path: &Path) -> Result<(), ItemError> {
        let not_a_dir = || ItemError::TrashNotADirectory(path.to_path_buf());
        match self.root.reach(path).await {
            Ok(metadata) if metadata.is_dir() => return Ok(()),
            Ok(_) => return Err(not_a_dir()),
            Err(_) => {}
        }
        if let Err(err) = fs::create_dir(path).await {
            if err.kind() != io::ErrorKind::AlreadyExists {
                return Err(ItemError::CreateDir(path.to_path_buf(), err));
            }
        }
        match self.root.reach(path).await {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            _ => Err(not_a_dir()),
        }
    }

    /// Moves the extra at `path` to its place in the trash, `rel` below the run's directory,
    /// once its whole tree is checked.
    async fn take(&self, path: &Path, rel: &Path, snapshot: &Snapshot) -> Result<(), ItemError> {
        self.root.check_tree(path, snapshot).await?;
        let dir = self.dir().await?.clone();
        let to = dir.join(rel);
        // The directories of `rel` in the trash, each a plain directory.
        let mut parent = dir.clone();
        if let Some(parents) = rel.parent() {
            for component in parents.components() {
                let Component::Normal(name) = component else {
                    return Err(ItemError::Changed(path.to_path_buf()));
                };
                parent.push(name);
                self.plain_dir(&parent).await?;
            }
        }
        // rename replaces an existing file on Unix: never move over anything.
        if fs::symlink_metadata(&to).await.is_ok() {
            return Err(ItemError::TrashTaken(to));
        }
        // Just before: the extra itself, reached from the root, is still the one checked.
        self.root.check(path, snapshot).await?;
        fs::rename(path, &to)
            .await
            .map_err(|source| ItemError::MoveToTrash {
                from: path.to_path_buf(),
                to,
                source,
            })
    }
}
