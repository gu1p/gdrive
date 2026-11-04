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

use std::sync::Arc;

use super::scheduler::{JobKind, Outcome};

/// Identifies one job of a scheduler run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct JobId(pub(crate) u64);

/// What happened to a job. Borrowed: a sink copies what it keeps.
#[derive(Debug)]
// Only the tests read the fields until the progress display of milestone 2 item 5 does.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "read by the progress display, which is not written yet"
    )
)]
pub(crate) enum ProgressEvent<'a> {
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
