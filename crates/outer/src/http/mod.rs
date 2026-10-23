//! HTTP surface: the platform host (operator probes now; MCP and OAuth
//! later) and every app host through [`proxy`].

pub mod client_ip;
pub mod proxy;
pub mod site;
pub mod tls;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use sqlx::SqlitePool;

use crate::store::snapshot::StateSync;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub sync: StateSync,
    pub auth: crate::auth::AuthState,
    pub mcp: crate::mcp::McpState,
    pub site: site::SiteDir,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        // The MCP endpoint lives on the platform host only: this router
        // never serves app hosts. GET/DELETE on /mcp answer 405 from the
        // method router; the handler refuses bodies over 32 MiB (§8.1).
        .route("/mcp", post(crate::mcp::mcp_handler))
        .fallback(platform_fallback)
        .with_state(state)
}

/// Platform-host fallback, in order: the built docs site (static files
/// from `OUTER_SITE_DIR`), then the auth router (OAuth, well-known
/// discovery) with its own 404 for anything else.
async fn platform_fallback(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let headers = req.headers().clone();
    // OAuth and well-known discovery belong to the auth router even when
    // the site has no file for them: the site's 404 must not swallow them.
    let auth_owned =
        path == "/oauth" || path.starts_with("/oauth/") || path.starts_with("/.well-known/");
    if !auth_owned {
        if let Some(res) = state
            .site
            .serve_request(&method, &path, query.as_deref(), &headers)
            .await
        {
            return res;
        }
    }
    use tower::ServiceExt as _;
    match crate::auth::router(state.auth.clone()).oneshot(req).await {
        Ok(res) => res,
        Err(never) => match never {},
    }
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
        let auth = crate::auth::test_support::test_state(pool.clone());
        let app = router(AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
            auth: auth.clone(),
            mcp: crate::mcp::McpState::for_tests(pool.clone(), auth),
            site: site::SiteDir::new(std::path::Path::new("/nonexistent")),
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

    /// The platform fallback order: Outer's own routes first, then the
    /// built site, then the auth router (OAuth, well-known) with its 404.
    #[tokio::test]
    async fn platform_fallback_serves_site_then_auth() {
        let site_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(site_root.path().join("connect")).unwrap();
        std::fs::write(site_root.path().join("index.html"), "<h1>Outer</h1>").unwrap();
        std::fs::write(
            site_root.path().join("connect/index.html"),
            "<h1>Connect</h1>",
        )
        .unwrap();
        std::fs::write(site_root.path().join("connect.md"), "# Connect\n").unwrap();
        std::fs::write(site_root.path().join("404.html"), "<h1>Missing</h1>").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let auth = crate::auth::test_support::test_state(pool.clone());
        let app = router(AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
            auth: auth.clone(),
            mcp: crate::mcp::McpState::for_tests(pool.clone(), auth),
            site: site::SiteDir::new(site_root.path()),
        });
        async fn get(
            app: &Router,
            path: &str,
            accept: Option<&str>,
        ) -> (StatusCode, axum::http::HeaderMap, String) {
            let mut req = Request::get(path);
            if let Some(accept) = accept {
                req = req.header(axum::http::header::ACCEPT, accept);
            }
            let res = app
                .clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = res.status();
            let headers = res.headers().clone();
            let body = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, headers, String::from_utf8(body.to_vec()).unwrap())
        }

        // Outer's own routes win and never reach the site.
        let (status, _, body) = get(&app, "/health", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");

        // The site serves extensionless routes and the .md twin.
        let (status, _, body) = get(&app, "/", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "<h1>Outer</h1>");
        let (status, headers, _) = get(&app, "/connect", Some("text/markdown")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[axum::http::header::CONTENT_TYPE],
            "text/markdown; charset=utf-8"
        );

        // Trailing slashes redirect; missing pages use the site's 404.
        let res = app
            .clone()
            .oneshot(Request::get("/connect/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(res.headers()[axum::http::header::LOCATION], "/connect");
        let (status, _, body) = get(&app, "/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "<h1>Missing</h1>");

        // Unknown non-site paths still reach the auth router.
        let (status, _, _) = get(&app, "/.well-known/oauth-authorization-server", None).await;
        assert_eq!(status, StatusCode::OK);
    }
}
