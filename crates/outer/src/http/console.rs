//! Console: the read-mostly web UI under `/console` (spec §13.1).
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
use serde_json::json;

use crate::auth::{AccountRow, AuthState};
use crate::http::client_ip::client_ip;
use crate::mcp::tools::{latest_deploy, plan_of};
use crate::mcp::McpState;

pub fn router(state: super::AppState) -> Router<super::AppState> {
    Router::new()
        .route("/console/sign-in", get(sign_in_get).post(sign_in_post))
        .route("/api/console/me", get(api_me))
        .route("/api/console/apps", get(api_apps))
        .route("/api/console/apps/{slug}", get(api_app))
        .route("/api/console/apps/{slug}/actions", get(api_actions))
        .route("/api/console/apps/{slug}/metrics", get(api_metrics))
        .route("/api/console/apps/{slug}/logs", get(api_logs))
        .route("/api/console/apps/{slug}/errors", get(api_errors))
        .route("/api/console/sign-out", post(api_sign_out))
        .with_state(state)
}

fn nostore_json(status: StatusCode, error: &str, message: &str) -> Response {
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

fn not_found() -> Response {
    nostore_json(StatusCode::NOT_FOUND, "not_found", "no such app")
}

/// `true` only when the request is a same-origin browser fetch. Browsers set
/// this header themselves; a cross-site page cannot spoof it, which is what
/// keeps the cookie-only API safe from CSRF.
fn fetch_ok(headers: &HeaderMap) -> bool {
    headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("same-origin")
}

async fn guard(auth: &AuthState, headers: &HeaderMap) -> Result<AccountRow, Box<Response>> {
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

/// The app `slug` names, owned by `account`. Anything else — missing, or
/// owned by someone else — is `None`, and the caller answers 404 (spec §8.2).
async fn owned_app(
    mcp: &McpState,
    account: &AccountRow,
    slug: &str,
) -> anyhow::Result<Option<crate::apps::App>> {
    let app = crate::apps::get_by_slug(&mcp.pool, slug).await?;
    Ok(app.filter(|app| app.owner_id == account.id))
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
    (
        StatusCode::OK,
        [
            ("cache-control", "no-store"),
            ("content-type", "application/json"),
        ],
        json!({"email": account.email, "plan": plan}).to_string(),
    )
        .into_response()
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
    for app in apps.iter().filter(|a| a.owner_id == account.id) {
        let last = latest_deploy(&mcp.pool, &app.id).await.unwrap_or(None);
        let status =
            crate::mcp::tools::apps::app_status(app, last.as_ref().map(|r| r.status.as_str()));
        let mut v = json!({
            "slug": app.slug,
            "url": mcp.app_url(&app.slug),
            "role": "owner",
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
    let app = match owned_app(mcp, &account, &slug).await {
        Ok(Some(app)) => app,
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
            "role": "owner",
            "visibility": app.visibility.as_str(),
            "status": status,
            "last_request_at": app.last_request_at,
            "deploys": deploys,
        })
        .to_string(),
    )
        .into_response()
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
    let app = match owned_app(mcp, &account, &slug).await {
        Ok(Some(app)) => app,
        Ok(None) => return not_found(),
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the app right now",
            );
        }
    };
    let since = query.get("since").and_then(|s| s.parse::<u64>().ok());
    let limit = query
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let lines = crate::mcp::tools::runtime::log_lines(
        &mcp.pool,
        &mcp.fleets,
        &app.id,
        &app.slug,
        since,
        limit,
    )
    .await;
    let next_since = lines
        .last()
        .and_then(|l| l.get("seq").and_then(|s| s.as_u64()));
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
    let app = match owned_app(mcp, &account, &slug).await {
        Ok(Some(app)) => app,
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
    ok_json(json!({
        "issues": found.iter().map(|issue| json!({
            "fingerprint": issue.fingerprint,
            "message": issue.message,
            "frame": issue.frame,
            "count": issue.count,
            "first_seen": issue.first_seen,
            "last_seen": issue.last_seen,
            "status": issue.status,
        })).collect::<Vec<_>>(),
        "s5xx_last_hour": s5xx,
    }))
}
/// `GET /api/console/apps/:slug/actions`: the app's agent action log,
/// newest first. `before` is the opaque `<created_at>:<id>` cursor of the
/// last row on the previous page; `limit` defaults to 20 and clamps to
/// 1–100. Answers `{actions, next?}` where `next` is the cursor for the
/// following page. The caller MUST own the app (see `owned_app`): anything
/// else answers 404 exactly like a missing app.
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
    let app = match owned_app(mcp, &account, &slug).await {
        Ok(Some(app)) => app,
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
    let app = match owned_app(mcp, &account, &slug).await {
        Ok(Some(app)) => app,
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
fn ok_json(body: serde_json::Value) -> Response {
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
    sqlx::query("DELETE FROM web_session WHERE id_hash = ?")
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

fn email_form(error: Option<&str>) -> Markup {
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
    page(StatusCode::OK, email_form(None), None)
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
            email_form(Some("Open the sign-in page on outer.now and try again.")),
            None,
        );
    }
    let mcp = &state.mcp;
    let ip = request_ip(&headers, mcp.auth.config.origin_locked, Some(peer));
    match form.get("step").map(String::as_str) {
        Some("email") => {
            let email = form.get("email").cloned().unwrap_or_default();
            match crate::auth::send_code(&mcp.pool, &mcp.auth.config, &email, ip.as_deref()).await {
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
                        email_form(Some(crate::auth::pages::friendly_error(err.code))),
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
                    email_form(Some("Sign in with email first.")),
                    None,
                );
            };
            let Ok(Some(account)) = crate::auth::account_from_session(&mcp.pool, &session).await
            else {
                return page(
                    StatusCode::BAD_REQUEST,
                    email_form(Some("Sign in with email first.")),
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
            email_form(Some("Unknown sign-in step.")),
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
        crate::auth::send_code(&pool, &config, "ana@example.com", None)
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
            "INSERT INTO account (id, email, last_active_at, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(format!("acct-{owner}"))
        .bind(owner)
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
            serde_json::json!({"email": "ana@example.com", "plan": "free"})
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
            "INSERT INTO agent_action (id, app_id, account_id, grant_id, api_key_id, \
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
        assert_eq!(first["by"], "API key");
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
        let found = body["issues"].as_array().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["fingerprint"], "fp1");
        assert_eq!(found[0]["message"], "Error: kaboom");
        assert_eq!(found[0]["frame"], "at handler (x:1:2)");
        assert_eq!(found[0]["count"], 3);
        assert_eq!(found[0]["status"], "open");
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
}
