//! The bounded job scheduler of recursive transfers (milestone 2, item 2 of
//! docs/triage/README.md).
//!
//! A recursive download or upload turns every file (and, for an upload, every folder) into a
//! [`Job`] and hands a [`Stream`] of them to [`Scheduler::run`]. The scheduler runs them with
//! bounded concurrency and returns a [`Report`] of their typed [`Outcome`]s.
//!
//! # Design
//!
//! ```text
//!   follow-ups of finished jobs (lazy iterators: an upload folder's subfolders, its files)
//!      |  taken first, a job at a time while the queue of its kind has room
//!   source: Stream<Item = Job>              (a walk of Drive, a local tree, ...)
//!      |  polled only while no follow-up waits and both queues have room: a full queue stops
//!      |  the source, and a walk behind it stops listing once its own channel is full
//!      v
//!   pending queue, one per JobKind          at most `queue` jobs each
//!      |  started while a slot of its kind is free: `jobs` media transfers and
//!      |  `metadata_jobs` metadata requests at once
//!      v
//!   JoinSet of running jobs  ---- one task per job, owned by the coordinator, every handle
//!      |                          awaited; a job gets a JobContext (Printer, JobProgress)
//!      v
//!   coordinator: counts each Outcome in the Report, prints failures (stderr, through the
//!   Printer), queues follow-up jobs, emits ProgressEvents
//! ```
//!
//! # Rules
//!
//! - Capacity before work: a job's closure runs only once it has a slot, so nothing (a file
//!   handle, a request) is opened for a job that waits.
//! - Every job ends with exactly one outcome: what it returned, [`Outcome::Cancelled`] when it
//!   was aborted or never started, or [`Outcome::Failed`] when it panicked.
//! - A failure stops the run unless [`Options::continue_on_error`]: the source is dropped (a
//!   walk stops listing), queued jobs are cancelled, running jobs are aborted and their cleanup
//!   ([`Job::on_cancel`], which removes temporary files) runs once their task has ended. With
//!   the option, independent jobs go on; the report still says the run failed.
//! - Cancellation (`cancel` of [`Scheduler::run`], Ctrl-C in the CLI) stops the run the same
//!   way.
//! - A job that ends failed or cancelled takes its [`Subtree`] with it: an upload folder that
//!   could not be created counts every file and folder below it as cancelled.
//! - Output: jobs print through the run's [`Printer`], a single task that writes whole lines in
//!   the order it receives them, so parallel jobs never interleave their lines. Output that
//!   cannot be written (a full disk behind stdout, not a closed pipe) stops the run like a
//!   failure, and fails the report.

#[cfg(test)]
pub(crate) mod capture;
mod printer;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    error::Error,
    fmt::{self, Display, Formatter, Write as _},
    future::Future,
    iter::Peekable,
    num::NonZeroUsize,
    pin::{pin, Pin},
    sync::Arc,
};

use futures::{future::BoxFuture, FutureExt, Stream, StreamExt};
use tokio::task::{self, JoinError, JoinSet};

pub(crate) use printer::{OutputError, Printer};

use super::{
    progress::{JobId, JobProgress, NoProgress, ProgressEvent, ProgressSink},
    walk::WalkOptions,
};
use crate::common::human_size::human_size;

/// The error of a failed job.
pub(crate) type JobError = Box<dyn Error + Send + Sync + 'static>;

/// What a job does, which decides the limit it counts against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum JobKind {
    /// Transfers a file's content: limited by [`Options::jobs`]. Counted as a file.
    Media,
    /// A metadata request, such as creating a folder: limited by [`Options::metadata_jobs`].
    /// Counted as a folder.
    Metadata,
}

/// Why an item is not transferred, and not a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum SkipReason {
    /// A Google Docs, Sheets, Slides, ... file: it has no content to download, only exports,
    /// and none was asked for (no `--export`, or `skip` for its type).
    GoogleDocument,
    /// A Google Workspace file of a type gdrive has no export format for (Forms, Sites, ...).
    NotExportable,
    /// A shortcut, which a recursive download does not follow.
    Shortcut,
}

/// How a job ended.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The content was transferred: `bytes` of it.
    Transferred {
        bytes: u64,
    },
    /// Nothing to do: the destination already holds the same content (same md5).
    Unchanged,
    Skipped {
        reason: SkipReason,
    },
    Failed {
        error: JobError,
    },
    /// Aborted while running, or never started, because the run stopped.
    Cancelled,
}

impl Outcome {
    #[must_use]
    pub(crate) fn failed(error: impl Error + Send + Sync + 'static) -> Self {
        Outcome::Failed {
            error: Box::new(error),
        }
    }

    /// Failed or cancelled: the job's follow-ups and subtree are not done.
    fn ends_subtree(&self) -> bool {
        matches!(self, Outcome::Failed { .. } | Outcome::Cancelled)
    }
}

/// The jobs a finished job releases, made as the scheduler takes them.
type FollowUps = Box<dyn Iterator<Item = Job> + Send>;

/// What a job returns: its outcome, and the jobs it releases (the children of an upload folder,
/// which need the folder to exist first).
///
/// The follow-ups are iterators that the scheduler takes jobs from only while its queue has
/// room for their kind, like a source; an iterator whose next job does not fit waits, the others
/// go on. So a folder of a hundred thousand files does not queue a hundred thousand jobs, and
/// its subfolders (one iterator) are created while its files (another) upload.
pub(crate) struct JobOutput {
    pub(crate) outcome: Outcome,
    /// Taken from when `outcome` is a success; cancelled otherwise.
    follow_ups: Vec<FollowUps>,
}

impl JobOutput {
    /// Adds the jobs of `follow_ups`, taken after those added before when their kinds have room.
    #[must_use]
    pub(crate) fn follow_up<I>(mut self, follow_ups: I) -> Self
    where
        I: IntoIterator<Item = Job>,
        I::IntoIter: Send + 'static,
    {
        self.follow_ups.push(Box::new(follow_ups.into_iter()));
        self
    }
}

impl From<Outcome> for JobOutput {
    fn from(outcome: Outcome) -> Self {
        JobOutput {
            outcome,
            follow_ups: Vec::new(),
        }
    }
}

impl fmt::Debug for JobOutput {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobOutput")
            .field("outcome", &self.outcome)
            .field("follow_ups", &self.follow_ups.len())
            .finish()
    }
}

/// The items below a job that only its follow-up jobs would account for: when the job fails or
/// is cancelled, they count as cancelled. An upload folder's subtree is every file and folder
/// below it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Subtree {
    pub(crate) files: u64,
    pub(crate) folders: u64,
}

/// What a running job gets from the scheduler.
pub(crate) struct JobContext {
    /// The run's only way to stdout and stderr.
    pub(crate) printer: Printer,
    pub(crate) progress: JobProgress,
}

type RunFn = Box<dyn FnOnce(JobContext) -> BoxFuture<'static, JobOutput> + Send>;
type CleanupFn = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;

/// One unit of work: a file transfer, a folder creation, or an item decided without work.
pub(crate) struct Job {
    kind: JobKind,
    /// The item's path, for messages and progress.
    label: String,
    size: Option<u64>,
    subtree: Subtree,
    work: Work,
}

enum Work {
    Run {
        run: RunFn,
        cleanup: Option<CleanupFn>,
    },
    /// Decided already (a skipped document, an item below a failed folder): counted, not run.
    Done(Outcome),
}

impl Job {
    /// A job that runs `run` once it has a slot of its `kind`.
    #[must_use]
    pub(crate) fn new<F, Fut>(kind: JobKind, label: impl Into<String>, run: F) -> Job
    where
        F: FnOnce(JobContext) -> Fut + Send + 'static,
        Fut: Future<Output = JobOutput> + Send + 'static,
    {
        Job {
            kind,
            label: label.into(),
            size: None,
            subtree: Subtree::default(),
            work: Work::Run {
                run: Box::new(move |context| run(context).boxed()),
                cleanup: None,
            },
        }
    }

    /// An item whose outcome is known without running anything. It takes no slot.
    #[must_use]
    pub(crate) fn done(kind: JobKind, label: impl Into<String>, outcome: Outcome) -> Job {
        Job {
            kind,
            label: label.into(),
            size: None,
            subtree: Subtree::default(),
            work: Work::Done(outcome),
        }
    }

    /// The size of the content, for progress.
    #[must_use]
    pub(crate) fn with_size(mut self, size: u64) -> Job {
        self.size = Some(size);
        self
    }

    #[must_use]
    pub(crate) fn with_subtree(mut self, subtree: Subtree) -> Job {
        self.subtree = subtree;
        self
    }

    /// `cleanup` runs after the job's task was aborted (or panicked) and has ended, for example
    /// to remove the temporary file of a download. It does not run when the job returned.
    #[must_use]
    pub(crate) fn on_cancel<F, Fut>(mut self, cleanup: F) -> Job
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if let Work::Run { cleanup: slot, .. } = &mut self.work {
            *slot = Some(Box::new(move || cleanup().boxed()));
        }
        self
    }
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job")
            .field("kind", &self.kind)
            .field("label", &self.label)
            .field("size", &self.size)
            .field("subtree", &self.subtree)
            .field("done", &matches!(self.work, Work::Done(_)))
            .finish_non_exhaustive()
    }
}

/// The limits and the failure policy of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Options {
    /// Media transfers at once (`--jobs`).
    pub(crate) jobs: NonZeroUsize,
    /// Metadata requests at once, such as folder creations; a walk's listing requests use the
    /// same limit ([`WalkOptions::listing_jobs`]).
    pub(crate) metadata_jobs: NonZeroUsize,
    /// Jobs of each kind taken from the source or from follow-ups before they can start.
    pub(crate) queue: NonZeroUsize,
    /// Go on after a failure with the jobs that do not depend on it (`--continue-on-error`).
    pub(crate) continue_on_error: bool,
}

impl Options {
    /// The plan's starting point: four transfers at once. Benchmark before raising it.
    pub(crate) const DEFAULT_JOBS: NonZeroUsize = NonZeroUsize::new(4).unwrap();
    /// The largest `--jobs`.
    pub(crate) const MAX_JOBS: usize = 64;
    /// Two listings or folder creations at once.
    pub(crate) const DEFAULT_METADATA_JOBS: NonZeroUsize =
        NonZeroUsize::new(WalkOptions::DEFAULT_LISTING_JOBS).unwrap();
    /// Queued jobs per transfer slot.
    const QUEUE_PER_JOB: NonZeroUsize = NonZeroUsize::new(2).unwrap();

    #[must_use]
    pub(crate) fn new(jobs: NonZeroUsize, continue_on_error: bool) -> Self {
        Options {
            jobs,
            metadata_jobs: Self::DEFAULT_METADATA_JOBS,
            queue: jobs.saturating_mul(Self::QUEUE_PER_JOB),
            continue_on_error,
        }
    }

    fn limit(&self, kind: JobKind) -> usize {
        match kind {
            JobKind::Media => self.jobs.get(),
            JobKind::Metadata => self.metadata_jobs.get(),
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Options::new(Options::DEFAULT_JOBS, false)
    }
}

/// The outcomes of one kind of item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) transferred: u64,
    /// Bytes of the transferred items.
    pub(crate) bytes: u64,
    pub(crate) unchanged: u64,
    pub(crate) skipped: u64,
    pub(crate) failed: u64,
    pub(crate) cancelled: u64,
}

impl Counts {
    fn record(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Transferred { bytes } => {
                self.transferred += 1;
                self.bytes += bytes;
            }
            Outcome::Unchanged => self.unchanged += 1,
            Outcome::Skipped { .. } => self.skipped += 1,
            Outcome::Failed { .. } => self.failed += 1,
            Outcome::Cancelled => self.cancelled += 1,
        }
    }
}

/// The result of a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Report {
    /// [`JobKind::Media`] jobs.
    pub(crate) files: Counts,
    /// [`JobKind::Metadata`] jobs.
    pub(crate) folders: Counts,
    /// Skipped items by reason, both kinds.
    pub(crate) skipped: BTreeMap<SkipReason, u64>,
    /// The run stopped scheduling after a failure.
    pub(crate) stopped: bool,
    /// The run was cancelled from outside (Ctrl-C).
    pub(crate) interrupted: bool,
    /// The printer could not write the output ([`Printer::flush`] has the error).
    pub(crate) output_failed: bool,
}

impl Report {
    fn counts_mut(&mut self, kind: JobKind) -> &mut Counts {
        match kind {
            JobKind::Media => &mut self.files,
            JobKind::Metadata => &mut self.folders,
        }
    }

    fn record(&mut self, kind: JobKind, outcome: &Outcome) {
        self.counts_mut(kind).record(outcome);
        if let Outcome::Skipped { reason } = outcome {
            *self.skipped.entry(*reason).or_default() += 1;
        }
    }

    fn cancel_subtree(&mut self, subtree: Subtree) {
        self.files.cancelled += subtree.files;
        self.folders.cancelled += subtree.folders;
    }

    /// The items skipped for `reason`.
    #[must_use]
    pub(crate) fn skipped(&self, reason: SkipReason) -> u64 {
        self.skipped.get(&reason).copied().unwrap_or_default()
    }

    /// The summary line for stderr, for example
    /// `Summary: 12 transferred (3.4 MB), 2 unchanged, 1 skipped, 0 failed, 0 cancelled`. The
    /// counts are files; folders that failed are added as `; 1 folder failed`, and a run that
    /// was interrupted ends with `; interrupted`.
    #[must_use]
    pub(crate) fn summary(&self) -> String {
        let files = &self.files;
        let mut line = format!(
            "Summary: {} transferred ({}), {} unchanged, {} skipped, {} failed, {} cancelled",
            files.transferred,
            human_size(files.bytes),
            files.unchanged,
            files.skipped,
            files.failed,
            files.cancelled,
        );
        if self.folders.failed > 0 {
            let folders = plural(self.folders.failed, "folder", "folders");
            let _ = write!(line, "; {folders} failed");
        }
        if self.interrupted {
            line.push_str("; interrupted");
        }
        line
    }

    /// `Ok` when every item ended without a failure and the run was not interrupted.
    pub(crate) fn check(&self) -> Result<(), Incomplete> {
        let failed = self.files.failed + self.folders.failed;
        if failed == 0 && !self.interrupted && !self.output_failed {
            return Ok(());
        }
        Err(Incomplete {
            failed_files: self.files.failed,
            failed_folders: self.folders.failed,
            cancelled: self.files.cancelled + self.folders.cancelled,
            stopped: self.stopped && !self.interrupted,
            interrupted: self.interrupted,
            output_failed: self.output_failed,
        })
    }
}

/// A run in which something failed or that was interrupted; the command exits non-zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Incomplete {
    failed_files: u64,
    failed_folders: u64,
    cancelled: u64,
    stopped: bool,
    interrupted: bool,
    output_failed: bool,
}

impl Display for Incomplete {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.interrupted {
            f.write_str("interrupted")?;
        } else if self.failed_files + self.failed_folders == 0 && self.output_failed {
            f.write_str("the output could not be written")?;
        } else {
            match (self.failed_files, self.failed_folders) {
                (files, 0) => write!(f, "{} failed", plural(files, "file", "files"))?,
                (0, folders) => write!(f, "{} failed", plural(folders, "folder", "folders"))?,
                (files, folders) => write!(
                    f,
                    "{} and {} failed",
                    plural(files, "file", "files"),
                    plural(folders, "folder", "folders")
                )?,
            }
        }
        if self.cancelled > 0 {
            write!(f, ", {} cancelled", self.cancelled)?;
        }
        if self.stopped {
            f.write_str("; use --continue-on-error to finish the other transfers after a failure")?;
        }
        Ok(())
    }
}

impl Error for Incomplete {}

/// `1 file`, `2 files`.
#[must_use]
pub(crate) fn plural(count: u64, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
    }
}

/// Resolves on Ctrl-C, the `cancel` of a CLI run. Never resolves where no handler can be
/// installed.
pub(crate) async fn ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Turns the error of a failed job into what is printed for it.
type Describe = Arc<dyn Fn(&(dyn Error + 'static)) -> String + Send + Sync>;

/// Runs jobs per its [`Options`]; see the module documentation.
pub(crate) struct Scheduler {
    options: Options,
    printer: Printer,
    progress: Arc<dyn ProgressSink>,
    describe: Describe,
}

impl Scheduler {
    #[must_use]
    pub(crate) fn new(options: Options, printer: Printer) -> Self {
        Scheduler {
            options,
            printer,
            progress: Arc::new(NoProgress),
            // As a failed command: the message of the error and the account's hint.
            describe: Arc::new(crate::failure_message),
        }
    }

    /// Prints failures with `describe` instead of the command's format, for tests that cannot
    /// set up an account.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_describe(
        mut self,
        describe: impl Fn(&(dyn Error + 'static)) -> String + Send + Sync + 'static,
    ) -> Self {
        self.describe = Arc::new(describe);
        self
    }

    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "for the progress display, which is not written yet"
        )
    )]
    pub(crate) fn with_progress(mut self, progress: Arc<dyn ProgressSink>) -> Self {
        self.progress = progress;
        self
    }

    /// Runs every job of `source`, and their follow-ups, until all ended, or until the run
    /// stops (a failure without `continue_on_error`, or `cancel` resolved) and every started
    /// job ended. Failed jobs are printed to stderr as they end; the summary is up to the
    /// caller ([`Report::summary`]).
    pub(crate) async fn run<S>(&self, source: S, cancel: impl Future<Output = ()> + Send) -> Report
    where
        S: Stream<Item = Job> + Send,
    {
        let run = Run {
            options: self.options,
            printer: self.printer.clone(),
            progress: Arc::clone(&self.progress),
            describe: Arc::clone(&self.describe),
            source: Some(Box::pin(source)),
            pending: PerKind::default(),
            active: PerKind::default(),
            tasks: JoinSet::new(),
            running: HashMap::new(),
            follow_ups: VecDeque::new(),
            next_id: 0,
            report: Report::default(),
            stopping: false,
        };
        run.execute(cancel).await
    }
}

/// A value per [`JobKind`].
#[derive(Debug, Default)]
struct PerKind<T> {
    media: T,
    metadata: T,
}

impl<T> PerKind<T> {
    fn get_mut(&mut self, kind: JobKind) -> &mut T {
        match kind {
            JobKind::Media => &mut self.media,
            JobKind::Metadata => &mut self.metadata,
        }
    }
}

/// A job waiting for a slot.
struct Queued {
    id: JobId,
    kind: JobKind,
    label: String,
    subtree: Subtree,
    run: RunFn,
    cleanup: Option<CleanupFn>,
}

/// A job whose task runs.
struct Running {
    id: JobId,
    kind: JobKind,
    label: String,
    subtree: Subtree,
    cleanup: Option<CleanupFn>,
}

/// The state of one [`Scheduler::run`].
struct Run<S> {
    options: Options,
    printer: Printer,
    progress: Arc<dyn ProgressSink>,
    describe: Describe,
    /// `None` once exhausted or dropped by a stop.
    source: Option<Pin<Box<S>>>,
    pending: PerKind<VecDeque<Queued>>,
    active: PerKind<usize>,
    tasks: JoinSet<JobOutput>,
    running: HashMap<task::Id, Running>,
    /// The follow-ups of finished jobs, taken from before the source.
    follow_ups: VecDeque<Peekable<FollowUps>>,
    next_id: u64,
    report: Report,
    /// Nothing new starts: the run stopped after a failure or was cancelled.
    stopping: bool,
}

impl<S: Stream<Item = Job> + Send> Run<S> {
    async fn execute(mut self, cancel: impl Future<Output = ()> + Send) -> Report {
        let mut cancel = pin!(cancel.fuse());

        loop {
            self.output_check();
            self.fill().await;
            // The source is read when the follow-ups are done and both kinds have room: its
            // next job may be of either.
            let pull = self.source.is_some()
                && !self.stopping
                && self.follow_ups.is_empty()
                && self.has_room(JobKind::Media)
                && self.has_room(JobKind::Metadata);
            // Nothing runs, so nothing is queued and no follow-up waits either: every queued job
            // of a kind with a free slot was just started, and follow-ups only wait for a full
            // queue.
            if self.tasks.is_empty() && !pull {
                break;
            }

            tokio::select! {
                biased;
                () = &mut cancel, if !self.report.interrupted => {
                    self.report.interrupted = true;
                    self.printer
                        .err("Interrupted: cancelling the transfers in progress")
                        .await;
                    self.stop();
                }
                Some(joined) = self.tasks.join_next_with_id() => {
                    self.joined(joined).await;
                }
                job = next_job(&mut self.source), if pull => {
                    if let Some(job) = job {
                        self.enqueue(job).await;
                    } else {
                        self.source = None;
                        self.progress.event(&ProgressEvent::SourceDone);
                    }
                }
            }
        }

        self.report
    }

    /// A failed output stops the run like a failed job: what it prints is lost.
    fn output_check(&mut self) {
        if !self.report.output_failed && self.printer.failed() {
            self.report.output_failed = true;
            if !self.options.continue_on_error {
                self.stop();
            }
        }
    }

    /// Whether the queue of `kind` takes another job.
    fn has_room(&self, kind: JobKind) -> bool {
        let queued = match kind {
            JobKind::Media => self.pending.media.len(),
            JobKind::Metadata => self.pending.metadata.len(),
        };
        queued < self.options.queue.get()
    }

    /// Starts what can start and queues follow-ups while their kinds have room.
    async fn fill(&mut self) {
        loop {
            self.start_ready();
            if self.stopping {
                return;
            }
            let Some(job) = self.next_follow_up() else {
                return;
            };
            self.enqueue(job).await;
        }
    }

    /// The next follow-up that fits: from the first iterator whose next job is decided already
    /// or has room in its kind's queue. Exhausted iterators are dropped.
    fn next_follow_up(&mut self) -> Option<Job> {
        let mut index = 0;
        while index < self.follow_ups.len() {
            let next = self.follow_ups[index]
                .peek()
                .map(|job| (job.kind, matches!(job.work, Work::Done(_))));
            let fits = match next {
                None => {
                    self.follow_ups.remove(index);
                    continue;
                }
                Some((kind, done)) => done || self.has_room(kind),
            };
            if fits {
                return self.follow_ups[index].next();
            }
            index += 1;
        }
        None
    }

    /// Starts queued jobs while their kind has a free slot; folders first, so that the files
    /// they release can follow.
    fn start_ready(&mut self) {
        if self.stopping {
            return;
        }
        for kind in [JobKind::Metadata, JobKind::Media] {
            let limit = self.options.limit(kind);
            while *self.active.get_mut(kind) < limit {
                let Some(queued) = self.pending.get_mut(kind).pop_front() else {
                    break;
                };
                self.spawn(queued);
            }
        }
    }

    fn spawn(&mut self, queued: Queued) {
        let Queued {
            id,
            kind,
            label,
            subtree,
            run,
            cleanup,
        } = queued;
        let context = JobContext {
            printer: self.printer.clone(),
            progress: JobProgress::new(id, Arc::clone(&self.progress)),
        };
        self.progress.event(&ProgressEvent::Started { job: id });
        let handle = self.tasks.spawn(run(context));
        self.running.insert(
            handle.id(),
            Running {
                id,
                kind,
                label,
                subtree,
                cleanup,
            },
        );
        *self.active.get_mut(kind) += 1;
    }

    /// Takes a job from the source or from a finished job's follow-ups.
    async fn enqueue(&mut self, job: Job) {
        let Job {
            kind,
            label,
            size,
            subtree,
            work,
        } = job;
        let id = JobId(self.next_id);
        self.next_id += 1;
        self.progress.event(&ProgressEvent::Queued {
            job: id,
            kind,
            label: &label,
            size,
        });

        match work {
            Work::Done(outcome) => self.finish(id, kind, &label, subtree, outcome).await,
            Work::Run { .. } if self.stopping => {
                self.finish(id, kind, &label, subtree, Outcome::Cancelled)
                    .await;
            }
            Work::Run { run, cleanup } => self.pending.get_mut(kind).push_back(Queued {
                id,
                kind,
                label,
                subtree,
                run,
                cleanup,
            }),
        }
    }

    /// A job's task ended.
    async fn joined(&mut self, joined: Result<(task::Id, JobOutput), JoinError>) {
        let (task_id, result) = match joined {
            Ok((task_id, output)) => (task_id, Ok(output)),
            Err(error) => (error.id(), Err(error)),
        };
        let Some(running) = self.running.remove(&task_id) else {
            return;
        };
        let Running {
            id,
            kind,
            label,
            subtree,
            cleanup,
        } = running;
        *self.active.get_mut(kind) -= 1;

        let JobOutput {
            outcome,
            follow_ups,
        } = match result {
            Ok(output) => output,
            Err(error) => {
                // The task is gone, and with it every file handle it held.
                if let Some(cleanup) = cleanup {
                    cleanup().await;
                }
                if error.is_cancelled() {
                    Outcome::Cancelled.into()
                } else {
                    Outcome::failed(JobPanicked {
                        label: label.clone(),
                    })
                    .into()
                }
            }
        };

        let ends_subtree = outcome.ends_subtree();
        self.finish(id, kind, &label, subtree, outcome).await;
        for follow_ups in follow_ups {
            if ends_subtree || self.stopping {
                follow_ups.for_each(|job| self.cancel(job));
            } else {
                self.follow_ups.push_back(follow_ups.peekable());
            }
        }
    }

    /// Counts a job's outcome, prints a failure, and stops the run on a failure unless it
    /// continues on errors.
    async fn finish(
        &mut self,
        id: JobId,
        kind: JobKind,
        label: &str,
        subtree: Subtree,
        outcome: Outcome,
    ) {
        self.progress.event(&ProgressEvent::Finished {
            job: id,
            kind,
            label,
            outcome: &outcome,
        });
        if let Outcome::Failed { error } = &outcome {
            let message = (self.describe)(error.as_ref());
            self.printer.err(message).await;
        }
        self.report.record(kind, &outcome);
        if outcome.ends_subtree() {
            self.report.cancel_subtree(subtree);
        }
        if matches!(outcome, Outcome::Failed { .. }) && !self.options.continue_on_error {
            self.report.stopped = true;
            self.stop();
        }
    }

    /// Counts a job that will never run as cancelled, with its subtree.
    fn cancel(&mut self, job: Job) {
        let Job {
            kind,
            label,
            size,
            subtree,
            work: _,
        } = job;
        let id = JobId(self.next_id);
        self.next_id += 1;
        self.progress.event(&ProgressEvent::Queued {
            job: id,
            kind,
            label: &label,
            size,
        });
        self.cancelled(id, kind, &label, subtree);
    }

    fn cancelled(&mut self, id: JobId, kind: JobKind, label: &str, subtree: Subtree) {
        self.progress.event(&ProgressEvent::Finished {
            job: id,
            kind,
            label,
            outcome: &Outcome::Cancelled,
        });
        self.report.record(kind, &Outcome::Cancelled);
        self.report.cancel_subtree(subtree);
    }

    /// Stops scheduling: drops the source, cancels the queued jobs and aborts the running ones,
    /// whose ends [`Self::execute`] still awaits.
    fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        self.source = None;
        for kind in [JobKind::Metadata, JobKind::Media] {
            let queued = std::mem::take(self.pending.get_mut(kind));
            for job in queued {
                self.cancelled(job.id, job.kind, &job.label, job.subtree);
            }
        }
        for follow_ups in std::mem::take(&mut self.follow_ups) {
            follow_ups.for_each(|job| self.cancel(job));
        }
        self.tasks.abort_all();
    }
}

/// The next job of `source`, `None` when it is exhausted or gone. Cancel safe: the source keeps
/// its state when this future is dropped.
async fn next_job<S: Stream<Item = Job>>(source: &mut Option<Pin<Box<S>>>) -> Option<Job> {
    match source {
        Some(source) => source.next().await,
        None => None,
    }
}

/// A job's task panicked (a bug); the panic message is on stderr already.
#[derive(Debug)]
struct JobPanicked {
    label: String,
}

impl Display for JobPanicked {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "internal error while transferring '{}'", self.label)
    }
}

impl Error for JobPanicked {}
