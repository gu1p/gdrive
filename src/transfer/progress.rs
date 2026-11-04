//! Progress events of the transfer scheduler ([`super::scheduler`]), for a progress display
//! (milestone 2, item 5 of docs/triage/README.md: event driven, per file and aggregate, confirmed
//! bytes). The scheduler reports every job's life to one [`ProgressSink`]; jobs report their
//! confirmed bytes through their [`JobProgress`]. The default sink, [`NoProgress`], ignores
//! everything, so a progress display only adds a sink: nothing else changes.
//!
//! The events of one job, in order: [`ProgressEvent::Queued`], then [`ProgressEvent::Started`]
//! (not for a job that was decided without running, or cancelled while queued), any number of
//! [`ProgressEvent::Bytes`], and one [`ProgressEvent::Finished`]. [`ProgressEvent::SourceDone`]
//! comes once, when no further job will be queued from the source (the walk ended): from then on
//! the totals are final, apart from follow-up jobs (an upload's folder releasing its children).
//! A run whose totals are known before its jobs are (a local tree, walked first) says so with
//! [`ProgressEvent::Planned`], before the first job.
//!
//! [`render`] draws the events on stderr; [`terminal`] decides whether it does (`--progress`,
//! `--no-progress`, `GDRIVE_PROGRESS`, a terminal on stderr) and keeps other output above the
//! bars.

pub(crate) mod render;
pub(crate) mod terminal;

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

pub(crate) use render::ProgressDisplay;
pub(crate) use terminal::{eprintln, println, suspend, Mode};

use super::scheduler::{JobKind, Outcome};

/// Identifies one job of a scheduler run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct JobId(pub(crate) u64);

/// What happened to a job. Borrowed: a sink copies what it keeps.
#[derive(Debug)]
pub(crate) enum ProgressEvent<'a> {
    /// The run will transfer `files` files of `bytes` bytes in all: the totals, known before
    /// the jobs are.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the recursive upload sends it from the next commit on"
        )
    )]
    Planned { files: u64, bytes: u64 },
    /// The job entered the scheduler's queue. `size` is the content size when known.
    Queued {
        job: JobId,
        kind: JobKind,
        label: &'a str,
        size: Option<u64>,
    },
    /// The job got a slot and runs.
    Started { job: JobId },
    /// `bytes` more of the job's content are confirmed: written to disk for a download,
    /// acknowledged by Drive for an upload.
    Bytes { job: JobId, bytes: u64 },
    /// The job ended with `outcome`.
    Finished {
        job: JobId,
        kind: JobKind,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "the display knows its jobs by id; for sinks that keep no record of them"
            )
        )]
        label: &'a str,
        outcome: &'a Outcome,
    },
    /// The source of jobs is exhausted.
    SourceDone,
}

/// Receives the [`ProgressEvent`]s of a run. Called from the scheduler's task and from the jobs'
/// tasks, so it must be cheap and must not block: a display renders from its own state on its
/// own schedule.
pub(crate) trait ProgressSink: Send + Sync {
    fn event(&self, event: &ProgressEvent<'_>) {
        let _ = event;
    }
}

/// The sink that ignores every event.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NoProgress;

impl ProgressSink for NoProgress {}

/// A job's handle to report the bytes it confirmed.
#[derive(Clone)]
pub(crate) struct JobProgress {
    job: JobId,
    sink: Arc<dyn ProgressSink>,
}

impl JobProgress {
    #[must_use]
    pub(crate) fn new(job: JobId, sink: Arc<dyn ProgressSink>) -> Self {
        JobProgress { job, sink }
    }

    /// Reports `bytes` more confirmed bytes of the job.
    pub(crate) fn bytes(&self, bytes: u64) {
        if bytes > 0 {
            self.sink.event(&ProgressEvent::Bytes {
                job: self.job,
                bytes,
            });
        }
    }
}

/// Turns the running total that an uploader reports (the bytes Drive holds) into the increments
/// of a [`JobProgress`]: only what is past the largest total so far is new, so a byte is counted
/// once, also when a session starts over.
pub(crate) struct ConfirmedTotal {
    progress: JobProgress,
    total: AtomicU64,
}

impl ConfirmedTotal {
    #[must_use]
    pub(crate) fn new(progress: JobProgress) -> Self {
        ConfirmedTotal {
            progress,
            total: AtomicU64::new(0),
        }
    }

    /// Drive holds `total` bytes of the content.
    pub(crate) fn held(&self, total: u64) {
        let before = self.total.fetch_max(total, Ordering::SeqCst);
        self.progress.bytes(total.saturating_sub(before));
    }

    /// The largest total reported.
    #[must_use]
    pub(crate) fn total(&self) -> u64 {
        self.total.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{ConfirmedTotal, JobId, JobProgress, ProgressEvent, ProgressSink};

    #[derive(Default)]
    struct Bytes(Mutex<Vec<u64>>);

    impl ProgressSink for Bytes {
        fn event(&self, event: &ProgressEvent<'_>) {
            if let ProgressEvent::Bytes { bytes, .. } = event {
                self.0.lock().unwrap().push(*bytes);
            }
        }
    }

    #[test]
    fn a_confirmed_total_counts_each_byte_once() {
        let sink = Arc::new(Bytes::default());
        let confirmed = ConfirmedTotal::new(JobProgress::new(JobId(0), sink.clone()));

        // A session that starts over reports 0 and then the same bytes again.
        for total in [0, 256, 512, 512, 0, 256, 768] {
            confirmed.held(total);
        }

        assert_eq!(*sink.0.lock().unwrap(), [256, 256, 256]);
        assert_eq!(confirmed.total(), 768);
    }
}
