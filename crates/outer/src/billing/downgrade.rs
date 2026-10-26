//! Downgrade extras and stopped-app retention (spec §3, §7.7).
//!
//! `downgrade_account` (in `super`) owns the `user`-row transition; this
//! module owns everything around the owner's apps: stripping Pro extras
//! through the same shared paths the tools use, and the 30-day
//! stopped-app retention with its day-23 warning mail.
//!
//! Spec choices, one place: a downgrade deletes every custom hostname on
//! every owned app (the same `remove_all_for_app` `delete_app` uses:
//! best-effort Cloudflare SaaS delete, row drop, edge purge of the host),
//! and forces every public app back to `unlisted` through the shared
//! `set_visibility` plus the same app-host edge purge the MCP tool and the
//! console do — the stored visibility must not stay `public` on a Free
//! account. Re-upgrading cancels a pending purge (the stamps clear) but
//! restarts nothing: stopped apps stay stopped until the owner wakes one.

use sqlx::SqlitePool;

use crate::apps::delete::{DeleteConfig, DeleteDeps, DeleteLocks};
use crate::apps::{domains::remove_all_for_app, set_visibility, Visibility};
use crate::auth::EmailSender;
use crate::billing::BillingState;
use crate::cloudflare::{purge_host_cache, CachePurge, CustomHostnames};
use crate::fleet::manager::FleetManager;
use crate::git::{GitConfig, GitSync};
use crate::mcp::{Deploys, RollbackLocks};
use crate::store::Store;

/// Warn this long after the downgrade stop (spec §3: "purged after an
/// email warning"). The sweep mails once, tracked by
/// `app.downgrade_purge_warned_at`.
pub const DOWNGRADE_WARN_AFTER_SECS: i64 = 23 * 24 * 3600;

/// Purge this long after the downgrade stop (spec §3: "kept for 30 days").
pub const DOWNGRADE_PURGE_AFTER_SECS: i64 = 30 * 24 * 3600;

/// Strip Pro extras from every app `owner_id` owns: custom hostnames go
/// (spec §7.7: a downgrade deletes the custom hostname) and public apps go
/// back to unlisted (public is Pro-only, so a stored `public` on a Free
/// account must not survive). Same shared paths as the tools: SaaS delete
/// best-effort, rows dropped, edge caches purged.
pub async fn strip_pro_extras(
    pool: &SqlitePool,
    cf: Option<&CustomHostnames>,
    purge: Option<&CachePurge>,
    apps_domain: &str,
    owner_id: &str,
) -> anyhow::Result<()> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: String,
        slug: String,
        visibility: String,
    }
    let apps: Vec<Row> = sqlx::query_as(
        "SELECT id, slug, visibility FROM app WHERE owner_id = ?1 ORDER BY created_at ASC, id ASC",
    )
    .bind(owner_id)
    .fetch_all(pool)
    .await?;
    for app in &apps {
        remove_all_for_app(pool, cf, purge, &app.id).await;
        if app.visibility == Visibility::Public.as_str() {
            // `unlisted` is never gated, so the plan argument is inert;
            // this is the same function the MCP tool and console call.
            set_visibility(pool, &app.id, Visibility::Unlisted, "free").await?;
            purge_host_cache(purge, &format!("{}.{}", app.slug, apps_domain)).await;
        }
    }
    Ok(())
}

/// What the stopped-app retention pass needs beyond the pool and the
/// clock: every handle `delete_app` takes, owned, so the hourly sweep can
/// move them into its task. Same lock discipline as the MCP `delete_app`
/// path (deploy + rollback + wake across the whole deletion).
pub struct RetentionDeps {
    pub store: Store,
    pub git: GitConfig,
    pub sync: GitSync,
    pub fleets: FleetManager,
    pub deploys: Deploys,
    pub rollback_locks: RollbackLocks,
    pub delete_cfg: DeleteConfig,
}

/// One retention pass over downgrade-stopped apps: warn once at day 23,
/// purge at day 30 through the shared `delete_app` path. Apps whose owner
/// is Pro again self-heal (the stamps clear; `set_pro` normally beats us
/// to it). Returns `(warned, purged)`.
pub async fn run_downgrade_retention(
    pool: &SqlitePool,
    sender: &EmailSender,
    billing: &BillingState,
    deps: &RetentionDeps,
    now_secs: i64,
) -> anyhow::Result<(usize, usize)> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: String,
        slug: String,
        owner_id: String,
        email: String,
        plan: String,
        #[sqlx(rename = "downgraded_stopped_at")]
        stopped_at: Option<i64>,
        #[sqlx(rename = "downgrade_purge_warned_at")]
        warned_at: Option<i64>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT a.id AS id, a.slug AS slug, a.owner_id AS owner_id, u.email AS email, \
                u.plan AS plan, a.downgraded_stopped_at AS downgraded_stopped_at, \
                a.downgrade_purge_warned_at AS downgrade_purge_warned_at \
         FROM app AS a JOIN user AS u ON u.id = a.owner_id \
         WHERE a.downgraded_stopped_at IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut warned = 0;
    let mut purged = 0;
    for row in &rows {
        let Some(stopped_at) = row.stopped_at else {
            continue;
        };
        if row.plan == "pro" {
            // Re-upgraded: cancel the purge, restart nothing.
            sqlx::query(
                "UPDATE app SET downgraded_stopped_at = NULL, \
                 downgrade_purge_warned_at = NULL WHERE id = ?1",
            )
            .bind(&row.id)
            .execute(pool)
            .await?;
            continue;
        }
        let age = now_secs.saturating_sub(stopped_at);
        if age >= DOWNGRADE_PURGE_AFTER_SECS {
            match delete_stopped(pool, deps, &row.id, &row.slug).await {
                Ok(true) => purged += 1,
                Ok(false) => {}
                Err(err) => tracing::warn!(
                    slug = %row.slug,
                    error = %format!("{err:#}"),
                    "downgrade retention purge failed; the next tick retries"
                ),
            }
        } else if age >= DOWNGRADE_WARN_AFTER_SECS && row.warned_at.is_none() {
            let upgrade = billing.upgrade_url(&row.owner_id, now_secs);
            let text = format!(
                "{slug} has been stopped since your account returned to Free and will be \
                 deleted in 7 days (30 days after the downgrade). Resubscribe to keep it: \
                 {upgrade}",
                slug = row.slug,
            );
            match sender
                .send_text(
                    &row.email,
                    &format!("{} will be deleted in 7 days", row.slug),
                    &text,
                )
                .await
            {
                Ok(()) => {
                    sqlx::query("UPDATE app SET downgrade_purge_warned_at = ?1 WHERE id = ?2")
                        .bind(now_secs)
                        .bind(&row.id)
                        .execute(pool)
                        .await?;
                    warned += 1;
                }
                Err(err) => tracing::warn!(
                    slug = %row.slug,
                    error = %format!("{err:#}"),
                    "downgrade retention warning failed; the next tick retries"
                ),
            }
        }
    }
    Ok((warned, purged))
}

/// Delete one downgrade-stopped app through the shared `delete_app` path:
/// fleet stopped, bucket prefixes wiped, rows dropped with the slug
/// quarantined, edge caches purged. Returns `false` when the row was
/// already gone (a concurrent delete won).
async fn delete_stopped(
    pool: &SqlitePool,
    deps: &RetentionDeps,
    app_id: &str,
    slug: &str,
) -> anyhow::Result<bool> {
    let rollback = deps.rollback_locks.lock(app_id).await;
    let deploy = deps.deploys.app_lock(app_id).lock_owned().await;
    let wake_key = deps.fleets.wake_lock(slug).await;
    let wake = wake_key.lock_owned().await;
    let locks = DeleteLocks {
        deploy: &deploy,
        rollback: Some(&rollback),
        wake: Some(&wake),
    };
    let delete_deps = DeleteDeps {
        pool,
        store: &deps.store,
        git: &deps.git,
        sync: &deps.sync,
        fleets: Some(&deps.fleets),
    };
    match crate::apps::delete::delete_app(&delete_deps, &deps.delete_cfg, slug, slug, locks).await {
        Ok(_) => Ok(true),
        Err(crate::apps::delete::DeleteError::NotFound(_)) => Ok(false),
        Err(crate::apps::delete::DeleteError::ConfirmMismatch(_, _)) => {
            // `confirm` is the slug by construction: unreachable.
            anyhow::bail!("downgrade retention confirm mismatch for {slug:?}")
        }
        Err(crate::apps::delete::DeleteError::Internal(err)) => Err(err),
    }
}
