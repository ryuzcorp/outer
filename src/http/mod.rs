//! HTTP surface. M0 serves only the operator probes; MCP, OAuth and the app
//! proxy mount here in later milestones.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use sqlx::SqlitePool;

use crate::store::snapshot::StateSync;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub sync: StateSync,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .with_state(state)
}

/// Liveness: the process answers.
async fn health() -> &'static str {
    "ok"
}

/// Readiness: 200 only when every dependency answers, else 503 naming the
/// failing checks. The snapshot status is reported but never fails the gate:
/// a slow bucket must not take the platform out of rotation.
async fn ready(State(state): State<AppState>) -> Response {
    let mut failing = Vec::new();
    if let Err(err) = sqlx::query("SELECT 1").execute(&state.pool).await {
        failing.push(format!("db: {err}"));
    }
    let snapshot = state.sync.snapshot_status();
    if failing.is_empty() {
        Json(json!({ "ok": true, "snapshot": snapshot })).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ok": false, "failing": failing, "snapshot": snapshot })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn ready_reports_a_closed_database() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let app = router(AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
        });

        let ok = app
            .clone()
            .oneshot(Request::get("/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);

        pool.close().await;
        let down = app
            .oneshot(Request::get("/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(down.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
