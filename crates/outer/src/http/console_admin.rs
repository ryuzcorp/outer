//! Console Admin moderation (spec §15.3, §13 `admin_stats`).
//!
//! Admin-only JSON next to the invite-codes/waitlist section: the moderation
//! queue (held deploys and flagged apps with the gate's findings), approve
//! and reject reviews, suspend/restore and account bans, and the overview
//! panel. Every endpoint reuses `console.rs`'s `admin_guard` (session +
//! same-origin + `OUTER_ADMIN_EMAILS`, 404 for signed-in non-admins so a
//! 403 cannot oracle the admin surface), calls the SAME
//! domain functions future MCP `admin_*` tools will call (no second
//! implementation), audits under the MCP tool names (`agent_action` with
//! client "Console" for app-scoped writes, `account_action` for the ban —
//! fail-closed on the admin path: a lost audit row answers 500), and
//! answers `{error, message}` with the MCP codes.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::console::{admin_guard, audit_mutation, nostore_json, ok_json};
use super::console_actions::tool_response;
use super::AppState;
use crate::auth::AccountRow;
use crate::mcp::error::ToolError;
use crate::mcp::McpState;
use crate::moderation::admin;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/console/admin/moderation/queue", get(api_queue))
        .route("/api/console/admin/stats", get(api_stats))
        .route("/api/console/admin/review", post(api_review))
        .route("/api/console/admin/apps/{slug}/suspend", post(api_suspend))
        .route("/api/console/admin/apps/{slug}/restore", post(api_restore))
        .route("/api/console/admin/accounts/ban", post(api_ban))
        .route("/api/console/admin/audit", get(api_audit))
}

/// `GET /api/console/admin/moderation/queue`: the actionable moderation
/// work, newest first — latest blocked deploy per app (with gate findings,
/// scores and review state), held/suspended apps, unreviewed crawl/report
/// checks. Reads are not audited.
async fn api_queue(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    if let Err(res) = admin_guard(&mcp.auth, &headers).await {
        return *res;
    }
    match admin::queue(&mcp.pool).await {
        Ok(items) => ok_json(json!({"items": items})),
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "the moderation queue is unavailable right now",
        ),
    }
}

/// `GET /api/console/admin/stats`: the `admin_stats` overview (spec §13).
/// Reads are not audited.
async fn api_stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    if let Err(res) = admin_guard(&mcp.auth, &headers).await {
        return *res;
    }
    match admin::stats(&mcp.pool, crate::auth::now_ms()).await {
        Ok(stats) => ok_json(serde_json::to_value(&stats).unwrap_or(serde_json::Value::Null)),
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "admin stats are unavailable right now",
        ),
    }
}

#[derive(Debug, Deserialize)]
struct ReviewBody {
    kind: String,
    id: String,
    verdict: String,
}

/// `POST /api/console/admin/review {kind: "deploy"|"app", id, verdict:
/// "allow"|"deny"}`: the `admin_review` path. `allow` on a blocked deploy
/// clears that app's chunk hashes and redeploys the blocked sha through the
/// shared deploy machinery; `deny` records the decision and keeps the
/// block. Audited as `admin_review`.
async fn api_review(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ReviewBody>,
) -> Response {
    let mcp = &state.mcp;
    let account = match admin_guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let kind = match body.kind.as_str() {
        "deploy" => admin::ReviewKind::Deploy,
        "app" => admin::ReviewKind::App,
        other => {
            return tool_response(&ToolError::invalid_argument(format!(
                "kind must be deploy or app, got {other:?}"
            )));
        }
    };
    let verdict = match body.verdict.as_str() {
        "allow" => admin::Verdict::Allow,
        "deny" => admin::Verdict::Deny,
        other => {
            return tool_response(&ToolError::invalid_argument(format!(
                "verdict must be allow or deny, got {other:?}"
            )));
        }
    };
    let applied = match admin::apply_review(
        &mcp.pool,
        kind,
        body.id.trim(),
        verdict,
        &account.email,
        crate::auth::now_ms(),
    )
    .await
    {
        Ok(applied) => applied,
        Err(err) => {
            return tool_response(&err);
        }
    };
    if !applied.redeploy {
        if let Err(res) = audit_review(mcp, &account, &applied.app_id, verdict, "ok", None).await
        {
            return *res;
        }
        return ok_json(json!({"reviewed": true, "redeployed": false}));
    }
    let sha = applied.sha.clone().unwrap_or_default();
    match crate::apps::get_by_slug(&mcp.pool, &applied.app_slug).await {
        Ok(Some(app)) => match crate::mcp::tools::run_deploy(mcp, &app, &sha, false).await {
            Ok(deploy) => {
                if let Err(res) =
                    audit_review(mcp, &account, &applied.app_id, verdict, "ok", None).await
                {
                    return *res;
                }
                ok_json(json!({"reviewed": true, "redeployed": true, "deploy": deploy}))
            }
            Err(err) => {
                let code = err.code;
                audit_review_refusal(mcp, &account, &applied.app_id, verdict, Some(code)).await;
                tool_response(&err)
            }
        },
        Ok(None) => {
            let err = ToolError::not_found(format!("no app {:?}", applied.app_slug));
            let code = err.code;
            audit_review_refusal(mcp, &account, &applied.app_id, verdict, Some(code)).await;
            tool_response(&err)
        }
        Err(_) => {
            let err = ToolError::internal("could not load the app right now".to_string());
            let code = err.code;
            audit_review_refusal(mcp, &account, &applied.app_id, verdict, Some(code)).await;
            tool_response(&err)
        }
    }
}

async fn audit_review(
    mcp: &McpState,
    account: &AccountRow,
    app_id: &str,
    verdict: admin::Verdict,
    outcome: &str,
    error_code: Option<&str>,
) -> Result<(), Box<Response>> {
    let summary = match verdict {
        admin::Verdict::Allow => "admin_review allow",
        admin::Verdict::Deny => "admin_review deny",
    };
    audit_mutation(
        &mcp.pool, app_id, account, "admin_review", summary, outcome, error_code, 0, true,
    )
    .await
}

/// A refused review is already the closed outcome, so its audit stays
/// best-effort — but a lost row logs loudly.
async fn audit_review_refusal(
    mcp: &McpState,
    account: &AccountRow,
    app_id: &str,
    verdict: admin::Verdict,
    error_code: Option<&str>,
) {
    let summary = match verdict {
        admin::Verdict::Allow => "admin_review allow",
        admin::Verdict::Deny => "admin_review deny",
    };
    if let Err(err) = crate::mcp::actions::try_record_console(
        &mcp.pool, app_id, &account.id, "admin_review", summary, "error", error_code, 0,
    )
    .await
    {
        tracing::error!(user = %account.id, error = %format!("{err:#}"), "admin review refusal audit failed");
    }
}

/// `POST /api/console/admin/apps/{slug}/suspend`: the `admin_suspend_app`
/// path — flip the state, stop the fleet, purge the edge. Audited as
/// `admin_suspend_app`.
async fn api_suspend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let mcp = &state.mcp;
    let account = match admin_guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app_id, slug) =
        match admin::set_moderation_state(&mcp.pool, slug.trim(), "suspended").await {
            Ok(pair) => pair,
            Err(err) => return tool_response(&err),
        };
    mcp.fleets.stop_app(&slug).await;
    purge_host_cache(mcp, &slug).await;
    if let Err(res) = audit_mutation(
        &mcp.pool,
        &app_id,
        &account,
        "admin_suspend_app",
        &format!("admin_suspend_app {slug}"),
        "ok",
        None,
        0,
        true,
    )
    .await
    {
        return *res;
    }
    ok_json(json!({"suspended": true, "slug": slug}))
}

/// `POST /api/console/admin/apps/{slug}/restore`: the
/// `admin_restore_app` path — back to `ok` (the wake sweep picks the app up
/// again on demand). Audited as `admin_restore_app`.
async fn api_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let mcp = &state.mcp;
    let account = match admin_guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app_id, slug) = match admin::set_moderation_state(&mcp.pool, slug.trim(), "ok").await {
        Ok(pair) => pair,
        Err(err) => return tool_response(&err),
    };
    if let Err(res) = audit_mutation(
        &mcp.pool,
        &app_id,
        &account,
        "admin_restore_app",
        &format!("admin_restore_app {slug}"),
        "ok",
        None,
        0,
        true,
    )
    .await
    {
        return *res;
    }
    ok_json(json!({"restored": true, "slug": slug}))
}

async fn purge_host_cache(mcp: &McpState, slug: &str) {
    let host = format!("{}.{}", slug, mcp.delete_cfg.apps_domain);
    crate::cloudflare::purge_host_cache(mcp.delete_cfg.cache_purge.as_ref(), &host).await;
}

#[derive(Debug, Deserialize)]
struct BanBody {
    email: String,
    reason: String,
}

/// `POST /api/console/admin/accounts/ban {email, reason}`: the
/// `admin_ban_account` path — refuse sign-in, park every owned app as
/// `stopped`, stop the live fleets. Audited as `admin_ban_account` in
/// `account_action` (it targets the account, not one app).
async fn api_ban(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BanBody>,
) -> Response {
    let mcp = &state.mcp;
    let account = match admin_guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let started = crate::auth::now_ms();
    let target = crate::auth::normalize_email(&body.email);
    if crate::auth::is_admin(&mcp.auth.config, &target) {
        return tool_response(&ToolError::forbidden(
            "administrator accounts cannot be banned".to_string(),
        ));
    }
    let outcome = admin::ban_account(&mcp.pool, &target, &body.reason).await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            // A refused ban is already the closed outcome; its audit stays
            // best-effort but a lost row logs loudly.
            if let Err(audit_err) = crate::accounts::try_record_account_action(
                &mcp.pool,
                &account.id,
                "admin_ban_account",
                &format!("admin_ban_account {target}"),
                "error",
                Some(err.code),
                started,
            )
            .await
            {
                tracing::error!(user = %account.id, error = %format!("{audit_err:#}"), "admin ban refusal audit failed");
            }
            return tool_response(&err);
        }
    };
    for slug in &outcome.app_slugs {
        mcp.fleets.stop_app(slug).await;
        purge_host_cache(mcp, slug).await;
    }
    if let Err(err) = crate::accounts::try_record_account_action(
        &mcp.pool,
        &account.id,
        "admin_ban_account",
        &format!(
            "admin_ban_account {} ({})",
            outcome.email,
            outcome.app_slugs.len()
        ),
        "ok",
        None,
        started,
    )
    .await
    {
        tracing::error!(user = %account.id, error = %format!("{err:#}"), "admin ban audit failed");
        return nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "could not record the admin action right now",
        );
    }
    ok_json(json!({
        "banned": true,
        "email": outcome.email,
        "apps_stopped": outcome.app_slugs.len(),
    }))
}

/// `GET /api/console/admin/audit?limit=`: the admin audit view — recent
/// admin-tool `account_action` rows across accounts, newest first. This is
/// where durable admin acts surface: directory reads (`admin_list_apps`,
/// one throttled row per admin per minute), bans (`admin_ban_account`),
/// and app deletes (`admin_delete_app`, keyed to the deleted app's owner
/// because the app row — and any `agent_action` row — is gone). Per-app
/// admin reads and mutations stay in that app's `agent_action` log with
/// the `admin <email>:` prefix.
#[derive(Debug, Deserialize)]
struct AuditQuery {
    limit: Option<u64>,
}

async fn api_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> Response {
    let mcp = &state.mcp;
    if let Err(res) = admin_guard(&mcp.auth, &headers).await {
        return *res;
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let rows: Vec<(String, String, String, String, Option<String>, i64)> = match sqlx::query_as(
        "SELECT user_id, tool, summary, outcome, error_code, created_at FROM account_action \
         WHERE tool LIKE 'admin\\_%' ESCAPE '\\' ORDER BY created_at DESC, rowid DESC LIMIT ?1",
    )
    .bind(limit as i64)
    .fetch_all(&mcp.pool)
    .await
    {
        Ok(rows) => rows,
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the admin audit right now",
            );
        }
    };
    ok_json(json!({
        "actions": rows
            .iter()
            .map(|(user_id, tool, summary, outcome, error_code, created_at)| {
                json!({
                    "user_id": user_id,
                    "tool": tool,
                    "summary": summary,
                    "outcome": outcome,
                    "error_code": error_code,
                    "created_at": created_at,
                })
            })
            .collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::{test_config, test_pool};
    use crate::store::snapshot::StateSync;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::SqlitePool;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    fn peer(mut req: Request<Body>) -> Request<Body> {
        req.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:43123".parse::<SocketAddr>().unwrap(),
        ));
        req
    }

    async fn test_app() -> (Router, SqlitePool) {
        let pool = test_pool().await;
        let (config, _) = test_config();
        let auth = crate::auth::AuthState {
            pool: pool.clone(),
            config,
        };
        let mcp = McpState::for_tests(pool.clone(), auth.clone());
        let missing = std::path::Path::new("/nonexistent-site");
        let app = super::super::router(super::super::AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
            auth,
            mcp,
            site: crate::http::site::SiteDir::new(missing),
        });
        (app, pool)
    }

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String) {
        let res = app.clone().oneshot(peer(req)).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    fn same_origin(req: axum::http::request::Builder) -> axum::http::request::Builder {
        req.header("sec-fetch-site", "same-origin")
    }

    fn authed(path: &str, cookie: &str) -> Request<Body> {
        same_origin(Request::get(path))
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    }

    fn post_json(path: &str, cookie: &str, body: serde_json::Value) -> Request<Body> {
        same_origin(Request::post(path).header("content-type", "application/json"))
            .header("cookie", cookie)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn seed_user(pool: &SqlitePool, email: &str, now: i64) {
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(format!("acct-{email}"))
        .bind(email)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn login(pool: &SqlitePool, email: &str) -> String {
        let id = crate::auth::create_session(pool, &format!("acct-{email}"))
            .await
            .unwrap();
        format!("__Host-outer_sid={id}")
    }

    async fn seed_app(pool: &SqlitePool, owner: &str, slug: &str, now: i64) {
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(format!("app-{slug}"))
        .bind(slug)
        .bind(format!("acct-{owner}"))
        .bind(300_000 + slug.len() as i64)
        .bind(18_000 + slug.len() as i64)
        .bind(18_001 + slug.len() as i64)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_blocked_deploy(pool: &SqlitePool, now: i64) {
        sqlx::query(
            "INSERT INTO deploy (id, app_id, sha, status, findings, created_at, finished_at) \
             VALUES ('dep-blocked', 'app-shop', 'deadbee', 'blocked', \
             '[{\"path\":\"pages/index.tsx\",\"line\":42,\"category\":\"profanity\",\"signal\":\"jev\",\"score\":0.95}]', \
             ?1, ?1)",
        )
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO moderation_check (id, app_id, deploy_id, sha, stage, findings, scores, verdict, created_at) \
             VALUES ('chk-1', 'app-shop', 'dep-blocked', 'deadbee', 'gate', \
             '[{\"path\":\"pages/index.tsx\",\"category\":\"profanity\"}]', '{\"profanity\":0.95}', 'block', ?1)",
        )
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO moderation_text (app_id, text_hash, verdict, checked_at) \
             VALUES ('app-shop', 'abc123', 'clear', ?1)",
        )
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn non_admin_gets_404_on_every_admin_endpoint() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "ana@example.com", now).await;
        seed_user(&pool, "owner@example.com", now).await;
        seed_app(&pool, "ana@example.com", "shop", now).await;
        let ana = login(&pool, "ana@example.com").await;

        // 404, never 403: the admin surface must not reveal that it exists.
        for (status, _) in [
            send(&app, authed("/api/console/admin/moderation/queue", &ana)).await,
            send(&app, authed("/api/console/admin/stats", &ana)).await,
            send(&app, authed("/api/console/admin/audit", &ana)).await,
        ] {
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        for (status, body) in [
            send(
                &app,
                post_json(
                    "/api/console/admin/review",
                    &ana,
                    serde_json::json!({"kind": "deploy", "id": "x", "verdict": "deny"}),
                ),
            )
            .await,
            send(
                &app,
                post_json(
                    "/api/console/admin/apps/shop/suspend",
                    &ana,
                    serde_json::json!({}),
                ),
            )
            .await,
            send(
                &app,
                post_json(
                    "/api/console/admin/apps/shop/restore",
                    &ana,
                    serde_json::json!({}),
                ),
            )
            .await,
            send(
                &app,
                post_json(
                    "/api/console/admin/accounts/ban",
                    &ana,
                    serde_json::json!({"email": "ana@example.com", "reason": "spam"}),
                ),
            )
            .await,
        ] {
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(
                body.parse::<serde_json::Value>().unwrap()["error"],
                "not_found"
            );
        }
    }

    #[tokio::test]
    async fn queue_lists_blocked_deploy_with_findings_and_scores() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "ana@example.com", now).await;
        seed_user(&pool, "owner@example.com", now).await;
        seed_app(&pool, "ana@example.com", "shop", now).await;
        seed_blocked_deploy(&pool, now).await;
        let admin = login(&pool, "owner@example.com").await;

        let (status, body) =
            send(&app, authed("/api/console/admin/moderation/queue", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item["kind"], "blocked_deploy");
        assert_eq!(item["id"], "dep-blocked");
        assert_eq!(item["app_slug"], "shop");
        assert_eq!(item["owner_email"], "ana@example.com");
        assert_eq!(item["sha"], "deadbee");
        assert_eq!(item["findings"][0]["category"], "profanity");
        assert_eq!(item["scores"]["profanity"], 0.95);
        assert!(item["reviewed_by"].is_null());
    }

    #[tokio::test]
    async fn queue_hides_superseded_blocked_deploys() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "ana@example.com", now).await;
        seed_user(&pool, "owner@example.com", now).await;
        seed_app(&pool, "ana@example.com", "shop", now).await;
        seed_blocked_deploy(&pool, now).await;
        sqlx::query(
            "INSERT INTO deploy (id, app_id, sha, status, created_at, finished_at) \
             VALUES ('dep-new', 'app-shop', 'cafef00', 'success', ?1, ?1)",
        )
        .bind(now + 1)
        .execute(&pool)
        .await
        .unwrap();
        let admin = login(&pool, "owner@example.com").await;

        let (status, body) =
            send(&app, authed("/api/console/admin/moderation/queue", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["items"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn review_deny_keeps_block_and_audits() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "ana@example.com", now).await;
        seed_user(&pool, "owner@example.com", now).await;
        seed_app(&pool, "ana@example.com", "shop", now).await;
        seed_blocked_deploy(&pool, now).await;
        let admin = login(&pool, "owner@example.com").await;

        let (status, body) = send(
            &app,
            post_json(
                "/api/console/admin/review",
                &admin,
                serde_json::json!({"kind": "deploy", "id": "dep-blocked", "verdict": "deny"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(
            body,
            serde_json::json!({"reviewed": true, "redeployed": false})
        );
        let (status,): (String,) =
            sqlx::query_as("SELECT status FROM deploy WHERE id = 'dep-blocked'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "blocked");
        let (reviewed_by,): (Option<String>,) =
            sqlx::query_as("SELECT reviewed_by FROM moderation_check WHERE id = 'chk-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(reviewed_by.as_deref(), Some("owner@example.com"));
        let (tool, outcome): (String, String) =
            sqlx::query_as("SELECT tool, outcome FROM agent_action WHERE app_id = 'app-shop'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((tool.as_str(), outcome.as_str()), ("admin_review", "ok"));
        // Decided blocks leave the queue.
        let queue = crate::moderation::admin::queue(&pool).await.unwrap();
        assert!(
            queue.iter().all(|item| item.id != "dep-blocked"),
            "denied block still queued"
        );
    }

    #[tokio::test]
    async fn review_allow_clears_hashes_redeploys_and_audits() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "ana@example.com", now).await;
        seed_user(&pool, "owner@example.com", now).await;
        seed_app(&pool, "ana@example.com", "shop", now).await;
        seed_blocked_deploy(&pool, now).await;
        let admin = login(&pool, "owner@example.com").await;

        let (status, body) = send(
            &app,
            post_json(
                "/api/console/admin/review",
                &admin,
                serde_json::json!({"kind": "deploy", "id": "dep-blocked", "verdict": "allow"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["reviewed"], true);
        assert_eq!(body["redeployed"], true);
        let (verdict,): (String,) =
            sqlx::query_as("SELECT verdict FROM moderation_text WHERE app_id = 'app-shop'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(verdict, "allowed");
        let (tool, outcome): (String, String) =
            sqlx::query_as("SELECT tool, outcome FROM agent_action WHERE app_id = 'app-shop'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((tool.as_str(), outcome.as_str()), ("admin_review", "ok"));
    }

    #[tokio::test]
    async fn review_rejects_unknown_shapes() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "owner@example.com", now).await;
        let admin = login(&pool, "owner@example.com").await;

        let (status, _) = send(
            &app,
            post_json(
                "/api/console/admin/review",
                &admin,
                serde_json::json!({"kind": "deploy", "id": "missing", "verdict": "deny"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            &app,
            post_json(
                "/api/console/admin/review",
                &admin,
                serde_json::json!({"kind": "site", "id": "x", "verdict": "deny"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(
            &app,
            post_json(
                "/api/console/admin/review",
                &admin,
                serde_json::json!({"kind": "deploy", "id": "x", "verdict": "maybe"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn suspend_and_restore_flip_state_and_audit() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "ana@example.com", now).await;
        seed_user(&pool, "owner@example.com", now).await;
        seed_app(&pool, "ana@example.com", "shop", now).await;
        let admin = login(&pool, "owner@example.com").await;

        let (status, body) = send(
            &app,
            post_json(
                "/api/console/admin/apps/shop/suspend",
                &admin,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["suspended"],
            true
        );
        let (state,): (String,) =
            sqlx::query_as("SELECT moderation_state FROM app WHERE slug = 'shop'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "suspended");

        let (status, _) = send(
            &app,
            post_json(
                "/api/console/admin/apps/shop/restore",
                &admin,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (state,): (String,) =
            sqlx::query_as("SELECT moderation_state FROM app WHERE slug = 'shop'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "ok");
        let tools: Vec<(String,)> = sqlx::query_as(
            "SELECT tool FROM agent_action WHERE app_id = 'app-shop' ORDER BY created_at, rowid",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            tools,
            [
                ("admin_suspend_app".to_string(),),
                ("admin_restore_app".to_string(),)
            ]
        );
        let (status, _) = send(
            &app,
            post_json(
                "/api/console/admin/apps/missing/suspend",
                &admin,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ban_parks_apps_and_audits_in_account_action() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "ana@example.com", now).await;
        seed_user(&pool, "owner@example.com", now).await;
        seed_app(&pool, "ana@example.com", "shop", now).await;
        let admin = login(&pool, "owner@example.com").await;

        let (status, body) = send(
            &app,
            post_json(
                "/api/console/admin/accounts/ban",
                &admin,
                serde_json::json!({"email": "ana@example.com", "reason": "phishing kit"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["banned"], true);
        assert_eq!(body["apps_stopped"], 1);
        let (banned, desired): (i64, String) = sqlx::query_as(
            "SELECT banned, desired_state FROM user JOIN app ON app.owner_id = user.id \
             WHERE user.email = 'ana@example.com'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((banned, desired.as_str()), (1, "stopped"));
        let (tool, outcome): (String, String) = sqlx::query_as(
            "SELECT tool, outcome FROM account_action WHERE user_id = 'acct-owner@example.com'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (tool.as_str(), outcome.as_str()),
            ("admin_ban_account", "ok")
        );

        let (status, _) = send(
            &app,
            post_json(
                "/api/console/admin/accounts/ban",
                &admin,
                serde_json::json!({"email": "ghost@example.com", "reason": "spam"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            &app,
            post_json(
                "/api/console/admin/accounts/ban",
                &admin,
                serde_json::json!({"email": "ana@example.com", "reason": "  "}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    #[tokio::test]
    async fn ban_refuses_admin_accounts_and_normalizes_email() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "ana@example.com", now).await;
        seed_user(&pool, "owner@example.com", now).await;
        let admin = login(&pool, "owner@example.com").await;

        for target in ["owner@example.com", " Owner@Example.com "] {
            let (status, body) = send(
                &app,
                post_json(
                    "/api/console/admin/accounts/ban",
                    &admin,
                    serde_json::json!({"email": target, "reason": "spam"}),
                ),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "target {target:?} got: {body}"
            );
            assert_eq!(
                body.parse::<serde_json::Value>().unwrap()["error"],
                "forbidden"
            );
        }
        let (banned,): (i64,) =
            sqlx::query_as("SELECT banned FROM user WHERE email = 'owner@example.com'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(banned, 0);

        // Mixed case still reaches the account: the lookup uses the normalized email.
        let (status, _) = send(
            &app,
            post_json(
                "/api/console/admin/accounts/ban",
                &admin,
                serde_json::json!({"email": " ANA@EXAMPLE.COM ", "reason": "spam"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    #[tokio::test]
    async fn stats_match_queue_after_review() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "ana@example.com", now).await;
        seed_user(&pool, "owner@example.com", now).await;
        seed_app(&pool, "ana@example.com", "shop", now).await;
        seed_blocked_deploy(&pool, now).await;
        let admin = login(&pool, "owner@example.com").await;

        // The blocked deploy queues, but its gate check is not an
        // unreviewed after-live check.
        let items = admin::queue(&pool).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "blocked_deploy");
        let (status, body) = send(&app, authed("/api/console/admin/stats", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["blocked_deploys"], 1);
        assert_eq!(body["unreviewed_checks"], 0);

        // A deny leaves the deploy blocked but reviewed: it drops out of
        // both the queue and the blocked-deploy count.
        admin::apply_review(
            &pool,
            admin::ReviewKind::Deploy,
            "dep-blocked",
            admin::Verdict::Deny,
            "owner@example.com",
            now,
        )
        .await
        .unwrap();
        let items = admin::queue(&pool).await.unwrap();
        assert!(items.is_empty(), "queue after deny: {items:?}");
        let (status, body) = send(&app, authed("/api/console/admin/stats", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["blocked_deploys"], 0);
        assert_eq!(body["unreviewed_checks"], 0);

        // A non-gate check still counts in both places.
        sqlx::query(
            "INSERT INTO moderation_check (id, app_id, deploy_id, sha, stage, findings, scores, verdict, created_at) \
             VALUES ('chk-2', 'app-shop', NULL, 'deadbee', 'crawl', '[]', '{}', 'flag', ?1)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let items = admin::queue(&pool).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "flagged_check");
        let (status, body) = send(&app, authed("/api/console/admin/stats", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["blocked_deploys"], 0);
        assert_eq!(body["unreviewed_checks"], 1);
    }

    #[tokio::test]
    async fn stats_counts_match_seeds() {
        let (app, pool) = test_app().await;
        let now = crate::auth::now_ms();
        seed_user(&pool, "ana@example.com", now).await;
        seed_user(&pool, "owner@example.com", now).await;
        seed_app(&pool, "ana@example.com", "shop", now).await;
        seed_blocked_deploy(&pool, now).await;
        sqlx::query("INSERT INTO waitlist (email, first_seen) VALUES ('waiter@example.com', ?1)")
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        let admin = login(&pool, "owner@example.com").await;

        let (status, body) = send(&app, authed("/api/console/admin/stats", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["accounts_total"], 2);
        assert_eq!(body["waitlist_size"], 1);
        assert_eq!(body["apps_total"], 1);
        assert_eq!(body["blocked_deploys"], 1);
        // The only check is the gate check behind that blocked deploy:
        // gate-stage checks never count as unreviewed after-live checks.
        assert_eq!(body["unreviewed_checks"], 0);
        assert_eq!(body["deploys_7d"], 1);
    }
}
