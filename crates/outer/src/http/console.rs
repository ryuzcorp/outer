//! Console: the web UI under `/console` (spec §13.1).
//!
//! Routes (platform host only; explicit Axum routes, so they win over the
//! site fallback in [`super::platform_fallback`]):
//!
//! - `GET /console/sign-in` + `POST /console/sign-in`: the email-code web
//!   sign-in. Same codes, rate limits, beta gate and terms acceptance as the
//!   OAuth email step ([`crate::auth`]); success mints a `__Host-outer_sid`
//!   session and redirects to `/console`.
//! - `GET /api/console/me`: `{email, plan}`.
//! - `GET /api/console/apps`: the caller's apps (same rows as MCP
//!   `list_apps`, plus `last_request_at` and the last deploy).
//! - `GET /api/console/apps/:slug`: the app plus its last 20 deploys.
//! - `GET /api/console/apps/:slug/actions?before=&limit=`: the agent
//!   action log, newest first (same rows as `actions::list`).
//! - `GET /api/console/apps/:slug/metrics?hours=`: requests, visitors,
//!   4xx/5xx, latency p50/p95, hourly buckets and top paths (same rows as
//!   `metrics::summary`; `hours` is 24, 168 or 720).
//! - `GET /api/console/apps/:slug/logs?since=&limit=`: the fleet's
//!   in-memory log ring merged with persisted lines (same lines as MCP `app_logs`).
//! - `GET /api/console/apps/:slug/errors?status=`: grouped runtime errors
//!   plus the trailing-hour proxy 5xx (same rows as MCP `app_errors`).
//! - `GET /api/console/apps/:slug/secrets`: the app's secret names plus
//!   `updated_at` (same rows as MCP `list_secrets`; values never leave the
//!   server — they are injected as Worker env vars at deploy time).
//! - `POST /api/console/apps/:slug/visibility {visibility}`: the MCP
//!   `set_visibility` path, including the shared Pro gate.
//! - `POST /api/console/apps/:slug/delete {confirm}`: the MCP `delete_app`
//!   path; `confirm` must equal the slug.
//! - `PUT /api/console/apps/:slug/secrets/:name {value}` and
//!   `DELETE /api/console/apps/:slug/secrets/:name`: the MCP `set_secret` /
//!   `unset_secret` paths; values never leave the client again.
//! - `POST /api/console/apps/:slug/rollback {sha}`: the MCP `rollback`
//!   path, without waiting for the new deploy.
//! - `POST /api/console/sign-out`: deletes the session, clears the cookie.
//!
//! Rules: session cookie only (an `Authorization` header is ignored, never
//! honored); every app query is scoped to the signed-in account's apps, and
//! a foreign or unknown slug answers 404 exactly like a missing one (spec
//! §8.2); every `/api/console` request must carry
//! `Sec-Fetch-Site: same-origin` (403 otherwise, so a cross-site page cannot
//! drive the API off the user's cookie); JSON errors; `Cache-Control:
//! no-store` on everything.
//!
//!

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::{html, Markup};
use serde::Deserialize;
use serde_json::json;

use crate::auth::{AccountRow, AuthState};
use crate::http::client_ip::client_ip;
use crate::mcp::tools::{latest_deploy, plan_of};
use crate::mcp::McpState;

pub fn router(state: super::AppState) -> Router<super::AppState> {
    Router::new()
        .route("/console/sign-in", get(sign_in_get).post(sign_in_post))
        .route("/api/console/me", get(api_me))
        .route("/api/console/invites", get(api_invites))
        .route(
            "/api/console/admin/invites",
            get(api_admin_invites).post(api_admin_create_invites),
        )
        .route(
            "/api/console/admin/invites/{code}/revoke",
            post(api_admin_revoke_invite),
        )
        .route("/api/console/admin/waitlist", get(api_admin_waitlist))
        .route("/api/console/apps", get(api_apps))
        .route("/api/console/apps/{slug}", get(api_app))
        .route("/api/console/apps/{slug}/actions", get(api_actions))
        .route("/api/console/apps/{slug}/metrics", get(api_metrics))
        .route("/api/console/apps/{slug}/logs", get(api_logs))
        .route("/api/console/apps/{slug}/errors", get(api_errors))
        .route("/api/console/apps/{slug}/secrets", get(api_secrets))
        .merge(super::console_actions::routes())
        .merge(super::console_code::routes())
        .merge(super::console_domains::routes())
        .merge(super::console_export::routes())
        .route("/api/console/sign-out", post(api_sign_out))
        .merge(super::console_observe::routes())
        .merge(super::console_account::routes())
        .merge(super::console_collaborators::routes())
        .with_state(state)
}

pub(crate) fn nostore_json(status: StatusCode, error: &str, message: &str) -> Response {
    (
        status,
        [
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
            ("content-type", "application/json"),
        ],
        json!({"error": error, "message": message}).to_string(),
    )
        .into_response()
}

/// The `user_message` variant (contract): the UI shows `user_message` in an
/// `alert`, and agents relay it word for word. Same codes as the MCP tools.
pub(crate) fn nostore_json_user(
    status: StatusCode,
    error: &str,
    message: impl Into<String>,
    user_message: impl Into<String>,
) -> Response {
    (
        status,
        [
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
            ("content-type", "application/json"),
        ],
        json!({"error": error, "message": message.into(), "user_message": user_message.into()})
            .to_string(),
    )
        .into_response()
}
pub(crate) fn not_found() -> Response {
    nostore_json(StatusCode::NOT_FOUND, "not_found", "no such app")
}

/// `true` only when the request is a same-origin browser fetch. Browsers set
/// this header themselves; a cross-site page cannot spoof it, which is what
/// keeps the cookie-only API safe from CSRF.
pub(crate) fn fetch_ok(headers: &HeaderMap) -> bool {
    headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("same-origin")
}

/// Shared with `console_data` (the D1/R2/DO panels): the same session,
/// same-origin and owner-scoped-404 checks as every other console API.
pub(crate) async fn guard(
    auth: &AuthState,
    headers: &HeaderMap,
) -> Result<AccountRow, Box<Response>> {
    let Some(session) = crate::auth::session_from_headers(headers) else {
        return Err(nostore_json(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "sign in at /console/sign-in",
        )
        .into());
    };
    let account = crate::auth::account_from_session(&auth.pool, &session)
        .await
        .map_err(|_| {
            Box::new(nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "sign-in is unavailable right now",
            ))
        })?;
    let Some(account) = account else {
        return Err(nostore_json(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "sign in at /console/sign-in",
        )
        .into());
    };
    if !fetch_ok(headers) {
        return Err(nostore_json(
            StatusCode::FORBIDDEN,
            "forbidden",
            "same-origin requests only",
        )
        .into());
    }
    Ok(account)
}

/// The app `slug` names, visible to `account` as owner or collaborator
/// (Pro-gated: a downgrade hides the app again). Anything else is `None`,
/// and the caller answers 404 (spec §8.2).
pub(crate) async fn visible_app(
    mcp: &McpState,
    account: &AccountRow,
    slug: &str,
) -> anyhow::Result<Option<(crate::apps::App, crate::collaborators::Role)>> {
    let Some(app) = crate::apps::get_by_slug(&mcp.pool, slug).await? else {
        return Ok(None);
    };
    let role = crate::collaborators::role(
        &mcp.pool,
        &app.id,
        &app.owner_id,
        &account.id,
        &account.email,
    )
    .await?;
    Ok(role.map(|role| (app, role)))
}

async fn api_me(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let plan = plan_of(&mcp.pool, &account.id)
        .await
        .unwrap_or_else(|_| "free".into());
    let is_admin = crate::auth::is_admin(&mcp.auth.config, &account.email);
    (
        StatusCode::OK,
        [
            ("cache-control", "no-store"),
            ("content-type", "application/json"),
        ],
        json!({"email": account.email, "plan": plan, "is_admin": is_admin}).to_string(),
    )
        .into_response()
}

/// The `guard` plus the admin check: non-admins get 403, decided by the
/// `OUTER_ADMIN_EMAILS` config like everywhere else.
async fn admin_guard(auth: &AuthState, headers: &HeaderMap) -> Result<AccountRow, Box<Response>> {
    let account = guard(auth, headers).await?;
    if !crate::auth::is_admin(&auth.config, &account.email) {
        return Err(nostore_json(StatusCode::FORBIDDEN, "forbidden", "admin only").into());
    }
    Ok(account)
}

fn invite_json(row: &crate::auth::invites::InviteRow) -> serde_json::Value {
    json!({
        "code": row.code,
        "kind": row.kind,
        "note": row.note,
        "createdAt": row.created_at,
        "redeemedAt": row.redeemed_at,
        "redeemedByEmail": row.redeemed_by_email,
        "revokedAt": row.revoked_at,
    })
}

fn admin_invite_json(row: &crate::auth::invites::InviteRow) -> serde_json::Value {
    let mut v = invite_json(row);
    v["createdByEmail"] = json!(row.created_by_email);
    v
}

/// `GET /api/console/invites`: the caller's own codes, newest first.
async fn api_invites(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    match crate::auth::invites::list_for_user(&mcp.pool, &account.id).await {
        Ok(rows) => ok_json(json!({"codes": rows.iter().map(invite_json).collect::<Vec<_>>()})),
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "invites are unavailable right now",
        ),
    }
}

/// `GET /api/console/admin/invites?limit=&before=`: every code, newest
/// first. `before` is the opaque `<createdAt>:<code>` cursor of the last
/// row on the previous page.
async fn api_admin_invites(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mcp = &state.mcp;
    if let Err(res) = admin_guard(&mcp.auth, &headers).await {
        return *res;
    }
    let limit = query
        .get("limit")
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 100);
    let before = match query.get("before") {
        None => None,
        Some(raw) => match raw.split_once(':') {
            Some((at, code)) if !code.is_empty() => match at.parse::<i64>() {
                Ok(at) => Some((at, code.to_string())),
                Err(_) => {
                    return nostore_json(
                        StatusCode::BAD_REQUEST,
                        "invalid_cursor",
                        "before must be <createdAt>:<code>",
                    );
                }
            },
            _ => {
                return nostore_json(
                    StatusCode::BAD_REQUEST,
                    "invalid_cursor",
                    "before must be <createdAt>:<code>",
                );
            }
        },
    };
    match crate::auth::invites::list_all(&mcp.pool, limit + 1, before).await {
        Ok(rows) => {
            let (page, next) = if rows.len() as i64 > limit {
                let next = rows
                    .get(limit as usize - 1)
                    .map(|row| format!("{}:{}", row.created_at, row.code));
                (&rows[..limit as usize], next)
            } else {
                (&rows[..], None)
            };
            let mut body = json!({"codes": page.iter().map(admin_invite_json).collect::<Vec<_>>()});
            if let Some(next) = next {
                body["next"] = json!(next);
            }
            ok_json(body)
        }
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "invites are unavailable right now",
        ),
    }
}

#[derive(Debug, Deserialize)]
struct CreateInvites {
    count: i64,
    note: Option<String>,
}

/// `POST /api/console/admin/invites {count 1..50, note?}`: mint admin codes.
async fn api_admin_create_invites(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    body: axum::Json<CreateInvites>,
) -> Response {
    let mcp = &state.mcp;
    let account = match admin_guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    if !(1..=crate::auth::invites::MAX_ADMIN_BATCH).contains(&body.count) {
        return nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_count",
            "count must be between 1 and 50",
        );
    }
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|note| !note.is_empty())
        .map(|note| note.chars().take(200).collect::<String>());
    let now = crate::auth::now_ms();
    match crate::auth::invites::mint_admin(&mcp.pool, &account.id, body.count, note.as_deref(), now)
        .await
    {
        Ok(codes) => ok_json(json!({"codes": codes})),
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "invites are unavailable right now",
        ),
    }
}

/// `POST /api/console/admin/invites/{code}/revoke`: revoke an unused code.
/// Redeemed codes refuse; unknown codes 404.
async fn api_admin_revoke_invite(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Response {
    let mcp = &state.mcp;
    if let Err(res) = admin_guard(&mcp.auth, &headers).await {
        return *res;
    }
    let code = crate::auth::invites::normalize_invite_code(&code).unwrap_or(code);
    match crate::auth::invites::revoke(&mcp.pool, &code, crate::auth::now_ms()).await {
        Ok(crate::auth::invites::RevokeOutcome::Missing) => {
            nostore_json(StatusCode::NOT_FOUND, "not_found", "no such invite code")
        }
        Ok(crate::auth::invites::RevokeOutcome::Redeemed) => nostore_json(
            StatusCode::BAD_REQUEST,
            "already_used",
            "that code was already used and cannot be revoked",
        ),
        Ok(
            crate::auth::invites::RevokeOutcome::Revoked
            | crate::auth::invites::RevokeOutcome::AlreadyRevoked,
        ) => ok_json(json!({"revoked": true})),
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "invites are unavailable right now",
        ),
    }
}

/// `GET /api/console/admin/waitlist?limit=`: the waitlist, newest first.
async fn api_admin_waitlist(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mcp = &state.mcp;
    if let Err(res) = admin_guard(&mcp.auth, &headers).await {
        return *res;
    }
    let limit = query
        .get("limit")
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    match crate::auth::invites::waitlist(&mcp.pool, limit).await {
        Ok(entries) => ok_json(json!({"entries": entries})),
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "the waitlist is unavailable right now",
        ),
    }
}

fn deploy_brief(row: &crate::mcp::tools::DeployRow) -> serde_json::Value {
    let mut v = json!({"sha": row.sha, "status": row.status});
    let at = row.finished_at.or(Some(row.created_at));
    if let Some(at) = at {
        v["at"] = json!(at);
    }
    if let Some(finished) = row.finished_at {
        v["duration_ms"] = json!((finished - row.created_at).max(0));
    }
    v
}

async fn api_apps(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let apps = match crate::apps::list(&mcp.pool).await {
        Ok(apps) => apps,
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not list apps right now",
            );
        }
    };
    let mut out = Vec::new();
    for app in &apps {
        // Owners match inline; collaborators read their row (Pro-gated, so
        // a downgrade hides the app from the rail again).
        let role = match crate::collaborators::role(
            &mcp.pool,
            &app.id,
            &app.owner_id,
            &account.id,
            &account.email,
        )
        .await
        {
            Ok(role) => role,
            Err(_) => {
                return nostore_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "unavailable",
                    "could not list apps right now",
                );
            }
        };
        let Some(role) = role else { continue };
        let last = latest_deploy(&mcp.pool, &app.id).await.unwrap_or(None);
        let status =
            crate::mcp::tools::apps::app_status(app, last.as_ref().map(|r| r.status.as_str()));
        let mut v = json!({
            "slug": app.slug,
            "url": mcp.app_url(&app.slug),
            "role": match role {
                crate::collaborators::Role::Owner => "owner",
                crate::collaborators::Role::Collaborator => "collaborator",
            },
            "visibility": app.visibility.as_str(),
            "status": status,
            "last_request_at": app.last_request_at,
        });
        if let Some(last) = last {
            v["last_deploy"] = deploy_brief(&last);
        }
        out.push(v);
    }
    (
        StatusCode::OK,
        [
            ("cache-control", "no-store"),
            ("content-type", "application/json"),
        ],
        json!({"apps": out}).to_string(),
    )
        .into_response()
}

#[derive(Debug, sqlx::FromRow)]
struct DeployListRow {
    id: String,
    sha: String,
    status: String,
    error: Option<String>,
    findings: Option<String>,
    created_at: i64,
    finished_at: Option<i64>,
}

/// How many findings one deploy row carries: a JSON array or object counts
/// its entries; anything else (or unparsable) counts zero.
fn findings_count(raw: Option<&str>) -> u64 {
    let Some(raw) = raw else { return 0 };
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Array(items)) => items.len() as u64,
        Ok(serde_json::Value::Object(map)) => map.len() as u64,
        _ => 0,
    }
}

async fn api_app(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app, role) = match visible_app(mcp, &account, &slug).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(),
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the app right now",
            );
        }
    };
    let last = latest_deploy(&mcp.pool, &app.id).await.unwrap_or(None);
    let status =
        crate::mcp::tools::apps::app_status(&app, last.as_ref().map(|r| r.status.as_str()));
    // The action log and metrics live behind their own endpoints, so this
    // object stays small and each panel loads independently.
    let rows: Vec<DeployListRow> = match sqlx::query_as::<_, DeployListRow>(
        "SELECT id, sha, status, error, findings, created_at, finished_at \
         FROM deploy WHERE app_id = ?1 ORDER BY created_at DESC LIMIT 20",
    )
    .bind(&app.id)
    .fetch_all(&mcp.pool)
    .await
    {
        Ok(rows) => rows,
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load deploys right now",
            );
        }
    };
    let deploys: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let mut v = json!({
                "deploy_id": r.id,
                "sha": r.sha,
                "status": r.status,
                "findings_count": findings_count(r.findings.as_deref()),
                "created_at": r.created_at,
            });
            if let Some(finished) = r.finished_at {
                v["duration_ms"] = json!((finished - r.created_at).max(0));
            }
            // The only per-deploy message the platform keeps: why a deploy
            // failed. Blocked deploys carry their reason in `findings`.
            if let Some(error) = r.error.as_deref() {
                v["message"] = json!(error);
            }
            v
        })
        .collect();
    (
        StatusCode::OK,
        [
            ("cache-control", "no-store"),
            ("content-type", "application/json"),
        ],
        json!({
            "slug": app.slug,
            "url": mcp.app_url(&app.slug),
            "role": match role {
                crate::collaborators::Role::Owner => "owner",
                crate::collaborators::Role::Collaborator => "collaborator",
            },
            "visibility": app.visibility.as_str(),
            "status": status,
            "last_request_at": app.last_request_at,
            "deploys": deploys,
        })
        .to_string(),
    )
        .into_response()
}

/// Parse the console logs query into a validated [`LogFilter`](crate::observe::logs::LogFilter)
/// plus cursor. Bounds: `q` at most 200 chars, `limit` 1–500, `stream` one
/// of `stdout|stderr`, `level` one of `error|warn|info`, `from`/`to` unix
/// milliseconds, `before` a seq cursor for `Load older`. Anything else is
/// a 400 — the filters shape SQL-adjacent reads, so unknown values refuse
/// loudly instead of silently returning everything.
pub(crate) fn logs_query(
    query: &HashMap<String, String>,
) -> Result<
    (
        crate::observe::logs::LogFilter,
        crate::observe::logs::LogCursor,
        usize,
    ),
    Box<Response>,
> {
    use crate::observe::logs::{LogCursor, LogFilter};
    let bad =
        |message: &str| Box::new(nostore_json(StatusCode::BAD_REQUEST, "bad_filter", message));
    let q = match query.get("q") {
        Some(q) if q.chars().count() > 200 => return Err(bad("q is at most 200 characters")),
        Some(q) if q.is_empty() => None,
        Some(q) => Some(q.clone()),
        None => None,
    };
    let stream = match query.get("stream").map(String::as_str) {
        None | Some("") => None,
        Some("stdout") | Some("stderr") => query.get("stream").cloned(),
        Some(_) => return Err(bad("stream is stdout or stderr")),
    };
    let level = match query.get("level").map(String::as_str) {
        None | Some("") => None,
        Some("error") | Some("warn") | Some("info") => query.get("level").cloned(),
        Some(_) => return Err(bad("level is error, warn or info")),
    };
    let parse_ms = |key: &str| -> Result<Option<i64>, Box<Response>> {
        let Some(raw) = query.get(key) else {
            return Ok(None);
        };
        if raw.is_empty() {
            return Ok(None);
        }
        raw.parse::<i64>()
            .map(Some)
            .map_err(|_| bad("from/to are unix milliseconds"))
    };
    let from = parse_ms("from")?;
    let to = parse_ms("to")?;
    if let (Some(from), Some(to)) = (from, to) {
        if from > to {
            return Err(bad("from is past to"));
        }
    }
    let since = query.get("since").and_then(|s| s.parse::<u64>().ok());
    let before = query.get("before").and_then(|s| s.parse::<u64>().ok());
    let cursor = match (since, before) {
        (Some(seq), _) => LogCursor::Since(seq),
        (None, Some(seq)) => LogCursor::Before(seq),
        (None, None) => LogCursor::Tail,
    };
    let limit = query
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    Ok((
        LogFilter {
            q,
            stream,
            level,
            from,
            to,
        },
        cursor,
        limit,
    ))
}

async fn api_logs(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app, _role) = match visible_app(mcp, &account, &slug).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(),
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the app right now",
            );
        }
    };
    let (filter, cursor, limit) = match logs_query(&query) {
        Ok(parsed) => parsed,
        Err(res) => return *res,
    };
    let lines = crate::observe::logs::read_filtered(
        &mcp.pool,
        &mcp.fleets,
        &app.id,
        &app.slug,
        &filter,
        cursor,
        limit,
    )
    .await;
    let next_since = lines.last().map(|l| l.seq);
    let lines: Vec<serde_json::Value> = lines.iter().map(|l| l.to_json()).collect();
    (
        StatusCode::OK,
        [
            ("cache-control", "no-store"),
            ("content-type", "application/json"),
        ],
        json!({"lines": lines, "next_since": next_since}).to_string(),
    )
        .into_response()
}

/// `GET /api/console/apps/:slug/errors?status=`: the app's grouped runtime
/// errors, newest first, plus the proxy 5xx count over the trailing hour
/// (same rows as MCP `app_errors`). `status` is `open`, `resolved` or
/// `ignored`; unset returns every status, anything else is a 400.
async fn api_errors(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app, _role) = match visible_app(mcp, &account, &slug).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(),
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the app right now",
            );
        }
    };
    let status = query.get("status").map(String::as_str);
    if let Some(status) = status {
        if !matches!(status, "open" | "resolved" | "ignored") {
            return nostore_json(
                StatusCode::BAD_REQUEST,
                "bad_status",
                "status must be one of open, resolved, ignored",
            );
        }
    }
    let found = match crate::observe::logs::issues(&mcp.pool, &app.id, status).await {
        Ok(found) => found,
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load errors right now",
            );
        }
    };
    let s5xx = match crate::observe::logs::s5xx_last_hour(&mcp.pool, &app.id, crate::apps::now_ms())
        .await
    {
        Ok(s5xx) => s5xx,
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load errors right now",
            );
        }
    };
    let (open, resolved, ignored) =
        match crate::observe::logs::status_counts(&mcp.pool, &app.id).await {
            Ok(counts) => counts,
            Err(_) => {
                return nostore_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "unavailable",
                    "could not load errors right now",
                );
            }
        };
    ok_json(json!({
        "issues": found.iter().map(|issue| json!({
            "fingerprint": issue.fingerprint,
            "message": issue.message,
            "frame": issue.frame,
            "count": issue.count,
            "first_seen": issue.first_seen,
            "last_seen": issue.last_seen,
            "status": issue.status,
            "resolved_at": issue.resolved_at,
            "regressed": crate::observe::logs::is_regressed(issue),
        })).collect::<Vec<_>>(),
        "counts": {"open": open, "resolved": resolved, "ignored": ignored},
        "s5xx_last_hour": s5xx,
    }))
}
/// `GET /api/console/apps/:slug/secrets`: the app's secret names plus
/// `updated_at`, ordered by name. Values never leave the server. The caller
/// must see the app (see `visible_app`): anything else answers 404 exactly
/// like a missing app.
async fn api_secrets(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app, _role) = match visible_app(mcp, &account, &slug).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(),
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load secrets right now",
            );
        }
    };
    match crate::secrets::list(&mcp.pool, &app.id).await {
        Ok(secrets) => ok_json(json!({
            "secrets": secrets.iter().map(|s| json!({
                "name": s.name,
                "updated_at": s.updated_at,
            })).collect::<Vec<_>>(),
        })),
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "could not load secrets right now",
        ),
    }
}

/// `GET /api/console/apps/:slug/actions`: the app's agent action log,
/// newest first. `before` is the opaque `<created_at>:<id>` cursor of the
/// last row on the previous page; `limit` defaults to 20 and clamps to
/// 1–100. Answers `{actions, next?}` where `next` is the cursor for the
/// following page. The caller must see the app (see `visible_app`):
/// anything else answers 404 exactly like a missing app.
async fn api_actions(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app, _role) = match visible_app(mcp, &account, &slug).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(),
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the app right now",
            );
        }
    };
    let limit = query
        .get("limit")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(20)
        .clamp(1, 100);
    let before = match query.get("before") {
        None => None,
        Some(raw) => match parse_action_cursor(raw) {
            Some(cursor) => Some(cursor),
            None => {
                return nostore_json(
                    StatusCode::BAD_REQUEST,
                    "bad_cursor",
                    "before must be the next cursor from the previous page",
                );
            }
        },
    };
    // One extra row tells a full page from the last one.
    let rows = match crate::mcp::actions::list(&mcp.pool, &app.id, before, limit + 1).await {
        Ok(rows) => rows,
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the action log right now",
            );
        }
    };
    let (page, next) = match rows.len() as u64 > limit {
        true => (&rows[..limit as usize], {
            let last = &rows[limit as usize - 1];
            Some(action_cursor(last.created_at, &last.id))
        }),
        false => (&rows[..], None),
    };
    let actions: Vec<serde_json::Value> = page
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": row.id,
                "at": row.created_at,
                "tool": row.tool,
                "summary": row.summary,
                "outcome": row.outcome,
                "error_code": row.error_code,
                "duration_ms": row.duration_ms,
                "by": row.client_label,
            })
        })
        .collect();
    ok_json(json!({"actions": actions, "next": next}))
}

/// Parse the opaque `before` cursor back into the `(created_at, id)` pair
/// that `actions::list` pages on. Anything else is a 400.
fn parse_action_cursor(raw: &str) -> Option<(i64, String)> {
    let (at, id) = raw.split_once(':')?;
    let at: i64 = at.parse().ok()?;
    if at < 0 || id.is_empty() || id.contains(':') {
        return None;
    }
    Some((at, id.to_string()))
}

/// Render one `actions::list` page edge as the opaque `before` cursor.
fn action_cursor(created_at: i64, id: &str) -> String {
    format!("{created_at}:{id}")
}

/// `GET /api/console/apps/:slug/metrics?hours=`: the `metrics::summary`
/// view over 24 h, 7 d or 30 d. Anything but `24|168|720` is a 400.
async fn api_metrics(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app, _role) = match visible_app(mcp, &account, &slug).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(),
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the app right now",
            );
        }
    };
    let hours: i64 = match query.get("hours").and_then(|s| s.parse().ok()) {
        Some(hours @ (24 | 168 | 720)) => hours,
        _ => {
            return nostore_json(
                StatusCode::BAD_REQUEST,
                "bad_hours",
                "hours must be one of 24, 168, 720",
            );
        }
    };
    match crate::observe::metrics::summary(&mcp.pool, &app.id, hours).await {
        Ok(summary) => ok_json(serde_json::to_value(summary).unwrap_or(json!({}))),
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "could not load metrics right now",
        ),
    }
}

/// A 200 JSON answer with the same `no-store` rules as the error shape.
pub(crate) fn ok_json(body: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        [
            ("cache-control", "no-store"),
            ("content-type", "application/json"),
        ],
        body.to_string(),
    )
        .into_response()
}
async fn api_sign_out(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    let Some(session) = crate::auth::session_from_headers(&headers) else {
        return nostore_json(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "no session to sign out",
        );
    };
    // Same-origin gate, like every other /api/console request.
    if !fetch_ok(&headers) {
        return nostore_json(
            StatusCode::FORBIDDEN,
            "forbidden",
            "same-origin requests only",
        );
    }
    let hash = crate::auth::session_hash(&session);
    sqlx::query("DELETE FROM session WHERE token = ?")
        .bind(&hash)
        .execute(&mcp.pool)
        .await
        .ok();
    (
        StatusCode::OK,
        [
            ("cache-control", "no-store"),
            ("content-type", "application/json"),
            ("set-cookie", &crate::auth::clear_session_cookie() as &str),
        ],
        json!({"signed_out": true}).to_string(),
    )
        .into_response()
}

// --- Sign-in pages ---------------------------------------------------------

const SIGNIN_CSP: &str = "default-src 'none'; base-uri 'none'; form-action 'self'; \
    frame-ancestors 'none'; style-src 'unsafe-inline'";

fn layout(title: &str, body: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Outer console" }
            }
            body style="font-family:system-ui,sans-serif;max-width:28rem;margin:4rem auto;padding:0 1rem;color:#111" {
                h1 style="font-size:1.25rem" { "Outer console" }
                (body)
            }
        }
    }
}

fn email_form(error: Option<&str>, invite_open: bool, invite_value: &str) -> Markup {
    layout(
        "Sign in",
        html! {
            p { "Sign in with your email to see your apps." }
            @if let Some(error) = error {
                p style="color:#a00" { (error) }
            }
            form method="post" action="/console/sign-in" {
                input type="hidden" name="step" value="email";
                label {
                    "Email" br;
                    input type="email" name="email" required autocomplete="email"
                        style="width:100%;padding:.5rem;margin:.25rem 0";
                }
                br;
                details open[invite_open] {
                    summary { "Have an invite code?" }
                    p { "Outer is in closed beta. New here without an allowlisted email? Paste the code someone on Outer shared with you." }
                    label {
                        "Invite code (optional)" br;
                        input type="text" name="invite_code" value=(invite_value)
                            autocomplete="off" placeholder="OUTER-XXXX-XXXX"
                            style="width:100%;padding:.5rem;margin:.25rem 0";
                    }
                }
                br;
                button type="submit" style="padding:.5rem 1rem" { "Send code" }
            }
        },
    )
}

fn code_form(email: &str, error: Option<&str>) -> Markup {
    layout(
        "Enter code",
        html! {
            p { "We emailed a 6-digit code to " strong { (email) } "." }
            @if let Some(error) = error {
                p style="color:#a00" { (error) }
            }
            form method="post" action="/console/sign-in" {
                input type="hidden" name="step" value="code";
                input type="hidden" name="email" value=(email);
                label {
                    "Code" br;
                    input type="text" name="code" required inputmode="numeric"
                        autocomplete="one-time-code" style="width:100%;padding:.5rem;margin:.25rem 0";
                }
                br;
                button type="submit" style="padding:.5rem 1rem" { "Sign in" }
            }
        },
    )
}

fn terms_form(version: &str, error: Option<&str>) -> Markup {
    layout(
        "Terms",
        html! {
            p { "One more step: accept the Terms (version " (version) ") and Privacy Policy to continue." }
            @if let Some(error) = error {
                p style="color:#a00" { (error) }
            }
            form method="post" action="/console/sign-in" {
                input type="hidden" name="step" value="terms";
                label {
                    input type="checkbox" name="accept" value="yes" required;
                    " I accept the Terms and Privacy Policy"
                }
                br; br;
                button type="submit" style="padding:.5rem 1rem" { "Continue" }
            }
        },
    )
}

fn page(status: StatusCode, page: Markup, cookie: Option<String>) -> Response {
    let mut response = (
        status,
        [
            ("content-type", "text/html; charset=utf-8"),
            ("content-security-policy", SIGNIN_CSP),
            ("cache-control", "no-store"),
        ],
        crate::auth::pages::render(page),
    )
        .into_response();
    if let Some(cookie) = cookie {
        response.headers_mut().append(
            axum::http::header::SET_COOKIE,
            cookie.parse().expect("session cookie is header-safe"),
        );
    }
    response
}

fn redirect(to: &str, cookie: Option<String>) -> Response {
    let mut response = (StatusCode::SEE_OTHER, [("location", to)], "").into_response();
    if let Some(cookie) = cookie {
        response.headers_mut().append(
            axum::http::header::SET_COOKIE,
            cookie.parse().expect("session cookie is header-safe"),
        );
    }
    response
}

fn request_ip(
    headers: &HeaderMap,
    origin_locked: bool,
    peer: Option<SocketAddr>,
) -> Option<String> {
    client_ip(headers, origin_locked, peer)
}

/// `GET /console/sign-in`: a signed-in session with current terms skips
/// straight to `/console`; a session owing terms sees the terms step; else
/// the email form.
async fn sign_in_get(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    if let Some(session) = crate::auth::session_from_headers(&headers) {
        if let Ok(Some(account)) = crate::auth::account_from_session(&mcp.pool, &session).await {
            if account.terms_accepted(&mcp.auth.config) {
                return redirect("/console", None);
            }
            return page(
                StatusCode::OK,
                terms_form(&mcp.auth.config.terms_version, None),
                None,
            );
        }
    }
    page(StatusCode::OK, email_form(None, false, ""), None)
}

/// `POST /console/sign-in`: one step per form (`email` → `code` → `terms`).
/// Same codes, limits and beta gate as the OAuth email step; the session is
/// minted at `code`, so the `terms` step is already authenticated.
async fn sign_in_post(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    // Same-origin gate on every step: the terms step acts on the session
    // cookie, so a cross-site form must not be able to drive it.
    if !fetch_ok(&headers) {
        return page(
            StatusCode::FORBIDDEN,
            email_form(
                Some("Open the sign-in page on outer.now and try again."),
                false,
                "",
            ),
            None,
        );
    }
    let mcp = &state.mcp;
    let ip = request_ip(&headers, mcp.auth.config.origin_locked, Some(peer));
    match form.get("step").map(String::as_str) {
        Some("email") => {
            let email = form.get("email").cloned().unwrap_or_default();
            let invite_value = form
                .get("invite_code")
                .map(|raw| raw.trim().to_string())
                .unwrap_or_default();
            match crate::auth::send_code(
                &mcp.pool,
                &mcp.auth.config,
                &email,
                (!invite_value.is_empty()).then_some(invite_value.as_str()),
                ip.as_deref(),
            )
            .await
            {
                Ok(_) => page(
                    StatusCode::OK,
                    code_form(&crate::auth::normalize_email(&email), None),
                    None,
                ),
                Err(err) => {
                    let status = if err.code == "rate_limited" {
                        StatusCode::TOO_MANY_REQUESTS
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    page(
                        status,
                        email_form(
                            Some(crate::auth::pages::friendly_error(err.code)),
                            err.code == "closed_beta",
                            &invite_value,
                        ),
                        None,
                    )
                }
            }
        }
        Some("code") => {
            let email = form.get("email").cloned().unwrap_or_default();
            let code = form.get("code").cloned().unwrap_or_default();
            match crate::auth::verify_code_core(
                &mcp.pool,
                &mcp.auth.config,
                &email,
                &code,
                ip.as_deref(),
            )
            .await
            {
                Ok((account_id, terms_ok)) => {
                    let session = crate::auth::create_session(&mcp.pool, &account_id)
                        .await
                        .ok()
                        .map(|id| crate::auth::set_session_cookie(&id));
                    if !terms_ok {
                        return page(
                            StatusCode::OK,
                            terms_form(&mcp.auth.config.terms_version, None),
                            session,
                        );
                    }
                    redirect("/console", session)
                }
                Err(err) => page(
                    StatusCode::BAD_REQUEST,
                    code_form(
                        &crate::auth::normalize_email(&email),
                        Some(crate::auth::pages::friendly_error(err.code)),
                    ),
                    None,
                ),
            }
        }
        Some("terms") => {
            let Some(session) = crate::auth::session_from_headers(&headers) else {
                return page(
                    StatusCode::BAD_REQUEST,
                    email_form(Some("Sign in with email first."), false, ""),
                    None,
                );
            };
            let Ok(Some(account)) = crate::auth::account_from_session(&mcp.pool, &session).await
            else {
                return page(
                    StatusCode::BAD_REQUEST,
                    email_form(Some("Sign in with email first."), false, ""),
                    None,
                );
            };
            if form.get("accept").map(String::as_str) != Some("yes") {
                return page(
                    StatusCode::BAD_REQUEST,
                    terms_form(
                        &mcp.auth.config.terms_version,
                        Some("Accept the Terms and Privacy Policy to continue."),
                    ),
                    None,
                );
            }
            if crate::auth::record_terms_acceptance(&mcp.pool, &mcp.auth.config, &account.id)
                .await
                .is_err()
            {
                return page(
                    StatusCode::BAD_REQUEST,
                    terms_form(
                        &mcp.auth.config.terms_version,
                        Some("Sign-in is unavailable right now."),
                    ),
                    None,
                );
            }
            redirect("/console", None)
        }
        _ => page(
            StatusCode::BAD_REQUEST,
            email_form(Some("Unknown sign-in step."), false, ""),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::{captured, test_config, test_pool};
    use crate::auth::AuthConfig;
    use crate::store::snapshot::StateSync;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::SqlitePool;
    use tower::ServiceExt;

    const PEER: &str = "127.0.0.1:43123";

    fn peer(mut req: Request<Body>) -> Request<Body> {
        req.extensions_mut().insert(ConnectInfo(
            PEER.parse::<SocketAddr>().expect("test peer parses"),
        ));
        req
    }

    async fn app_with_config(config: AuthConfig) -> (Router, SqlitePool) {
        let pool = test_pool().await;
        let auth = AuthState {
            pool: pool.clone(),
            config,
        };
        let mcp = McpState::for_tests(pool.clone(), auth.clone());
        let missing = std::path::Path::new("/nonexistent-site");
        // The fully wired platform router: also pins that the console
        // routes win over the site fallback and the auth router.
        let app = super::super::router(super::super::AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
            auth,
            mcp,
            site: crate::http::site::SiteDir::new(missing),
        });
        (app, pool)
    }

    async fn test_app() -> (
        Router,
        SqlitePool,
        AuthConfig,
        std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
    ) {
        let (config, codes) = test_config();
        let (app, pool) = app_with_config(config.clone()).await;
        (app, pool, config, codes)
    }

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let res = app.clone().oneshot(peer(req)).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    fn same_origin(req: axum::http::request::Builder) -> axum::http::request::Builder {
        req.header("sec-fetch-site", "same-origin")
    }

    fn form(params: &[(&str, &str)]) -> String {
        let mut out = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in params {
            out.append_pair(key, value);
        }
        out.finish()
    }

    fn post_form(path: &str, body: &str, cookie: Option<&str>) -> Request<Body> {
        // Browsers send `Sec-Fetch-Site: same-origin` on the sign-in form.
        let mut req = same_origin(
            Request::post(path).header("content-type", "application/x-www-form-urlencoded"),
        );
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    fn session_cookie(headers: &HeaderMap) -> String {
        headers
            .get_all("set-cookie")
            .iter()
            .find_map(|value| {
                let value = value.to_str().ok()?;
                value
                    .starts_with("__Host-outer_sid=")
                    .then(|| value.split(';').next().unwrap().to_string())
            })
            .expect("session cookie set")
    }

    /// Drive the real email→code→terms flow over HTTP; returns the session
    /// cookie for later calls.
    async fn sign_in(
        app: &Router,
        codes: &std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
        email: &str,
    ) -> String {
        let (status, _, body) = send(
            app,
            post_form(
                "/console/sign-in",
                &form(&[("step", "email"), ("email", email)]),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("6-digit"), "code step renders, got: {body}");
        let code = captured(codes, &crate::auth::normalize_email(email));
        let (status, headers, body) = send(
            app,
            post_form(
                "/console/sign-in",
                &form(&[("step", "code"), ("email", email), ("code", &code)]),
                None,
            ),
        )
        .await;
        // Fresh accounts owe the terms step before any redirect.
        assert_eq!(status, StatusCode::OK, "terms step renders, got: {body}");
        assert!(body.contains("Terms"), "terms step renders, got: {body}");
        let cookie = session_cookie(&headers);
        let (status, headers, _) = send(
            app,
            post_form(
                "/console/sign-in",
                &form(&[("step", "terms"), ("accept", "yes")]),
                Some(&cookie),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers["location"], "/console");
        cookie
    }

    #[tokio::test]
    async fn api_without_cookie_is_401_with_nostore() {
        let (app, _) = app_with_config(test_config().0).await;
        let (status, headers, body) = send(
            &app,
            same_origin(Request::get("/api/console/me"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers["cache-control"], "no-store");
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["error"], "unauthorized");
    }

    #[tokio::test]
    async fn bearer_header_is_ignored_without_session_cookie() {
        let (app, pool, config, codes) = test_app().await;
        // A real account with a real API key: the Authorization header
        // must still not authenticate the console API.
        crate::auth::send_code(&pool, &config, "ana@example.com", None, None)
            .await
            .unwrap();
        let code = captured(&codes, "ana@example.com");
        let verified =
            crate::auth::otp_verify(&pool, &config, "ana@example.com", &code, true, None, None)
                .await
                .unwrap();
        let (status, _, _) = send(
            &app,
            same_origin(Request::get("/api/console/me"))
                .header("authorization", format!("Bearer {}", verified.api_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// Seed one account plus one app with one finished deploy.
    async fn seed_app(pool: &SqlitePool, owner: &str, slug: &str, now: i64) {
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
        sqlx::query(
            "INSERT INTO deploy (id, app_id, sha, status, created_at, finished_at) \
             VALUES (?, ?, 'abc123', 'success', ?, ?)",
        )
        .bind(format!("deploy-{slug}"))
        .bind(format!("app-{slug}"))
        .bind(now - 60_000)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A session cookie pair (`name=value`) for assertions that don't go
    /// through the HTTP sign-in: the account row already exists via
    /// `seed_app`, so the session is minted directly.
    async fn login(pool: &SqlitePool, email: &str) -> String {
        let id = crate::auth::create_session(pool, &format!("acct-{email}"))
            .await
            .unwrap();
        format!("__Host-outer_sid={id}")
    }

    fn authed(path: &str, cookie: &str) -> Request<Body> {
        same_origin(Request::get(path))
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn cross_site_fetch_metadata_is_403() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let session = login(&pool, "ana@example.com").await;
        // No metadata at all: refuse.
        let (status, _, _) = send(
            &app,
            Request::get("/api/console/me")
                .header("cookie", session.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Cross-site: refuse even with a valid session.
        let (status, _, _) = send(
            &app,
            Request::get("/api/console/me")
                .header("cookie", session.clone())
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Same-origin: through.
        let (status, _, body) = send(&app, authed("/api/console/me", &session)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(
            body,
            serde_json::json!({"email": "ana@example.com", "plan": "free", "is_admin": false})
        );
    }

    #[tokio::test]
    async fn apps_are_owner_scoped_with_last_deploy() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_app(&pool, "ben@example.com", "benblog", now).await;
        let session = login(&pool, "ana@example.com").await;

        let (status, _, body) = send(&app, authed("/api/console/apps", &session)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        let slugs: Vec<&str> = body["apps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs, vec!["groceries"], "only the caller's own apps list");
        let first = &body["apps"][0];
        assert_eq!(first["role"], "owner");
        assert_eq!(first["status"], "running");
        assert!(first["url"].as_str().unwrap().contains("groceries"));
        assert_eq!(first["last_deploy"]["sha"], "abc123");
        assert_eq!(first["last_deploy"]["status"], "success");
        assert_eq!(first["last_deploy"]["duration_ms"], 60_000);

        // The app detail carries the last 20 deploys, newest first.
        let (status, _, body) = send(&app, authed("/api/console/apps/groceries", &session)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["slug"], "groceries");
        assert_eq!(body["deploys"].as_array().unwrap().len(), 1);
        let deploy = &body["deploys"][0];
        assert_eq!(deploy["sha"], "abc123");
        assert_eq!(deploy["findings_count"], 0);
        assert!(
            deploy.get("message").is_none(),
            "success carries no message"
        );

        // A foreign slug answers exactly like a missing one.
        let (status, _, foreign) = send(&app, authed("/api/console/apps/benblog", &session)).await;
        let (missing_status, _, missing) =
            send(&app, authed("/api/console/apps/nope", &session)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(missing_status, StatusCode::NOT_FOUND);
        assert_eq!(foreign, missing);
        assert_eq!(
            foreign.parse::<serde_json::Value>().unwrap()["error"],
            "not_found"
        );
    }

    /// The terms step acts on the session cookie: a cross-site form must not
    /// drive any sign-in step.
    #[tokio::test]
    async fn sign_in_post_refuses_cross_site_forms() {
        let (app, _, _, codes) = test_app().await;
        let cookie = sign_in(&app, &codes, "ana@example.com").await;
        let (status, _, _) = send(
            &app,
            Request::post("/console/sign-in")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("sec-fetch-site", "cross-site")
                .header("cookie", &cookie)
                .body(Body::from(form(&[
                    ("step", "terms"),
                    ("accept_terms", "on"),
                ])))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn sign_in_flow_signs_out_and_respects_closed_signup() {
        let (app, _, config, codes) = test_app().await;
        let cookie = sign_in(&app, &codes, "ana@example.com").await;
        let (status, _, body) = send(&app, authed("/api/console/me", &cookie)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "session authenticates the API, got: {body}"
        );

        let (status, headers, _) = send(
            &app,
            same_origin(Request::post("/api/console/sign-out"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0"));
        let (status, _, _) = send(&app, authed("/api/console/me", &cookie)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "sign-out kills the session"
        );

        // Closed signup: a stranger gets the closed-beta page and no code.
        let mut closed = config.clone();
        closed.signup_open = false;
        let (closed_app, _) = app_with_config(closed).await;
        let (status, _, body) = send(
            &closed_app,
            post_form(
                "/console/sign-in",
                &form(&[("step", "email"), ("email", "stranger@example.com")]),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("closed beta"),
            "closed signup refuses, got: {body}"
        );
        assert!(
            codes.lock().unwrap().get("stranger@example.com").is_none(),
            "no code leaves for a waitlisted email"
        );
        // The allowlisted admin still gets a code under closed signup.
        let (status, _, _) = send(
            &closed_app,
            post_form(
                "/console/sign-in",
                &form(&[("step", "email"), ("email", "owner@example.com")]),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn console_routes_win_over_site_files() {
        // Even if a build emits `console/sign-in/index.html`, the Rust
        // handler answers `/console/sign-in`: the explicit route beats the
        // site fallback.
        let site_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(site_root.path().join("console/sign-in")).unwrap();
        std::fs::write(
            site_root.path().join("console/sign-in/index.html"),
            "<h1>stale static page</h1>",
        )
        .unwrap();
        let (config, _) = test_config();
        let pool = test_pool().await;
        let auth = AuthState {
            pool: pool.clone(),
            config,
        };
        let mcp = McpState::for_tests(pool.clone(), auth.clone());
        let app = super::super::router(super::super::AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
            auth,
            mcp,
            site: crate::http::site::SiteDir::new(site_root.path()),
        });
        let (status, _, body) = send(
            &app,
            Request::get("/console/sign-in")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("Outer console"),
            "Rust sign-in page wins, got: {body}"
        );
    }

    async fn seed_action(
        pool: &SqlitePool,
        app_id: &str,
        id: &str,
        tool: &str,
        outcome: &str,
        created_at: i64,
    ) {
        sqlx::query(
            "INSERT INTO agent_action (id, app_id, user_id, consent_id, apikey_id, \
             tool, summary, outcome, error_code, duration_ms, created_at) \
             VALUES (?1, ?2, 'acct-ana@example.com', NULL, NULL, ?3, 'did a thing', ?4, NULL, 120, ?5)",
        )
        .bind(id)
        .bind(app_id)
        .bind(tool)
        .bind(outcome)
        .bind(created_at)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn actions_and_metrics_need_session_and_same_origin() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let paths = [
            "/api/console/apps/groceries/actions".to_string(),
            "/api/console/apps/groceries/metrics?hours=24".to_string(),
            "/api/console/apps/groceries/errors".to_string(),
        ];
        for path in &paths {
            let (status, headers, _) = send(
                &app,
                same_origin(Request::get(path)).body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
            assert_eq!(headers["cache-control"], "no-store", "{path}");
        }
        let session = login(&pool, "ana@example.com").await;
        for path in &paths {
            let (status, _, _) = send(
                &app,
                Request::get(path)
                    .header("cookie", session.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        }
    }

    #[tokio::test]
    async fn actions_and_metrics_are_owner_scoped() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_app(&pool, "ben@example.com", "benblog", now).await;
        let session = login(&pool, "ana@example.com").await;
        for prefix in ["actions", "metrics?hours=24", "errors"] {
            let (status, _, foreign) = send(
                &app,
                authed(&format!("/api/console/apps/benblog/{prefix}"), &session),
            )
            .await;
            let (missing_status, _, missing) = send(
                &app,
                authed(&format!("/api/console/apps/nope/{prefix}"), &session),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{prefix}");
            assert_eq!(missing_status, StatusCode::NOT_FOUND, "{prefix}");
            assert_eq!(foreign, missing, "foreign answers like missing: {prefix}");
        }
    }

    #[tokio::test]
    async fn actions_rejects_bad_cursor() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let session = login(&pool, "ana@example.com").await;
        for before in ["bogus", "abc:xyz", "-5:zzz", "123:", ":zzz", "12:34:56"] {
            let (status, _, body) = send(
                &app,
                authed(
                    &format!("/api/console/apps/groceries/actions?before={before}"),
                    &session,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{before}");
            assert_eq!(
                body.parse::<serde_json::Value>().unwrap()["error"],
                "bad_cursor",
                "{before}"
            );
        }
        // No cursor: the first page, empty here.
        let (status, headers, body) = send(
            &app,
            authed("/api/console/apps/groceries/actions", &session),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["actions"], json!([]));
        assert!(body["next"].is_null());
    }

    #[tokio::test]
    async fn actions_page_newest_first_with_cursor_round_trip() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_action(
            &pool,
            "app-groceries",
            "a1",
            "get_app",
            "success",
            now - 3000,
        )
        .await;
        seed_action(
            &pool,
            "app-groceries",
            "a2",
            "commit",
            "blocked",
            now - 2000,
        )
        .await;
        seed_action(
            &pool,
            "app-groceries",
            "a3",
            "rollback",
            "failed",
            now - 2000,
        )
        .await;
        let session = login(&pool, "ana@example.com").await;
        let (status, _, body) = send(
            &app,
            authed("/api/console/apps/groceries/actions?limit=2", &session),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        let ids: Vec<&str> = body["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        // Same `created_at` breaks the tie by id, newest first.
        assert_eq!(ids, vec!["a3", "a2"]);
        let first = &body["actions"][0];
        assert_eq!(first["at"], now - 2000);
        assert_eq!(first["tool"], "rollback");
        assert_eq!(first["summary"], "did a thing");
        assert_eq!(first["outcome"], "failed");
        assert!(first["error_code"].is_null());
        assert_eq!(first["duration_ms"], 120);
        // NULL consent + API key IDs are console writes (console-actions
        // contract): the action log labels them "Console".
        assert_eq!(first["by"], "Console");
        let next = body["next"]
            .as_str()
            .expect("full page carries next")
            .to_string();
        let (status, _, body) = send(
            &app,
            authed(
                &format!("/api/console/apps/groceries/actions?limit=2&before={next}"),
                &session,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        let ids: Vec<&str> = body["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a1"]);
        assert!(body["next"].is_null(), "last page carries no cursor");
    }

    #[tokio::test]
    async fn metrics_rejects_bad_hours() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let session = login(&pool, "ana@example.com").await;
        for query in ["", "?hours=25", "?hours=abc", "?hours=0", "?hours=48"] {
            let (status, _, body) = send(
                &app,
                authed(
                    &format!("/api/console/apps/groceries/metrics{query}"),
                    &session,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query:?}");
            assert_eq!(
                body.parse::<serde_json::Value>().unwrap()["error"],
                "bad_hours",
                "{query:?}"
            );
        }
    }

    #[tokio::test]
    async fn metrics_happy_path_shape() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let minute = now / 60_000 * 60_000;
        let hour = now / 3_600_000 * 3_600_000;
        let hist = "[0,0,0,0,0,10,0,0,0,0,0,0,0,0]";
        sqlx::query(
            "INSERT INTO metric_minute (app_id, minute, requests, s4xx, s5xx, latency_hist, bytes_out, visitors) \
             VALUES ('app-groceries', ?1, 10, 2, 1, ?2, 0, 0)",
        )
        .bind(minute)
        .bind(hist)
        .execute(&pool)
        .await
        .unwrap();
        // Outside the 24 h window: must not leak in.
        sqlx::query(
            "INSERT INTO metric_minute (app_id, minute, requests, s4xx, s5xx, latency_hist, bytes_out, visitors) \
             VALUES ('app-groceries', ?1, 100, 0, 0, NULL, 0, 0)",
        )
        .bind(minute - 25 * 3_600_000)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO path_hour (app_id, hour, path, requests, s5xx) VALUES \
             ('app-groceries', ?1, '/', 10, 1), ('app-groceries', ?1, '/about', 4, 0)",
        )
        .bind(hour)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO visitor_day (app_id, day, visitors) VALUES ('app-groceries', ?1, 7)",
        )
        .bind(now.div_euclid(86_400_000))
        .execute(&pool)
        .await
        .unwrap();
        let session = login(&pool, "ana@example.com").await;
        let (status, headers, body) = send(
            &app,
            authed("/api/console/apps/groceries/metrics?hours=24", &session),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["requests"], 10);
        assert_eq!(body["visitors"], 7);
        assert_eq!(body["s4xx"], 2);
        assert_eq!(body["s5xx"], 1);
        assert_eq!(body["p50_ms"], 50);
        assert_eq!(body["p95_ms"], 50);
        let buckets = body["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 25, "zero-filled hourly buckets over 24 h");
        let total: i64 = buckets
            .iter()
            .map(|b| b["requests"].as_i64().unwrap())
            .sum();
        assert_eq!(total, 10);
        let paths = body["top_paths"].as_array().unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0]["path"], "/");
        assert_eq!(paths[0]["requests"], 10);
        assert_eq!(paths[0]["s5xx"], 1);
    }

    #[tokio::test]
    async fn errors_shape_rejects_bad_status_and_counts_s5xx() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        sqlx::query(
            "INSERT INTO error_issue (app_id, fingerprint, message, frame, count, first_seen, last_seen, status) \
             VALUES ('app-groceries', 'fp1', 'Error: kaboom', 'at handler (x:1:2)', 3, ?1, ?2, 'open')",
        )
        .bind(now - 1_000)
        .bind(now - 500)
        .execute(&pool)
        .await
        .unwrap();
        let minute = now / 60_000 * 60_000;
        sqlx::query(
            "INSERT INTO metric_minute (app_id, minute, requests, s4xx, s5xx, latency_hist, bytes_out, visitors) \
             VALUES ('app-groceries', ?1, 10, 0, 2, '', 0, 0)",
        )
        .bind(minute)
        .execute(&pool)
        .await
        .unwrap();
        let session = login(&pool, "ana@example.com").await;
        let (status, headers, body) =
            send(&app, authed("/api/console/apps/groceries/errors", &session)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["s5xx_last_hour"], 2);
        assert_eq!(
            body["counts"],
            serde_json::json!({"open": 1, "resolved": 0, "ignored": 0})
        );
        let found = body["issues"].as_array().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["fingerprint"], "fp1");
        assert_eq!(found[0]["message"], "Error: kaboom");
        assert_eq!(found[0]["frame"], "at handler (x:1:2)");
        assert_eq!(found[0]["count"], 3);
        assert_eq!(found[0]["status"], "open");
        assert!(found[0]["resolved_at"].is_null());
        assert_eq!(found[0]["regressed"], false);
        let (open_status, _, open_body) = send(
            &app,
            authed("/api/console/apps/groceries/errors?status=open", &session),
        )
        .await;
        assert_eq!(open_status, StatusCode::OK);
        assert_eq!(
            open_body.parse::<serde_json::Value>().unwrap()["issues"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let (resolved_status, _, _) = send(
            &app,
            authed(
                "/api/console/apps/groceries/errors?status=resolved",
                &session,
            ),
        )
        .await;
        assert_eq!(resolved_status, StatusCode::OK);
        let (bad_status, _, bad_body) = send(
            &app,
            authed("/api/console/apps/groceries/errors?status=bogus", &session),
        )
        .await;
        assert_eq!(bad_status, StatusCode::BAD_REQUEST);
        assert_eq!(
            bad_body.parse::<serde_json::Value>().unwrap()["error"],
            "bad_status"
        );
    }

    async fn seed_issue(
        pool: &SqlitePool,
        fp: &str,
        status: &str,
        resolved_at: Option<i64>,
        now: i64,
    ) {
        sqlx::query(
            "INSERT INTO error_issue (app_id, fingerprint, message, frame, count, first_seen, last_seen, status, resolved_at) \
             VALUES ('app-groceries', ?1, 'Error: kaboom', 'at handler (x:1:2)', 2, ?2, ?3, ?4, ?5)",
        )
        .bind(fp)
        .bind(now - 60_000)
        .bind(now - 1_000)
        .bind(status)
        .bind(resolved_at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_log(pool: &SqlitePool, seq: i64, ts_secs: i64, stream: &str, line: &str) {
        sqlx::query(
            "INSERT INTO app_log (app_id, seq, ts, stream, line) \
             VALUES ('app-groceries', ?1, ?2, ?3, ?4)",
        )
        .bind(seq)
        .bind(ts_secs)
        .bind(stream)
        .bind(line)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn errors_tabs_carry_counts_and_regressed_flags() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_issue(&pool, "fp-open", "open", None, now).await;
        seed_issue(&pool, "fp-regressed", "open", Some(now - 30_000), now).await;
        seed_issue(&pool, "fp-resolved", "resolved", Some(now - 30_000), now).await;
        seed_issue(&pool, "fp-ignored", "ignored", Some(now - 30_000), now).await;
        let session = login(&pool, "ana@example.com").await;
        let (_, _, body) = send(&app, authed("/api/console/apps/groceries/errors", &session)).await;
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(
            body["counts"],
            serde_json::json!({"open": 2, "resolved": 1, "ignored": 1}),
            "tabs count every status, not just the 50-issue page"
        );
        let by_fp = |fp: &str| {
            body["issues"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["fingerprint"] == fp)
                .cloned()
                .unwrap()
        };
        assert_eq!(by_fp("fp-open")["regressed"], false);
        assert_eq!(
            by_fp("fp-regressed")["regressed"],
            true,
            "open with a stamp is a regression"
        );
        assert_eq!(by_fp("fp-resolved")["regressed"], false);
        assert_eq!(by_fp("fp-ignored")["regressed"], false);
        assert_eq!(by_fp("fp-regressed")["resolved_at"], now - 30_000);
    }

    #[tokio::test]
    async fn errors_triage_moves_issues_and_audits_console() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_app(&pool, "ben@example.com", "benblog", now).await;
        seed_issue(&pool, "fp1", "open", None, now).await;
        let session = login(&pool, "ana@example.com").await;
        // Resolve moves the issue out of the open tab.
        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/errors/fp1/resolve",
                &session,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(
            body,
            serde_json::json!({"fingerprint": "fp1", "status": "resolved", "updated": 1})
        );
        let (_, _, open) = send(
            &app,
            authed("/api/console/apps/groceries/errors?status=open", &session),
        )
        .await;
        assert!(open.parse::<serde_json::Value>().unwrap()["issues"]
            .as_array()
            .unwrap()
            .is_empty());
        // Ignore then reopen round-trips the stamp.
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/errors/fp1/ignore",
                &session,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/errors/fp1/reopen",
                &session,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.parse::<serde_json::Value>().unwrap()["status"], "open");
        // Unknown fingerprints 404, exactly like a missing app.
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/errors/nope/resolve",
                &session,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // A foreign slug answers exactly like a missing one.
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/apps/benblog/errors/fp1/resolve",
                &session,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // No session: 401 even with the right shape.
        let (status, _, _) = send(
            &app,
            same_origin(Request::post(
                "/api/console/apps/groceries/errors/fp1/resolve",
            ))
            .body(Body::from("{}"))
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // The console mutation is an agent_action row under the MCP tool
        // name, and the action log labels its client Console.
        let row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT tool, consent_id, apikey_id FROM agent_action WHERE app_id = 'app-groceries' ORDER BY rowid DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "reopen_error");
        assert_eq!((row.1, row.2), (None, None));
        let (_, _, actions) = send(
            &app,
            authed("/api/console/apps/groceries/actions", &session),
        )
        .await;
        let actions: serde_json::Value = actions.parse().unwrap();
        let reopened = actions["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["tool"] == "reopen_error")
            .expect("console triage is in the action log");
        assert_eq!(reopened["by"], "Console");
    }

    #[tokio::test]
    async fn errors_bulk_moves_a_selection_with_bounds() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        for fp in ["fp1", "fp2", "fp3"] {
            seed_issue(&pool, fp, "open", None, now).await;
        }
        let session = login(&pool, "ana@example.com").await;
        let bulk = |action: &str, fps: serde_json::Value| {
            post_json(
                "/api/console/apps/groceries/errors/bulk",
                &session,
                serde_json::json!({"action": action, "fingerprints": fps}),
            )
        };
        let (status, _, body) = send(
            &app,
            bulk("resolve", serde_json::json!(["fp1", "fp2", "fpX"])),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(
            body,
            serde_json::json!({"action": "resolve", "status": "resolved", "updated": 2})
        );
        // Unknown action, empty list and oversize lists refuse loudly.
        for (action, fps) in [
            ("explode", serde_json::json!(["fp1"])),
            ("resolve", serde_json::json!([])),
            ("resolve", serde_json::json!(vec!["fp1"; 101])),
        ] {
            let (status, _, error) = send(&app, bulk(action, fps)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "action {action}");
            assert!(error.contains("bad_"), "error code, got: {error}");
        }
        // Unknown fingerprints match nothing but still audit zero moves.
        let (status, _, body) = send(&app, bulk("ignore", serde_json::json!(["fpX"]))).await;
        assert_eq!(status, StatusCode::OK);
        // Unknown fingerprints match nothing and audit nothing.
        assert_eq!(body.parse::<serde_json::Value>().unwrap()["updated"], 0);
    }

    #[tokio::test]
    async fn error_detail_serves_sparkline_and_recent_lines() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        // Seed through the real flush, so the stored fingerprint is the
        // one the log lines group to.
        let mut ring = LogRing::new(10);
        ring.push(LogStream::Stdout, "serving on :8080".to_string());
        ring.push(LogStream::Stderr, "Error: kaboom".to_string());
        ring.push(LogStream::Stderr, "    at handler (x:1:2)".to_string());
        crate::observe::logs::flush_fleet(&pool, "app-groceries", &ring.tail(10))
            .await
            .unwrap();
        let fp = crate::observe::logs::issues(&pool, "app-groceries", None)
            .await
            .unwrap()[0]
            .fingerprint
            .clone();
        let session = login(&pool, "ana@example.com").await;
        let (status, _, body) = send(
            &app,
            authed(
                &format!("/api/console/apps/groceries/errors/{fp}"),
                &session,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["issue"]["fingerprint"], fp);
        assert_eq!(body["issue"]["count"], 1);
        assert_eq!(body["issue"]["status"], "open");
        assert_eq!(body["issue"]["regressed"], false);
        let sparkline = body["sparkline"].as_array().unwrap();
        assert_eq!(sparkline.len(), 24, "24 hourly buckets");
        assert_eq!(
            sparkline
                .iter()
                .map(|b| b["count"].as_i64().unwrap())
                .sum::<i64>(),
            1,
            "the one retained event lands in the window"
        );
        let recent = body["recent"].as_array().unwrap();
        assert_eq!(recent.len(), 2, "the event's own lines");
        assert!(recent[0]["line"].as_str().unwrap().contains("kaboom"));
        assert_eq!(recent[0]["fingerprint"], fp);
        let (status, _, _) = send(
            &app,
            authed("/api/console/apps/groceries/errors/nope", &session),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn logs_filters_page_and_reject_bad_shapes() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let ts = now / 1000;
        seed_log(&pool, 0, ts, "stdout", "INFO serving on :8080").await;
        seed_log(&pool, 1, ts, "stdout", "console.log hello visitor").await;
        seed_log(&pool, 2, ts, "stderr", "ERROR disk full on vol-9").await;
        seed_log(&pool, 3, ts, "stderr", "WARN retrying fetch").await;
        let session = login(&pool, "ana@example.com").await;
        let get = |qs: &str| authed(&format!("/api/console/apps/groceries/logs{qs}"), &session);
        // Substring search is case-insensitive; stream and level filter.
        let (_, _, body) = send(&app, get("?q=DISK")).await;
        let lines = body.parse::<serde_json::Value>().unwrap()["lines"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0]["fingerprint"].as_str().unwrap().len(),
            64,
            "error lines link to their issue"
        );
        let (_, _, body) = send(&app, get("?stream=stderr&level=warn")).await;
        let lines = body.parse::<serde_json::Value>().unwrap()["lines"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(lines.len(), 1);
        assert!(lines[0]["line"].as_str().unwrap().contains("retrying"));
        // Time range in unix ms; a window ending an hour ago is empty.
        let (_, _, body) = send(
            &app,
            get(&format!("?from={}&to={}", now - 3_600_000, now + 60_000)),
        )
        .await;
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["lines"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        let (_, _, body) = send(&app, get(&format!("?to={}", now - 3_600_000))).await;
        assert!(body.parse::<serde_json::Value>().unwrap()["lines"]
            .as_array()
            .unwrap()
            .is_empty());
        // `Load older`: the newest lines strictly older than the seq.
        let (_, _, body) = send(&app, get("?limit=2")).await;
        let tail = body.parse::<serde_json::Value>().unwrap()["lines"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(tail[1]["line"].as_str().unwrap(), "WARN retrying fetch");
        assert_eq!(
            tail[1]["ts"].as_i64().unwrap(),
            ts * 1000,
            "console log timestamps are unix ms, like every other console time"
        );
        let before = tail[0]["seq"].as_u64().unwrap();
        let (_, _, body) = send(&app, get(&format!("?limit=2&before={before}"))).await;
        let older = body.parse::<serde_json::Value>().unwrap()["lines"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(older.len(), 2);
        assert!(older[0]["line"].as_str().unwrap().contains("serving"));
        // Bad shapes refuse loudly instead of silently returning everything.
        for qs in [
            "?stream=bogus",
            "?level=bogus",
            "?from=nope",
            "?from=5&to=4",
        ] {
            let (status, _, error) = send(&app, get(qs)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "query {qs}");
            let error: serde_json::Value = error.parse().unwrap();
            assert!(
                error["error"].as_str().unwrap().starts_with("bad_"),
                "query {qs}"
            );
        }
        let long_q = "q=".to_string() + &"x".repeat(201);
        let (status, _, _) = send(&app, get(&format!("?{long_q}"))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn logs_stream_emits_the_live_tail() {
        use http_body_util::BodyExt as _;
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_log(&pool, 0, now / 1000, "stdout", "hello persisted").await;
        let session = login(&pool, "ana@example.com").await;
        let res = app
            .clone()
            .oneshot(peer(authed(
                "/api/console/apps/groceries/logs/stream",
                &session,
            )))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["cache-control"], "no-store");
        assert!(
            res.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream"),
            "SSE content type"
        );
        let mut body = res.into_body();
        // The first tick always emits (possibly empty), so a few seconds
        // of waiting proves the stream is live without hanging the suite.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), body.frame())
            .await
            .expect("first SSE tick")
            .expect("frame ok")
            .expect("first frame");
        let chunk = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(chunk.starts_with("data: "), "SSE data frame, got: {chunk}");
        assert!(
            chunk.contains("\"lines\""),
            "log page payload, got: {chunk}"
        );
    }

    #[tokio::test]
    async fn secrets_need_session_and_same_origin() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        // No cookie: 401, even same-origin.
        let (status, _, _) = send(
            &app,
            same_origin(Request::get("/api/console/apps/groceries/secrets"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // Valid session but cross-site: 403.
        let session = login(&pool, "ana@example.com").await;
        let (status, _, _) = send(
            &app,
            Request::get("/api/console/apps/groceries/secrets")
                .header("cookie", session.clone())
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn secrets_are_owner_scoped_and_names_only() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_app(&pool, "ben@example.com", "benblog", now).await;
        let key = crate::secrets::SecretsKey::test_key();
        for name in ["STRIPE_KEY", "API_TOKEN"] {
            let enc = crate::secrets::encrypt(&key, "app-groceries", name, b"shh");
            let capped = crate::secrets::set(&pool, "app-groceries", name, &enc, now)
                .await
                .unwrap();
            assert!(!capped);
        }
        let session = login(&pool, "ana@example.com").await;
        let (status, headers, body) = send(
            &app,
            authed("/api/console/apps/groceries/secrets", &session),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        let body: serde_json::Value = body.parse().unwrap();
        let names: Vec<&str> = body["secrets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["API_TOKEN", "STRIPE_KEY"], "ordered by name");
        for secret in body["secrets"].as_array().unwrap() {
            assert_eq!(
                secret.as_object().unwrap().len(),
                2,
                "name + updated_at only, never a value: {secret}"
            );
            assert_eq!(secret["updated_at"], now);
        }
        let raw = body.to_string();
        assert!(!raw.contains("shh"), "no plaintext value in the response");
        // A foreign slug answers exactly like a missing one.
        let (foreign_status, _, foreign_body) =
            send(&app, authed("/api/console/apps/benblog/secrets", &session)).await;
        let (missing_status, _, missing) =
            send(&app, authed("/api/console/apps/nope/secrets", &session)).await;
        assert_eq!(foreign_status, StatusCode::NOT_FOUND);
        assert_eq!(missing_status, StatusCode::NOT_FOUND);
        assert_eq!(foreign_body, missing);
        assert_eq!(
            foreign_body.parse::<serde_json::Value>().unwrap()["error"],
            "not_found"
        );
    }
    fn post_json(path: &str, cookie: &str, body: serde_json::Value) -> Request<Body> {
        same_origin(Request::post(path).header("content-type", "application/json"))
            .header("cookie", cookie)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// A bare account (no apps): the invite endpoints need none.
    async fn seed_account(pool: &SqlitePool, email: &str) {
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, 0, 0, 0)",
        )
        .bind(format!("acct-{email}"))
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn console_me_reports_is_admin() {
        let (app, pool, _, _) = test_app().await;
        seed_account(&pool, "ana@example.com").await;
        seed_account(&pool, "owner@example.com").await;
        let ana = login(&pool, "ana@example.com").await;
        let owner = login(&pool, "owner@example.com").await;

        let (status, _, body) = send(&app, authed("/api/console/me", &ana)).await;
        assert_eq!(status, StatusCode::OK);
        let me: serde_json::Value = body.parse().unwrap();
        assert_eq!(me["is_admin"], false);
        let (status, _, body) = send(&app, authed("/api/console/me", &owner)).await;
        assert_eq!(status, StatusCode::OK);
        let me: serde_json::Value = body.parse().unwrap();
        assert_eq!(me["is_admin"], true, "OUTER_ADMIN_EMAILS decides");
    }

    #[tokio::test]
    async fn console_admin_endpoints_forbid_non_admins() {
        let (app, pool, _, _) = test_app().await;
        seed_account(&pool, "ana@example.com").await;
        let ana = login(&pool, "ana@example.com").await;

        let (status, _, body) = send(&app, authed("/api/console/admin/invites", &ana)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["error"],
            "forbidden"
        );
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/admin/invites",
                &ana,
                serde_json::json!({"count": 1}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/admin/invites/OUTER-AAAA-BBBB/revoke",
                &ana,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = send(&app, authed("/api/console/admin/waitlist", &ana)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn console_invites_list_own_codes_with_status() {
        let (app, pool, _, _) = test_app().await;
        seed_account(&pool, "ana@example.com").await;
        seed_account(&pool, "owner@example.com").await;
        let first =
            crate::auth::invites::mint_admin(&pool, "acct-ana@example.com", 1, Some("beta"), 7)
                .await
                .unwrap();
        let second =
            crate::auth::invites::mint_admin(&pool, "acct-ana@example.com", 1, Some("beta"), 8)
                .await
                .unwrap();
        let made = [first[0].clone(), second[0].clone()];
        // One code joined by the owner, one revoked.
        sqlx::query("UPDATE invite_code SET redeemedBy = 'acct-owner@example.com', redeemedAt = 9 WHERE code = ?")
            .bind(&made[0])
            .execute(&pool)
            .await
            .unwrap();
        crate::auth::invites::revoke(&pool, &made[1], 10)
            .await
            .unwrap();
        let ana = login(&pool, "ana@example.com").await;

        let (status, headers, body) = send(&app, authed("/api/console/invites", &ana)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        let list: serde_json::Value = body.parse().unwrap();
        let codes = list["codes"].as_array().unwrap();
        assert_eq!(codes.len(), 2);
        assert_eq!(codes[0]["code"], made[1].as_str(), "newest first");
        assert_eq!(codes[0]["kind"], "admin");
        assert_eq!(codes[0]["note"], "beta");
        assert_eq!(codes[0]["revokedAt"], 10);
        assert_eq!(codes[0]["redeemedByEmail"], serde_json::Value::Null);
        assert_eq!(codes[1]["code"], made[0].as_str());
        assert_eq!(codes[1]["redeemedAt"], 9);
        assert_eq!(codes[1]["redeemedByEmail"], "owner@example.com");
        assert_eq!(codes[1]["revokedAt"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn console_admin_invites_create_list_revoke() {
        let (app, pool, _, _) = test_app().await;
        seed_account(&pool, "owner@example.com").await;
        let owner = login(&pool, "owner@example.com").await;

        for (count, want) in [(0, StatusCode::BAD_REQUEST), (51, StatusCode::BAD_REQUEST)] {
            let (status, _, _) = send(
                &app,
                post_json(
                    "/api/console/admin/invites",
                    &owner,
                    serde_json::json!({"count": count}),
                ),
            )
            .await;
            assert_eq!(status, want, "count {count} refuses");
        }
        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/admin/invites",
                &owner,
                serde_json::json!({"count": 3, "note": "beta friends"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let created: serde_json::Value = body.parse().unwrap();
        assert_eq!(created["codes"].as_array().unwrap().len(), 3);
        let first = created["codes"][0].as_str().unwrap().to_string();

        // Paging: two per page, newest first, cursor continues.
        let (status, _, body) =
            send(&app, authed("/api/console/admin/invites?limit=2", &owner)).await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = body.parse().unwrap();
        assert_eq!(page["codes"].as_array().unwrap().len(), 2);
        for code in page["codes"].as_array().unwrap() {
            assert_eq!(code["createdByEmail"], "owner@example.com");
            assert_eq!(code["note"], "beta friends");
        }
        let next = page["next"].as_str().unwrap().to_string();
        let (status, _, body) = send(
            &app,
            authed(
                &format!("/api/console/admin/invites?limit=2&before={next}"),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rest: serde_json::Value = body.parse().unwrap();
        assert_eq!(rest["codes"].as_array().unwrap().len(), 1);
        assert!(rest.get("next").is_none(), "last page carries no cursor");

        // Revoke an unused code, then refuse the redeemed one.
        let (status, _, body) = send(
            &app,
            post_json(
                &format!("/api/console/admin/invites/{first}/revoke"),
                &owner,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.parse::<serde_json::Value>().unwrap()["revoked"], true);
        sqlx::query("UPDATE invite_code SET redeemedBy = 'acct-owner@example.com', redeemedAt = 1 WHERE code = ?")
            .bind(&first)
            .execute(&pool)
            .await
            .unwrap();
        let (status, _, body) = send(
            &app,
            post_json(
                &format!("/api/console/admin/invites/{first}/revoke"),
                &owner,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["error"],
            "already_used"
        );
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/admin/invites/OUTER-ZZZZ-YYYY/revoke",
                &owner,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn console_admin_waitlist_lists_entries() {
        let (app, pool, _, _) = test_app().await;
        seed_account(&pool, "owner@example.com").await;
        sqlx::query(
            "INSERT INTO waitlist (email, first_seen, attempts) VALUES \
             ('early@example.com', 5, 2), ('late@example.com', 9, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let owner = login(&pool, "owner@example.com").await;

        let (status, _, body) = send(&app, authed("/api/console/admin/waitlist", &owner)).await;
        assert_eq!(status, StatusCode::OK);
        let list: serde_json::Value = body.parse().unwrap();
        let entries = list["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["email"], "late@example.com", "newest first");
        assert_eq!(entries[1]["attempts"], 2);
    }

    fn put_json(path: &str, cookie: &str, body: serde_json::Value) -> Request<Body> {
        same_origin(Request::put(path).header("content-type", "application/json"))
            .header("cookie", cookie)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn delete_authed(path: &str, cookie: &str) -> Request<Body> {
        same_origin(Request::delete(path))
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    }

    async fn action_rows(pool: &SqlitePool) -> Vec<(String, String, Option<String>)> {
        sqlx::query_as("SELECT tool, outcome, error_code FROM agent_action ORDER BY created_at")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn console_visibility_pro_gate() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let session = login(&pool, "ana@example.com").await;
        let path = "/api/console/apps/groceries/visibility";

        // Free owner: `public` is refused with the MCP code and a user
        // message pointing to Pro.
        let (status, _, body) = send(
            &app,
            post_json(path, &session, serde_json::json!({"visibility": "public"})),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["error"], "payment_required");
        assert!(
            body["user_message"].as_str().unwrap().contains("Pro"),
            "{body}"
        );

        // `unlisted` is always allowed, on any plan.
        let (status, _, body) = send(
            &app,
            post_json(
                path,
                &session,
                serde_json::json!({"visibility": "unlisted"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["visibility"],
            "unlisted"
        );

        // Pro owner: `public` flips the row.
        sqlx::query("UPDATE user SET plan = 'pro' WHERE id = 'acct-ana@example.com'")
            .execute(&pool)
            .await
            .unwrap();
        let (status, _, body) = send(
            &app,
            post_json(path, &session, serde_json::json!({"visibility": "public"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["visibility"],
            "public"
        );
        let visibility: Option<String> =
            sqlx::query_scalar("SELECT visibility FROM app WHERE slug = 'groceries'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(visibility.as_deref(), Some("public"));

        // Every attempt audited under the MCP tool name, with no grant or
        // key ids — the log labels the client "Console".
        let rows = action_rows(&pool).await;
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(
            rows.iter().all(|(tool, _, _)| tool == "set_visibility"),
            "{rows:?}"
        );
        assert!(rows.iter().any(|(_, outcome, _)| outcome == "ok"));
        assert!(rows.iter().any(|(_, outcome, code)| outcome == "error"
            && *code == Some("payment_required".to_string())));
        let ids: Vec<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT consent_id, apikey_id FROM agent_action")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            ids.iter().all(|(c, k)| c.is_none() && k.is_none()),
            "{ids:?}"
        );
        let listed = crate::mcp::actions::list(&pool, "app-groceries", None, 10)
            .await
            .unwrap();
        assert_eq!(listed.len(), 3);
        assert!(
            listed.iter().all(|row| row.client_label == "Console"),
            "{listed:?}"
        );
    }

    #[tokio::test]
    async fn console_visibility_rejects_bad_value_foreign_slugs_and_cross_site() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_app(&pool, "bob@example.com", "bobshop", now).await;
        let ana = login(&pool, "ana@example.com").await;
        let bob = login(&pool, "bob@example.com").await;
        let path = "/api/console/apps/groceries/visibility";

        let (status, _, body) = send(
            &app,
            post_json(path, &ana, serde_json::json!({"visibility": "everyone"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["error"],
            "invalid_argument"
        );

        // Another owner's slug answers 404, exactly like a missing one.
        let (status, _, _) = send(
            &app,
            post_json(path, &bob, serde_json::json!({"visibility": "unlisted"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Same-origin required, like every other /api/console route.
        let (status, _, _) = send(
            &app,
            Request::post(path)
                .header("content-type", "application/json")
                .header("cookie", ana)
                .body(Body::from(r#"{"visibility":"unlisted"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn console_rename_moves_the_app_and_audits_as_console() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let cookie = login(&pool, "ana@example.com").await;

        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/rename",
                &cookie,
                serde_json::json!({"slug": "pantry"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let out: serde_json::Value = body.parse().unwrap();
        assert_eq!(out["slug"], "pantry");
        assert_eq!(out["url"], "http://pantry.outer.localhost");

        // The row flipped and the old slug is quarantined with its target.
        let id: String = sqlx::query_scalar("SELECT id FROM app WHERE slug = 'pantry'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(id, "app-groceries");
        let row: (i64, Option<String>) = sqlx::query_as(
            "SELECT until, renamed_to FROM slug_quarantine WHERE slug = 'groceries'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(row.0 > now);
        assert_eq!(row.1.as_deref(), Some("pantry"));
        // Never deployed: nothing redeployed, so no new deploy rows.
        let deploys: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM deploy WHERE app_id = 'app-groceries'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(deploys, 1);

        // The audit row is the MCP tool name with both credential columns
        // NULL, and the action log reads that pair as "Console".
        let audit: (String, String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT tool, summary, consent_id, apikey_id FROM agent_action WHERE app_id = 'app-groceries' AND tool = 'rename_app'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(audit.1, "rename_app groceries to pantry");
        assert!(audit.2.is_none() && audit.3.is_none());
        let (status, _, body) =
            send(&app, authed("/api/console/apps/pantry/actions", &cookie)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let actions: serde_json::Value = body.parse().unwrap();
        let entry = actions["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["tool"] == "rename_app")
            .expect("rename audit row listed");
        assert_eq!(entry["by"], "Console");
        assert_eq!(entry["summary"], "rename_app groceries to pantry");
    }

    #[tokio::test]
    async fn console_rename_needs_same_origin_owner_and_shape() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_app(&pool, "bob@example.com", "bobapp", now).await;
        let cookie = login(&pool, "ana@example.com").await;

        // No same-origin metadata: refuse before touching anything.
        let (status, _, _) = send(
            &app,
            Request::post("/api/console/apps/groceries/rename")
                .header("cookie", cookie.clone())
                .header("content-type", "application/json")
                .body(Body::from(r#"{"slug":"pantry"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Foreign slug answers exactly like a missing one.
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/apps/bobapp/rename",
                &cookie,
                serde_json::json!({"slug": "bobbins"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Missing and misshapen bodies are 400 with the MCP error codes.
        for body in [
            serde_json::json!({}),
            serde_json::json!({"slug": ""}),
            serde_json::json!({"slug": "Bad_Shape"}),
        ] {
            let (status, _, text) = send(
                &app,
                post_json("/api/console/apps/groceries/rename", &cookie, body.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let err: serde_json::Value = text.parse().unwrap();
            assert_eq!(err["error"], "invalid_argument", "{body}");
            assert!(err["message"].is_string(), "{body}");
            assert!(err["user_message"].is_null(), "{body}");
        }
        // Nothing moved.
        let slug: String = sqlx::query_scalar("SELECT slug FROM app WHERE id = 'app-groceries'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(slug, "groceries");
    }

    #[tokio::test]
    async fn console_delete_requires_confirm_and_is_owner_scoped() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_app(&pool, "bob@example.com", "bobshop", now).await;
        let ana = login(&pool, "ana@example.com").await;
        let bob = login(&pool, "bob@example.com").await;
        let path = "/api/console/apps/groceries/delete";

        // A wrong confirm refuses and deletes nothing.
        let (status, _, body) = send(
            &app,
            post_json(path, &ana, serde_json::json!({"confirm": "grocerie"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["error"],
            "invalid_argument"
        );
        let (status, _, body) = send(&app, authed("/api/console/apps", &ana)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("groceries"), "{body}");

        // Another owner's slug answers 404, exactly like a missing one.
        let (status, _, _) = send(
            &app,
            post_json(path, &bob, serde_json::json!({"confirm": "groceries"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // The exact slug deletes: the rail drops the app and the slug
        // quarantines.
        let (status, _, body) = send(
            &app,
            post_json(path, &ana, serde_json::json!({"confirm": "groceries"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["deleted"], true);
        let (status, _, body) = send(&app, authed("/api/console/apps", &ana)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("groceries"), "{body}");
        let quarantined: Option<i64> =
            sqlx::query_scalar("SELECT until FROM slug_quarantine WHERE slug = 'groceries'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert!(quarantined.is_some_and(|until| until > now));
    }

    #[tokio::test]
    async fn console_secrets_round_trip_never_returns_values() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_app(&pool, "bob@example.com", "bobshop", now).await;
        let ana = login(&pool, "ana@example.com").await;
        let bob = login(&pool, "bob@example.com").await;
        let put_path = "/api/console/apps/groceries/secrets/API_KEY";

        let (status, _, body) = send(
            &app,
            put_json(put_path, &ana, serde_json::json!({"value": "s3cr3t-value"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["name"], "API_KEY");
        assert_eq!(body["applies"], "next deploy");
        assert!(body["updated_at"].as_i64().is_some_and(|at| at >= now));

        // Neither the write nor the read ever carries the value.
        let (status, _, body) =
            send(&app, authed("/api/console/apps/groceries/secrets", &ana)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("s3cr3t-value"), "{body}");
        let list: serde_json::Value = body.parse().unwrap();
        assert_eq!(list["secrets"].as_array().unwrap().len(), 1);
        assert_eq!(list["secrets"][0]["name"], "API_KEY");

        // Replace re-validates: lowercase and oversize values refuse.
        let (status, _, _) = send(
            &app,
            put_json(
                "/api/console/apps/groceries/secrets/lower",
                &ana,
                serde_json::json!({"value": "x"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let big = "v".repeat(crate::secrets::MAX_VALUE_BYTES + 1);
        let (status, _, _) = send(
            &app,
            put_json(put_path, &ana, serde_json::json!({"value": big})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Another owner sees 404 on the write.
        let (status, _, _) = send(
            &app,
            put_json(put_path, &bob, serde_json::json!({"value": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Remove, then removing again is 404.
        let (status, _, body) = send(&app, delete_authed(put_path, &ana)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["name"],
            "API_KEY"
        );
        let (status, _, _) = send(&app, delete_authed(put_path, &ana)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // The secret mutations audited under the MCP tool names.
        let rows = action_rows(&pool).await;
        let tools: Vec<&str> = rows.iter().map(|(tool, _, _)| tool.as_str()).collect();
        assert!(tools.contains(&"set_secret"), "{tools:?}");
        assert!(tools.contains(&"unset_secret"), "{tools:?}");
    }

    #[tokio::test]
    async fn console_rollback_refuses_unknown_sha_without_spending_quota() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let ana = login(&pool, "ana@example.com").await;

        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/rollback",
                &ana,
                serde_json::json!({"sha": "0123456789abcdef0123456789abcdef01234567"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["error"], "invalid_argument");

        // Refused before the claim: the daily rollback budget is untouched.
        let spent: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM rollback_day WHERE app_id = 'app-groceries'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(spent, 0);
        let rows = action_rows(&pool).await;
        assert!(
            rows.iter().any(|(tool, outcome, code)| tool == "rollback"
                && outcome == "error"
                && *code == Some("invalid_argument".to_string())),
            "{rows:?}"
        );
    }

    async fn seed_pro_app(pool: &SqlitePool, owner: &str, slug: &str, now: i64) {
        // seed_app inserts the account row too; the plan flips after.
        seed_app(pool, owner, slug, now).await;
        sqlx::query("UPDATE user SET plan = 'pro' WHERE email = ?1")
            .bind(owner)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn console_collaborators_owner_invite_and_remove_flow() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_pro_app(&pool, "ana@example.com", "groceries", now).await;
        let owner = login(&pool, "ana@example.com").await;

        let (status, _, body) = send(
            &app,
            authed("/api/console/apps/groceries/collaborators", &owner),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let list: serde_json::Value = body.parse().unwrap();
        assert_eq!(list["role"], "owner");
        assert_eq!(list["owner_plan"], "pro");
        assert!(list["collaborators"].as_array().unwrap().is_empty());

        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/collaborators",
                &owner,
                serde_json::json!({"email": "bruno@example.com"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let added: serde_json::Value = body.parse().unwrap();
        assert_eq!(added["collaborator"]["email"], "bruno@example.com");
        assert_eq!(added["collaborator"]["status"], "pending");

        // The mutation is audited as the MCP tool with client "Console".
        let rows = crate::mcp::actions::list(&pool, "app-groceries", None, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, "add_collaborator");
        assert_eq!(rows[0].client_label, "Console");

        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/collaborators/remove",
                &owner,
                serde_json::json!({"email": "bruno@example.com"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let removed: serde_json::Value = body.parse().unwrap();
        assert_eq!(removed["removed"], true);
        let (status, _, body) = send(
            &app,
            authed("/api/console/apps/groceries/collaborators", &owner),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let list: serde_json::Value = body.parse().unwrap();
        assert!(list["collaborators"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn console_collaborators_are_scoped_like_the_tools() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_pro_app(&pool, "ana@example.com", "groceries", now).await;
        seed_account(&pool, "bruno@example.com").await;
        seed_account(&pool, "stranger@example.com").await;
        sqlx::query(
            "INSERT INTO app_collaborator (app_id, email, userId, added_at) \
             VALUES ('app-groceries', 'bruno@example.com', 'acct-bruno@example.com', ?1)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let bruno = login(&pool, "bruno@example.com").await;
        let stranger = login(&pool, "stranger@example.com").await;

        // The app shows in the collaborator's rail and detail as Shared.
        let (status, _, body) = send(&app, authed("/api/console/apps", &bruno)).await;
        assert_eq!(status, StatusCode::OK);
        let list: serde_json::Value = body.parse().unwrap();
        let apps = list["apps"].as_array().unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0]["slug"], "groceries");
        assert_eq!(apps[0]["role"], "collaborator");
        let (status, _, body) = send(&app, authed("/api/console/apps/groceries", &bruno)).await;
        assert_eq!(status, StatusCode::OK);
        let detail: serde_json::Value = body.parse().unwrap();
        assert_eq!(detail["role"], "collaborator");

        // Collaborator reads (list, secrets) but never invites.
        let (status, _, _) = send(
            &app,
            authed("/api/console/apps/groceries/collaborators", &bruno),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) =
            send(&app, authed("/api/console/apps/groceries/secrets", &bruno)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/collaborators",
                &bruno,
                serde_json::json!({"email": "x@example.com"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let refused: serde_json::Value = body.parse().unwrap();
        assert_eq!(refused["error"], "forbidden");
        assert!(refused["user_message"].as_str().unwrap().contains("owner"));

        // No role reads exactly like a missing app.
        let (status, _, _) = send(&app, authed("/api/console/apps/groceries", &stranger)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/collaborators",
                &stranger,
                serde_json::json!({"email": "x@example.com"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn console_collaborator_invite_needs_a_pro_owner() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        let owner = login(&pool, "ana@example.com").await;
        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/collaborators",
                &owner,
                serde_json::json!({"email": "bruno@example.com"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        let refused: serde_json::Value = body.parse().unwrap();
        assert_eq!(refused["error"], "payment_required");
        assert!(refused["user_message"].as_str().unwrap().contains("Pro"));
    }

    #[tokio::test]
    async fn console_mutations_scope_collaborators_like_the_tools() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        seed_account(&pool, "bruno@example.com").await;
        seed_account(&pool, "stranger@example.com").await;
        sqlx::query("UPDATE user SET plan = 'pro' WHERE email = 'ana@example.com'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO app_collaborator (app_id, email, userId, added_at) \
             VALUES ('app-groceries', 'bruno@example.com', 'acct-bruno@example.com', ?1)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let bruno = login(&pool, "bruno@example.com").await;
        let stranger = login(&pool, "stranger@example.com").await;

        // Rollback admits collaborators like the MCP tool: the scope gate
        // passes and the unknown sha refuses as `invalid_argument`.
        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/rollback",
                &bruno,
                serde_json::json!({"sha": "0123456789abcdef0123456789abcdef01234567"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["error"],
            "invalid_argument"
        );

        // Visibility, rename, secret writes and delete stay owner-only: a
        // collaborator reads 403 with the MCP code and a user message.
        for (path, body) in [
            (
                "/api/console/apps/groceries/visibility",
                serde_json::json!({"visibility": "unlisted"}),
            ),
            (
                "/api/console/apps/groceries/rename",
                serde_json::json!({"slug": "brand-new"}),
            ),
            (
                "/api/console/apps/groceries/delete",
                serde_json::json!({"confirm": "groceries"}),
            ),
        ] {
            let (status, _, body) = send(&app, post_json(path, &bruno, body)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
            let refused: serde_json::Value = body.parse().unwrap();
            assert_eq!(refused["error"], "forbidden", "{path}");
            assert!(
                refused["user_message"].as_str().unwrap().contains("owner"),
                "{path}: {refused}"
            );
        }
        let (status, _, body) = send(
            &app,
            put_json(
                "/api/console/apps/groceries/secrets/API_KEY",
                &bruno,
                serde_json::json!({"value": "x"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let refused: serde_json::Value = body.parse().unwrap();
        assert_eq!(refused["error"], "forbidden");
        assert!(
            refused["user_message"].as_str().unwrap().contains("owner"),
            "{refused}"
        );

        // A stranger's slug reads 404 exactly like a missing one.
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/visibility",
                &stranger,
                serde_json::json!({"visibility": "unlisted"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/rollback",
                &stranger,
                serde_json::json!({"sha": "0123456789abcdef0123456789abcdef01234567"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn console_collaborator_may_triage_and_read_observe() {
        let (app, pool, _, _) = test_app().await;
        let now = crate::auth::now_ms();
        seed_app(&pool, "ana@example.com", "groceries", now).await;
        sqlx::query("UPDATE user SET plan = 'pro' WHERE email = 'ana@example.com'")
            .execute(&pool)
            .await
            .unwrap();
        seed_account(&pool, "bruno@example.com").await;
        seed_account(&pool, "stranger@example.com").await;
        sqlx::query(
            "INSERT INTO app_collaborator (app_id, email, userId, added_at) \
             VALUES ('app-groceries', 'bruno@example.com', 'acct-bruno@example.com', ?1)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        seed_issue(&pool, "fp1", "open", None, now).await;
        let bruno = login(&pool, "bruno@example.com").await;
        let stranger = login(&pool, "stranger@example.com").await;

        // Observe reads admit collaborators: the error list and one issue.
        let (status, _, _) = send(&app, authed("/api/console/apps/groceries/errors", &bruno)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(
            &app,
            authed("/api/console/apps/groceries/errors/fp1", &bruno),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Triage admits collaborators, like the MCP tools: resolve one
        // issue, then ignore the selection through bulk.
        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/errors/fp1/resolve",
                &bruno,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let triaged: serde_json::Value = body.parse().unwrap();
        assert_eq!(triaged["status"], "resolved");
        let (status, _, body) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/errors/bulk",
                &bruno,
                serde_json::json!({"fingerprints": ["fp1"], "action": "ignore"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let bulk: serde_json::Value = body.parse().unwrap();
        assert_eq!(bulk["updated"], 1);

        // A stranger's slug reads 404 exactly like a missing one, for
        // reads and triage alike.
        let (status, _, _) = send(
            &app,
            authed("/api/console/apps/groceries/errors/fp1", &stranger),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/console/apps/groceries/errors/fp1/resolve",
                &stranger,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
