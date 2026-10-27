//! The app tools: `create_app`, `list_apps`, `get_app`, `set_visibility`,
//! `share_app`, plus the collaborator tools (Pro, spec §7.4). Reads and
//! `share_app` admit collaborators; visibility, collaborators and delete
//! stay owner-only (spec §8.2 table).

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    authed, check_app_quota, internal, latest_deploy, latest_preview_deploy, ok, owner_app,
    run_deploy, visible_app, Ctx, NoArgs,
};
use crate::apps::{self, App, Visibility};
use crate::collaborators::Role;
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateAppInput {
    /// URL name: lowercase letters, digits and dashes, 1-48 chars. Brand and
    /// account-page names (paypal, apple, bank, login, …) and flagged names
    /// are refused; without a Jev client (localhost dev) the content check
    /// is skipped.
    pub slug: String,
    /// Display name, 1–60 chars, no control chars (defaults to the slug
    /// title-cased: `kalkulator-pizzy` → `Kalkulator Pizzy`). Written to the
    /// repo's `brand.json` with the accent; the app row keeps the same name.
    /// Checked by the same content check as the slug.
    pub name: Option<String>,
    /// One-line pitch, 1–160 chars, no control chars (defaults to the name).
    /// Written to the repo's `brand.json`. Checked by the same content
    /// check as the slug, with the same refusal.
    pub description: Option<String>,
    /// Which starter to seed from: "app" (interactive Ilha + Void app) or
    /// "website" (Astro + Ilha-islands content site). Defaults to "app";
    /// any other value is refused. Pick "website" for landing pages,
    /// portfolios, blogs and event pages; "app" for anything with accounts,
    /// live state or dashboards.
    pub template: Option<String>,
    /// Ready features to seed in the first commit: applied onto the empty
    /// template before it, so the app is born with working pages. Unknown
    /// names are refused before anything is created.
    pub examples: Option<Vec<String>>,
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
pub struct SetAlwaysOnInput {
    /// The app slug.
    pub app: String,
    /// true keeps the fleet awake (Pro only); false lets it sleep on idle.
    pub on: bool,
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
    /// old name redirects to the new one until someone claims it. Brand
    /// and account-page names and flagged names are refused like at
    /// creation.
    pub slug: String,
}

/// Seed `slug` from the vendored template and create its repo. Mirrors
/// `ops::create_app` without the deploy: the MCP layer runs deploys through
/// its own [`super::Deploys`] so tests can substitute a fake.
/// Everything `seed_app` needs beyond the shared state.
struct SeedRequest<'a> {
    owner_id: &'a str,
    owner_email: &'a str,
    slug: &'a str,
    name: Option<&'a str>,
    description: Option<&'a str>,
    template: crate::templates::Template,
    examples: &'a [crate::examples::Example],
}

async fn seed_app(
    ctx: &Ctx<'_>,
    seed: SeedRequest<'_>,
) -> Result<(App, String, Vec<String>, Vec<String>), ToolError> {
    let SeedRequest {
        owner_id,
        owner_email,
        slug,
        name,
        description,
        template,
        examples,
    } = seed;
    // so a refused name or description costs nothing.
    let display_name = crate::brand::resolve_brand(slug, name, description)
        .map(|brand| brand.name)
        .map_err(|err| ToolError::invalid_argument(err.to_string()))?;
    let scratch = tempfile::tempdir().map_err(internal)?;
    let tree = scratch.path().join("tree");
    let reference = crate::templates::materialize(&ctx.state.store, template, &tree)
        .await
        .map_err(internal)?;
    // The seed commit carries the brand: it overwrites the template's own
    // `brand.json` with the new app's values.
    crate::templates::write_brand(&tree, slug, name, description)
        .map_err(|err| ToolError::invalid_argument(err.to_string()))?;
    // Optional examples apply into the seed tree before the first commit,
    // so the app is born with working features: one commit, one deploy.
    let mut added = Vec::new();
    let mut nav = Vec::new();
    if !examples.is_empty() {
        let inputs: Vec<crate::examples::apply::ExampleInput> = examples
            .iter()
            .map(|example| example.apply_input())
            .collect();
        let requested: Vec<&str> = examples
            .iter()
            .map(|example| example.name.as_str())
            .collect();
        let seed = read_tree(&tree).map_err(internal)?;
        let applied = crate::examples::apply::plan_apply(&seed, template.id(), &inputs, &requested)
            .map_err(|refusal| ToolError::invalid_argument(refusal.to_string()))?;
        for (path, text) in &applied.writes {
            let dest = tree.join(path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(internal)?;
            }
            std::fs::write(&dest, text).map_err(internal)?;
        }
        added = applied.added;
        nav = applied.nav;
    }
    let plan = super::plan_of(&ctx.state.pool, owner_id).await?;
    let mut app = apps::create(
        &ctx.state.pool,
        owner_id,
        slug,
        Some(&display_name),
        &ctx.state.alloc,
        super::app_limit(&plan),
    )
    .await
    .map_err(|err| ToolError::from_create(err, &plan, &ctx.state.billing, owner_id))?;
    // New Pro apps default to keep-warm (feature #6); Free apps start
    // cold. The flag is stored per app, so a later downgrade keeps the
    // value but the sweep ignores it until the owner upgrades again. Best
    // effort: the row already exists here, so a flag write that fails must
    // not abort creation and orphan it (the app stays usable; the flag can
    // be set later). Seeding failures below still remove the row.
    if plan == "pro" {
        match apps::always_on::set_always_on(&ctx.state.pool, &app.id, true, &plan).await {
            Ok(()) => app.always_on = true,
            Err(err) => tracing::warn!(
                slug = %slug,
                error = %format!("{err:#}"),
                "pro default keep-warm flag not set; set it later"
            ),
        }
    }
    let author = crate::ops::noreply_author(owner_id, owner_email, &ctx.state.domain);
    let seeded = async {
        // Claim-point wipe: a crashed delete can leave local orphans behind
        // under this now-unclaimed slug. The row insert above is the claim
        // and nothing local is written yet, so wipe before `init_repo`. A
        // wipe failure lands in the row-removing cleanup below (nobody ever
        // saw the slug). The stale in-memory manifest seq goes too, or
        // `ensure_synced` could read it as already-applied.
        crate::apps::local::remove_local_state(
            &ctx.state.delete_cfg.data_dir,
            &ctx.state.git,
            ctx.state.delete_cfg.cgroups.as_deref(),
            slug,
        )
        .await?;
        ctx.state.sync.forget(slug).await;
        crate::git::manifest::init_repo(&ctx.state.store, &ctx.state.git, &ctx.state.sync, slug)
            .await?;
        let message = template.seed_message(&reference.sha);
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
    Ok((app, sha, added, nav))
}

pub async fn create_app(input: CreateAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    apps::validate_slug(&input.slug).map_err(|e| ToolError::invalid_argument(e.to_string()))?;
    let template = match input.template.as_deref() {
        None => crate::templates::Template::App,
        Some(id) => crate::templates::Template::from_id(id).ok_or_else(|| {
            ToolError::invalid_argument(format!(
                "unknown template {id:?}: want \"app\" or \"website\""
            ))
        })?,
    };
    // Requested examples load before anything is created: an unknown name
    // refuses before any row exists.
    let wanted: Vec<String> = input.examples.clone().unwrap_or_default();
    let examples = if wanted.is_empty() {
        Vec::new()
    } else {
        super::examples::load_closure(&ctx.state.store, &ctx.state.delete_cfg.data_dir, &wanted)
            .await?
    };
    check_app_quota(&ctx.state.pool, &ctx.state.billing, &caller.account_id).await?;
    // The name and description checks run before any row exists and before
    // the quota claim, so refused text costs nothing and Jev never sees a
    // name the cheap checks already refused.
    apps::check_slug_profanity(
        ctx.state.jev.as_ref(),
        &input.slug,
        input.name.as_deref(),
        input.description.as_deref(),
    )
    .await
    .map_err(ToolError::from_profanity)?;
    let (app, sha, added, nav) = seed_app(
        ctx,
        SeedRequest {
            owner_id: &caller.account_id,
            owner_email: &caller.email,
            slug: &input.slug,
            name: input.name.as_deref(),
            description: input.description.as_deref(),
            template,
            examples: &examples,
        },
    )
    .await?;
    // The uid set changed: reinstall so the new uid's per-uid rules exist
    // at once (best-effort; the sweep tick is the backstop).
    ctx.state.egress.refresh(&ctx.state.pool).await;
    // The seed deploy carries the shared admission gate: a capped or frozen
    // app refuses here with the quota shape, and the `?` leaves the seeded
    // app row in place — the agent retries the deploy after the reset.
    let deploy = run_deploy(ctx.state, &app, &sha, input.wait.unwrap_or(true)).await?;
    // Name this template's ready features from the live examples list; a
    // missing or empty list falls back to a plain pointer. Never fails the
    // creation: examples are a hint, not the app.
    let hint = match crate::examples::list(
        &ctx.state.store,
        &ctx.state.delete_cfg.data_dir,
        Some(template),
    )
    .await
    {
        Ok(examples) if !examples.is_empty() => {
            let names = examples
                .iter()
                .map(|example| example.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("For ready features ({names}) call add_example {{app, name}}.")
        }
        _ => "For ready features call add_example {app, name}.".to_string(),
    };
    let url = ctx.state.app_url(&app.slug);
    let lead = seed_deploy_lead(&app.slug, &url, &deploy);
    let summary = if added.is_empty() {
        let shell = match template {
            crate::templates::Template::App => format!(
                "{lead} It's an empty app shell: build what your user asked for. Pages go in pages/ (list them in pages/_nav.ts). {hint}",
            ),
            crate::templates::Template::Website => format!(
                "{lead} It's an empty website shell: build what your user asked for. Content lives in Astro pages (islands for interactive parts). {hint}",
            ),
        };
        format!(
            "{shell} Nothing the user asked for exists yet: add_example or commit before telling them it's built."
        )
    } else {
        let live = if nav.is_empty() {
            "working files".to_string()
        } else {
            nav.join(", ")
        };
        match deploy["status"].as_str().unwrap_or("unknown") {
            "success" => format!(
                "{} is live at {} with {}: {} are live. Customise the copy with commit; check with look_at_app. {}",
                app.slug,
                url,
                added.join(", "),
                live,
                hint,
            ),
            _ => format!("{lead} with {}. {hint}", added.join(", ")),
        }
    };
    let structured = json!({
        "app": {"slug": app.slug, "url": ctx.state.app_url(&app.slug), "visibility": app.visibility.as_str()},
        "deploy": deploy,
        "added": added,
        "next": "open_app {app} to orient, then read_files (batched ranges), search_code, add_example for ready features, commit edits",
    });
    Ok(super::ok_deploy(
        summary,
        &structured["deploy"].clone(),
        structured,
    ))
}

/// The seed tree as path → full text, app-relative with `/` separators.
fn read_tree(dir: &std::path::Path) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    fn visit(
        dir: &std::path::Path,
        base: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, base, out)?;
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .map_err(|err| anyhow::anyhow!("{err:#}"))?
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, std::fs::read_to_string(&path)?);
            }
        }
        Ok(())
    }
    let mut out = std::collections::BTreeMap::new();
    visit(dir, dir, &mut out)?;
    Ok(out)
}

/// The first sentence of `create_app`'s result, true to the seed deploy:
/// only a `success` says "live". A failed or blocked first deploy names the
/// step and error so the agent fixes it instead of reporting a dead link;
/// one still running (`wait: false` or the wait cap) says how to follow it.
fn seed_deploy_lead(slug: &str, url: &str, deploy: &Value) -> String {
    let status = deploy["status"].as_str().unwrap_or("unknown");
    match status {
        "success" => format!("{slug} is live at {url}."),
        "failed" | "blocked" => {
            let step = deploy["failed_step"]
                .as_str()
                .map(|step| format!(" at {step}"))
                .unwrap_or_default();
            let error = deploy["error"]
                .as_str()
                .map(|error| format!(": {error}"))
                .unwrap_or_default();
            format!(
                "{slug} was created, but its first deploy {status}{step}{error}. It is not live yet: fix it and commit (deploy_log {{app}} has the full log)."
            )
        }
        _ => format!(
            "{slug} was created; its first deploy is {status}. Call deploy_status {{app, wait: true}} until it reports success at {url}."
        ),
    }
}

pub(crate) fn app_status(app: &App, last_status: Option<&str>) -> &'static str {
    if app.expired_at.is_some() {
        return "expired";
    }
    if app.asleep_since.is_some() {
        return "sleeping";
    }
    match last_status {
        Some("queued") | Some("building") | Some("reviewing") | Some("deploying") => "building",
        Some("failed") => "failed",
        _ => "running",
    }
}

pub async fn app_summary(ctx: &Ctx<'_>, app: &App, role: Role) -> Result<Value, ToolError> {
    let last = latest_deploy(&ctx.state.pool, &app.id).await?;
    let status = app_status(app, last.as_ref().map(|r| r.status.as_str()));
    let mut v = json!({
        "slug": app.slug,
        "url": ctx.state.app_url(&app.slug),
        "role": match role {
            Role::Owner => "owner",
            Role::Collaborator => "collaborator",
        },
        "visibility": app.visibility.as_str(),
        // Pro keep-warm flag (feature #6); Free owners see it stored but
        // ignored until they upgrade.
        "always_on": app.always_on,
        "status": status,
    });
    // Inactivity deadline (spec §7.8) when it applies: Free owners see when
    // the app goes away without activity; Pro owners are exempt (no key).
    let (owner_plan, owner_last) =
        crate::inactivity::owner_clock(&ctx.state.pool, &app.owner_id).await;
    if let Some(deletes_at) = crate::inactivity::deletes_at_ms(
        app.last_activity_at,
        owner_last,
        &owner_plan,
        app.expired_at,
        app.downgraded_stopped_at,
        ctx.state.inactivity,
    ) {
        v["deletes_at"] = json!(deletes_at);
    }
    if let Some(last) = last {
        v["last_deploy"] = json!({"sha": last.sha, "status": last.status});
    }
    if let Some(preview) = crate::preview::preview_of(app) {
        let preview_status = latest_preview_deploy(&ctx.state.pool, &app.id)
            .await?
            .map(|row| row.status)
            .unwrap_or_else(|| "building".to_string());
        v["preview"] = json!({
            "url": crate::preview::preview_url(
                &ctx.state.app_scheme,
                &app.slug,
                &ctx.state.apps_domain,
                &ctx.state.app_port_suffix,
            ),
            "sha": preview.sha,
            "status": preview_status,
            "updated_at": preview.updated_at_ms,
        });
    }
    Ok(v)
}

pub async fn list_apps(_input: NoArgs, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let all = apps::list(&ctx.state.pool).await.map_err(internal)?;
    let mut out = Vec::new();
    for app in &all {
        // One role lookup per app: owners match inline, collaborators read
        // their row (Pro-gated, so a downgrade hides the app again).
        let role = crate::collaborators::role(
            &ctx.state.pool,
            &app.id,
            &app.owner_id,
            &caller.account_id,
            &caller.email,
        )
        .await
        .map_err(internal)?;
        if let Some(role) = role {
            out.push(app_summary(ctx, app, role).await?);
        }
    }
    Ok(ok(format!("{} app(s).", out.len()), json!({"apps": out})))
}

pub async fn get_app(input: GetAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, role) = visible_app(ctx, &input.app).await?;
    Ok(ok(
        format!("{} ({})", app.slug, ctx.state.app_url(&app.slug)),
        app_summary(ctx, &app, role).await?,
    ))
}

pub async fn set_visibility(input: SetVisibilityInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
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
        .map_err(|err| visibility_tool_error(&app.slug, err, &ctx.state.billing, &app.owner_id))?;
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
/// `billing` + `owner_id` supply the `upgrade_url` the refusal carries.
pub fn visibility_tool_error(
    slug: &str,
    err: crate::apps::VisibilityError,
    billing: &crate::billing::BillingState,
    owner_id: &str,
) -> ToolError {
    match err {
        crate::apps::VisibilityError::ProOnly => super::with_upgrade(
            ToolError::new(
                "payment_required",
                format!("{slug} is on a Free account; public apps are a Pro feature"),
            )
            .with_user(format!(
                "Public apps are Pro. Upgrade to Pro to make {slug} public; switching back to unlisted is always free.",
            )),
            billing,
            owner_id,
        ),
        crate::apps::VisibilityError::Internal(err) => internal(err),
    }
}

/// Set the keep-warm flag (feature #6, owner only): `on` keeps the fleet
/// awake on Pro (up to the host cap); a Free owner is refused with
/// `payment_required`, and `off` is always allowed.
pub async fn set_always_on(input: SetAlwaysOnInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    // The Pro gate lives in the shared domain function, so the console
    // endpoint gating on the same function refuses identically.
    let plan = super::plan_of(&ctx.state.pool, &app.owner_id).await?;
    apps::always_on::set_always_on(&ctx.state.pool, &app.id, input.on, &plan)
        .await
        .map_err(|err| always_on_tool_error(&app.slug, err, &ctx.state.billing, &app.owner_id))?;
    Ok(ok(
        if input.on {
            format!("{} stays awake from now on.", app.slug)
        } else {
            format!("{} sleeps on idle again.", app.slug)
        },
        json!({"always_on": input.on}),
    ))
}

/// Map the shared keep-warm refusal to the error both surfaces return:
/// Free owners get `payment_required`; turning the flag off never fails.
/// `billing` + `owner_id` supply the `upgrade_url` the refusal carries.
pub fn always_on_tool_error(
    slug: &str,
    err: crate::apps::always_on::AlwaysOnError,
    billing: &crate::billing::BillingState,
    owner_id: &str,
) -> ToolError {
    match err {
        crate::apps::always_on::AlwaysOnError::ProOnly { .. } => super::with_upgrade(
            ToolError::new(
                "payment_required",
                format!("{slug} is on a Free account; keeping apps warm is a Pro feature"),
            )
            .with_user(format!(
                "Keeping {slug} warm is Pro. Upgrade to Pro so it never sleeps; turning the flag off is always free.",
            )),
            billing,
            owner_id,
        ),
        crate::apps::always_on::AlwaysOnError::Internal(err) => internal(err),
    }
}

pub async fn share_app(input: ShareAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
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
/// its bucket prefixes and local state, drops its rows (the slug is
/// claimable again immediately), and purges its edge cache. `confirm`
/// must equal the slug exactly; anything else is refused with what to
/// confirm. A slug the caller has no role on answers `not_found`, exactly
/// like a missing one.
pub async fn delete_app(input: DeleteAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    // `not_found` first: a foreign slug never reaches the confirm check.
    let app = owner_app(ctx, &input.app).await?;
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
    // The uid set changed: reinstall so the deleted uid's per-uid rules go
    // away at once (best-effort; the sweep tick is the backstop).
    ctx.state.egress.refresh(&ctx.state.pool).await;
    let user_message = format!("{host} is deleted. The name is free to claim again.");
    Ok(ok(
        user_message.clone(),
        json!({
            "deleted": true,
            "slug": out.slug,
            "user_message": user_message,
        }),
    ))
}

/// Rename an app (owner only): move its slug, hostname, code, data and
/// files to `slug`; the old name redirects to the new hostname until
/// someone claims it. D1 and Durable Object data survive: the fleet's
/// bucket prefix moves with the app. Counts one code change; a running app
/// redeploys under the new name. A slug the caller has no role on answers
/// `not_found`, exactly like a missing one.
pub async fn rename_app(input: RenameAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    // `not_found` first: a foreign slug never reaches validation or quota.
    let app = owner_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    if input.app == input.slug {
        return Err(ToolError::invalid_argument(format!(
            "the app is already named {:?}; nothing to rename",
            input.app
        )));
    }
    apps::validate_slug(&input.slug).map_err(|e| ToolError::invalid_argument(e.to_string()))?;
    // After the cheap checks, before the quota claim: a refused name costs
    // nothing and Jev never runs for a malformed, reserved or brand slug.
    apps::check_slug_profanity(ctx.state.jev.as_ref(), &input.slug, None, None)
        .await
        .map_err(ToolError::from_profanity)?;
    // Claimed before the move; refunded on any error. The domain rename
    // never fails post-commit (post-commit steps are warn-and-continue, and
    // the maintenance drain finishes them), so an error always means the
    // rename never happened — while a committed rename counts even when
    // its redeploy fails or is blocked.
    let claim =
        super::claim_change(&ctx.state.pool, &ctx.state.billing, &caller.account_id).await?;
    // The rename redeploys a running app, so the redeploy's plan admission
    // runs here too, before the move: a capped or frozen app refuses with
    // the quota shape and the rename never happens (the claimed change is
    // refunded). The inner `run_deploy` gate backstops the quota spent after
    // this check.
    if let Err(err) =
        super::check_build_quota(&ctx.state.pool, &ctx.state.billing, &caller.account_id).await
    {
        let _ = super::refund_change(&ctx.state.pool, &caller.account_id).await;
        return Err(err);
    }
    if let Err(err) = super::check_storage_writable(&ctx.state.pool, &ctx.state.billing, &app).await
    {
        let _ = super::refund_change(&ctx.state.pool, &caller.account_id).await;
        return Err(err);
    }
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
        "{} is now {} at {}. The old name redirects here until claimed.",
        out.old_slug, out.app.slug, url
    );
    let structured = json!({
        "slug": out.app.slug,
        "url": url,
        "old_slug": out.old_slug,
        "quota": quota,
    });
    match out.redeploy_sha {
        Some(sha) => {
            // The rename already committed (the quota counts): a redeploy
            // failure — infrastructure or a plan refusal from the shared
            // admission gate — still answers rename success, like the
            // console path, with the failure embedded in the deploy object.
            let deploy = match run_deploy(ctx.state, &out.app, &sha, true).await {
                Ok(deploy) => deploy,
                Err(err) => json!({"status": "failed", "sha": sha, "error": err.message}),
            };
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

#[cfg(test)]
mod tests {
    use super::seed_deploy_lead;
    use serde_json::json;

    const URL: &str = "https://shop.outer.test";

    #[test]
    fn only_a_successful_seed_deploy_is_reported_live() {
        let live = seed_deploy_lead("shop", URL, &json!({"status": "success"}));
        assert_eq!(live, "shop is live at https://shop.outer.test.");

        let failed = seed_deploy_lead(
            "shop",
            URL,
            &json!({
                "status": "failed",
                "failed_step": "deploy",
                "error": "Error: an asset-only project cannot declare Worker bindings",
            }),
        );
        assert!(!failed.contains("is live"), "{failed}");
        assert!(
            failed.contains("first deploy failed at deploy: Error: an asset-only project"),
            "{failed}"
        );

        let blocked = seed_deploy_lead("shop", URL, &json!({"status": "blocked"}));
        assert!(blocked.contains("first deploy blocked."), "{blocked}");
        assert!(!blocked.contains("is live"), "{blocked}");

        let building = seed_deploy_lead("shop", URL, &json!({"status": "building"}));
        assert!(building.contains("deploy_status"), "{building}");
        assert!(!building.contains("is live"), "{building}");
    }
}
