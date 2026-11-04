//! `files download --recursive`: the walk of the folder feeds the scheduler directly, so files
//! are downloaded while the folder is still being listed (milestone 2, items 2 and 3 of
//! docs/triage/README.md).
//!
//! ```text
//!   Walk (listing, `metadata_jobs` requests at once, bounded channel)
//!     -> LocalTree (local names per folder on FolderDone, see common::file_tree_drive)
//!     -> Source: creates each local directory as its path becomes known, turns files into
//!        jobs, documents and shortcuts into skipped items, listing errors into failures
//!     -> Scheduler (`jobs` downloads at once, bounded queue: a full queue pauses the walk)
//! ```
//!
//! Each file goes through the same steps as before the scheduler: the `--overwrite` check, the
//! md5 check that skips an identical local file, the media request with Range resume, and
//! `save_to_file`, which writes a temporary file and renames it once size and md5 match. A job
//! that is aborted (a failure elsewhere, Ctrl-C) removes the temporary files it created, and only
//! those.

#[cfg(test)]
mod tests;

use std::{
    collections::VecDeque,
    future::Future,
    mem,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use futures::{stream, Stream, StreamExt};
use same_file::Handle;
use tokio::fs;

use super::{
    err_if_local_file_exists, errors, file_content, local_file_is_identical, remove_temp_file,
    resume::{Identity, ResumableDownload, Revision},
    save_through, Config, ExistingFileAction, MAX_TEMP_FILE_ATTEMPTS,
};
use crate::{
    common::{
        file_tree_drive::{self, Action, LocalTree, PlannedFile, Totals},
        human_size::human_size,
        local_name,
    },
    hub::Hub,
    transfer::{
        scheduler::{
            plural, Job, JobContext, JobKind, Outcome, Printer, Report, Scheduler, SkipReason,
        },
        walk::{Walk, WalkOptions},
    },
};

/// Downloads the folder `file` into the destination of `config`, printing through `printer`,
/// until every file is done or `cancel` resolves.
pub(super) async fn download_directory(
    hub: &Hub,
    file: google_drive3::api::File,
    config: &Config,
    printer: &Printer,
    cancel: impl Future<Output = ()> + Send,
) -> Result<(), errors::Download> {
    use errors::Download as E;

    let root_path = config.canonical_destination_root()?;
    let (mut tree, root) = LocalTree::new(file)
        .map_err(|err| E::CreateFileTree(file_tree_drive::errors::FileTreeDrive(err)))?;
    let walk_options = WalkOptions {
        listing_jobs: config.transfer.metadata_jobs.get(),
        ..WalkOptions::default()
    };
    let walk = Walk::start(Arc::new(hub.clone()), root, walk_options);

    let totals = Arc::new(Mutex::new(None));
    let source = Source {
        walk: Some(walk),
        actions: tree.take_actions().into(),
        tree,
        printer: printer.clone(),
        files: Arc::new(Files {
            hub: hub.clone(),
            root_path,
            existing_file_action: config.existing_file_action,
            acknowledge_abuse: config.acknowledge_abuse,
        }),
        totals: Arc::clone(&totals),
    };
    let report = Scheduler::new(config.transfer, printer.clone())
        .run(source.into_stream(), cancel)
        .await;

    let totals = totals.lock().unwrap_or_else(PoisonError::into_inner).take();
    finish(&report, totals, config.transfer.continue_on_error, printer).await
}

/// Prints the end of a recursive download and tells whether it failed.
async fn finish(
    report: &Report,
    totals: Option<Totals>,
    continue_on_error: bool,
    printer: &Printer,
) -> Result<(), errors::Download> {
    let mut result = report.check().map_err(errors::Download::Incomplete);
    let incomplete_listing = totals.is_some_and(|totals| totals.incomplete);
    if result.is_ok() && incomplete_listing && !continue_on_error {
        result = Err(errors::Download::IncompleteListing);
    }

    if let (Ok(()), Some(totals)) = (&result, totals) {
        printer
            .out(format!(
                "Downloaded {} files in {} directories with a total size of {}",
                totals.files,
                totals.folders,
                human_size(totals.bytes)
            ))
            .await;
    }
    let documents = report.skipped(SkipReason::GoogleDocument);
    if documents > 0 {
        let documents = plural(documents, "Google Docs file", "Google Docs files");
        printer
            .err(format!(
                "{documents} skipped; use `gdrive files export` to download them"
            ))
            .await;
    }
    let shortcuts = report.skipped(SkipReason::Shortcut);
    if shortcuts > 0 {
        let shortcuts = plural(shortcuts, "shortcut", "shortcuts");
        printer
            .err(format!(
                "{shortcuts} skipped; a recursive download does not follow shortcuts"
            ))
            .await;
    }
    printer.err(report.summary()).await;
    // A lost output is the first thing to fix; the rest of the report may follow from it.
    printer
        .flush()
        .await
        .map_err(errors::Download::WriteOutput)?;

    result
}

/// The jobs of a recursive download, from the walk.
struct Source {
    /// `None` once the walk ended.
    walk: Option<Walk>,
    tree: LocalTree,
    /// Released by the tree, not yet turned into jobs.
    actions: VecDeque<Action>,
    printer: Printer,
    files: Arc<Files>,
    /// What the walk found, once it ended.
    totals: Arc<Mutex<Option<Totals>>>,
}

impl Source {
    fn into_stream(self) -> impl Stream<Item = Job> + Send {
        stream::unfold(self, |mut source| async move {
            let job = source.next_job().await?;
            Some((job, source))
        })
    }

    /// Follows the tree's actions until one is a job; reads the walk when there is none.
    async fn next_job(&mut self) -> Option<Job> {
        loop {
            if let Some(action) = self.actions.pop_front() {
                match action {
                    Action::CreateDir(path) => {
                        if let Some(failed) = self.create_dir(&path).await {
                            return Some(failed);
                        }
                    }
                    Action::Download(file) => return Some(self.files.job(file)),
                    Action::Done {
                        kind,
                        label,
                        outcome,
                    } => return Some(Job::done(kind, label, outcome)),
                    Action::Warning(warning) => self.printer.err(warning).await,
                }
                continue;
            }

            let walk = self.walk.as_mut()?;
            if let Some(item) = walk.next().await {
                self.tree.push(item);
            } else {
                self.walk = None;
                let totals = self.tree.totals();
                self.printer
                    .out(format!(
                        "Found {} files in {} directories with a total size of {}",
                        totals.files,
                        totals.folders,
                        human_size(totals.bytes),
                    ))
                    .await;
                *self.totals.lock().unwrap_or_else(PoisonError::into_inner) = Some(totals);
            }
            self.actions.extend(self.tree.take_actions());
        }
    }

    /// Creates a local directory; a failure is the folder's outcome. The files in it then fail
    /// on their own.
    async fn create_dir(&self, path: &Path) -> Option<Job> {
        self.printer
            .out(format!("Creating directory {}", path.display()))
            .await;
        let abs_path = self.files.root_path.join(path);
        match fs::create_dir_all(&abs_path).await {
            Ok(()) => None,
            Err(err) => Some(Job::done(
                JobKind::Metadata,
                local_name::tree_path(path),
                Outcome::failed(errors::Download::CreateDirectory(abs_path, err)),
            )),
        }
    }
}

/// What every file job of a recursive download shares.
struct Files {
    hub: Hub,
    /// The canonical destination.
    root_path: PathBuf,
    existing_file_action: ExistingFileAction,
    acknowledge_abuse: bool,
}

impl Files {
    fn job(self: &Arc<Self>, file: PlannedFile) -> Job {
        let files = Arc::clone(self);
        let label = local_name::tree_path(&file.path);
        let size = file.size;
        let partial = PartialFiles::new(self.root_path.join(&file.path), &file.id);
        let cleanup = partial.clone();
        Job::new(JobKind::Media, label, move |context| async move {
            files.download(file, &partial, &context).await.into()
        })
        .with_size(size)
        .on_cancel(move || async move { cleanup.remove().await })
    }

    async fn download(
        &self,
        file: PlannedFile,
        partial: &PartialFiles,
        context: &JobContext,
    ) -> Outcome {
        match self.try_download(&file, partial, context).await {
            Ok(outcome) => outcome,
            Err(source) => Outcome::failed(errors::Download::RecursiveFile {
                path: file.path,
                source: Box::new(source),
            }),
        }
    }

    async fn try_download(
        &self,
        file: &PlannedFile,
        partial: &PartialFiles,
        context: &JobContext,
    ) -> Result<Outcome, errors::Download> {
        use errors::Download as E;

        let abs_file_path = self.root_path.join(&file.path);
        err_if_local_file_exists(&abs_file_path, self.existing_file_action).await?;
        if local_file_is_identical(&abs_file_path, file.md5.as_ref()).await {
            return Ok(Outcome::Unchanged);
        }

        let label = format!("download of '{}'", local_name::tree_path(&file.path));
        let source = file_content(&self.hub, &file.id, self.acknowledge_abuse, label)
            .map_err(|err| E::DownloadFile(Box::new(err)))?;
        // The walk lists md5 and size; the first check before a resume adds the rest.
        let revision = Revision {
            md5: file.md5,
            size: Some(file.size),
            ..Revision::default()
        };
        let identity = Identity::new(&self.hub, &file.id, revision);
        let mut download =
            ResumableDownload::new(source, self.hub.retrier().state(), revision.expected())
                .with_identity(identity);
        let first = download
            .next()
            .await
            .map_err(|err| E::DownloadFile(Box::new(err)))?;

        context
            .printer
            .out(format!("Downloading file '{}'", file.path.display()))
            .await;
        // `save_to_file`, with every temporary file it creates noted for the cleanup.
        save_through(download, first, &abs_file_path, &file.id, |file| {
            partial.track(file)
        })
        .await
        .map_err(|source| E::SaveBodyToFile {
            path: abs_file_path.clone(),
            source,
        })?;
        context.progress.bytes(file.size);

        Ok(Outcome::Transferred { bytes: file.size })
    }
}

/// The temporary files of one download, for when its task is aborted: `save_to_file` removes
/// its temporary file whenever it fails, but an aborted task runs no code.
///
/// `save_to_file` names a temporary file `.gdrive-<id>.part`, `.gdrive-<id>-1.part`, ..., the
/// first that is free, and another one when the content starts over. Other files with these
/// names (a stale one of an earlier run, one of a concurrent gdrive, one that took the name of a
/// temporary file of this download after it was removed) are not this download's, so the names
/// alone do not say what to remove. [`Self::track`] keeps an open handle on every temporary file
/// as it is created, and [`Self::remove`] removes only the temporary names that are still one of
/// these files, compared as [`same_file::Handle`]s: device and inode on Unix, volume serial number
/// and file index on Windows.
///
/// The handles stay open for the life of the job because these numbers identify a file only while
/// it is open: ext4 gives the inode of a removed file to the next file it creates, and Windows
/// documents the file index as stable only while a handle is open (FAT derives it from the
/// directory entry, so a file created under the same name gets the same one). An open handle
/// keeps the file, removed or not, and with it its numbers. The cost: a temporary file that
/// `save_to_file` removes when the content starts over keeps its disk space until the job ends,
/// and on a Windows file system without POSIX delete semantics (FAT, some network shares) also
/// its name.
///
/// The handles do not get in the way of the download: std opens files with `FILE_SHARE_DELETE` on
/// Windows, the handles kept here and the one same-file opens to compare included, so a temporary
/// file can still be renamed into place or removed while they are open. Where no identity is
/// available (same-file knows Unix and Windows), nothing is removed: a leftover temporary file is
/// better than a removed file of someone else.
#[derive(Clone)]
struct PartialFiles(Arc<PartialState>);

struct PartialState {
    /// The final path of the download.
    path: PathBuf,
    file_id: String,
    /// An open handle on every temporary file this download created.
    created: Mutex<Vec<Handle>>,
}

impl PartialFiles {
    fn new(path: PathBuf, file_id: &str) -> Self {
        PartialFiles(Arc::new(PartialState {
            path,
            file_id: file_id.to_owned(),
            created: Mutex::new(Vec::new()),
        }))
    }

    fn temp_path(&self, attempt: u32) -> PathBuf {
        let dir = self.0.path.parent().unwrap_or_else(|| Path::new(""));
        local_name::safe_join(dir, &local_name::temp_file_name(&self.0.file_id, attempt))
    }

    fn created(&self) -> std::sync::MutexGuard<'_, Vec<Handle>> {
        self.0
            .created
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps a handle on a temporary file `save_to_file` just created, and hands the file back
    /// to it unchanged.
    fn track(&self, file: fs::File) -> fs::File {
        // Nothing has run on the new file yet, so it converts without waiting.
        let file = match file.try_into_std() {
            Ok(file) => file,
            Err(file) => return file,
        };
        // A second handle on the same open file: the download closes its own when it is done
        // writing.
        let handle = file.try_clone().and_then(Handle::from_file).ok();
        if let Some(handle) = handle {
            self.created().push(handle);
        }
        fs::File::from_std(file)
    }

    /// Removes the temporary files this download created that are still there, and lets go of
    /// the handles.
    async fn remove(&self) {
        let mut created = mem::take(&mut *self.created());
        for attempt in 0..MAX_TEMP_FILE_ATTEMPTS {
            if created.is_empty() {
                break;
            }
            let path = self.temp_path(attempt);
            // Only a regular file is opened to compare: opening a FIFO would wait for a writer.
            let is_file = fs::symlink_metadata(&path)
                .await
                .is_ok_and(|metadata| metadata.is_file());
            if !is_file {
                continue;
            }
            let Some(found) = open_handle(path.clone()).await else {
                continue;
            };
            if let Some(ours) = created.iter().position(|ours| *ours == found) {
                drop(found);
                remove_temp_file(&path).await;
                created.swap_remove(ours);
            }
        }
    }
}

/// A handle on the file at `path`, to compare with the ones [`PartialFiles::track`] keeps.
async fn open_handle(path: PathBuf) -> Option<Handle> {
    tokio::task::spawn_blocking(move || Handle::from_path(path))
        .await
        .ok()?
        .ok()
}
