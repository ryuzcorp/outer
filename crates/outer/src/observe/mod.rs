//! The `observe` surface: per-app usage metrics for the console view.
//!
//! The console branch adds the HTTP routes; this module owns the data:
//! recording in the proxy, the flush to SQLite, retention, and reads.
pub mod logs;
pub mod metrics;
