//! Console collaborators endpoints (Pro, spec §7.4).
//!
//! - `GET /api/console/apps/{slug}/collaborators`: the app's invites with
//!   joined/pending status. Owners and collaborators read; anyone else gets
//!   the owner-scoped 404 (spec §8.2).
//! - `POST …/collaborators {email}`: invite one address (owner only). Calls
//!   the same domain function as MCP `add_collaborator`, audits as
//!   `add_collaborator` with client "Console", and answers the same §8.3
//!   codes the tool does.
//! - `POST …/collaborators/remove {email}`: remove one address (owner only),
//!   audited as `remove_collaborator`.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::console::{
    fetch_ok, guard, nostore_json, nostore_json_refusal, nostore_json_user, ok_json,
};
use crate::auth::AccountRow;
use crate::collaborators::{self, AddError, Role};
use crate::http::AppState;
use crate::mcp::McpState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/console/apps/{slug}/collaborators",
            get(api_collaborators).post(api_invite),
        )
        .route(
            "/api/console/apps/{slug}/collaborators/remove",
            post(api_remove),
        )
}

#[derive(Debug, Deserialize)]
struct EmailBody {
    email: String,
}

fn collaborator_json(collaborator: &collaborators::Collaborator) -> serde_json::Value {
    json!({
        "email": collaborator.email,
        "status": if collaborator.joined { "joined" } else { "pending" },
        "added_at": collaborator.added_at,
    })
}

/// `GET /api/console/apps/:slug/collaborators`.
async fn api_collaborators(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    if !fetch_ok(&headers) {
        return nostore_json(
            StatusCode::FORBIDDEN,
            "forbidden",
            "same-origin requests only",
        );
    }
    let authz =
        match super::console::authorized_app(mcp, &account, &headers, &slug, Some("collaborators"))
            .await
        {
            Ok(authz) => authz,
            Err(res) => return *res,
        };
    let app = authz.app;
    let role = authz.role;
    let rows = match collaborators::list(&mcp.pool, &app.id).await {
        Ok(rows) => rows,
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load collaborators right now",
            );
        }
    };
    let owner_plan = crate::mcp::tools::plan_of(&mcp.pool, &app.owner_id)
        .await
        .unwrap_or_else(|_| "free".to_string());
    ok_json(json!({
        "app": app.slug,
        "role": match role {
            Role::Owner => "owner",
            Role::Collaborator => "collaborator",
        },
        "owner_plan": owner_plan,
        "collaborators": rows.iter().map(collaborator_json).collect::<Vec<_>>(),
    }))
}

/// The owner check behind the two mutations: no role reads 404 (spec §8.2),
/// a collaborator reads 403 (spec §8.2 table: owner-only). The admin
/// override reports the owner role, but managing access itself stays with
/// the true owner: an admin invite or removal would mint a step-up-free,
/// unaudited foothold (or lock the owner out), so the admin path is refused
/// below and the refusal is audited.
async fn owner_app(
    mcp: &McpState,
    account: &AccountRow,
    headers: &HeaderMap,
    slug: &str,
) -> Result<(crate::apps::App, bool), Box<Response>> {
    // Mutations audit themselves, so no read label.
    match super::console::authorized_app(mcp, account, headers, slug, None).await {
        Ok(authz) if authz.role == Role::Owner => Ok((authz.app, authz.admin)),
        Ok(_) => Err(nostore_json_user(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only the app owner manages collaborators",
            "Only the app owner can invite or remove collaborators.",
        )
        .into()),
        Err(res) => Err(res),
    }
}

/// Refuse a collaborator add/remove that arrived through the admin override,
/// and audit the refusal. Access management stays with the true owner: an
/// admin could otherwise grant themselves (or a sockpuppet) a permanent,
/// step-up-free collaborator role. The 403 itself is the closed outcome, so
/// the audit is best-effort — but a lost row logs loudly.
async fn refuse_admin_access_change(
    mcp: &McpState,
    account: &AccountRow,
    app: &crate::apps::App,
    tool: &str,
    email: &str,
) -> Response {
    if let Err(err) = crate::mcp::actions::try_record_console(
        &mcp.pool,
        &app.id,
        &account.id,
        tool,
        &super::console::audit_summary(
            account,
            &format!("{tool} {email} refused: admin cannot manage collaborators"),
            true,
        ),
        "error",
        Some("forbidden"),
        0,
    )
    .await
    {
        tracing::error!(user = %account.id, tool, error = %format!("{err:#}"), "admin collaborator-refusal audit failed");
    }
    nostore_json_user(
        StatusCode::FORBIDDEN,
        "forbidden",
        "admins cannot manage collaborators",
        "Only the app owner can invite or remove collaborators. Admin access is read-and-mutate, never grant.",
    )
}

async fn api_invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<EmailBody>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    if !fetch_ok(&headers) {
        return nostore_json(
            StatusCode::FORBIDDEN,
            "forbidden",
            "same-origin requests only",
        );
    }
    if body.email.len() > 320 {
        return nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "that address is not usable",
        );
    }
    let (app, admin) = match owner_app(mcp, &account, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    if admin {
        return refuse_admin_access_change(mcp, &account, &app, "add_collaborator", &body.email)
            .await;
    }
    match collaborators::add(
        &mcp.pool,
        &mcp.auth.config.email,
        &mcp.mcp_url(),
        &app,
        &account.email,
        &body.email,
    )
    .await
    {
        Ok(collaborator) => {
            crate::mcp::actions::record_console(
                &mcp.pool,
                &app.id,
                &account.id,
                "add_collaborator",
                &super::console::audit_summary(
                    &account,
                    &format!("add_collaborator {}", collaborator.email),
                    admin,
                ),
                "ok",
                None,
                0,
            )
            .await;
            ok_json(json!({
                "app": app.slug,
                "collaborator": collaborator_json(&collaborator),
            }))
        }
        Err(AddError::LimitReached { limit }) => nostore_json_refusal(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            format!("app quota: {limit} collaborator(s) allowed on {}", app.slug),
            if limit == crate::limits::FREE_COLLABORATORS {
                format!(
                    "{} already has its Free collaborator. Upgrade to Pro for up to {} per app.",
                    app.slug,
                    crate::limits::PRO_COLLABORATORS
                )
            } else {
                format!(
                    "{} already has the maximum {} collaborators.",
                    app.slug, limit
                )
            },
            mcp.billing
                .upgrade_url(&app.owner_id, chrono::Utc::now().timestamp()),
        ),
        Err(AddError::InvalidEmail(_)) => nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "that address is not usable",
        ),
        Err(AddError::SelfInvite) => nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "you already own this app",
        ),
        Err(AddError::Duplicate) => nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "that address is already a collaborator",
        ),
        Err(AddError::Internal(_)) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "could not invite that address right now",
        ),
    }
}

/// `POST /api/console/apps/:slug/collaborators/remove {email}`.
async fn api_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<EmailBody>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    if !fetch_ok(&headers) {
        return nostore_json(
            StatusCode::FORBIDDEN,
            "forbidden",
            "same-origin requests only",
        );
    }
    let (app, admin) = match owner_app(mcp, &account, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    if admin {
        return refuse_admin_access_change(mcp, &account, &app, "remove_collaborator", &body.email)
            .await;
    }
    let email = crate::auth::normalize_email(&body.email);
    match collaborators::remove(&mcp.pool, &app.id, &email).await {
        Ok(true) => {
            crate::mcp::actions::record_console(
                &mcp.pool,
                &app.id,
                &account.id,
                "remove_collaborator",
                &super::console::audit_summary(
                    &account,
                    &format!("remove_collaborator {email}"),
                    admin,
                ),
                "ok",
                None,
                0,
            )
            .await;
            ok_json(json!({"app": app.slug, "email": email, "removed": true}))
        }
        Ok(false) => nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "that address is not a collaborator",
        ),
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "could not remove that address right now",
        ),
    }
}
