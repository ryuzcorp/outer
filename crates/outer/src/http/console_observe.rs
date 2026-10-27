//! Console observe endpoints, wave 2 (spec §13.1): error triage and the
//! live log tail. New endpoints live here (not in `console.rs`), which
//! only gains the one `.merge(routes())` line; the session guard, the
//! same-origin check and the role-scoped 404 come from `console.rs` as
//! `pub(crate)` helpers.
//!
//! Follow is server-sent events, not long-poll: one connection streams a
//! `{lines}` page every poll tick, the browser's `EventSource` reconnects
//! with backoff after drops, and closing the tab (or hiding it — the
//! view closes the source on `visibilitychange`) ends the stream
//! server-side when the receiver drops. No CSP change: the console
//! policy's `connect-src 'self'` already allows same-origin event
//! streams, and the stream carries `no-store` like every console answer.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::console::{authorized_app, guard, logs_query, nostore_json, not_found, ok_json};

/// Seconds between follow polls; each tick emits at most one page.
const FOLLOW_POLL_SECS: u64 = 2;
/// Most lines one follow tick emits: a flood prints in pages, not one
/// giant event, and the cursor still advances past all of them.
const FOLLOW_PAGE: usize = 200;

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/api/console/apps/{slug}/logs/stream", get(api_logs_stream))
        .route(
            "/api/console/apps/{slug}/errors/{fingerprint}",
            get(api_error_detail),
        )
        .route(
            "/api/console/apps/{slug}/errors/{fingerprint}/resolve",
            post(api_error_resolve),
        )
        .route(
            "/api/console/apps/{slug}/errors/{fingerprint}/ignore",
            post(api_error_ignore),
        )
        .route(
            "/api/console/apps/{slug}/errors/{fingerprint}/reopen",
            post(api_error_reopen),
        )
        .route(
            "/api/console/apps/{slug}/errors/bulk",
            post(api_errors_bulk),
        )
}

/// `GET .../logs/stream`: the live tail as server-sent events. Same query
/// as `GET .../logs` (`q`, `stream`, `level`, `from`, `to`, `limit`
/// ignored here); `since` seeds the cursor, unset starts after the
/// current tail. Every poll tick emits one `data: {"lines": [...]}` page
/// (possibly empty), so the client just appends. The stream ends when the
/// caller stops being entitled to it: every tick re-verifies the session
/// (live, unbanned), and admin streams additionally re-check the step-up
/// window, the admin flag, and that the app still exists — reopening
/// forces a fresh step-up check.
async fn api_logs_stream(
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
    let authz = match authorized_app(mcp, &account, &headers, &slug, Some("logs")).await {
        Ok(authz) => authz,
        Err(res) => return *res,
    };
    // The tick re-verifies against this session, so hold it: `guard` just
    // proved it is present.
    let Some(session) = crate::auth::session_from_headers(&headers) else {
        return nostore_json(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "sign in at /console/sign-in",
        );
    };
    let app = authz.app;
    let (filter, cursor, _) = match logs_query(&query) {
        Ok(parsed) => parsed,
        Err(res) => return *res,
    };
    let seed = match cursor {
        crate::observe::logs::LogCursor::Since(seq) => seq,
        // `before` and the bare tail both mean "only what arrives next":
        // seed the cursor past the current newest line.
        _ => {
            let tail = crate::observe::logs::read_filtered(
                &mcp.pool,
                &mcp.fleets,
                &app.id,
                &app.slug,
                &filter,
                crate::observe::logs::LogCursor::Tail,
                1,
            )
            .await;
            tail.last().map(|l| l.seq).unwrap_or(0)
        }
    };
    let stream = futures::stream::unfold(
        (
            mcp.pool.clone(),
            mcp.fleets.clone(),
            mcp.auth.clone(),
            session,
            authz.admin,
            app.id.clone(),
            app.slug.clone(),
            filter,
            seed,
            true,
        ),
        move |(pool, fleets, auth, session, admin, app_id, slug, filter, mut cursor, mut first)| async move {
            // Re-verified every tick: a revoked or expired session, or a
            // ban, ends the stream for everyone (reopening signs in
            // again). Admin streams additionally re-check the step-up
            // window, the admin flag, and that the app still exists: the
            // step-up was only checked at open, and must not leak future
            // lines past its lapse.
            loop {
                tokio::time::sleep(Duration::from_secs(FOLLOW_POLL_SECS)).await;
                let live = crate::auth::account_from_session(&pool, &session)
                    .await
                    .ok()
                    .flatten();
                let Some(live) = live else {
                    return None;
                };
                if admin {
                    if !crate::auth::is_admin(&auth.config, &live.email) {
                        return None;
                    }
                    let fresh = crate::auth::session_created_at(&pool, &session)
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|created| {
                            crate::auth::now_ms() - created <= crate::auth::ADMIN_STEP_UP_MS
                        });
                    if !fresh {
                        return None;
                    }
                    let still_there = crate::apps::get_by_slug(&pool, &slug).await.ok().flatten();
                    if still_there.is_none() {
                        return None;
                    }
                } else {
                    let visible = crate::apps::get_by_slug(&pool, &slug).await.ok()?;
                    let app = visible.as_ref()?;
                    crate::collaborators::role(
                        &pool,
                        &app.id,
                        &app.owner_id,
                        &live.id,
                        &live.email,
                    )
                    .await
                    .ok()??;
                }
                let lines = crate::observe::logs::read_filtered(
                    &pool,
                    &fleets,
                    &app_id,
                    &slug,
                    &filter,
                    crate::observe::logs::LogCursor::Since(cursor),
                    FOLLOW_PAGE,
                )
                .await;
                if let Some(last) = lines.last() {
                    cursor = last.seq;
                }
                if !lines.is_empty() || first {
                    first = false;
                    let data = json!({
                        "lines": lines.iter().map(|l| l.to_json()).collect::<Vec<_>>(),
                    })
                    .to_string();
                    let state = (
                        pool, fleets, auth, session, admin, app_id, slug, filter, cursor, first,
                    );
                    return Some((Ok::<Event, Infallible>(Event::default().data(data)), state));
                }
            }
        },
    );
    let mut res = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    res.headers_mut().insert(
        "cache-control",
        "no-store".parse().expect("static header parses"),
    );
    res
}

/// `GET .../errors/{fingerprint}`: one issue with its sparkline and the
/// most recent matching log lines. Unknown fingerprints 404, exactly like
/// a missing app.
async fn api_error_detail(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path((slug, fingerprint)): Path<(String, String)>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let authz = match authorized_app(mcp, &account, &headers, &slug, Some("errors")).await {
        Ok(authz) => authz,
        Err(res) => return *res,
    };
    let app = authz.app;
    if fingerprint.is_empty() || fingerprint.len() > 64 {
        return not_found();
    }
    let detail = match crate::observe::logs::issue_detail(
        &mcp.pool,
        &app.id,
        &fingerprint,
        crate::apps::now_ms(),
    )
    .await
    {
        Ok(Some(detail)) => detail,
        Ok(None) => return not_found(),
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the error right now",
            );
        }
    };
    ok_json(json!({
        "issue": {
            "fingerprint": detail.issue.fingerprint,
            "message": detail.issue.message,
            "frame": detail.issue.frame,
            "count": detail.issue.count,
            "first_seen": detail.issue.first_seen,
            "last_seen": detail.issue.last_seen,
            "status": detail.issue.status,
            "resolved_at": detail.issue.resolved_at,
            "regressed": crate::observe::logs::is_regressed(&detail.issue),
        },
        "sparkline": detail.sparkline.iter().map(|b| json!({"start": b.start, "count": b.count})).collect::<Vec<_>>(),
        "recent": detail.recent.iter().map(|l| l.to_json()).collect::<Vec<_>>(),
    }))
}

/// Move one issue (or a bulk list) to a new status through the same
/// domain function the MCP triage tools call, and record the console
/// mutation as an `agent_action` row under the MCP tool's name.
/// Collaborators may triage, like the MCP tools (spec §7.4): the endpoints
/// below gate on [`authorized_app`], not the owner.
async fn triage(
    pool: &sqlx::SqlitePool,
    account: &crate::auth::AccountRow,
    admin: bool,
    app_id: &str,
    fingerprints: &[&str],
    status: &str,
) -> Result<u64, Box<Response>> {
    match crate::observe::logs::set_status(
        pool,
        app_id,
        fingerprints,
        status,
        crate::apps::now_ms(),
    )
    .await
    {
        Ok(updated) => {
            // Only an actual mutation audits: an unknown fingerprint moves
            // nothing, and the 404 already says so.
            if updated > 0 {
                let tool = match status {
                    "resolved" => "resolve_error",
                    "ignored" => "ignore_error",
                    _ => "reopen_error",
                };
                let summary = if fingerprints.len() == 1 {
                    format!(
                        "{tool} {}",
                        &fingerprints[0][..fingerprints[0].len().min(12)]
                    )
                } else {
                    format!("{tool} {} issues", fingerprints.len())
                };
                let summary = super::console::audit_summary(account, &summary, admin);
                if let Err(res) = super::console::audit_mutation(
                    pool, app_id, account, tool, &summary, "ok", None, 0, admin,
                )
                .await
                {
                    return Err(res);
                }
            }
            Ok(updated)
        }
        Err(_) => Err(Box::new(nostore_json(
            StatusCode::BAD_REQUEST,
            "bad_triage",
            "triage takes 1–100 fingerprints",
        ))),
    }
}

async fn api_error_resolve(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path((slug, fingerprint)): Path<(String, String)>,
) -> Response {
    api_error_triage_one(state, headers, slug, fingerprint, "resolved").await
}

async fn api_error_ignore(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path((slug, fingerprint)): Path<(String, String)>,
) -> Response {
    api_error_triage_one(state, headers, slug, fingerprint, "ignored").await
}

async fn api_error_reopen(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path((slug, fingerprint)): Path<(String, String)>,
) -> Response {
    api_error_triage_one(state, headers, slug, fingerprint, "open").await
}

async fn api_error_triage_one(
    state: super::AppState,
    headers: HeaderMap,
    slug: String,
    fingerprint: String,
    status: &'static str,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let authz = match authorized_app(mcp, &account, &headers, &slug, None).await {
        Ok(authz) => authz,
        Err(res) => return *res,
    };
    let app = authz.app;
    if fingerprint.is_empty() || fingerprint.len() > 64 {
        return not_found();
    }
    match triage(
        &mcp.pool,
        &account,
        authz.admin,
        &app.id,
        &[&fingerprint],
        status,
    )
    .await
    {
        Ok(0) => not_found(),
        Ok(updated) => {
            ok_json(json!({"fingerprint": fingerprint, "status": status, "updated": updated}))
        }
        Err(res) => *res,
    }
}

#[derive(Debug, Deserialize)]
struct BulkTriage {
    fingerprints: Vec<String>,
    action: String,
}

/// `POST .../errors/bulk {fingerprints, action}`: resolve, ignore or
/// reopen a checkbox selection (at most 100). Unknown fingerprints match
/// nothing; the answer reports how many moved.
async fn api_errors_bulk(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<BulkTriage>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let authz = match authorized_app(mcp, &account, &headers, &slug, None).await {
        Ok(authz) => authz,
        Err(res) => return *res,
    };
    let app = authz.app;
    let status = match body.action.as_str() {
        "resolve" => "resolved",
        "ignore" => "ignored",
        "reopen" => "open",
        _ => {
            return nostore_json(
                StatusCode::BAD_REQUEST,
                "bad_action",
                "action is resolve, ignore or reopen",
            );
        }
    };
    if body.fingerprints.is_empty()
        || body.fingerprints.len() > crate::observe::logs::MAX_BULK_TRIAGE
    {
        return nostore_json(
            StatusCode::BAD_REQUEST,
            "bad_triage",
            "triage takes 1–100 fingerprints",
        );
    }
    if body
        .fingerprints
        .iter()
        .any(|fp| fp.is_empty() || fp.len() > 64)
    {
        return nostore_json(
            StatusCode::BAD_REQUEST,
            "bad_triage",
            "fingerprints are at most 64 characters",
        );
    }
    let refs: Vec<&str> = body.fingerprints.iter().map(String::as_str).collect();
    match triage(&mcp.pool, &account, authz.admin, &app.id, &refs, status).await {
        Ok(updated) => {
            ok_json(json!({"action": body.action, "status": status, "updated": updated}))
        }
        Err(res) => *res,
    }
}
