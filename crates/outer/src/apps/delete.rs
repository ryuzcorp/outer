//! Delete an app (spec §7.6): stop the fleet, drop its custom domains at
//! Cloudflare (spec §7.7), wipe its bucket prefixes and local state, drop
//! its rows (quarantining the slug for 30 days), and purge its edge cache
//! (app host plus every custom hostname).
//!
//! One function serves the MCP `delete_app` tool, `delete_account` (which
//! deletes owned apps through this same path) and the operator CLI, so all
//! three delete exactly the same way. The caller holds the app's deploy lock
//! (and, on the MCP path, the rollback and wake locks) across the call: the
//! [`DeleteLocks`] references prove it. The row goes LAST, after every
//! bucket and local step, and every earlier step is idempotent — a failure
//! keeps the row, so a retried `delete_app` finishes the job.
//!
//! Custom domains delete best-effort at Cloudflare (a failure warns and the
//! row drops anyway — a dangling hostname can never serve, since the proxy
//! only routes rows, and re-adding surfaces the conflict); collaborator
//! notices (Pro, M2) don't exist yet: there is nothing to notify.

use std::path::PathBuf;

use sqlx::SqlitePool;

use crate::cloudflare::purge_host_cache;
use crate::fleet::manager::FleetManager;
use crate::git::{manifest, GitConfig, GitSync};
use crate::store::Store;

/// What deletion needs beyond the app row: dirs, the edge host, and the
/// optional Cloudflare zone for the cache purge.
#[derive(Debug, Clone)]
pub struct DeleteConfig {
    /// Platform data dir: worktrees, caches and fleet state live under it.
    pub data_dir: PathBuf,
    /// Cgroup anchor when containment is active, else `None` (dev).
    pub cgroups: Option<PathBuf>,
    /// Apps live at `<slug>.<apps_domain>`; the purge targets that host.
    pub apps_domain: String,
    /// Edge cache purge. `None` skips the purge with a warn (localhost dev).
    pub cache_purge: Option<CachePurge>,
}

/// Cloudflare zone purge credentials (spec §4.5, §7.6). Shared with the
/// deploy pipeline; see [`crate::cloudflare::CachePurge`].
pub use crate::cloudflare::CachePurge;

/// Proof the caller serialized this deletion against deploys, rollbacks and
/// wakes. The MCP path holds all three; the operator CLI (its own process)
/// holds only the deploy lock — cross-process races converge through the
/// idempotent steps and the sleep sweep's orphan-fleet stop.
pub struct DeleteLocks<'g> {
    /// The app's deploy mutex: no pipeline run can start mid-delete.
    pub deploy: &'g tokio::sync::OwnedMutexGuard<()>,
    /// The app's rollback guard (MCP only): no rollback can commit mid-delete.
    pub rollback: Option<&'g tokio::sync::OwnedMutexGuard<()>>,
    /// The slug's wake lock (MCP only): no wake can start mid-delete.
    pub wake: Option<&'g tokio::sync::OwnedMutexGuard<()>>,
}

/// What a finished deletion hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOutput {
    pub slug: String,
    /// Unix milliseconds: the slug is claimable again after this.
    pub quarantined_until: i64,
}

/// Why deletion refused, or what broke mid-way. `NotFound` and
/// `ConfirmMismatch` are the caller's to fix; `Internal` keeps the row, so
/// a retry finishes the job.
#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    #[error("no app named {0:?}")]
    NotFound(String),
    #[error("confirm {1:?} does not match the slug {0:?}: pass the slug exactly to delete it")]
    ConfirmMismatch(String, String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

fn internal(err: impl Into<anyhow::Error>) -> DeleteError {
    DeleteError::Internal(err.into())
}

/// Every app-scoped table keyed by `app_id`, including the ones whose
/// foreign key already cascades: explicit deletes are idempotent, and they
/// keep working if a cascade is ever dropped. `slug_quarantine` is keyed by
/// slug and written by [`crate::apps::delete_row`], not here.
const CHILD_TABLES: &[&str] = &[
    "deploy",
    "app_secret",
    "app_domain",
    "app_collaborator",
    "moderation_text",
    "moderation_check",
    "app_cron",
    "rollback_day",
    "metric_minute",
    "path_hour",
    "visitor_day",
    "app_log",
    "error_issue",
    "agent_action",
];

/// The live handles deletion touches: the row, the bucket, the repo mirror
/// and (on the MCP path) the tracked fleets.
pub struct DeleteDeps<'a> {
    pub pool: &'a SqlitePool,
    pub store: &'a Store,
    pub git: &'a GitConfig,
    pub sync: &'a GitSync,
    pub fleets: Option<&'a FleetManager>,
}

/// Delete `slug` after `confirm` proves intent. See the module docs for the
/// order and the retry contract.
pub async fn delete_app(
    deps: &DeleteDeps<'_>,
    cfg: &DeleteConfig,
    slug: &str,
    confirm: &str,
    _locks: DeleteLocks<'_>,
) -> Result<DeleteOutput, DeleteError> {
    let DeleteDeps {
        pool,
        store,
        git,
        sync,
        fleets,
    } = deps;
    let (pool, store, git, sync, fleets) = (*pool, *store, *git, *sync, *fleets);
    let app = crate::apps::get_by_slug(pool, slug)
        .await
        .map_err(internal)?
        .ok_or_else(|| DeleteError::NotFound(slug.to_string()))?;
    if confirm != slug {
        return Err(DeleteError::ConfirmMismatch(
            slug.to_string(),
            confirm.to_string(),
        ));
    }

    // 1. Stop the fleet first: nothing may serve or write while its state
    // goes away. Missing is a no-op (already asleep or never woken); the
    // cached scoped credentials for the prefix go with it.
    if let Some(fleets) = fleets {
        fleets.stop_tracked(slug).await;
    }

    // 1b. Custom domains (spec §7.7): delete the Cloudflare custom
    // hostnames, drop the rows, and purge each host's edge cache — before
    // the app row goes, while the app id still resolves them. Best effort
    // at Cloudflare (see [`super::domains::remove_all_for_app`]).
    let custom = crate::cloudflare::CustomHostnames::from_purge(cfg.cache_purge.as_ref());
    super::domains::remove_all_for_app(pool, custom.as_ref(), cfg.cache_purge.as_ref(), &app.id)
        .await;
    // 2. Bucket: the repo WAL under `git/{slug}/`, and deploys, D1 data and
    // assets under `fleets/{slug}/`. Trailing slashes keep a sibling slug
    // (`{slug}-2`) out of the prefix. `manifest::purge` also drops the local
    // bare mirror and the sync generation marker.
    store
        .delete_prefix(&format!("fleets/{slug}/"))
        .await
        .map_err(internal)?;
    manifest::purge(store, git, sync, slug)
        .await
        .map_err(internal)?;

    // 3. Local state: the mirror, the worktree and its sidecars, the
    // install cache, the fleet's state dir, the secret sidecars, and the
    // fleet cgroup leaf. All idempotent (missing is done).
    crate::apps::local::remove_local_state(&cfg.data_dir, git, cfg.cgroups.as_deref(), slug)
        .await
        .map_err(internal)?;

    // 4. Rows: every app-scoped child explicitly, then the app row itself —
    // LAST, so a failure above keeps the row and a retry finishes the job.
    // `CHILD_TABLES` is a compile-time list, never agent input: the table
    // name cannot inject. `AssertSqlSafe` records that audit.
    for table in CHILD_TABLES {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE app_id = ?1"
        )))
        .bind(&app.id)
        .execute(pool)
        .await
        .map_err(internal)?;
    }
    crate::apps::delete_row(pool, &app.id)
        .await
        .map_err(internal)?;
    let quarantined_until: Option<i64> =
        sqlx::query_scalar("SELECT until FROM slug_quarantine WHERE slug = ?1")
            .bind(slug)
            .fetch_optional(pool)
            .await
            .map_err(internal)?;
    let Some(quarantined_until) = quarantined_until else {
        // The row vanished between lookup and delete: a concurrent delete
        // won. Report it as gone, not as deleted.
        return Err(DeleteError::NotFound(slug.to_string()));
    };

    // 5. Edge cache: a cached page must not outlive the deletion. Best
    // effort — the row is already gone, so a failure warns instead of
    // failing; the entry expires on its own.
    purge_host_cache(
        cfg.cache_purge.as_ref(),
        &format!("{slug}.{}", cfg.apps_domain),
    )
    .await;

    Ok(DeleteOutput {
        slug: slug.to_string(),
        quarantined_until,
    })
}
