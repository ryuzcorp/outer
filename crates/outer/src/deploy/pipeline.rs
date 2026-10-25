//! The deploy pipeline (spec §10.1, §10.2; `crate::deploy` contract).
//!
//! `Deployer::deploy` materializes `sha` from the bare mirror into
//! `{data}/work/{slug}`, runs `bun install` (skipped when the lockfile hash
//! is unchanged) and `bun run build`, validates the wrangler config at the
//! deploy root, applies pending D1 migrations and uploads with
//! `celld deploy`, and reloads the fleet. Install, build, deploy and
//! migrate run sandboxed as the app's uid through `run_sandboxed`; the
//! worktree and deploy-root steps run as outer (they only touch git
//! metadata and read files).
//!
//! Concurrency: a global semaphore bounds simultaneous builds and a per-app
//! mutex serializes deploys of the same app. The `deploy` row is inserted as
//! are held, and to `deploying` before the migrate/upload steps run. The
//! migrate step runs under the same `deploying` status, before the upload
//! for an existing app and after it for a first deploy: a blocked deploy
//! (gate) never touches app data, a failed migration on an existing app
//! leaves the previous version serving, and a failed first-deploy migration
//! leaves the app not-live.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::apps::{self, App};
use crate::cron;
use crate::deploy::migrate;
use crate::fleet::cgroups;
use crate::fleet::manager::FleetStorage;
use crate::fleet::sandbox::{self, Sandbox};
use crate::git::manifest::{self, GitConfig};
use crate::moderation::gate;
use crate::moderation::jev::JevClient;

/// What the pipeline needs: dirs, tool paths, storage creds, timeouts.
#[derive(Clone)]
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
    /// Apps live at `<slug>.<apps_domain>`; the purge targets that host.
    pub apps_domain: String,
    /// Edge cache purge after a successful deploy. `None` skips the purge
    /// at debug (localhost dev without `CLOUDFLARE_ZONE_ID`).
    pub cache_purge: Option<crate::cloudflare::CachePurge>,
    /// Jev classification for the pre-live gate. `None` only on localhost
    /// dev without the Cloudflare pair: the gate logs a warning and passes.
    /// Off localhost the config refuses to boot without the pair, so the
    /// gate always classifies there.
    pub jev: Option<JevClient>,
    /// Wakes a sleeping fleet before the migrate step applies D1 migrations:
    /// `celld d1` reaches the database through a running node. `Some` in
    /// `serve` (the fleet manager lives there); `None` in the operator CLI
    /// and in tests with a fake `celld`, where no fleet can be woken and the
    /// apply fails loudly when no fleet serves the app.
    pub wake: Option<crate::fleet::manager::FleetManager>,
    /// AES-256-GCM key for app secrets, derived from `OUTER_SECRET` at boot.
    /// Secrets are decrypted only for the `celld deploy` step and injected
    /// into the deploy-root config `vars`; builds never see them.
    pub secrets_key: crate::secrets::SecretsKey,
}

/// One pipeline step's timing for the `deploy` row's `steps` JSON.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Step {
    pub name: String,
    pub ms: u64,
    pub skipped: bool,
}

/// The deploy result returned to the caller (and mirrored into the row).
/// `status` is `success`, `failed` or `blocked`. A blocked deploy carries
/// `reason` (`profanity` | `too_large_to_review`), `findings` and a
/// `user_message` written for the human; the CLI prints the whole outcome
/// as JSON.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeployOutcome {
    pub deploy_id: String,
    pub sha: String,
    pub status: &'static str,
    pub failed_step: Option<String>,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<crate::moderation::gate::Finding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_message: Option<String>,
    pub steps: Vec<Step>,
    pub log_tail: String,
    pub version: Option<String>,
    /// An awake fleet acknowledged the reload (success only). A sleeping
    /// fleet adopts the deploy when it wakes, so `false` is not a failure.
    #[serde(skip)]
    pub reloaded: bool,
}

/// What the deploy-root step selected: the directory celld deploys and
/// reads migrations from, plus the exact wrangler file that won discovery
/// (`dist/wrangler.json`, else `wrangler.jsonc`, else `wrangler.json`).
/// The migrate step takes this instead of re-deciding, so upload and D1
/// detection always read the same config.
#[derive(Debug)]
pub struct DeployRoot {
    pub root: PathBuf,
    pub config: PathBuf,
}
/// The patched deploy-root config while its `vars` hold plaintext: the
/// original bytes plus their mode, for a byte-identical restore once the
/// upload has the file. Plaintext lives only between `inject_secrets` and
/// the guard's drop, plus the uploaded copy under `fleets/<slug>/`, where
/// celld needs it to serve `env.*`.
///
/// RAII: [`Drop`] restores best-effort (warn, never panic), so a panic or
/// an early return in the upload window still scrubs the worktree. A
/// crash between processes is covered by [`scrub_interrupted_injections`],
/// which replays the sidecar stashed before the inject.
struct InjectedSecrets {
    path: PathBuf,
    original: Vec<u8>,
    mode: u32,
    injected: usize,
    overwritten: Vec<String>,
    /// The crash-recovery sidecar (`<slug>.orig` + `<slug>.target` under
    /// `{data}/secret-sidecars/`), removed once the config is restored.
    sidecar_orig: Option<PathBuf>,
    sidecar_target: Option<PathBuf>,
}
impl InjectedSecrets {
    /// One journal line: counts and overwritten names only, never values.
    fn journal_line(&self) -> String {
        if self.overwritten.is_empty() {
            format!("secrets: injected {} vars\n", self.injected)
        } else {
            format!(
                "secrets: injected {} vars (overwrote code-defined {})\n",
                self.injected,
                self.overwritten.join(", ")
            )
        }
    }
    /// Byte-identical restore, pass or fail. Best-effort: a restore
    /// failure warns but must not fail a deploy whose upload succeeded.
    /// Never panics, so [`Drop`] can call it on the unwind path.
    fn restore(&self) {
        if let Err(e) = atomic_replace_no_follow(&self.path, &self.original, self.mode) {
            tracing::warn!(path = %self.path.display(), error = %format!("{e:#}"), "restoring deploy config after secrets injection");
            return;
        }
        for sidecar in [&self.sidecar_orig, &self.sidecar_target]
            .into_iter()
            .flatten()
        {
            if let Err(e) = std::fs::remove_file(sidecar) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %sidecar.display(), error = %format!("{e:#}"), "removing secrets sidecar after restore");
                }
            }
        }
    }
}
impl Drop for InjectedSecrets {
    fn drop(&mut self) {
        self.restore();
    }
}
/// Refuse when any existing component of `path` is a symlink: the build
/// runs as the app uid and can plant links anywhere under the worktree, so
/// a deploy step running as outer/root must never resolve through one.
/// Missing trailing components are fine (a not-yet-created file has no
/// link to follow); any other stat failure fails closed.
fn refuse_symlink_components(path: &Path) -> anyhow::Result<()> {
    let mut cursor = PathBuf::new();
    for component in path.components() {
        cursor.push(component.as_os_str());
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                anyhow::bail!(
                    "refusing {}: {} is a symlink",
                    path.display(),
                    cursor.display()
                );
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                anyhow::bail!("cannot stat {}: {e:#}", cursor.display(),);
            }
        }
    }
    Ok(())
}
/// Read a file without following a trailing symlink (fails when the final
/// component is a link), returning its bytes and mode. Component symlinks
/// are refused first, so a planted `dist -> elsewhere` fails closed too.
fn read_no_follow(path: &Path) -> anyhow::Result<(Vec<u8>, u32)> {
    use std::os::unix::fs::OpenOptionsExt;
    refuse_symlink_components(path)?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e:#}", path.display()))?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        file.metadata()
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0o644)
    };
    #[cfg(not(unix))]
    let mode = 0o644;
    let mut bytes = Vec::new();
    use std::io::Read as _;
    file.read_to_end(&mut bytes)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e:#}", path.display()))?;
    Ok((bytes, mode))
}
/// Replace `path` atomically without following a trailing symlink: the new
/// bytes land in a fresh `O_CREAT|O_EXCL|O_NOFOLLOW` temp file (mode 0600
/// from the start, then `fchmod`ed to `mode` on its fd — never a
/// write-then-chmod of the target) and `rename` swaps it over the
/// original, which never resolves the final component through a link.
fn atomic_replace_no_follow(path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    refuse_symlink_components(path)?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cannot replace {}: no parent dir", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("cannot replace {}: no file name", path.display()))?;
    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut tmp: Option<(PathBuf, std::fs::File)> = None;
    for _ in 0..8 {
        let n = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = dir.join(format!(
            ".{}.outer-{}-{}-{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            n,
            crate::apps::now_ms()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&candidate)
        {
            Ok(file) => {
                tmp = Some((candidate, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                anyhow::bail!("cannot stage replacement for {}: {e:#}", path.display());
            }
        }
    }
    let (tmp_path, tmp_file) = tmp.ok_or_else(|| {
        anyhow::anyhow!(
            "cannot stage replacement for {}: temp names busy",
            path.display()
        )
    })?;
    let failed = |e: anyhow::Error| {
        let _ = std::fs::remove_file(&tmp_path);
        e
    };
    use std::io::Write as _;
    if let Err(e) = (&tmp_file)
        .write_all(bytes)
        .and_then(|()| tmp_file.sync_all())
    {
        return Err(failed(anyhow::anyhow!(
            "cannot write replacement for {}: {e:#}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // `fchmod` on the open fd: the target path is never chmod-ed
        // through a possibly swapped link, and umask never interferes.
        let rc = unsafe { libc::fchmod(tmp_file.as_raw_fd(), mode & 0o7777) };
        if rc != 0 {
            return Err(failed(anyhow::anyhow!(
                "cannot set mode on replacement for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        } // Keep the original's owner: `celld deploy` runs as the app uid and
          // must still read the file, while Outer (root in production) created
          // the temp. `fchown` on the fd, never through the path.
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            use std::os::unix::fs::MetadataExt;
            let rc = unsafe { libc::fchown(tmp_file.as_raw_fd(), meta.uid(), meta.gid()) };
            if rc != 0 {
                return Err(failed(anyhow::anyhow!(
                    "cannot set owner on replacement for {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                )));
            }
        }
    }
    // `rename` swaps atomically and never follows the final component:
    // a link re-planted at `path` is replaced, not written through.
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        return Err(failed(anyhow::anyhow!(
            "cannot replace {}: {e:#}",
            path.display()
        )));
    }
    Ok(())
}
/// Which fleet a pipeline run serves: the live app, or its Pro preview.
/// The kind rides the `deploy` row (`kind = 'main' | 'preview'`) so live
/// history queries never see preview runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployKind {
    Main,
    Preview,
}

impl DeployKind {
    pub fn sql(self) -> &'static str {
        match self {
            DeployKind::Main => "main",
            DeployKind::Preview => "preview",
        }
    }

    pub fn is_preview(self) -> bool {
        matches!(self, DeployKind::Preview)
    }
}
pub struct Deployer {
    settings: DeploySettings,
    semaphore: Arc<Semaphore>,
    app_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    purges: PurgeTasks,
}

/// Edge purges still in flight. Each runs detached so `deploy` never waits
/// on Cloudflare, but the process owns them: the CLI and `serve`'s shutdown
/// call [`PurgeTasks::drain`] before exiting, so a purge is never dropped
/// with the runtime. Each purge is bounded by the client's 10 s timeout.
#[derive(Clone, Default)]
pub struct PurgeTasks(Arc<Mutex<tokio::task::JoinSet<()>>>);

impl PurgeTasks {
    fn spawn(&self, purge: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut set = self.0.lock().expect("purge set poisoned");
        // Reap finished purges so a long-running `serve` doesn't accumulate them.
        while set.try_join_next().is_some() {}
        set.spawn(purge);
    }

    /// Wait for every purge spawned so far.
    pub async fn drain(&self) {
        let mut set = std::mem::take(&mut *self.0.lock().expect("purge set poisoned"));
        while set.join_next().await.is_some() {}
    }
}

/// Ask the app's fleet to adopt the new deploy now. A sleeping fleet isn't
/// listening, which is fine: it adopts the deploy when it wakes, and an
/// awake fleet would also pick it up on its own pointer poll.
async fn reload_if_awake(app: &App) -> bool {
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
/// Combined log kept per deploy, tail-capped before it hits the row.
const LOG_TAIL_CAP: usize = 64 * 1024;
/// Cap on one `error` line stored in the row.
const ERROR_CAP: usize = 1024;
/// Bounds for one build's cgroup leaf (spec §12.2: 4 GiB parent, 2 GiB leaf).
/// The template's Vite build (Rolldown, Tailwind/daisyUI, the PWA plugin)
/// peaks around 1.5 GB RSS in native memory a heap cap doesn't reach, and a
/// 1 GiB leaf OOM-killed it intermittently on the 8-core host.
const BUILD_PARENT_MEMORY_MAX: u64 = 4 * 1024 * 1024 * 1024;
const BUILD_PARENT_CPU_WEIGHT: u64 = 20;
const BUILD_LEAF_MEMORY_MAX: u64 = 2 * 1024 * 1024 * 1024;

/// Agent-facing refusal when the built config declares more than one D1
/// database or R2 bucket (spec: one of each for now). Kept as a const so
/// the deploy-root step can surface the same line as the outcome's
/// `user_message`.
pub(crate) const BINDING_CAP_MESSAGE: &str =
    "Outer apps can have one D1 database and one R2 bucket for now; merge the extra ones";

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

/// Close deploy rows a restart cut off. `serve` owns the deploy pipeline, so
/// at its boot no deploy can still be running: a row left `queued`,
/// `building`, `reviewing` or `deploying` would otherwise read as in flight
/// forever (rollback coalesces onto it, `deploy_status wait` polls it).
/// Returns how many rows were closed.
pub async fn fail_interrupted(pool: &SqlitePool) -> anyhow::Result<u64> {
    let closed = sqlx::query(
        "UPDATE deploy SET status = 'failed', \
         error = 'interrupted: outer restarted before this deploy finished', \
         finished_at = ? \
         WHERE status NOT IN ('success', 'failed', 'blocked')",
    )
    .bind(apps::now_ms())
    .execute(pool)
    .await?
    .rows_affected();
    Ok(closed)
}

/// Outer-private dir holding one crash-recovery sidecar per app whose
/// deploy config currently carries injected plaintext (`<slug>.orig` with
/// the pre-inject bytes, `<slug>.target` with `"<mode:o>\n<config path>"`).
/// Both files are 0600; the dir is 0700 and never inside an app-writable
/// worktree, so the app uid can neither read the bytes nor swap the names.
fn secret_sidecar_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("secret-sidecars")
}

/// Stash the pre-inject config bytes before the worktree file is touched:
/// a crash in the upload window leaves this behind for
/// [`scrub_interrupted_injections`] to replay at boot. Returns the two
/// sidecar paths for the guard to remove after it restores.
fn stash_secret_sidecar(
    data_dir: &Path,
    slug: &str,
    config: &Path,
    mode: u32,
    original: &[u8],
) -> anyhow::Result<(PathBuf, PathBuf)> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = secret_sidecar_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("cannot create secrets sidecar dir: {e:#}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| anyhow::anyhow!("cannot lock down secrets sidecar dir: {e:#}"))?;
    }
    let orig_path = dir.join(format!("{slug}.orig"));
    let target_path = dir.join(format!("{slug}.target"));
    let write_sidecar = |path: &Path, bytes: &[u8]| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| {
                anyhow::anyhow!("cannot stash secrets sidecar {}: {e:#}", path.display())
            })?;
        use std::io::Write as _;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| {
                anyhow::anyhow!("cannot stash secrets sidecar {}: {e:#}", path.display())
            })?;
        Ok(())
    };
    write_sidecar(&orig_path, original)?;
    let target_meta = format!("{:o}\n{}", mode & 0o7777, config.display());
    if let Err(e) = write_sidecar(&target_path, target_meta.as_bytes()) {
        let _ = std::fs::remove_file(&orig_path);
        return Err(e);
    }
    Ok((orig_path, target_path))
}

/// Boot scrub for a deploy interrupted mid-upload: for every leftover
/// sidecar, put the pre-inject bytes back over the worktree config (the
/// same symlink-pinned atomic replace) and delete the sidecar. A config
/// whose parent is gone holds no plaintext, so its sidecar is just
/// dropped; a config that fails the symlink check keeps its sidecar and
/// warns, so the next boot retries instead of writing through a link.
/// Returns how many configs were restored. Call next to
/// [`fail_interrupted`]: at boot no deploy runs, so nothing races this.
pub fn scrub_interrupted_injections(data_dir: &Path) -> anyhow::Result<u64> {
    let dir = secret_sidecar_dir(data_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => anyhow::bail!("cannot list secrets sidecar dir: {e:#}"),
    };
    let mut restored = 0u64;
    for entry in entries {
        let entry = entry.map_err(|e| anyhow::anyhow!("cannot list secrets sidecar dir: {e:#}"))?;
        let orig_path = entry.path();
        if orig_path.extension().is_none_or(|ext| ext != "orig") {
            continue;
        }
        let target_path = orig_path.with_extension("target");
        let original = match std::fs::read(&orig_path) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(path = %orig_path.display(), error = %format!("{e:#}"), "unreadable secrets sidecar; skipping");
                continue;
            }
        };
        let target_meta = match std::fs::read_to_string(&target_path) {
            Ok(meta) => meta,
            Err(e) => {
                tracing::warn!(path = %target_path.display(), error = %format!("{e:#}"), "secrets sidecar target missing; dropping orphaned plaintext backup");
                let _ = std::fs::remove_file(&orig_path);
                continue;
            }
        };
        let (mode_text, config_text) = target_meta
            .split_once('\n')
            .unwrap_or(("", target_meta.as_str()));
        let mode = u32::from_str_radix(mode_text.trim(), 8).unwrap_or(0o644);
        let config = PathBuf::from(config_text.trim());
        if config.as_os_str().is_empty() {
            tracing::warn!(path = %target_path.display(), "empty secrets sidecar target; dropping");
            let _ = std::fs::remove_file(&orig_path);
            let _ = std::fs::remove_file(&target_path);
            continue;
        }
        if config.parent().is_none_or(|parent| !parent.is_dir()) {
            tracing::info!(path = %config.display(), "injected config dir gone; dropping secrets sidecar");
            let _ = std::fs::remove_file(&orig_path);
            let _ = std::fs::remove_file(&target_path);
            continue;
        }
        match atomic_replace_no_follow(&config, &original, mode) {
            Ok(()) => {
                let _ = std::fs::remove_file(&orig_path);
                let _ = std::fs::remove_file(&target_path);
                restored += 1;
            }
            Err(e) => {
                tracing::warn!(path = %config.display(), error = %format!("{e:#}"), "cannot scrub injected secrets config; keeping sidecar for the next boot");
            }
        }
    }
    Ok(restored)
}

/// Deploys kept per app (spec §13: the console shows the last 50).
pub const KEEP_PER_APP: i64 = 50;

/// Delete deploy rows older than the newest [`KEEP_PER_APP`] per app (the
/// daily prune). Newest-first by `(created_at, rowid)`; ties keep the
/// later insert. Returns the rows removed.
pub async fn prune_old(pool: &SqlitePool) -> anyhow::Result<u64> {
    let removed = sqlx::query(
        "DELETE FROM deploy WHERE rowid NOT IN ( \
             SELECT d2.rowid FROM deploy d2 \
             WHERE d2.app_id = deploy.app_id \
             ORDER BY d2.created_at DESC, d2.rowid DESC \
             LIMIT ?1 \
         )",
    )
    .bind(KEEP_PER_APP)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(removed)
}

impl Deployer {
    pub fn new(settings: DeploySettings, build_concurrency: usize) -> Self {
        Self {
            settings,
            semaphore: Arc::new(Semaphore::new(build_concurrency)),
            app_locks: Mutex::new(HashMap::new()),
            purges: PurgeTasks::default(),
        }
    }

    /// The settings this deployer runs with (bucket, credentials, dirs,
    /// celld binary). The console data panels reuse them for scoped storage
    /// reads and celld calls, so they never mint their own credentials.
    pub(crate) fn settings(&self) -> &DeploySettings {
        &self.settings
    }

    /// The in-flight edge purges, for the owner of the process to drain.
    pub fn purges(&self) -> PurgeTasks {
        self.purges.clone()
    }

    /// Deploy `sha` of `app`: row `queued` → `building` → `reviewing` →
    /// `deploying` → `success`|`failed`|`blocked`. A blocked deploy writes
    /// `status = 'blocked'` with findings JSON, leaves `last_deploy_sha`
    /// alone (the previous version keeps serving), and returns normally —
    /// it is not an `Err`. On success calls `apps::set_last_deploy` and
    /// replaces the app's cron rows. Returns the outcome for success, step
    /// failure and blocks; only bookkeeping failures (the row itself) are `Err`.
    pub async fn deploy(
        &self,
        pool: &SqlitePool,
        git: &GitConfig,
        app: &App,
        sha: &str,
    ) -> anyhow::Result<DeployOutcome> {
        let slug = app.slug.clone();
        self.deploy_inner(pool, git, app, &slug, DeployKind::Main, sha)
            .await
    }

    /// Deploy `sha` to `app`'s preview fleet instead of live: the same
    /// pipeline (build, pre-live gate, migrations into the fresh preview
    /// namespace, upload), but the row is `kind = 'preview'`, `last_deploy_sha`
    /// and cron rows are untouched, and the edge purge covers only the
    /// preview host. `app` is the base app; the preview columns must already
    /// name this fleet (the caller sets them before deploying).
    pub async fn deploy_preview(
        &self,
        pool: &SqlitePool,
        git: &GitConfig,
        app: &App,
        sha: &str,
    ) -> anyhow::Result<DeployOutcome> {
        let Some(preview) = crate::preview::preview_app(app) else {
            anyhow::bail!("no preview fleet for {}", app.slug);
        };
        self.deploy_inner(pool, git, &preview, &app.slug, DeployKind::Preview, sha)
            .await
    }

    async fn deploy_inner(
        &self,
        pool: &SqlitePool,
        git: &GitConfig,
        app: &App,
        git_slug: &str,
        kind: DeployKind,
        sha: &str,
    ) -> anyhow::Result<DeployOutcome> {
        let deploy_id = uuid::Uuid::new_v4().to_string();
        insert_deploy_row(pool, &deploy_id, &app.id, sha, kind, apps::now_ms()).await?;

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
            .run_steps(
                pool,
                &deploy_id,
                git,
                app,
                git_slug,
                kind,
                sha,
                &mut journal,
            )
            .await;
        let Journal { steps, log } = journal;
        match result {
            Ok((version, crons)) => {
                let log_tail = tail_cap(&log);
                let steps_json = steps_json(&steps);
                finish_success(pool, &deploy_id, &steps_json, &log_tail).await?;
                match kind {
                    DeployKind::Main => {
                        apps::set_last_deploy(pool, &app.id, sha).await?;
                        cron::sync_app_crons(pool, &app.id, &crons, apps::now_ms()).await?;
                    }
                    DeployKind::Preview => {
                        // Live state is untouched, but the terminal deploy is
                        // activity on the preview: the idle TTL measures from
                        // the last finished run, not just requests.
                        let _ = crate::preview::touch_preview(pool, &app.id, apps::now_ms()).await;
                    }
                }
                // Switch an awake fleet first, then purge: purging while
                // the old version still serves would let the edge re-cache
                // its files. The purge runs off the critical path.
                let reloaded = reload_if_awake(app).await;
                self.spawn_edge_purge(pool, &app.id, &app.slug, kind);
                Ok(DeployOutcome {
                    deploy_id,
                    sha: sha.to_string(),
                    status: "success",
                    failed_step: None,
                    error: None,
                    reason: None,
                    findings: Vec::new(),
                    user_message: None,
                    steps,
                    log_tail,
                    version,
                    reloaded,
                })
            }
            Err(StepEnd::Blocked(blocked)) => {
                // Nothing was uploaded and the fleet is untouched: the
                // previous version keeps serving.
                let log_tail = tail_cap(&log);
                let steps_json = steps_json(&steps);
                finish_blocked(pool, &deploy_id, &blocked, &steps_json, &log_tail).await?;
                Ok(DeployOutcome {
                    deploy_id,
                    sha: sha.to_string(),
                    status: "blocked",
                    failed_step: Some("gate".to_string()),
                    error: Some(blocked.reason.to_string()),
                    reason: Some(blocked.reason.to_string()),
                    findings: blocked.findings,
                    user_message: Some(blocked.user_message),
                    steps,
                    log_tail,
                    version: None,
                    reloaded: false,
                })
            }
            Err(StepEnd::Failed(failed)) => {
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
                    reason: None,
                    findings: Vec::new(),
                    user_message: failed.user_message,
                    steps,
                    log_tail,
                    version: None,
                    reloaded: false,
                })
            }
        }
    }

    /// Purge `<slug>.<apps_domain>` plus every `active` custom hostname from
    /// the edge cache after the new version went live. Detached (no global
    /// state, just cloned handles) so a slow Cloudflare never holds up the
    /// deploy response; `None` skips at debug, failures warn. Only
    /// successful deploys purge: failed and blocked deploys leave the
    /// previous version serving, and its cached files are still the right
    /// ones. A preview deploy purges only its own host: custom domains
    /// serve the live app, and their cached files are still the right ones.
    fn spawn_edge_purge(&self, pool: &SqlitePool, app_id: &str, slug: &str, kind: DeployKind) {
        let purge = self.settings.cache_purge.clone();
        let pool = pool.clone();
        let app_id = app_id.to_string();
        let hosts = vec![format!("{slug}.{}", self.settings.apps_domain)];
        let include_custom = !kind.is_preview();
        self.purges.spawn(async move {
            let mut hosts = hosts;
            if include_custom {
                hosts.extend(crate::apps::domains::active_hosts(&pool, &app_id).await);
            }
            for host in hosts {
                crate::cloudflare::purge_host_cache(purge.as_ref(), &host).await;
            }
        });
    }

    /// The per-app deploy mutex. `deploy` holds it across a whole pipeline
    /// run; `delete_app` (MCP and the `outer app delete` CLI) holds it
    /// across the whole deletion so no deploy can start mid-delete.
    pub fn app_lock(&self, app_id: &str) -> Arc<AsyncMutex<()>> {
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
    /// the celld version and the validated cron schedules on success. The
    /// review runs before `deploy-root`, and before migrations, so a blocked
    /// deploy never touches the app's data or uploads anything. For an app
    /// with a live version migrations run before the upload, so no fleet can
    /// ever serve new code against an unmigrated schema; for a first deploy
    /// (no successful deploy yet) the upload runs first because `celld d1`
    /// needs a running fleet and there is nothing to start one from yet.
    // Nine params: the run identity (fleet app, mirror slug, kind) rides
    // alongside the deploy identity. Splitting the struct would obscure
    // the step order below, so the count stays.
    #[allow(clippy::too_many_arguments)]
    async fn run_steps(
        &self,
        pool: &SqlitePool,
        deploy_id: &str,
        git: &GitConfig,
        app: &App,
        git_slug: &str,
        kind: DeployKind,
        sha: &str,
        journal: &mut Journal,
    ) -> Result<(Option<String>, Vec<String>), StepEnd> {
        let Journal { steps, log } = journal;
        // 1. Worktree. The handover (root-owned checkout to the app uid) is
        // part of this step: it walks the whole tree (~0.3 s on the
        // template), and hiding it made `deploy.duration_ms` miss it.
        let started = Instant::now();
        match self.materialize(git, git_slug, app, sha).await {
            Ok(out) => {
                // The worktree is outer-owned; the install, build and deploy
                // steps run as the app uid, so hand it over before they write
                // into it.
                let handover_start = Instant::now();
                hand_worktree_to_app(&app.slug, &self.work_dir(&app.slug), app.uid);
                let handover_ms = handover_start.elapsed().as_millis();
                push_step(
                    steps,
                    log,
                    "worktree",
                    started,
                    false,
                    &format!("{out}handover {handover_ms}ms\n"),
                );
            }
            Err(e) => {
                let ms = push_step(steps, log, "worktree", started, false, &format!("{e:#}"));
                tracing::warn!(slug = %app.slug, sha, ms, error = %format!("{e:#}"), "deploy failed at worktree");
                return Err(StepEnd::Failed(StepFailed::plain("worktree", &e)));
            }
        }
        // 2. Install.
        let started = Instant::now();
        match self.install(app, deploy_id).await {
            Ok((skipped, out)) => {
                push_step(steps, log, "install", started, skipped, &out);
            }
            Err(e) => {
                push_step(steps, log, "install", started, false, &format!("{e:#}"));
                return Err(StepEnd::Failed(StepFailed::sandboxed("install", &e)));
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
                return Err(StepEnd::Failed(StepFailed::sandboxed("build", &e)));
            }
        }

        // 4. Review: the pre-live profanity gate (spec §15.1). A block
        // stops the pipeline here: nothing is uploaded, the fleet is
        // untouched, and the outcome returns normally as `blocked`.
        set_deploy_status(pool, deploy_id, "reviewing")
            .await
            .map_err(|e| StepEnd::Failed(StepFailed::plain("gate", &e)))?;
        let started = Instant::now();
        match self.review_gate(pool, app, deploy_id, sha).await {
            Ok((None, out)) => {
                push_step(steps, log, "gate", started, false, &out);
            }
            Ok((Some(blocked), out)) => {
                push_step(steps, log, "gate", started, false, &out);
                return Err(StepEnd::Blocked(blocked));
            }
            Err(gate::GateError::Unavailable(e)) => {
                // Jev is down after its retry: fail closed. Nothing is
                // uploaded, the previous version keeps serving, and nothing
                // was cached as clear.
                tracing::warn!(slug = %app.slug, sha, error = %e, "content review unavailable; failing the deploy closed");
                let user_message = crate::moderation::unavailable_message();
                let line = "gate: jev unavailable after retry → failed review_unavailable";
                push_step(steps, log, "gate", started, false, line);
                return Err(StepEnd::Failed(StepFailed::unavailable(
                    "gate",
                    user_message,
                )));
            }
            Err(gate::GateError::Other(e)) => {
                push_step(steps, log, "gate", started, false, &format!("{e:#}"));
                return Err(StepEnd::Failed(StepFailed::plain("gate", &e)));
            }
        }

        // 5. Deploy root.
        let started = Instant::now();
        let (deploy_root, crons) = match self.deploy_root(app) {
            Ok((deploy_root, crons, out)) => {
                push_step(steps, log, "deploy-root", started, false, &out);
                (deploy_root, crons)
            }
            Err(e) => {
                let message = format!("{e:#}");
                push_step(steps, log, "deploy-root", started, false, &message);
                // The binding cap is an agent-fixable config error, so it
                // rides the outcome's `user_message` like a failed migration.
                let failed = if message == BINDING_CAP_MESSAGE {
                    StepFailed::refused("deploy-root", message)
                } else {
                    StepFailed::plain("deploy-root", &e)
                };
                return Err(StepEnd::Failed(failed));
            }
        };

        // 6+7. D1 migrations and the `celld deploy` upload. For an app with
        // a live version the migrate step runs before the upload: celld
        // offers no staged or invisible upload (the upload publishes
        // `deploy/current.json` and every fleet adopts it on its own poll),
        // so ordering is the only gate: when the new pointer becomes visible
        // the schema is already ready, and a migrate failure uploads
        // nothing, so the previous version keeps serving. The old code keeps
        // serving during the migration (expand-first for destructive
        // changes).
        //
        // A first deploy (`last_deploy_sha` is NULL: no successful deploy
        // yet) reverses the order. `celld d1 migrations apply` finds its
        // node through the bucket's node leases, and a first deploy has
        // published nothing yet, so no fleet can start and the wake before
        // the migrate step times out. The upload goes first, then the wake
        // (now possible), then the migrations — and only a migrate success
        // marks the deploy successful. Nothing is served before that: the
        // proxy refuses to forward for an app with no successful deploy, so
        // no fleet can serve the new code unmigrated.
        //
        // The live-version signal comes from the row, not the caller's
        // snapshot: a previous deploy may have succeeded after the caller
        // loaded the app (the per-app lock serializes deploys, so the row
        // is current). `last_deploy_sha` NULL means no successful deploy yet.
        // A preview deploy reads the preview signal instead: the first
        // preview deploy (no successful preview run yet) uploads before
        // migrating for the same lease reason, against the preview fleet.
        let is_first_deploy = match kind {
            DeployKind::Main => last_deploy_sha(pool, &app.id)
                .await
                .map_err(|e| StepEnd::Failed(StepFailed::plain("migrate", &e)))?
                .is_none(),
            DeployKind::Preview => last_preview_success(pool, &app.id)
                .await
                .map_err(|e| StepEnd::Failed(StepFailed::plain("migrate", &e)))?
                .is_none(),
        };
        set_status_blocking(pool, deploy_id)
            .await
            .map_err(StepEnd::Failed)?;
        if is_first_deploy {
            let version = self
                .deploy_step(pool, app, deploy_id, &deploy_root, journal)
                .await?;
            self.migrate_step(
                pool,
                app,
                deploy_id,
                &deploy_root,
                is_first_deploy,
                kind,
                journal,
            )
            .await?;
            Ok((version, crons))
        } else {
            self.migrate_step(
                pool,
                app,
                deploy_id,
                &deploy_root,
                is_first_deploy,
                kind,
                journal,
            )
            .await?;
            let version = self
                .deploy_step(pool, app, deploy_id, &deploy_root, journal)
                .await?;
            Ok((version, crons))
        }
    }

    /// The `celld deploy <root> ... --json` upload step; returns the version.
    /// The upload publishes `deploy/current.json`, which every fleet adopts
    /// on its own poll — so for an existing app it runs after the migrate
    /// step (a migrate failure uploads nothing at all), while for a first
    /// deploy it runs first (nothing to start a fleet from otherwise).
    async fn deploy_step(
        &self,
        pool: &SqlitePool,
        app: &App,
        deploy_id: &str,
        deploy_root: &DeployRoot,
        journal: &mut Journal,
    ) -> Result<Option<String>, StepEnd> {
        let Journal { steps, log } = journal;
        let started = Instant::now();
        // Decrypt after the gate: a blocked deploy never touches secrets.
        let injected = self
            .inject_secrets(pool, app, deploy_root)
            .await
            .map_err(|e| StepEnd::Failed(StepFailed::plain("deploy", &e)))?;
        let outcome = self.celld_deploy(app, deploy_id, deploy_root).await;
        // The patched config held plaintext: dropping the guard restores it
        // byte-identically, pass or fail — and would too on panic, since the
        // restore lives in `Drop`, not in a manual call.
        if let Some(guard) = injected {
            let line = guard.journal_line();
            drop(guard);
            log.push_str(&line);
        }
        match outcome {
            Ok((out, version)) => {
                push_step(steps, log, "deploy", started, false, &out);
                Ok(version)
            }
            Err(e) => {
                push_step(steps, log, "deploy", started, false, &format!("{e:#}"));
                Err(StepEnd::Failed(StepFailed::sandboxed("deploy", &e)))
            }
        }
    }

    /// The D1 migrations step: pending `NNNN_*.sql` files for the built
    /// config's `d1_databases`, applied with the app's scoped credentials.
    /// For an existing app this runs before the upload makes the new code
    /// visible to any fleet; for a first deploy it runs after the upload
    /// (which is what lets the woken fleet exist at all). No files (or no
    /// D1) skips the step without waking a sleeping fleet.
    // The kind rides along for the wake target and the pin-record switch.
    #[allow(clippy::too_many_arguments)]
    async fn migrate_step(
        &self,
        pool: &SqlitePool,
        app: &App,
        deploy_id: &str,
        deploy_root: &DeployRoot,
        is_first_deploy: bool,
        kind: DeployKind,
        journal: &mut Journal,
    ) -> Result<(), StepEnd> {
        let Journal { steps, log } = journal;
        let started = Instant::now();
        match self
            .migrate(pool, app, deploy_id, deploy_root, is_first_deploy, kind)
            .await
        {
            Ok((skipped, out)) => {
                push_step(steps, log, "migrate", started, skipped, &out);
                Ok(())
            }
            Err(e) => {
                push_step(steps, log, "migrate", started, false, &e.error);
                Err(StepEnd::Failed(e))
            }
        }
    }

    /// Step 1: the worktree holds exactly `sha`'s tree, except `node_modules`.
    ///
    /// `GIT_INDEX_FILE` points at a sidecar index (`{data}/work/{slug}.index`)
    /// so `clean` knows what the last materialization tracked: `checkout
    /// --force` brings tracked files to `sha`, then `clean -ffdx -e
    /// node_modules` removes everything else (build output, stale files).
    async fn materialize(
        &self,
        git: &GitConfig,
        mirror_slug: &str,
        app: &App,
        sha: &str,
    ) -> anyhow::Result<String> {
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("refusing to materialize {sha:?}: not a full commit sha");
        }
        let mirror = manifest::mirror_path(git, mirror_slug);
        if !mirror.is_dir() {
            anyhow::bail!("no mirror for {mirror_slug}");
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
        // Deps changed (or first install): the persistent tmp may hold stale
        // artifacts keyed to the old tree, so wipe it before installing.
        #[cfg(unix)]
        if stored.as_deref() != Some(current.as_str()) {
            wipe_cache_tmp(&self.cache_dir(&app.slug), app.uid, app.uid)?;
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
        // Cap the persistent tmp before building: an overgrown tmp is wiped
        // rather than carried into the build.
        #[cfg(unix)]
        if tmp_needs_wipe(&self.cache_dir(&app.slug)) {
            wipe_cache_tmp(&self.cache_dir(&app.slug), app.uid, app.uid)?;
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

    /// Step 4: find and validate the wrangler config. Returns the deploy
    /// root, the exact config file selected, and the validated
    /// `triggers.crons` schedules. An invalid cron expression fails the
    /// step, naming the expression, before anything goes live. The migrate
    /// step reads this same file (never re-decides), so D1 detection and
    /// the upload can never disagree about which config won.
    fn deploy_root(&self, app: &App) -> anyhow::Result<(DeployRoot, Vec<String>, String)> {
        let work = self.work_dir(&app.slug);
        // Discovery refuses symlinks (fail closed): a planted link is a
        // deploy error, never a resolved path.
        let config = find_deploy_config(&work)?.ok_or_else(|| {
            anyhow::anyhow!(
                "no wrangler config: looked for dist/wrangler.json, wrangler.jsonc, wrangler.json under {}",
                work.display()
            )
        })?;
        let parsed = read_config_file(&config)?;
        validate_config_keys(&parsed)?;
        let crons = cron::crons_from_config(&parsed)?;
        let root = config
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| work.clone());
        let out = format!("deploy root {}\n", root.display());
        Ok((DeployRoot { root, config }, crons, out))
    }

    /// The `celld deploy <root> ... --json` upload; returns (output, version).
    /// Bucket credentials come from the per-prefix cache when fresh (minted
    /// on a miss), so the step env never carries the parent keys.
    async fn celld_deploy(
        &self,
        app: &App,
        deploy_id: &str,
        deploy_root: &DeployRoot,
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
            deploy_root.root.to_string_lossy().into_owned(),
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
    /// Decrypt the app's secrets into the deploy-root config `vars`, for this
    /// deploy only. Returns `None` when the app has no (usable) secrets (the
    /// config is untouched), else a guard holding the original bytes: the
    /// caller runs the upload, and the guard restores byte-identically on
    /// drop (see [`InjectedSecrets`]).
    ///
    /// Code-defined `vars` keys lose to same-named secrets; the overwrite is
    /// journaled by name (names are public via `list_secrets`, values never
    /// are). The patched file is `0o600` while it holds plaintext; the
    /// upload carries it to `fleets/<slug>/`, where celld needs it.
    ///
    /// Rows whose names are platform-reserved or fail the name shape are
    /// skipped with a warning: the sink re-checks because pre-existing or
    /// DB-written rows predate the tool-time checks, and must never override
    /// platform vars. The config symlink checks run again here (discovery
    /// ran before the build): a planted link fails the deploy closed.
    async fn inject_secrets(
        &self,
        pool: &SqlitePool,
        app: &App,
        deploy_root: &DeployRoot,
    ) -> anyhow::Result<Option<InjectedSecrets>> {
        let secrets =
            crate::secrets::decrypt_all(pool, &app.id, &self.settings.secrets_key).await?;
        let mut usable: Vec<(String, Vec<u8>)> = Vec::with_capacity(secrets.len());
        for (name, value) in secrets {
            if crate::secrets::check_name(&name).is_err()
                || crate::secrets::check_reserved(&name).is_err()
            {
                tracing::warn!(name = %name, app = %app.slug, "skipping secret with reserved or invalid name during injection");
                continue;
            }
            usable.push((name, value));
        }
        if usable.is_empty() {
            return Ok(None);
        }
        let path = deploy_root.config.clone();
        let (original, mode) = read_no_follow(&path)
            .map_err(|e| anyhow::anyhow!("reading deploy config {}: {e:#}", path.display()))?;
        let raw = String::from_utf8(original.clone())
            .map_err(|_| anyhow::anyhow!("deploy config {} is not valid UTF-8", path.display()))?;
        let mut parsed: serde_json::Value = if path.extension().is_some_and(|e| e == "jsonc") {
            serde_json::from_str(&strip_jsonc_comments(&raw))?
        } else {
            serde_json::from_str(&raw)?
        };
        let obj = parsed
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("deploy config {} is not an object", path.display()))?;
        let vars = obj
            .entry("vars".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        let vars = vars.as_object_mut().ok_or_else(|| {
            anyhow::anyhow!("deploy config {} has a non-object vars", path.display())
        })?;
        let mut overwritten = Vec::new();
        for (name, value) in &usable {
            let text = String::from_utf8(value.clone()).map_err(|_| {
                anyhow::anyhow!("secret {name:?} is not valid UTF-8; refusing to inject")
            })?;
            if vars.contains_key(name.as_str()) {
                overwritten.push(name.clone());
            }
            vars.insert(name.clone(), serde_json::Value::String(text));
        }
        let patched = serde_json::to_string_pretty(&parsed)?;
        // Stash the pre-inject bytes first: a crash past this point leaves
        // the sidecar for the boot scrub to replay.
        let (sidecar_orig, sidecar_target) =
            stash_secret_sidecar(&self.settings.data_dir, &app.slug, &path, mode, &original)?;
        atomic_replace_no_follow(&path, patched.as_bytes(), 0o600)
            .map_err(|e| anyhow::anyhow!("writing deploy config {}: {e:#}", path.display()))?;
        Ok(Some(InjectedSecrets {
            path,
            original,
            mode,
            injected: usable.len(),
            overwritten,
            sidecar_orig: Some(sidecar_orig),
            sidecar_target: Some(sidecar_target),
        }))
    }
    /// Apply pending D1 migrations, then record what was applied.
    /// Returns `(skipped, journal)`: skipped when the built config declares
    /// no D1 databases or the repo holds no migration files, in which case a
    /// sleeping fleet is never woken. Otherwise the fleet is woken first
    /// (`celld d1` reaches the database through a running node), pending
    /// files apply in numeric-prefix order under the app's scoped
    /// credentials, and every file present is recorded in `d1_migration` so
    /// a later edit of an applied file fails the deploy instead of drifting.
    /// A failure names the migration file when celld's output identifies it
    /// and carries an agent-facing line; the fleet is never reloaded, so an
    /// existing app keeps serving its previous version while a first deploy
    /// leaves the app not-live (the proxy forwards only after a success).
    /// `is_first_deploy` selects that wording and the operator-CLI hint
    /// below; the caller derives it from `last_deploy_sha` being NULL.
    async fn migrate(
        &self,
        pool: &SqlitePool,
        app: &App,
        deploy_id: &str,
        deploy_root: &DeployRoot,
        is_first_deploy: bool,
        kind: DeployKind,
    ) -> Result<(bool, String), StepFailed> {
        let root = &deploy_root.root;
        let work = self.work_dir(&app.slug);
        // The exact file deploy-root selected: never re-decide, so D1
        // detection and the upload read the same config.
        let parsed =
            read_config_file(&deploy_root.config).map_err(|e| StepFailed::plain("migrate", &e))?;
        let databases =
            migrate::d1_databases(&parsed).map_err(|e| StepFailed::plain("migrate", &e))?;
        if databases.is_empty() {
            return Ok((
                true,
                "migrate: no d1_databases declared; skipping migrations".to_string(),
            ));
        }
        // Stage each distinct migrations dir once; several databases may
        // share one directory, and each of them gets the staged files.
        let mut by_dir: Vec<(String, Vec<migrate::MigrationFile>)> = Vec::new();
        for db in &databases {
            let dir = migrate::migrations_dir_name(db).to_string();
            if !by_dir.iter().any(|(seen, _)| *seen == dir) {
                let files = migrate::stage_migrations(root, &work, &dir)
                    .map_err(|e| StepFailed::plain("migrate", &e))?
                    .unwrap_or_default();
                by_dir.push((dir, files));
            }
        }
        let mut staged: Vec<(migrate::D1Database, Vec<migrate::MigrationFile>)> = Vec::new();
        for db in databases {
            let dir = migrate::migrations_dir_name(&db);
            let files = by_dir
                .iter()
                .find(|(seen, _)| seen == dir)
                .map(|(_, files)| files.clone())
                .unwrap_or_default();
            staged.push((db, files));
        }
        if staged.iter().all(|(_, files)| files.is_empty()) {
            return Ok((
                true,
                "migrate: no migration files; skipping migrations".to_string(),
            ));
        }
        // An applied file edited afterwards is a new migration pretending to
        // be an old one: fail naming it instead of letting celld silently
        // skip it (celld tracks names only) and drifting from the database.
        // Files with no recorded hash are pending; when every file is
        // already recorded there is nothing for celld to do, and no reason
        // to wake a sleeping fleet.
        // A first deploy that never went live has no previous version: say
        // the app isn't live yet instead of naming one that keeps serving.
        let live_line = if kind.is_preview() {
            "Your live app is untouched; only the preview failed to update."
        } else if is_first_deploy {
            "Your app isn't live yet."
        } else {
            "Your app still shows the previous version."
        };
        let mut any_pending = false;
        for (db, files) in &staged {
            for file in files {
                let recorded =
                    migrate::applied_sha(pool, &app.id, db.effective_identity(), &file.name)
                        .await
                        .map_err(|e| StepFailed::plain("migrate", &e))?;
                match recorded {
                    Some(sha) if sha != file.sha256 => {
                        let error = format!(
                            "migration {} for database {} was edited after it was applied; \
                              write a new migration instead",
                            file.name, db.database_name
                        );
                        let user_message = format!(
                            "Outer couldn't publish this change because migration {} was edited \
                              after it was already applied to database {}. {live_line} Write a \
                              new migration file instead of editing an applied one.",
                            file.name, db.database_name
                        );
                        return Err(StepFailed::migration(error, user_message));
                    }
                    Some(_) => {}
                    None => any_pending = true,
                }
            }
        }
        // A preview database is disposable (fresh prefix per preview
        // lineage), so "all already applied" against the live pins must not
        // skip the apply: the preview database may never have seen these
        // files. Preview applies are idempotent (celld's own `d1_migrations`
        // journal) and never recorded.
        if !any_pending && !kind.is_preview() {
            let total: usize = staged.iter().map(|(_, files)| files.len()).sum();
            return Ok((
                true,
                format!("migrate: {total} migration files, all already applied"),
            ));
        }
        if let Some(wake) = &self.settings.wake {
            match kind {
                DeployKind::Main => {
                    wake.ensure_awake(pool, app)
                        .await
                        .map_err(|e| StepFailed::plain("migrate", &e))?;
                }
                DeployKind::Preview => {
                    wake.ensure_awake_preview(pool, app)
                        .await
                        .map_err(|e| StepFailed::plain("migrate", &e))?;
                }
            }
        }
        let mut journal = String::new();
        for (db, files) in &staged {
            if files.is_empty() {
                continue;
            }
            let out = self
                .celld_migrate(app, deploy_id, root, &db.database_name)
                .await
                .map_err(|e| {
                    let text = format!("{e:#}");
                    // The operator CLI runs no fleet manager (`wake: None`):
                    // a first deploy can never have a fleet for `celld d1`
                    // to reach, so the lease error it returns means the
                    // deploy must go through the server instead. Say so
                    // instead of surfacing celld's lease internals. (With a
                    // fleet manager the wake above runs first, so a lease
                    // error there is a real failure, not this hint.)
                    if is_first_deploy && self.settings.wake.is_none() && text.contains("no node leases") {
                        return StepFailed::migration(
                            format!(
                                "first deploy with D1 migrations needs the server; deploy through MCP \
                                 (celld d1 needs a running fleet and none serves {} yet): {}",
                                app.slug,
                                first_output_line(&text)
                            ),
                            "Outer couldn't publish this app because its first deploy needs \
                             database migrations and those only run on the server. Deploy through \
                             MCP instead of the operator CLI. Your app isn't live yet."
                                .to_string(),
                        );
                    }
                    let failed = migrate::failing_file(&text);
                    let error = match &failed {
                        Some(name) => format!(
                            "migration {name} for database {} failed: {}",
                            db.database_name,
                            error_detail(&text, Some(name))
                        ),
                        None => format!(
                            "migrations for database {} failed: {}",
                            db.database_name,
                            first_output_line(&text)
                        ),
                    };
                    let user_message = match &failed {
                        Some(name) => format!(
                            "Outer couldn't publish this change because migration {name} failed \
                             to apply to database {}: {}. {live_line}",
                            db.database_name,
                            error_detail(&text, Some(name))
                        ),
                        None => format!(
                            "Outer couldn't publish this change because the database migrations \
                             failed to apply: {}. {live_line}",
                            first_output_line(&text)
                        ),
                    };
                    StepFailed::migration(error, user_message)
                })?;
            journal.push_str(&out);
            if !out.ends_with('\n') {
                journal.push('\n');
            }
            if !kind.is_preview() {
                migrate::record_applied(
                    pool,
                    &app.id,
                    &db.database_name,
                    db.effective_identity(),
                    files,
                    apps::now_ms(),
                )
                .await
                .map_err(|e| StepFailed::plain("migrate", &e))?;
            }
        }
        Ok((
            false,
            format!("migrate: applied pending migrations\n{journal}"),
        ))
    }

    /// One `celld d1 migrations apply` under the app's scoped storage
    /// credentials (the same grant shape `celld deploy` uses: celld reads
    /// the fleet's shared secret from the bucket, so it needs the prefix
    /// grant, never the parent keys).
    /// No `--` before the positionals on purpose: celld 0.6.2's leaf parsers
    /// honor neither `--help` nor `--` (`migrations list -- foo` still
    /// reports "needs a database name"), so a `--` would break the call
    /// instead of protecting it. Option injection is closed one layer up:
    /// `migrate::d1_databases` only accepts names that cannot parse as flags
    /// (and argv never passes through a shell).
    async fn celld_migrate(
        &self,
        app: &App,
        deploy_id: &str,
        root: &Path,
        database_name: &str,
    ) -> anyhow::Result<String> {
        let prefix = FleetStorage::fleet_prefix(&app.slug);
        let creds = self
            .settings
            .storage
            .credentials
            .for_prefix(&prefix)
            .await
            .map_err(|err| {
                anyhow::anyhow!(
                    "minting scoped R2 credentials for migrations of {}: {err:#}",
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
        let args = vec![
            "d1".to_string(),
            "migrations".to_string(),
            "apply".to_string(),
            database_name.to_string(),
            root.to_string_lossy().into_owned(),
            "--bucket".to_string(),
            bucket,
            "--endpoint".to_string(),
            endpoint,
            "--region".to_string(),
            region,
        ];
        self.run_sandboxed_step(
            app,
            deploy_id,
            "migrate",
            &self.settings.celld_bin,
            &args,
            env,
        )
        .await
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

    /// Step 4: the pre-live gate over the built worktree. Returns the block
    /// with a journal line, or `None` with a journal line when the deploy
    /// passes. Jev classifies the new chunks; when Jev is unreachable the
    /// error fails the deploy closed (`review_unavailable`). Without a Jev
    /// client (localhost dev only) the gate logs a warning and passes.
    async fn review_gate(
        &self,
        pool: &SqlitePool,
        app: &App,
        deploy_id: &str,
        sha: &str,
    ) -> Result<(Option<BlockedInfo>, String), gate::GateError> {
        use crate::moderation::extract;
        let work = self.work_dir(&app.slug);
        let chunks = match extract::collect(&work, &app.slug, app.name.as_deref()) {
            Ok(chunks) => chunks,
            Err(too_large) => {
                let outcome =
                    gate::run_too_large(pool, &app.id, deploy_id, sha, &too_large).await?;
                let gate::GateOutcome::Block {
                    findings,
                    user_message,
                    ..
                } = outcome
                else {
                    unreachable!("over-caps always blocks");
                };
                let line = format!(
                    "gate: over caps ({} files, {} bytes) → blocked too_large_to_review",
                    too_large.files, too_large.bytes
                );
                return Ok((
                    Some(BlockedInfo {
                        reason: "too_large_to_review",
                        findings,
                        user_message,
                    }),
                    line,
                ));
            }
        };
        let Some(jev) = &self.settings.jev else {
            tracing::warn!(slug = %app.slug, sha, "gate: no Jev client (localhost dev without the Cloudflare pair); passing unchecked");
            let outcome = gate::run_skipped(pool, &app.id, deploy_id, sha, &chunks).await?;
            let gate::GateOutcome::Pass { stats, .. } = outcome else {
                unreachable!("skipped always passes");
            };
            return Ok((
                None,
                format!(
                    "gate: {} files, {} chunks ({} checked, {} cached) → pass (unchecked localhost dev)",
                    stats.files, stats.chunks, stats.checked, stats.skipped
                ),
            ));
        };
        let outcome = gate::run_gate(pool, &app.id, deploy_id, sha, chunks, jev).await?;
        match outcome {
            gate::GateOutcome::Pass { stats, queued } => Ok((
                None,
                format!(
                    "gate: {} files, {} chunks ({} checked, {} cached) → pass{}",
                    stats.files,
                    stats.chunks,
                    stats.checked,
                    stats.skipped,
                    if queued { " (queued for review)" } else { "" },
                ),
            )),
            gate::GateOutcome::Block {
                reason,
                findings,
                user_message,
                stats,
            } => Ok((
                Some(BlockedInfo {
                    reason: reason.as_str(),
                    findings,
                    user_message,
                }),
                format!(
                    "gate: {} files, {} chunks ({} checked, {} cached) → blocked",
                    stats.files, stats.chunks, stats.checked, stats.skipped
                ),
            )),
        }
    }
}

/// How `run_steps` ends without a version: a step failure, or a gate
/// block. Blocks are not failures: nothing was uploaded, the fleet is
/// untouched, and the outcome returns normally as `blocked`.
enum StepEnd {
    Failed(StepFailed),
    Blocked(BlockedInfo),
}

/// A gate block: the deploy row's `error` column carries `reason`, and
/// `findings` go to the row as JSON.
struct BlockedInfo {
    reason: &'static str,
    findings: Vec<crate::moderation::gate::Finding>,
    user_message: String,
}

/// A step that failed: its name, the first meaningful output line, and an
/// optional agent-facing line (the gate sets one for `review_unavailable`,
/// migrate for file-specific failures).
struct StepFailed {
    step: &'static str,
    error: String,
    user_message: Option<String>,
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
            user_message: None,
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
            user_message: None,
        }
    }

    /// For Jev being unreachable: the deploy fails closed with error code
    /// `review_unavailable` and an agent-facing line.
    fn unavailable(step: &'static str, user_message: String) -> Self {
        Self {
            step,
            error: "review_unavailable".to_string(),
            user_message: Some(user_message),
        }
    }

    /// For a failed migration: names the file when identifiable, with an
    /// agent-facing line. The fleet is never reloaded, so an existing app
    /// keeps serving its previous version while a first deploy stays
    /// not-live.
    fn migration(error: String, user_message: String) -> Self {
        Self {
            step: "migrate",
            error: cap_error(&error),
            user_message: Some(user_message),
        }
    }

    /// For a config the deploy refuses (the binding cap): the refusal line
    /// is the agent-facing message, so it doubles as the outcome error.
    fn refused(step: &'static str, user_message: String) -> Self {
        Self {
            step,
            error: cap_error(&user_message),
            user_message: Some(user_message),
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
/// `wrangler.jsonc`, else root `wrangler.json`. Returns the pinned path —
/// every component is checked with `symlink_metadata`, so a config (or a
/// parent like `dist`) planted as a symlink fails the deploy closed instead
/// of resolving through the link. `is_file` would follow links, so it is
/// never used here. `Ok(None)` means no config exists; `Err` means a link
/// (or an unreadable entry) refused the discovery.
pub(crate) fn find_deploy_config(work: &Path) -> anyhow::Result<Option<PathBuf>> {
    for candidate in [
        work.join("dist").join("wrangler.json"),
        work.join("wrangler.jsonc"),
        work.join("wrangler.json"),
    ] {
        match std::fs::symlink_metadata(&candidate) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                anyhow::bail!("cannot stat deploy config {}: {e:#}", candidate.display());
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                anyhow::bail!(
                    "refusing deploy config {}: it is a symlink",
                    candidate.display()
                );
            }
            Ok(meta) if !meta.file_type().is_file() => continue,
            Ok(_) => {
                refuse_symlink_components(&candidate)?;
                return Ok(Some(candidate));
            }
        }
    }
    Ok(None)
}
/// Parse one wrangler file (JSON, or JSONC with comments stripped),
/// opening with `O_NOFOLLOW` so a path swapped for a symlink between
/// discovery and this read fails instead of handing over foreign bytes.
pub(crate) fn read_config_file(config: &Path) -> anyhow::Result<serde_json::Value> {
    let (bytes, _) = read_no_follow(config)
        .map_err(|e| anyhow::anyhow!("reading deploy config {}: {e:#}", config.display()))?;
    let raw = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("deploy config {} is not valid UTF-8", config.display()))?;
    Ok(if config.extension().is_some_and(|e| e == "jsonc") {
        serde_json::from_str(&strip_jsonc_comments(&raw))?
    } else {
        serde_json::from_str(&raw)?
    })
}

/// The first meaningful output line: the sandboxed runner prepends a
/// `prog [args] exit ..` header, which names no migration file.
fn first_output_line(text: &str) -> &str {
    text.lines()
        .skip(1)
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| {
            text.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("")
        })
}

/// The output line naming the failed migration, when one does; otherwise the
/// first meaningful line. celld prints progress lines for applied files
/// before the error, so the first line would name the wrong file.
fn error_detail<'a>(text: &'a str, failed: Option<&str>) -> &'a str {
    let Some(name) = failed else {
        return first_output_line(text);
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .find(|l| l.contains(name))
        .unwrap_or_else(|| first_output_line(text))
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
    // One D1 database and one R2 bucket per app for now: anything more
    // fails the deploy at its validation step, before migrations or upload.
    for key in ["d1_databases", "r2_buckets"] {
        let count = obj
            .get(key)
            .and_then(|v| v.as_array())
            .map(|list| list.len())
            .unwrap_or(0);
        if count > 1 {
            anyhow::bail!(BINDING_CAP_MESSAGE);
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
        // Other app uids traverse `work/` too: the tree itself is private.
        crate::fleet::sandbox::make_private_dir(work);
    }
    #[cfg(not(unix))]
    {
        let _ = (slug, work, uid);
    }
}

/// Test hook for [`chown_tree_with`]: called once per visited path in the
/// window where the old path-based walk was racable. `None` skips the hook.
#[cfg(unix)]
type ChownHook<'a> = Option<&'a dyn Fn(&Path, u32, u32) -> bool>;

/// Recursively hand `path` to (`uid`, `gid`) without following symlinks (cf.
/// Noite's `lchown_tree`): an app-writable tree can link anywhere, and a
/// following chown run by root would hand platform files to the app uid.
/// The walk is fd-based: the root is opened `O_NOFOLLOW`, entries are listed
/// through the open fd, changed with `fchownat(…, AT_SYMLINK_NOFOLLOW)`, and
/// descended into with `openat(…, O_NOFOLLOW)` — a path string is never
/// re-resolved after its parent fd is pinned, so an app-uid process swapping
/// a checked directory for a symlink mid-walk only fails that entry closed
/// (the swap is never followed). Links are handed over themselves and never
/// descended into. Returns false when any entry could not be changed.
#[cfg(unix)]
fn chown_tree(path: &Path, uid: u32, gid: u32) -> bool {
    chown_tree_with(path, uid, gid, &|_, _, _| true)
}

/// [`chown_tree`] with a hook called once per visited path, in the window
/// where the old path-based walk was racable (after the entry is pinned by
/// its parent fd, before descending into it): tests use it to swap an entry
/// mid-walk and prove the swap is never followed.
#[cfg(unix)]
fn chown_tree_with(
    path: &Path,
    uid: u32,
    gid: u32,
    hook: &dyn Fn(&Path, u32, u32) -> bool,
) -> bool {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::RawFd;
    let Ok(root) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `open` with `O_NOFOLLOW|O_DIRECTORY` fails rather than
    // following a link; the fd pins the root for the whole walk. A root
    // link fails closed here.
    let fd: RawFd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return false;
    }
    chown_opened_dir(fd, path, uid, gid, Some(hook), true)
}

/// Hand over the dir pinned by `fd` (owned: listed via `fdopendir`, which
/// closes it, or closed here when that fails). `logical` only labels hook
/// calls and test records — it is never re-resolved. `chown_self` hands
/// over the dir itself too; subdirs arrive already handed over by their
/// parent's `fchownat` and pass false.
#[cfg(unix)]
fn chown_opened_dir(
    fd: std::os::unix::io::RawFd,
    logical: &Path,
    uid: u32,
    gid: u32,
    hook: ChownHook<'_>,
    chown_self: bool,
) -> bool {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;
    let mut ok = true;
    if chown_self {
        // SAFETY: `fchown` acts on the open dir itself; nothing to follow.
        if unsafe { libc::fchown(fd, uid, gid) } != 0 {
            ok = false;
        }
        if let Some(hook) = hook {
            if !hook(logical, uid, gid) {
                ok = false;
            }
        }
    }
    // SAFETY: `fdopendir` takes ownership of `fd` on success; on failure
    // the fd stays ours and is closed below.
    let dirp = unsafe { libc::fdopendir(fd) };
    if dirp.is_null() {
        // SAFETY: `fd` is ours; `fdopendir` failed and did not take it.
        unsafe { libc::close(fd) };
        return false;
    }
    loop {
        // SAFETY: `dirp` is live until `closedir` below; `readdir` returns
        // NULL at the end or a pointer valid until the next call. A NULL
        // from a mid-stream error just ends the walk of this dir; the
        // entries already visited were still handed over by fd.
        let entry = unsafe { libc::readdir(dirp) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `d_name` is NUL-terminated by the kernel.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let child = logical.join(OsStr::from_bytes(name.to_bytes()));
        // SAFETY: `fchownat` with `AT_SYMLINK_NOFOLLOW` changes the entry
        // itself relative to the pinned `fd`; no path is re-resolved.
        if unsafe { libc::fchownat(fd, name.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            ok = false;
        }
        // The old race window: after the entry is pinned by its parent fd,
        // before descending. A swap here only renames what is never
        // re-resolved.
        if let Some(hook) = hook {
            if !hook(&child, uid, gid) {
                ok = false;
            }
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fstatat` reports the entry itself; the buffer is ours.
        if unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            ok = false;
            continue;
        }
        if (st.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            // SAFETY: `openat` with `O_NOFOLLOW` refuses a swapped-in link;
            // the new fd pins the real dir for the recursion.
            let sub = unsafe {
                libc::openat(
                    fd,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if sub < 0 {
                ok = false;
                continue;
            }
            if !chown_opened_dir(sub, &child, uid, gid, hook, false) {
                ok = false;
            }
        }
    }
    // SAFETY: `dirp` is live; `closedir` also closes the `fdopendir` fd.
    unsafe { libc::closedir(dirp) };
    ok
}

/// Cap on `cache/<slug>/tmp` before a build wipes it: bun's node-compile
/// cache is worth keeping warm, but an unbounded tmp is a disk-fill vector
/// for an app filling its own cache.
#[cfg(unix)]
const MAX_CACHE_TMP_BYTES: u64 = 1 << 30;

/// Budget bounding the tmp size walk: an app filling its tmp with millions
/// of entries must not stall the deploy measuring it. Past the budget the
/// tree counts as over cap and gets wiped.
#[cfg(unix)]
const TMP_WALK_BUDGET: u64 = 100_000;

/// Remove the tree pinned by `fd` (owned: consumed via `fdopendir`) without
/// re-resolving a path string: entries are unlinked with `unlinkat` relative
/// to the open parent, subdirs are recursed into with `openat(…,
/// O_NOFOLLOW)` and removed with `AT_REMOVEDIR` once emptied. A symlink is
/// unlinked itself, never followed. Vanished entries count as removed.
#[cfg(unix)]
fn remove_opened_dir(fd: std::os::unix::io::RawFd) -> bool {
    use std::ffi::CStr;
    let mut ok = true;
    // SAFETY: `fdopendir` takes ownership of `fd` on success; on failure
    // the fd stays ours and is closed below.
    let dirp = unsafe { libc::fdopendir(fd) };
    if dirp.is_null() {
        // SAFETY: `fd` is ours; `fdopendir` failed and did not take it.
        unsafe { libc::close(fd) };
        return false;
    }
    loop {
        // SAFETY: `dirp` is live until `closedir` below; see
        // `chown_opened_dir` for the readdir contract. Entries created
        // concurrently may be missed (documented): the caller retries the
        // walk before giving up.
        let entry = unsafe { libc::readdir(dirp) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `d_name` is NUL-terminated by the kernel.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fstatat` reports the entry itself; the buffer is ours.
        if unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
                ok = false;
            }
            continue;
        }
        if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
            // SAFETY: `unlinkat` without flags removes the link or file
            // itself relative to the pinned `fd`.
            if unsafe { libc::unlinkat(fd, name.as_ptr(), 0) } != 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound
            {
                ok = false;
            }
            continue;
        }
        // SAFETY: `openat` with `O_NOFOLLOW` refuses a swapped-in link; the
        // new fd pins the real dir for the recursion.
        let sub = unsafe {
            libc::openat(
                fd,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if sub >= 0 && !remove_opened_dir(sub) {
            ok = false;
            continue;
        }
        // SAFETY: `AT_REMOVEDIR` removes the emptied dir itself relative to
        // the pinned `fd`. A vanished dir is already gone.
        if unsafe { libc::unlinkat(fd, name.as_ptr(), libc::AT_REMOVEDIR) } != 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound
        {
            ok = false;
        }
    }
    // SAFETY: `dirp` is live; `closedir` also closes the `fdopendir` fd.
    unsafe { libc::closedir(dirp) };
    ok
}

/// Whether the tmp tree pinned by `fd` (owned) stays within `limit` bytes
/// (`st_blocks`, so sparseness counts) and `budget` entries, counted with a
/// no-follow walk. False on overuse, over budget, or any error: all three
/// wipe.
#[cfg(unix)]
fn tmp_within_limit(fd: std::os::unix::io::RawFd, limit: u64, budget: u64) -> bool {
    use std::ffi::CStr;
    struct Usage {
        bytes: u64,
        left: u64,
    }
    let mut usage = Usage {
        bytes: 0,
        left: budget,
    };
    fn walk(fd: std::os::unix::io::RawFd, limit: u64, usage: &mut Usage) -> bool {
        // SAFETY: ownership transfer as in `remove_opened_dir`.
        let dirp = unsafe { libc::fdopendir(fd) };
        if dirp.is_null() {
            // SAFETY: `fd` is ours; `fdopendir` failed and did not take it.
            unsafe { libc::close(fd) };
            return false;
        }
        let mut ok = true;
        loop {
            // SAFETY: as in `remove_opened_dir`.
            let entry = unsafe { libc::readdir(dirp) };
            if entry.is_null() {
                break;
            }
            // SAFETY: `d_name` is NUL-terminated by the kernel.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            if usage.left == 0 {
                return false;
            }
            usage.left -= 1;
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: no-follow; the buffer is ours.
            if unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0
            {
                ok = false;
                continue;
            }
            usage.bytes = usage
                .bytes
                .saturating_add((st.st_blocks as u64).saturating_mul(512));
            if usage.bytes > limit {
                // SAFETY: `dirp` is live.
                unsafe { libc::closedir(dirp) };
                return false;
            }
            if (st.st_mode & libc::S_IFMT) == libc::S_IFDIR {
                // SAFETY: `O_NOFOLLOW` pins the real dir.
                let sub = unsafe {
                    libc::openat(
                        fd,
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if sub < 0 {
                    ok = false;
                    continue;
                }
                if !walk(sub, limit, usage) {
                    // SAFETY: `dirp` is live.
                    unsafe { libc::closedir(dirp) };
                    return false;
                }
            }
        }
        // SAFETY: `dirp` is live.
        unsafe { libc::closedir(dirp) };
        ok
    }
    walk(fd, limit, &mut usage)
}

/// Whether `cache/<slug>/tmp` must be wiped: missing (create it), not a real
/// dir (planted link/file), or over [`MAX_CACHE_TMP_BYTES`] by a bounded
/// no-follow walk. Measurement errors wipe too: an unreadable tmp is not a
/// tmp to build on.
#[cfg(unix)]
fn tmp_needs_wipe(cache_root: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let tmp = cache_root.join("tmp");
    let Ok(meta) = std::fs::symlink_metadata(&tmp) else {
        return true;
    };
    if !meta.file_type().is_dir() {
        return true;
    }
    let Ok(path) = std::ffi::CString::new(tmp.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: `O_NOFOLLOW|O_DIRECTORY` pins the real dir or fails.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return true;
    }
    !tmp_within_limit(fd, MAX_CACHE_TMP_BYTES, TMP_WALK_BUDGET)
}

/// Rename `tmp` aside (rename never follows the final component) and remove
/// the renamed tree with an fd-based walk, then recreate `tmp` 0700 for
/// (`uid`, `gid`). Missing `tmp` is just created. Fails closed: a tmp that
/// cannot be removed or recreated fails the step rather than building on an
/// attacker-planted tree.
#[cfg(unix)]
fn wipe_cache_tmp(cache_root: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::RawFd;
    // A missing root is a fresh app (the step's own ensure would create it
    // next): create it here so the wipe below converges. The parents
    // (`<data>/cache`) are platform-owned; a root that exists as a link
    // fails closed at the `O_NOFOLLOW` open below.
    if std::fs::symlink_metadata(cache_root).is_err() {
        std::fs::create_dir_all(cache_root)?;
    }
    let root = CString::new(cache_root.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("cache root has an interior NUL"))?;
    // SAFETY: `O_NOFOLLOW|O_DIRECTORY` pins the real root or fails.
    let fd: RawFd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("opening cache root {}", cache_root.display()));
    }
    struct Close(RawFd);
    impl Drop for Close {
        fn drop(&mut self) {
            // SAFETY: ours until now; nothing else closes it.
            unsafe { libc::close(self.0) };
        }
    }
    let _close = Close(fd);
    let tmp = CString::new("tmp").expect("literal has no NUL");
    if fd_has_entry(fd, &tmp) {
        // First free `tmp.old.<n>`: stale ones mean an earlier wipe died
        // mid-removal; they are cleared in place, then the name is reused.
        let mut old: Option<CString> = None;
        for n in 0..64 {
            let name = CString::new(format!("tmp.old.{n}")).expect("no NUL in a number");
            if clear_stale_old(fd, &name) {
                old = Some(name);
                break;
            }
        }
        let Some(old) = old else {
            anyhow::bail!("no free tmp.old.<n> under {}", cache_root.display());
        };
        // SAFETY: `renameat` never follows the final components; the names
        // themselves move.
        if unsafe { libc::renameat(fd, tmp.as_ptr(), fd, old.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("renaming aside {}", cache_root.join("tmp").display()));
        }
        remove_renamed_tmp(fd, &old, cache_root)?;
    }
    // SAFETY: `mkdirat` never follows the final component.
    if unsafe { libc::mkdirat(fd, tmp.as_ptr(), 0o700) } != 0
        && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("recreating {}", cache_root.join("tmp").display()));
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: no-follow; the buffer is ours.
    if unsafe { libc::fstatat(fd, tmp.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0
        || (st.st_mode & libc::S_IFMT) != libc::S_IFDIR
    {
        anyhow::bail!("tmp under {} is not a real dir", cache_root.display());
    }
    // Best-effort like `lchown_one`: production runs as root and hands the
    // fresh dir over; an unprivileged (dev, test) outer cannot chown to an
    // app uid and must not fail the deploy for it — the step itself runs as
    // the current user there anyway.
    // SAFETY: `fchownat` with `AT_SYMLINK_NOFOLLOW` changes the confirmed
    // dir itself.
    if unsafe { libc::fchownat(fd, tmp.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        tracing::debug!(uid, gid, "tmp handover failed; continuing");
    }
    Ok(())
}

/// Whether `name` exists under the pinned `fd` (no follow).
#[cfg(unix)]
fn fd_has_entry(fd: std::os::unix::io::RawFd, name: &std::ffi::CString) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: no-follow; the buffer is ours.
    (unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0)
}

/// Make `name` under the pinned `fd` reusable as a rename target: missing is
/// free; a non-dir is unlinked (never followed); a leftover dir is removed
/// in place with retries. False when the name stays occupied.
#[cfg(unix)]
fn clear_stale_old(fd: std::os::unix::io::RawFd, name: &std::ffi::CString) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: no-follow; the buffer is ours.
    if unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return true;
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        // SAFETY: not a dir; removing the name itself never follows.
        return (unsafe { libc::unlinkat(fd, name.as_ptr(), 0) } == 0);
    }
    remove_child_dir(fd, name)
}

/// Remove the dir `name` under the pinned `fd` with an fd-based walk,
/// retrying twice for entries created concurrently with a walk. The opened
/// fd is consumed by each walk; a name that vanishes counts as removed.
#[cfg(unix)]
fn remove_child_dir(fd: std::os::unix::io::RawFd, name: &std::ffi::CString) -> bool {
    for _ in 0..3 {
        // SAFETY: `O_NOFOLLOW` pins the real dir.
        let sub = unsafe {
            libc::openat(
                fd,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if sub < 0 {
            // A link, file, or vanished name: unlinking the name itself
            // (never the target) converges it.
            // SAFETY: never follows the final component.
            return (unsafe { libc::unlinkat(fd, name.as_ptr(), 0) } == 0)
                || std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound;
        }
        let emptied = remove_opened_dir(sub);
        // SAFETY: `AT_REMOVEDIR` removes the emptied dir itself.
        if unsafe { libc::unlinkat(fd, name.as_ptr(), libc::AT_REMOVEDIR) } == 0 {
            return true;
        }
        if emptied && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            return true;
        }
    }
    false
}

/// Remove the renamed-aside tmp (`old` under the pinned `fd`): it may be a
/// dir (walk + `AT_REMOVEDIR`, one retry), a link or file (unlink the name),
/// or already vanished. Anything else fails the wipe closed.
#[cfg(unix)]
fn remove_renamed_tmp(
    fd: std::os::unix::io::RawFd,
    old: &std::ffi::CString,
    cache_root: &Path,
) -> anyhow::Result<()> {
    if remove_child_dir(fd, old) {
        return Ok(());
    }
    anyhow::bail!("removing {}", cache_root.join("tmp").display())
}

/// Mark the deploy `deploying` before the migrate/upload steps run. A separate
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
    kind: DeployKind,
    now: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO deploy (id, app_id, sha, kind, status, created_at) VALUES (?, ?, ?, ?, 'queued', ?)",
    )
    .bind(deploy_id)
    .bind(app_id)
    .bind(sha)
    .bind(kind.sql())
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

/// The app's latest successful deploy sha, if any. Read fresh from the row
/// (never from the caller's snapshot) to decide first-deploy routing: a
/// previous deploy in this process may have succeeded after the caller
/// loaded the app.
async fn last_deploy_sha(pool: &SqlitePool, app_id: &str) -> anyhow::Result<Option<String>> {
    let (sha,): (Option<String>,) = sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = ?1")
        .bind(app_id)
        .fetch_one(pool)
        .await?;
    Ok(sha)
}

/// The app's latest successful preview deploy sha, if any: the first-deploy
/// routing signal for preview runs (a preview fleet starts from the preview
/// upload, not the live one). Scoped to the current lineage: discard and
/// promote delete the app's preview rows, so a fresh preview always routes
/// as a first deploy (upload before migrate) against its empty namespace.
async fn last_preview_success(pool: &SqlitePool, app_id: &str) -> anyhow::Result<Option<String>> {
    let sha: Option<String> = sqlx::query_scalar(
        "SELECT sha FROM deploy WHERE app_id = ?1 AND kind = 'preview' AND status = 'success' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(app_id)
    .fetch_optional(pool)
    .await?;
    Ok(sha)
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

/// A blocked deploy: `status = 'blocked'` with the findings JSON. The
/// `moderation_check` row (stage `gate`) was already written by the gate
/// itself; `last_deploy_sha` is untouched so the previous version serves.
async fn finish_blocked(
    pool: &SqlitePool,
    deploy_id: &str,
    blocked: &BlockedInfo,
    steps_json: &str,
    log_tail: &str,
) -> anyhow::Result<()> {
    let findings_json =
        serde_json::to_string(&blocked.findings).unwrap_or_else(|_| "[]".to_string());
    sqlx::query(
        "UPDATE deploy SET status = 'blocked', error = ?, findings = ?, steps = ?, log_tail = ?, finished_at = ? WHERE id = ?",
    )
    .bind(blocked.reason)
    .bind(findings_json)
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
    use crate::moderation::jev::fake::{envelope, FakeJev, Responder};
    use crate::moderation::jev::JevConfig;
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
    fn astro_website_config_passes_key_validation() {
        // The shape the website template's `celld-config` integration derives
        // into `dist/wrangler.json` from the Cloudflare adapter's
        // `dist/server/wrangler.json` (verbatim from a real `bun run build`):
        // `main` rewrites to `server/…`, assets to `client`, `vars` to `{}`.
        // Every key must stay inside ACCEPTED_CONFIG_KEYS, so an adapter
        // addition fails loudly here instead of slipping into a deploy.
        let config = serde_json::json!({
            "name": "outer-website",
            "main": "server/entry.mjs",
            "compatibility_date": "2026-09-01",
            "compatibility_flags": ["nodejs_als"],
            "durable_objects": {"bindings": []},
            "migrations": [],
            "assets": {"binding": "ASSETS", "directory": "client"},
            "services": [],
            "triggers": {},
            "vars": {},
            "d1_databases": [{"binding": "DB", "database_name": "site-db", "migrations_dir": "db/migrations"}],
            "kv_namespaces": [],
            "queues": {"producers": [], "consumers": []},
            "workflows": [],
            "r2_buckets": [],
        });
        validate_config_keys(&config).unwrap();
        let databases = crate::deploy::migrate::d1_databases(&config).unwrap();
        assert_eq!(databases.len(), 1);
        assert_eq!(databases[0].binding, "DB");
        assert_eq!(
            crate::deploy::migrate::migrations_dir_name(&databases[0]),
            "db/migrations"
        );
    }

    #[tokio::test]
    async fn astro_website_layout_selects_dist_and_stages_migrations() {
        // The website template's build output layout: `dist/wrangler.json`
        // wins discovery (over a root config, of which there is none), and
        // the repo's `db/migrations` stages into the deploy root for celld.
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 1);
        let app = test_app(10013);
        let work = deployer.work_dir(&app.slug);
        let dist = work.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(
            dist.join("wrangler.json"),
            r#"{"name":"outer-website","main":"server/entry.mjs","compatibility_date":"2026-09-01","compatibility_flags":["nodejs_als"],"assets":{"binding":"ASSETS","directory":"client"},"vars":{},"d1_databases":[{"binding":"DB","database_name":"site-db","migrations_dir":"db/migrations"}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(work.join("db/migrations")).unwrap();
        std::fs::write(
            work.join("db/migrations/0001_newsletter.sql"),
            "CREATE TABLE IF NOT EXISTS newsletter (id INTEGER PRIMARY KEY AUTOINCREMENT, email TEXT NOT NULL UNIQUE);\n",
        )
        .unwrap();
        assert_eq!(
            find_deploy_config(&work).unwrap().unwrap(),
            dist.join("wrangler.json")
        );
        let (root, crons, _) = deployer.deploy_root(&app).unwrap();
        assert_eq!(root.root, dist);
        assert!(crons.is_empty(), "a website ships no cron schedules");
        let staged = crate::deploy::migrate::stage_migrations(&root.root, &work, "db/migrations")
            .unwrap()
            .unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].name, "0001_newsletter.sql");
        assert!(
            root.root
                .join("db/migrations/0001_newsletter.sql")
                .is_file(),
            "reviewed worktree bytes reach the deploy root"
        );
    }

    #[test]
    fn second_d1_database_fails_with_the_cap_message() {
        let two = serde_json::json!({
            "name": "app",
            "main": "index.js",
            "d1_databases": [
                {"binding": "DB", "database_name": "app-db"},
                {"binding": "AUX", "database_name": "aux-db"},
            ],
        });
        let err = validate_config_keys(&two).unwrap_err();
        assert_eq!(format!("{err:#}"), BINDING_CAP_MESSAGE);
    }

    #[test]
    fn second_r2_bucket_fails_with_the_cap_message() {
        let two = serde_json::json!({
            "name": "app",
            "main": "index.js",
            "r2_buckets": [
                {"binding": "FILES", "bucket_name": "files"},
                {"binding": "AVATARS", "bucket_name": "avatars"},
            ],
        });
        let err = validate_config_keys(&two).unwrap_err();
        assert_eq!(format!("{err:#}"), BINDING_CAP_MESSAGE);
    }

    #[test]
    fn one_d1_database_and_one_r2_bucket_pass_validation() {
        let one_each = serde_json::json!({
            "name": "app",
            "main": "index.js",
            "d1_databases": [{"binding": "DB", "database_name": "app-db"}],
            "r2_buckets": [{"binding": "FILES", "bucket_name": "files"}],
        });
        validate_config_keys(&one_each).unwrap();
    }

    #[test]
    fn deploy_root_prefers_dist_then_jsonc_then_json() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path();
        assert!(find_deploy_config(work).unwrap().is_none());

        std::fs::write(work.join("wrangler.json"), "{}").unwrap();
        assert_eq!(
            find_deploy_config(work).unwrap().unwrap(),
            work.join("wrangler.json")
        );

        std::fs::write(work.join("wrangler.jsonc"), "{}").unwrap();
        assert_eq!(
            find_deploy_config(work).unwrap().unwrap(),
            work.join("wrangler.jsonc")
        );

        std::fs::create_dir_all(work.join("dist")).unwrap();
        std::fs::write(work.join("dist").join("wrangler.json"), "{}").unwrap();
        assert_eq!(
            find_deploy_config(work).unwrap().unwrap(),
            work.join("dist").join("wrangler.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_discovery_refuses_a_symlinked_config() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path();
        // The attacker's target: planted elsewhere, must stay untouched.
        let elsewhere = dir.path().join("elsewhere.json");
        std::fs::write(&elsewhere, r#"{"name":"victim"}"#).unwrap();
        std::fs::create_dir_all(work.join("dist")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, work.join("dist").join("wrangler.json")).unwrap();
        let err = format!("{:#}", find_deploy_config(work).unwrap_err());
        assert!(
            err.contains("symlink"),
            "fails closed naming the link: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            r#"{"name":"victim"}"#,
            "the link target is never read or written"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_discovery_refuses_a_symlinked_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path();
        let real_dist = dir.path().join("real-dist");
        std::fs::create_dir_all(&real_dist).unwrap();
        std::fs::write(real_dist.join("wrangler.json"), "{}").unwrap();
        std::os::unix::fs::symlink(&real_dist, work.join("dist")).unwrap();
        let err = format!("{:#}", find_deploy_config(work).unwrap_err());
        assert!(
            err.contains("symlink"),
            "fails closed naming the link: {err}"
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
        let cfg = GitConfig::new(dir.path().join("git"), None);
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
            apps_domain: "outer.localhost".to_string(),
            cache_purge: None,
            jev: None,
            wake: None,
            secrets_key: crate::secrets::SecretsKey::test_key(),
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
            always_on: false,
            preview_sha: None,
            preview_updated_at: None,
            preview_port: None,
            preview_internal_port: None,
        }
    }

    #[tokio::test]
    async fn worktree_materialization_tracks_the_tree_but_keeps_node_modules() {
        let (_dir, cfg, _store, _sync, sha_a, sha_b) = seed_mirror("grocery").await;
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 1);
        let app = test_app(10000);

        deployer
            .materialize(&cfg, &app.slug.clone(), &app, &sha_a)
            .await
            .unwrap();
        let work = deployer.work_dir(&app.slug);
        assert_eq!(
            std::fs::read_to_string(work.join("index.ts")).unwrap(),
            "v1"
        );

        // Untracked build residue plus the one directory clean must spare.
        std::fs::create_dir_all(work.join("node_modules")).unwrap();
        std::fs::write(work.join("node_modules").join("keepme.js"), "x").unwrap();
        std::fs::write(work.join("old.ts"), "stale overwrite").unwrap();

        deployer
            .materialize(&cfg, &app.slug.clone(), &app, &sha_b)
            .await
            .unwrap();
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
        let pool = test_pool_in(dir.path()).await;
        (dir, pool)
    }
    /// A pool whose database lives in `dir`; the caller keeps `dir` alive
    /// for as long as the pool may open new connections.
    async fn test_pool_in(dir: &Path) -> SqlitePool {
        let pool = crate::db::open(&dir.join("outer.sqlite")).await.unwrap();
        sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('owner1', 'o@example.com', 0, 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        pool
    }
    fn jev_client(base_url: &str) -> JevClient {
        let mut config = JevConfig::new(
            "test-account".to_string(),
            "test-token".to_string(),
            "outer".to_string(),
        );
        config.api_base = base_url.to_string();
        config.timeout = Duration::from_secs(2);
        JevClient::new(config).unwrap()
    }

    /// Jev flags chunks containing the marker at profanity 0.95; every
    /// other signal is clear.
    fn block_responder() -> Responder {
        Arc::new(|body| {
            let mut answers = serde_json::Map::new();
            let state = body.pointer("/input/state").unwrap().as_object().unwrap();
            let questions = body
                .pointer("/input/questions")
                .unwrap()
                .as_object()
                .unwrap();
            for (qid, question) in questions {
                let slot = qid.split('_').next().unwrap_or("");
                let text = state.get(slot).and_then(|v| v.as_str()).unwrap_or("");
                let hot = text.contains("fuck");
                let value = if question.get("type").and_then(|t| t.as_str()) == Some("score") {
                    serde_json::json!({"type": "score", "score": 0.0})
                } else if qid.ends_with("_profanity") {
                    serde_json::json!({"type": "noul", "noul": if hot { 0.95 } else { 0.01 }})
                } else {
                    serde_json::json!({"type": "noul", "noul": 0.01})
                };
                answers.insert(qid.clone(), value);
            }
            (200, envelope(serde_json::json!({"answers": answers})))
        })
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
        assert_eq!(
            names,
            ["worktree", "install", "build", "gate", "deploy-root"]
        );
        assert!(!outcome.steps[0].skipped, "worktree always runs");
        assert!(
            outcome.steps[1..3].iter().all(|s| s.skipped),
            "no package.json skips install and build: {:?}",
            outcome.steps
        );
        assert!(!outcome.steps[3].skipped, "the gate always runs");

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
        assert_eq!(steps.len(), 5, "the failed step is the last steps entry");
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

        let check: (String, String) =
            sqlx::query_as("SELECT stage, verdict FROM moderation_check WHERE app_id = ?")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(check, ("gate".to_string(), "pass".to_string()));
    }

    /// `DeploySettings` aimed at `fake`: the purge posts to the fake
    /// Cloudflare instead of the real API.
    fn purge_settings(
        data: &tempfile::TempDir,
        fake: &crate::cloudflare::fake::FakePurge,
    ) -> DeploySettings {
        let mut settings = test_settings(data.path().to_path_buf());
        settings.cache_purge = Some(
            crate::cloudflare::CachePurge::new("zone-1".to_string(), "token-1".to_string())
                .with_api_base(fake.base_url.clone()),
        );
        settings
    }

    /// The CLI exits right after `deploy` returns: draining must not come
    /// back before the detached purge reached Cloudflare.
    #[tokio::test]
    async fn drain_waits_for_in_flight_purges() {
        let data = tempfile::tempdir().unwrap();
        let fake = crate::cloudflare::fake::FakePurge::start().await;
        let deployer = Deployer::new(purge_settings(&data, &fake), 1);
        let (_ddir, pool) = test_pool().await;
        // An `active` custom hostname purges alongside the app host.
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, last_activity_at, created_at) \
             VALUES ('app1', 'grocery', 'owner1', 200001, 21001, 31001, 'unlisted', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app_domain (hostname, app_id, cf_custom_hostname_id, status, created_at) \
             VALUES ('shop.example.com', 'app1', 'cf-1', 'active', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        deployer.spawn_edge_purge(&pool, "app1", "grocery", DeployKind::Main);
        deployer.purges().drain().await;
        assert_eq!(fake.wait_for(2, std::time::Duration::from_secs(5)).await, 2);
        let recorded = fake.requests.lock().unwrap();
        let hosts: Vec<_> = recorded
            .iter()
            .map(|r| r.body["hosts"][0].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            hosts,
            vec![
                format!("grocery.{}", deployer.settings.apps_domain),
                "shop.example.com".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn failed_deploy_issues_no_purge() {
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
        let purge = crate::cloudflare::fake::FakePurge::start().await;
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(purge_settings(&data, &purge), 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha_b).await.unwrap();
        assert_eq!(outcome.status, "failed");
        // No spawn happens on the failure path at all; the settle wait only
        // guards against a stray detached task.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            purge.calls(),
            0,
            "a failed deploy leaves the old version serving: nothing to purge"
        );
    }

    #[tokio::test]
    async fn blocked_deploy_issues_no_purge() {
        // Same profane tree as `blocked_deploy_never_reaches_the_deploy_step`.
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "profane".to_string(),
                base: None,
                writes: vec![
                    write(
                        "package.json",
                        r#"{"name":"t","scripts":{"build":"echo hi"}}"#,
                    ),
                    write(
                        "pages/index.tsx",
                        "export default () => <h1>this list is fuck awesome</h1>;",
                    ),
                    write(
                        "dist/wrangler.json",
                        r#"{"name":"t","main":"index.js","compatibility_date":"2026-01-01"}"#,
                    ),
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let (_ddir, pool) = test_pool().await;
        let app = test_app(10010);
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
        let fake = FakeJev::start(block_responder()).await;
        let purge = crate::cloudflare::fake::FakePurge::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut settings = purge_settings(&data, &purge);
        settings.bun_bin = "/bin/true".to_string();
        settings.celld_bin = "/nonexistent/celld-binary".to_string();
        settings.jev = Some(jev_client(&fake.base_url));
        let deployer = Deployer::new(settings, 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "blocked");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            purge.calls(),
            0,
            "a blocked deploy uploads nothing: the fleet is untouched, nothing to purge"
        );
    }
    #[tokio::test]
    async fn blocked_deploy_never_reaches_the_deploy_step() {
        // A profane JSX line with a valid deploy-root config behind it.
        // Install and build are no-op `true`; `celld` is missing, so a
        // pass-through would fail at `deploy`: `blocked` proves the gate
        // stopped the pipeline, with nothing uploaded.
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "profane".to_string(),
                base: None,
                writes: vec![
                    write(
                        "package.json",
                        r#"{"name":"t","scripts":{"build":"echo hi"}}"#,
                    ),
                    write(
                        "pages/index.tsx",
                        "export default () => <h1>this list is fuck awesome</h1>;",
                    ),
                    write(
                        "dist/wrangler.json",
                        r#"{"name":"t","main":"index.js","compatibility_date":"2026-01-01"}"#,
                    ),
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10010);
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

        // Jev flags profanity: `blocked` proves the gate stopped the
        // pipeline, with nothing uploaded. Install and build are no-op
        // `true`; `celld` is missing, so a pass-through would fail at
        // `deploy` instead.
        let fake = FakeJev::start(block_responder()).await;
        let data = tempfile::tempdir().unwrap();
        let mut settings = test_settings(data.path().to_path_buf());
        settings.bun_bin = "/bin/true".to_string();
        settings.celld_bin = "/nonexistent/celld-binary".to_string();
        settings.jev = Some(jev_client(&fake.base_url));
        let deployer = Deployer::new(settings, 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "blocked");
        assert_eq!(outcome.failed_step.as_deref(), Some("gate"));
        assert_eq!(outcome.reason.as_deref(), Some("profanity"));
        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].path, "pages/index.tsx");
        assert_eq!(outcome.findings[0].line, 1);
        assert_eq!(outcome.findings[0].excerpt, "this list is fuck awesome");
        assert_eq!(outcome.findings[0].category, "profanity");
        assert_eq!(outcome.findings[0].signal, "jev");
        let message = outcome.user_message.as_deref().unwrap_or("");
        assert!(
            message.contains("pages/index.tsx, line 1"),
            "agent-facing line names path and line: {message}"
        );
        let names: Vec<&str> = outcome.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["worktree", "install", "build", "gate"]);

        let row: (String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT status, error, findings FROM deploy WHERE app_id = ?")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "blocked");
        assert_eq!(row.1.as_deref(), Some("profanity"));
        assert!(
            row.2.as_deref().unwrap_or("").contains("pages/index.tsx"),
            "findings JSON is in the row"
        );
        let check: (String, String, String) = sqlx::query_as(
            "SELECT stage, verdict, findings FROM moderation_check WHERE app_id = ?",
        )
        .bind(&app.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(check.0, "gate");
        assert_eq!(check.1, "blocked");
        assert!(check.2.contains("pages/index.tsx"));
        let (last_sha,): (Option<String>,) =
            sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = ?")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            last_sha, None,
            "a blocked deploy never touches last_deploy_sha: the previous version serves"
        );
    }
    #[tokio::test]
    async fn unavailable_jev_fails_the_deploy_closed_at_gate() {
        // Same tree as the blocked test, but Jev answers 500: the deploy
        // fails at `gate` with `review_unavailable` instead of blocking.
        // Nothing is uploaded, the previous version keeps serving, and
        // nothing is cached as clear.
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "clean".to_string(),
                base: None,
                writes: vec![
                    write(
                        "package.json",
                        r#"{"name":"t","scripts":{"build":"echo hi"}}"#,
                    ),
                    write(
                        "pages/index.tsx",
                        "export default () => <h1>Welcome to my grocery list</h1>;",
                    ),
                    write(
                        "dist/wrangler.json",
                        r#"{"name":"t","main":"index.js","compatibility_date":"2026-01-01"}"#,
                    ),
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10011);
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

        let fake = FakeJev::start(Arc::new(|_| (500, serde_json::json!({"error": "down"})))).await;
        let data = tempfile::tempdir().unwrap();
        let mut settings = test_settings(data.path().to_path_buf());
        settings.bun_bin = "/bin/true".to_string();
        settings.celld_bin = "/nonexistent/celld-binary".to_string();
        settings.jev = Some(jev_client(&fake.base_url));
        let deployer = Deployer::new(settings, 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failed_step.as_deref(), Some("gate"));
        assert_eq!(outcome.error.as_deref(), Some("review_unavailable"));
        let message = outcome.user_message.as_deref().unwrap_or("");
        assert!(
            message.contains("temporarily unavailable"),
            "agent-facing line says to try again shortly: {message}"
        );
        assert!(
            message.contains("previous version"),
            "agent-facing line says the old version serves: {message}"
        );
        let names: Vec<&str> = outcome.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["worktree", "install", "build", "gate"]);

        let row: (String, Option<String>) =
            sqlx::query_as("SELECT status, error FROM deploy WHERE app_id = ?")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("review_unavailable"));
        let (last_sha,): (Option<String>,) =
            sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = ?")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            last_sha, None,
            "a review-unavailable deploy never touches last_deploy_sha"
        );
        let (texts,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM moderation_text")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(texts, 0, "nothing is cached as clear");
        let (checks,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM moderation_check")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(checks, 0, "nothing is recorded");
    }

    /// A fake `celld` for migrate tests: answers `deploy` with JSON and
    /// simulates `d1 migrations apply` by walking `<root>/migrations`
    /// in order. A `<root>/.fake-fail` file names the migration that must
    /// fail, like a syntax error would. Bucket-shaped state never leaves the
    /// test: the fake treats `--bucket s3://<dir>/fleets/<slug>` as a state
    /// dir (`<dir>` is a tempdir), recording calls and env there, so counts
    /// survive the worktree wipe between deploys.
    fn install_fake_celld(state: &Path) -> PathBuf {
        let script = state.join("fake-celld");
        let tmp = script.with_extension("tmp");
        std::fs::write(
            &tmp,
            r#"#!/bin/sh
state_of_bucket() {
  prev=""
  for a in "$@"; do
    if [ "$prev" = "--bucket" ]; then printf '%s' "$a"; return 0; fi
    prev="$a"
  done
}
if [ "$1" = "deploy" ]; then
  bucket=$(state_of_bucket "$@")
  statedir=$(printf '%s' "$bucket" | sed -e 's|^s3://||' -e 's|/fleets/.*||')
  mkdir -p "$statedir"
   printf '%s\n' "$*" >> "$statedir/calls.log"
   root="$2"
   for f in "$root"/wrangler.json "$root"/wrangler.jsonc; do
     [ -f "$f" ] && cp "$f" "$statedir/deployed-config"
   done
   printf '{"version":"fake-v1"}\n'
  exit 0
fi
bucket=$(state_of_bucket "$@")
statedir=$(printf '%s' "$bucket" | sed -e 's|^s3://||' -e 's|/fleets/.*||')
mkdir -p "$statedir"
printf '%s\n' "$*" >> "$statedir/calls.log"
env | grep '^AWS_' | sort >> "$statedir/env.log"
root="$5"
failpat=""
if [ -f "$root/.fake-fail" ]; then failpat=$(cat "$root/.fake-fail"); fi
for f in "$root"/migrations/*.sql; do
  [ -e "$f" ] || continue
  name=$(basename "$f")
  if [ -n "$failpat" ] && [ "$name" = "$failpat" ]; then
    echo "error applying $name: near \"oops\": syntax error" >&2
    exit 1
  fi
  echo "applied $name"
done
exit 0
"#,
        )
        .unwrap();
        // Atomic install (write tmp, chmod, rename): a deploy exec racing a
        // direct write fails ETXTBSY, and the parallel suite forks freely.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::rename(&tmp, &script).unwrap();
        script
    }
    /// A fake `bun` for the install and build steps: records argv and the
    /// full step env, then succeeds. Secrets must never appear in a build
    /// step's env (builds run untrusted code), so the test greps this log.
    fn install_fake_bun(state: &Path) -> PathBuf {
        let script = state.join("fake-bun");
        // Atomic install like the celld fake above: execs must never catch
        // the script mid-write (ETXTBSY under the parallel suite).
        let tmp = script.with_extension("tmp");
        std::fs::write(
             &tmp,
             format!(
                 "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}/build-calls.log\"\nenv | sort >> \"{}/build-env.log\"\nexit 0\n",
                 state.display(),
                 state.display()
             ),
         )
         .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::rename(&tmp, &script).unwrap();
        script
    }

    fn migrate_settings(data: &tempfile::TempDir, state: &Path, script: &Path) -> DeploySettings {
        let mut settings = test_settings(data.path().to_path_buf());
        settings.celld_bin = script.to_string_lossy().into_owned();
        settings.bun_bin = "/bin/true".to_string();
        // The fake reads its state dir back out of the bucket URL.
        settings.storage.bucket = state.to_string_lossy().into_owned();
        settings
    }

    fn apply_calls(state: &Path) -> usize {
        let log = state.join("calls.log");
        if !log.is_file() {
            return 0;
        }
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|l| l.contains("d1 migrations apply"))
            .count()
    }

    /// How many `celld deploy` uploads the fake saw.
    fn upload_calls(state: &Path) -> usize {
        let log = state.join("calls.log");
        if !log.is_file() {
            return 0;
        }
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("deploy "))
            .count()
    }

    /// The fake-celld subcommand sequence: `deploy` for an upload,
    /// `apply` for a `d1 migrations apply`. The fake logs one line per
    /// invocation in order, so this pins migrate-vs-upload ordering.
    fn call_kinds(state: &Path) -> Vec<&'static str> {
        let log = state.join("calls.log");
        if !log.is_file() {
            return Vec::new();
        }
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| {
                if l.starts_with("deploy ") {
                    "deploy"
                } else if l.contains("d1 migrations apply") {
                    "apply"
                } else {
                    "other"
                }
            })
            .collect()
    }

    async fn insert_test_app(pool: &SqlitePool, app: &App) {
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) VALUES (?, ?, ?, ?, ?, ?, 0, 0)",
        )
        .bind(&app.id)
        .bind(&app.slug)
        .bind(&app.owner_id)
        .bind(app.uid as i64)
        .bind(app.port as i64)
        .bind(app.internal_port as i64)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A commit carrying a valid deploy root, a clean page, and the given
    /// extra files (migration files, sentinels). An extra file at a default
    /// path replaces the default, so tests can commit a profane page.
    async fn commit_tree(
        cfg: &GitConfig,
        store: &Store,
        sync: &GitSync,
        slug: &str,
        extra: Vec<FileWrite>,
    ) -> String {
        let paths: Vec<&str> = extra.iter().map(|w| w.path.as_str()).collect();
        let mut writes: Vec<FileWrite> = [
            (
                "dist/wrangler.json",
                r#"{"name":"t","main":"index.js","compatibility_date":"2026-01-01","d1_databases":[{"binding":"DB","database_name":"app-db"}]}"#,
            ),
            (
                "pages/index.tsx",
                "export default () => <h1>grocery list</h1>;",
            ),
        ]
        .into_iter()
        .filter(|(path, _)| !paths.contains(path))
        .map(|(path, content)| write(path, content))
        .collect();
        writes.extend(extra);
        commit::commit(
            store,
            cfg,
            sync,
            slug,
            CommitRequest {
                message: "migrate test".to_string(),
                base: None,
                writes,
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn migrate_applies_pending_in_order_and_records_them() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit_tree(
            &cfg,
            &store,
            &sync,
            "grocery",
            vec![
                write(
                    "db/migrations/0001_init.sql",
                    "CREATE TABLE item (id INTEGER);",
                ),
                write(
                    "db/migrations/0002_add_owner.sql",
                    "ALTER TABLE item ADD COLUMN owner TEXT;",
                ),
            ],
        )
        .await;

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10101);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "success", "error: {:?}", outcome.error);
        // A first deploy (`last_deploy_sha` NULL) uploads before it
        // migrates: `celld d1` needs a running fleet and nothing is
        // published yet.
        let names: Vec<&str> = outcome.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "worktree",
                "install",
                "build",
                "gate",
                "deploy-root",
                "deploy",
                "migrate"
            ]
        );
        assert!(
            !outcome.steps.last().unwrap().skipped,
            "pending files run the step"
        );
        assert!(
            outcome.log_tail.contains("applied 0001_init.sql")
                && outcome.log_tail.contains("applied 0002_add_owner.sql"),
            "the celld output is journaled: {}",
            outcome.log_tail
        );

        // Staged into the deploy root, in order, and recorded per file.
        let root = deployer.work_dir(&app.slug).join("dist");
        assert!(root.join("migrations").join("0001_init.sql").is_file());
        assert!(root.join("migrations").join("0002_add_owner.sql").is_file());
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT name, sha256 FROM d1_migration WHERE app_id = ?1 ORDER BY name")
                .bind(&app.id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "0001_init.sql");
        assert_eq!(rows[1].0, "0002_add_owner.sql");
        assert_ne!(rows[0].1, rows[1].1, "distinct content hashes");

        // The apply ran once, against this app's database and prefix grant.
        // The upload runs first so the fleet can exist for the apply; the
        // proxy still serves nothing until this success lands.
        assert_eq!(apply_calls(state.path()), 1);
        assert_eq!(call_kinds(state.path()), ["deploy", "apply"]);
        let calls = std::fs::read_to_string(state.path().join("calls.log")).unwrap();
        assert!(
            calls.contains("d1 migrations apply app-db"),
            "the database name travels: {calls}"
        );
        assert!(
            calls.contains(&format!("fleets/{}", app.slug)),
            "the scoped prefix grant travels: {calls}"
        );
        let env = std::fs::read_to_string(state.path().join("env.log")).unwrap();
        assert!(
            env.contains("AWS_ACCESS_KEY_ID=outeraccess"),
            "the scoped key travels, never the parent: {env}"
        );

        let (last_sha,): (Option<String>,) =
            sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = ?1")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(last_sha.as_deref(), Some(sha.as_str()));
    }
    #[tokio::test]
    async fn secrets_land_in_deploy_vars_not_in_build_env() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        // A code-defined `vars` entry: secrets merge over it, and the merge
        // is journaled by name. `package.json` forces the install and build
        // steps through the fake `bun`, which records their env.
        let sha = commit_tree(
             &cfg,
             &store,
             &sync,
             "grocery",
             vec![
                 write(
                     "dist/wrangler.json",
                     r#"{"name":"t","main":"index.js","compatibility_date":"2026-01-01","vars":{"CLASH":"code","KEPT":"code"}}"#,
                 ),
                 write("package.json", r#"{"name":"t","scripts":{"build":"echo built"}}"#),
                 write("bun.lock", "lock"),
             ],
         )
         .await;
        let (_ddir, pool) = test_pool().await;
        let app = test_app(10201);
        insert_test_app(&pool, &app).await;
        let key = crate::secrets::SecretsKey::test_key();
        let marker = "DEPLOY_SECRET_MARKER_8H7G6F";
        for (name, value) in [("MY_KEY", marker), ("CLASH", "secret-wins")] {
            let blob = crate::secrets::encrypt(&key, &app.id, name, value.as_bytes());
            let capped = crate::secrets::set(&pool, &app.id, name, &blob, 1)
                .await
                .unwrap();
            assert!(!capped, "{name} fits");
        }
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let bun = install_fake_bun(state.path());
        let data = tempfile::tempdir().unwrap();
        let mut settings = migrate_settings(&data, state.path(), &script);
        settings.bun_bin = bun.to_string_lossy().into_owned();
        let deployer = Deployer::new(settings, 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "success", "error: {:?}", outcome.error);
        // The uploaded config carries the decrypted values: the fresh var,
        // the overwrite winning over code, and the untouched code var.
        let deployed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(state.path().join("deployed-config")).unwrap(),
        )
        .unwrap();
        assert_eq!(deployed["vars"]["MY_KEY"], marker);
        assert_eq!(deployed["vars"]["CLASH"], "secret-wins");
        assert_eq!(deployed["vars"]["KEPT"], "code");
        assert!(
            outcome.log_tail.contains("injected 2 vars"),
            "journaled: {}",
            outcome.log_tail
        );
        assert!(
            outcome.log_tail.contains("CLASH"),
            "overwrite journaled: {}",
            outcome.log_tail
        );
        // Both sandboxed build steps ran through the fake, and neither saw
        // a value: builds run untrusted code, so they never get secrets.
        let build_calls = std::fs::read_to_string(state.path().join("build-calls.log")).unwrap();
        assert!(
            build_calls.contains("install"),
            "install ran: {build_calls}"
        );
        assert!(
            build_calls.contains("run build"),
            "build ran: {build_calls}"
        );
        let build_env = std::fs::read_to_string(state.path().join("build-env.log")).unwrap();
        assert!(!build_env.contains(marker), "build env carries no secret");
        assert!(
            !build_env.contains("secret-wins"),
            "build env carries no secret"
        );
        // The worktree config is restored byte-identically: no plaintext
        // lingers on disk past the upload.
        let work_config = deployer.work_dir(&app.slug).join("dist/wrangler.json");
        let restored = std::fs::read_to_string(&work_config).unwrap();
        assert!(!restored.contains(marker), "scrubbed: {restored}");
        assert!(!restored.contains("secret-wins"), "scrubbed: {restored}");
        let restored_json: serde_json::Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(
            restored_json["vars"],
            serde_json::json!({"CLASH": "code", "KEPT": "code"}),
            "byte-identical restore"
        );
    }

    /// Deploy-root config, one secret row per `(name, value)`, and the
    /// pristine config bytes: the harness every injection test builds on.
    #[cfg(unix)]
    async fn inject_harness(
        rows: &[(&str, &str)],
    ) -> (
        tempfile::TempDir,
        Deployer,
        SqlitePool,
        App,
        PathBuf,
        Vec<u8>,
    ) {
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 1);
        // The database lives in `data`, which the caller keeps: a TempDir
        // dropped here would delete it while the pool can still open new
        // connections ("unable to open database file", flaky in CI).
        let pool = test_pool_in(data.path()).await;
        let app = test_app(10301);
        insert_test_app(&pool, &app).await;
        let key = crate::secrets::SecretsKey::test_key();
        for (name, value) in rows {
            let blob = crate::secrets::encrypt(&key, &app.id, name, value.as_bytes());
            crate::secrets::set(&pool, &app.id, name, &blob, 1)
                .await
                .unwrap();
        }
        let work = deployer.work_dir(&app.slug);
        std::fs::create_dir_all(work.join("dist")).unwrap();
        let config = work.join("dist").join("wrangler.json");
        let original =
            br#"{"name":"t","main":"index.js","compatibility_date":"2026-01-01","vars":{"KEPT":"code"}}"#
                .to_vec();
        std::fs::write(&config, &original).unwrap();
        (data, deployer, pool, app, config, original)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inject_guard_restores_byte_identically_on_drop_and_clears_sidecar() {
        use std::os::unix::fs::PermissionsExt;
        let (_data, deployer, pool, app, config, original) =
            inject_harness(&[("MY_KEY", "s3cr3t")]).await;
        let mode_before = std::fs::metadata(&config).unwrap().permissions().mode();
        let (root, _, _) = deployer.deploy_root(&app).unwrap();
        assert_eq!(root.config, config);
        let guard = deployer
            .inject_secrets(&pool, &app, &root)
            .await
            .unwrap()
            .expect("one usable row injects");
        assert_eq!(guard.injected, 1);
        // The sidecar lands before the worktree file is touched, and the
        // patched file is 0600 from creation — never write-then-chmod.
        let sidecar_dir = secret_sidecar_dir(&deployer.settings.data_dir);
        assert!(sidecar_dir.join("grocery.orig").is_file());
        assert!(sidecar_dir.join("grocery.target").is_file());
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o600,
            "injected config is owner-only"
        );
        assert!(
            std::fs::read_to_string(&config).unwrap().contains("s3cr3t"),
            "plaintext present in the window"
        );
        drop(guard);
        assert_eq!(std::fs::read(&config).unwrap(), original, "byte-identical");
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions().mode(),
            mode_before,
            "mode restored"
        );
        assert!(
            !sidecar_dir.join("grocery.orig").exists(),
            "sidecar cleared after restore"
        );
        assert!(!sidecar_dir.join("grocery.target").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inject_guard_restores_when_the_upload_panics() {
        let (_data, deployer, pool, app, config, original) =
            inject_harness(&[("MY_KEY", "s3cr3t")]).await;
        let (root, _, _) = deployer.deploy_root(&app).unwrap();
        let guard = deployer
            .inject_secrets(&pool, &app, &root)
            .await
            .unwrap()
            .expect("one usable row injects");
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = guard;
            panic!("simulated upload panic");
        }));
        assert!(crashed.is_err(), "the panic really happened");
        assert_eq!(
            std::fs::read(&config).unwrap(),
            original,
            "Drop scrubbed the plaintext on the unwind path"
        );
    }

    #[cfg(unix)]
    /// `celld deploy` runs as the app uid: the replaced config must keep the
    /// original's owner, or the upload can't read it (found live: EACCES on
    /// a root-owned 0600 `dist/wrangler.json`). Needs root to chown.
    #[test]
    #[ignore = "privileged: needs root to own a file as another uid"]
    fn atomic_replace_keeps_the_original_owner() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrangler.json");
        std::fs::write(&path, b"{}").unwrap();
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::chown(c.as_ptr(), 12345, 12345) }, 0);
        atomic_replace_no_follow(&path, br#"{"vars":{}}"#, 0o600).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (12345, 12345));
        assert_eq!(meta.mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn inject_refuses_symlinked_config_and_leaves_target_untouched() {
        let (_data, deployer, pool, app, config, _original) =
            inject_harness(&[("MY_KEY", "s3cr3t")]).await;
        let elsewhere = deployer.settings.data_dir.join("elsewhere.json");
        std::fs::write(&elsewhere, r#"{"name":"victim"}"#).unwrap();
        std::fs::remove_file(&config).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &config).unwrap();
        // Discovery fails closed first …
        let err = format!("{:#}", deployer.deploy_root(&app).unwrap_err());
        assert!(err.contains("symlink"), "fails closed: {err}");
        // … and the sink re-checks even when handed the path directly.
        let root = DeployRoot {
            root: config.parent().unwrap().to_path_buf(),
            config: config.clone(),
        };
        let err = match deployer.inject_secrets(&pool, &app, &root).await {
            Ok(_) => panic!("injecting through a symlink must fail"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("symlink"), "fails closed: {err}");
        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            r#"{"name":"victim"}"#,
            "never written through the link"
        );
        assert!(
            !secret_sidecar_dir(&deployer.settings.data_dir)
                .join("grocery.orig")
                .exists(),
            "no sidecar stashed for a refused inject"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inject_skips_reserved_and_misshapen_secret_rows() {
        // `set` takes whatever the DB holds: rows written before the
        // tool-time checks (or straight into the DB) reach the sink.
        let (_data, deployer, pool, app, config, _original) = inject_harness(&[
            ("GOOD_KEY", "good"),
            ("PORT", "platform-override"),
            ("BUN_FOO", "bun-claim"),
            ("BUN_INSTALL_CACHE_DIR", "cache-claim"),
            ("lower", "misshapen"),
        ])
        .await;
        let (root, _, _) = deployer.deploy_root(&app).unwrap();
        let guard = deployer
            .inject_secrets(&pool, &app, &root)
            .await
            .unwrap()
            .expect("one usable row still injects");
        assert_eq!(guard.injected, 1, "only GOOD_KEY counts");
        assert!(guard.overwritten.is_empty());
        let patched: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(patched["vars"]["GOOD_KEY"], "good");
        for banned in ["PORT", "BUN_FOO", "BUN_INSTALL_CACHE_DIR", "lower"] {
            assert!(
                patched["vars"].get(banned).is_none(),
                "{banned} must never reach platform vars"
            );
        }
        drop(guard);
    }

    #[cfg(unix)]
    #[test]
    fn boot_scrub_replays_sidecar_and_deletes_it() {
        use std::os::unix::fs::PermissionsExt;
        let data = tempfile::tempdir().unwrap();
        let config_dir = data.path().join("work").join("grocery").join("dist");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = config_dir.join("wrangler.json");
        let original = br#"{"name":"t","vars":{}}"#.to_vec();
        std::fs::write(&config, &original).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o640)).unwrap();
        // Simulate the crash: stash, then clobber the worktree file.
        let (orig_sidecar, target_sidecar) =
            stash_secret_sidecar(data.path(), "grocery", &config, 0o640, &original).unwrap();
        std::fs::write(&config, r#"{"name":"t","vars":{"MY_KEY":"plaintext"}}"#).unwrap();
        assert_eq!(scrub_interrupted_injections(data.path()).unwrap(), 1);
        assert_eq!(std::fs::read(&config).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o640,
            "mode replayed from the sidecar"
        );
        assert!(!orig_sidecar.exists() && !target_sidecar.exists());
        assert_eq!(
            scrub_interrupted_injections(data.path()).unwrap(),
            0,
            "idempotent"
        );
        // An orphaned `.orig` (target write crashed) is dropped, never
        // replayed blindly: there is no verified destination.
        std::fs::write(
            data.path().join("secret-sidecars").join("grocery.orig"),
            b"x",
        )
        .unwrap();
        assert_eq!(scrub_interrupted_injections(data.path()).unwrap(), 0);
        assert!(!data
            .path()
            .join("secret-sidecars")
            .join("grocery.orig")
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn boot_scrub_keeps_sidecar_when_config_is_a_link() {
        let data = tempfile::tempdir().unwrap();
        let config_dir = data.path().join("work").join("grocery").join("dist");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = config_dir.join("wrangler.json");
        let original = br#"{"name":"t","vars":{}}"#.to_vec();
        std::fs::write(&config, &original).unwrap();
        let (orig_sidecar, _) =
            stash_secret_sidecar(data.path(), "grocery", &config, 0o644, &original).unwrap();
        let elsewhere = data.path().join("elsewhere.json");
        std::fs::write(&elsewhere, r#"{"name":"victim"}"#).unwrap();
        std::fs::remove_file(&config).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &config).unwrap();
        assert_eq!(scrub_interrupted_injections(data.path()).unwrap(), 0);
        assert!(orig_sidecar.exists(), "kept for the next boot");
        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            r#"{"name":"victim"}"#,
            "never written through the link"
        );
    }

    #[tokio::test]
    async fn migrate_runs_before_upload_for_existing_app() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha_a = commit_tree(
            &cfg,
            &store,
            &sync,
            "grocery",
            vec![write(
                "db/migrations/0001_init.sql",
                "CREATE TABLE item (id INTEGER);",
            )],
        )
        .await;

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10110);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let first = deployer.deploy(&pool, &cfg, &app, &sha_a).await.unwrap();
        assert_eq!(first.status, "success", "error: {:?}", first.error);
        // The first deploy has no live version yet, so it uploads before it
        // migrates.
        let first_names: Vec<&str> = first.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            first_names,
            [
                "worktree",
                "install",
                "build",
                "gate",
                "deploy-root",
                "deploy",
                "migrate"
            ]
        );
        // A second tree with one pending migration: the app has a live
        // version now, so the migrate step must run before the upload — an
        // awake fleet must never serve new code against the old schema.
        let sha_b = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "second migration".to_string(),
                base: Some(sha_a.clone()),
                writes: vec![write(
                    "db/migrations/0002_add_owner.sql",
                    "ALTER TABLE item ADD COLUMN owner TEXT;",
                )],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha_b).await.unwrap();
        assert_eq!(outcome.status, "success", "error: {:?}", outcome.error);
        let names: Vec<&str> = outcome.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "worktree",
                "install",
                "build",
                "gate",
                "deploy-root",
                "migrate",
                "deploy"
            ]
        );
        let kinds = call_kinds(state.path());
        assert_eq!(
            kinds,
            ["deploy", "apply", "apply", "deploy"],
            "the first deploy uploads before it migrates; the existing-app deploy migrates before its upload"
        );
        let (last_sha,): (Option<String>,) =
            sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = ?1")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(last_sha.as_deref(), Some(sha_b.as_str()));
    }

    #[tokio::test]
    async fn migrate_failure_skips_upload_for_existing_app() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha_a = commit_tree(
            &cfg,
            &store,
            &sync,
            "grocery",
            vec![write(
                "db/migrations/0001_init.sql",
                "CREATE TABLE item (id INTEGER);",
            )],
        )
        .await;

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10111);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let first = deployer.deploy(&pool, &cfg, &app, &sha_a).await.unwrap();
        assert_eq!(first.status, "success", "error: {:?}", first.error);
        assert_eq!(upload_calls(state.path()), 1);

        // A pending migration that fails: the live deploy must fail before
        // any upload, so the previous version keeps serving with certainty.
        let sha_b = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "bad migration".to_string(),
                base: Some(sha_a.clone()),
                writes: vec![
                    write("db/migrations/0002_bad.sql", "CREATE TABLE oops (;"),
                    write("dist/.fake-fail", "0002_bad.sql"),
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha_b).await.unwrap();
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failed_step.as_deref(), Some("migrate"));
        assert_eq!(
            upload_calls(state.path()),
            1,
            "the failed migrate uploads nothing"
        );
        let names: Vec<&str> = outcome.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "worktree",
                "install",
                "build",
                "gate",
                "deploy-root",
                "migrate"
            ],
            "the deploy step is never reached"
        );
        let (last_sha,): (Option<String>,) =
            sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = ?1")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            last_sha.as_deref(),
            Some(sha_a.as_str()),
            "a failed migrate never touches last_deploy_sha"
        );
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM d1_migration")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "a failed apply records nothing new");
    }

    #[tokio::test]
    async fn migrate_skips_when_everything_is_already_applied() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit_tree(
            &cfg,
            &store,
            &sync,
            "grocery",
            vec![write(
                "db/migrations/0001_init.sql",
                "CREATE TABLE item (id INTEGER);",
            )],
        )
        .await;

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10102);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let first = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(first.status, "success", "error: {:?}", first.error);
        assert_eq!(apply_calls(state.path()), 1);

        // Same tree again: nothing pending, no second apply, no wake.
        let second = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(second.status, "success", "error: {:?}", second.error);
        let migrate = second
            .steps
            .iter()
            .find(|s| s.name == "migrate")
            .expect("the migrate step is journaled");
        assert!(migrate.skipped, "all files recorded: {migrate:?}");
        assert_eq!(
            apply_calls(state.path()),
            1,
            "no celld call when nothing is pending"
        );
    }

    #[tokio::test]
    async fn migrate_skips_without_d1_or_files() {
        // No `d1_databases` declared: the step skips without touching celld.
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "no d1".to_string(),
                base: None,
                writes: vec![
                    write(
                        "dist/wrangler.json",
                        r#"{"name":"t","main":"index.js","compatibility_date":"2026-01-01"}"#,
                    ),
                    write("pages/index.tsx", "export default () => <h1>hi</h1>;"),
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10103);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "success", "error: {:?}", outcome.error);
        let migrate = outcome
            .steps
            .iter()
            .find(|s| s.name == "migrate")
            .expect("the migrate step is journaled");
        assert!(migrate.skipped);
        assert!(
            outcome.log_tail.contains("no d1_databases"),
            "the journal says why: {}",
            outcome.log_tail
        );
        assert_eq!(apply_calls(state.path()), 0, "celld never runs");
    }

    #[tokio::test]
    async fn migrate_skips_with_d1_but_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit_tree(&cfg, &store, &sync, "grocery", vec![]).await;

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10104);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "success", "error: {:?}", outcome.error);
        let migrate = outcome
            .steps
            .iter()
            .find(|s| s.name == "migrate")
            .expect("the migrate step is journaled");
        assert!(migrate.skipped);
        assert_eq!(apply_calls(state.path()), 0, "celld never runs");
    }

    #[tokio::test]
    async fn first_deploy_migrate_failure_uploads_then_fails_not_live() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit_tree(
            &cfg,
            &store,
            &sync,
            "grocery",
            vec![
                write(
                    "db/migrations/0001_init.sql",
                    "CREATE TABLE item (id INTEGER);",
                ),
                write("db/migrations/0002_bad.sql", "CREATE TABLE oops (;"),
                write("dist/.fake-fail", "0002_bad.sql"),
            ],
        )
        .await;

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10105);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failed_step.as_deref(), Some("migrate"));
        let error = outcome.error.as_deref().unwrap_or("");
        assert!(
            error.contains("0002_bad.sql"),
            "the error names the failing file: {error}"
        );
        let message = outcome.user_message.as_deref().unwrap_or("");
        assert!(
            message.contains("0002_bad.sql") && message.contains("isn't live yet"),
            "the agent learns which file failed and that nothing is live: {message}"
        );
        assert!(
            outcome.log_tail.contains("0002_bad.sql"),
            "the migration error is in the log tail"
        );
        // Nothing recorded and the app stays not-live: there is no previous
        // version, and the proxy still refuses to forward.
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM d1_migration")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a failed apply records nothing");
        let (last_sha,): (Option<String>,) =
            sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = ?1")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(last_sha, None, "a failed first deploy never goes live");
        // The upload ran first (the fleet must exist before `celld d1` can
        // reach it); the failure is in the migrate step that followed it.
        assert_eq!(upload_calls(state.path()), 1);
        assert_eq!(
            call_kinds(state.path()),
            ["deploy", "apply"],
            "a first deploy uploads before it migrates, even when the migration fails"
        );
        let names: Vec<&str> = outcome.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "worktree",
                "install",
                "build",
                "gate",
                "deploy-root",
                "deploy",
                "migrate"
            ]
        );
    }

    #[tokio::test]
    async fn failed_first_deploy_retries_on_first_deploy_path() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha_a = commit_tree(
            &cfg,
            &store,
            &sync,
            "grocery",
            vec![
                write(
                    "db/migrations/0001_init.sql",
                    "CREATE TABLE item (id INTEGER);",
                ),
                write("db/migrations/0002_bad.sql", "CREATE TABLE oops (;"),
                write("dist/.fake-fail", "0002_bad.sql"),
            ],
        )
        .await;
        let (_ddir, pool) = test_pool().await;
        let app = test_app(10112);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let failed = deployer.deploy(&pool, &cfg, &app, &sha_a).await.unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.failed_step.as_deref(), Some("migrate"));
        // Still no successful deploy, so the retry is a first deploy too:
        // it uploads before it migrates again — and this time succeeds.
        // (celld skips already-applied files on its own; the failed apply
        // recorded nothing, so both files apply fresh here.)
        let sha_b = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "fix migration".to_string(),
                base: Some(sha_a.clone()),
                writes: vec![],
                edits: vec![crate::git::commit::Edit {
                    path: "db/migrations/0002_bad.sql".to_string(),
                    old: "CREATE TABLE oops (;".to_string(),
                    new: "CREATE TABLE oops (id INTEGER);".to_string(),
                    replace_all: false,
                }],
                deletes: vec!["dist/.fake-fail".to_string()],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha_b).await.unwrap();
        assert_eq!(outcome.status, "success", "error: {:?}", outcome.error);
        let names: Vec<&str> = outcome.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "worktree",
                "install",
                "build",
                "gate",
                "deploy-root",
                "deploy",
                "migrate"
            ]
        );
        assert_eq!(
            call_kinds(state.path()),
            ["deploy", "apply", "deploy", "apply"],
            "the retry is still a first deploy: upload before migrate"
        );
        let (last_sha,): (Option<String>,) =
            sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = ?1")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(last_sha.as_deref(), Some(sha_b.as_str()));
    }

    #[tokio::test]
    async fn migrate_failure_on_edited_applied_file_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha_a = commit_tree(
            &cfg,
            &store,
            &sync,
            "grocery",
            vec![write(
                "db/migrations/0001_init.sql",
                "CREATE TABLE item (id INTEGER);",
            )],
        )
        .await;

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10106);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let first = deployer.deploy(&pool, &cfg, &app, &sha_a).await.unwrap();
        assert_eq!(first.status, "success", "error: {:?}", first.error);
        assert_eq!(apply_calls(state.path()), 1);

        // Same name, new content: the deploy fails before celld runs again.
        let sha_b = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "edit applied migration".to_string(),
                base: Some(sha_a.clone()),
                writes: vec![],
                edits: vec![crate::git::commit::Edit {
                    path: "db/migrations/0001_init.sql".to_string(),
                    old: "CREATE TABLE item (id INTEGER);".to_string(),
                    new: "CREATE TABLE item (id TEXT);".to_string(),
                    replace_all: false,
                }],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha_b).await.unwrap();
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failed_step.as_deref(), Some("migrate"));
        let error = outcome.error.as_deref().unwrap_or("");
        assert!(
            error.contains("0001_init.sql") && error.contains("edited"),
            "the error names the edited file: {error}"
        );
        let message = outcome.user_message.as_deref().unwrap_or("");
        assert!(
            message.contains("0001_init.sql") && message.contains("new migration"),
            "the agent learns to write a new file: {message}"
        );
        assert_eq!(
            apply_calls(state.path()),
            1,
            "an edited file never reaches celld again"
        );
        assert_eq!(
            upload_calls(state.path()),
            1,
            "the edit refusal happens before any second upload"
        );
        let (last_sha,): (Option<String>,) =
            sqlx::query_as("SELECT last_deploy_sha FROM app WHERE id = ?1")
                .bind(&app.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(last_sha.as_deref(), Some(sha_a.as_str()));
    }

    #[tokio::test]
    async fn blocked_gate_never_migrates() {
        // A profane page with a valid deploy root and migration files: the
        // gate blocks, and celld (deploy and migrate alike) never runs.
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit_tree(
            &cfg,
            &store,
            &sync,
            "grocery",
            vec![
                write(
                    "pages/index.tsx",
                    "export default () => <h1>this list is fuck awesome</h1>;",
                ),
                write(
                    "db/migrations/0001_init.sql",
                    "CREATE TABLE item (id INTEGER);",
                ),
            ],
        )
        .await;

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10107);
        insert_test_app(&pool, &app).await;
        let fake = FakeJev::start(block_responder()).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let mut settings = migrate_settings(&data, state.path(), &script);
        settings.bun_bin = "/bin/true".to_string();
        settings.jev = Some(jev_client(&fake.base_url));
        let deployer = Deployer::new(settings, 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "blocked");
        assert_eq!(
            apply_calls(state.path()),
            0,
            "the gate runs before migrations"
        );
        assert!(
            !state.path().join("calls.log").is_file(),
            "celld never runs at all: the deploy step is never reached"
        );
    }

    #[tokio::test]
    async fn migrate_applies_the_declared_database() {
        // One database: the deploy cap (see BINDING_CAP_MESSAGE) rejects a
        // second entry at deploy-root, so a full deploy only ever migrates
        // the single declared database. Multi-entry parsing stays covered
        // by `migrate::d1_databases` unit tests.
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "one database".to_string(),
                base: None,
                writes: vec![
                    write(
                        "dist/wrangler.json",
                        r#"{"name":"t","main":"index.js","compatibility_date":"2026-01-01","d1_databases":[{"binding":"DB","database_name":"app-db"}]}"#,
                    ),
                    write("pages/index.tsx", "export default () => <h1>hi</h1>;"),
                    write("db/migrations/0001_init.sql", "CREATE TABLE item (id INTEGER);"),
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10108);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "success", "error: {:?}", outcome.error);
        assert_eq!(apply_calls(state.path()), 1, "one apply for the database");
        let calls = std::fs::read_to_string(state.path().join("calls.log")).unwrap();
        assert!(calls.contains("d1 migrations apply app-db"), "{calls}");
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM d1_migration WHERE app_id = ?1 AND database_identity = ?2",
        )
        .bind(&app.id)
        .bind("app-db")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "recorded under the effective identity");
    }

    #[tokio::test]
    async fn changing_database_id_reapplies_to_the_new_database() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let wrangler = |id: &str| {
            write(
                "dist/wrangler.json",
                &format!(
                    "{{\"name\":\"t\",\"main\":\"index.js\",\"compatibility_date\":\"2026-01-01\",\
                      \"d1_databases\":[{{\"binding\":\"DB\",\"database_name\":\"app-db\",\
                      \"database_id\":\"{id}\"}}]}}"
                ),
            )
        };
        let sha_a = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "first id".to_string(),
                base: None,
                writes: vec![
                    wrangler("db-id-1"),
                    write("pages/index.tsx", "export default () => <h1>hi</h1>;"),
                    write(
                        "db/migrations/0001_init.sql",
                        "CREATE TABLE item (id INTEGER);",
                    ),
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();

        let (_ddir, pool) = test_pool().await;
        let app = test_app(10109);
        insert_test_app(&pool, &app).await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(migrate_settings(&data, state.path(), &script), 1);
        let first = deployer.deploy(&pool, &cfg, &app, &sha_a).await.unwrap();
        assert_eq!(first.status, "success", "error: {:?}", first.error);
        assert_eq!(apply_calls(state.path()), 1);

        // Same name, new id: celld sees a new (empty) database, so the same
        // files are pending again and apply a second time.
        let sha_b = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "second id".to_string(),
                base: Some(sha_a.clone()),
                writes: vec![wrangler("db-id-2")],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha_b).await.unwrap();
        assert_eq!(outcome.status, "success", "error: {:?}", outcome.error);
        assert_eq!(apply_calls(state.path()), 2, "the new identity re-applies");
        let mut identities: Vec<(String,)> =
            sqlx::query_as("SELECT database_identity FROM d1_migration WHERE app_id = ?1")
                .bind(&app.id)
                .fetch_all(&pool)
                .await
                .unwrap();
        identities.sort();
        assert_eq!(
            identities,
            [("db-id-1".to_string(),), ("db-id-2".to_string(),)],
            "each identity pins its own hash"
        );
    }

    /// The real `celld d1 migrations` contract the pipeline depends on:
    /// `apply` is idempotent (a second run is a no-op success) and `list`
    /// shows the applied file. Needs a live fleet, so it runs only when the
    /// integrator sets it up at merge time:
    /// `OUTER_LIVE_D1_BUCKET` (e.g. `s3://outer/fleets/pwa-check`),
    /// `OUTER_LIVE_D1_PROJECT` (a dir with the app's wrangler config and a
    /// `migrations/` dir holding `0001_outer_smoke.sql`), `OUTER_LIVE_D1_DB`
    /// (the `database_name`), plus the bucket credentials in the env and a
    /// fleet serving that app. Uses the real `celld` on PATH, unsandboxed:
    /// it pins the CLI surface, not the pipeline (the fake covers that).
    #[tokio::test]
    #[ignore = "needs a live fleet: OUTER_LIVE_D1_BUCKET, OUTER_LIVE_D1_PROJECT, OUTER_LIVE_D1_DB"]
    async fn live_celld_migrations_apply_is_idempotent() {
        let bucket = std::env::var("OUTER_LIVE_D1_BUCKET")
            .expect("OUTER_LIVE_D1_BUCKET must be set (e.g. s3://outer/fleets/pwa-check)");
        let project = std::env::var("OUTER_LIVE_D1_PROJECT")
            .expect("OUTER_LIVE_D1_PROJECT must be set (dir with wrangler config + migrations/)");
        let db = std::env::var("OUTER_LIVE_D1_DB")
            .expect("OUTER_LIVE_D1_DB must be set (the d1_databases database_name)");
        let mut extra = Vec::new();
        if let Ok(endpoint) = std::env::var("OUTER_LIVE_D1_ENDPOINT") {
            extra.push("--endpoint".to_string());
            extra.push(endpoint);
        }
        if let Ok(region) = std::env::var("OUTER_LIVE_D1_REGION") {
            extra.push("--region".to_string());
            extra.push(region);
        }
        let run = |args: &[String]| {
            let args = args.to_vec();
            let extra = extra.clone();
            let bucket = bucket.clone();
            async move {
                let mut cmd = tokio::process::Command::new("celld");
                cmd.args(["d1", "migrations"]);
                cmd.args(&args);
                cmd.args(["--bucket", bucket.as_str()]);
                cmd.args(&extra);
                cmd.output().await.expect("celld must be on PATH")
            }
        };
        let project_arg = project.clone();
        let db_arg = db.clone();
        let first = run(&["apply".to_string(), db_arg.clone(), project_arg.clone()]).await;
        assert!(
            first.status.success(),
            "apply must succeed: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        let second = run(&["apply".to_string(), db_arg.clone(), project_arg.clone()]).await;
        assert!(
            second.status.success(),
            "a second apply is a no-op success: {}",
            String::from_utf8_lossy(&second.stderr)
        );
        let mut list = tokio::process::Command::new("celld");
        list.args([
            "d1",
            "migrations",
            "list",
            db_arg.as_str(),
            project_arg.as_str(),
            "--bucket",
            bucket.as_str(),
        ]);
        list.args(&extra);
        let list = list.output().await.expect("celld must be on PATH");
        assert!(
            list.status.success(),
            "list must succeed: {}",
            String::from_utf8_lossy(&list.stderr)
        );
        let stdout = String::from_utf8_lossy(&list.stdout);
        assert!(
            stdout.contains("0001_outer_smoke"),
            "list shows the applied file: {stdout}"
        );
    }

    #[test]
    fn deploy_root_returns_validated_crons() {
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 1);
        let app = test_app(10000);
        let work = deployer.work_dir(&app.slug);
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join("wrangler.json"),
            r#"{"name":"grocery","main":"index.js","triggers":{"crons":["*/15 * * * *","0 9 * * MON"]}}"#,
        )
        .unwrap();
        let (deploy_root, crons, _) = deployer.deploy_root(&app).unwrap();
        assert_eq!(deploy_root.root, work);
        assert_eq!(deploy_root.config, work.join("wrangler.json"));
        assert_eq!(crons, ["*/15 * * * *", "0 9 * * MON"]);
    }

    #[test]
    fn deploy_root_rejects_an_invalid_cron_naming_it() {
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 1);
        let app = test_app(10000);
        let work = deployer.work_dir(&app.slug);
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join("wrangler.json"),
            r#"{"name":"grocery","triggers":{"crons":["every hour"]}}"#,
        )
        .unwrap();
        let err = format!("{:#}", deployer.deploy_root(&app).unwrap_err());
        assert!(
            err.contains("every hour"),
            "deploy-root names the bad expression: {err}"
        );
    }

    #[tokio::test]
    async fn deploy_with_invalid_cron_fails_at_deploy_root_and_writes_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "seed".to_string(),
                base: None,
                writes: vec![
                    write("index.ts", "v1"),
                    write(
                        "wrangler.json",
                        r#"{"name":"grocery","main":"index.js","triggers":{"crons":["not-a-cron"]}}"#,
                    ),
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
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

        // No package.json: install and build skip without binaries, and the
        // bad cron stops the deploy at deploy-root, before celld runs.
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failed_step.as_deref(), Some("deploy-root"));
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap_or("")
                .contains("not-a-cron"),
            "the error names the expression: {:?}",
            outcome.error
        );
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM app_cron WHERE app_id = ?1")
            .bind(&app.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a failed deploy writes no cron rows");
    }

    #[tokio::test]
    async fn deploy_with_two_d1_databases_fails_at_deploy_root() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "seed".to_string(),
                base: None,
                writes: vec![
                    write("index.ts", "v1"),
                    write(
                        "wrangler.json",
                        r#"{"name":"grocery","main":"index.js","d1_databases":[{"binding":"DB","database_name":"app-db"},{"binding":"AUX","database_name":"aux-db"}]}"#,
                    ),
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let (_ddir, pool) = test_pool().await;
        let app = test_app(10011);
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

        // No package.json: install and build skip without binaries, and the
        // second database stops the deploy at deploy-root with the cap
        // message, before migrations or celld run.
        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failed_step.as_deref(), Some("deploy-root"));
        assert_eq!(outcome.error.as_deref(), Some(BINDING_CAP_MESSAGE));
        assert_eq!(outcome.user_message.as_deref(), Some(BINDING_CAP_MESSAGE));
    }

    #[tokio::test]
    async fn deploy_with_two_r2_buckets_fails_at_deploy_root() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let sha = commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "seed".to_string(),
                base: None,
                writes: vec![
                    write("index.ts", "v1"),
                    write(
                        "wrangler.json",
                        r#"{"name":"grocery","main":"index.js","r2_buckets":[{"binding":"FILES","bucket_name":"files"},{"binding":"AVATARS","bucket_name":"avatars"}]}"#,
                    ),
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let (_ddir, pool) = test_pool().await;
        let app = test_app(10012);
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

        let data = tempfile::tempdir().unwrap();
        let deployer = Deployer::new(test_settings(data.path().to_path_buf()), 1);
        let outcome = deployer.deploy(&pool, &cfg, &app, &sha).await.unwrap();
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failed_step.as_deref(), Some("deploy-root"));
        assert_eq!(outcome.error.as_deref(), Some(BINDING_CAP_MESSAGE));
        assert_eq!(outcome.user_message.as_deref(), Some(BINDING_CAP_MESSAGE));
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
        let cfg = GitConfig::new(dir.path().join("git"), None);
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
            always_on: false,
            preview_sha: None,
            preview_updated_at: None,
            preview_port: None,
            preview_internal_port: None,
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
            apps_domain: "outer.now".to_string(),
            cache_purge: None,
            jev: None,
            wake: None,
            secrets_key: crate::secrets::SecretsKey::test_key(),
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
        let me = unsafe { libc::geteuid() };
        let mine = unsafe { libc::getegid() };
        let visited = RefCell::new(Vec::new());
        // The walk really chowns (to our own ids, so it succeeds
        // unprivileged) while the hook records the visited paths.
        let ok = chown_tree_with(&root, me, mine, &|p, uid, gid| {
            assert_eq!((uid, gid), (me, mine));
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

    /// The old walk (`symlink_metadata` then `read_dir` on a path string)
    /// was TOCTOU-racable: an app-uid process swapping a checked dir for a
    /// symlink between the check and the listing made root traverse and
    /// hand over an arbitrary tree. The hook replays that swap in the same
    /// window (after the entry is pinned, before descending): the fd walk
    /// must never follow it.
    #[test]
    #[cfg(unix)]
    fn chown_tree_ignores_a_mid_walk_dir_for_symlink_swap() {
        use std::cell::{Cell, RefCell};
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "platform").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file.txt"), "file").unwrap();
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), "inner").unwrap();
        let me = unsafe { libc::geteuid() };
        let mine = unsafe { libc::getegid() };
        let swapped = Cell::new(false);
        let visited = RefCell::new(Vec::new());
        let ok = chown_tree_with(&root, me, mine, &|p, _, _| {
            visited.borrow_mut().push(p.to_path_buf());
            if p == sub && !swapped.get() {
                swapped.set(true);
                // The attacker: replace the checked dir with a link to an
                // outside tree before the walk descends into it.
                std::fs::rename(&sub, root.join("sub.orig")).unwrap();
                symlink(outside.path(), &sub).unwrap();
            }
            true
        });
        assert!(swapped.get(), "the swap must have run mid-walk");
        // The swap is not followed: the post-swap `fstatat` reports a link,
        // which is handed over itself and never descended into — while
        // everything pinned is still handed over: success.
        assert!(ok, "the rest of the tree is still handed over");
        let visited = visited.borrow();
        assert!(
            !visited.iter().any(|p| p.starts_with(outside.path())),
            "nothing outside the tree was visited: {visited:?}"
        );
        assert!(
            !visited.contains(&sub.join("inner.txt")),
            "the pre-swap contents were not descended into either: {visited:?}"
        );
        for expected in [root.clone(), root.join("file.txt"), sub.clone()] {
            assert!(
                visited.contains(&expected),
                "pinned entries are still handed over: {expected:?} not in {visited:?}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            "platform",
            "the outside tree is untouched"
        );
    }

    /// The tmp wipe removes the whole tree without following links: a
    /// symlinked tmp entry and a dir link into an outside tree are unlinked
    /// themselves, and the fresh tmp is a real 0700 dir for the app ids.
    #[test]
    #[cfg(unix)]
    fn wipe_cache_tmp_removes_the_tree_without_following_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let outside = tempfile::tempdir().unwrap();
        let marker = outside.path().join("platform.txt");
        std::fs::write(&marker, "platform").unwrap();
        let sub = outside.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), "inner").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let tmp = cache.join("tmp");
        std::fs::create_dir_all(tmp.join("realdir")).unwrap();
        std::fs::write(tmp.join("a.txt"), "a").unwrap();
        std::fs::write(tmp.join("realdir").join("b.txt"), "b").unwrap();
        symlink(outside.path().join("platform.txt"), tmp.join("link")).unwrap();
        symlink(&sub, tmp.join("dirlink")).unwrap();
        let me = unsafe { libc::geteuid() };
        let mine = unsafe { libc::getegid() };
        wipe_cache_tmp(&cache, me, mine).unwrap();
        let meta = std::fs::symlink_metadata(&tmp).unwrap();
        assert!(meta.file_type().is_dir(), "tmp is a real dir again");
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::read_dir(&tmp).unwrap().count(),
            0,
            "the old tree is gone"
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "platform");
        assert_eq!(
            std::fs::read_to_string(sub.join("inner.txt")).unwrap(),
            "inner",
            "the linked outside dir is untouched"
        );
    }

    /// A planted file or link in tmp's place is replaced with a real dir
    /// (never followed), and a stale `tmp.old.0` from a died wipe is cleared
    /// instead of blocking the rename.
    #[test]
    #[cfg(unix)]
    fn wipe_cache_tmp_replaces_planted_non_dirs_and_clears_stale_olds() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        let marker = outside.path().join("platform.txt");
        std::fs::write(&marker, "platform").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        symlink(&marker, cache.join("tmp")).unwrap();
        let stale = cache.join("tmp.old.0");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("junk.txt"), "junk").unwrap();
        let me = unsafe { libc::geteuid() };
        let mine = unsafe { libc::getegid() };
        wipe_cache_tmp(&cache, me, mine).unwrap();
        assert!(
            std::fs::symlink_metadata(cache.join("tmp"))
                .unwrap()
                .file_type()
                .is_dir(),
            "planted link replaced with a real dir"
        );
        assert!(
            !cache.join("tmp.old.0").exists(),
            "the stale rename target was cleared and reused"
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "platform");
        // Missing tmp is just created.
        std::fs::remove_dir(cache.join("tmp")).unwrap();
        wipe_cache_tmp(&cache, me, mine).unwrap();
        assert!(cache.join("tmp").is_dir());
    }

    /// Open `path` `O_NOFOLLOW|O_DIRECTORY` for the `tmp_within_limit`
    /// tests below.
    #[cfg(unix)]
    fn open_dir_for_test(path: &Path) -> std::os::unix::io::RawFd {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: test-only opener for a tempdir path.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        assert!(fd >= 0, "opening test dir {}", path.to_string_lossy());
        fd
    }

    /// The size walk counts real bytes (never following links), fails a
    /// tiny limit, and counts an entry flood as over cap without walking
    /// it all.
    #[test]
    #[cfg(unix)]
    fn tmp_within_limit_counts_bytes_and_bounds_the_walk() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("big.txt"), "x".repeat(4096)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("tmp");
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("a.txt"), "hello").unwrap();
        symlink(outside.path().join("big.txt"), tmp.join("link")).unwrap();
        // A link counts as itself, not its 4 KiB target.
        assert!(tmp_within_limit(open_dir_for_test(&tmp), 1 << 20, 1000));
        assert!(
            !tmp_within_limit(open_dir_for_test(&tmp), 0, 1000),
            "any content exceeds a zero limit"
        );
        // Two entries (a.txt, link, sub) under a budget of one: over.
        assert!(
            !tmp_within_limit(open_dir_for_test(&tmp), 1 << 20, 1),
            "an entry flood counts as over cap"
        );
    }

    /// `tmp_needs_wipe`: missing or planted tmp wipes (it is created or
    /// replaced), a small real dir does not.
    #[test]
    #[cfg(unix)]
    fn tmp_needs_wipe_flags_missing_and_planted_but_keeps_small_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        assert!(tmp_needs_wipe(&cache), "missing tmp is created by the wipe");
        std::os::unix::fs::symlink(dir.path(), cache.join("tmp")).unwrap();
        assert!(tmp_needs_wipe(&cache), "a planted link is replaced");
        std::fs::remove_file(cache.join("tmp")).unwrap();
        std::fs::create_dir_all(cache.join("tmp").join("sub")).unwrap();
        std::fs::write(cache.join("tmp").join("a.txt"), "hello").unwrap();
        assert!(
            !tmp_needs_wipe(&cache),
            "a small real tmp survives under the 1 GiB cap"
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

    /// A restart leaves in-flight rows behind; boot closes exactly those.
    #[tokio::test]
    async fn boot_fails_only_rows_a_restart_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('a1', 'o@example.com', 0, 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, last_activity_at, created_at) \
             VALUES ('app1', 'boot', 'a1', 210002, 32121, 32122, 'unlisted', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (id, status) in [
            ("d1", "success"),
            ("d2", "blocked"),
            ("d3", "failed"),
            ("d4", "queued"),
            ("d5", "building"),
            ("d6", "deploying"),
        ] {
            sqlx::query("INSERT INTO deploy (id, app_id, sha, status, created_at) VALUES (?, 'app1', 'abc', ?, 0)")
                .bind(id)
                .bind(status)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(fail_interrupted(&pool).await.unwrap(), 3);
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, status FROM deploy ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let statuses: Vec<&str> = rows.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(
            statuses,
            ["success", "blocked", "failed", "failed", "failed", "failed"]
        );
        assert_eq!(fail_interrupted(&pool).await.unwrap(), 0, "idempotent");
    }

    #[tokio::test]
    async fn prune_old_keeps_the_newest_fifty_per_app() {
        let (_ddir, pool) = test_pool().await;
        for (id, slug, uid, port, internal_port) in [
            ("app1", "shop", 10101, 18101, 18102),
            ("app2", "blog", 10102, 18103, 18104),
        ] {
            sqlx::query(
                "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) \
                 VALUES (?, ?, 'owner1', ?, ?, ?, 0, 0)",
            )
            .bind(id)
            .bind(slug)
            .bind(uid)
            .bind(port)
            .bind(internal_port)
            .execute(&pool)
            .await
            .unwrap();
        }
        for i in 0..KEEP_PER_APP + 2 {
            sqlx::query(
                "INSERT INTO deploy (id, app_id, sha, status, created_at) VALUES (?, 'app1', 'abc', 'success', ?)",
            )
            .bind(format!("d{i}"))
            .bind(i)
            .execute(&pool)
            .await
            .unwrap();
        }
        for (id, created_at) in [("e1", 7), ("e2", 7), ("e3", 8)] {
            sqlx::query(
                "INSERT INTO deploy (id, app_id, sha, status, created_at) VALUES (?, 'app2', 'abc', 'success', ?)",
            )
            .bind(id)
            .bind(created_at)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(prune_old(&pool).await.unwrap(), 2);
        let kept: Vec<String> =
            sqlx::query_scalar("SELECT id FROM deploy WHERE app_id = 'app1' ORDER BY created_at")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(kept.len(), KEEP_PER_APP as usize);
        assert_eq!(kept[0], "d2", "the two oldest go, the rest stay");
        assert_eq!(kept[kept.len() - 1], format!("d{}", KEEP_PER_APP + 1));
        let other: Vec<String> =
            sqlx::query_scalar("SELECT id FROM deploy WHERE app_id = 'app2' ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(other, vec!["e1", "e2", "e3"], "small apps keep everything");
        assert_eq!(prune_old(&pool).await.unwrap(), 0, "idempotent");
    }
}
