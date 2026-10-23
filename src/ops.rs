//! App operations: the units the CLI runs today and the MCP tools will run
//! in M1. Each one composes the registry, the git WAL, the template and the
//! deploy pipeline.

use std::time::Duration;

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
    let app = apps::create(&p.pool, &owner_id, slug, name, &p.alloc).await?;
    let author = noreply_author(&owner_id, owner_email, &p.domain);
    let seeded = async {
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

/// Ask the app's fleet to adopt the new deploy now. A sleeping fleet isn't
/// listening, which is fine: it adopts the deploy when it wakes, and an
/// awake fleet would also pick it up on its own pointer poll.
pub async fn reload_if_awake(app: &App) -> bool {
    let url = format!("http://127.0.0.1:{}/reload", app.internal_port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build();
    let Ok(client) = client else { return false };
    client
        .post(url)
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}
