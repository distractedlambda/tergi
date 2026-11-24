//! All the integration tests, built as one test binary.
//!
//! Cargo runs test binaries one after another. Most property tests are a
//! single long test, so with a binary for each file only one core was busy
//! for most of the run. In one binary, the test harness runs them all at
//! the same time. It also means linking one binary instead of one per file.
//!
//! `autotests = false` in Cargo.toml stops Cargo from building each file
//! on its own, so a new test file only runs once it's listed here.

mod common;

mod commit_props;
mod conflict_props;
mod fmt;
mod hunk_edges;
mod log_props;
mod ops;
mod patch_props;
mod path_props;
mod rebase;
mod rebase_props;
mod rebase_stop_props;
mod refs_props;
mod staging;
mod stash_file_props;
mod stash_props;
mod ui;
mod ui_props;
mod watch_props;
