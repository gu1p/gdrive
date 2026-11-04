//! The transfer engine of milestone 2 (docs/triage/README.md): bounded, incremental work on Drive
//! trees.
//!
//! - [`walk`]: an incremental, breadth-first listing of a Drive folder subtree as a stream.
//! - [`scheduler`]: bounded concurrent jobs with typed outcomes, one printer and a summary.
//! - [`progress`]: the events a progress display receives from the scheduler.
//! - [`upload`]: gdrive's own uploader, resumable sessions and multipart requests.
//! - [`relay`]: copies a file of one account into another account, streaming.

#[cfg(test)]
pub(crate) mod fake_drive;
pub(crate) mod progress;
pub(crate) mod relay;
pub(crate) mod scheduler;
pub(crate) mod upload;
pub(crate) mod walk;
