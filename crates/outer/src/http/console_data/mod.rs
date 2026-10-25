//! Console data panels: browse app storage (D1, R2, Durable Objects) from
//! the web console (spec §13.1 "Later (data panels)", DATA-PANELS.md).
//!
//! Routes live under `/api/console/apps/{slug}/data/…` with the same guard
//! as the other console APIs: session cookie, `Sec-Fetch-Site:
//! same-origin`, and an owner-scoped 404 for foreign or unknown slugs.
//! Reads use server-held scoped credentials (`fleets/{slug}/`), never
//! exposed to the browser; writes go through celld (never plain S3 PUT, so
//! the stored record is what a worker would write) and are recorded in
//! `agent_action` with both ids NULL (client "Console").
//!
//! Each panel lives in its own file (`r2.rs`, `durable.rs`, and the D1
//! agent's `d1.rs`); this file only declares the modules, merges the
//! routers, and holds the helpers they share, so panel changes never
//! conflict with each other.

pub mod durable;
pub mod r2;

use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;

use super::console::{guard, nostore_json, not_found, ok_json, owned_app};

pub fn router(state: super::AppState) -> Router<super::AppState> {
    Router::new()
        .merge(r2::router())
        .merge(durable::router())
        .with_state(state)
}

/// The HTTP state, re-exported under the name the panel modules see it as:
/// from `r2.rs`/`durable.rs`, `super` is this module, so `super::AppState`
/// would not resolve without this alias.
pub(crate) type AppState = super::AppState;

/// Session + same-origin + owner-scoped app lookup. Foreign or unknown
/// slugs answer 404 exactly like a missing one (spec §8.2).
// `Response` is 128 bytes, so this trips `result_large_err`; boxing would
// only add an allocation per request to satisfy the lint. The sibling
// `console::guard` boxes because it nests inside other boxed results —
// this one returns directly to handlers, so the plain `Response` stays.
#[allow(clippy::result_large_err)]
pub(crate) async fn console_app(
    state: &super::AppState,
    headers: &HeaderMap,
    slug: &str,
) -> Result<(crate::auth::AccountRow, crate::apps::App), Response> {
    let mcp = &state.mcp;
    let account = guard(&mcp.auth, headers).await.map_err(|b| *b)?;
    let app = owned_app(mcp, &account, slug).await.map_err(|_| {
        nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "storage is unavailable right now",
        )
    })?;
    let Some(app) = app else {
        return Err(not_found());
    };
    Ok((account, app))
}

/// The deploy work dir the built wrangler config is read from at request
/// time: `dist/wrangler.json`, then `wrangler.jsonc`, then `wrangler.json`
/// (same discovery and symlink refusal as the deploy pipeline).
pub(crate) fn deploy_settings(
    state: &super::AppState,
) -> anyhow::Result<&crate::deploy::pipeline::DeploySettings> {
    state
        .mcp
        .deploys
        .deploy_settings()
        .ok_or_else(|| anyhow::anyhow!("storage panels are unavailable in this build"))
}

/// The authoritative deployed manifest, read from the bucket at request
/// time: `fleets/{slug}/deploy/current.json` names the live deploy's
/// `prefix`, and `fleets/{slug}/{prefix}/manifest.json` carries celld's own
/// record of it — `raw_metadata.bindings` (R2/D1/…) plus top-level
/// `do_classes`/`sqlite_classes`. The local `work/{slug}/dist/wrangler.json`
/// is only a build artifact (wiped by sleeps and failed builds), so it is
/// just the fallback when the manifest is missing.
///
/// Only binding shapes are parsed out (see the panel parsers); `vars` and
/// everything else in the manifest are never read, logged or returned.
pub(crate) async fn deploy_manifest(
    state: &super::AppState,
    slug: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let prefix = format!("fleets/{slug}/");
    let current_key = format!("{prefix}deploy/current.json");
    let Some(current_bytes) = state.mcp.store.get(&current_key).await? else {
        return Ok(None);
    };
    let current: serde_json::Value = match serde_json::from_slice(&current_bytes) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(slug, error = %format!("{e:#}"), "deploy current.json does not parse; falling back to the local config");
            return Ok(None);
        }
    };
    let Some(deploy_prefix) = current.get("prefix").and_then(|v| v.as_str()) else {
        tracing::warn!(
            slug,
            "deploy current.json names no prefix; falling back to the local config"
        );
        return Ok(None);
    };
    if deploy_prefix.is_empty() || deploy_prefix.starts_with('/') || deploy_prefix.contains("..") {
        anyhow::bail!("deploy current.json names an unsafe prefix");
    }
    let manifest_key = format!("{prefix}{deploy_prefix}/manifest.json");
    let Some(manifest_bytes) = state.mcp.store.get(&manifest_key).await? else {
        tracing::warn!(
            slug,
            "deploy manifest is missing; falling back to the local config"
        );
        return Ok(None);
    };
    let manifest: serde_json::Value = match serde_json::from_slice(&manifest_bytes) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(slug, error = %format!("{e:#}"), "deploy manifest does not parse; falling back to the local config");
            return Ok(None);
        }
    };
    Ok(Some(manifest))
}

/// The deploy work dir the built wrangler config is read from at request
/// time: `dist/wrangler.json`, then `wrangler.jsonc`, then `wrangler.json`
/// (same discovery and symlink refusal as the deploy pipeline). Fallback
/// only: the bucket manifest above is authoritative whenever present.
pub(crate) fn built_config_value(
    state: &super::AppState,
    slug: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let settings = deploy_settings(state)?;
    let work = settings.data_dir.join("work").join(slug);
    let Some(path) = crate::deploy::pipeline::find_deploy_config(&work)? else {
        return Ok(None);
    };
    Ok(Some(crate::deploy::pipeline::read_config_file(&path)?))
}

pub(crate) async fn scoped_s3(
    state: &super::AppState,
    slug: &str,
) -> anyhow::Result<crate::store::s3::S3Backend> {
    let settings = deploy_settings(state)?;
    let storage = &settings.storage;
    let creds = storage
        .credentials
        .for_prefix(&crate::fleet::manager::FleetStorage::fleet_prefix(slug))
        .await?;
    crate::store::s3::S3Backend::new(&crate::store::S3Config {
        endpoint: storage.endpoint.clone(),
        region: storage.region.clone(),
        bucket: storage.bucket.clone(),
        access_key_id: creds.access_key_id.clone(),
        secret_access_key: creds.secret_access_key.clone(),
        session_token: creds.session_token.clone(),
    })
}

pub(crate) fn celld_env(
    storage: &crate::fleet::manager::FleetStorage,
    creds: &crate::store::credentials::ScopedCredentials,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("AWS_ACCESS_KEY_ID".to_string(), creds.access_key_id.clone()),
        (
            "AWS_SECRET_ACCESS_KEY".to_string(),
            creds.secret_access_key.clone(),
        ),
        ("AWS_REGION".to_string(), storage.region.clone()),
        ("AWS_EC2_METADATA_DISABLED".to_string(), "true".to_string()),
        ("S3_ENDPOINT".to_string(), storage.endpoint.clone()),
    ];
    if let Some(token) = creds.session_token.as_ref() {
        env.push(("AWS_SESSION_TOKEN".to_string(), token.clone()));
    }
    env
}

pub(crate) async fn scoped_creds(
    state: &super::AppState,
    slug: &str,
) -> anyhow::Result<crate::store::credentials::ScopedCredentials> {
    let settings = deploy_settings(state)?;
    let storage = &settings.storage;
    storage
        .credentials
        .for_prefix(&crate::fleet::manager::FleetStorage::fleet_prefix(slug))
        .await
}

/// Run one `celld` storage subcommand against the fleet's own bucket.
/// `args` after the binary; `env` carries the scoped grant. Request-scoped
/// with a short timeout — no fleet wake, no sandboxing: the credential
/// prefix (`fleets/{slug}/`) is the isolation.
pub(crate) async fn run_celld(
    state: &super::AppState,
    args: &[String],
    env: &[(String, String)],
    timeout: Duration,
) -> anyhow::Result<String> {
    let celld_bin = deploy_settings(state)?.celld_bin.clone();
    let mut cmd = tokio::process::Command::new(&celld_bin);
    cmd.args(args);
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "celld {} timed out",
                args.first().map(String::as_str).unwrap_or("?")
            )
        })?
        .map_err(|e| anyhow::anyhow!("running celld: {e:#}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.lines().rev().take(5).collect::<Vec<_>>().join(" | ");
        anyhow::bail!("celld failed: {tail}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Record a console mutation in `agent_action` with both ids NULL, which the
/// action log labels client "Console" (CONSOLE-ACTIONS-CONTRACT.md). Unknown
/// or foreign apps write nothing — callers only reach here past `owned_app`,
/// so the lookup below always hits.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_console_action(
    pool: &sqlx::SqlitePool,
    app_id: &str,
    account_id: &str,
    tool: &str,
    summary: &str,
    outcome: &str,
    error_code: Option<&str>,
    duration_ms: i64,
) {
    let summary: String = summary.chars().take(2000).collect();
    if let Err(err) = sqlx::query(
        "INSERT INTO agent_action (id, app_id, user_id, consent_id, apikey_id, \
         tool, summary, outcome, error_code, duration_ms, created_at) \
         VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(app_id)
    .bind(account_id)
    .bind(tool)
    .bind(&summary)
    .bind(outcome)
    .bind(error_code)
    .bind(duration_ms)
    .bind(crate::auth::now_ms())
    .execute(pool)
    .await
    {
        tracing::warn!(tool, error = %format!("{err:#}"), "console agent_action insert failed");
    }
}

/// Error shape for data endpoints: `{error, message, user_message}` per the
/// console-actions contract. `message` and `user_message` carry the same
/// text here: every message is built to be safe to show (callers pass
/// validation failures and celld/S3 failure tails — env values and
/// credentials never enter them), and the panels surface the detail the
/// owner needs to act on.
fn data_error(status: StatusCode, error: &str, message: String) -> Response {
    (
        status,
        [
            ("cache-control", "no-store"),
            ("content-type", "application/json"),
        ],
        serde_json::json!({"error": error, "message": message, "user_message": message})
            .to_string(),
    )
        .into_response()
}

/// A 400 for a bad bucket, key, prefix or body: the caller's input, so the
/// message is safe to show.
pub(crate) fn bad_request(message: impl Into<String>) -> Response {
    data_error(StatusCode::BAD_REQUEST, "bad_request", message.into())
}

/// A 500 for a celld/S3 failure: the message carries the failure's tail
/// (never credentials — env values never enter it).
pub(crate) fn unavailable(message: impl Into<String>) -> Response {
    data_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "unavailable",
        message.into(),
    )
}

/// A 404 for a missing object or undeclared bucket/class.
pub(crate) fn missing(message: impl Into<String>) -> Response {
    data_error(StatusCode::NOT_FOUND, "not_found", message.into())
}

/// A 413 for an object or upload over the cap.
pub(crate) fn too_large(message: impl Into<String>) -> Response {
    data_error(StatusCode::PAYLOAD_TOO_LARGE, "too_large", message.into())
}

/// A 200 JSON answer with the console's `no-store` rules.
pub(crate) fn ok(body: serde_json::Value) -> Response {
    ok_json(body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::{test_config, test_pool};
    use crate::auth::AuthState;
    use crate::deploy::pipeline::{DeploySettings, Deployer};
    use crate::fleet::manager::FleetStorage;
    use crate::mcp::{Deploys, McpState};
    use crate::store::credentials::CredentialSource;
    use crate::store::snapshot::StateSync;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    const PEER: &str = "127.0.0.1:43123";

    fn peer(mut req: Request<Body>) -> Request<Body> {
        req.extensions_mut().insert(ConnectInfo(
            PEER.parse::<SocketAddr>().expect("test peer parses"),
        ));
        req
    }

    async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let res = app.clone().oneshot(peer(req)).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    fn authed(path: &str, cookie: &str) -> axum::http::request::Builder {
        Request::get(path)
            .header("sec-fetch-site", "same-origin")
            .header("cookie", cookie)
    }

    /// Live deploys with a fake `celld` and static credentials: guard and
    /// allowlist paths run fully; only S3 reads would touch the network,
    /// and no test below reaches them.
    async fn test_state(
        celld_bin: &str,
    ) -> (
        axum::Router,
        sqlx::SqlitePool,
        std::path::PathBuf,
        crate::store::Store,
    ) {
        let pool = test_pool().await;
        let (config, _) = test_config();
        let auth = AuthState {
            pool: pool.clone(),
            config,
        };
        let mut mcp = McpState::for_tests(pool.clone(), auth.clone());
        let store = mcp.store.clone();
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();
        std::mem::forget(dir);
        mcp.deploys = Deploys::live(Deployer::new(
            DeploySettings {
                data_dir: data_dir.clone(),
                celld_bin: celld_bin.to_string(),
                esbuild_bin: None,
                bun_bin: "bun".to_string(),
                storage: FleetStorage {
                    endpoint: "http://127.0.0.1:1".to_string(),
                    region: "auto".to_string(),
                    bucket: "outer-test".to_string(),
                    credentials: CredentialSource::Static {
                        access_key_id: "testaccess".to_string(),
                        secret_access_key: "testsecret".to_string(),
                    },
                },
                step_timeout: Duration::from_secs(30),
                cgroups: None,
                apps_domain: "example.com".to_string(),
                cache_purge: None,
                jev: None,
                wake: None,
                secrets_key: crate::secrets::SecretsKey::test_key(),
            },
            1,
        ));
        let missing = std::path::Path::new("/nonexistent-site");
        let app = super::super::router(super::AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
            auth,
            mcp,
            site: crate::http::site::SiteDir::new(missing),
        });
        (app, pool, data_dir, store)
    }

    async fn seed_app(pool: &sqlx::SqlitePool, owner: &str, slug: &str, now: i64) {
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(format!("acct-{owner}"))
        .bind(owner)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        let uid = 300_000 + slug.bytes().map(u32::from).sum::<u32>() % 10_000;
        let port = 18_000 + uid % 1_000;
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(format!("app-{slug}"))
        .bind(slug)
        .bind(format!("acct-{owner}"))
        .bind(uid)
        .bind(port as i64)
        .bind(port as i64 + 1)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn login(pool: &sqlx::SqlitePool, email: &str) -> String {
        let id = crate::auth::create_session(pool, &format!("acct-{email}"))
            .await
            .unwrap();
        format!("__Host-outer_sid={id}")
    }

    fn fake_celld(exit: i32) -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("celld");
        std::fs::write(&bin, format!("#!/bin/sh\nexit {exit}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::mem::forget(dir);
        bin
    }

    fn write_config(data_dir: &std::path::Path, slug: &str, value: serde_json::Value) {
        let dist = data_dir.join("work").join(slug).join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("wrangler.json"), value.to_string()).unwrap();
    }

    async fn seed_manifest(store: &crate::store::Store, slug: &str) {
        let prefix = "deploy/test/abc123";
        store
            .put(
                &format!("fleets/{slug}/deploy/current.json"),
                serde_json::json!({"script_name": slug, "version": "abc123", "prefix": prefix})
                    .to_string()
                    .into_bytes(),
            )
            .await
            .unwrap();
        // `vars` rides along like production (injected secrets included):
        // the endpoints must never return it.
        store
            .put(
                &format!("fleets/{slug}/{prefix}/manifest.json"),
                serde_json::json!({
                    "raw_metadata": {
                        "bindings": [
                            {"type": "r2_bucket", "name": "FILES", "bucket_name": "files"},
                            {"type": "r2_bucket", "name": "AVATARS", "bucket_name": "avatars"},
                            {"type": "d1", "name": "DB", "database_name": "app-db"},
                        ],
                        "vars": {"SECRET": "should-never-leak"},
                    },
                    "do_classes": ["Counter", "RateLimiter", "__D1Database"],
                    "sqlite_classes": [],
                })
                .to_string()
                .into_bytes(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn data_endpoints_need_session_and_same_origin() {
        let (app, pool, _, _) = test_state("celld").await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let cookie = login(&pool, "ana@example.com").await;
        // No session cookie: 401, even with the header.
        let (status, headers, _) = send(
            &app,
            Request::get("/api/console/apps/groceries/data/r2/buckets")
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers["cache-control"], "no-store");
        // Session but cross-site: 403.
        let (status, _, _) = send(
            &app,
            Request::get("/api/console/apps/groceries/data/r2/buckets")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn data_endpoints_hide_foreign_apps() {
        let (app, pool, _, _) = test_state("celld").await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_app(&pool, "bob@example.com", "bakery", now).await;
        let cookie = login(&pool, "ana@example.com").await;
        for path in [
            "/api/console/apps/bakery/data/r2/buckets",
            "/api/console/apps/bakery/data/do/classes",
            "/api/console/apps/missing/data/r2/buckets",
        ] {
            let (status, _, body) =
                send(&app, authed(path, &cookie).body(Body::empty()).unwrap()).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "for {path}");
            let body: serde_json::Value = body.parse().unwrap();
            assert_eq!(body["error"], "not_found");
        }
    }

    #[tokio::test]
    async fn r2_buckets_come_from_the_manifest_then_the_local_config() {
        let (app, pool, data_dir, store) = test_state("celld").await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let cookie = login(&pool, "ana@example.com").await;
        // Nothing anywhere: empty, not an error.
        let (status, headers, body) = send(
            &app,
            authed("/api/console/apps/groceries/data/r2/buckets", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["buckets"], serde_json::json!([]));
        // The local build artifact is the fallback when the manifest is
        // missing.
        write_config(
            &data_dir,
            "groceries",
            serde_json::json!({"r2_buckets": [{"binding": "LOCAL", "bucket_name": "local"}]}),
        );
        let (status, _, body) = send(
            &app,
            authed("/api/console/apps/groceries/data/r2/buckets", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(
            body["buckets"],
            serde_json::json!([{"binding": "LOCAL", "bucket": "local"}])
        );
        // The bucket manifest wins over the local artifact, carries the
        // binding names, and never leaks `vars`.
        seed_manifest(&store, "groceries").await;
        let (status, _, body) = send(
            &app,
            authed("/api/console/apps/groceries/data/r2/buckets", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains("should-never-leak"),
            "manifest vars leaked: {body}"
        );
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(
            body["buckets"],
            serde_json::json!([
                {"binding": "FILES", "bucket": "files"},
                {"binding": "AVATARS", "bucket": "avatars"},
            ])
        );
    }

    #[tokio::test]
    async fn r2_undeclared_bucket_is_404_and_bad_keys_are_400() {
        let (app, pool, _, store) = test_state("celld").await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let cookie = login(&pool, "ana@example.com").await;
        seed_manifest(&store, "groceries").await;
        let (status, _, body) = send(
            &app,
            authed(
                "/api/console/apps/groceries/data/r2/list?bucket=nope&prefix=",
                &cookie,
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["error"], "not_found");
        assert!(body["user_message"].is_string());
        // A traversal key never reaches celld: 400.
        let (status, _, _) = send(
            &app,
            authed(
                "/api/console/apps/groceries/data/r2/file?bucket=files&key=..%2Fescape",
                &cookie,
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn r2_put_and_delete_audit_console_rows() {
        let bin = fake_celld(0);
        let (app, pool, _, store) = test_state(bin.to_str().unwrap()).await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let cookie = login(&pool, "ana@example.com").await;
        seed_manifest(&store, "groceries").await;
        let (status, _, body) = send(
            &app,
            Request::put("/api/console/apps/groceries/data/r2/object?bucket=files&key=a%2Fb.txt")
                .header("sec-fetch-site", "same-origin")
                .header("cookie", &cookie)
                .header("content-type", "text/plain")
                .body(Body::from("hello"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "put failed: {body}");
        let (status, _, body) = send(
            &app,
            Request::post("/api/console/apps/groceries/data/r2/delete")
                .header("sec-fetch-site", "same-origin")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"bucket":"files","keys":["a/b.txt"]}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "delete failed: {body}");
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["deleted"], 1);
        // Both mutations wrote audit rows with both ids NULL (client "Console").
        let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT tool, outcome, consent_id, apikey_id FROM agent_action WHERE app_id = 'app-groceries' ORDER BY created_at",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "r2_put");
        assert_eq!(rows[0].1, "ok");
        assert_eq!(rows[1].0, "r2_delete");
        assert!(rows.iter().all(|r| r.2.is_none() && r.3.is_none()));
    }

    #[tokio::test]
    async fn r2_delete_rejects_empty_and_oversized_key_lists() {
        let bin = fake_celld(0);
        let (app, pool, _, store) = test_state(bin.to_str().unwrap()).await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let cookie = login(&pool, "ana@example.com").await;
        seed_manifest(&store, "groceries").await;
        for payload in [
            r#"{"bucket":"files","keys":[]}"#,
            r#"{"bucket":"files","keys":["a/../b"]}"#,
        ] {
            let (status, _, _) = send(
                &app,
                Request::post("/api/console/apps/groceries/data/r2/delete")
                    .header("sec-fetch-site", "same-origin")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "for {payload}");
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_action WHERE app_id = 'app-groceries'")
                .fetch_one(&pool)
                .await
                .unwrap();
        // Refused writes still audit (outcome error), like MCP's `record`.
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn do_classes_list_and_unknown_class_is_400() {
        let (app, pool, _, store) = test_state("celld").await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let cookie = login(&pool, "ana@example.com").await;
        seed_manifest(&store, "groceries").await;
        let (status, _, body) = send(
            &app,
            authed("/api/console/apps/groceries/data/do/classes", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(
            body["classes"],
            serde_json::json!([{"name": "Counter", "class": "Counter"}, {"name": "RateLimiter", "class": "RateLimiter"}])
        );
        let (status, _, body) = send(
            &app,
            authed(
                "/api/console/apps/groceries/data/do/instances?class=Nope",
                &cookie,
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body: serde_json::Value = body.parse().unwrap();
        assert!(body["user_message"].is_string());
    }

    #[tokio::test]
    async fn do_probe_is_best_effort_null() {
        let (app, pool, _, _) = test_state("celld").await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let cookie = login(&pool, "ana@example.com").await;
        // Nothing listens on the test app's internal port: the probe stays
        // blank instead of failing.
        let (status, _, body) = send(
            &app,
            authed(
                "/api/console/apps/groceries/data/do/probe?scope=room-1",
                &cookie,
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["scope"], "room-1");
        assert!(body["preview"].is_null());
    }

    #[test]
    fn celld_env_carries_the_scoped_grant() {
        let storage = FleetStorage {
            endpoint: "https://ep".to_string(),
            region: "auto".to_string(),
            bucket: "outer-test".to_string(),
            credentials: CredentialSource::Static {
                access_key_id: "id".to_string(),
                secret_access_key: "secret".to_string(),
            },
        };
        let creds = crate::store::credentials::ScopedCredentials {
            access_key_id: "scoped-id".to_string(),
            secret_access_key: "scoped-secret".to_string(),
            session_token: Some("token".to_string()),
            minted_at: None,
            reused: false,
        };
        let env = celld_env(&storage, &creds);
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(get("AWS_ACCESS_KEY_ID").as_deref(), Some("scoped-id"));
        assert_eq!(get("S3_ENDPOINT").as_deref(), Some("https://ep"));
        assert_eq!(get("AWS_SESSION_TOKEN").as_deref(), Some("token"));
        assert_eq!(get("AWS_REGION").as_deref(), Some("auto"));
        assert_eq!(get("AWS_EC2_METADATA_DISABLED").as_deref(), Some("true"));
    }
}
