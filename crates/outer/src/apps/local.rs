//! Per-slug local state: every path on this host a slug owns.
//!
//! A slug owns its bare mirror (`{repos}/{slug}.git`), its worktree and
//! sidecars (`work/{slug}`, `work/{slug}.index`, `work/{slug}.install-hash`),
//! its install cache (`cache/{slug}`), its fleet state dir
//! (`fleets/{slug}`), its crash-recovery secret sidecars
//! (`secret-sidecars/{slug}.orig`, `.target`), and its fleet cgroup leaf.
//! [`remove_local_state`] wipes all of them. Idempotent: a missing path is
//! done, anything else is a real failure.
//!
//! Shared by three claim points: `delete_app` (step 3, after the bucket
//! purge), the create path (right after the row insert, before `init_repo`
//! writes anything), and `rename_app` (for the TARGET slug, before the old
//! dirs move in). The create/rename wipes matter because a crashed delete
//! can leave local orphans behind under a slug that becomes unclaimed again:
//! without the wipe, `move_path`'s existing-destination-means-done would
//! silently skip moving the old dirs in (leaving the rename half-applied),
//! and a fresh create would inherit stale caches, fleet state and sidecars.

use std::path::Path;

use crate::fleet::cgroups;
use crate::git::GitConfig;

/// Wipe every local path `slug` owns. See the module docs for the list.
pub(crate) async fn remove_local_state(
    data_dir: &Path,
    git_cfg: &GitConfig,
    cgroups_anchor: Option<&Path>,
    slug: &str,
) -> anyhow::Result<()> {
    remove_dir(&crate::git::manifest::mirror_path(git_cfg, slug)).await?;
    let work = data_dir.join("work");
    remove_dir(&work.join(slug)).await?;
    remove_file(&work.join(format!("{slug}.index"))).await?;
    remove_file(&work.join(format!("{slug}.install-hash"))).await?;
    remove_dir(&data_dir.join("cache").join(slug)).await?;
    remove_dir(&data_dir.join("fleets").join(slug)).await?;
    let sidecars = data_dir.join("secret-sidecars");
    remove_file(&sidecars.join(format!("{slug}.orig"))).await?;
    remove_file(&sidecars.join(format!("{slug}.target"))).await?;
    if let Some(anchor) = cgroups_anchor {
        cgroups::remove_fleet_cgroup(&anchor.join(cgroups::fleet_dir_name(slug)));
    }
    Ok(())
}

/// `remove_dir_all`, treating a missing path as done. Anything else is a
/// real failure (the row stays, so a retry finishes the job).
async fn remove_dir(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(anyhow::anyhow!(
            "removing local dir {}: {err}",
            path.display()
        )),
    }
}

/// `remove_file`, treating a missing path as done.
async fn remove_file(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(anyhow::anyhow!(
            "removing local file {}: {err}",
            path.display()
        )),
    }
}
