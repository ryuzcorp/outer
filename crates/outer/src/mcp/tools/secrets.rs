//! The secret tools: `list_secrets` (owners and collaborators, spec §8.2
//! table), `set_secret` and `unset_secret` (owner only). Owner-only calls
//! from a collaborator answer `forbidden`; a slug with no role answers
//! `not_found`, exactly like a missing one.
//!
//! Values are never returned, logged or put in an action-log summary: only
//! names travel. What a set changes takes effect at the next deploy, when
//! the pipeline decrypts the values into the deploy-root config `vars`.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{internal, ok, owner_app, visible_app, Ctx};
use crate::mcp::error::ToolError;
use crate::secrets;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListSecretsInput {
    /// The app slug.
    pub app: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SetSecretInput {
    /// The app slug.
    pub app: String,
    /// Worker-env name: `^[A-Z_][A-Z0-9_]{0,63}$`, platform-reserved names refused.
    pub key: String,
    /// The secret value (at most 4 KiB). Encrypted at rest, never shown again.
    pub value: String,
}

/// Never formats the value: tool inputs can land in logs and error reports.
impl std::fmt::Debug for SetSecretInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetSecretInput")
            .field("app", &self.app)
            .field("key", &self.key)
            .field("value", &"<redacted>")
            .finish()
    }
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnsetSecretInput {
    /// The app slug.
    pub app: String,
    /// The secret name to remove.
    pub key: String,
}

pub async fn list_secrets(input: ListSecretsInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let metas = secrets::list(&ctx.state.pool, &app.id)
        .await
        .map_err(internal)?;
    let out: Vec<Value> = metas
        .iter()
        .map(|m| json!({"name": m.name, "updated_at": m.updated_at}))
        .collect();
    Ok(ok(
        format!("{} secret(s) on {}.", out.len(), app.slug),
        json!({"secrets": out}),
    ))
}

pub async fn set_secret(input: SetSecretInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    secrets::check_name(&input.key).map_err(ToolError::invalid_argument)?;
    secrets::check_reserved(&input.key).map_err(ToolError::invalid_argument)?;
    if input.value.len() > secrets::MAX_VALUE_BYTES {
        return Err(ToolError::invalid_argument(format!(
            "value is {} bytes; at most {} bytes",
            input.value.len(),
            secrets::MAX_VALUE_BYTES
        )));
    }
    let blob = secrets::encrypt(
        &ctx.state.secrets_key,
        &app.id,
        &input.key,
        input.value.as_bytes(),
    );
    let capped = secrets::set(
        &ctx.state.pool,
        &app.id,
        &input.key,
        &blob,
        crate::apps::now_ms(),
    )
    .await
    .map_err(internal)?;
    if capped {
        return Err(ToolError::invalid_argument(format!(
            "{} already has the maximum {} secrets; unset one first",
            app.slug,
            secrets::MAX_SECRETS_PER_APP
        )));
    }
    Ok(ok(
        format!(
            "Secret {:?} set on {}. Applies on next deploy.",
            input.key, app.slug
        ),
        json!({"ok": true, "applies": "next deploy"}),
    ))
}

pub async fn unset_secret(input: UnsetSecretInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    secrets::check_name(&input.key).map_err(ToolError::invalid_argument)?;
    let removed = secrets::unset(&ctx.state.pool, &app.id, &input.key)
        .await
        .map_err(internal)?;
    if !removed {
        return Err(ToolError::not_found(format!(
            "no secret {:?} on {}",
            input.key, app.slug
        )));
    }
    Ok(ok(
        format!(
            "Secret {:?} removed from {}. Applies on next deploy.",
            input.key, app.slug
        ),
        json!({"ok": true, "applies": "next deploy"}),
    ))
}

#[cfg(test)]
mod tests {
    use super::SetSecretInput;

    #[test]
    fn set_input_debug_redacts_the_value() {
        let input = SetSecretInput {
            app: "grocery".to_string(),
            key: "API_KEY".to_string(),
            value: "super-secret-value".to_string(),
        };
        let shown = format!("{input:?}");
        assert!(shown.contains("grocery"), "keeps context: {shown}");
        assert!(shown.contains("API_KEY"), "keeps the name: {shown}");
        assert!(!shown.contains("super-secret-value"), "redacts: {shown}");
    }
}
