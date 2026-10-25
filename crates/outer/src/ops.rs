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

/// Create `slug` for `owner_email` from the vendored `template` starter and
/// deploy it. The template is materialized before the app row exists, so a
/// missing `outer templates sync` fails without leaving anything behind; a
/// failure after the row exists removes the row and the repo again (without
/// quarantining the slug: nobody ever saw it).
pub async fn create_app(
    p: &Platform,
    owner_email: &str,
    slug: &str,
    name: Option<&str>,
    template: templates::Template,
) -> anyhow::Result<(App, DeployOutcome)> {
    let scratch = tempfile::tempdir().context("template scratch dir")?;
    let tree = scratch.path().join("tree");
    let reference = templates::materialize(&p.store, template, &tree).await?;
    // Cheap checks first (shape, reserved, brand), then the Jev name check:
    // Jev never runs for a name the wordlists already refused, and a
    // missing client on localhost dev logs a warning and passes — exactly
    // like the gate. Failures here abort before the app row exists.
    apps::validate_slug(slug)?;
    apps::check_slug_profanity(p.deployer.settings().jev.as_ref(), slug, name).await?;
    let owner_id = apps::ensure_account(&p.pool, owner_email).await?;
    let plan: Option<String> = sqlx::query_scalar("SELECT plan FROM user WHERE id = ?1")
        .bind(&owner_id)
        .fetch_optional(&p.pool)
        .await?;
    let mut app = apps::create(
        &p.pool,
        &owner_id,
        slug,
        name,
        &p.alloc,
        apps::app_limit_for_plan(&plan.clone().unwrap_or_default()),
    )
    .await?;
    // Same Pro default as the MCP path: new Pro apps keep warm. Best
    // effort: the row already exists here, so a flag write that fails must
    // not abort creation and orphan it (the app stays usable; the flag can
    // be set later). Seeding failures below still remove the row.
    if plan.as_deref() == Some("pro") {
        match apps::always_on::set_always_on(&p.pool, &app.id, true, "pro").await {
            Ok(()) => app.always_on = true,
            Err(err) => tracing::warn!(
                slug = %slug,
                error = %format!("{err:#}"),
                "pro default keep-warm flag not set; set it later"
            ),
        }
    }
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
        let message = template.seed_message(&reference.sha);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::DeploySettings;
    use crate::fleet::manager::FleetStorage;

    /// The operator CLI path gives a Pro owner's new app `always_on`: the
    /// plan lookup must bind the owner, or every Pro create starts cold.
    #[tokio::test]
    async fn pro_create_defaults_to_keep_warm() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let owner = apps::ensure_account(&pool, "pro@example.com")
            .await
            .unwrap();
        sqlx::query("UPDATE user SET plan = 'pro' WHERE id = ?1")
            .bind(&owner)
            .execute(&pool)
            .await
            .unwrap();

        // A minimal deployable template: no package.json (install and build
        // skip), no D1 (migrate skips), a valid deploy root for the upload.
        let store = Store::memory();
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("dist")).unwrap();
        std::fs::create_dir_all(src.path().join("pages")).unwrap();
        std::fs::write(
            src.path().join("dist/wrangler.json"),
            "{\"name\":\"t\",\"main\":\"index.js\",\"compatibility_date\":\"2026-01-01\"}",
        )
        .unwrap();
        std::fs::write(src.path().join("pages/index.tsx"), "<h1>hi</h1>\n").unwrap();
        let git_out = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .current_dir(src.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        git_out(&["init", "-b", "main"]);
        git_out(&["add", "-A"]);
        git_out(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "seed",
        ]);
        let sha = git_out(&["rev-parse", "HEAD"]);
        let bundle = src.path().join("template.bundle");
        git_out(&["bundle", "create", bundle.to_str().unwrap(), "main"]);
        crate::templates::put_test_template(
            &store,
            crate::templates::Template::App,
            sha.trim(),
            &bundle,
        )
        .await
        .unwrap();

        // A fake celld that answers the upload; no other step runs celld.
        let state = tempfile::tempdir().unwrap();
        let script = state.path().join("fake-celld");
        let tmp = script.with_extension("tmp");
        std::fs::write(
            &tmp,
            "#!/bin/sh\nprintf '{\"version\":\"fake-v1\"}\\n'\nexit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::rename(&tmp, &script).unwrap();

        let data = tempfile::tempdir().unwrap();
        let settings = DeploySettings {
            data_dir: data.path().to_path_buf(),
            celld_bin: script.to_string_lossy().into_owned(),
            esbuild_bin: None,
            bun_bin: "/bin/true".to_string(),
            storage: FleetStorage {
                endpoint: "http://127.0.0.1:19000".to_string(),
                region: "us-east-1".to_string(),
                bucket: state.path().to_string_lossy().into_owned(),
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: "outeraccess".to_string(),
                    secret_access_key: "outersecretoutersecretoutersecret12".to_string(),
                },
            },
            step_timeout: std::time::Duration::from_secs(120),
            cgroups: None,
            apps_domain: "outer.localhost".to_string(),
            cache_purge: None,
            jev: None,
            wake: None,
            secrets_key: crate::secrets::SecretsKey::test_key(),
        };
        let p = Platform {
            pool: pool.clone(),
            store,
            git: GitConfig::new(data.path().join("git"), None),
            sync: GitSync::default(),
            deployer: Deployer::new(settings, 1),
            alloc: Allocation {
                uid_range: (210000, 210003),
                port_range: (18000, 18003),
            },
            data_dir: data.path().to_path_buf(),
            cgroups: None,
            domain: "outer.localhost".to_string(),
        };
        let (app, outcome) = create_app(
            &p,
            "pro@example.com",
            "warmshop",
            None,
            crate::templates::Template::App,
        )
        .await
        .unwrap();
        assert_eq!(outcome.status, "success", "error: {:?}", outcome.error);
        assert!(app.always_on, "a Pro owner's new app keeps warm");
        let flag: i64 = sqlx::query_scalar("SELECT always_on FROM app WHERE id = ?1")
            .bind(&app.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(flag, 1);
        drop(p);
        pool.close().await;
    }
}
