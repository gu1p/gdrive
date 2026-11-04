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
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use futures::{stream, Stream, StreamExt};
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
                path.display().to_string(),
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
        let label = file.path.display().to_string();
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

        let label = format!("download of '{}'", file.path.display());
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
/// names (a stale one of an earlier run, one of a concurrent gdrive) are not this download's, so
/// the names alone do not say what to remove. [`Self::track`] notes the identity of every
/// temporary file as it is created (device and inode on Unix, the creation time on Windows), and
/// [`Self::remove`] removes only the temporary names that are still one of those files. Where
/// no identity is available, nothing is removed: a leftover temporary file is better than a
/// removed file of someone else.
#[derive(Clone)]
struct PartialFiles(Arc<PartialState>);

struct PartialState {
    /// The final path of the download.
    path: PathBuf,
    file_id: String,
    /// The temporary files this download created.
    created: Mutex<Vec<FileIdentity>>,
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

    fn created(&self) -> std::sync::MutexGuard<'_, Vec<FileIdentity>> {
        self.0
            .created
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes the identity of a temporary file `save_to_file` just created, and hands the file
    /// back to it unchanged.
    fn track(&self, file: fs::File) -> fs::File {
        // Nothing has run on the new file yet, so it converts without waiting.
        let file = match file.try_into_std() {
            Ok(file) => file,
            Err(file) => return file,
        };
        if let Some(identity) = file.metadata().ok().as_ref().and_then(FileIdentity::of) {
            self.created().push(identity);
        }
        fs::File::from_std(file)
    }

    /// Removes the temporary files this download created that are still there.
    async fn remove(&self) {
        let mut created = self.created().clone();
        for attempt in 0..MAX_TEMP_FILE_ATTEMPTS {
            if created.is_empty() {
                break;
            }
            let path = self.temp_path(attempt);
            let Ok(metadata) = fs::symlink_metadata(&path).await else {
                continue;
            };
            let identity = metadata
                .is_file()
                .then(|| FileIdentity::of(&metadata))
                .flatten();
            if let Some(found) = created.iter().position(|ours| Some(*ours) == identity) {
                created.swap_remove(found);
                remove_temp_file(&path).await;
            }
        }
    }
}

/// What tells one file from another while both exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    /// Unix: the device and the inode. Windows: the creation time, in 100 ns units.
    id: (u64, u64),
}

impl FileIdentity {
    #[cfg(unix)]
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the same signature as on platforms without file identities"
    )]
    fn of(metadata: &std::fs::Metadata) -> Option<FileIdentity> {
        use std::os::unix::fs::MetadataExt;
        Some(FileIdentity {
            id: (metadata.dev(), metadata.ino()),
        })
    }

    #[cfg(windows)]
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the same signature as on platforms without file identities"
    )]
    fn of(metadata: &std::fs::Metadata) -> Option<FileIdentity> {
        use std::os::windows::fs::MetadataExt;
        Some(FileIdentity {
            id: (metadata.creation_time(), 0),
        })
    }

    #[cfg(not(any(unix, windows)))]
    fn of(_metadata: &std::fs::Metadata) -> Option<FileIdentity> {
        None
    }
}
