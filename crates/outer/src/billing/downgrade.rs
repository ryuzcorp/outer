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
//! account. The visitor gate (`access_mode`) is deliberately untouched:
//! removing it would expose a private app, so a gated app stays gated and
//! the owner can only turn it off, never set a new one. Re-upgrading
//! cancels a pending purge (the stamps clear) and restarts the apps the
//! downgrade stopped; apps an operator stopped (a ban) stay stopped.

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

/// A late warning pushes the purge out: the purge needs the warning to be
/// this old, so a warning mailed after day 23 still buys its full week.
pub const DOWNGRADE_WARN_CUSHION_SECS: i64 = DOWNGRADE_PURGE_AFTER_SECS - DOWNGRADE_WARN_AFTER_SECS;

/// At most this many purges per pass per sweep. A pass that hits the cap
/// logs at error level and leaves the rest for the next pass, so a
/// mass deletion is noticed instead of silent.
pub const MAX_PURGES_PER_PASS: usize = 25;

/// What `delete_retained` must re-verify under the locks before it deletes:
/// the pass's row list is stale by the time the locks are held, so each
/// variant re-reads the row by id and re-evaluates its own predicate.
#[derive(Debug, Clone, Copy)]
pub enum Retention {
    /// Inactivity purge: `expired_at` still set and past grace, the owner
    /// still Free, no owner or app activity since the stamp, same slug.
    Inactive { now_ms: i64, grace_ms: i64 },
    /// Downgrade purge: the stop stamp still set, the owner still Free, the
    /// warning old enough that the purge date holds, same slug.
    Downgraded { now_secs: i64 },
}

/// Strip Pro extras from every app `owner_id` owns: custom hostnames go
/// (spec §7.7: a downgrade deletes the custom hostname) and public apps go
/// back to unlisted (public is Pro-only, so a stored `public` on a Free
/// account must not survive). Same shared paths as the tools: SaaS delete
/// best-effort, rows dropped, edge caches purged. Every app host is purged
/// too: a downgrade stops all but the two oldest, and the edge may still
/// hold the stopped apps' cached content even though the stopped page is
/// `no-store`.
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
        }
        purge_host_cache(purge, &format!("{}.{}", app.slug, apps_domain)).await;
    }
    Ok(())
}

/// What the stopped-app retention pass needs beyond the pool and the
/// clock: every handle `delete_app` takes, owned, so the hourly sweep can
/// move them into its task. Same lock discipline as the MCP `delete_app`
/// path (deploy + rollback + wake across the whole deletion).
#[derive(Clone)]
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
/// purge at day 30 through the shared `delete_app` path — but only once the
/// warning is a week old, so a late warning pushes the purge out instead of
/// purging early. Apps whose owner
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
        // The warning comes first, even late: a missing warning is mailed
        // now (the purge below waits out its week), so a failed send never
        // settles into purge-without-warning.
        let mut warned_at = row.warned_at;
        if age >= DOWNGRADE_WARN_AFTER_SECS && warned_at.is_none() {
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
                    warned_at = Some(now_secs);
                    warned += 1;
                }
                Err(err) => tracing::warn!(
                    slug = %row.slug,
                    error = %format!("{err:#}"),
                    "downgrade retention warning failed; the next tick retries"
                ),
            }
        }
        if age >= DOWNGRADE_PURGE_AFTER_SECS {
            if purged >= MAX_PURGES_PER_PASS {
                tracing::error!(
                    purged,
                    "downgrade retention purge cap hit; the rest wait for the next pass"
                );
                break;
            }
            match warned_at {
                Some(sent_at)
                    if now_secs.saturating_sub(sent_at) >= DOWNGRADE_WARN_CUSHION_SECS =>
                {
                    match delete_retained(
                        pool,
                        deps,
                        &row.id,
                        &row.slug,
                        Retention::Downgraded { now_secs },
                    )
                    .await
                    {
                        Ok(true) => purged += 1,
                        Ok(false) => {}
                        Err(err) => tracing::warn!(
                            slug = %row.slug,
                            error = %format!("{err:#}"),
                            "downgrade retention purge failed; the next tick retries"
                        ),
                    }
                }
                _ => tracing::warn!(
                    slug = %row.slug,
                    "downgrade retention purge skipped: no delivered warning; the next pass retries"
                ),
            }
        }
    }
    Ok((warned, purged))
}

/// Delete one retention-doomed app through the shared `delete_app` path:
/// fleet stopped, bucket prefixes wiped, rows dropped (the slug is claimable
/// again immediately), edge caches purged. Shared by the downgrade-retention
/// pass and the inactivity sweep. After taking the locks the row is re-read
/// by id and the caller's `retention` predicate is re-evaluated, so a user
/// who returned or resubscribed mid-pass, or a rename, is never deleted on
/// a stale snapshot. Returns `false` when the row was already gone or the
/// predicate no longer holds (a concurrent delete or a revival won).
pub(crate) async fn delete_retained(
    pool: &SqlitePool,
    deps: &RetentionDeps,
    app_id: &str,
    slug: &str,
    retention: Retention,
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
    // Re-read under the locks: the pass's row list predates them, so the
    // owner may have resubscribed, returned, or renamed since.
    #[derive(sqlx::FromRow)]
    struct Fresh {
        slug: String,
        plan: String,
        last_activity_at: i64,
        expired_at: Option<i64>,
        downgraded_stopped_at: Option<i64>,
        downgrade_purge_warned_at: Option<i64>,
        #[sqlx(rename = "lastActiveAt")]
        owner_last_active: i64,
    }
    let fresh: Option<Fresh> = sqlx::query_as(
        "SELECT a.slug AS slug, u.plan AS plan, \
                a.last_activity_at AS last_activity_at, a.expired_at AS expired_at, \
                a.downgraded_stopped_at AS downgraded_stopped_at, \
                a.downgrade_purge_warned_at AS downgrade_purge_warned_at, \
                u.lastActiveAt AS lastActiveAt \
         FROM app AS a JOIN user AS u ON u.id = a.owner_id WHERE a.id = ?1",
    )
    .bind(app_id)
    .fetch_optional(pool)
    .await?;
    let Some(fresh) = fresh else {
        return Ok(false);
    };
    let holds = match retention {
        Retention::Inactive { now_ms, grace_ms } => match fresh.expired_at {
            Some(stamped) => {
                fresh.slug == slug
                    && fresh.plan != "pro"
                    && fresh.downgraded_stopped_at.is_none()
                    && now_ms >= stamped.saturating_add(grace_ms)
                    && fresh.last_activity_at.max(fresh.owner_last_active) <= stamped
            }
            None => false,
        },
        Retention::Downgraded { now_secs } => {
            match (fresh.downgraded_stopped_at, fresh.downgrade_purge_warned_at) {
                (Some(_), Some(warned_at)) => {
                    fresh.slug == slug
                        && fresh.plan != "pro"
                        && now_secs.saturating_sub(warned_at) >= DOWNGRADE_WARN_CUSHION_SECS
                }
                _ => false,
            }
        }
    };
    if !holds {
        return Ok(false);
    }
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
            anyhow::bail!("retention confirm mismatch for {slug:?}")
        }
        Err(crate::apps::delete::DeleteError::Internal(err)) => Err(err),
    }
}
