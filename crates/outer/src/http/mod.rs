//! HTTP surface: the platform host (operator probes now; MCP and OAuth
//! later) and every app host through [`proxy`].

pub mod badge;
pub mod client_ip;
pub mod console;
pub mod console_account;
pub mod console_actions;
pub mod console_admin;
pub mod console_admin_apps;
pub mod console_admin_code;
pub mod console_admin_telemetry;
pub mod console_billing;
pub mod console_code;
pub mod console_collaborators;
pub mod console_data;
pub mod console_domains;
pub mod console_export;
pub mod console_observe;
pub mod keys;
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
    // The console API and sign-in are explicit routes, so they win over the
    // site fallback below (which serves the static `/console` pages): even
    // if a build ever emitted `console/sign-in/index.html`, the Rust
    // handler still answers `/console/sign-in`.
    let console = console::router(state.clone());
    // Data panels (D1, R2, Durable Objects): explicit Axum routes like the
    // console API above, so they win over the site fallback below.
    let console_data = console_data::routes();
    // `/keys` (D3): the web flow scripts use to mint an API key. An explicit
    // route, like the console above, so it wins over the site fallback.
    let keys = keys::routes();
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        // The MCP endpoint lives on the platform host only: this router
        // never serves app hosts. GET/DELETE on /mcp answer 405 from the
        // method router; the handler refuses bodies over 32 MiB (§8.1).
        .route("/mcp", post(crate::mcp::mcp_handler))
        .merge(console)
        .merge(console_data)
        .merge(keys)
        .fallback(platform_fallback)
        .with_state(state)
}

/// OpenAI's app-submission domain check path: the configured token serves
/// here verbatim, as plain text.
const OPENAI_APPS_CHALLENGE_PATH: &str = "/.well-known/openai-apps-challenge";

/// Platform-host fallback, in order: the built web site (static files
/// from `OUTER_SITE_DIR`), then the auth router (OAuth, well-known
/// discovery) with its own 404 for anything else.
async fn platform_fallback(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let headers = req.headers().clone();
    // OAuth and the exact well-known discovery paths belong to the auth
    // router even when the site has no file for them: the site's 404 must
    // not swallow them. Every other `/.well-known/*` path is the site's
    // (Blume ships `ai-catalog.json`, `ard.json`, `agent-skills/…` there).
    let auth_owned = crate::auth::owns_path(&path);
    if !auth_owned {
        // OpenAI's app-submission domain check: the portal-issued token,
        // served verbatim. Unset leaves the path to the site and its 404.
        if path == OPENAI_APPS_CHALLENGE_PATH && (method == "GET" || method == "HEAD") {
            if let Some(challenge) = &state.auth.config.openai_apps_challenge {
                // HEAD answers the same headers and body length; hyper sends
                // no body for it.
                return (
                    StatusCode::OK,
                    [
                        ("content-type", "text/plain; charset=utf-8"),
                        ("cache-control", "no-store"),
                    ],
                    challenge.clone(),
                )
                    .into_response();
            }
        }
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
/// failing checks. The snapshot status rides along, and trips the gate only
/// after [`READY_LINEAGE_REFUSAL_LIMIT`] consecutive lineage refusals: by
/// then the host is serving writes it cannot back up, and monitoring must
/// see it as down. A slow bucket alone never takes the platform out of
/// rotation.
async fn ready(State(state): State<AppState>) -> Response {
    let mut failing = Vec::new();
    if let Err(err) = sqlx::query("SELECT 1").execute(&state.pool).await {
        failing.push(format!("db: {err}"));
    }
    let snapshot = state.sync.snapshot_status();
    if snapshot.consecutive_lineage_refusals >= crate::store::snapshot::READY_LINEAGE_REFUSAL_LIMIT
    {
        failing.push(format!(
            "snapshot: refusing uploads ({} consecutive, backups stopped): {}",
            snapshot.consecutive_lineage_refusals,
            snapshot.last_error.as_deref().unwrap_or("lineage refused"),
        ));
    }
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

    /// One request through the whole platform router.
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

    /// The platform router over a temp database and site dir, carrying an
    /// optional OpenAI challenge token in the auth config.
    async fn test_router(
        dir: &std::path::Path,
        site_root: &std::path::Path,
        challenge: Option<&str>,
    ) -> Router {
        let pool = crate::db::open(&dir.join("outer.sqlite")).await.unwrap();
        let mut auth = crate::auth::test_support::test_state(pool.clone());
        auth.config.openai_apps_challenge = challenge.map(str::to_string);
        router(AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
            auth: auth.clone(),
            mcp: crate::mcp::McpState::for_tests(pool.clone(), auth),
            site: site::SiteDir::new(site_root),
        })
    }

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

    /// `/ready` fails after sustained lineage refusals (backups stopped),
    /// naming the refusal — and recovers when an upload lands again.
    #[tokio::test]
    async fn ready_fails_after_sustained_lineage_refusals() {
        use crate::store::snapshot::{SnapshotConfig, READY_LINEAGE_REFUSAL_LIMIT};
        use crate::store::Store;

        // A bucket holding another database's snapshot.
        let bucket_dir = tempfile::tempdir().unwrap();
        let bucket_db = bucket_dir.path().join("outer.sqlite");
        let url = format!("sqlite:{0}?mode=rwc", bucket_db.display());
        let bucket_pool = sqlx::SqlitePool::connect(&url).await.unwrap();
        sqlx::query("CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT)")
            .execute(&bucket_pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kv (k, v) VALUES ('a', '1')")
            .execute(&bucket_pool)
            .await
            .unwrap();
        let bucket_store = Store::memory();
        let bucket_cfg = SnapshotConfig {
            db_path: bucket_db,
            key_prefix: "outer/state".into(),
            host_id: "host-1".into(),
        };
        let seeder = StateSync::new("host-1");
        seeder.claim(&bucket_store, &bucket_cfg).await.unwrap();
        seeder
            .snapshot_now(&bucket_pool, &bucket_store, &bucket_cfg)
            .await
            .unwrap();
        bucket_pool.close().await;

        // This host: same bucket, different database, no sidecar.
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query("CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        let sync = StateSync::new("host-1");
        let auth = crate::auth::test_support::test_state(pool.clone());
        let app = router(AppState {
            pool: pool.clone(),
            sync: sync.clone(),
            auth: auth.clone(),
            mcp: crate::mcp::McpState::for_tests(pool.clone(), auth),
            site: site::SiteDir::new(std::path::Path::new("/nonexistent")),
        });
        let cfg = SnapshotConfig {
            db_path: dir.path().join("outer.sqlite"),
            key_prefix: "outer/state".into(),
            host_id: "host-1".into(),
        };
        assert_eq!(get(&app, "/ready", None).await.0, StatusCode::OK);
        for _ in 0..READY_LINEAGE_REFUSAL_LIMIT {
            sync.snapshot_now(&pool, &bucket_store, &cfg)
                .await
                .expect_err("wrong bucket refuses");
        }
        let (status, _, body) = get(&app, "/ready", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body.contains("refusing uploads") && body.contains("refusing state snapshot upload"),
            "ready names the refusal: {body}"
        );
        // A landed upload clears the tripwire.
        let home = Store::memory();
        sync.claim(&home, &cfg).await.unwrap();
        sync.snapshot_now(&pool, &home, &cfg).await.unwrap();
        assert_eq!(get(&app, "/ready", None).await.0, StatusCode::OK);
        pool.close().await;
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

    /// `/.well-known/*`: only the auth router's exact discovery paths are
    /// auth-owned; everything else is a site file, and the OpenAI challenge
    /// token (when configured) wins over both.
    #[tokio::test]
    async fn well_known_is_site_except_auth_paths_and_openai_challenge() {
        let site_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(site_root.path().join(".well-known/agent-skills")).unwrap();
        std::fs::write(
            site_root.path().join(".well-known/ai-catalog.json"),
            "{\"apps\":[]}",
        )
        .unwrap();
        std::fs::write(
            site_root.path().join(".well-known/agent-skills/index.json"),
            "[]",
        )
        .unwrap();
        // A site file at an auth-owned path must lose to the auth router.
        std::fs::write(
            site_root
                .path()
                .join(".well-known/oauth-authorization-server"),
            "from-site",
        )
        .unwrap();
        std::fs::write(site_root.path().join("404.html"), "<h1>Missing</h1>").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(dir.path(), site_root.path(), None).await;

        // Blume's own well-known files are served, nested paths included.
        let (status, headers, body) = get(&app, "/.well-known/ai-catalog.json", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[axum::http::header::CONTENT_TYPE],
            "application/json"
        );
        assert_eq!(body, "{\"apps\":[]}");
        let (status, _, body) = get(&app, "/.well-known/agent-skills/index.json", None).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));

        // The auth router wins for its exact discovery paths even when the
        // site dir holds a file with the same name.
        let (status, headers, body) =
            get(&app, "/.well-known/oauth-authorization-server", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[axum::http::header::CONTENT_TYPE],
            "application/json"
        );
        assert!(body.contains("\"issuer\""), "{body}");
        let (status, _, _) = get(&app, "/.well-known/oauth-protected-resource", None).await;
        assert_eq!(status, StatusCode::OK);

        // Unknown well-known paths are the site's, so its 404 answers.
        let (status, _, body) = get(&app, "/.well-known/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "<h1>Missing</h1>");

        // Unset challenge: no handler, so the path falls through to the
        // site's 404 too.
        let (status, _, _) = get(&app, "/.well-known/openai-apps-challenge", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Set: the exact token, as plain text, never cached.
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(
            dir.path(),
            site_root.path(),
            Some("openai-apps-challenge-token-abc123"),
        )
        .await;
        let (status, headers, body) = get(&app, "/.well-known/openai-apps-challenge", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[axum::http::header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(headers[axum::http::header::CACHE_CONTROL], "no-store");
        assert_eq!(body, "openai-apps-challenge-token-abc123");

        // HEAD gets the same status and headers (hyper sends no body).
        let res = app
            .clone()
            .oneshot(
                Request::head("/.well-known/openai-apps-challenge")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()[axum::http::header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(res.headers()[axum::http::header::CACHE_CONTROL], "no-store");
    }
}
