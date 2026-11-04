//! The transfer engine of milestone 2 (docs/triage/README.md): bounded, incremental work on Drive
//! trees.
//!
//! - [`walk`]: an incremental, breadth-first listing of a Drive folder subtree as a stream.
//! - [`scheduler`]: bounded concurrent jobs with typed outcomes, one printer and a summary.
//! - [`progress`]: the events a progress display receives from the scheduler.
//! - [`upload`]: gdrive's own uploader, resumable sessions and multipart requests.

#[cfg(test)]
pub(crate) mod fake_drive;
pub(crate) mod progress;
pub(crate) mod scheduler;
pub(crate) mod upload;
pub(crate) mod walk;
