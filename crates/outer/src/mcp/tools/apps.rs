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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenameAppInput {
    /// The app's current slug.
    pub app: String,
    /// The new slug: lowercase letters, digits and dashes, 1-48 chars. The
    /// old name stays reserved for 30 days and redirects to the new one.
    pub slug: String,
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
    // The Pro gate lives in the shared domain function, so the console
    // endpoint gating on the same function refuses identically.
    let plan = super::plan_of(&ctx.state.pool, &app.owner_id).await?;
    apps::set_visibility(&ctx.state.pool, &app.id, visibility, &plan)
        .await
        .map_err(|err| visibility_tool_error(&app.slug, err))?;
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

/// Map the shared visibility refusal to the error both surfaces return: the
/// MCP `isError` and the console HTTP body carry the same code and user
/// message. Free owners get `payment_required` with a user message pointing
/// to Pro; switching back to `unlisted` never reaches this gate.
pub fn visibility_tool_error(slug: &str, err: crate::apps::VisibilityError) -> ToolError {
    match err {
        crate::apps::VisibilityError::ProOnly => ToolError::new(
            "payment_required",
            format!("{slug} is on a Free account; public apps are a Pro feature"),
        )
        .with_user(format!(
            "Public apps are Pro. Upgrade to Pro to make {slug} public; switching back to unlisted is always free.",
        )),
        crate::apps::VisibilityError::Internal(err) => internal(err),
    }
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

/// Rename an app (owner only): move its slug, hostname, code, data and
/// files to `slug`, and quarantine the old slug for 30 days (it answers 308
/// to the new hostname while the quarantine holds). D1 and Durable Object
/// data survive: the fleet's bucket prefix moves with the app. Counts one
/// code change; a running app redeploys under the new name. A slug the
/// caller has no role on answers `not_found`, exactly like a missing one.
pub async fn rename_app(input: RenameAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    // `not_found` first: a foreign slug never reaches validation or quota.
    let app = owned_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    if input.app == input.slug {
        return Err(ToolError::invalid_argument(format!(
            "the app is already named {:?}; nothing to rename",
            input.app
        )));
    }
    apps::validate_slug(&input.slug).map_err(|e| ToolError::invalid_argument(e.to_string()))?;
    // Claimed before the move; refunded on any error. The domain rename
    // never fails post-commit (post-commit steps are warn-and-continue, and
    // the maintenance drain finishes them), so an error always means the
    // rename never happened — while a committed rename counts even when
    // its redeploy fails or is blocked.
    let claim = super::claim_change(&ctx.state.pool, &caller.account_id).await?;
    // Serialize against a racing rollback, deploy and wake, on both slugs:
    // the wake pair sorts by slug so two renames can never ABBA.
    let rollback = ctx.state.rollback_locks.lock(&app.id).await;
    let deploy = ctx.state.deploys.app_lock(&app.id).lock_owned().await;
    let (first, second) = if input.app.as_str() <= input.slug.as_str() {
        (&input.app, &input.slug)
    } else {
        (&input.slug, &input.app)
    };
    let wake_first_key = ctx.state.fleets.wake_lock(first).await;
    let wake_first = wake_first_key.lock_owned().await;
    let wake_second_key = ctx.state.fleets.wake_lock(second).await;
    let wake_second = wake_second_key.lock_owned().await;
    let (wake_old, wake_new) = if first == &input.app {
        (&wake_first, &wake_second)
    } else {
        (&wake_second, &wake_first)
    };
    let locks = crate::apps::rename::RenameLocks {
        deploy: &deploy,
        rollback: &rollback,
        wake_old,
        wake_new,
    };
    let deps = crate::apps::rename::RenameDeps {
        pool: &ctx.state.pool,
        store: &ctx.state.store,
        git: &ctx.state.git,
        sync: &ctx.state.sync,
        fleets: Some(&ctx.state.fleets),
        data_dir: &ctx.state.delete_cfg.data_dir,
        cgroups: ctx.state.delete_cfg.cgroups.as_deref(),
        apps_domain: &ctx.state.delete_cfg.apps_domain,
        cache_purge: ctx.state.delete_cfg.cache_purge.as_ref(),
    };
    let out = match crate::apps::rename::rename_app(&deps, &input.app, &input.slug, locks).await {
        Ok(out) => out,
        Err(err) => {
            if let Err(refund) = super::refund_change(&ctx.state.pool, &caller.account_id).await {
                tracing::warn!("rename refund failed: {refund:#}");
            }
            return Err(ToolError::from_rename(err));
        }
    };
    let url = ctx.state.app_url(&out.app.slug);
    let quota =
        json!({"limit": claim.limit, "used": claim.used, "resets_at": super::resets_at_utc()});
    let summary = format!(
        "{} is now {} at {}. The name {} stays reserved for 30 days.",
        out.old_slug, out.app.slug, url, out.old_slug
    );
    let structured = json!({
        "slug": out.app.slug,
        "url": url,
        "quarantined_until": out.quarantined_until,
        "quota": quota,
    });
    match out.redeploy_sha {
        Some(sha) => {
            let deploy = run_deploy(ctx.state, &out.app, &sha, true).await?;
            let mut structured = structured;
            structured["deploy"] = deploy;
            Ok(super::ok_deploy(
                summary,
                &structured["deploy"].clone(),
                structured,
            ))
        }
        None => Ok(ok(summary, structured)),
    }
}
