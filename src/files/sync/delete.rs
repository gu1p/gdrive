//! Phase 3 of a sync, with `--delete` only: the extras of the plan are removed, after phase 2
//! ended without a failure on a complete inventory (the parent module decides).
//!
//! - Drive: an extra goes to the Drive trash (`files.update` with `trashed`), never
//!   `files.delete`; a folder goes with everything in it, which the plan made sure are all
//!   extras.
//! - Local: an extra is moved to `<LOCAL_DIR>/.gdrive-trash/<time of the run>/`, under its path
//!   below the local root, after checking that it is still what the inventory found (the type,
//!   and for a file the size and modification time). Nothing is moved over an existing entry,
//!   and nothing through a `.gdrive-trash` that is not a plain directory.
//! - Local with `--delete-permanently`: the files the inventory found are removed one by one,
//!   each checked as above, and then their directories, which must be empty by then: anything
//!   that appeared since the inventory stays.

use std::{
    io,
    path::{Path, PathBuf},
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
    inventory::LOCAL_TRASH,
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
        trash: LocalTrash::new(local_root),
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
                        Mode::Permanently => remove(&path, &snapshot).await,
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

/// The local trash of one run, `<root>/.gdrive-trash/<time>/`, created when the first extra
/// goes there.
struct LocalTrash {
    root: PathBuf,
    /// `20261104T102030Z`: sortable, and valid on every filesystem.
    stamp: String,
    dir: OnceCell<PathBuf>,
}

impl LocalTrash {
    fn new(root: &Path) -> Self {
        LocalTrash {
            root: root.to_path_buf(),
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
        let trash = self.root.join(LOCAL_TRASH);
        plain_dir(&trash).await?;
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

    /// Moves the extra at `path` to its place in the trash, `rel` below the run's directory.
    async fn take(&self, path: &Path, rel: &Path, snapshot: &Snapshot) -> Result<(), ItemError> {
        check(path, snapshot).await?;
        let dir = self.dir().await?;
        let to = dir.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|err| ItemError::CreateDir(parent.to_path_buf(), err))?;
        }
        // rename replaces an existing file on Unix: never move over anything.
        if fs::symlink_metadata(&to).await.is_ok() {
            return Err(ItemError::TrashTaken(to));
        }
        fs::rename(path, &to)
            .await
            .map_err(|source| ItemError::MoveToTrash {
                from: path.to_path_buf(),
                to,
                source,
            })
    }
}

/// Makes sure `path` is a directory and not a symlink, creating it if it is missing.
async fn plain_dir(path: &Path) -> Result<(), ItemError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(ItemError::TrashNotADirectory(path.to_path_buf())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => match fs::create_dir(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(path)
                    .await
                    .map_err(|err| ItemError::CreateDir(path.to_path_buf(), err))?;
                if metadata.is_dir() {
                    Ok(())
                } else {
                    Err(ItemError::TrashNotADirectory(path.to_path_buf()))
                }
            }
            Err(err) => Err(ItemError::CreateDir(path.to_path_buf(), err)),
        },
        Err(err) => Err(ItemError::CreateDir(path.to_path_buf(), err)),
    }
}

/// Whether the entry at `path` is still what the inventory found: the same type (never a
/// symlink), and for a file the same size and modification time.
async fn check(path: &Path, snapshot: &Snapshot) -> Result<(), ItemError> {
    let changed = || ItemError::Changed(path.to_path_buf());
    let metadata = fs::symlink_metadata(path).await.map_err(|_| changed())?;
    let same = match snapshot {
        Snapshot::File { size, modified } => {
            metadata.is_file() && metadata.len() == *size && metadata.modified().ok() == *modified
        }
        Snapshot::Dir(_) => metadata.is_dir(),
    };
    if same {
        Ok(())
    } else {
        Err(changed())
    }
}

/// Removes what the inventory found at `path`: files after checking them, then their
/// directories, which must be empty by then.
async fn remove(path: &Path, snapshot: &Snapshot) -> Result<(), ItemError> {
    // Depth first, each directory after its contents.
    let mut order = Vec::new();
    let mut pending = vec![(path.to_path_buf(), snapshot, false)];
    while let Some((path, snapshot, expanded)) = pending.pop() {
        match snapshot {
            Snapshot::Dir(children) if !expanded => {
                pending.push((path, snapshot, true));
                for (child, child_snapshot) in children {
                    pending.push((child.clone(), child_snapshot, false));
                }
            }
            _ => order.push((path, snapshot)),
        }
    }
    for (path, snapshot) in order {
        check(&path, snapshot).await?;
        let removed = match snapshot {
            Snapshot::File { .. } => fs::remove_file(&path).await,
            Snapshot::Dir(_) => fs::remove_dir(&path).await,
        };
        removed.map_err(|err| ItemError::Remove(path.clone(), err))?;
    }
    Ok(())
}
