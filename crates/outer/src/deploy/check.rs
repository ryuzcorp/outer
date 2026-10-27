//! MCP `check` (spec §1.5): install, build, typecheck, lint and tests over a
//! draft tree in the build sandbox, without deploying.
//!
//! The draft tree (a dangling commit from [`crate::git::drafts`]) is
//! materialized into its own `checks/<slug>` dir — never the live
//! worktree, so a check racing a deploy can corrupt neither — handed to the
//! app uid and built with the same sandbox helper as the pipeline. Steps the
//! template does not support skip (no `package.json`, no `scripts.build`,
//! no `tsconfig.json`, no oxlint, no tests). Every step runs even when an
//! earlier one failed, so one check returns every diagnostic at once.
//!
//! Charging lives with the caller ([`crate::mcp::tools`]): the whole check
//! wall time lands on the owner's daily build minutes, exactly like the
//! pipeline's install+build charge.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sqlx::SqlitePool;

use super::pipeline::Deployer;
use crate::git::GitConfig;

/// One `check` step for the response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckStep {
    pub name: String,
    pub ms: u64,
    pub skipped: bool,
    pub ok: bool,
}

/// What `check_tree` ran: per-step timings plus the combined output the
/// caller parses into structured diagnostics.
#[derive(Debug, Clone)]
pub struct CheckOutput {
    pub steps: Vec<CheckStep>,
    pub log: String,
}

impl Deployer {
    /// Run the check steps over the draft tree `draft_sha` (a full commit
    /// sha in the app's mirror) and return the combined log. Never deploys,
    /// never runs the content gate.
    pub async fn check_tree(
        &self,
        _pool: &SqlitePool,
        git: &GitConfig,
        app: &crate::apps::App,
        draft_sha: &str,
    ) -> anyhow::Result<CheckOutput> {
        if draft_sha.len() != 40 || !draft_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("refusing to check {draft_sha:?}: not a full commit sha");
        }
        let work = self.check_dir(&app.slug);
        if work.exists() {
            tokio::fs::remove_dir_all(&work).await?;
        }
        tokio::fs::create_dir_all(&work).await?;
        let mut log = String::new();
        let mut steps = Vec::new();
        materialize_draft(git, &app.slug, draft_sha, &work, &mut log).await?;
        super::pipeline::hand_worktree_to_app(&app.slug, &work, app.uid);

        let started = Instant::now();
        let (skipped, ok, out) = self.check_install(app, &work).await;
        push_check(&mut steps, &mut log, "install", started, skipped, ok, &out);
        let started = Instant::now();
        let (skipped, ok, out) = self.check_build(app, &work).await;
        push_check(&mut steps, &mut log, "build", started, skipped, ok, &out);
        let started = Instant::now();
        let (skipped, ok, out) = self.check_typecheck(app, &work).await;
        push_check(
            &mut steps,
            &mut log,
            "typecheck",
            started,
            skipped,
            ok,
            &out,
        );
        let started = Instant::now();
        let (skipped, ok, out) = self.check_lint(app, &work).await;
        push_check(&mut steps, &mut log, "lint", started, skipped, ok, &out);
        let started = Instant::now();
        let (skipped, ok, out) = self.check_tests(app, &work, draft_sha, git).await;
        push_check(&mut steps, &mut log, "tests", started, skipped, ok, &out);

        Ok(CheckOutput { steps, log })
    }

    /// This check's scratch dir. Separate from the deploy worktree so a
    /// check racing a deploy corrupts neither.
    fn check_dir(&self, slug: &str) -> PathBuf {
        self.settings().data_dir.join("checks").join(slug)
    }

    /// `bun install` when the draft has a `package.json`.
    async fn check_install(&self, app: &crate::apps::App, work: &Path) -> (bool, bool, String) {
        let pkg = work.join("package.json");
        if !pkg.is_file() {
            return (true, true, "no package.json; skipping install".to_string());
        }
        let mut args = vec!["install".to_string()];
        if work.join("bun.lock").is_file() {
            args.push("--frozen-lockfile".to_string());
        }
        self.run_check_step(app, work, "install", &args).await
    }

    /// `bun run build` when `package.json` has `scripts.build`.
    async fn check_build(&self, app: &crate::apps::App, work: &Path) -> (bool, bool, String) {
        let parsed: serde_json::Value = match std::fs::read_to_string(work.join("package.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(v) => v,
            None => return (true, true, "no package.json; skipping build".to_string()),
        };
        let has_build = parsed
            .get("scripts")
            .and_then(|s| s.get("build"))
            .and_then(|b| b.as_str())
            .is_some_and(|b| !b.trim().is_empty());
        if !has_build {
            return (true, true, "no scripts.build; skipping build".to_string());
        }
        self.run_check_step(
            app,
            work,
            "build",
            &["run".to_string(), "build".to_string()],
        )
        .await
    }

    /// `tsc --noEmit` when the draft has a `tsconfig.json`.
    async fn check_typecheck(&self, app: &crate::apps::App, work: &Path) -> (bool, bool, String) {
        if !work.join("tsconfig.json").is_file() {
            return (
                true,
                true,
                "no tsconfig.json; skipping typecheck".to_string(),
            );
        }
        self.run_check_step(
            app,
            work,
            "typecheck",
            &["x".to_string(), "tsc".to_string(), "--noEmit".to_string()],
        )
        .await
    }

    /// oxlint as JSON when the draft opts in: an oxlint config file or an
    /// oxlint dependency. JSON keeps the diagnostics machine-readable.
    async fn check_lint(&self, app: &crate::apps::App, work: &Path) -> (bool, bool, String) {
        let has_config = work.join(".oxlintrc.json").is_file();
        let has_dep = std::fs::read_to_string(work.join("package.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .map(|pkg| {
                ["dependencies", "devDependencies"]
                    .iter()
                    .any(|scope| pkg.get(scope).and_then(|d| d.get("oxlint")).is_some())
            })
            .unwrap_or(false);
        if !has_config && !has_dep {
            return (true, true, "no oxlint config; skipping lint".to_string());
        }
        self.run_check_step(
            app,
            work,
            "lint",
            &[
                "x".to_string(),
                "oxlint".to_string(),
                "--format=json".to_string(),
                ".".to_string(),
            ],
        )
        .await
    }

    /// `bun test` when the draft has a test script or any test files.
    async fn check_tests(
        &self,
        app: &crate::apps::App,
        work: &Path,
        draft_sha: &str,
        git: &GitConfig,
    ) -> (bool, bool, String) {
        let has_script = std::fs::read_to_string(work.join("package.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|pkg| {
                pkg.get("scripts")
                    .and_then(|s| s.get("test"))
                    .and_then(|t| t.as_str())
                    .map(|t| !t.trim().is_empty())
            })
            .unwrap_or(false);
        if !has_script && !has_test_files(git, &app.slug, draft_sha).await {
            return (true, true, "no tests; skipping".to_string());
        }
        self.run_check_step(app, work, "tests", &["test".to_string()])
            .await
    }

    /// One sandboxed step in the check dir, mirroring the pipeline's runner:
    /// cleared env plus the bun dir, the app uid, the shared cache root and
    /// a per-step cgroup leaf. A failing step returns its output, never an
    /// error: the caller records every step.
    async fn run_check_step(
        &self,
        app: &crate::apps::App,
        work: &Path,
        step: &str,
        args: &[String],
    ) -> (bool, bool, String) {
        let parent = Path::new(&self.settings().bun_bin)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "/usr/local/bin".to_string());
        let env = vec![(
            "PATH".to_string(),
            format!("{parent}:/usr/local/bin:/usr/bin:/bin"),
        )];
        let cache_root = self.settings().data_dir.join("cache").join(&app.slug);
        let leaf = self
            .build_leaf(&format!("check-{}", uuid::Uuid::new_v4()), step)
            .ok()
            .flatten();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let sb = crate::fleet::sandbox::Sandbox {
            uid: Some(app.uid),
            gid: Some(app.uid),
            platform_env: &env,
            env: &[],
            cache_root: &cache_root,
            cgroup_leaf: leaf.as_deref(),
        };
        match crate::fleet::sandbox::run_sandboxed(
            &self.settings().bun_bin,
            &arg_refs,
            work,
            &sb,
            self.settings().step_timeout,
        )
        .await
        {
            Ok(out) => (false, true, out),
            Err(e) => (false, false, format!("{e:#}")),
        }
    }
}

/// Materialize the draft tree into an empty dir: `read-tree --reset -u`
/// moves the sidecar index and worktree to exactly the draft, `clean`
/// deletes files the tree no longer has. Same shape as the deploy's
/// materialize, against a scratch dir instead of the live worktree.
async fn materialize_draft(
    git: &GitConfig,
    slug: &str,
    draft_sha: &str,
    work: &Path,
    log: &mut String,
) -> anyhow::Result<()> {
    let mirror = crate::git::manifest::mirror_path(git, slug);
    if !mirror.is_dir() {
        anyhow::bail!("no mirror for {slug}");
    }
    let index = work.with_extension("index");
    let git_dir = mirror.to_string_lossy().into_owned();
    let work_tree = work.to_string_lossy().into_owned();
    let timeout = Duration::from_secs(120);
    let checkout = run_git_sidecar(
        &[
            "--git-dir",
            &git_dir,
            "--work-tree",
            &work_tree,
            "read-tree",
            "--reset",
            "-u",
            draft_sha,
        ],
        work,
        &index,
        timeout,
    )
    .await?;
    log.push_str(&checkout);
    let clean = run_git_sidecar(
        &[
            "--git-dir",
            &git_dir,
            "--work-tree",
            &work_tree,
            "clean",
            "-ffdx",
            "-e",
            "node_modules",
        ],
        work,
        &index,
        timeout,
    )
    .await?;
    log.push_str(&clean);
    let _ = tokio::fs::remove_file(&index).await;
    Ok(())
}
/// Run git with a sidecar index, capturing both streams; non-zero exits and
/// timeouts are errors carrying the output. Mirrors the pipeline's helper
/// for the check's own scratch index.
async fn run_git_sidecar(
    args: &[&str],
    cwd: &Path,
    index: &Path,
    timeout: Duration,
) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(timeout, cmd.output()).await??;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(combined)
    } else {
        anyhow::bail!("git {} failed: {combined}", args.join(" "));
    }
}

/// Whether the draft tree holds any test files outside `node_modules` (a
/// fresh check dir has none yet, so ask the mirror instead of the disk).
async fn has_test_files(git: &GitConfig, slug: &str, draft_sha: &str) -> bool {
    let mirror = crate::git::manifest::mirror_path(git, slug);
    let git_dir = format!("--git-dir={}", mirror.display());
    let listing = match crate::git::manifest::run_git(
        &[&git_dir, "ls-tree", "-r", "--name-only", draft_sha],
        &[],
        Duration::from_secs(30),
    )
    .await
    {
        Ok(out) => out,
        Err(_) => return false,
    };
    listing.lines().any(|p| {
        let testy = p.contains(".test.") || p.contains(".spec.");
        testy && !p.starts_with("node_modules/") && !p.starts_with("dist/")
    })
}

fn push_check(
    steps: &mut Vec<CheckStep>,
    log: &mut String,
    name: &str,
    started: Instant,
    skipped: bool,
    ok: bool,
    out: &str,
) {
    steps.push(CheckStep {
        name: name.to_string(),
        ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        skipped,
        ok,
    });
    log.push_str(&format!("=== {name} ===\n{out}\n"));
}
