//! tergi: a terminal git client inspired by Sublime Merge.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod app;
pub mod config;
pub mod git;
pub mod highlight;
pub mod terminal;
pub mod theme;
pub mod ui;
pub mod util;
pub mod watch;
