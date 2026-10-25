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
    fetch_ok, guard, nostore_json, nostore_json_refusal, nostore_json_user, not_found, ok_json,
    visible_app,
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
/// a collaborator reads 403 (spec §8.2 table: owner-only).
async fn owner_app(
    mcp: &McpState,
    account: &AccountRow,
    slug: &str,
) -> Result<crate::apps::App, Box<Response>> {
    match visible_app(mcp, account, slug).await {
        Ok(Some((app, Role::Owner))) => Ok(app),
        Ok(Some(_)) => Err(nostore_json_user(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only the app owner manages collaborators",
            "Only the app owner can invite or remove collaborators.",
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

/// `POST /api/console/apps/:slug/collaborators {email}`.
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
    let app = match owner_app(mcp, &account, &slug).await {
        Ok(app) => app,
        Err(res) => return *res,
    };
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
                &format!("add_collaborator {}", collaborator.email),
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
        Err(AddError::NotPro) => nostore_json_refusal(
            StatusCode::PAYMENT_REQUIRED,
            "payment_required",
            format!("collaborators on {} need a Pro owner", app.slug),
            "Collaborators are a Pro feature. Upgrade to Pro to invite people to your apps.",
            mcp.billing
                .upgrade_url(&app.owner_id, chrono::Utc::now().timestamp()),
        ),
        Err(AddError::LimitReached) => nostore_json_refusal(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            format!(
                "app quota: {} collaborators allowed, {} on {}",
                collaborators::MAX_COLLABORATORS,
                collaborators::MAX_COLLABORATORS,
                app.slug
            ),
            format!(
                "{} already has the maximum {} collaborators.",
                app.slug,
                collaborators::MAX_COLLABORATORS
            ),
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
    let app = match owner_app(mcp, &account, &slug).await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    let email = crate::auth::normalize_email(&body.email);
    match collaborators::remove(&mcp.pool, &app.id, &email).await {
        Ok(true) => {
            crate::mcp::actions::record_console(
                &mcp.pool,
                &app.id,
                &account.id,
                "remove_collaborator",
                &format!("remove_collaborator {email}"),
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
