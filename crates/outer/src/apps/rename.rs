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
//! proof), so no deploy, commit, rollback, wake or concurrent rename
//! interleaves it. Inside:
//!
//! 0. Fast pre-check of the target (live app or live quarantine refuses),
//!    then stop the fleet first (D1/DO state is at rest in the bucket).
//! 1. Pre-copy guard under the create lock: re-check the target has no live
//!    app and no live quarantine, then `INSERT OR IGNORE` the target's
//!    cleanup rows. A racing `create_app` for the target converges here:
//!    its check (under the same lock) sees our rows and refuses, and if its
//!    check ran first we see its row/quarantine and refuse — the claim is
//!    recorded under the lock before either side touches the bucket.
//!    Pending cleanup rows are deliberately ignored by this check: they are
//!    either our own abandoned attempt's (adopted: the copy wipes the target
//!    first) or another rename's in-flight target (serialized on the new
//!    slug's wake lock, which we hold).
//! 2. Copy both bucket prefixes old→new. The copy wipes the target prefix
//!    first: the target was unclaimed and must be empty, so anything there
//!    is an abandoned partial copy and wiping makes a retry converge on
//!    identical bytes instead of mixing stale with fresh. Overwrite makes
//!    each object converge; a retry after a crash mid-copy rewrites the
//!    same bytes.
//! 3. Move the local dirs (each `rename(2)` atomic). A fresh attempt wipes
//!    the whole target first, so abandoned orphans never read as
//!    already-moved; a retry after the moves started leaves missing sources
//!    (already-moved) alone, so moved content survives.
//! 4. ONE transaction: drop our target cleanup rows (they are live data
//!    now, and must never be drained), re-check the target authoritatively
//!    — cleanup rows included, so a racing create or quarantine wins here
//!    as [`RenameError::Taken`] — flip the row only if it still names the
//!    old slug, quarantine the old slug with `renamed_to`, and insert the
//!    old prefixes' cleanup rows. The transaction is the point of no return:
//!    before it, a failure leaves the old identity fully intact (old
//!    prefixes untouched, retry with any target converges); after it, the
//!    new identity is durable and only deletions remain.
//! 5. Post-commit: delete the old prefixes, delete their cleanup rows, purge
//!    both hosts' edge caches. Every step is warn-and-continue: after the
//!    commit the rename is done, and [`drain_prefix_cleanup`] (startup and
//!    the sleep-sweep tick) finishes whatever is left. Nothing after the
//!    commit can fail the rename, so callers refund the daily-change quota
//!    on any error — an error always means "never happened".
//!
//! Why the target rows, not wipe-first alone: wiping the target first is
//! necessary (idempotent retries) but not sufficient — an abandoned target
//! copy (A→B crashed, retried as A→C) would sit under an unclaimed slug
//! until someone claims it, and `create_app` never wipes: it would inherit
//! the stale deploy pointer and D1 state. Recording the target rows before
//! copying makes the target unclaimable from that moment on, and the flip
//! transaction atomically trades the target rows (live data, never drained)
//! for the old rows (garbage, drained next pass). A slug with a pending
//! cleanup row is never claimable — not by create, not by rename, not even
//! past its quarantine — and the drain deletes the data before the slug
//! opens up again, so leftovers are never inherited.
//!
//! The drain takes the slug's wake lock before deleting: the rename holds
//! the new slug's wake lock across the copy and the commit, so the drain
//! cannot land a delete between the copy's last write and the flip's row
//! removal. After the lock it re-reads the rows — a committed rename's
//! target rows are already gone, a failed one's are still there — and
//! deletes only what is still pending. As a backstop it never deletes a
//! prefix whose slug names a live app (impossible by the locking above):
//! it drops the rows and warns.
//!
//! One process-wide note: the copy runs under no global lock (unlike
//! `create`'s `CREATE_LOCK`), because the pre-copy guard serializes the
//! claim and the transaction re-checks it — a racing `create_app` for the
//! new slug converges to a clean refusal either way.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use sqlx::SqlitePool;

use super::{
    check_slug_preclaim, cleanup_prefixes, lock_for_rename_target, now_ms, App, CreateError,
    QUARANTINE_MS,
};
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

/// Why a rename refused, or what broke mid-way. Every variant is
/// pre-commit: the rename never happened, and the caller refunds any
/// claimed quota. Post-commit steps never fail (see the module docs).
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

/// Deterministic crash points for tests: the rename fails (or, for
/// [`RenameFailStep::SkipCleanup`], skips its post-commit work) exactly at
/// the named boundary. Prod always runs with `fail_at: None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameFailStep {
    /// After stopping the fleet, before recording the target rows.
    AfterStop,
    /// After copying `git/{old}/`, before `fleets/{old}/`.
    AfterGitCopy,
    /// After copying both prefixes, before moving local dirs.
    AfterFleetsCopy,
    /// After moving local dirs, before the flip transaction.
    BeforeCommit,
    /// Simulates a crash right after the commit: the post-commit deletes
    /// are skipped but the rename still answers `Ok`, which is exactly the
    /// state a restart would find (committed row, pending cleanup rows,
    /// old prefixes still present).
    SkipCleanup,
}

/// Fault injection for deterministic crash tests. `Default` is no faults.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenameHooks {
    pub fail_at: Option<RenameFailStep>,
}

fn fail_if(hooks: RenameHooks, at: RenameFailStep) -> Result<(), RenameError> {
    if hooks.fail_at == Some(at) {
        return Err(internal(anyhow::anyhow!("injected fault at {at:?}")));
    }
    Ok(())
}

/// Copy every object under `from_prefix` to `to_prefix`, preserving the
/// suffix. The target prefix is wiped first: the target slug was unclaimed
/// and must be empty, so anything there is an abandoned partial copy from
/// a crashed attempt, and wiping makes the retry converge on identical
/// bytes. Each object then streams through a temp file, so a large bundle
/// never sits whole in memory; `put` overwrites, so a retry after a crash
/// mid-copy converges on identical bytes. Returns the objects copied.
async fn copy_prefix(store: &Store, from_prefix: &str, to_prefix: &str) -> anyhow::Result<u64> {
    let keys = store
        .list(from_prefix)
        .await
        .with_context(|| format!("listing {from_prefix} for the rename copy"))?;
    if keys.is_empty() {
        return Ok(0);
    }
    // Wipe-first (see above): retrying a crashed copy restarts from empty
    // instead of mixing stale bytes with fresh ones.
    store
        .delete_prefix(to_prefix)
        .await
        .with_context(|| format!("clearing {to_prefix} for the rename copy"))?;
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

/// Move one local path for the rename, wiping the target first — but only
/// when the source still exists. A crashed delete can leave orphans behind
/// under the now-unclaimed target, so the wipe keeps an existing
/// destination from reading as already-moved. When the source is gone the
/// move already happened (a retry after a pre-commit crash): the target is
/// left untouched, so the retry never wipes moved content and then no-ops.
/// Each `rename(2)` is atomic; a missing source means never-existed or
/// already-moved. Returns whether the move ran.
async fn move_local_path(src: &Path, dst: &Path) -> anyhow::Result<bool> {
    match tokio::fs::symlink_metadata(src).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(anyhow::anyhow!("cannot stat {}: {e:#}", src.display()));
        }
    }
    remove_local_path(dst).await?;
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    tokio::fs::rename(src, dst)
        .await
        .with_context(|| format!("moving {} to {}", src.display(), dst.display()))?;
    Ok(true)
}

/// Remove a local path that may be a dir, a file, or already missing.
async fn remove_local_path(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.file_type().is_dir() => {
            tokio::fs::remove_dir_all(path)
                .await
                .with_context(|| format!("removing local dir {}", path.display()))?;
        }
        Ok(_) => {
            tokio::fs::remove_file(path)
                .await
                .with_context(|| format!("removing local file {}", path.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::anyhow!("cannot stat {}: {e:#}", path.display()));
        }
    }
    Ok(())
}

/// The slug a cleanup row belongs to: `fleets/{slug}/` or `git/{slug}/`.
/// Anything else is a corrupt row the drain only deletes by raw prefix when
/// the prefix is still a safe scope (non-empty, trailing `/`); otherwise the
/// row is kept and the drain warns.
fn cleanup_slug(prefix: &str) -> Option<&str> {
    let rest = prefix
        .strip_prefix("fleets/")
        .or_else(|| prefix.strip_prefix("git/"))?;
    rest.strip_suffix("/")
}

/// Delete every prefix with a pending cleanup row and remove the rows.
/// Idempotent: a prefix already gone is a no-op delete, and re-running with
/// no rows is one indexed `SELECT`. Run at startup and on the maintenance
/// loop; a crash between the commit and the post-commit deletes converges
/// here. Returns the rows removed.
///
/// The slug's wake lock is held across its prefixes' deletes (when a fleet
/// manager is available): the rename holds the new slug's wake lock across
/// the copy and the commit, so the drain can never land between the copy's
/// last write and the flip's row removal. Rows are re-read under the lock —
/// a rename that committed while we waited already removed its target rows.
pub async fn drain_prefix_cleanup(
    pool: &SqlitePool,
    store: &Store,
    fleets: Option<&FleetManager>,
) -> anyhow::Result<u64> {
    let prefixes: Vec<String> =
        sqlx::query_scalar("SELECT prefix FROM prefix_cleanup ORDER BY prefix")
            .fetch_all(pool)
            .await?;
    if prefixes.is_empty() {
        return Ok(0);
    }
    let mut by_slug: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut raw: Vec<String> = Vec::new();
    for prefix in prefixes {
        match cleanup_slug(&prefix) {
            Some(slug) => by_slug.entry(slug.to_string()).or_default().push(prefix),
            None => raw.push(prefix),
        }
    }
    let mut removed = 0u64;
    for prefix in raw {
        // Unparseable rows are never deleted blindly: without a trailing
        // `/` the raw prefix could name a wider scope than intended. Keep
        // the row and shout — an operator owns the cleanup.
        if prefix.is_empty() || !prefix.ends_with('/') {
            tracing::warn!(
                prefix = %prefix,
                "prefix_cleanup row has an unsafe prefix; keeping the row, deleting nothing"
            );
            continue;
        }
        store
            .delete_prefix(&prefix)
            .await
            .with_context(|| format!("draining unparseable cleanup prefix {prefix}"))?;
        sqlx::query("DELETE FROM prefix_cleanup WHERE prefix = ?1")
            .bind(&prefix)
            .execute(pool)
            .await?;
        removed += 1;
    }
    for (slug, _) in by_slug {
        let _wake = match fleets {
            Some(fleets) => Some(fleets.wake_lock(&slug).await.lock_owned().await),
            None => None,
        };
        let [fleets_prefix, git_prefix] = cleanup_prefixes(&slug);
        let pending: Vec<String> = sqlx::query_scalar(
            "SELECT prefix FROM prefix_cleanup WHERE prefix IN (?1, ?2) ORDER BY prefix",
        )
        .bind(&fleets_prefix)
        .bind(&git_prefix)
        .fetch_all(pool)
        .await?;
        if pending.is_empty() {
            continue;
        }
        // A live app owns this slug: its prefixes are live data, never
        // garbage. Impossible under the rename/create locking (neither
        // records a claim the other can miss), so drop just the rows and
        // warn loudly instead of deleting a running app's state.
        if super::get_by_slug(pool, &slug).await?.is_some() {
            tracing::warn!(
                slug,
                "prefix_cleanup rows name a live app; dropping the rows, keeping the data"
            );
            sqlx::query("DELETE FROM prefix_cleanup WHERE prefix IN (?1, ?2)")
                .bind(&fleets_prefix)
                .bind(&git_prefix)
                .execute(pool)
                .await?;
            removed += pending.len() as u64;
            continue;
        }
        for prefix in &pending {
            store
                .delete_prefix(prefix)
                .await
                .with_context(|| format!("draining cleanup prefix {prefix}"))?;
        }
        sqlx::query("DELETE FROM prefix_cleanup WHERE prefix IN (?1, ?2)")
            .bind(&fleets_prefix)
            .bind(&git_prefix)
            .execute(pool)
            .await?;
        removed += pending.len() as u64;
    }
    Ok(removed)
}

/// Rename `old_slug` to `new_slug`. See the module docs for the order and
/// the crash-consistency argument.
pub async fn rename_app(
    deps: &RenameDeps<'_>,
    old_slug: &str,
    new_slug: &str,
    locks: RenameLocks<'_>,
) -> Result<RenameOutput, RenameError> {
    rename_app_with_hooks(deps, old_slug, new_slug, locks, RenameHooks::default()).await
}

/// [`rename_app`] with deterministic crash points for tests.
pub async fn rename_app_with_hooks(
    deps: &RenameDeps<'_>,
    old_slug: &str,
    new_slug: &str,
    _locks: RenameLocks<'_>,
    hooks: RenameHooks,
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
    // Fast pre-check with agent-facing messages; the pre-copy guard and the
    // transaction re-check authoritatively (a racing create converges on
    // Taken there). Cleanup rows are ignored here — they may be our own
    // abandoned attempt's, adopted below.
    check_slug_preclaim(pool, new_slug)
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
    fail_if(hooks, RenameFailStep::AfterStop)?;

    // 2. Claim the target under the create lock (see the module docs): no
    // live app, no live quarantine, then record the target rows so the slug
    // is unclaimable until the flip trades them away. `OR IGNORE` adopts
    // our own abandoned attempt's rows — the copy below wipes the target
    // first, so the retry converges.
    let [to_fleets, to_git] = cleanup_prefixes(new_slug);
    {
        let _guard = lock_for_rename_target().await;
        let live: Option<String> = sqlx::query_scalar("SELECT id FROM app WHERE slug = ?1")
            .bind(new_slug)
            .fetch_optional(pool)
            .await
            .map_err(internal)?;
        if live.is_some() {
            return Err(RenameError::Taken);
        }
        let until: Option<i64> =
            sqlx::query_scalar("SELECT until FROM slug_quarantine WHERE slug = ?1")
                .bind(new_slug)
                .fetch_optional(pool)
                .await
                .map_err(internal)?;
        if until.is_some_and(|until| until > now_ms()) {
            return Err(RenameError::Taken);
        }
        let now = now_ms();
        for prefix in [&to_fleets, &to_git] {
            sqlx::query(
                "INSERT OR IGNORE INTO prefix_cleanup (prefix, reason, created_at) \
                 VALUES (?1, 'rename-target', ?2)",
            )
            .bind(prefix)
            .bind(now)
            .execute(pool)
            .await
            .map_err(internal)?;
        }
    }

    // 3. Bucket: the repo WAL under `git/{old}/`, and deploys, D1/DO cell
    // state, leases and the fleet secret under `fleets/{old}/`. The cell and
    // database identities live in the app's config (unchanged by a rename),
    // so the copied state stays valid under the new prefix; the copied node
    // leases name stopped nodes, and the new fleet claims its cells past
    // them (same failover as a dead owner node — the old fleet is stopped,
    // so there is no split brain).
    let [from_fleets, from_git] = cleanup_prefixes(old_slug);
    copy_prefix(store, &from_git, &to_git)
        .await
        .map_err(internal)?;
    fail_if(hooks, RenameFailStep::AfterGitCopy)?;
    copy_prefix(store, &from_fleets, &to_fleets)
        .await
        .map_err(internal)?;
    fail_if(hooks, RenameFailStep::AfterFleetsCopy)?;

    // 4. Local state: the bare mirror, the worktree and its sidecars, the
    // install cache, the fleet's state dir, and the fleet cgroup leaf (empty:
    // the fleet is stopped — the next wake recreates it under the new name).
    // The mirror moves first, so its presence means a fresh attempt: no move
    // has completed yet, everything under the target is an abandoned orphan
    // (a crashed delete's leftovers — even under paths the old slug never
    // had), and the whole target is wiped before the moves. Its absence
    // means a retry after the moves started: per path, a present source is
    // wiped-then-moved, while a missing source means already-moved and the
    // target is left alone — so moved content is never wiped into a no-op
    // move. An abandoned target's leftovers stay unclaimable behind their
    // cleanup rows until the drain proves them empty, and `create_app` keeps
    // its own claim-time wipe, so no stale state is ever inherited.
    // The manifest generation marker follows the repo; the old slug's lock
    // and wake map entries leak one idle mutex each (bounded by renames).
    let mirror_src = crate::git::manifest::mirror_path(git, old_slug);
    let fresh = tokio::fs::symlink_metadata(&mirror_src).await.is_ok();
    if fresh {
        super::local::remove_local_state(data_dir, git, cgroups, new_slug)
            .await
            .map_err(internal)?;
        sync.forget(new_slug).await;
    }
    let mirror_dst = crate::git::manifest::mirror_path(git, new_slug);
    let mirror_moved = move_local_path(&mirror_src, &mirror_dst)
        .await
        .map_err(internal)?;
    let work = PathBuf::from(data_dir).join("work");
    move_local_path(&work.join(old_slug), &work.join(new_slug))
        .await
        .map_err(internal)?;
    for sidecar in ["index", "install-hash"] {
        move_local_path(
            &work.join(format!("{old_slug}.{sidecar}")),
            &work.join(format!("{new_slug}.{sidecar}")),
        )
        .await
        .map_err(internal)?;
    }
    move_local_path(
        &PathBuf::from(data_dir).join("cache").join(old_slug),
        &PathBuf::from(data_dir).join("cache").join(new_slug),
    )
    .await
    .map_err(internal)?;
    move_local_path(
        &PathBuf::from(data_dir).join("fleets").join(old_slug),
        &PathBuf::from(data_dir).join("fleets").join(new_slug),
    )
    .await
    .map_err(internal)?;
    let sidecars = PathBuf::from(data_dir).join("secret-sidecars");
    for ext in ["orig", "target"] {
        move_local_path(
            &sidecars.join(format!("{old_slug}.{ext}")),
            &sidecars.join(format!("{new_slug}.{ext}")),
        )
        .await
        .map_err(internal)?;
    }
    if let Some(anchor) = cgroups {
        let old_leaf = anchor.join(crate::fleet::cgroups::fleet_dir_name(old_slug));
        if tokio::fs::symlink_metadata(&old_leaf).await.is_ok() {
            // A fresh attempt carries abandoned orphans away with it; after
            // the moves the leaf is already gone and stays gone.
            crate::fleet::cgroups::remove_fleet_cgroup(
                &anchor.join(crate::fleet::cgroups::fleet_dir_name(new_slug)),
            );
        }
        crate::fleet::cgroups::remove_fleet_cgroup(&old_leaf);
    }
    // The in-memory manifest seq follows the repo: it moves only while the
    // repo move is still pending (`mirror_moved`) or the old value is still
    // here (a crash between the mirror move and this transfer). A retry
    // after the transfer finds neither and keeps the moved value; a stale
    // target value never survives a fresh attempt (`set_applied_seq` only
    // moves forward, so the stale value goes first).
    let old_seq = sync.applied_seq(old_slug).await;
    if mirror_moved || old_seq.is_some() {
        sync.forget(new_slug).await;
        if let Some(seq) = old_seq {
            sync.set_applied_seq(new_slug, seq).await;
        }
    }
    sync.forget(old_slug).await;
    fail_if(hooks, RenameFailStep::BeforeCommit)?;

    // 5. The point of no return, in one transaction: drop our target rows
    // (they are live data now — the drain must never touch them), re-check
    // the new slug (a racing create took it, or its quarantine has not
    // expired), flip the row only if it still names the old slug, and
    // quarantine the old slug with `renamed_to` while recording the old
    // prefixes' cleanup rows. A UNIQUE failure on the flip is a lost race,
    // not corruption: map it to Taken.
    let mut tx = pool.begin().await.map_err(internal)?;
    for prefix in [&to_fleets, &to_git] {
        sqlx::query("DELETE FROM prefix_cleanup WHERE prefix = ?1")
            .bind(prefix)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
    }
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
    let [old_fleets, old_git] = [from_fleets.clone(), from_git.clone()];
    let now = now_ms();
    for (prefix, reason) in [(&old_fleets, "rename-old"), (&old_git, "rename-old")] {
        if let Err(e) = sqlx::query(
            "INSERT INTO prefix_cleanup (prefix, reason, created_at) VALUES (?1, ?2, ?3)",
        )
        .bind(prefix)
        .bind(reason)
        .bind(now)
        .execute(&mut *tx)
        .await
        {
            // Impossible under the claim locking (a live source slug cannot
            // have pending rows), but a lost race must refuse, never adopt.
            if is_unique_violation(&e) {
                return Err(RenameError::Taken);
            }
            return Err(internal(e));
        }
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

    // 6. Post-commit: delete the old prefixes, then their cleanup rows, then
    // purge both hosts' edge caches. Every step is warn-and-continue — the
    // rename is committed, and `drain_prefix_cleanup` (startup and the
    // maintenance loop) finishes whatever is left. Nothing here can fail
    // the rename, so any error above means "never happened" and the caller
    // refunds the quota.
    if hooks.fail_at != Some(RenameFailStep::SkipCleanup) {
        if let Err(err) = store.delete_prefix(&from_git).await {
            tracing::warn!(error = %format!("{err:#}"), "rename post-commit git prefix delete failed; the maintenance drain retries it");
        }
        if let Err(err) = store.delete_prefix(&from_fleets).await {
            tracing::warn!(error = %format!("{err:#}"), "rename post-commit fleets prefix delete failed; the maintenance drain retries it");
        }
        for prefix in [&from_git, &from_fleets] {
            if let Err(err) = sqlx::query("DELETE FROM prefix_cleanup WHERE prefix = ?1")
                .bind(prefix)
                .execute(pool)
                .await
            {
                tracing::warn!(error = %format!("{err:#}"), "rename post-commit cleanup row delete failed; the maintenance drain retries it");
            }
        }
    }
    crate::cloudflare::purge_host_cache(cache_purge, &format!("{old_slug}.{apps_domain}")).await;
    crate::cloudflare::purge_host_cache(cache_purge, &format!("{new_slug}.{apps_domain}")).await;
    // Custom hosts ride on the app id, which the rename keeps — no row
    // touch needed — but their cached pages are stale under either name.
    for host in super::domains::active_hosts(pool, &app.id).await {
        crate::cloudflare::purge_host_cache(cache_purge, &host).await;
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Fx {
        _dir: tempfile::TempDir,
        pool: SqlitePool,
        store: Store,
        git: GitConfig,
        sync: GitSync,
        data: PathBuf,
    }

    async fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let data = dir.path().join("data");
        let git = GitConfig::new(data.join("git"), None);
        Fx {
            _dir: dir,
            pool,
            store: Store::memory(),
            git,
            sync: GitSync::default(),
            data,
        }
    }

    fn alloc() -> super::super::Allocation {
        super::super::Allocation {
            uid_range: (210000, 210003),
            port_range: (18000, 18003),
        }
    }

    async fn seed_app(fx: &Fx, slug: &str) {
        let owner = super::super::ensure_account(&fx.pool, "ana@example.com")
            .await
            .unwrap();
        super::super::create(&fx.pool, &owner, slug, None, &alloc(), 10)
            .await
            .unwrap();
        for (key, body) in [
            (format!("fleets/{slug}/deploy/current.json"), "{}"),
            (format!("fleets/{slug}/cells/d1.sqlite"), "D1DATA"),
            (format!("git/{slug}/main/MANIFEST.json"), "{}"),
        ] {
            fx.store.put(&key, body.as_bytes().to_vec()).await.unwrap();
        }
        tokio::fs::create_dir_all(fx.data.join(format!("work/{slug}")))
            .await
            .unwrap();
        tokio::fs::write(fx.data.join(format!("work/{slug}.index")), b"i")
            .await
            .unwrap();
        tokio::fs::write(fx.data.join(format!("work/{slug}.install-hash")), b"h")
            .await
            .unwrap();
        tokio::fs::create_dir_all(fx.data.join(format!("cache/{slug}")))
            .await
            .unwrap();
        tokio::fs::create_dir_all(fx.data.join(format!("fleets/{slug}")))
            .await
            .unwrap();
        tokio::fs::create_dir_all(fx.git.repos_dir.join(format!("{slug}.git")))
            .await
            .unwrap();
    }

    async fn run(
        fx: &Fx,
        old: &str,
        new: &str,
        hooks: RenameHooks,
    ) -> Result<RenameOutput, RenameError> {
        let deploy_m = Arc::new(tokio::sync::Mutex::new(()));
        let rollback_m = Arc::new(tokio::sync::Mutex::new(()));
        let wake_old_m = Arc::new(tokio::sync::Mutex::new(()));
        let wake_new_m = Arc::new(tokio::sync::Mutex::new(()));
        let deploy = deploy_m.lock_owned().await;
        let rollback = rollback_m.lock_owned().await;
        let wake_old = wake_old_m.lock_owned().await;
        let wake_new = wake_new_m.lock_owned().await;
        let deps = RenameDeps {
            pool: &fx.pool,
            store: &fx.store,
            git: &fx.git,
            sync: &fx.sync,
            fleets: None,
            data_dir: &fx.data,
            cgroups: None,
            apps_domain: "outer.localhost",
            cache_purge: None,
        };
        rename_app_with_hooks(
            &deps,
            old,
            new,
            RenameLocks {
                deploy: &deploy,
                rollback: &rollback,
                wake_old: &wake_old,
                wake_new: &wake_new,
            },
            hooks,
        )
        .await
    }

    async fn cleanup_rows(fx: &Fx) -> Vec<String> {
        sqlx::query_scalar("SELECT prefix FROM prefix_cleanup ORDER BY prefix")
            .fetch_all(&fx.pool)
            .await
            .unwrap()
    }

    async fn claimable(fx: &Fx, slug: &str) -> bool {
        super::super::check_slug_claimable(&fx.pool, slug)
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn normal_path_leaves_no_job_rows() {
        let fx = fx().await;
        seed_app(&fx, "groceries").await;
        let out = run(&fx, "groceries", "pantry", RenameHooks::default())
            .await
            .unwrap();
        assert_eq!(out.app.slug, "pantry");
        assert_eq!(
            fx.store.get("fleets/pantry/cells/d1.sqlite").await.unwrap(),
            Some(b"D1DATA".to_vec())
        );
        assert_eq!(
            fx.store.get("git/pantry/main/MANIFEST.json").await.unwrap(),
            Some(b"{}".to_vec())
        );
        assert!(fx.store.list("fleets/groceries/").await.unwrap().is_empty());
        assert!(fx.store.list("git/groceries/").await.unwrap().is_empty());
        assert!(cleanup_rows(&fx).await.is_empty(), "no jobs left behind");
        let target: Option<String> =
            sqlx::query_scalar("SELECT renamed_to FROM slug_quarantine WHERE slug = 'groceries'")
                .fetch_one(&fx.pool)
                .await
                .unwrap();
        assert_eq!(target.as_deref(), Some("pantry"));
        assert!(!claimable(&fx, "groceries").await);
    }

    /// Abandoned local state under the target (a crashed delete's orphans):
    /// stale markers under every local path for `slug`.
    async fn plant_stale_target(fx: &Fx, slug: &str) {
        tokio::fs::create_dir_all(fx.data.join(format!("work/{slug}")))
            .await
            .unwrap();
        tokio::fs::write(fx.data.join(format!("work/{slug}/leftover")), b"stale")
            .await
            .unwrap();
        tokio::fs::write(fx.data.join(format!("work/{slug}.index")), b"stale")
            .await
            .unwrap();
        tokio::fs::write(fx.data.join(format!("work/{slug}.install-hash")), b"stale")
            .await
            .unwrap();
        tokio::fs::create_dir_all(fx.data.join(format!("cache/{slug}")))
            .await
            .unwrap();
        tokio::fs::write(fx.data.join(format!("cache/{slug}/leftover")), b"stale")
            .await
            .unwrap();
        tokio::fs::create_dir_all(fx.data.join(format!("fleets/{slug}")))
            .await
            .unwrap();
        tokio::fs::write(fx.data.join(format!("fleets/{slug}/leftover")), b"stale")
            .await
            .unwrap();
        tokio::fs::create_dir_all(fx.data.join("secret-sidecars"))
            .await
            .unwrap();
        tokio::fs::write(
            fx.data.join(format!("secret-sidecars/{slug}.orig")),
            b"stale",
        )
        .await
        .unwrap();
        tokio::fs::write(
            fx.data.join(format!("secret-sidecars/{slug}.target")),
            b"stale",
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(fx.git.repos_dir.join(format!("{slug}.git")))
            .await
            .unwrap();
        tokio::fs::write(
            fx.git.repos_dir.join(format!("{slug}.git/leftover")),
            b"stale",
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn target_orphans_are_wiped_before_the_local_moves() {
        let fx = fx().await;
        seed_app(&fx, "groceries").await;
        // Distinct old-slug content that must move over to the target.
        tokio::fs::write(fx.data.join("work/groceries/treasure"), b"old")
            .await
            .unwrap();
        tokio::fs::write(fx.git.repos_dir.join("groceries.git/oldfile"), b"old")
            .await
            .unwrap();
        tokio::fs::create_dir_all(fx.data.join("secret-sidecars"))
            .await
            .unwrap();
        tokio::fs::write(fx.data.join("secret-sidecars/groceries.orig"), b"old")
            .await
            .unwrap();
        // A crashed delete's orphans under the unclaimed target, plus a
        // stale in-memory manifest seq for it.
        plant_stale_target(&fx, "pantry").await;
        fx.sync.set_applied_seq("pantry", 99).await;

        let out = run(&fx, "groceries", "pantry", RenameHooks::default())
            .await
            .unwrap();
        assert_eq!(out.app.slug, "pantry");
        // The old dirs moved in over the wiped target.
        assert_eq!(
            tokio::fs::read(fx.data.join("work/pantry/treasure"))
                .await
                .unwrap(),
            b"old"
        );
        assert_eq!(
            tokio::fs::read(fx.data.join("work/pantry.index"))
                .await
                .unwrap(),
            b"i"
        );
        assert_eq!(
            tokio::fs::read(fx.data.join("work/pantry.install-hash"))
                .await
                .unwrap(),
            b"h"
        );
        assert_eq!(
            tokio::fs::read(fx.git.repos_dir.join("pantry.git/oldfile"))
                .await
                .unwrap(),
            b"old"
        );
        assert_eq!(
            tokio::fs::read(fx.data.join("secret-sidecars/pantry.orig"))
                .await
                .unwrap(),
            b"old"
        );
        // Every stale marker is gone; the stale seq went with them.
        for stale in [
            "work/pantry/leftover",
            "cache/pantry/leftover",
            "fleets/pantry/leftover",
            "secret-sidecars/pantry.target",
        ] {
            assert!(!fx.data.join(stale).exists(), "{stale} survived the wipe");
        }
        assert!(!fx.git.repos_dir.join("pantry.git/leftover").exists());
        assert_eq!(fx.sync.applied_seq("pantry").await, None);
        // The old slug's paths are gone.
        assert!(!fx.data.join("work/groceries").exists());
        assert!(!fx.git.repos_dir.join("groceries.git").exists());
    }
    #[tokio::test]
    async fn crash_after_moves_retries_without_wiping_moved_state() {
        let fx = fx().await;
        seed_app(&fx, "groceries").await;
        // Distinct old-slug content that must survive the retry.
        tokio::fs::write(fx.data.join("work/groceries/treasure"), b"old")
            .await
            .unwrap();
        tokio::fs::write(fx.git.repos_dir.join("groceries.git/oldfile"), b"old")
            .await
            .unwrap();
        tokio::fs::create_dir_all(fx.data.join("secret-sidecars"))
            .await
            .unwrap();
        tokio::fs::write(fx.data.join("secret-sidecars/groceries.orig"), b"old")
            .await
            .unwrap();
        fx.sync.set_applied_seq("groceries", 7).await;
        // Crash after the local moves, before the flip: the row still names
        // the old slug but every local path already moved.
        let err = run(
            &fx,
            "groceries",
            "pantry",
            RenameHooks {
                fail_at: Some(RenameFailStep::BeforeCommit),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RenameError::Internal(_)));
        assert!(!fx.data.join("work/groceries").exists());
        assert!(!fx.git.repos_dir.join("groceries.git").exists());
        assert_eq!(
            tokio::fs::read(fx.data.join("work/pantry/treasure"))
                .await
                .unwrap(),
            b"old"
        );
        // The same-target retry keeps the moved content: per path, a missing
        // source means already-moved, so the target is left alone.
        let out = run(&fx, "groceries", "pantry", RenameHooks::default())
            .await
            .unwrap();
        assert_eq!(out.app.slug, "pantry");
        assert_eq!(
            tokio::fs::read(fx.data.join("work/pantry/treasure"))
                .await
                .unwrap(),
            b"old"
        );
        assert_eq!(
            tokio::fs::read(fx.data.join("work/pantry.index"))
                .await
                .unwrap(),
            b"i"
        );
        assert_eq!(
            tokio::fs::read(fx.data.join("work/pantry.install-hash"))
                .await
                .unwrap(),
            b"h"
        );
        assert_eq!(
            tokio::fs::read(fx.git.repos_dir.join("pantry.git/oldfile"))
                .await
                .unwrap(),
            b"old"
        );
        assert_eq!(
            tokio::fs::read(fx.data.join("secret-sidecars/pantry.orig"))
                .await
                .unwrap(),
            b"old"
        );
        assert_eq!(fx.sync.applied_seq("pantry").await, Some(7));
        assert_eq!(fx.sync.applied_seq("groceries").await, None);
        // The old slug's paths are still gone, and the bucket data the copy
        // re-converged on is intact.
        assert!(!fx.data.join("work/groceries").exists());
        assert!(!fx.git.repos_dir.join("groceries.git").exists());
        assert_eq!(
            fx.store.get("fleets/pantry/cells/d1.sqlite").await.unwrap(),
            Some(b"D1DATA".to_vec())
        );
        assert!(cleanup_rows(&fx).await.is_empty(), "no jobs left behind");
    }

    #[tokio::test]
    async fn crash_after_copy_retries_to_another_slug_without_orphans() {
        let fx = fx().await;
        seed_app(&fx, "groceries").await;
        // Crash after both prefixes copied, before local moves: the failed
        // rename copied the data but changed nothing durable.
        let err = run(
            &fx,
            "groceries",
            "pantry",
            RenameHooks {
                fail_at: Some(RenameFailStep::AfterFleetsCopy),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RenameError::Internal(_)));
        // The old identity is fully intact: row, prefixes, data.
        let slug: Option<String> =
            sqlx::query_scalar("SELECT slug FROM app WHERE slug = 'groceries'")
                .fetch_optional(&fx.pool)
                .await
                .unwrap();
        assert_eq!(slug.as_deref(), Some("groceries"));
        assert_eq!(
            fx.store
                .get("fleets/groceries/cells/d1.sqlite")
                .await
                .unwrap(),
            Some(b"D1DATA".to_vec())
        );
        assert!(!fx.store.list("fleets/pantry/").await.unwrap().is_empty());
        // The abandoned target is blocked: its rows are pending.
        assert_eq!(
            cleanup_rows(&fx).await,
            vec!["fleets/pantry/".to_string(), "git/pantry/".to_string()]
        );
        assert!(!claimable(&fx, "pantry").await);
        // Retry to a DIFFERENT slug converges with all data.
        let out = run(&fx, "groceries", "cellar", RenameHooks::default())
            .await
            .unwrap();
        assert_eq!(out.app.slug, "cellar");
        assert_eq!(
            fx.store.get("fleets/cellar/cells/d1.sqlite").await.unwrap(),
            Some(b"D1DATA".to_vec())
        );
        assert_eq!(
            fx.store.get("git/cellar/main/MANIFEST.json").await.unwrap(),
            Some(b"{}".to_vec())
        );
        // The first attempt's target is still quarantined-by-cleanup until
        // the drain proves it empty; the old slug redirects. The successful
        // retry's post-commit already removed its own old rows, so only
        // the abandoned pantry copy remains.
        assert!(!claimable(&fx, "pantry").await);
        assert!(!claimable(&fx, "groceries").await);
        let drained = drain_prefix_cleanup(&fx.pool, &fx.store, None)
            .await
            .unwrap();
        assert_eq!(drained, 2, "the abandoned pantry copy's two rows");
        assert!(fx.store.list("fleets/pantry/").await.unwrap().is_empty());
        assert!(fx.store.list("git/pantry/").await.unwrap().is_empty());
        assert!(fx.store.list("fleets/groceries/").await.unwrap().is_empty());
        assert!(cleanup_rows(&fx).await.is_empty());
        // The abandoned target is claimable again, and provably empty: a
        // fresh app inherits no stale deploy pointer or D1 state.
        assert!(claimable(&fx, "pantry").await);
        let owner = super::super::ensure_account(&fx.pool, "bob@example.com")
            .await
            .unwrap();
        super::super::create(&fx.pool, &owner, "pantry", None, &alloc(), 10)
            .await
            .unwrap();
        assert!(fx.store.list("fleets/pantry/").await.unwrap().is_empty());
        assert!(fx.store.list("git/pantry/").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn crash_after_commit_drains_on_next_pass() {
        let fx = fx().await;
        seed_app(&fx, "groceries").await;
        // A crash right after the commit: the rename reports Ok (what a
        // restart finds), but the old prefixes and their rows are pending.
        let out = run(
            &fx,
            "groceries",
            "pantry",
            RenameHooks {
                fail_at: Some(RenameFailStep::SkipCleanup),
            },
        )
        .await
        .unwrap();
        assert_eq!(out.app.slug, "pantry");
        assert!(!fx.store.list("fleets/groceries/").await.unwrap().is_empty());
        assert_eq!(
            cleanup_rows(&fx).await,
            vec![
                "fleets/groceries/".to_string(),
                "git/groceries/".to_string()
            ]
        );
        // The new data is already live under the new prefix.
        assert_eq!(
            fx.store.get("fleets/pantry/cells/d1.sqlite").await.unwrap(),
            Some(b"D1DATA".to_vec())
        );
        // The old slug is not claimable while the job is pending.
        assert!(!claimable(&fx, "groceries").await);
        // The next maintenance pass drains the prefixes and the rows.
        let drained = drain_prefix_cleanup(&fx.pool, &fx.store, None)
            .await
            .unwrap();
        assert_eq!(drained, 2);
        assert!(fx.store.list("fleets/groceries/").await.unwrap().is_empty());
        assert!(fx.store.list("git/groceries/").await.unwrap().is_empty());
        assert!(cleanup_rows(&fx).await.is_empty());
        // ...but the old slug stays quarantined (redirecting) until expiry.
        assert!(!claimable(&fx, "groceries").await);
        let row: Option<String> =
            sqlx::query_scalar("SELECT renamed_to FROM slug_quarantine WHERE slug = 'groceries'")
                .fetch_optional(&fx.pool)
                .await
                .unwrap();
        assert_eq!(row.as_deref(), Some("pantry"));
    }

    #[tokio::test]
    async fn crash_before_commit_never_blocks_the_same_target_retry() {
        let fx = fx().await;
        seed_app(&fx, "groceries").await;
        for fail_at in [
            RenameFailStep::AfterStop,
            RenameFailStep::AfterGitCopy,
            RenameFailStep::AfterFleetsCopy,
            RenameFailStep::BeforeCommit,
        ] {
            let err = run(
                &fx,
                "groceries",
                "pantry",
                RenameHooks {
                    fail_at: Some(fail_at),
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(err, RenameError::Internal(_)), "{fail_at:?}");
        }
        // The old identity survived every crash point (row + prefixes).
        let slug: Option<String> =
            sqlx::query_scalar("SELECT slug FROM app WHERE slug = 'groceries'")
                .fetch_optional(&fx.pool)
                .await
                .unwrap();
        assert_eq!(slug.as_deref(), Some("groceries"));
        assert_eq!(
            fx.store
                .get("fleets/groceries/cells/d1.sqlite")
                .await
                .unwrap(),
            Some(b"D1DATA".to_vec())
        );
        // The same-target retry converges: the abandoned rows are adopted,
        // the target wiped and recopied, the flip trades them away — and the
        // successful retry's post-commit removes its own old rows, so no
        // maintenance pass is even due.
        let out = run(&fx, "groceries", "pantry", RenameHooks::default())
            .await
            .unwrap();
        assert_eq!(out.app.slug, "pantry");
        let drained = drain_prefix_cleanup(&fx.pool, &fx.store, None)
            .await
            .unwrap();
        assert_eq!(drained, 0, "the successful retry left no rows behind");
        assert!(cleanup_rows(&fx).await.is_empty());
    }

    #[tokio::test]
    async fn pending_cleanup_blocks_claims_past_quarantine() {
        let fx = fx().await;
        let now = now_ms();
        sqlx::query("INSERT INTO slug_quarantine (slug, until) VALUES ('stale', ?1)")
            .bind(now - 1)
            .execute(&fx.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO prefix_cleanup (prefix, reason, created_at) VALUES ('fleets/stale/', 'rename-old', ?1)",
        )
        .bind(now)
        .execute(&fx.pool)
        .await
        .unwrap();
        // An expired quarantine alone would prune; the pending row spares it.
        assert_eq!(
            super::super::prune_quarantine(&fx.pool, now).await.unwrap(),
            0
        );
        assert!(!claimable(&fx, "stale").await);
        // Once drained, the quarantine expires normally.
        let drained = drain_prefix_cleanup(&fx.pool, &fx.store, None)
            .await
            .unwrap();
        assert_eq!(drained, 1);
        assert_eq!(
            super::super::prune_quarantine(&fx.pool, now).await.unwrap(),
            1
        );
        assert!(claimable(&fx, "stale").await);
    }
    #[tokio::test]
    async fn drain_keeps_unsafe_raw_prefixes_and_deletes_nothing() {
        let fx = fx().await;
        // Data under scopes a bare prefix would swallow: it must survive.
        fx.store
            .put("fleets/keep/cells/d1.sqlite", b"D1DATA".to_vec())
            .await
            .unwrap();
        fx.store.put("stray/file", b"stray".to_vec()).await.unwrap();
        let now = now_ms();
        for prefix in ["", "fleets", "fleets/keep", "stray"] {
            sqlx::query(
                "INSERT INTO prefix_cleanup (prefix, reason, created_at) VALUES (?1, 'rename-old', ?2)",
            )
            .bind(prefix)
            .bind(now)
            .execute(&fx.pool)
            .await
            .unwrap();
        }
        // A safe unparseable row still drains by raw prefix.
        sqlx::query(
            "INSERT INTO prefix_cleanup (prefix, reason, created_at) VALUES ('stray/', 'rename-old', ?1)",
        )
        .bind(now)
        .execute(&fx.pool)
        .await
        .unwrap();
        let drained = drain_prefix_cleanup(&fx.pool, &fx.store, None)
            .await
            .unwrap();
        assert_eq!(drained, 1, "only the safe raw row drains");
        // Nothing under the unsafe scopes was touched.
        assert_eq!(
            fx.store.get("fleets/keep/cells/d1.sqlite").await.unwrap(),
            Some(b"D1DATA".to_vec())
        );
        assert_eq!(
            fx.store.get("stray/file").await.unwrap(),
            None,
            "the safe stray/ row drained"
        );
        // The unsafe rows are kept for an operator.
        assert_eq!(
            cleanup_rows(&fx).await,
            vec![
                "".to_string(),
                "fleets".to_string(),
                "fleets/keep".to_string(),
                "stray".to_string(),
            ]
        );
    }
}
