//! Console Admin tool telemetry (Wave 4, migration 0015): the efficiency
//! numbers behind the agent loop — per-tool calls, p50/p95 latency, error
//! rate and average response bytes over 24h/7d, plus calls-per-commit (how
//! many tool calls precede each `commit` in a session).
//!
//! One read-only route in its own file, so the Console-shell rework never
//! touches it: `GET /api/console/admin/telemetry` answers both windows at
//! once. The gate reuses the existing admin check (`is_admin` over the
//! console session); non-admins get 404, not 403, so the endpoint does not
//! confirm its own existence.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::json;

use super::console::{guard, nostore_json, ok_json};
use super::AppState;
use crate::mcp::telemetry;

/// The admin telemetry route, merged by [`super::console::router`].
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/console/admin/telemetry", get(api_telemetry))
}

fn telemetry_json(summary: &telemetry::TelemetrySummary) -> serde_json::Value {
    json!({
        "tools": summary.tools.iter().map(|t| json!({
            "tool": t.tool,
            "calls": t.calls,
            "p50_ms": t.p50_ms,
            "p95_ms": t.p95_ms,
            "error_rate": t.error_rate,
            "avg_bytes": t.avg_bytes,
        })).collect::<Vec<_>>(),
        "calls_per_commit": summary.calls_per_commit,
        "sessions_with_commits": summary.sessions_with_commits,
        "commits": summary.commits,
        "calls": summary.calls,
    })
}

/// `GET /api/console/admin/telemetry`: per-tool aggregates plus the
/// calls-per-commit efficiency metric, for the last 24h and the last 7d.
/// Reads are not audited.
async fn api_telemetry(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    if !crate::auth::is_admin(&mcp.auth.config, &account.email) {
        return nostore_json(StatusCode::NOT_FOUND, "not_found", "not found");
    }
    let now = crate::apps::now_ms();
    let (day, week) = match tokio::join!(
        telemetry::summary(&mcp.pool, 1, now),
        telemetry::summary(&mcp.pool, 7, now),
    ) {
        (Ok(day), Ok(week)) => (day, week),
        _ => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "tool telemetry is unavailable right now",
            );
        }
    };
    ok_json(json!({
        "day": telemetry_json(&day),
        "week": telemetry_json(&week),
        "generated_at": now,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::create_session;

    async fn app_state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        std::mem::forget(dir);
        let (auth_cfg, _) = crate::auth::test_support::test_config();
        let auth = crate::auth::AuthState {
            pool: pool.clone(),
            config: auth_cfg,
        };
        let mcp = crate::mcp::McpState::for_tests(pool, auth.clone());
        AppState {
            pool: mcp.pool.clone(),
            sync: crate::store::snapshot::StateSync::new("test"),
            auth: auth.clone(),
            mcp,
            site: crate::http::site::SiteDir::new(std::path::Path::new("/nonexistent")),
        }
    }

    async fn headers_for(state: &AppState, email: &str) -> HeaderMap {
        // The test config admins `owner@example.com` (see
        // `test_support::test_config`).
        let account_id = crate::apps::ensure_account(&state.pool, email)
            .await
            .unwrap();
        let session = create_session(&state.pool, &account_id).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            format!("__Host-outer_sid={session}").parse().unwrap(),
        );
        headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn telemetry_endpoint_is_404_for_non_admins() {
        let state = app_state().await;
        let headers = headers_for(&state, "ana@example.com").await;
        let resp = api_telemetry(State(state), headers).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn telemetry_endpoint_serves_admins() {
        let state = app_state().await;
        let headers = headers_for(&state, "owner@example.com").await;
        let resp = api_telemetry(State(state), headers).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
