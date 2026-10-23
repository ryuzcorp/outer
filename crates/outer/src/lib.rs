//! Outer: a personal software forge for AI agents.
//!
//! The library holds the platform's modules; `src/main.rs` is the `outer`
//! binary that boots them.

pub mod apps;
pub mod config;
pub mod cron;
pub mod db;
pub mod deploy;
pub mod fleet;
pub mod git;
pub mod http;
pub mod ops;
pub mod store;
pub mod templates;
