//! The preview tools: `promote_preview` and `discard_preview` (Pro).
//!
//! Both share their domain path with the console endpoints, so the plan
//! checks, the git fast-forward and the cleanup behave identically from
//! MCP and the Deploys view. `commit {preview: true}` lives in
//! [`super::code`]: it reuses the whole commit input shape with one flag.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{internal, latest_preview_deploy, ok, plan_of, run_deploy, visible_app, Ctx};
use crate::mcp::error::ToolError;
use crate::mcp::McpState;
use crate::preview::{self, preview_of};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PromotePreviewInput {
    /// The app slug.
    pub app: String,
    /// Wait for the live deploy (default true). False returns immediately;
    /// poll deploy_status.
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiscardPreviewInput {
    /// The app slug.
    pub app: String,
}
/// The Pro gate for preview creation and promotion: the owner's plan pays,
/// whoever calls. A plan-lookup failure propagates as `internal`: a transient
/// DB error must never read as a Free plan refusing with `payment_required`.
/// `feature` is the lowercase noun ("preview deploys"), `action` the verb
/// phrase for the human text.
pub(crate) async fn require_pro(
    ctx: &Ctx<'_>,
    owner_id: &str,
    slug: &str,
    feature: &str,
    verb: &str,
    action: &str,
) -> Result<(), ToolError> {
    require_pro_state(ctx.state, owner_id, slug, feature, verb, action).await
}

pub(crate) async fn require_pro_state(
    state: &McpState,
    owner_id: &str,
    slug: &str,
    feature: &str,
    verb: &str,
    action: &str,
) -> Result<(), ToolError> {
    let plan = plan_of(&state.pool, owner_id).await?;
    if plan != "pro" {
        return Err(super::with_upgrade(
            ToolError::payment_required(
                format!("{slug} is on a Free account; {feature} {verb} a Pro feature"),
                format!(
                    "{} {verb} a Pro feature. Upgrade to Pro to {action}.",
                    capitalize(feature)
                ),
            ),
            &state.billing,
            owner_id,
        ));
    }
    Ok(())
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
pub async fn promote_preview(
    input: PromotePreviewInput,
    ctx: &Ctx<'_>,
) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    promote_preview_to(ctx.state, &app, input.wait.unwrap_or(true)).await
}

/// The shared promote path: the MCP tool and the console endpoint both run
/// here, so the plan gate, the success-deploy check, the fast-forward and
/// the live deploy are identical. `app` must already be role-checked by the
/// caller (MCP's `visible_app` and the console's `mutation_app` refuse
/// role-less slugs as `not_found`; collaborators may promote).
pub async fn promote_preview_to(
    state: &McpState,
    app: &crate::apps::App,
    wait: bool,
) -> Result<Value, ToolError> {
    require_pro_state(
        state,
        &app.owner_id,
        &app.slug,
        "preview promotion",
        "is",
        "promote a preview to live",
    )
    .await?;
    // Fail fast before the fast-forward and the trial-state wipe below: a
    // capped or frozen app refuses here with the quota shape, and the inner
    // `run_deploy` gate backstops the quota spent in between.
    super::check_build_quota(&state.pool, &state.billing, &app.owner_id).await?;
    super::check_storage_writable(&state.pool, &state.billing, app).await?;
    let preview = preview_of(app)
        .ok_or_else(|| ToolError::not_found(format!("no active preview for {:?}", app.slug)))?;
    let row = latest_preview_deploy(&state.pool, &app.id).await?;
    match row {
        Some(row) if row.sha == preview.sha && row.status == "success" => {}
        _ => {
            return Err(ToolError::invalid_argument(
                "the preview has no successful deploy yet: run commit with preview: true and wait for success first",
            ));
        }
    }
    let sha = preview.sha.clone();
    // Fast-forward main to the preview commit. The sync lock serializes this
    // with racing main and preview commits; a moved main refuses instead of
    // superseding the live change.
    match crate::git::manifest::promote_preview(
        &state.store,
        &state.git,
        &state.sync,
        &app.slug,
        &sha,
    )
    .await
    {
        Ok(_) => {}
        // A cancelled promote can strand the manifest ahead of the row:
        // main already names the preview tip while the DB still does. That
        // is the completed fast-forward, not a conflict — clear the row and
        // deploy it instead of refusing.
        Err(e) if e.to_string().contains("no preview to promote") => {
            let main = crate::git::manifest::read_manifest(&state.store, &app.slug)
                .await
                .map_err(internal)?
                .and_then(|m| m.main.map(|r| r.sha));
            if main.as_deref() != Some(sha.as_str()) {
                return Err(map_promote_err(state, &app.slug, &e).await);
            }
            tracing::info!(slug = %app.slug, sha = %sha, "promote resumed after a cancelled run");
        }
        Err(e) => return Err(map_promote_err(state, &app.slug, &e).await),
    }
    // The preview is over: stop its fleet and wipe the trial namespace,
    // deploy rows and row state (same cleanup as discard, minus the manifest
    // entry the promote already cleared). The next preview then starts from
    // a clean slate and routes as a first deploy.
    preview::clear_preview_trial_state(
        &state.pool,
        &state.store,
        &state.git,
        &state.fleets,
        &app.id,
        &app.slug,
    )
    .await
    .map_err(internal)?;
    let deploy = run_deploy(state, app, &sha, wait).await?;
    Ok(super::ok_deploy(
        format!(
            "Promoted the preview to live ({}).",
            deploy["status"].as_str().unwrap_or("")
        ),
        &deploy.clone(),
        json!({"sha": sha, "deploy": deploy}),
    ))
}
async fn map_promote_err(state: &McpState, slug: &str, err: &anyhow::Error) -> ToolError {
    let text = format!("{err:#}");
    if text.contains("main moved") {
        // Name the live head the agent must rebase onto.
        let current = crate::git::manifest::read_manifest(&state.store, slug)
            .await
            .ok()
            .flatten()
            .and_then(|m| m.main.map(|r| r.sha))
            .unwrap_or_default();
        return ToolError::conflict(
            format!("a live commit landed after the preview branched: {text}"),
            &current,
        )
        .with_user(
            "Someone committed to the live site after the preview branched. Run discard_preview, then commit a fresh preview from the new live version and promote that.",
        );
    }
    if text.contains("no preview to promote") || text.contains("preview moved") {
        return ToolError::invalid_argument(format!(
            "the preview changed under this promote: {text}"
        ));
    }
    tracing::warn!("promote_preview internal: {text}");
    ToolError::internal("the promote failed on our side; try again in a minute")
}

/// Discard the app's preview: the preview fleet stops and the trial state
/// goes away; live is untouched. Cleanup is never plan-gated: a downgraded
/// account can always shed its preview.
pub async fn discard_preview(
    input: DiscardPreviewInput,
    ctx: &Ctx<'_>,
) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    discard_preview_for(ctx.state, &app).await
}

/// The shared discard path: the MCP tool and the console endpoint both run
/// here. Cleanup is never plan-gated: a downgraded account can always shed
/// its preview. `app` must already be role-checked by the caller.
pub async fn discard_preview_for(
    state: &McpState,
    app: &crate::apps::App,
) -> Result<Value, ToolError> {
    let discarded = preview::discard_state(
        &state.pool,
        &state.store,
        &state.git,
        &state.sync,
        &state.fleets,
        &app.id,
        &app.slug,
    )
    .await
    .map_err(internal)?;
    if !discarded {
        return Err(ToolError::not_found(format!(
            "no active preview for {:?}",
            app.slug
        )));
    }
    Ok(ok(
        format!("Discarded the preview for {}.", app.slug),
        json!({"app": app.slug}),
    ))
}
