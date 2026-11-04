//! The transfer engine of milestone 2 (docs/triage/README.md): bounded, incremental work on Drive
//! trees.
//!
//! - [`walk`]: an incremental, breadth-first listing of a Drive folder subtree as a stream.
//! - [`scheduler`]: bounded concurrent jobs with typed outcomes, one printer and a summary.
//! - [`progress`]: the events a progress display receives from the scheduler.

// The recursive download and upload use the scheduler from the next commits on.
#![allow(
    dead_code,
    reason = "the recursive download and upload are not wired to the scheduler yet"
)]

#[cfg(test)]
pub(crate) mod fake_drive;
pub(crate) mod progress;
pub(crate) mod scheduler;
pub(crate) mod walk;
