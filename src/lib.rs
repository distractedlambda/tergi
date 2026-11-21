//! Tergi: yet another terminal Git client.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod app;
pub mod config;
pub mod git;
pub mod highlight;
pub mod state;
pub mod terminal;
pub mod theme;
pub mod ui;
pub mod util;
pub mod watch;
