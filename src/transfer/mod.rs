//! The transfer engine of milestone 2 (docs/triage/README.md): bounded, incremental work on Drive
//! trees.
//!
//! - [`walk`]: an incremental, breadth-first listing of a Drive folder subtree as a stream.

// The scheduler workstream consumes the whole walk API; until it lands, the recursive download
// (`common::file_tree_drive`) uses only part of it and the rest is exercised by the tests.
#![cfg_attr(
    not(test),
    allow(dead_code, reason = "the scheduler will use the rest of the walk API")
)]

#[cfg(test)]
pub(crate) mod fake_drive;
pub(crate) mod walk;
