//! App operations: the units the CLI runs today and the MCP tools will run
//! in M1. Each one composes the registry, the git WAL, the template and the
//! deploy pipeline.

use anyhow::Context as _;
use sqlx::SqlitePool;

use crate::apps::{self, Allocation, App};
use crate::deploy::{DeployOutcome, Deployer};
use crate::git::{manifest, Author, GitConfig, GitSync};
use crate::store::Store;
use crate::templates;

/// Everything an app operation touches.
pub struct Platform {
    pub pool: SqlitePool,
    pub store: Store,
    pub git: GitConfig,
    pub sync: GitSync,
    pub deployer: Deployer,
    pub alloc: Allocation,
    /// Local state lives here (`work/`, `cache/`, `fleets/`, mirrors): the
    /// create path wipes a claimed slug's orphans before writing any.
    pub data_dir: std::path::PathBuf,
    /// Cgroup anchor when containment is active, else `None` (dev).
    pub cgroups: Option<std::path::PathBuf>,
    /// Commit identities are `<account-id>@users.noreply.<domain>`.
    pub domain: String,
}

/// The noreply identity commits are authored as.
pub fn noreply_author(account_id: &str, email: &str, domain: &str) -> Author {
    let name = email.split('@').next().unwrap_or(email).to_string();
    Author {
        name,
        email: format!("{account_id}@users.noreply.{domain}"),
    }
}

/// Create `slug` for `owner_email` from the vendored Ilha + Void template and
/// deploy it. The template is materialized before the app row exists, so a
/// missing `outer templates sync` fails without leaving anything behind; a
/// failure after the row exists removes the row and the repo again (without
/// quarantining the slug: nobody ever saw it).
pub async fn create_app(
    p: &Platform,
    owner_email: &str,
    slug: &str,
    name: Option<&str>,
) -> anyhow::Result<(App, DeployOutcome)> {
    let scratch = tempfile::tempdir().context("template scratch dir")?;
    let tree = scratch.path().join("tree");
    let template = templates::materialize(&p.store, &tree).await?;

    let owner_id = apps::ensure_account(&p.pool, owner_email).await?;
    let plan: Option<String> = sqlx::query_scalar("SELECT plan FROM user WHERE id = ?1")
        .bind(&owner_id)
        .fetch_optional(&p.pool)
        .await?;
    let app = apps::create(
        &p.pool,
        &owner_id,
        slug,
        name,
        &p.alloc,
        apps::app_limit_for_plan(&plan.unwrap_or_default()),
    )
    .await?;
    let author = noreply_author(&owner_id, owner_email, &p.domain);
    let seeded = async {
        // Claim-point wipe: a crashed delete can leave local orphans behind
        // under this now-unclaimed slug. The row insert above is the claim
        // and nothing local is written yet, so wipe before `init_repo`. A
        // wipe failure lands in the row-removing cleanup below (nobody ever
        // saw the slug: no quarantine). The stale in-memory manifest seq
        // goes too, or the sync could read it as already-applied.
        crate::apps::local::remove_local_state(&p.data_dir, &p.git, p.cgroups.as_deref(), slug)
            .await?;
        p.sync.forget(slug).await;
        manifest::init_repo(&p.store, &p.git, &p.sync, slug).await?;
        let message = format!(
            "Start from the Ilha + Void template ({})",
            &template.sha[..12.min(template.sha.len())]
        );
        let sha = crate::git::commit::import_dir(
            &p.store, &p.git, &p.sync, slug, &tree, &message, &author,
        )
        .await?;
        anyhow::Ok(sha)
    }
    .await;
    let sha = match seeded {
        Ok(sha) => sha,
        Err(err) => {
            let _ = manifest::purge(&p.store, &p.git, &p.sync, slug).await;
            let _ = sqlx::query("DELETE FROM app WHERE id = ?1")
                .bind(&app.id)
                .execute(&p.pool)
                .await;
            return Err(err.context(format!("seeding {slug} from the template")));
        }
    };
    let outcome = p.deployer.deploy(&p.pool, &p.git, &app, &sha).await?;
    Ok((app, outcome))
}

/// Deploy the current tip of `slug`'s `main`.
pub async fn deploy_app(p: &Platform, slug: &str) -> anyhow::Result<DeployOutcome> {
    let app = apps::get_by_slug(&p.pool, slug)
        .await?
        .with_context(|| format!("no app {slug:?}"))?;
    let bare = manifest::ensure_synced(&p.store, &p.git, &p.sync, slug).await?;
    let sha = manifest::head_of(&bare)
        .await?
        .with_context(|| format!("{slug} has no commits on main"))?;
    p.deployer.deploy(&p.pool, &p.git, &app, &sha).await
}

/// Delete `slug`: the operator CLI's path to the same core the MCP
/// `delete_app` tool runs. `confirm` is the slug itself there (the operator
/// typed the command). No fleet manager runs in this process: the serve
/// loop's sleep sweep stops the orphaned fleet once its row is gone (it
/// already does for missing rows).
pub async fn delete_app(
    p: &Platform,
    cfg: &crate::apps::delete::DeleteConfig,
    slug: &str,
    confirm: &str,
    locks: crate::apps::delete::DeleteLocks<'_>,
) -> anyhow::Result<crate::apps::delete::DeleteOutput> {
    let deps = crate::apps::delete::DeleteDeps {
        pool: &p.pool,
        store: &p.store,
        git: &p.git,
        sync: &p.sync,
        fleets: None,
    };
    crate::apps::delete::delete_app(&deps, cfg, slug, confirm, locks)
        .await
        .map_err(|err| anyhow::anyhow!("{err:#}"))
}
