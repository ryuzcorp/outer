//! The visitor-gate tool: `set_access`, plus the shared `access` summary
//! key every app view carries (never the hash).

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{internal, ok, owner_app, with_upgrade, Ctx};
use crate::access::AccessMode;
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetAccessInput {
    /// The app slug.
    pub app: String,
    /// open (default, everyone walks in), pin (4-8 digits) or password
    /// (8-128 chars, no control characters).
    pub mode: String,
    /// Required for pin/password, refused for open. Never echoed back.
    pub secret: Option<String>,
}

/// Set the app's visitor gate (owner only). `pin`/`password` need Pro;
/// `open` never does. Every change bumps the access version, so a new
/// secret signs every issued cookie out.
pub async fn set_access(input: SetAccessInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    let mode = match input.mode.as_str() {
        "open" => AccessMode::Open,
        "pin" => AccessMode::Pin,
        "password" => AccessMode::Password,
        other => {
            return Err(ToolError::invalid_argument(format!(
                "mode must be open, pin or password, got {other:?}"
            )));
        }
    };
    // The Pro gate lives in the shared domain function, so the console
    // endpoint gating on the same function refuses identically.
    let plan = super::plan_of(&ctx.state.pool, &app.owner_id).await?;
    let (mode, changed) = crate::access::set_access(
        &ctx.state.pool,
        &app.id,
        mode,
        input.secret.as_deref(),
        &plan,
    )
    .await
    .map_err(|err| access_tool_error(&app.slug, err, &ctx.state.billing, &app.owner_id))?;
    // A real change evicts anything cached before the lock (or unlock):
    // the live host, the preview host and every active custom domain.
    if changed {
        crate::access::spawn_gate_purge(
            &ctx.state.purges,
            ctx.state.delete_cfg.cache_purge.clone(),
            ctx.state.pool.clone(),
            ctx.state.delete_cfg.apps_domain.clone(),
            app.id.clone(),
            app.slug.clone(),
        );
    }
    let summary = match mode {
        AccessMode::Pin => format!(
            "{} now asks visitors for a PIN. Share it with your family; changing it signs everyone out.",
            app.slug,
        ),
        AccessMode::Password => format!(
            "{} now asks visitors for a password. Share it with your family; changing it signs everyone out.",
            app.slug,
        ),
        AccessMode::Open => format!(
            "{} is open again — visitors go straight in.",
            app.slug,
        ),
    };
    Ok(ok(summary, json!({"mode": mode.as_str()})))
}

/// Map the shared gate refusal to the error both surfaces return: the MCP
/// `isError` and the console HTTP body carry the same code and user
/// message. Free owners get `payment_required` with a user message pointing
/// to Pro; switching back to `open` never reaches this gate.
/// `billing` + `owner_id` supply the `upgrade_url` the refusal carries.
pub fn access_tool_error(
    slug: &str,
    err: crate::access::AccessError,
    billing: &crate::billing::BillingState,
    owner_id: &str,
) -> ToolError {
    match err {
        crate::access::AccessError::ProOnly => with_upgrade(
            ToolError::new(
                "payment_required",
                format!("{slug} is on a Free account; a visitor gate is a Pro feature"),
            )
            .with_user(format!(
                "Visitor gates are Pro. Upgrade to Pro to gate {slug} with a PIN or password; switching back to open is always free.",
            )),
            billing,
            owner_id,
        ),
        crate::access::AccessError::BadSecret(message) => ToolError::invalid_argument(message),
        crate::access::AccessError::Internal(err) => internal(err),
    }
}
