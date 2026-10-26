//! Console custom domains (spec §7.7, §13.1): the Settings → Custom
//! domain card reads and mutates through here, calling the SAME domain
//! functions as the MCP `add_domain` / `remove_domain` / `list_domains`
//! tools (same Pro gate, same validation, same messages).
//!
//! - `GET /api/console/apps/{slug}/domains`: `{plan, role, domains: [...]}`.
//!   Live-refreshes from Cloudflare when reachable, else the stored row.
//! - `POST .../domains {hostname}`: add. Audited as `add_domain`.
//! - `POST .../domains/refresh`: re-read from Cloudflare (user-initiated
//!   sync, not audited: it changes no user state).
//! - `DELETE .../domains`: remove. Audited as `remove_domain`.
//!
//! Every endpoint needs the session cookie + `Sec-Fetch-Site: same-origin`
//! (the [`super::console::guard`] rules) and answers 404 for foreign slugs
//! exactly like a missing one (spec §8.2). Domain management is owner-only:
//! a collaborator reads 403 with the MCP `forbidden` code and user message
//! (spec §7.4), and the card itself only renders for owners. Errors carry
//! the `{error, message, user_message?}` MCP shape via
//! [`super::console_actions::tool_response`], so the UI shows `user_message`
//! in an `alert`. Mutations confirm in a daisyUI modal first.

use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::apps::domains;
use crate::mcp::tools::plan_of;

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route(
            "/api/console/apps/{slug}/domains",
            get(api_list).post(api_add),
        )
        .route(
            "/api/console/apps/{slug}/domains/refresh",
            post(api_refresh),
        )
        .route("/api/console/apps/{slug}/domains", delete(api_remove))
}

fn domain_json(view: &domains::DomainView) -> serde_json::Value {
    json!({
        "hostname": view.hostname,
        "status": view.status,
        "ssl_status": view.ssl_status,
        "reason": view.reason,
        "cname_target": view.cname_target,
        "txt_records": view.txt_records.iter().map(|r| json!({"name": r.name, "value": r.value})).collect::<Vec<_>>(),
    })
}

fn cf(state: &super::AppState) -> Option<crate::cloudflare::CustomHostnames> {
    crate::cloudflare::CustomHostnames::from_purge(state.mcp.delete_cfg.cache_purge.as_ref())
}

/// The session plus the visible app, plus the explicit owner check: custom
/// domains are owner-only (spec §7.4), so a collaborator reads 403 with the
/// same code and user message the MCP tool answers — never 404 (spec §8.2
/// reserves 404 for slugs the caller has no role on). The admin override
/// reports the owner role, so admins pass this check. `read_action` labels
/// the access audit for reads; mutations audit themselves.
async fn owner_app(
    mcp: &crate::mcp::McpState,
    headers: &HeaderMap,
    slug: &str,
    read_action: Option<&'static str>,
) -> Result<(crate::auth::AccountRow, crate::apps::App, bool), Box<Response>> {
    use crate::mcp::error::ToolError;
    let account = match super::console::guard(&mcp.auth, headers).await {
        Ok(account) => account,
        Err(res) => return Err(res),
    };
    match super::console::authorized_app(mcp, &account, headers, slug, read_action).await {
        Ok(authz) if authz.role == crate::collaborators::Role::Owner => {
            Ok((authz.account, authz.app, authz.admin))
        }
        Ok(_) => Err(super::console_actions::tool_response(
            &ToolError::forbidden(format!("only the app owner may manage {slug:?} this way"))
                .with_user("Only the app owner can do that. Collaborators can read and deploy, but not change these settings."),
        )
        .into()),
        Err(res) => Err(res),
    }
}

/// One `agent_action` row through the single console audit writer
/// ([`crate::mcp::actions::record_console`], client "Console"): successes
/// and refusals both land, like the MCP path. A refused Pro-gated add is
/// signal, not noise.
async fn audit(
    pool: &sqlx::SqlitePool,
    account: &crate::auth::AccountRow,
    app: &crate::apps::App,
    tool: &str,
    summary: &str,
    outcome: (&str, Option<&str>),
    started: Instant,
) {
    crate::mcp::actions::record_console(
        pool,
        &app.id,
        &account.id,
        tool,
        summary,
        outcome.0,
        outcome.1,
        started.elapsed().as_millis().min(u128::from(u64::MAX)) as i64,
    )
    .await;
}

async fn api_list(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let (account, app, admin) = match owner_app(&state.mcp, &headers, &slug, Some("domains")).await
    {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let plan = plan_of(
        &state.mcp.pool,
        super::console::billing_id(&account, &app, admin),
    )
    .await
    .unwrap_or_else(|_| "free".to_string());
    // Live state when Cloudflare answers; the stored row when it doesn't.
    // A missing domain is `{domains: []}`, never a 404: the card shows the
    // empty state with the add form.
    let view = match domains::refresh_domain(
        &state.mcp.pool,
        cf(&state).as_ref(),
        &state.mcp.domain,
        &app,
    )
    .await
    {
        Ok(view) => Some(view),
        Err(domains::DomainError::NotFound(_, _)) => None,
        Err(domains::DomainError::Unavailable(_)) => {
            domains::list_domains(&state.mcp.pool, &state.mcp.domain, &app)
                .await
                .map(|mut v| v.pop())
                .unwrap_or(None)
        }
        Err(err) => {
            return super::console_actions::tool_response(
                &crate::mcp::tools::domains::tool_error(err, &state.mcp.billing, &app.owner_id)
                    .await,
            );
        }
    };
    let out: Vec<serde_json::Value> = view.iter().map(domain_json).collect();
    super::console::ok_json(json!({"plan": plan, "role": "owner", "domains": out}))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    hostname: Option<String>,
}

/// Bounded: a hostname overlong is refused by validation anyway, but the
/// cap keeps one log row small whatever the caller sent.
const MAX_HOSTNAME_BYTES: usize = 512;

async fn api_add(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    use crate::mcp::error::ToolError;
    let started = Instant::now();
    let (account, app, admin) = match owner_app(&state.mcp, &headers, &slug, None).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let raw: AddBody = match serde_json::from_value(body) {
        Ok(raw) => raw,
        Err(_) => {
            return super::console_actions::tool_response(&ToolError::invalid_argument(
                "body must be {hostname}",
            ));
        }
    };
    let Some(hostname) = raw.hostname else {
        return super::console_actions::tool_response(&ToolError::invalid_argument(
            "body must be {hostname}",
        ));
    };
    if hostname.len() > MAX_HOSTNAME_BYTES {
        return super::console_actions::tool_response(&ToolError::invalid_argument(
            "hostname too long",
        ));
    }
    let plan = plan_of(
        &state.mcp.pool,
        super::console::billing_id(&account, &app, admin),
    )
    .await
    .unwrap_or_else(|_| "free".to_string());
    let outcome = domains::add_domain(
        &state.mcp.pool,
        cf(&state).as_ref(),
        &state.mcp.domain,
        &state.mcp.apps_domain,
        &plan,
        &app,
        &hostname,
    )
    .await;
    // Audited like the MCP tool (client "Console": both ids NULL).
    let (outcome_str, error_code) = match &outcome {
        Ok(_) => ("ok", None),
        Err(err) => ("error", Some(err.code())),
    };
    audit(
        &state.mcp.pool,
        &account,
        &app,
        "add_domain",
        &super::console::audit_summary(&account, &format!("add_domain {hostname}"), admin),
        (outcome_str, error_code),
        started,
    )
    .await;
    match outcome {
        Ok(view) => super::console::ok_json(json!({"domain": domain_json(&view)})),
        Err(err) => super::console_actions::tool_response(
            &crate::mcp::tools::domains::tool_error(err, &state.mcp.billing, &app.owner_id).await,
        ),
    }
}

async fn api_refresh(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let (_, app, _) = match owner_app(&state.mcp, &headers, &slug, None).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    match domains::refresh_domain(
        &state.mcp.pool,
        cf(&state).as_ref(),
        &state.mcp.domain,
        &app,
    )
    .await
    {
        Ok(view) => super::console::ok_json(json!({"domain": domain_json(&view)})),
        Err(domains::DomainError::NotFound(_, _)) => {
            super::console::ok_json(json!({"domain": serde_json::Value::Null}))
        }
        Err(err) => super::console_actions::tool_response(
            &crate::mcp::tools::domains::tool_error(err, &state.mcp.billing, &app.owner_id).await,
        ),
    }
}

async fn api_remove(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let started = Instant::now();
    let (account, app, admin) = match owner_app(&state.mcp, &headers, &slug, None).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let outcome = domains::remove_domain(
        &state.mcp.pool,
        cf(&state).as_ref(),
        state.mcp.delete_cfg.cache_purge.as_ref(),
        &app,
        None,
    )
    .await;
    let (outcome_str, error_code) = match &outcome {
        Ok(_) => ("ok", None),
        Err(err) => ("error", Some(err.code())),
    };
    audit(
        &state.mcp.pool,
        &account,
        &app,
        "remove_domain",
        &super::console::audit_summary(&account, "remove_domain", admin),
        (outcome_str, error_code),
        started,
    )
    .await;
    match outcome {
        Ok(hostname) => super::console::ok_json(json!({"removed": hostname})),
        Err(err) => super::console_actions::tool_response(
            &crate::mcp::tools::domains::tool_error(err, &state.mcp.billing, &app.owner_id).await,
        ),
    }
}
