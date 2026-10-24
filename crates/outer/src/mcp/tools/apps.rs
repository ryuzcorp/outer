//! The app tools: `create_app`, `list_apps`, `get_app`, `set_visibility`,
//! `share_app`. Owner only in M1 (collaborators are Pro/M2).

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    authed, check_app_quota, internal, latest_deploy, ok, owned_app, run_deploy, Ctx, NoArgs,
};
use crate::apps::{self, App, Visibility};
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateAppInput {
    /// URL name: lowercase letters, digits and dashes, 1-48 chars.
    pub slug: String,
    /// Display name (defaults to the slug).
    pub name: Option<String>,
    /// Wait for the first deploy (default true). False returns immediately;
    /// poll deploy_status.
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetAppInput {
    /// The app slug.
    pub app: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetVisibilityInput {
    /// The app slug.
    pub app: String,
    /// unlisted (default, kept out of search engines) or public.
    pub visibility: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShareAppInput {
    /// The app slug.
    pub app: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteAppInput {
    /// The app slug.
    pub app: String,
    /// Type the slug again to confirm: must equal `app` exactly.
    pub confirm: String,
}
/// Seed `slug` from the vendored template and create its repo. Mirrors
/// `ops::create_app` without the deploy: the MCP layer runs deploys through
/// its own [`super::Deploys`] so tests can substitute a fake.
async fn seed_app(
    ctx: &Ctx<'_>,
    owner_id: &str,
    owner_email: &str,
    slug: &str,
    name: Option<&str>,
) -> Result<(App, String), ToolError> {
    let scratch = tempfile::tempdir().map_err(internal)?;
    let tree = scratch.path().join("tree");
    let template = crate::templates::materialize(&ctx.state.store, &tree)
        .await
        .map_err(internal)?;
    let plan = super::plan_of(&ctx.state.pool, owner_id).await?;
    let app = apps::create(
        &ctx.state.pool,
        owner_id,
        slug,
        name,
        &ctx.state.alloc,
        super::app_limit(&plan),
    )
    .await
    .map_err(|err| ToolError::from_create(err, &plan))?;
    let author = crate::ops::noreply_author(owner_id, owner_email, &ctx.state.domain);
    let seeded = async {
        crate::git::manifest::init_repo(&ctx.state.store, &ctx.state.git, &ctx.state.sync, slug)
            .await?;
        let message = format!(
            "Start from the Ilha + Void template ({})",
            &template.sha[..12.min(template.sha.len())]
        );
        crate::git::commit::import_dir(
            &ctx.state.store,
            &ctx.state.git,
            &ctx.state.sync,
            slug,
            &tree,
            &message,
            &author,
        )
        .await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(err) = seeded {
        let _ =
            crate::git::manifest::purge(&ctx.state.store, &ctx.state.git, &ctx.state.sync, slug)
                .await;
        let _ = sqlx::query("DELETE FROM app WHERE id = ?1")
            .bind(&app.id)
            .execute(&ctx.state.pool)
            .await;
        return Err(internal(
            err.context(format!("seeding {slug} from the template")),
        ));
    }
    let bare = crate::git::manifest::ensure_synced(
        &ctx.state.store,
        &ctx.state.git,
        &ctx.state.sync,
        slug,
    )
    .await
    .map_err(internal)?;
    let sha = crate::git::manifest::head_of(&bare)
        .await
        .map_err(internal)?
        .ok_or_else(|| ToolError::internal(format!("seeded {slug} has no commits")))?;
    Ok((app, sha))
}

pub async fn create_app(input: CreateAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    apps::validate_slug(&input.slug).map_err(|e| ToolError::invalid_argument(e.to_string()))?;
    check_app_quota(&ctx.state.pool, &caller.account_id).await?;
    let (app, sha) = seed_app(
        ctx,
        &caller.account_id,
        &caller.email,
        &input.slug,
        input.name.as_deref(),
    )
    .await?;
    let deploy = run_deploy(ctx.state, &app, &sha, input.wait.unwrap_or(true)).await?;
    let summary = format!("{} is live at {}.", app.slug, ctx.state.app_url(&app.slug));
    let structured = json!({
        "app": {"slug": app.slug, "url": ctx.state.app_url(&app.slug), "visibility": app.visibility.as_str()},
        "deploy": deploy,
    });
    Ok(super::ok_deploy(
        summary,
        &structured["deploy"].clone(),
        structured,
    ))
}

pub(crate) fn app_status(app: &App, last_status: Option<&str>) -> &'static str {
    if app.asleep_since.is_some() {
        return "sleeping";
    }
    match last_status {
        Some("queued") | Some("building") | Some("reviewing") | Some("deploying") => "building",
        Some("failed") => "failed",
        _ => "running",
    }
}

pub async fn app_summary(ctx: &Ctx<'_>, app: &App) -> Result<Value, ToolError> {
    let last = latest_deploy(&ctx.state.pool, &app.id).await?;
    let status = app_status(app, last.as_ref().map(|r| r.status.as_str()));
    let mut v = json!({
        "slug": app.slug,
        "url": ctx.state.app_url(&app.slug),
        "role": "owner",
        "visibility": app.visibility.as_str(),
        "status": status,
    });
    if let Some(last) = last {
        v["last_deploy"] = json!({"sha": last.sha, "status": last.status});
    }
    Ok(v)
}

pub async fn list_apps(_input: NoArgs, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let all = apps::list(&ctx.state.pool).await.map_err(internal)?;
    let mut out = Vec::new();
    for app in all.iter().filter(|a| a.owner_id == caller.account_id) {
        out.push(app_summary(ctx, app).await?);
    }
    Ok(ok(format!("{} app(s).", out.len()), json!({"apps": out})))
}

pub async fn get_app(input: GetAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    Ok(ok(
        format!("{} ({})", app.slug, ctx.state.app_url(&app.slug)),
        app_summary(ctx, &app).await?,
    ))
}

pub async fn set_visibility(input: SetVisibilityInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    let visibility = match input.visibility.as_str() {
        "unlisted" => Visibility::Unlisted,
        "public" => Visibility::Public,
        other => {
            return Err(ToolError::invalid_argument(format!(
                "visibility must be unlisted or public, got {other:?}"
            )));
        }
    };
    apps::set_visibility(&ctx.state.pool, &app.id, visibility)
        .await
        .map_err(internal)?;
    // Visibility changes `X-Robots-Tag` and `/robots.txt`, which the edge
    // caches (robots.txt for hours): purge the host so the new answer is
    // what crawlers see. Best-effort, like the deploy and delete purges.
    let host = format!("{}.{}", app.slug, ctx.state.delete_cfg.apps_domain);
    crate::cloudflare::purge_host_cache(ctx.state.delete_cfg.cache_purge.as_ref(), &host).await;
    Ok(ok(
        format!("{} is now {}.", app.slug, visibility.as_str()),
        json!({"visibility": visibility.as_str()}),
    ))
}

pub async fn share_app(input: ShareAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    let url = ctx.state.app_url(&app.slug);
    let name = app.name.as_deref().unwrap_or(&app.slug);
    let text = format!("{name} is live at {url}");
    let qr = qrcode::QrCode::new(&url)
        .map(|code| {
            code.render::<qrcode::render::unicode::Dense1x2>()
                .dark_color(qrcode::render::unicode::Dense1x2::Dark)
                .light_color(qrcode::render::unicode::Dense1x2::Light)
                .build()
        })
        .map_err(|e| ToolError::internal(format!("qr render failed: {e}")))?;
    Ok(ok(
        text.clone(),
        json!({"url": url, "text": text, "qr": qr}),
    ))
}

/// Delete an app forever (spec §7.6, owner only): stops the fleet, wipes
/// its bucket prefixes and local state, drops its rows (quarantining the
/// slug for 30 days), and purges its edge cache. `confirm` must equal the
/// slug exactly; anything else is refused with what to confirm. A slug the
/// caller has no role on answers `not_found`, exactly like a missing one.
pub async fn delete_app(input: DeleteAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    // `not_found` first: a foreign slug never reaches the confirm check.
    let app = owned_app(ctx, &input.app).await?;
    // Serialize against a racing rollback, deploy and wake: rollback waits
    // on its guard instead of failing, while a deploy or wake holds its own
    // lock and runs to a state the idempotent steps tolerate.
    let rollback = ctx.state.rollback_locks.lock(&app.id).await;
    let deploy = ctx.state.deploys.app_lock(&app.id).lock_owned().await;
    let wake_key = ctx.state.fleets.wake_lock(&app.slug).await;
    let wake = wake_key.lock_owned().await;
    let locks = crate::apps::delete::DeleteLocks {
        deploy: &deploy,
        rollback: Some(&rollback),
        wake: Some(&wake),
    };
    let deps = crate::apps::delete::DeleteDeps {
        pool: &ctx.state.pool,
        store: &ctx.state.store,
        git: &ctx.state.git,
        sync: &ctx.state.sync,
        fleets: Some(&ctx.state.fleets),
    };
    let out = crate::apps::delete::delete_app(
        &deps,
        &ctx.state.delete_cfg,
        &app.slug,
        &input.confirm,
        locks,
    )
    .await
    .map_err(|err| match err {
        crate::apps::delete::DeleteError::ConfirmMismatch(slug, _) => {
            ToolError::invalid_argument(format!(
                "confirm {input:?} does not match the slug {slug:?}",
                input = input.confirm,
            ))
            .with_user(format!(
                "To delete {slug}, call delete_app with confirm {slug:?} (the slug, exactly)."
            ))
        }
        crate::apps::delete::DeleteError::NotFound(slug) => {
            ToolError::not_found(format!("no app named {slug:?}"))
        }
        crate::apps::delete::DeleteError::Internal(err) => internal(err),
    })?;
    let host = format!("{}.{}", out.slug, ctx.state.delete_cfg.apps_domain);
    let user_message = format!("{host} is deleted. The name stays reserved for 30 days.");
    Ok(ok(
        user_message.clone(),
        json!({
            "deleted": true,
            "slug": out.slug,
            "quarantined_until": out.quarantined_until,
            "user_message": user_message,
        }),
    ))
}
