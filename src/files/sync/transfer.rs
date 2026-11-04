//! Phase 2 of a sync: the creates and updates of the plan, through the scheduler with the
//! job builders of the recursive transfers.
//!
//! The plan is walked folder before contents. An item without work (skipped, unchanged) is
//! printed as it is reached; every other item is a job that prints its line when it starts.
//!
//! - `up`: a new folder is a metadata job ([`create_folder`]) whose follow-ups are the jobs of
//!   its contents, all new too, so nothing is uploaded into a folder before it exists. Files go
//!   up with [`send_file`]: a new file under a pre-generated id, a changed one as a new revision
//!   of its Drive file (same id, sharing and history kept).
//! - `down`: a new folder is created as it is reached, before the jobs of its contents. Files
//!   come through the recursive download's jobs ([`Files`]): a temporary file per download,
//!   renamed into place once size and md5 match, removed when the job is cancelled; a new file
//!   never replaces a local file that appeared since the inventory. A downloaded file gets
//!   Drive's `modifiedTime`, which the next run compares.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, PoisonError,
    },
    time::SystemTime,
};

use futures::{stream, Stream};
use tokio::fs;

use super::{
    errors::{self, ItemError},
    inventory::FileState,
    plan::{self, Direction, FileWork, Item, Touch, Work},
};
use crate::{
    common::file_tree_drive::{PlannedExport, PlannedFile},
    files::{
        download::{
            recursive::{Files, PartialFiles},
            ExistingFileAction,
        },
        export::set_modified_time,
        upload::recursive::{create_folder, send_file},
    },
    hub::Hub,
    transfer::{
        scheduler::{Job, JobContext, JobKind, JobOutput, Outcome, Printer, Subtree},
        upload::{UploadOptions, UploadTarget},
    },
};

/// What phase 2 did, by class; failures and cancellations are in the scheduler's report.
#[derive(Debug, Default)]
pub(crate) struct Done {
    pub(crate) created: AtomicU64,
    pub(crate) updated: AtomicU64,
    pub(crate) unchanged: AtomicU64,
}

impl Done {
    fn add(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// What every job of phase 2 shares.
pub(crate) struct Transfers {
    hub: Hub,
    direction: Direction,
    /// The canonical local root.
    local_root: PathBuf,
    upload: UploadOptions,
    /// `down`: new files, which never replace a local file.
    create: Arc<Files>,
    /// `down`: changed files, which replace the local file.
    update: Arc<Files>,
    /// `up`: ids from `files.generateIds` for the new files and folders.
    ids: Mutex<Vec<String>>,
    pub(crate) done: Done,
}

impl Transfers {
    #[must_use]
    pub(crate) fn new(
        hub: &Hub,
        direction: Direction,
        local_root: PathBuf,
        upload: UploadOptions,
        acknowledge_abuse: bool,
        ids: Vec<String>,
    ) -> Arc<Self> {
        let files = |action| {
            Arc::new(Files::for_plan(
                hub.clone(),
                local_root.clone(),
                action,
                acknowledge_abuse,
            ))
        };
        Arc::new(Transfers {
            hub: hub.clone(),
            direction,
            create: files(ExistingFileAction::Abort),
            update: files(ExistingFileAction::Overwrite),
            local_root,
            upload,
            ids: Mutex::new(ids),
            done: Done::default(),
        })
    }

    /// The jobs of `items`, the plan of the Drive folder `remote_root`, folder before contents.
    /// Items without work are printed as the stream reaches them.
    pub(crate) fn source(
        self: &Arc<Self>,
        items: Vec<Item>,
        remote_root: String,
        printer: Printer,
    ) -> impl Stream<Item = Job> + Send {
        let source = Source {
            stack: vec![Frame {
                items: items.into_iter(),
                parent: remote_root,
            }],
            transfers: Arc::clone(self),
            printer,
        };
        stream::unfold(source, |mut source| async move {
            let job = source.next_job().await?;
            Some((job, source))
        })
    }

    fn next_id(&self) -> Option<String> {
        self.ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
    }

    /// `up`: the job that creates a folder in `parent` and then releases its contents.
    fn folder_job(
        self: &Arc<Self>,
        label: String,
        name: String,
        items: Vec<Item>,
        parent: String,
    ) -> Job {
        let id = self.next_id();
        let subtree = subtree(&items);
        let transfers = Arc::clone(self);
        Job::new(
            JobKind::Metadata,
            label.clone(),
            move |context| async move {
                context.printer.out(format!("create {label}")).await;
                let folder_id =
                    match create_folder(&transfers.hub, id, name, Some(vec![parent])).await {
                        Ok(folder_id) => folder_id,
                        Err(source) => {
                            return Outcome::failed(errors::Item {
                                label,
                                source: ItemError::Upload(Box::new(source)),
                            })
                            .into()
                        }
                    };
                Done::add(&transfers.done.created);

                let (folders, files) = items
                    .into_iter()
                    .partition::<Vec<_>, _>(|item| matches!(item.work, Work::CreateDir { .. }));
                let folders_parent = folder_id.clone();
                let folder_transfers = Arc::clone(&transfers);
                let folders = folders
                    .into_iter()
                    .filter_map(move |item| folder_transfers.created_child(item, &folders_parent));
                let files = files
                    .into_iter()
                    .filter_map(move |item| transfers.created_child(item, &folder_id));
                JobOutput::from(Outcome::Transferred { bytes: 0 })
                    .follow_up(folders)
                    .follow_up(files)
            },
        )
        .with_subtree(subtree)
    }

    /// `up`: the job of an item in a folder that was just created. Skipped items are printed
    /// already.
    fn created_child(self: &Arc<Self>, item: Item, parent: &str) -> Option<Job> {
        let Item { label, work } = item;
        match work {
            Work::CreateDir { name, items, .. } => {
                Some(self.folder_job(label, name, items, parent.to_owned()))
            }
            Work::Create(work) => Some(self.file_job(label, work, true, parent.to_owned())),
            _ => None,
        }
    }

    /// The job of a new (`create`) or changed file; `parent` is the id of its Drive folder.
    fn file_job(
        self: &Arc<Self>,
        label: String,
        work: FileWork,
        create: bool,
        parent: String,
    ) -> Job {
        let verb = if create { "create" } else { "update" };
        let line = format!("{verb} {label}");
        match work {
            FileWork::Upload {
                path,
                name,
                size,
                existing,
            } => {
                let target = UploadTarget {
                    parents: vec![parent],
                    // An update keeps the Drive name.
                    name: existing.is_none().then_some(name),
                    mime: None,
                    new_file_id: if existing.is_none() {
                        self.next_id()
                    } else {
                        None
                    },
                    existing_file_id: existing,
                    convert_to: None,
                };
                self.upload_job(label, line, path, target, size)
            }
            FileWork::Download {
                rel,
                id,
                size,
                md5,
                modified,
            } => {
                let file = PlannedFile {
                    path: rel,
                    id,
                    size,
                    md5,
                };
                self.download_job(label, line, file, modified, create)
            }
            FileWork::Export {
                rel,
                id,
                doc_type,
                format,
                modified,
            } => {
                let export = PlannedExport {
                    path: rel,
                    id,
                    doc_type,
                    format,
                    modified_time: modified,
                };
                self.export_job(label, line, export, create)
            }
        }
    }

    /// `up`: sends the local file `path` to `target`.
    fn upload_job(
        self: &Arc<Self>,
        label: String,
        line: String,
        path: PathBuf,
        target: UploadTarget,
        size: u64,
    ) -> Job {
        let transfers = Arc::clone(self);
        let create = target.existing_file_id.is_none();
        Job::new(JobKind::Media, label.clone(), move |context| async move {
            context.printer.out(line).await;
            let sent = send_file(
                &transfers.hub,
                &path,
                &target,
                &transfers.upload,
                &context.progress,
            )
            .await;
            match sent {
                Ok(bytes) => {
                    transfers.count(create);
                    Outcome::Transferred { bytes }.into()
                }
                Err(source) => Outcome::failed(errors::Item {
                    label,
                    source: ItemError::Upload(Box::new(source)),
                })
                .into(),
            }
        })
        .with_size(size)
    }

    /// `down`: downloads `file` with the recursive download's job, then gives it Drive's time.
    fn download_job(
        self: &Arc<Self>,
        label: String,
        line: String,
        file: PlannedFile,
        modified: Option<chrono::DateTime<chrono::Utc>>,
        create: bool,
    ) -> Job {
        let transfers = Arc::clone(self);
        let files = Arc::clone(if create { &self.create } else { &self.update });
        let path = self.local_root.join(&file.path);
        let partial = PartialFiles::new(path.clone(), &file.id);
        let cleanup = partial.clone();
        let size = file.size;
        Job::new(JobKind::Media, label, move |context| async move {
            context.printer.out(line).await;
            let outcome = files.download(file, &partial, &context).await;
            if matches!(outcome, Outcome::Transferred { .. }) {
                if let Some(modified) = modified {
                    keep_time(&path, modified, &context).await;
                }
                transfers.count(create);
            }
            outcome.into()
        })
        .with_size(size)
        .on_cancel(move || async move { cleanup.remove().await })
    }

    /// `down --export`: exports `export` with the recursive download's job, which gives the
    /// file the document's time.
    fn export_job(
        self: &Arc<Self>,
        label: String,
        line: String,
        export: PlannedExport,
        create: bool,
    ) -> Job {
        let transfers = Arc::clone(self);
        let files = Arc::clone(if create { &self.create } else { &self.update });
        let partial = PartialFiles::new(self.local_root.join(&export.path), &export.id);
        let cleanup = partial.clone();
        Job::new(JobKind::Media, label, move |context| async move {
            context.printer.out(line).await;
            let outcome = files.export(export, &partial, &context).await;
            match outcome {
                Outcome::Transferred { .. } => transfers.count(create),
                Outcome::Unchanged => Done::add(&transfers.done.unchanged),
                _ => {}
            }
            outcome.into()
        })
        .on_cancel(move || async move { cleanup.remove().await })
    }

    fn count(&self, create: bool) {
        Done::add(if create {
            &self.done.created
        } else {
            &self.done.updated
        });
    }

    /// `down`: an unchanged file that was compared by md5 gets Drive's time, so that the next
    /// run does not read it again.
    ///
    /// The time is set through a handle on the file, once that handle shows the file as it was
    /// read (size, time and, where known, identity): a file that changed since, or that another
    /// file replaced, keeps its time and fails as changed.
    fn touch_job(self: &Arc<Self>, label: String, touch: Touch) -> Job {
        let transfers = Arc::clone(self);
        Job::new(JobKind::Media, label.clone(), move |context| async move {
            context.printer.out(format!("unchanged {label}")).await;
            match stamp(touch.clone()).await {
                Ok(()) => {}
                Err(Stamp::Changed) => {
                    return Outcome::failed(errors::Item {
                        label,
                        source: ItemError::Changed(touch.path),
                    })
                    .into();
                }
                Err(Stamp::Io(err)) => {
                    context
                        .printer
                        .err(format!(
                            "Warning: unable to set the modification time of '{}': {err}",
                            touch.path.display()
                        ))
                        .await;
                }
            }
            Done::add(&transfers.done.unchanged);
            Outcome::Unchanged.into()
        })
    }
}

/// Why [`stamp`] did not set the time.
enum Stamp {
    /// The file is not the one that was read.
    Changed,
    Io(io::Error),
}

/// Sets the time of `touch`, through a handle that shows the file as it was read.
async fn stamp(touch: Touch) -> Result<(), Stamp> {
    tokio::task::spawn_blocking(move || {
        // Windows needs write access to change the time; nothing is written.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&touch.path)
            .map_err(|err| {
                if err.kind() == io::ErrorKind::NotFound {
                    Stamp::Changed
                } else {
                    Stamp::Io(err)
                }
            })?;
        let now = FileState::of(&file.metadata().map_err(Stamp::Io)?);
        if !touch.read.is_some_and(|read| read.same(&now)) {
            return Err(Stamp::Changed);
        }
        file.set_modified(SystemTime::from(touch.modified))
            .map_err(Stamp::Io)
    })
    .await
    .map_err(|err| Stamp::Io(io::Error::other(err)))?
}

/// Gives the local file Drive's modification time; a failure is a warning, the content is
/// right.
async fn keep_time(path: &Path, modified: chrono::DateTime<chrono::Utc>, context: &JobContext) {
    if let Err(err) = set_modified_time(path, modified).await {
        context
            .printer
            .err(format!(
                "Warning: unable to set the modification time of '{}': {err}",
                path.display()
            ))
            .await;
    }
}

/// The items of one folder still to walk.
struct Frame {
    items: std::vec::IntoIter<Item>,
    /// The Drive folder they are in (`up`).
    parent: String,
}

struct Source {
    stack: Vec<Frame>,
    transfers: Arc<Transfers>,
    printer: Printer,
}

impl Source {
    async fn next_job(&mut self) -> Option<Job> {
        loop {
            let frame = self.stack.last_mut()?;
            let Some(item) = frame.items.next() else {
                self.stack.pop();
                continue;
            };
            let parent = frame.parent.clone();
            let line = plan::line(&item);
            let Item { label, work } = item;
            match work {
                Work::Descend { remote_id, items } => self.stack.push(Frame {
                    items: items.into_iter(),
                    parent: remote_id,
                }),
                Work::Skip(_) | Work::Compare { .. } => {
                    self.printer.out(line.unwrap_or_default()).await;
                }
                Work::Unchanged(None) => {
                    self.printer.out(line.unwrap_or_default()).await;
                    Done::add(&self.transfers.done.unchanged);
                }
                Work::Unchanged(Some(touch)) => {
                    return Some(self.transfers.touch_job(label, touch))
                }
                Work::Create(work) => {
                    return Some(self.transfers.file_job(label, work, true, parent));
                }
                Work::Update(work) => {
                    return Some(self.transfers.file_job(label, work, false, parent));
                }
                Work::CreateDir { name, rel, items } => match self.transfers.direction {
                    Direction::Up => {
                        self.print_skipped(&items).await;
                        return Some(self.transfers.folder_job(label, name, items, parent));
                    }
                    Direction::Down => {
                        self.printer.out(format!("create {label}")).await;
                        let path = self.transfers.local_root.join(&rel);
                        match create_local_dir(&path).await {
                            Ok(()) => {
                                Done::add(&self.transfers.done.created);
                                self.stack.push(Frame {
                                    items: items.into_iter(),
                                    parent,
                                });
                            }
                            Err(source) => {
                                let error = errors::Item {
                                    label: label.clone(),
                                    source: ItemError::CreateDir(path, source),
                                };
                                return Some(
                                    Job::done(JobKind::Metadata, label, Outcome::failed(error))
                                        .with_subtree(subtree(&items)),
                                );
                            }
                        }
                    }
                },
            }
        }
    }

    /// `up`: the lines of the items without work in a new folder, whose jobs are follow-ups.
    async fn print_skipped(&self, items: &[Item]) {
        let mut pending = vec![items];
        while let Some(items) = pending.pop() {
            for item in items {
                match &item.work {
                    Work::Skip(_) => {
                        if let Some(line) = plan::line(item) {
                            self.printer.out(line).await;
                        }
                    }
                    Work::CreateDir { items, .. } => pending.push(items),
                    _ => {}
                }
            }
        }
    }
}

/// Creates a local directory whose parent exists. A directory that is there already is fine;
/// anything else there (a file, a symlink) is an error: nothing is written through it.
async fn create_local_dir(path: &Path) -> io::Result<()> {
    match fs::create_dir(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).await?;
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "something that is not a directory is in its place",
                ))
            }
        }
        Err(err) => Err(err),
    }
}

/// The jobs below a new folder, which count as cancelled when it fails.
fn subtree(items: &[Item]) -> Subtree {
    let mut subtree = Subtree::default();
    let mut pending = vec![items];
    while let Some(items) = pending.pop() {
        for item in items {
            match &item.work {
                Work::CreateDir { items, .. } => {
                    subtree.folders += 1;
                    pending.push(items);
                }
                Work::Create(work) | Work::Update(work) => {
                    subtree.files += 1;
                    subtree.bytes += work.size();
                }
                Work::Unchanged(Some(_)) => subtree.files += 1,
                _ => {}
            }
        }
    }
    subtree
}

/// The file jobs of `items` and the bytes they transfer, for the progress's totals: transfers,
/// and the files that get Drive's time. Exports count with no bytes, their size is unknown.
#[must_use]
pub(crate) fn planned(items: &[Item]) -> (u64, u64) {
    let (mut files, mut bytes) = (0, 0);
    let mut pending = vec![items];
    while let Some(items) = pending.pop() {
        for item in items {
            match &item.work {
                Work::CreateDir { items, .. } | Work::Descend { items, .. } => {
                    pending.push(items);
                }
                Work::Create(work) | Work::Update(work) => {
                    files += 1;
                    bytes += work.size();
                }
                Work::Unchanged(Some(_)) => files += 1,
                Work::Unchanged(None) | Work::Compare { .. } | Work::Skip(_) => {}
            }
        }
    }
    (files, bytes)
}

/// `up`: how many ids the new files and folders of `items` need.
#[must_use]
pub(crate) fn new_ids(items: &[Item]) -> usize {
    let mut count = 0;
    let mut pending = vec![items];
    while let Some(items) = pending.pop() {
        for item in items {
            match &item.work {
                Work::CreateDir { items, .. } => {
                    count += 1;
                    pending.push(items);
                }
                Work::Descend { items, .. } => pending.push(items),
                Work::Create(FileWork::Upload { .. }) => count += 1,
                _ => {}
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{stamp, Stamp, Touch};
    use crate::files::sync::inventory::FileState;

    fn read_now(path: &std::path::Path) -> FileState {
        FileState::of(&std::fs::metadata(path).unwrap())
    }

    fn drive_time() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
    }

    #[tokio::test]
    async fn the_time_is_set_only_on_the_file_that_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "same").unwrap();
        let touch = Touch {
            path: path.clone(),
            modified: drive_time(),
            read: Some(read_now(&path)),
        };

        // Edited since it was read.
        std::fs::write(&path, "edited").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(matches!(stamp(touch.clone()).await, Err(Stamp::Changed)));
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );

        // Gone.
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(stamp(touch).await, Err(Stamp::Changed)));

        // Unchanged.
        std::fs::write(&path, "same").unwrap();
        let touch = Touch {
            path: path.clone(),
            modified: drive_time(),
            read: Some(read_now(&path)),
        };
        assert!(stamp(touch).await.is_ok());
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            SystemTime::from(drive_time())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn another_file_with_the_same_size_and_time_keeps_its_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "same").unwrap();
        let read = Some(read_now(&path));
        let time = std::fs::metadata(&path).unwrap().modified().unwrap();
        // Created while the first exists: another inode.
        std::fs::write(dir.path().join("b"), "diff").unwrap();
        std::fs::rename(dir.path().join("b"), &path).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(time)
            .unwrap();

        let touch = Touch {
            path: path.clone(),
            modified: drive_time(),
            read,
        };
        assert!(matches!(stamp(touch).await, Err(Stamp::Changed)));
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), time);
    }
}
