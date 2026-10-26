//! Console mutating actions: visibility, keep-warm, delete, secrets, rollback, rename.
//!
//! One module per the wave-2 seam: `console.rs` only gains a
//! `.merge(console_actions::routes())` line, so parallel console work lands
//! without touching this file. Every endpoint reuses `console.rs`'s `guard`
//! and `fetch_ok` rules (session cookie only, same-origin, role-scoped
//! 404s), calls the SAME domain functions the MCP tools call, records the
//! mutation in `agent_action` under the MCP tool name (client "Console"),
//! and answers `{error, message, user_message?}` with the MCP codes.
//! Visibility, delete and secret writes stay owner-only (spec §7.4: a
//! collaborator reads 403); rollback admits collaborators like the MCP tool.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::console::{guard, nostore_json, not_found, ok_json, visible_app};
use super::AppState;
use crate::auth::AccountRow;
use crate::mcp::error::ToolError;
use crate::mcp::tools::{claim_change, plan_of, refund_change, run_deploy};
use crate::mcp::McpState;

/// The mutating console routes, merged by [`super::console::router`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/console/apps/{slug}/visibility", post(api_visibility))
        .route("/api/console/apps/{slug}/always-on", post(api_always_on))
        .route("/api/console/apps/{slug}/rename", post(api_rename))
        .route("/api/console/apps/{slug}/delete", post(api_delete))
        .route(
            "/api/console/apps/{slug}/secrets/{name}",
            put(api_secret_put).delete(api_secret_delete),
        )
        .route("/api/console/apps/{slug}/rollback", post(api_rollback))
        .route(
            "/api/console/apps/{slug}/preview/promote",
            post(api_preview_promote),
        )
        .route(
            "/api/console/apps/{slug}/preview/discard",
            post(api_preview_discard),
        )
}

/// Map a domain refusal to its HTTP status. The body always carries the same
/// `{error, message, user_message?}` shape the MCP tool returns, so the UI
/// shows `user_message` in an `alert alert-error alert-soft`.
pub(crate) fn tool_status(code: &str) -> StatusCode {
    match code {
        "not_found" => StatusCode::NOT_FOUND,
        "invalid_argument" => StatusCode::BAD_REQUEST,
        "forbidden" => StatusCode::FORBIDDEN,
        "payment_required" => StatusCode::PAYMENT_REQUIRED,
        "conflict" => StatusCode::CONFLICT,
        "quota_exceeded" | "rate_limited" => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

pub(crate) fn tool_response(err: &ToolError) -> Response {
    let mut body = json!({"error": err.code, "message": err.message});
    if let Some(user) = &err.user_message {
        body["user_message"] = json!(user);
    }
    for (key, value) in &err.extra {
        body[key] = value.clone();
    }
    (
        tool_status(err.code),
        [
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
            ("content-type", "application/json"),
        ],
        body.to_string(),
    )
        .into_response()
}

/// The session plus the visible app, or the error response. Every mutating
/// console endpoint starts here: a foreign or missing slug answers 404
/// exactly like a missing one (spec §8.2), and cross-site requests never
/// reach the mutation (the same-origin check inside [`guard`]).
async fn mutation_app(
    mcp: &McpState,
    headers: &HeaderMap,
    slug: &str,
) -> Result<(AccountRow, crate::apps::App), Box<Response>> {
    let account = match guard(&mcp.auth, headers).await {
        Ok(account) => account,
        Err(res) => return Err(res),
    };
    match visible_app(mcp, &account, slug).await {
        Ok(Some((app, _))) => Ok((account, app)),
        Ok(None) => Err(not_found().into()),
        Err(_) => Err(nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "could not load the app right now",
        )
        .into()),
    }
}

/// [`mutation_app`] plus the owner check: visibility, delete and secret
/// writes stay owner-only (spec §7.4), so a collaborator reads 403 with the
/// same code and user message the MCP tool answers — never 404 (spec §8.2
/// reserves 404 for slugs the caller has no role on).
async fn owner_mutation_app(
    mcp: &McpState,
    headers: &HeaderMap,
    slug: &str,
) -> Result<(AccountRow, crate::apps::App), Box<Response>> {
    let account = match guard(&mcp.auth, headers).await {
        Ok(account) => account,
        Err(res) => return Err(res),
    };
    match visible_app(mcp, &account, slug).await {
        Ok(Some((app, crate::collaborators::Role::Owner))) => Ok((account, app)),
        Ok(Some(_)) => Err(tool_response(
            &ToolError::forbidden(format!(
                "only the app owner may change {slug:?} this way"
            ))
            .with_user("Only the app owner can do that. Collaborators can read and deploy, but not change these settings."),
        )
        .into()),
        Ok(None) => Err(not_found().into()),
        Err(_) => Err(nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "could not load the app right now",
        )
        .into()),
    }
}

/// Record the console mutation under its MCP tool name, then render the
/// result. Successes and refusals both land in `agent_action` (like the MCP
/// log); both `consent_id` and `apikey_id` stay NULL, so the log labels the
/// client "Console".
async fn audited(
    mcp: &McpState,
    account: &AccountRow,
    app: &crate::apps::App,
    tool: &str,
    summary: &str,
    result: Result<serde_json::Value, ToolError>,
) -> Response {
    let (outcome, error_code) = match &result {
        Ok(_) => ("ok", None),
        Err(err) => ("error", Some(err.code)),
    };
    crate::mcp::actions::record_console(
        &mcp.pool,
        &app.id,
        &account.id,
        tool,
        summary,
        outcome,
        error_code,
        0,
    )
    .await;
    match result {
        Ok(body) => ok_json(body),
        Err(err) => tool_response(&err),
    }
}

#[derive(Debug, Deserialize)]
struct VisibilityBody {
    visibility: String,
}

/// `POST /api/console/apps/:slug/visibility {visibility}`: the MCP
/// `set_visibility` path, including the shared Pro gate (Free refuses
/// `public` with `payment_required`; `unlisted` is always allowed).
async fn api_visibility(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<VisibilityBody>,
) -> Response {
    let mcp = &state.mcp;
    let (account, app) = match owner_mutation_app(mcp, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let summary = format!("set_visibility {}", body.visibility);
    let result = match body.visibility.as_str() {
        "unlisted" => set_app_visibility(mcp, &app, crate::apps::Visibility::Unlisted).await,
        "public" => set_app_visibility(mcp, &app, crate::apps::Visibility::Public).await,
        other => Err(ToolError::invalid_argument(format!(
            "visibility must be unlisted or public, got {other:?}"
        ))),
    };
    audited(mcp, &account, &app, "set_visibility", &summary, result).await
}

/// The domain call plus the edge purge, shared by the visibility endpoint's
/// two valid inputs.
async fn set_app_visibility(
    mcp: &McpState,
    app: &crate::apps::App,
    visibility: crate::apps::Visibility,
) -> Result<serde_json::Value, ToolError> {
    let plan = plan_of(&mcp.pool, &app.owner_id).await?;
    crate::apps::set_visibility(&mcp.pool, &app.id, visibility, &plan)
        .await
        .map_err(|err| {
            crate::mcp::tools::apps::visibility_tool_error(
                &app.slug,
                err,
                &mcp.billing,
                &app.owner_id,
            )
        })?;
    // Same purge as the MCP tool: visibility changes `X-Robots-Tag` and
    // `/robots.txt`, which the edge caches for hours.
    let host = format!("{}.{}", app.slug, mcp.delete_cfg.apps_domain);
    crate::cloudflare::purge_host_cache(mcp.delete_cfg.cache_purge.as_ref(), &host).await;
    Ok(json!({"visibility": visibility.as_str()}))
}

#[derive(Debug, Deserialize)]
struct AlwaysOnBody {
    on: bool,
}

/// `POST /api/console/apps/:slug/always-on {on}`: the MCP `set_always_on`
/// path, including the shared Pro gate (Free refuses `on` with
/// `payment_required`; `off` is always allowed).
async fn api_always_on(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<AlwaysOnBody>,
) -> Response {
    let mcp = &state.mcp;
    let (account, app) = match owner_mutation_app(mcp, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let summary = format!("set_always_on {}", if body.on { "on" } else { "off" });
    let result = set_app_always_on(mcp, &app, body.on).await;
    audited(mcp, &account, &app, "set_always_on", &summary, result).await
}

/// The domain call, shared by the always-on endpoint's single input.
async fn set_app_always_on(
    mcp: &McpState,
    app: &crate::apps::App,
    on: bool,
) -> Result<serde_json::Value, ToolError> {
    let plan = plan_of(&mcp.pool, &app.owner_id).await?;
    crate::apps::always_on::set_always_on(&mcp.pool, &app.id, on, &plan)
        .await
        .map_err(|err| {
            crate::mcp::tools::apps::always_on_tool_error(
                &app.slug,
                err,
                &mcp.billing,
                &app.owner_id,
            )
        })?;
    Ok(json!({"always_on": on}))
}

#[derive(Debug, Deserialize)]
struct DeleteBody {
    confirm: String,
}

/// `POST /api/console/apps/:slug/delete {confirm}`: the MCP `delete_app`
/// path — same locks, same domain function, same confirm rule. No audit row:
/// the app row cascades, taking the entry with it (the MCP tool is unlogged
/// for the same reason).
async fn api_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<DeleteBody>,
) -> Response {
    let mcp = &state.mcp;
    let (_, app) = match owner_mutation_app(mcp, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    if body.confirm.len() > 64 {
        return tool_response(&ToolError::invalid_argument(format!(
            "confirm {:?} does not match the slug {:?}: pass the slug exactly to delete it",
            body.confirm, app.slug,
        )));
    }
    // Same lock trio as the MCP tool: serialize against a racing rollback,
    // deploy and wake.
    let rollback = mcp.rollback_locks.lock(&app.id).await;
    let deploy = mcp.deploys.app_lock(&app.id).lock_owned().await;
    let wake_key = mcp.fleets.wake_lock(&app.slug).await;
    let wake = wake_key.lock_owned().await;
    let locks = crate::apps::delete::DeleteLocks {
        deploy: &deploy,
        rollback: Some(&rollback),
        wake: Some(&wake),
    };
    let deps = crate::apps::delete::DeleteDeps {
        pool: &mcp.pool,
        store: &mcp.store,
        git: &mcp.git,
        sync: &mcp.sync,
        fleets: Some(&mcp.fleets),
    };
    match crate::apps::delete::delete_app(&deps, &mcp.delete_cfg, &app.slug, &body.confirm, locks)
        .await
    {
        Ok(out) => {
            let host = format!("{}.{}", out.slug, mcp.delete_cfg.apps_domain);
            ok_json(json!({
                "deleted": true,
                "slug": out.slug,
                "user_message": format!(
                    "{host} is deleted. The name is free to claim again."
                ),
            }))
        }
        Err(err) => tool_response(&match err {
            crate::apps::delete::DeleteError::ConfirmMismatch(_, _) => {
                ToolError::invalid_argument(format!(
                    "confirm {:?} does not match the slug {:?}: pass the slug exactly to delete it",
                    body.confirm, app.slug,
                ))
            }
            crate::apps::delete::DeleteError::NotFound(slug) => {
                ToolError::not_found(format!("no app named {slug:?}"))
            }
            crate::apps::delete::DeleteError::Internal(err) => crate::mcp::tools::internal(err),
        }),
    }
}

#[derive(Debug, Deserialize)]
struct SecretValueBody {
    value: String,
}

/// `PUT /api/console/apps/:slug/secrets/:name {value}`: the MCP `set_secret`
/// path — same name and reserved checks, same 4 KiB cap, same encryption,
/// same per-app count. Values never come back: the answer carries the name,
/// `updated_at` and when the value takes effect.
async fn api_secret_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
    Json(body): Json<SecretValueBody>,
) -> Response {
    let mcp = &state.mcp;
    let (account, app) = match owner_mutation_app(mcp, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let summary = format!("set_secret {name}");
    let result = set_app_secret(mcp, &app, &name, &body.value).await;
    audited(mcp, &account, &app, "set_secret", &summary, result).await
}

async fn set_app_secret(
    mcp: &McpState,
    app: &crate::apps::App,
    name: &str,
    value: &str,
) -> Result<serde_json::Value, ToolError> {
    crate::secrets::check_name(name).map_err(ToolError::invalid_argument)?;
    crate::secrets::check_reserved(name).map_err(ToolError::invalid_argument)?;
    if value.len() > crate::secrets::MAX_VALUE_BYTES {
        return Err(ToolError::invalid_argument(format!(
            "value is {} bytes; at most {} bytes",
            value.len(),
            crate::secrets::MAX_VALUE_BYTES
        )));
    }
    let blob = crate::secrets::encrypt(&mcp.secrets_key, &app.id, name, value.as_bytes());
    let now = crate::apps::now_ms();
    let capped = crate::secrets::set(&mcp.pool, &app.id, name, &blob, now)
        .await
        .map_err(crate::mcp::tools::internal)?;
    if capped {
        return Err(ToolError::invalid_argument(format!(
            "{} already has the maximum {} secrets; unset one first",
            app.slug,
            crate::secrets::MAX_SECRETS_PER_APP
        )));
    }
    Ok(json!({"name": name, "updated_at": now, "applies": "next deploy"}))
}

/// `DELETE /api/console/apps/:slug/secrets/:name`: the MCP `unset_secret`
/// path. Answers `{name}` plus when the removal takes effect.
async fn api_secret_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
) -> Response {
    let mcp = &state.mcp;
    let (account, app) = match owner_mutation_app(mcp, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let summary = format!("unset_secret {name}");
    let result = async {
        crate::secrets::check_name(&name).map_err(ToolError::invalid_argument)?;
        let removed = crate::secrets::unset(&mcp.pool, &app.id, &name)
            .await
            .map_err(crate::mcp::tools::internal)?;
        if !removed {
            return Err(ToolError::not_found(format!(
                "no secret {name:?} on {}",
                app.slug
            )));
        }
        Ok(json!({"name": name, "applies": "next deploy"}))
    }
    .await;
    audited(mcp, &account, &app, "unset_secret", &summary, result).await
}

#[derive(Debug, Deserialize)]
struct RollbackBody {
    sha: String,
}

/// `POST /api/console/apps/:slug/rollback {sha}`: the MCP `rollback` path
/// (target checks and daily quota included), without waiting: the answer
/// carries the new commit and the building deploy, and the UI refreshes the
/// row list to show it.
async fn api_rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<RollbackBody>,
) -> Response {
    let mcp = &state.mcp;
    let (account, app) = match mutation_app(mcp, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let summary = format!("rollback to {}", body.sha);
    let result = async {
        if body.sha.len() > 128 {
            return Err(ToolError::invalid_argument(
                "sha must be a commit sha from history",
            ));
        }
        let out = crate::mcp::tools::code::rollback_to(
            mcp,
            &app,
            &account.id,
            &account.email,
            &body.sha,
            false,
        )
        .await?;
        let structured = out.get("structuredContent").cloned().unwrap_or_default();
        Ok(json!({
            "sha": structured.pointer("/commit/sha"),
            "deploy": structured.pointer("/deploy"),
        }))
    }
    .await;
    audited(mcp, &account, &app, "rollback", &summary, result).await
}

/// `POST /api/console/apps/:slug/preview/promote`: the MCP
/// `promote_preview` path (Pro gate, success-deploy check and fast-forward
/// included), without waiting: the answer carries the sha and the building
/// live deploy, and the UI refreshes the app. Audited under
/// `promote_preview`.
async fn api_preview_promote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let mcp = &state.mcp;
    let (account, app) = match mutation_app(mcp, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let result = async {
        let out = crate::mcp::tools::preview::promote_preview_to(mcp, &app, false).await?;
        let structured = out.get("structuredContent").cloned().unwrap_or_default();
        Ok(json!({
            "sha": structured.pointer("/sha"),
            "deploy": structured.pointer("/deploy"),
        }))
    }
    .await;
    audited(
        mcp,
        &account,
        &app,
        "promote_preview",
        "promote preview to live",
        result,
    )
    .await
}

/// `POST /api/console/apps/:slug/preview/discard`: the MCP
/// `discard_preview` path. Audited under `discard_preview`.
async fn api_preview_discard(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let mcp = &state.mcp;
    let (account, app) = match mutation_app(mcp, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let result = async {
        let out = crate::mcp::tools::preview::discard_preview_for(mcp, &app).await?;
        let structured = out.get("structuredContent").cloned().unwrap_or_default();
        Ok(json!({
            "app": structured.pointer("/app"),
        }))
    }
    .await;
    audited(
        mcp,
        &account,
        &app,
        "discard_preview",
        "discard preview",
        result,
    )
    .await
}

/// `POST /api/console/apps/:slug/rename {slug: "<new>"}`: the console side
/// of MCP `rename_app` — the same domain function, the same daily-changes
/// quota, and the audit row under the `rename_app` tool name with the
/// "Console" client label. Answers `{slug, url}` for the new name; the
/// Settings UI navigates there (console reads stay slug-addressed).
async fn api_rename(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = std::time::Instant::now();
    let mcp = &state.mcp;
    let (account, app) = match owner_mutation_app(mcp, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    // One audit row whatever the outcome (mirrors the MCP path, which logs
    // refused calls too): the app is owner-gated above, so the row always
    // resolves.
    let audit = |summary: &str, outcome: &str, error_code: Option<&str>| {
        let pool = mcp.pool.clone();
        let app_id = app.id.clone();
        let account_id = account.id.clone();
        let summary = summary.to_string();
        let outcome = outcome.to_string();
        let error_code = error_code.map(str::to_string);
        let duration_ms = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
        async move {
            crate::mcp::actions::record_console(
                &pool,
                &app_id,
                &account_id,
                "rename_app",
                &summary,
                &outcome,
                error_code.as_deref(),
                duration_ms,
            )
            .await;
        }
    };
    let summary_for = |old: &str, new: &str| format!("rename_app {old} to {new}");
    let new_slug: Option<String> = body
        .get("slug")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let Some(new_slug) = new_slug.filter(|s| !s.is_empty()) else {
        let err = ToolError::invalid_argument("body must be {\"slug\": \"<new-slug>\"}");
        audit(&summary_for(&slug, "?"), "error", Some(err.code)).await;
        return tool_response(&err);
    };
    if slug == new_slug {
        let err = ToolError::invalid_argument(format!(
            "the app is already named {slug:?}; nothing to rename"
        ));
        audit(&summary_for(&slug, &new_slug), "error", Some(err.code)).await;
        return tool_response(&err);
    }
    if let Err(e) = crate::apps::validate_slug(&new_slug) {
        let err = ToolError::invalid_argument(e.to_string());
        audit(&summary_for(&slug, &new_slug), "error", Some(err.code)).await;
        return tool_response(&err);
    }
    // After the cheap checks, before the quota claim: mirrors the MCP tool,
    // so the console refuses flagged and uncheckable names identically.
    if let Err(e) = crate::apps::check_slug_profanity(mcp.jev.as_ref(), &new_slug, None, None).await
    {
        let err = ToolError::from_profanity(e);
        audit(&summary_for(&slug, &new_slug), "error", Some(err.code)).await;
        return tool_response(&err);
    }
    if let Err(err) = claim_change(&mcp.pool, &mcp.billing, &account.id).await {
        audit(&summary_for(&slug, &new_slug), "error", Some(err.code)).await;
        return tool_response(&err);
    }
    // Same locks as the MCP tool, in the same order (deploy, rollback, then
    // the wake pair sorted by slug so two renames can never ABBA).
    let rollback = mcp.rollback_locks.lock(&app.id).await;
    let deploy = mcp.deploys.app_lock(&app.id).lock_owned().await;
    let (first, second) = if slug.as_str() <= new_slug.as_str() {
        (&slug, &new_slug)
    } else {
        (&new_slug, &slug)
    };
    let wake_first_key = mcp.fleets.wake_lock(first).await;
    let wake_first = wake_first_key.lock_owned().await;
    let wake_second_key = mcp.fleets.wake_lock(second).await;
    let wake_second = wake_second_key.lock_owned().await;
    let (wake_old, wake_new) = if first == &slug {
        (&wake_first, &wake_second)
    } else {
        (&wake_second, &wake_first)
    };
    let locks = crate::apps::rename::RenameLocks {
        deploy: &deploy,
        rollback: &rollback,
        wake_old,
        wake_new,
    };
    let deps = crate::apps::rename::RenameDeps {
        pool: &mcp.pool,
        store: &mcp.store,
        git: &mcp.git,
        sync: &mcp.sync,
        fleets: Some(&mcp.fleets),
        data_dir: &mcp.delete_cfg.data_dir,
        cgroups: mcp.delete_cfg.cgroups.as_deref(),
        apps_domain: &mcp.delete_cfg.apps_domain,
        cache_purge: mcp.delete_cfg.cache_purge.as_ref(),
    };
    let out = match crate::apps::rename::rename_app(&deps, &slug, &new_slug, locks).await {
        Ok(out) => out,
        Err(err) => {
            // Any domain error is pre-commit (post-commit steps never fail),
            // so the rename never happened: refund the claimed change.
            let _ = refund_change(&mcp.pool, &account.id).await;
            let err = ToolError::from_rename(err);
            audit(&summary_for(&slug, &new_slug), "error", Some(err.code)).await;
            return tool_response(&err);
        }
    };
    let url = mcp.app_url(&out.app.slug);
    // A running app redeploys under the new name; the rename already counts
    // the change, so a failed redeploy still answers success for the rename
    // itself (the deploy has its own rows and the previous version serves).
    let deploy_status = match out.redeploy_sha {
        Some(sha) => match run_deploy(mcp, &out.app, &sha, true).await {
            Ok(deploy) => deploy
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("building")
                .to_string(),
            Err(_) => "failed".to_string(),
        },
        None => "ok".to_string(),
    };
    audit(&summary_for(&slug, &out.app.slug), &deploy_status, None).await;
    ok_json(json!({"slug": out.app.slug, "url": url}))
}
