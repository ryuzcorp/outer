//! The deploy pipeline (spec §10.1, §10.2; `crate::deploy` contract).
//!
//! `Deployer::deploy` materializes `sha` from the bare mirror into
//! `{data}/work/{slug}`, runs `bun install` (skipped when the lockfile hash
//! is unchanged) and `bun run build`, validates the wrangler config at the
//! deploy root, and runs `celld deploy`. Steps 2, 3 and 5 run sandboxed as the
//! app's uid through `run_sandboxed`; the worktree and deploy-root steps run
//! as outer (they only touch git metadata and read files).
//!
//! Concurrency: a global semaphore bounds simultaneous builds and a per-app
//! mutex serializes deploys of the same app. The `deploy` row is inserted as
//! `queued` before either is waited on, then moved to `building` once both
//! are held, and to `deploying` before `celld deploy` runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::apps::{self, App};
use crate::fleet::cgroups;
use crate::fleet::manager::FleetStorage;
use crate::fleet::sandbox::{self, Sandbox};
use crate::git::manifest::{self, GitConfig};

/// What the pipeline needs: dirs, tool paths, storage creds, timeouts.
#[derive(Debug, Clone)]
pub struct DeploySettings {
    /// Worktrees at `{data}/work/{slug}`, caches at `{data}/cache/{slug}`.
    pub data_dir: PathBuf,
    /// `celld` binary (`celld` on PATH, or an absolute path).
    pub celld_bin: String,
    /// Passed to celld as `CELLD_ESBUILD` when set.
    pub esbuild_bin: Option<String>,
    /// `bun` binary (`bun` on PATH, or an absolute path).
    pub bun_bin: String,
    pub storage: FleetStorage,
    /// Per step (120 s in production).
    pub step_timeout: Duration,
    /// Cgroup anchor when containment is active, else `None` (dev).
    pub cgroups: Option<PathBuf>,
}

/// One pipeline step's timing for the `deploy` row's `steps` JSON.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Step {
    pub name: String,
    pub ms: u64,
    pub skipped: bool,
}

/// The deploy result returned to the caller (and mirrored into the row).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeployOutcome {
    pub deploy_id: String,
    pub sha: String,
    pub status: &'static str,
    pub failed_step: Option<String>,
    pub error: Option<String>,
    pub steps: Vec<Step>,
    pub log_tail: String,
    pub version: Option<String>,
}

/// Builds apps: the global semaphore plus per-app locks live here.
pub struct Deployer {
    settings: DeploySettings,
    semaphore: Arc<Semaphore>,
    app_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

/// Combined log kept per deploy, tail-capped before it hits the row.
const LOG_TAIL_CAP: usize = 64 * 1024;
/// Cap on one `error` line stored in the row.
const ERROR_CAP: usize = 1024;
/// Bounds for one build's cgroup leaf (spec §12.2: 2 GiB parent, 1 GiB leaf).
const BUILD_PARENT_MEMORY_MAX: u64 = 2 * 1024 * 1024 * 1024;
const BUILD_PARENT_CPU_WEIGHT: u64 = 20;
const BUILD_LEAF_MEMORY_MAX: u64 = 1024 * 1024 * 1024;

/// Top-level wrangler keys celld accepts. Anything else fails `deploy-root`.
const ACCEPTED_CONFIG_KEYS: &[&str] = &[
    "$schema",
    "name",
    "main",
    "no_bundle",
    "compatibility_date",
    "compatibility_flags",
    "durable_objects",
    "migrations",
    "assets",
    "services",
    "triggers",
    "vars",
    "d1_databases",
    "kv_namespaces",
    "queues",
    "workflows",
    "r2_buckets",
    "worker_loaders",
    "containers",
    "define",
    "rules",
];

impl Deployer {
    pub fn new(settings: DeploySettings, build_concurrency: usize) -> Self {
        Self {
            settings,
            semaphore: Arc::new(Semaphore::new(build_concurrency)),
            app_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Deploy `sha` of `app`: row `queued` → `building` → `deploying` →
    /// `success`|`failed`. On success calls `apps::set_last_deploy`. Returns
    /// the outcome for both success and step failure; only bookkeeping
    /// failures (the row itself) are `Err`.
    pub async fn deploy(
        &self,
        pool: &SqlitePool,
        git: &GitConfig,
        app: &App,
        sha: &str,
    ) -> anyhow::Result<DeployOutcome> {
        let deploy_id = uuid::Uuid::new_v4().to_string();
        insert_deploy_row(pool, &deploy_id, &app.id, sha, apps::now_ms()).await?;

        // Both after the row: a queued deploy is visible while it waits.
        // App lock first, so deploys queued behind one app hold no build slot.
        let app_lock = self.app_lock(&app.id);
        let _app_guard = app_lock.lock().await;
        let _build_permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| anyhow::anyhow!("build semaphore closed: {e}"))?;

        set_deploy_status(pool, &deploy_id, "building").await?;

        let mut journal = Journal::default();
        let result = self
            .run_steps(pool, &deploy_id, git, app, sha, &mut journal)
            .await;
        let Journal { steps, log } = journal;
        match result {
            Ok(version) => {
                let log_tail = tail_cap(&log);
                let steps_json = steps_json(&steps);
                finish_success(pool, &deploy_id, &steps_json, &log_tail).await?;
                apps::set_last_deploy(pool, &app.id, sha).await?;
                Ok(DeployOutcome {
                    deploy_id,
                    sha: sha.to_string(),
                    status: "success",
                    failed_step: None,
                    error: None,
                    steps,
                    log_tail,
                    version,
                })
            }
            Err(failed) => {
                if failed.step == "build" {
                    // A failed build may leave half-written output; drop it so
                    // the next deploy starts from a clean checkout.
                    let work = self.work_dir(&app.slug);
                    if let Err(e) = tokio::fs::remove_dir_all(&work).await {
                        tracing::warn!(slug = %app.slug, error = %format!("{e:#}"), "failed build left a worktree behind");
                    }
                }
                let log_tail = tail_cap(&log);
                let steps_json = steps_json(&steps);
                finish_failed(pool, &deploy_id, &failed.error, &steps_json, &log_tail).await?;
                Ok(DeployOutcome {
                    deploy_id,
                    sha: sha.to_string(),
                    status: "failed",
                    failed_step: Some(failed.step.to_string()),
                    error: Some(failed.error),
                    steps,
                    log_tail,
                    version: None,
                })
            }
        }
    }

    fn app_lock(&self, app_id: &str) -> Arc<AsyncMutex<()>> {
        self.app_locks
            .lock()
            .expect("app lock map poisoned")
            .entry(app_id.to_string())
            .or_default()
            .clone()
    }

    fn work_dir(&self, slug: &str) -> PathBuf {
        self.settings.data_dir.join("work").join(slug)
    }

    fn cache_dir(&self, slug: &str) -> PathBuf {
        self.settings.data_dir.join("cache").join(slug)
    }

    /// Rows run in order; the first failure stops the pipeline. `Ok` carries
    /// the celld version on success.
    async fn run_steps(
        &self,
        pool: &SqlitePool,
        deploy_id: &str,
        git: &GitConfig,
        app: &App,
        sha: &str,
        journal: &mut Journal,
    ) -> Result<Option<String>, StepFailed> {
        let Journal { steps, log } = journal;
        // 1. Worktree.
        let started = Instant::now();
        match self.materialize(git, app, sha).await {
            Ok(out) => {
                push_step(steps, log, "worktree", started, false, &out);
            }
            Err(e) => {
                let ms = push_step(steps, log, "worktree", started, false, &format!("{e:#}"));
                tracing::warn!(slug = %app.slug, sha, ms, error = %format!("{e:#}"), "deploy failed at worktree");
                return Err(StepFailed::plain("worktree", &e));
            }
        }

        // The worktree is outer-owned; the install, build and deploy steps
        // run as the app uid, so hand it over before they write into it.
        hand_worktree_to_app(&app.slug, &self.work_dir(&app.slug), app.uid);
        // 2. Install.
        let started = Instant::now();
        match self.install(app, deploy_id).await {
            Ok((skipped, out)) => {
                push_step(steps, log, "install", started, skipped, &out);
            }
            Err(e) => {
                push_step(steps, log, "install", started, false, &format!("{e:#}"));
                return Err(StepFailed::sandboxed("install", &e));
            }
        }

        // 3. Build.
        let started = Instant::now();
        match self.build(app, deploy_id).await {
            Ok((skipped, out)) => {
                push_step(steps, log, "build", started, skipped, &out);
            }
            Err(e) => {
                push_step(steps, log, "build", started, false, &format!("{e:#}"));
                return Err(StepFailed::sandboxed("build", &e));
            }
        }

        // 4. Deploy root.
        let started = Instant::now();
        let root = match self.deploy_root(app) {
            Ok((root, out)) => {
                push_step(steps, log, "deploy-root", started, false, &out);
                root
            }
            Err(e) => {
                push_step(steps, log, "deploy-root", started, false, &format!("{e:#}"));
                return Err(StepFailed::plain("deploy-root", &e));
            }
        };

        // 5. `celld deploy`.
        set_status_blocking(pool, deploy_id).await?;
        let started = Instant::now();
        match self.celld_deploy(app, deploy_id, &root).await {
            Ok((out, version)) => {
                push_step(steps, log, "deploy", started, false, &out);
                Ok(version)
            }
            Err(e) => {
                push_step(steps, log, "deploy", started, false, &format!("{e:#}"));
                Err(StepFailed::sandboxed("deploy", &e))
            }
        }
    }

    /// Step 1: the worktree holds exactly `sha`'s tree, except `node_modules`.
    ///
    /// `GIT_INDEX_FILE` points at a sidecar index (`{data}/work/{slug}.index`)
    /// so `clean` knows what the last materialization tracked: `checkout
    /// --force` brings tracked files to `sha`, then `clean -ffdx -e
    /// node_modules` removes everything else (build output, stale files).
    async fn materialize(&self, git: &GitConfig, app: &App, sha: &str) -> anyhow::Result<String> {
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("refusing to materialize {sha:?}: not a full commit sha");
        }
        let mirror = manifest::mirror_path(git, &app.slug);
        if !mirror.is_dir() {
            anyhow::bail!("no mirror for {}", app.slug);
        }
        let work = self.work_dir(&app.slug);
        std::fs::create_dir_all(&work)?;
        let index = self
            .settings
            .data_dir
            .join("work")
            .join(format!("{}.index", app.slug));
        let git_dir = mirror.to_string_lossy().into_owned();
        let work_tree = work.to_string_lossy().into_owned();

        let mut log = String::new();
        let timeout = self.settings.step_timeout;
        let checkout = run_git(
            &[
                "--git-dir",
                &git_dir,
                "--work-tree",
                &work_tree,
                // `read-tree --reset -u` moves index and worktree to exactly
                // `sha`'s tree, deleting files the tree no longer has;
                // `checkout <sha> -- .` would leave them behind.
                "read-tree",
                "--reset",
                "-u",
                sha,
            ],
            &work,
            &index,
            timeout,
        )
        .await?;
        log.push_str(&checkout);
        let clean = run_git(
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
            &work,
            &index,
            timeout,
        )
        .await?;
        log.push_str(&clean);
        Ok(log)
    }

    /// Step 2: `bun install`, skipped when the dependency hash is unchanged.
    async fn install(&self, app: &App, deploy_id: &str) -> anyhow::Result<(bool, String)> {
        let work = self.work_dir(&app.slug);
        let pkg = work.join("package.json");
        if !pkg.is_file() {
            return Ok((true, "no package.json; skipping install".to_string()));
        }
        let pkg_bytes = std::fs::read(&pkg)?;
        let lock_path = work.join("bun.lock");
        let lock_bytes = if lock_path.is_file() {
            Some(std::fs::read(&lock_path)?)
        } else {
            None
        };
        let current = install_hash(&pkg_bytes, lock_bytes.as_deref());
        let hash_file = self
            .settings
            .data_dir
            .join("work")
            .join(format!("{}.install-hash", app.slug));
        let stored = std::fs::read_to_string(&hash_file)
            .ok()
            .map(|s| s.trim().to_string());
        if should_skip_install(
            stored.as_deref(),
            &current,
            work.join("node_modules").is_dir(),
        ) {
            return Ok((true, "dependencies unchanged; skipping install".to_string()));
        }

        let mut args = vec!["install".to_string()];
        if lock_bytes.is_some() {
            args.push("--frozen-lockfile".to_string());
        }
        let out = self
            .run_sandboxed_step(
                app,
                deploy_id,
                "install",
                &self.settings.bun_bin,
                &args,
                vec![],
            )
            .await?;
        std::fs::write(&hash_file, format!("{current}\n"))?;
        Ok((false, out))
    }

    /// Step 3: `bun run build` when `package.json` has `scripts.build`.
    async fn build(&self, app: &App, deploy_id: &str) -> anyhow::Result<(bool, String)> {
        let work = self.work_dir(&app.slug);
        let pkg = work.join("package.json");
        if !pkg.is_file() {
            return Ok((true, "no package.json; skipping build".to_string()));
        }
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&pkg)?)?;
        let has_build = parsed
            .get("scripts")
            .and_then(|s| s.get("build"))
            .and_then(|b| b.as_str())
            .is_some_and(|s| !s.trim().is_empty());
        if !has_build {
            return Ok((true, "no scripts.build; skipping build".to_string()));
        }
        let out = self
            .run_sandboxed_step(
                app,
                deploy_id,
                "build",
                &self.settings.bun_bin,
                &["run".to_string(), "build".to_string()],
                vec![],
            )
            .await?;
        Ok((false, out))
    }

    /// Step 4: find and validate the wrangler config. Returns the deploy root.
    fn deploy_root(&self, app: &App) -> anyhow::Result<(PathBuf, String)> {
        let work = self.work_dir(&app.slug);
        let config = find_deploy_config(&work).ok_or_else(|| {
            anyhow::anyhow!(
                "no wrangler config: looked for dist/wrangler.json, wrangler.jsonc, wrangler.json under {}",
                work.display()
            )
        })?;
        let raw = std::fs::read_to_string(&config)?;
        let parsed: serde_json::Value = if config.extension().is_some_and(|e| e == "jsonc") {
            serde_json::from_str(&strip_jsonc_comments(&raw))?
        } else {
            serde_json::from_str(&raw)?
        };
        validate_config_keys(&parsed)?;
        let root = config
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| work.clone());
        Ok((root.clone(), format!("deploy root {}\n", root.display())))
    }

    /// Step 5: `celld deploy <root> ... --json`; returns (output, version).
    /// Bucket credentials are minted fresh for the app's prefix on every
    /// deploy, so the step env never carries the parent keys.
    async fn celld_deploy(
        &self,
        app: &App,
        deploy_id: &str,
        root: &Path,
    ) -> anyhow::Result<(String, Option<String>)> {
        let prefix = FleetStorage::fleet_prefix(&app.slug);
        let creds = self
            .settings
            .storage
            .credentials
            .for_prefix(&prefix)
            .await
            .map_err(|err| {
                anyhow::anyhow!(
                    "minting scoped R2 credentials for deploy of {}: {err:#}",
                    app.slug
                )
            })?;
        let bucket = self.settings.storage.bucket_url(&app.slug);
        let endpoint = self.settings.storage.endpoint.clone();
        let region = self.settings.storage.region.clone();
        let mut env = vec![
            ("AWS_ACCESS_KEY_ID".to_string(), creds.access_key_id),
            ("AWS_SECRET_ACCESS_KEY".to_string(), creds.secret_access_key),
            ("AWS_REGION".to_string(), region.clone()),
            ("AWS_EC2_METADATA_DISABLED".to_string(), "true".to_string()),
            ("S3_ENDPOINT".to_string(), endpoint.clone()),
        ];
        if let Some(token) = &creds.session_token {
            env.push(("AWS_SESSION_TOKEN".to_string(), token.clone()));
        }
        if let Some(esbuild) = &self.settings.esbuild_bin {
            env.push(("CELLD_ESBUILD".to_string(), esbuild.clone()));
        }
        let args = vec![
            "deploy".to_string(),
            root.to_string_lossy().into_owned(),
            "--bucket".to_string(),
            bucket,
            "--endpoint".to_string(),
            endpoint,
            "--region".to_string(),
            region,
            "--json".to_string(),
        ];
        let out = self
            .run_sandboxed_step(
                app,
                deploy_id,
                "deploy",
                &self.settings.celld_bin,
                &args,
                env,
            )
            .await?;
        let parsed: serde_json::Value = serde_json::from_str(out.trim())
            .map_err(|e| anyhow::anyhow!("celld printed non-JSON output: {e}"))?;
        let version = parsed
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok((out, version))
    }

    /// Run one sandboxed step: the binaries come resolved from the config
    /// (absolute when found on PATH at load, raw names otherwise), and the
    /// step PATH is built from the bun parent dir so the sandboxed env
    /// (which starts from `env_clear`) finds the same toolchain.
    async fn run_sandboxed_step(
        &self,
        app: &App,
        deploy_id: &str,
        step: &str,
        bin: &str,
        args: &[String],
        extra_env: Vec<(String, String)>,
    ) -> anyhow::Result<String> {
        let program_str = bin.to_string();
        let parent = Path::new(&self.settings.bun_bin)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "/usr/local/bin".to_string());
        let mut env = Vec::with_capacity(extra_env.len() + 1);
        env.push((
            "PATH".to_string(),
            format!("{parent}:/usr/local/bin:/usr/bin:/bin"),
        ));
        env.extend(extra_env);
        let cache_root = self.cache_dir(&app.slug);
        let leaf = self.build_leaf(deploy_id, step)?;
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let sb = Sandbox {
            uid: Some(app.uid),
            gid: Some(app.uid),
            // Outer's own values (PATH, bucket credentials); no app secrets yet.
            platform_env: &env,
            env: &[],
            cache_root: &cache_root,
            cgroup_leaf: leaf.as_deref(),
        };
        sandbox::run_sandboxed(
            &program_str,
            &arg_refs,
            &self.work_dir(&app.slug),
            &sb,
            self.settings.step_timeout,
        )
        .await
    }

    /// A per-step build leaf under the shared parent, or `None` on dev.
    fn build_leaf(&self, deploy_id: &str, step: &str) -> anyhow::Result<Option<PathBuf>> {
        let Some(anchor) = &self.settings.cgroups else {
            return Ok(None);
        };
        let cfg = cgroups::CgroupConfig {
            build_parent: "builds".to_string(),
            ..cgroups::CgroupConfig::default()
        };
        let parent = cgroups::ensure_build_parent(
            anchor,
            &cfg,
            BUILD_PARENT_MEMORY_MAX,
            BUILD_PARENT_CPU_WEIGHT,
        )?;
        Ok(Some(cgroups::ensure_build_leaf(
            &parent,
            &format!("{deploy_id}-{step}"),
            BUILD_LEAF_MEMORY_MAX,
        )?))
    }
}

/// A step that failed: its name and the first meaningful output line.
struct StepFailed {
    step: &'static str,
    error: String,
}

impl StepFailed {
    /// For `run_sandboxed` errors: skip the `prog [args] exit ..` header the
    /// runner prepends and take the first output line.
    fn sandboxed(step: &'static str, e: &anyhow::Error) -> Self {
        let text = format!("{e:#}");
        let mut lines = text
            .lines()
            .skip(1)
            .map(str::trim)
            .filter(|l| !l.is_empty());
        let error = lines.next().unwrap_or(text.trim());
        Self {
            step,
            error: cap_error(error),
        }
    }

    /// For outer-run steps (git, file reads): the message's first line.
    fn plain(step: &'static str, e: &anyhow::Error) -> Self {
        let text = format!("{e:#}");
        let error = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        Self {
            step,
            error: cap_error(error),
        }
    }
}

fn cap_error(s: &str) -> String {
    let t = s.trim();
    if t.len() <= ERROR_CAP {
        return t.to_string();
    }
    let mut end = ERROR_CAP;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &t[..end])
}

/// Record a finished step and append its output to the combined log.
fn push_step(
    steps: &mut Vec<Step>,
    log: &mut String,
    name: &str,
    started: Instant,
    skipped: bool,
    out: &str,
) -> u64 {
    let ms = started.elapsed().as_millis() as u64;
    steps.push(Step {
        name: name.to_string(),
        ms,
        skipped,
    });
    log.push_str(&format!("### {name} ({ms}ms)\n"));
    log.push_str(out);
    if !out.ends_with('\n') {
        log.push('\n');
    }
    ms
}

fn steps_json(steps: &[Step]) -> String {
    serde_json::to_string(steps).unwrap_or_else(|_| "[]".to_string())
}

/// Keep the last 64 KiB of the combined log, cut at a UTF-8 boundary.
fn tail_cap(log: &str) -> String {
    if log.len() <= LOG_TAIL_CAP {
        return log.to_string();
    }
    let mut start = log.len() - LOG_TAIL_CAP;
    while !log.is_char_boundary(start) {
        start += 1;
    }
    log[start..].to_string()
}

/// sha256 over `package.json` bytes plus `bun.lock` bytes when present.
fn install_hash(pkg: &[u8], lock: Option<&[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(pkg);
    if let Some(lock) = lock {
        hasher.update(lock);
    }
    hex::encode(hasher.finalize())
}

/// Skip install only when the hash matches the last successful install AND
/// `node_modules` survived (a wipe or a new worktree reinstalls).
fn should_skip_install(stored: Option<&str>, current: &str, node_modules: bool) -> bool {
    stored.is_some_and(|s| s == current) && node_modules
}

/// Deploy-root discovery order: `dist/wrangler.json`, else root
/// `wrangler.jsonc`, else root `wrangler.json`.
fn find_deploy_config(work: &Path) -> Option<PathBuf> {
    [
        work.join("dist").join("wrangler.json"),
        work.join("wrangler.jsonc"),
        work.join("wrangler.json"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

/// Step timings and the combined output, filled in as the pipeline runs.
#[derive(Default)]
struct Journal {
    steps: Vec<Step>,
    log: String,
}

fn validate_config_keys(value: &serde_json::Value) -> anyhow::Result<()> {
    let Some(obj) = value.as_object() else {
        anyhow::bail!("wrangler config must be a JSON object");
    };
    for key in obj.keys() {
        if !ACCEPTED_CONFIG_KEYS.contains(&key.as_str()) {
            anyhow::bail!("unknown wrangler key {key:?}");
        }
    }
    Ok(())
}

/// Strip `//` and `/* */` comments from `.jsonc`, leaving strings (which may
/// contain `//` or `/* */`) byte-identical. The result parses as JSON.
fn strip_jsonc_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    for c2 in chars.by_ref() {
                        if c2 == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut prev_star = false;
                    for c2 in chars.by_ref() {
                        if prev_star && c2 == '/' {
                            break;
                        }
                        prev_star = c2 == '*';
                    }
                    // Outside a string this is only whitespace; keep one so
                    // tokens on either side cannot join.
                    out.push(' ');
                }
                _ => out.push(c),
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Run git with the sidecar index, capturing both streams; non-zero exits and
/// timeouts are errors carrying the output.
async fn run_git(
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
    // kill_on_drop: a timeout drops the wait future and with it the child.
    let child = cmd.spawn()?;
    let result = tokio::time::timeout(timeout, child.wait_with_output()).await;
    match result {
        Ok(Ok(output)) => {
            let mut log = String::from_utf8_lossy(&output.stdout).into_owned();
            log.push_str(&String::from_utf8_lossy(&output.stderr));
            if !output.status.success() {
                anyhow::bail!("git {args:?} exit {:?}\n{log}", output.status.code());
            }
            Ok(log)
        }
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(anyhow::anyhow!("git {args:?} timed out after {timeout:?}")),
    }
}

/// Hand the worktree to the app uid before sandboxed steps run: a root-run
/// deploy materializes files as outer, and `bun install` / `bun run build`
/// write into the tree as the app uid. Skipped when not root: an
/// unprivileged outer cannot chown, and the steps then run as the current
/// user anyway.
fn hand_worktree_to_app(slug: &str, work: &Path, uid: u32) {
    #[cfg(unix)]
    {
        if unsafe { libc::geteuid() } != 0 {
            tracing::debug!(slug, uid, "not root; leaving the worktree outer-owned");
            return;
        }
        if !chown_tree(work, uid, uid) {
            tracing::warn!(
                slug,
                "chown worktree to app uid failed; continuing (needs CAP_CHOWN)"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (slug, work, uid);
    }
}

/// Recursively hand `path` to (`uid`, `gid`) without following symlinks (cf.
/// Noite's `lchown_tree`): an app-writable tree can link anywhere, and a
/// following chown run by root would hand platform files to the app uid.
/// `symlink_metadata` reports a link as a link, so `lchown` changes the link
/// itself; links are never descended into and paths are never canonicalized.
/// Returns false when any entry could not be changed.
#[cfg(unix)]
fn chown_tree(path: &Path, uid: u32, gid: u32) -> bool {
    chown_tree_with(path, uid, gid, &|p, uid, gid| {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = CString::new(p.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `lchown` changes the link itself; it never follows it.
        unsafe { libc::lchown(path.as_ptr(), uid, gid) == 0 }
    })
}

/// [`chown_tree`] with the per-entry change injected, so tests can record
/// the visited paths without privileges.
#[cfg(unix)]
fn chown_tree_with(
    path: &Path,
    uid: u32,
    gid: u32,
    chown: &dyn Fn(&Path, u32, u32) -> bool,
) -> bool {
    let mut ok = true;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            ok = false;
            continue;
        };
        if !chown(&p, uid, gid) {
            ok = false;
        }
        if meta.file_type().is_dir() {
            let Ok(rd) = std::fs::read_dir(&p) else {
                ok = false;
                continue;
            };
            for entry in rd.flatten() {
                stack.push(entry.path());
            }
        }
    }
    ok
}

/// Mark the deploy `deploying` before `celld deploy` runs. A separate
/// function so the `?` maps to a bookkeeping error, not a step failure.
async fn set_status_blocking(pool: &SqlitePool, deploy_id: &str) -> Result<(), StepFailed> {
    set_deploy_status(pool, deploy_id, "deploying")
        .await
        .map_err(|e| StepFailed::plain("deploy", &e))
}

async fn insert_deploy_row(
    pool: &SqlitePool,
    deploy_id: &str,
    app_id: &str,
    sha: &str,
    now: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO deploy (id, app_id, sha, status, created_at) VALUES (?, ?, ?, 'queued', ?)",
    )
    .bind(deploy_id)
    .bind(app_id)
    .bind(sha)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

async fn set_deploy_status(pool: &SqlitePool, deploy_id: &str, status: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE deploy SET status = ? WHERE id = ?")
        .bind(status)
        .bind(deploy_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn finish_success(
    pool: &SqlitePool,
    deploy_id: &str,
    steps_json: &str,
    log_tail: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE deploy SET status = 'success', steps = ?, log_tail = ?, finished_at = ? WHERE id = ?",
    )
    .bind(steps_json)
    .bind(log_tail)
    .bind(apps::now_ms())
    .bind(deploy_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn finish_failed(
    pool: &SqlitePool,
    deploy_id: &str,
    error: &str,
    steps_json: &str,
    log_tail: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE deploy SET status = 'failed', error = ?, steps = ?, log_tail = ?, finished_at = ? WHERE id = ?",
    )
    .bind(error)
    .bind(steps_json)
    .bind(log_tail)
    .bind(apps::now_ms())
    .bind(deploy_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::commit::{self, Author, CommitRequest, FileWrite, WriteBody};
    use crate::git::manifest::{self, GitSync};
    use crate::store::Store;

    #[test]
    fn jsonc_comment_stripping_leaves_strings_alone() {
        let src = r#"{
  // a line comment
  "url": "https://example.com//path", /* block */
  "glob": "a/* */b", // trailing
  "name": "x" /* unterminated-tail is fine here */
}"#;
        let stripped = strip_jsonc_comments(src);
        let parsed: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(parsed["url"], "https://example.com//path");
        assert_eq!(parsed["glob"], "a/* */b");
        assert_eq!(parsed["name"], "x");
    }

    #[test]
    fn jsonc_block_comments_do_not_join_tokens() {
        let src = r#"{"a":1/*sep*/,"b":[1,/*x*/2]}// eof without newline"#;
        let parsed: serde_json::Value = serde_json::from_str(&strip_jsonc_comments(src)).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], serde_json::json!([1, 2]));
    }

    #[test]
    fn unknown_wrangler_key_fails_naming_it() {
        let bad = serde_json::json!({"name": "app", "main": "index.js", "bogus_key": true});
        let err = validate_config_keys(&bad).unwrap_err();
        assert!(
            format!("{err:#}").contains("bogus_key"),
            "error names the key: {err:#}"
        );
    }

    #[test]
    fn template_shaped_config_passes_key_validation() {
        // The shape the Ilha + Void template emits (brief facts): deploy root
        // `dist/`, `main: ssr/index.js`, client assets.
        let config = serde_json::json!({
            "name": "noite-app",
            "main": "ssr/index.js",
            "compatibility_date": "2026-01-01",
            "assets": {"directory": "./client"},
        });
        validate_config_keys(&config).unwrap();
        let full = serde_json::json!({
            "$schema": "", "name": "", "main": "", "no_bundle": false,
            "compatibility_date": "", "compatibility_flags": [],
            "durable_objects": {}, "migrations": [], "assets": {},
            "services": [], "triggers": {}, "vars": {},
            "d1_databases": [], "kv_namespaces": [], "queues": [],
            "workflows": [], "r2_buckets": [], "worker_loaders": [],
            "containers": [], "define": {}, "rules": [],
        });
        validate_config_keys(&full).unwrap();
    }

    #[test]
    fn deploy_root_prefers_dist_then_jsonc_then_json() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path();
        assert!(find_deploy_config(work).is_none());

        std::fs::write(work.join("wrangler.json"), "{}").unwrap();
        assert_eq!(
            find_deploy_config(work).unwrap(),
            work.join("wrangler.json")
        );

        std::fs::write(work.join("wrangler.jsonc"), "{}").unwrap();
        assert_eq!(
            find_deploy_config(work).unwrap(),
            work.join("wrangler.jsonc")
        );

        std::fs::create_dir_all(work.join("dist")).unwrap();
        std::fs::write(work.join("dist").join("wrangler.json"), "{}").unwrap();
        assert_eq!(
            find_deploy_config(work).unwrap(),
            work.join("dist").join("wrangler.json")
        );
    }

    #[test]
    fn install_skip_needs_same_hash_and_node_modules() {
        assert!(should_skip_install(Some("abc"), "abc", true));
        assert!(
            !should_skip_install(Some("abc"), "def", true),
            "hash change reinstalls"
        );
        assert!(
            !should_skip_install(Some("abc"), "abc", false),
            "a missing node_modules reinstalls"
        );
        assert!(
            !should_skip_install(None, "abc", true),
            "no stored hash installs"
        );
    }

    #[test]
    fn install_hash_covers_package_json_and_lockfile() {
        let a = install_hash(b"{}", Some(b"lock"));
        let no_lock = install_hash(b"{}", None);
        assert_ne!(a, no_lock);
        assert_ne!(a, install_hash(b"{} ", Some(b"lock")));
        assert_ne!(a, install_hash(b"{}", Some(b"lock2")));
        assert_eq!(a, install_hash(b"{}", Some(b"lock")));
    }

    #[test]
    fn log_tail_caps_at_64kib_on_a_char_boundary() {
        let big = format!("{}✓", "x".repeat(100 * 1024));
        let tail = tail_cap(&big);
        assert!(tail.len() <= LOG_TAIL_CAP);
        assert!(tail.ends_with("✓"));
        assert_eq!(
            tail.chars().count(),
            tail.len() - "✓".len() + 1,
            "no split char"
        );
        assert_eq!(tail_cap("short"), "short");
    }

    fn author() -> Author {
        Author {
            name: "Agent".to_string(),
            email: "agent@example.com".to_string(),
        }
    }

    fn write(path: &str, content: &str) -> FileWrite {
        FileWrite {
            path: path.to_string(),
            body: WriteBody::Text(content.to_string()),
        }
    }

    async fn seed_mirror(
        slug: &str,
    ) -> (tempfile::TempDir, GitConfig, Store, GitSync, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig {
            repos_dir: dir.path().join("git"),
        };
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, slug)
            .await
            .unwrap();

        let seed = CommitRequest {
            message: "seed".to_string(),
            base: None,
            writes: vec![
                write("index.ts", "v1"),
                write("old.ts", "remove me"),
                write("sub/keep.ts", "keep"),
            ],
            edits: vec![],
            deletes: vec![],
            moves: vec![],
            author: author(),
        };
        let sha_a = commit::commit(&store, &cfg, &sync, slug, seed)
            .await
            .unwrap();

        let second = CommitRequest {
            message: "second".to_string(),
            base: Some(sha_a.clone()),
            writes: vec![write("index.ts", "v2"), write("new.ts", "new")],
            edits: vec![],
            deletes: vec!["old.ts".to_string()],
            moves: vec![],
            author: author(),
        };
        let sha_b = commit::commit(&store, &cfg, &sync, slug, second)
            .await
            .unwrap();
        (dir, cfg, store, sync, sha_a, sha_b)
    }

    fn test_settings(data_dir: PathBuf) -> DeploySettings {
        DeploySettings {
            data_dir,
            celld_bin: "celld".to_string(),
            esbuild_bin: None,
            bun_bin: "bun".to_string(),
            storage: FleetStorage {
                endpoint: "http://127.0.0.1:19000".to_string(),
                region: "us-east-1".to_string(),
                bucket: "outer-live-test".to_string(),
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: "outeraccess".to_string(),
                    secret_access_key: "outersecretoutersecretoutersecret12".to_string(),
                },
            },
            step_timeout: Duration::from_secs(120),
            cgroups: None,
        }
    }

    fn test_app(uid: u32) -> App {
        App {
            id: "app1".to_string(),
            slug: "grocery".to_string(),
            owner_id: "owner1".to_string(),
            name: None,
            uid,
            port: 18001,
            internal_port: 18002,
            visibility: crate::apps::Visibility::Unlisted,
            asleep_since: None,
            last_request_at: None,
            last_deploy_sha: None,
            created_at: 0,
        }
    }

    #[tokio::test]
    async fn worktree_materialization_tracks_the_tree_but_keeps_node_modules() {
        let (_dir, cfg, _store, _sync, sha_a, sha_b) = seed_mirror("grocery").await;
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 1);
        let app = test_app(10000);

        deployer.materialize(&cfg, &app, &sha_a).await.unwrap();
        let work = deployer.work_dir(&app.slug);
        assert_eq!(
            std::fs::read_to_string(work.join("index.ts")).unwrap(),
            "v1"
        );

        // Untracked build residue plus the one directory clean must spare.
        std::fs::create_dir_all(work.join("node_modules")).unwrap();
        std::fs::write(work.join("node_modules").join("keepme.js"), "x").unwrap();
        std::fs::write(work.join("old.ts"), "stale overwrite").unwrap();

        deployer.materialize(&cfg, &app, &sha_b).await.unwrap();
        assert!(!work.join("old.ts").exists(), "a file deleted in B is gone");
        assert_eq!(
            std::fs::read_to_string(work.join("index.ts")).unwrap(),
            "v2"
        );
        assert_eq!(std::fs::read_to_string(work.join("new.ts")).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(work.join("node_modules").join("keepme.js")).unwrap(),
            "x",
            "node_modules survives clean"
        );

        // The worktree equals B's tree exactly, except node_modules.
        let bare = manifest::mirror_path(&cfg, &app.slug);
        let git_dir = format!("--git-dir={}", bare.display());
        let listed = manifest::run_git(
            &[&git_dir, "ls-tree", "-r", "--name-only", &sha_b],
            &[],
            Duration::from_secs(15),
        )
        .await
        .unwrap();
        let mut expected: Vec<String> = listed.lines().map(str::to_string).collect();
        expected.sort();
        let mut actual: Vec<String> = Vec::new();
        for entry in walkdir_files(&work) {
            if entry.starts_with("node_modules/") {
                continue;
            }
            actual.push(entry);
        }
        actual.sort();
        assert_eq!(actual, expected);
    }

    fn walkdir_files(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        out
    }

    async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query("INSERT INTO account (id, email, last_active_at, created_at) VALUES ('owner1', 'o@example.com', 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        (dir, pool)
    }

    #[tokio::test]
    async fn failed_deploy_writes_row_and_leaves_last_deploy_sha() {
        let (_mdir, cfg, _store, _sync, _sha_a, sha_b) = seed_mirror("grocery").await;
        let (_ddir, pool) = test_pool().await;
        let app = test_app(10001);
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) VALUES (?, ?, ?, ?, ?, ?, 0, 0)",
        )
        .bind(&app.id)
        .bind(&app.slug)
        .bind(&app.owner_id)
        .bind(app.uid as i64)
        .bind(app.port as i64)
        .bind(app.internal_port as i64)
        .execute(&pool)
        .await
        .unwrap();

        // No package.json, no wrangler config: fails at deploy-root.
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 2);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha_b).await.unwrap();
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failed_step.as_deref(), Some("deploy-root"));
        assert!(
            outcome.error.as_deref().unwrap_or("").contains("wrangler"),
            "error names the problem: {:?}",
            outcome.error
        );
        let names: Vec<&str> = outcome.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["worktree", "install", "build", "deploy-root"]);
        assert!(!outcome.steps[0].skipped, "worktree always runs");
        assert!(
            outcome.steps[1..3].iter().all(|s| s.skipped),
            "no package.json skips install and build: {:?}",
            outcome.steps
        );

        let row: (String, Option<String>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT status, error, steps, log_tail FROM deploy WHERE app_id = ?")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "failed");
        assert!(row.1.as_deref().unwrap_or("").contains("wrangler"));
        let steps: Vec<serde_json::Value> =
            serde_json::from_str(row.2.as_deref().unwrap_or("[]")).unwrap();
        assert_eq!(steps.len(), 4, "the failed step is the last steps entry");
        assert!(row.3.unwrap_or_default().contains("deploy-root"));

        let (last_sha,): (Option<String>,) =
            sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = ?")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            last_sha, None,
            "a failed deploy never touches last_deploy_sha"
        );
    }

    #[tokio::test]
    async fn queued_row_is_visible_while_waiting_for_permits() {
        let (_mdir, cfg, _store, _sync, _sha_a, sha_b) = seed_mirror("grocery").await;
        let (_ddir, pool) = test_pool().await;
        let app = test_app(10002);
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) VALUES ('app1', 'grocery', 'owner1', 10002, 18003, 18004, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Zero permits: the deploy blocks before `building`, and the row is
        // already there as `queued`.
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 0);
        let pool2 = pool.clone();
        let handle = tokio::spawn(async move { deployer.deploy(&pool2, &cfg, &app, &sha_b).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (status,): (String,) =
            sqlx::query_as("SELECT status FROM deploy WHERE app_id = 'app1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "queued");
        handle.abort();
    }

    #[tokio::test]
    #[ignore = "needs bun and celld"]
    async fn end_to_end_synthetic_app_against_rustfs() {
        let endpoint =
            std::env::var("OUTER_TEST_S3_ENDPOINT").expect("OUTER_TEST_S3_ENDPOINT must be set");
        let key = std::env::var("OUTER_TEST_S3_KEY").expect("OUTER_TEST_S3_KEY must be set");
        let secret =
            std::env::var("OUTER_TEST_S3_SECRET").expect("OUTER_TEST_S3_SECRET must be set");
        let bucket =
            std::env::var("OUTER_TEST_S3_BUCKET").unwrap_or_else(|_| "outer-live-test".to_string());
        let search = std::env::var("PATH").unwrap_or_default();
        for bin in ["bun", "celld"] {
            assert!(
                std::env::split_paths(&search).any(|dir| dir.join(bin).is_file()),
                "{bin} must be on PATH for the live deploy test"
            );
        }

        // A minimal app: install is a no-op (no deps), build copies the root
        // wrangler.json into dist/, celld deploys dist/ to RustFS.
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("package.json"),
            r#"{"name":"e2e","scripts":{"build":"mkdir -p dist && cp wrangler.json dist/wrangler.json"}}"#,
        )
        .unwrap();
        std::fs::write(
            src.path().join("wrangler.json"),
            r#"{"name":"e2e","main":"index.js","compatibility_date":"2026-01-01"}"#,
        )
        .unwrap();
        std::fs::write(
            src.path().join("index.js"),
            "export default {fetch: () => new Response('hi')};",
        )
        .unwrap();

        let slug = "e2e";
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig {
            repos_dir: dir.path().join("git"),
        };
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, slug)
            .await
            .unwrap();
        let sha = commit::import_dir(&store, &cfg, &sync, slug, src.path(), "seed", &author())
            .await
            .unwrap();

        let (_ddir, pool) = test_pool().await;
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) VALUES ('app1', 'e2e', 'owner1', 10003, 18005, 18006, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let app = App {
            id: "app1".to_string(),
            slug: slug.to_string(),
            owner_id: "owner1".to_string(),
            name: None,
            uid: 10003,
            port: 18005,
            internal_port: 18006,
            visibility: crate::apps::Visibility::Unlisted,
            asleep_since: None,
            last_request_at: None,
            last_deploy_sha: None,
            created_at: 0,
        };

        let data = tempfile::tempdir().unwrap();
        let settings = DeploySettings {
            data_dir: data.path().to_path_buf(),
            celld_bin: "celld".to_string(),
            esbuild_bin: std::env::var("CELLD_ESBUILD").ok(),
            bun_bin: "bun".to_string(),
            storage: FleetStorage {
                endpoint,
                region: "us-east-1".to_string(),
                bucket,
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: key,
                    secret_access_key: secret,
                },
            },
            step_timeout: Duration::from_secs(120),
            cgroups: None,
        };
        let deployer = Deployer::new(settings, 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "success", "error: {:?}", outcome.error);
        assert!(
            outcome.version.is_some(),
            "celld version parsed from --json output"
        );
        let (last_sha,): (Option<String>,) =
            sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = 'app1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(last_sha.as_deref(), Some(sha.as_str()));
    }

    /// A tree with a file, a real dir, a link to an outside file and a link
    /// to an outside dir. Both tempdirs stay alive in the return value so
    /// the links never dangle.
    #[cfg(unix)]
    fn symlink_tree() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target.txt");
        std::fs::write(&target, "outside").unwrap();
        let sub = outside.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), "inner").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file.txt"), "file").unwrap();
        std::fs::create_dir(root.join("realdir")).unwrap();
        symlink(&target, root.join("link")).unwrap();
        symlink(&sub, root.join("dirlink")).unwrap();
        (dir, outside, root, target)
    }

    #[test]
    #[cfg(unix)]
    fn chown_tree_hands_over_links_without_descending_into_them() {
        use std::cell::RefCell;
        let (_dir, _outside, root, _target) = symlink_tree();
        let visited = RefCell::new(Vec::new());
        let ok = chown_tree_with(&root, 1000, 1000, &|p, uid, gid| {
            assert_eq!((uid, gid), (1000, 1000));
            visited.borrow_mut().push(p.to_path_buf());
            true
        });
        assert!(ok);
        let visited = visited.borrow();
        for expected in [
            root.clone(),
            root.join("file.txt"),
            root.join("realdir"),
            root.join("link"),
            root.join("dirlink"),
        ] {
            assert!(
                visited.contains(&expected),
                "the link itself is handed over: {expected:?} not in {visited:?}"
            );
        }
        assert_eq!(
            visited.len(),
            5,
            "no traversal outside the tree: {visited:?}"
        );
    }

    /// Another group of the current user, if any: an owner may hand entries
    /// to a member group without privilege, which makes a real handover
    /// observable without root.
    #[cfg(unix)]
    fn alternate_member_group() -> Option<u32> {
        let mine = unsafe { libc::getegid() };
        let mut buf = [0 as libc::gid_t; 64];
        let n = unsafe { libc::getgroups(buf.len() as libc::c_int, buf.as_mut_ptr()) };
        if n < 0 {
            return None;
        }
        buf[..n as usize].iter().copied().find(|g| *g != mine)
    }

    #[test]
    #[cfg(unix)]
    fn chown_tree_moves_the_link_itself_leaving_its_target_alone() {
        use std::os::unix::fs::MetadataExt;
        let (_dir, _outside, root, target) = symlink_tree();
        let me = unsafe { libc::geteuid() };
        let mine = unsafe { libc::getegid() };
        let Some(alt) = alternate_member_group() else {
            // One group only: nothing observable without privilege beyond
            // the recorder test above; the real call must still leave the
            // tree structurally intact.
            chown_tree(&root, me, mine);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "outside");
            return;
        };
        // Park every in-tree entry and the outside target on the alternate
        // group, so the handover back is observable.
        let lchown = |p: &std::path::Path| {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let c = CString::new(p.as_os_str().as_bytes()).unwrap();
            assert!(
                unsafe { libc::lchown(c.as_ptr(), me, alt) } == 0,
                "setup chown of {p:?} failed"
            );
        };
        for rel in ["", "file.txt", "realdir", "link", "dirlink"] {
            lchown(&root.join(rel));
        }
        lchown(&target);
        assert!(chown_tree(&root, me, mine), "handover back must succeed");
        for rel in ["", "file.txt", "realdir", "link", "dirlink"] {
            let meta = std::fs::symlink_metadata(root.join(rel)).unwrap();
            assert_eq!(
                (meta.uid(), meta.gid()),
                (me, mine),
                "{rel:?} was handed over"
            );
        }
        // The link is still a link to the same target, and the target kept
        // the alternate group: never followed, never touched.
        assert_eq!(std::fs::read_link(root.join("link")).unwrap(), target);
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(!meta.file_type().is_symlink());
        assert_eq!((meta.uid(), meta.gid()), (me, alt));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "outside");
    }
}
