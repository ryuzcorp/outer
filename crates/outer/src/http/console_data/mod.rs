//! Console data panels: D1, R2 and Durable Objects (spec §13.1 "Later").
//!
//! Routes live under `/api/console/apps/{slug}/data/…` with the same guard
//! as the other console APIs: `__Host-outer_sid` session,
//! `Sec-Fetch-Site: same-origin`, and an owner-scoped 404 for apps the
//! caller can't see. Owner-only in v1.
//!
//! Isolation: every panel takes only `(app slug, scoped credentials for
//! `fleets/{slug}/`)`, so a cross-app path is unconstructable. Database and
//! bucket names are validated against the app's declared bindings; a typo'd
//! name opens an empty namespace in *this* fleet, never another app's data
//! (see `deploy::migrate` for the same argument).
//!
//! Seams: this agent owns `mod.rs` + `d1.rs`. The R2 + Durable Objects agent
//! adds `r2.rs` + `durable.rs` and their own `pub mod` lines + routes below.

pub mod d1;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::json;

/// Routes for every data panel. The R2/DO agent merges their routers here.
pub fn routes() -> Router<super::AppState> {
    d1::routes()
}

/// A console-data error: `{error, message, user_message}` with
/// `Cache-Control: no-store`, matching the console-actions contract. The UI
/// shows `user_message` when present, else `message`.
pub(crate) fn data_error(
    status: StatusCode,
    error: &str,
    message: impl Into<String>,
    user_message: Option<String>,
) -> Response {
    let message = message.into();
    let user_message = user_message.unwrap_or_else(|| message.clone());
    (
        status,
        [
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
            ("content-type", "application/json"),
        ],
        json!({"error": error, "message": message, "user_message": user_message}).to_string(),
    )
        .into_response()
}

/// Success JSON with `Cache-Control: no-store`.
pub(crate) fn data_json(status: StatusCode, value: serde_json::Value) -> Response {
    (
        status,
        [
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
            ("content-type", "application/json"),
        ],
        value.to_string(),
    )
        .into_response()
}
