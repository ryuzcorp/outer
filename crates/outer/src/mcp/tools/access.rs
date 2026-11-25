//! The visitor-gate tool: `set_access`, plus the shared `access` summary
//! key every app view carries (never the hash).

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{internal, ok, owner_app, with_plans, Ctx};
use crate::access::AccessMode;
use crate::mcp::error::ToolError;

#[derive(Deserialize, JsonSchema)]
pub struct SetAccessInput {
    /// The app slug.
    pub app: String,
    /// open (default, everyone walks in), pin (4-8 digits) or password
    /// (8-128 chars, no control characters).
    pub mode: String,
    /// Required for pin/password, refused for open. Never echoed back.
    pub secret: Option<String>,
}

/// Never formats the gate secret: tool inputs can land in logs and reports.
impl std::fmt::Debug for SetAccessInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetAccessInput")
            .field("app", &self.app)
            .field("mode", &self.mode)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
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
    .map_err(|err| access_tool_error(&app.slug, err, &ctx.state.plans_url()))?;
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
/// `plans_url` is the informational plans page the refusal links to.
pub fn access_tool_error(
    slug: &str,
    err: crate::access::AccessError,
    plans_url: &str,
) -> ToolError {
    match err {
        crate::access::AccessError::ProOnly => with_plans(
            ToolError::new(
                "payment_required",
                format!("{slug} is on a Free account; a visitor gate is a Pro feature"),
            )
            .with_user(format!(
                "Visitor gates are a Pro feature, not included on the Free plan — plans: {plans_url}. Switching back to open is always free.",
            )),
            plans_url,
        ),
        crate::access::AccessError::BadSecret(message) => ToolError::invalid_argument(message),
        crate::access::AccessError::Internal(err) => internal(err),
    }
}
#[cfg(test)]
mod tests {
    use super::SetAccessInput;

    #[test]
    fn hostile_access_input_debug_redacts_the_secret() {
        let input = SetAccessInput {
            app: "grocery".to_string(),
            mode: "password".to_string(),
            secret: Some("correct-horse-9".to_string()),
        };
        let shown = format!("{input:?}");
        assert!(shown.contains("grocery"), "keeps context: {shown}");
        assert!(shown.contains("password"), "keeps the mode: {shown}");
        assert!(!shown.contains("correct-horse-9"), "redacts: {shown}");
    }

    #[test]
    fn hostile_access_errors_name_the_rule_never_the_secret() {
        // Every shape refusal the gate can return, fed a canary secret:
        // the message must describe the rule, not echo the value.
        for (mode, secret) in [
            ("open", Some("correct-horse-9")),
            ("pin", Some("correct-horse-9")),
            ("pin", None),
            ("password", Some("short")),
            ("password", None),
        ] {
            let mode = match mode {
                "open" => crate::access::AccessMode::Open,
                "pin" => crate::access::AccessMode::Pin,
                _ => crate::access::AccessMode::Password,
            };
            let err = crate::access::check_secret(mode, secret).unwrap_err();
            let shown = format!("{err}");
            assert!(
                !shown.contains("correct-horse-9") && !shown.contains("short"),
                "mode {mode:?} echoes the secret: {shown}"
            );
        }
    }
}
