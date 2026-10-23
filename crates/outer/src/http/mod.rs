//! HTTP surface: the platform host (operator probes now; MCP and OAuth
//! later) and every app host through [`proxy`].

pub mod proxy;
pub mod tls;

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

/// The whole HTTP surface: requests for the platform host (or with no
/// `Host`) reach [`router`]; every other host is an app and goes to
/// [`proxy::handle`], which answers 404 for hosts that aren't apps.
pub fn app(platform: AppState, apps: proxy::ProxyState, platform_host: String) -> Router {
    let platform = router(platform);
    Router::new().fallback(move |req: axum::extract::Request| {
        let platform = platform.clone();
        let apps = apps.clone();
        let platform_host = platform_host.clone();
        async move {
            let host = proxy::request_host(&req)
                .map(|h| h.split(':').next().unwrap_or(&h).to_ascii_lowercase());
            match host {
                Some(host) if host != platform_host => proxy::handle(apps, req).await,
                _ => {
                    use tower::ServiceExt as _;
                    match platform.oneshot(req).await {
                        Ok(res) => res,
                        Err(never) => match never {},
                    }
                }
            }
        }
    })
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
