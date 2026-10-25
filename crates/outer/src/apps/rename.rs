//! Rename an app (M1): move its slug identity from `old` to `new`.
//!
//! The slug is an identity in five places, and the rename moves all of them:
//! the `app` row (the only slug-keyed row that moves — every child table is
//! keyed by `app_id` and follows it untouched), the `git/{slug}/` and
//! `fleets/{slug}/` bucket prefixes (repo WAL, deploy pointers, D1/DO cell
//! state, leases and the fleet secret — all per-fleet namespaces, so the
//! copy carries the data with it), the local mirror/worktree/cache/fleet
//! dirs and sidecars, and the edge cache for both hostnames. The old slug is
//! quarantined for 30 days with `renamed_to` set, so the proxy answers 308
//! to the new host while the quarantine holds.
//!
//! Order and crash-consistency: the per-app deploy + rollback locks and both
//! slugs' wake locks are held across the whole rename (the [`RenameLocks`]
//! proof), so no deploy, commit, rollback or wake interleaves it. Inside:
//! stop the fleet first (D1/DO state is at rest in the bucket); copy both
//! bucket prefixes old→new (idempotent overwrite through a temp dir, so a
//! retry after a crash mid-copy converges); delete the old prefixes (a crash
//! after this but before the commit retries the copy as a no-op — `list`
//! finds nothing left — and commits); move the local dirs (each `rename` is
//! atomic; a retry skips missing sources, which means either never-existed
//! or already-moved); then ONE transaction re-checks the new slug, flips the
//! row, and quarantines the old slug with `renamed_to`. The transaction is
//! the point of no return: before it, a failure leaves the old identity
//! fully intact (retry converges); after it, only best-effort edge purges
//! remain. The redeploy of the current sha (when the app was awake) runs
//! after, through the caller's deploy runner — a failed redeploy leaves the
//! rename committed with the previous version servable from the copied
//! pointer, exactly like a failed commit.
//!
//! One process-wide note: the copy runs under no global lock (unlike
//! `create`'s `CREATE_LOCK`), because the transaction re-checks the new slug
//! and the `UPDATE`'s UNIQUE violation maps to [`RenameError::Taken`] — a
//! racing `create_app` for the new slug converges to a clean refusal.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use sqlx::SqlitePool;

use super::{now_ms, App, CreateError, QUARANTINE_MS};
use crate::fleet::manager::FleetManager;
use crate::git::{GitConfig, GitSync};
use crate::store::Store;

/// Proof the caller serialized this rename against deploys, rollbacks and
/// wakes. Wake locks are sorted by slug at acquisition (see the MCP tool),
/// so two renames can never ABBA.
pub struct RenameLocks<'g> {
    /// The app's deploy mutex: no pipeline run can start mid-rename.
    pub deploy: &'g tokio::sync::OwnedMutexGuard<()>,
    /// The app's rollback guard: no rollback can commit mid-rename.
    pub rollback: &'g tokio::sync::OwnedMutexGuard<()>,
    /// The old slug's wake lock: no wake or sleep can start mid-rename.
    pub wake_old: &'g tokio::sync::OwnedMutexGuard<()>,
    /// The new slug's wake lock: nothing routes there until the commit.
    pub wake_new: &'g tokio::sync::OwnedMutexGuard<()>,
}

/// The live handles a rename touches: the row, the bucket, the repo mirror
/// and (on the MCP path) the tracked fleet.
pub struct RenameDeps<'a> {
    pub pool: &'a SqlitePool,
    pub store: &'a Store,
    pub git: &'a GitConfig,
    pub sync: &'a GitSync,
    pub fleets: Option<&'a FleetManager>,
    /// Platform data dir: `work/`, `cache/`, `fleets/`, `secret-sidecars/`.
    pub data_dir: &'a Path,
    /// Cgroup anchor when containment is active, else `None` (dev).
    pub cgroups: Option<&'a Path>,
    /// Apps live at `<slug>.<apps_domain>`; the purge targets both hosts.
    pub apps_domain: &'a str,
    /// Edge cache purge. `None` skips the purge with a warn (localhost dev).
    pub cache_purge: Option<&'a crate::apps::delete::CachePurge>,
}

/// What a finished rename hands back.
#[derive(Debug, Clone)]
pub struct RenameOutput {
    /// The app row after the flip (`slug` is the new slug).
    pub app: App,
    /// The quarantined old slug.
    pub old_slug: String,
    /// Unix milliseconds: the old slug is claimable again after this.
    pub quarantined_until: i64,
    /// The running app's last successful sha, to redeploy under the new
    /// name. `None` when the app was asleep or never deployed.
    pub redeploy_sha: Option<String>,
}

/// Why a rename refused, or what broke mid-way. `NotFound`, `SameSlug`,
/// `InvalidSlug` and `Taken` are the caller's to fix (the rename never
/// happened — refund any claimed quota); `Internal` keeps the old identity
/// wherever the failure landed before the commit, and the committed rename
/// stands wherever it landed after.
#[derive(Debug, thiserror::Error)]
pub enum RenameError {
    #[error("no app named {0:?}")]
    NotFound(String),
    #[error("the app is already named {0:?}")]
    SameSlug(String),
    #[error(transparent)]
    InvalidSlug(#[from] super::SlugError),
    #[error("slug is taken or quarantined; choose another slug")]
    Taken,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

fn internal(err: impl Into<anyhow::Error>) -> RenameError {
    RenameError::Internal(err.into())
}

/// The new slug both prefixes move between. Trailing slashes keep a sibling
/// slug (`{slug}-2`) out of the copy and the delete.
fn old_prefix(slug: &str) -> String {
    format!("fleets/{slug}/")
}

fn old_git_prefix(slug: &str) -> String {
    format!("git/{slug}/")
}

/// Copy every object under `from_prefix` to `to_prefix`, preserving the
/// suffix. Each object streams through a temp file, so a large bundle never
/// sits whole in memory; `put` overwrites, so a retry after a crash
/// mid-copy converges on identical bytes. Returns the objects copied.
async fn copy_prefix(store: &Store, from_prefix: &str, to_prefix: &str) -> anyhow::Result<u64> {
    let keys = store
        .list(from_prefix)
        .await
        .with_context(|| format!("listing {from_prefix} for the rename copy"))?;
    if keys.is_empty() {
        return Ok(0);
    }
    let tmp = tempfile::tempdir().context("staging the rename copy")?;
    let mut copied = 0u64;
    for key in &keys {
        let Some(suffix) = key.strip_prefix(from_prefix) else {
            continue;
        };
        let stage = tmp.path().join(format!("object-{copied}"));
        if !store
            .get_to_file(key, &stage)
            .await
            .with_context(|| format!("reading {key} for the rename copy"))?
        {
            // Listed but already gone: only a lock-holder writes these
            // prefixes, and every lock-holder is serialized behind us —
            // treat a vanishing key as done, not as a failure.
            continue;
        }
        let to = format!("{to_prefix}{suffix}");
        store
            .put_file(&to, &stage)
            .await
            .with_context(|| format!("writing {to} for the rename copy"))?;
        copied += 1;
    }
    Ok(copied)
}

/// `rename(2)` a local path, treating a missing source as done (never
/// existed, or already moved by a retry after a crash) and an existing
/// destination as done (a completed move — nothing else writes these paths
/// while the locks are held).
async fn move_path(src: &Path, dst: &Path) -> anyhow::Result<()> {
    match tokio::fs::symlink_metadata(src).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(anyhow::anyhow!("cannot stat {}: {e:#}", src.display()));
        }
    }
    if tokio::fs::symlink_metadata(dst).await.is_ok() {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    tokio::fs::rename(src, dst)
        .await
        .with_context(|| format!("moving {} to {}", src.display(), dst.display()))
}

/// Rename `old_slug` to `new_slug`. See the module docs for the order and
/// the crash-consistency argument.
pub async fn rename_app(
    deps: &RenameDeps<'_>,
    old_slug: &str,
    new_slug: &str,
    _locks: RenameLocks<'_>,
) -> Result<RenameOutput, RenameError> {
    let RenameDeps {
        pool,
        store,
        git,
        sync,
        fleets,
        data_dir,
        cgroups,
        apps_domain,
        cache_purge,
    } = deps;
    let (pool, store, git, sync, fleets) = (*pool, *store, *git, *sync, *fleets);
    let (data_dir, cgroups, apps_domain, cache_purge) =
        (*data_dir, *cgroups, *apps_domain, *cache_purge);
    if old_slug == new_slug {
        return Err(RenameError::SameSlug(old_slug.to_string()));
    }
    let app = super::get_by_slug(pool, old_slug)
        .await
        .map_err(internal)?
        .ok_or_else(|| RenameError::NotFound(old_slug.to_string()))?;
    // Fast pre-check with agent-facing messages; the transaction re-checks
    // authoritatively (a racing create converges on Taken there).
    super::check_slug_claimable(pool, new_slug)
        .await
        .map_err(|err| match err {
            CreateError::InvalidSlug(e) => RenameError::InvalidSlug(e),
            CreateError::Taken => RenameError::Taken,
            CreateError::Exhausted(_) | CreateError::Quota { .. } => {
                RenameError::Internal(anyhow::anyhow!("claimable check refused: {err}"))
            }
            CreateError::Internal(e) => RenameError::Internal(e),
        })?;

    // 1. Stop the fleet first: nothing may serve or write while the bucket
    // prefixes and local dirs move. The cached scoped credentials for the
    // old prefix go with it. Missing is a no-op.
    if let Some(fleets) = fleets {
        fleets.stop_tracked(old_slug).await;
    }
    let was_running = app.asleep_since.is_none();
    let redeploy_sha = if was_running {
        app.last_deploy_sha.clone()
    } else {
        None
    };

    // 2. Bucket: the repo WAL under `git/{old}/`, and deploys, D1/DO cell
    // state, leases and the fleet secret under `fleets/{old}/`. The cell and
    // database identities live in the app's config (unchanged by a rename),
    // so the copied state stays valid under the new prefix; the copied node
    // leases name stopped nodes, and the new fleet claims its cells past
    // them (same failover as a dead owner node — the old fleet is stopped,
    // so there is no split brain).
    let from_git = old_git_prefix(old_slug);
    let to_git = old_git_prefix(new_slug);
    let from_fleets = old_prefix(old_slug);
    let to_fleets = old_prefix(new_slug);
    copy_prefix(store, &from_git, &to_git)
        .await
        .map_err(internal)?;
    copy_prefix(store, &from_fleets, &to_fleets)
        .await
        .map_err(internal)?;

    // 3. The old prefixes go BEFORE the row flip, like delete wipes before
    // its row delete: a crash after this but before the commit retries the
    // copy as a no-op (`list` finds nothing left) and commits the already
    // complete new prefixes. A crash before this retries the copy over
    // partial bytes. Either way the commit only ever flips to complete
    // prefixes, and an orphaned old prefix can never outlive the commit —
    // so a later app reusing the old slug after quarantine expiry can never
    // adopt this app's deploy pointer or D1 state.
    store.delete_prefix(&from_git).await.map_err(internal)?;
    store.delete_prefix(&from_fleets).await.map_err(internal)?;

    // 4. Local state: the bare mirror, the worktree and its sidecars, the
    // install cache, the fleet's state dir, and the fleet cgroup leaf (empty:
    // the fleet is stopped — the next wake recreates it under the new name).
    // The manifest generation marker follows the repo; the old slug's lock
    // and wake map entries leak one idle mutex each (bounded by renames).
    move_path(
        &crate::git::manifest::mirror_path(git, old_slug),
        &crate::git::manifest::mirror_path(git, new_slug),
    )
    .await
    .map_err(internal)?;
    let work = PathBuf::from(data_dir).join("work");
    move_path(&work.join(old_slug), &work.join(new_slug))
        .await
        .map_err(internal)?;
    for sidecar in ["index", "install-hash"] {
        move_path(
            &work.join(format!("{old_slug}.{sidecar}")),
            &work.join(format!("{new_slug}.{sidecar}")),
        )
        .await
        .map_err(internal)?;
    }
    move_path(
        &PathBuf::from(data_dir).join("cache").join(old_slug),
        &PathBuf::from(data_dir).join("cache").join(new_slug),
    )
    .await
    .map_err(internal)?;
    move_path(
        &PathBuf::from(data_dir).join("fleets").join(old_slug),
        &PathBuf::from(data_dir).join("fleets").join(new_slug),
    )
    .await
    .map_err(internal)?;
    let sidecars = PathBuf::from(data_dir).join("secret-sidecars");
    for ext in ["orig", "target"] {
        move_path(
            &sidecars.join(format!("{old_slug}.{ext}")),
            &sidecars.join(format!("{new_slug}.{ext}")),
        )
        .await
        .map_err(internal)?;
    }
    if let Some(anchor) = cgroups {
        crate::fleet::cgroups::remove_fleet_cgroup(
            &anchor.join(crate::fleet::cgroups::fleet_dir_name(old_slug)),
        );
    }
    if let Some(seq) = sync.applied_seq(old_slug).await {
        sync.set_applied_seq(new_slug, seq).await;
    }
    sync.forget(old_slug).await;

    // 5. The point of no return, in one transaction: re-check the new slug
    // (a racing create took it, or its quarantine has not expired), flip the
    // row only if it still names the old slug, and quarantine the old slug
    // with `renamed_to` pointing at the new one. A UNIQUE failure on the
    // flip is a lost race, not corruption: map it to Taken.
    let mut tx = pool.begin().await.map_err(internal)?;
    super::check_slug_claimable_conn(&mut tx, new_slug)
        .await
        .map_err(|err| match err {
            CreateError::InvalidSlug(e) => RenameError::InvalidSlug(e),
            CreateError::Taken => RenameError::Taken,
            CreateError::Exhausted(_) | CreateError::Quota { .. } => {
                RenameError::Internal(anyhow::anyhow!("claimable check refused: {err}"))
            }
            CreateError::Internal(e) => RenameError::Internal(e),
        })?;
    let flipped = match sqlx::query("UPDATE app SET slug = ?1 WHERE id = ?2 AND slug = ?3")
        .bind(new_slug)
        .bind(&app.id)
        .bind(old_slug)
        .execute(&mut *tx)
        .await
    {
        Ok(flipped) => flipped,
        // A racing create took the new slug first: a lost race, not
        // corruption.
        Err(e) if is_unique_violation(&e) => return Err(RenameError::Taken),
        Err(e) => return Err(internal(e)),
    };
    if flipped.rows_affected() != 1 {
        return Err(internal(anyhow::anyhow!(
            "app {} changed under the rename locks",
            app.id
        )));
    }
    // An expired leftover row for the old slug (the daily prune has not run)
    // must not fail the insert below.
    sqlx::query("DELETE FROM slug_quarantine WHERE slug = ?1 AND until <= ?2")
        .bind(old_slug)
        .bind(now_ms())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    let quarantined_until = now_ms() + QUARANTINE_MS;
    if let Err(e) =
        sqlx::query("INSERT INTO slug_quarantine (slug, until, renamed_to) VALUES (?1, ?2, ?3)")
            .bind(old_slug)
            .bind(quarantined_until)
            .bind(new_slug)
            .execute(&mut *tx)
            .await
    {
        if is_unique_violation(&e) {
            return Err(RenameError::Taken);
        }
        return Err(internal(e));
    }
    tx.commit().await.map_err(internal)?;
    let until: Option<i64> =
        sqlx::query_scalar("SELECT until FROM slug_quarantine WHERE slug = ?1")
            .bind(old_slug)
            .fetch_optional(pool)
            .await
            .map_err(internal)?;
    let Some(quarantined_until) = until else {
        // The row vanished between commit and re-read: practically
        // impossible (quarantine rows only expire via the daily prune), but
        // report it as gone rather than as renamed.
        return Err(RenameError::NotFound(old_slug.to_string()));
    };

    // 6. Edge cache: neither hostname may serve the other's pages. Best
    // effort — the row is already flipped, so a failure warns instead of
    // failing; the entries expire on their own.
    crate::cloudflare::purge_host_cache(cache_purge, &format!("{old_slug}.{apps_domain}")).await;
    crate::cloudflare::purge_host_cache(cache_purge, &format!("{new_slug}.{apps_domain}")).await;

    let app = super::get_by_id(pool, &app.id)
        .await
        .map_err(internal)?
        .ok_or_else(|| RenameError::NotFound(old_slug.to_string()))?;
    Ok(RenameOutput {
        app,
        old_slug: old_slug.to_string(),
        quarantined_until,
        redeploy_sha,
    })
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.message().contains("UNIQUE constraint failed"))
}
