//! Outer: a personal software forge for AI agents.
//!
//! The library holds the platform's modules; `src/main.rs` is the `outer`
//! binary that boots them.

pub mod accounts;
pub mod alerts;
pub mod apps;
pub mod auth;
pub mod browser;
pub mod cloudflare;
pub mod collaborators;
pub mod config;
pub mod cron;
pub mod d1_backup;
pub mod db;
pub mod deploy;
pub mod fleet;
pub mod git;
pub mod http;
pub mod look_tokens;
pub mod mcp;
pub mod moderation;
pub mod observe;
pub mod ops;
pub mod secrets;
pub mod store;
pub mod templates;
