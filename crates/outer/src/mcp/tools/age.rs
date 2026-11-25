//! The adults-only tool: `set_adults_only`, plus the shared
//! `adults_only` summary key every app view carries.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{internal, ok, owner_app, Ctx};
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetAdultsOnlyInput {
    /// The app slug.
    pub app: String,
    /// true marks the app 18+: visitors confirm they are an adult before
    /// the app serves them. false removes the confirmation.
    pub adults_only: bool,
}

/// Mark the app adults-only (18+, owner only). Visitors then meet the age
/// interstitial — after the visitor gate, when the app has one — until they
/// confirm on that host; the confirmation lasts 30 days. Available on every
/// plan: confirming an age is not a Pro feature.
pub async fn set_adults_only(input: SetAdultsOnlyInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    let changed = crate::age::set_adults_only(&ctx.state.pool, &app.id, input.adults_only)
        .await
        .map_err(internal)?;
    // A real change evicts anything cached before the flag flipped: the
    // live host, the preview host and every active custom domain.
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
    let summary = if input.adults_only {
        format!(
            "{} is now adults-only (18+): visitors confirm their age before the app serves them.",
            app.slug,
        )
    } else {
        format!(
            "{} is no longer marked adults-only — visitors go straight in.",
            app.slug,
        )
    };
    Ok(ok(summary, json!({"adults_only": input.adults_only})))
}
