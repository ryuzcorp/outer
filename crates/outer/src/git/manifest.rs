// Derived from noite apps/runner/src/host/git_manifest.rs @ 58a977a (Apache-2.0).
// Outer change: reduced to the single ref refs/heads/main; Store replaces the
// s3/exec/Config helpers, and GitConfig carries only repos_dir.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::store::Store;

/// Manifest format version. Readers accept only what they know.
pub const MANIFEST_VERSION: u8 = 1;
/// The only ref Outer writes.
pub const MAIN_REF: &str = "refs/heads/main";

/// What git needs: the local bare-mirror root.
#[derive(Debug, Clone)]
pub struct GitConfig {
    /// Local bare mirrors live here: `{repos_dir}/{slug}.git`.
    pub repos_dir: PathBuf,
    /// Platform-owned oxfmt binary that formats commits. `None` disables
    /// formatting (commit still works); tests use `None` or a fake script.
    pub oxfmt_bin: Option<PathBuf>,
    /// Process-wide cap on concurrent formatter processes. Cloned configs
    /// share the permits (one `GitConfig` is built per process in `serve`,
    /// then cloned per commit), so a burst of concurrent commits cannot
    /// fork-bomb the host. No global static: the owner passes this down.
    pub oxfmt_slots: std::sync::Arc<tokio::sync::Semaphore>,
}

/// Most formatter processes running anywhere in this process at once.
pub const OXFMT_PROCESS_LIMIT: usize = 8;

impl GitConfig {
    /// Fresh config with a process-local formatter semaphore. Clone the
    /// returned config (not `new` again) wherever commits share a process.
    pub fn new(repos_dir: PathBuf, oxfmt_bin: Option<PathBuf>) -> Self {
        Self {
            repos_dir,
            oxfmt_bin,
            oxfmt_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(OXFMT_PROCESS_LIMIT)),
        }
    }
}

/// Key of a slug's manifest in the bucket.
pub fn manifest_key(slug: &str) -> String {
    format!("git/{slug}/MANIFEST.json")
}

/// Key of a tip bundle in the bucket.
pub fn bundle_key(slug: &str, sha: &str) -> String {
    format!("git/{slug}/main/{sha}.bundle")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRef {
    pub sha: String,
    pub bundle: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u8,
    pub seq: u64,
    pub main: Option<ManifestRef>,
}

/// Single-writer coordination for one slug: a per-slug mutex plus the last
/// manifest seq applied to the local bare mirror.
///
/// `applied_seq` only moves forward: a reader that syncs a manifest older
/// than one a concurrent write already applied must not drag the marker back.
/// Cold mirrors and purges reset/forget instead, so the next manifest wins.
#[derive(Clone, Default)]
pub struct GitSync {
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    applied: Arc<Mutex<HashMap<String, u64>>>,
}

impl GitSync {
    /// Hold across a whole publish (mirror CAS, bundle, manifest).
    pub async fn lock(&self, slug: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(slug.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    pub async fn applied_seq(&self, slug: &str) -> Option<u64> {
        self.applied.lock().await.get(slug).copied()
    }

    /// Record an applied manifest seq. Only moves forward.
    pub async fn set_applied_seq(&self, slug: &str, seq: u64) {
        let mut applied = self.applied.lock().await;
        if applied.get(slug).is_none_or(|current| seq > *current) {
            applied.insert(slug.to_string(), seq);
        }
    }

    /// Unconditional seq record for a rebuilt mirror: the mirror was just
    /// hydrated from the bundle, so the next manifest read must win.
    pub async fn reset_applied_seq(&self, slug: &str, seq: u64) {
        self.applied.lock().await.insert(slug.to_string(), seq);
    }

    /// Drop a slug's generation marker. Purge calls this under the slug's
    /// lock, so a recreated slug's seq (which restarts at 0/1) is applied
    /// instead of skipped as stale.
    pub async fn forget(&self, slug: &str) {
        self.applied.lock().await.remove(slug);
    }
}

pub(crate) fn validate_slug(slug: &str) -> anyhow::Result<()> {
    let ok = !slug.is_empty()
        && slug.len() <= 64
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !slug.starts_with('-')
        && !slug.ends_with('-');
    if ok {
        Ok(())
    } else {
        anyhow::bail!("bad slug {slug:?}")
    }
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn mirror_path(cfg: &GitConfig, slug: &str) -> PathBuf {
    cfg.repos_dir.join(format!("{slug}.git"))
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_path(prefix: &str) -> PathBuf {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("outer-{prefix}-{}-{n}", std::process::id()))
}

/// Run git with an explicit env (no inherited HOME config), a timeout, and
/// its own process group so a timed-out step's grandchildren die with it.
pub(crate) async fn run_git(
    args: &[&str],
    env: &[(&str, &str)],
    timeout: Duration,
) -> anyhow::Result<String> {
    Ok(String::from_utf8_lossy(&run_git_inner(args, None, env, timeout).await?).into_owned())
}

/// [`run_git`] with raw stdout (blob bytes are not UTF-8).
pub(crate) async fn run_git_raw(
    args: &[&str],
    env: &[(&str, &str)],
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    run_git_inner(args, None, env, timeout).await
}

/// [`run_git`] with stdin bytes; returns raw stdout.
pub(crate) async fn run_git_stdin(
    args: &[&str],
    stdin: &[u8],
    env: &[(&str, &str)],
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    run_git_inner(args, Some(stdin.to_vec()), env, timeout).await
}

async fn run_git_inner(
    args: &[&str],
    stdin: Option<Vec<u8>>,
    env: &[(&str, &str)],
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    use std::os::unix::process::CommandExt;

    fn kill_group(pid: u32) {
        unsafe {
            let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }

    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.as_std_mut().process_group(0);
    let mut child = cmd.spawn().context("spawn git")?;
    let pid = child.id();
    let writer = match child.stdin.take() {
        Some(mut pipe) => {
            let input = stdin.unwrap_or_default();
            Some(tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = pipe.write_all(&input).await;
                let _ = pipe.shutdown().await;
            }))
        }
        None => None,
    };
    let result = tokio::time::timeout(timeout, child.wait_with_output()).await;
    #[cfg(unix)]
    if let Some(pid) = pid {
        kill_group(pid);
    }
    let output = result
        .map_err(|_| anyhow::anyhow!("git {args:?} timed out"))?
        .context("wait git")?;
    if let Some(writer) = writer {
        let _ = writer.await;
    }
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {args:?} exit {:?}\n{err}", output.status.code());
    }
    Ok(output.stdout)
}

/// Read the manifest, if the slug has one yet. A missing key is `None`; any
/// other store failure is an error, so an unreachable store can never restart
/// `seq` and drop the old tip.
pub async fn read_manifest(store: &Store, slug: &str) -> anyhow::Result<Option<Manifest>> {
    let bytes = match store.get(&manifest_key(slug)).await? {
        Some(bytes) => bytes,
        None => return Ok(None),
    };
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    if manifest.version != MANIFEST_VERSION {
        anyhow::bail!("manifest version {} unsupported", manifest.version);
    }
    Ok(Some(manifest))
}

async fn write_manifest(store: &Store, slug: &str, manifest: &Manifest) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(manifest)?;
    store.put(&manifest_key(slug), bytes).await
}

/// The store's manifest when the mirror still needs it: `None` when no
/// manifest exists yet or `applied_seq` already covers it. Unlocked; a caller
/// that decides to sync MUST re-call this under the slug's lock before
/// syncing, since a concurrent publish can flip the manifest in between.
pub async fn pending_manifest(
    store: &Store,
    sync: &GitSync,
    slug: &str,
) -> anyhow::Result<Option<Manifest>> {
    let manifest = match read_manifest(store, slug).await? {
        Some(manifest) => manifest,
        None => return Ok(None),
    };
    if sync
        .applied_seq(slug)
        .await
        .is_some_and(|applied| applied >= manifest.seq)
    {
        return Ok(None);
    }
    Ok(Some(manifest))
}

/// Create the bare mirror (`--initial-branch=main`) and write the empty
/// manifest (seq 0, `main: null`).
pub async fn init_repo(
    store: &Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
) -> anyhow::Result<()> {
    validate_slug(slug)?;
    let _guard = sync.lock(slug).await;
    let bare = mirror_path(cfg, slug);
    if bare.join("HEAD").exists() {
        anyhow::bail!("repo for {slug} already exists");
    }
    if bare.exists() {
        tokio::fs::remove_dir_all(&bare).await?;
    }
    init_bare(&bare).await?;
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        seq: 0,
        main: None,
    };
    write_manifest(store, slug, &manifest).await?;
    sync.reset_applied_seq(slug, 0).await;
    Ok(())
}

async fn init_bare(bare: &Path) -> anyhow::Result<()> {
    if let Some(parent) = bare.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let Some(path) = bare.to_str() else {
        anyhow::bail!("bare mirror path is not valid UTF-8: {}", bare.display());
    };
    run_git(
        &["init", "--bare", "--initial-branch=main", path],
        &[],
        Duration::from_secs(30),
    )
    .await?;
    Ok(())
}

/// The head of `refs/heads/main` in a mirror, or `None` when the repo is
/// empty (fresh init, or a manifest with `main: null`).
pub async fn head_of(bare: &Path) -> anyhow::Result<Option<String>> {
    if !bare.join("HEAD").exists() {
        return Ok(None);
    }
    let git_dir = format!("--git-dir={}", bare.display());
    match run_git(
        &[&git_dir, "rev-parse", "--verify", MAIN_REF],
        &[],
        Duration::from_secs(15),
    )
    .await
    {
        Ok(out) => Ok(Some(out.trim().to_string())),
        Err(_) => Ok(None),
    }
}

/// Bring the mirror to what the manifest says, then return it. Hot path
/// serves local without the lock; only a newer manifest takes the lock and
/// re-reads before syncing, so a commit overlapping another publish can never
/// apply a pre-flip manifest read.
pub async fn ensure_synced(
    store: &Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
) -> anyhow::Result<PathBuf> {
    validate_slug(slug)?;
    let bare = mirror_path(cfg, slug);
    if !bare.join("HEAD").exists() {
        rehydrate(store, cfg, sync, slug).await?;
        return Ok(bare);
    }
    if pending_manifest(store, sync, slug).await?.is_some() {
        rehydrate(store, cfg, sync, slug).await?;
    }
    Ok(bare)
}

/// Rebuild a missing or stale mirror from the manifest's bundle. Returns the
/// head afterwards (`None` for a slug with no manifest yet, or an empty
/// repo). A mirror already at the manifest's sha is left alone.
pub async fn rehydrate(
    store: &Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
) -> anyhow::Result<Option<String>> {
    validate_slug(slug)?;
    let _guard = sync.lock(slug).await;
    rehydrate_inner(store, cfg, sync, slug).await
}

async fn rehydrate_inner(
    store: &Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
) -> anyhow::Result<Option<String>> {
    let manifest = read_manifest(store, slug).await?;
    let bare = mirror_path(cfg, slug);
    let (seq, entry) = match manifest {
        Some(m) => (m.seq, m.main),
        None => (0, None),
    };
    let Some(entry) = entry else {
        if !bare.join("HEAD").exists() {
            if bare.exists() {
                tokio::fs::remove_dir_all(&bare).await?;
            }
            init_bare(&bare).await?;
        }
        sync.reset_applied_seq(slug, seq).await;
        return Ok(None);
    };
    if head_of(&bare).await?.as_deref() == Some(entry.sha.as_str()) {
        sync.set_applied_seq(slug, seq).await;
        return Ok(Some(entry.sha.clone()));
    }
    let tmp = tmp_path(&format!("rehydrate-{slug}"));
    if !store.get_to_file(&entry.bundle, &tmp).await? {
        anyhow::bail!("bundle {} missing for {slug}", entry.bundle);
    }
    let result = async {
        if bare.exists() {
            tokio::fs::remove_dir_all(&bare).await?;
        }
        init_bare(&bare).await?;
        let git_dir = format!("--git-dir={}", bare.display());
        let Some(bundle) = tmp.to_str() else {
            anyhow::bail!("bundle tmp path is not valid UTF-8");
        };
        run_git(
            &[
                &git_dir,
                "fetch",
                bundle,
                &format!("+{}:{MAIN_REF}", entry.sha),
            ],
            &[],
            Duration::from_secs(120),
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let _ = tokio::fs::remove_file(&tmp).await;
    result?;
    sync.reset_applied_seq(slug, seq).await;
    Ok(Some(entry.sha.clone()))
}

/// CAS-update `refs/heads/main` in the mirror, then publish exactly like a
/// push: tip bundle, one manifest write (the commit point, `seq + 1`), then
/// prune of the superseded bundle. `expected_old = None` asserts the ref does
/// not exist yet. A CAS mismatch is an error whose message contains
/// "tip moved". The manifest is read before the CAS under the same lock, so
/// a failed read leaves the mirror untouched instead of stranding it on an
/// unpublished commit. If the manifest write fails, the outcome is ambiguous
/// (a response lost after the store applied it), so the manifest is re-read:
/// when it already names the new tip the flip is adopted, otherwise the ref
/// is restored and the bundle is kept — never deleted, since it may be the
/// live tip.
pub async fn publish(
    store: &Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
    expected_old: Option<&str>,
    new_sha: &str,
) -> anyhow::Result<String> {
    validate_slug(slug)?;
    if !is_full_sha(new_sha) {
        anyhow::bail!("publish needs a full sha, got {new_sha:?}");
    }
    let _guard = sync.lock(slug).await;
    let bare = mirror_path(cfg, slug);
    if !bare.join("HEAD").exists() {
        rehydrate_inner(store, cfg, sync, slug).await?;
    }
    // Read before the CAS: a failed read must leave the mirror untouched.
    let old = read_manifest(store, slug).await?;
    cas_update(&bare, expected_old, new_sha).await?;
    let bundle = bundle_key(slug, new_sha);
    let uploaded = async {
        let tmp = tmp_path(&format!("bundle-{slug}"));
        let result = async {
            let git_dir = format!("--git-dir={}", bare.display());
            let Some(path) = tmp.to_str() else {
                anyhow::bail!("bundle tmp path is not valid UTF-8");
            };
            run_git(
                &[&git_dir, "bundle", "create", path, MAIN_REF],
                &[],
                Duration::from_secs(120),
            )
            .await?;
            store.put_file(&bundle, &tmp).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        result
    }
    .await;
    if let Err(e) = uploaded {
        let _ = set_ref(&bare, expected_old).await;
        // Only a true orphan: bundle keys are content-addressed, so a
        // republish of the live tip reuses its key — deleting what the
        // manifest still names would remove the live bundle.
        if !bundle_is_live(old.as_ref(), &bundle) {
            let _ = store.delete(&bundle).await;
        }
        return Err(e);
    }
    let seq = old.as_ref().map(|m| m.seq + 1).unwrap_or(1);
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        seq,
        main: Some(ManifestRef {
            sha: new_sha.to_string(),
            bundle: bundle.clone(),
        }),
    };
    if let Err(e) = write_manifest(store, slug, &manifest).await {
        // The write may have landed despite the error (a response lost after
        // the store applied it). Re-read to learn the truth instead of
        // rolling back blindly: a manifest that already names our tip means
        // the flip happened, so adopt it. Otherwise — or when the truth is
        // unreadable — restore the ref; the kept bundle lets the next sync
        // rebuild the mirror from the manifest either way.
        match read_manifest(store, slug).await {
            Ok(current) if manifest_names_tip(current.as_ref(), seq, new_sha) => {
                sync.set_applied_seq(slug, seq).await;
                prune_superseded(store, slug, &bundle).await;
                return Ok(new_sha.to_string());
            }
            _ => {
                let _ = set_ref(&bare, expected_old).await;
            }
        }
        return Err(e);
    }
    sync.set_applied_seq(slug, seq).await;
    // Prune superseded bundles only after the flip, so concurrent readers
    // already moved on.
    prune_superseded(store, slug, &bundle).await;
    Ok(new_sha.to_string())
}

/// Whether the store's manifest already reflects this publish: it names our
/// tip at our seq. A failed write that still landed must be adopted, not
/// rolled back — otherwise the mirror is reset under a manifest that points
/// at a bundle the rollback just deleted.
fn manifest_names_tip(current: Option<&Manifest>, seq: u64, sha: &str) -> bool {
    current.is_some_and(|m| m.seq == seq && m.main.as_ref().is_some_and(|main| main.sha == sha))
}

/// Whether the manifest names `bundle` as its tip. Such a bundle is live and
/// must never be deleted as an "orphan": bundle keys are content-addressed,
/// so a republish of the same sha reuses the live key.
fn bundle_is_live(manifest: Option<&Manifest>, bundle: &str) -> bool {
    manifest
        .as_ref()
        .is_some_and(|m| m.main.as_ref().is_some_and(|main| main.bundle == bundle))
}

/// Delete superseded tip bundles, keeping only the live one. Runs only after
/// the manifest flip, so concurrent readers already moved on; every delete
/// is best-effort.
async fn prune_superseded(store: &Store, slug: &str, bundle: &str) {
    let prefix = format!("git/{slug}/main/");
    if let Ok(keys) = store.list(&prefix).await {
        for key in keys {
            if key != bundle && key.ends_with(".bundle") {
                let _ = store.delete(&key).await;
            }
        }
    }
}

/// Delete the mirror and the whole `git/{slug}/` prefix, and forget the
/// slug's generation so a recreated slug's seq is applied, not skipped.
pub async fn purge(
    store: &Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
) -> anyhow::Result<()> {
    validate_slug(slug)?;
    let _guard = sync.lock(slug).await;
    let bare = mirror_path(cfg, slug);
    match tokio::fs::remove_dir_all(&bare).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    store.delete_prefix(&format!("git/{slug}/")).await?;
    sync.forget(slug).await;
    Ok(())
}

/// Compare-and-swap the main ref. Distinguishes "the tip moved" from a real
/// git failure by re-reading the ref after a failed update.
async fn cas_update(bare: &Path, expected_old: Option<&str>, new_sha: &str) -> anyhow::Result<()> {
    let git_dir = format!("--git-dir={}", bare.display());
    // An empty old value is git's "the ref must not exist yet".
    let args = vec![
        git_dir.as_str(),
        "update-ref",
        MAIN_REF,
        new_sha,
        expected_old.unwrap_or(""),
    ];
    if let Err(e) = run_git(&args, &[], Duration::from_secs(15)).await {
        let now = head_of(bare).await.unwrap_or(None);
        if now.as_deref() != expected_old {
            anyhow::bail!(
                "tip moved under this update: {MAIN_REF} is {}",
                now.as_deref().unwrap_or("<absent>")
            );
        }
        return Err(e.context("update-ref"));
    }
    Ok(())
}

async fn set_ref(bare: &Path, sha: Option<&str>) -> anyhow::Result<()> {
    let git_dir = format!("--git-dir={}", bare.display());
    match sha {
        Some(sha) => {
            run_git(
                &[&git_dir, "update-ref", MAIN_REF, sha],
                &[],
                Duration::from_secs(15),
            )
            .await?;
        }
        None => {
            run_git(
                &[&git_dir, "update-ref", "-d", MAIN_REF],
                &[],
                Duration::from_secs(15),
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup(slug: &str) -> (tempfile::TempDir, GitConfig, Store, GitSync) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        init_repo(&store, &cfg, &sync, slug).await.unwrap();
        (dir, cfg, store, sync)
    }

    #[tokio::test]
    async fn init_writes_empty_manifest_and_purge_clears_it() {
        let (_dir, cfg, store, sync) = setup("grocery").await;

        let manifest = read_manifest(&store, "grocery").await.unwrap().unwrap();
        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_eq!(manifest.seq, 0);
        assert!(manifest.main.is_none(), "a fresh repo has no main yet");
        assert!(head_of(&mirror_path(&cfg, "grocery"))
            .await
            .unwrap()
            .is_none());

        purge(&store, &cfg, &sync, "grocery").await.unwrap();
        assert!(read_manifest(&store, "grocery").await.unwrap().is_none());
        assert!(!mirror_path(&cfg, "grocery").exists());
    }

    #[tokio::test]
    async fn rehydrate_empty_repo_leaves_an_empty_mirror() {
        let (_dir, cfg, store, sync) = setup("empty").await;
        tokio::fs::remove_dir_all(mirror_path(&cfg, "empty"))
            .await
            .unwrap();

        let head = rehydrate(&store, &cfg, &sync, "empty").await.unwrap();
        assert_eq!(head, None);
        assert!(mirror_path(&cfg, "empty").join("HEAD").exists());
    }

    /// Build a commit on the empty tree in the mirror without touching refs,
    /// so `publish` has something to point at.
    async fn commit_on_empty_tree(bare: &Path, parent: Option<&str>) -> String {
        let git_dir = format!("--git-dir={}", bare.display());
        let tree = run_git_stdin(&[&git_dir, "mktree"], &[], &[], Duration::from_secs(15))
            .await
            .map(|b| String::from_utf8(b).unwrap())
            .unwrap();
        let tree = tree.trim().to_string();
        let mut args = vec![git_dir.as_str(), "commit-tree", tree.as_str(), "-m", "test"];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        run_git(
            &args,
            &[
                ("GIT_AUTHOR_NAME", "outer-test"),
                ("GIT_AUTHOR_EMAIL", "outer-test@example.com"),
                ("GIT_COMMITTER_NAME", "outer-test"),
                ("GIT_COMMITTER_EMAIL", "outer-test@example.com"),
            ],
            Duration::from_secs(15),
        )
        .await
        .map(|s| s.trim().to_string())
        .unwrap()
    }

    /// Publish flips the manifest to seq+1, records the bundle and prunes the
    /// superseded one; a second publish builds on the first.
    #[tokio::test]
    async fn publish_advances_manifest_and_prunes_old_bundle() {
        let (_dir, cfg, store, sync) = setup("publish").await;
        let bare = mirror_path(&cfg, "publish");
        let first = commit_on_empty_tree(&bare, None).await;
        assert_eq!(
            publish(&store, &cfg, &sync, "publish", None, &first)
                .await
                .unwrap(),
            first
        );
        let manifest = read_manifest(&store, "publish").await.unwrap().unwrap();
        assert_eq!(manifest.seq, 1);
        assert_eq!(manifest.main.as_ref().unwrap().sha, first);
        assert_eq!(sync.applied_seq("publish").await, Some(1));
        assert_eq!(
            head_of(&bare).await.unwrap().as_deref(),
            Some(first.as_str())
        );
        let first_bundle = bundle_key("publish", &first);
        assert!(store.get(&first_bundle).await.unwrap().is_some());

        let second = commit_on_empty_tree(&bare, Some(&first)).await;
        assert_eq!(
            publish(&store, &cfg, &sync, "publish", Some(&first), &second)
                .await
                .unwrap(),
            second
        );
        let manifest = read_manifest(&store, "publish").await.unwrap().unwrap();
        assert_eq!(manifest.seq, 2);
        assert_eq!(manifest.main.as_ref().unwrap().sha, second);
        assert!(store
            .get(&bundle_key("publish", &second))
            .await
            .unwrap()
            .is_some());
        assert!(
            store.get(&first_bundle).await.unwrap().is_none(),
            "the superseded bundle is pruned after the flip"
        );
    }

    /// A manifest that cannot be read must fail the publish before the CAS:
    /// the mirror keeps its old ref and no bundle is uploaded.
    #[tokio::test]
    async fn failed_manifest_read_leaves_mirror_untouched() {
        let (_dir, cfg, store, sync) = setup("unreadable").await;
        let bare = mirror_path(&cfg, "unreadable");
        let sha = commit_on_empty_tree(&bare, None).await;
        store
            .put(&manifest_key("unreadable"), b"{not json".to_vec())
            .await
            .unwrap();
        assert!(publish(&store, &cfg, &sync, "unreadable", None, &sha)
            .await
            .is_err());
        assert!(head_of(&bare).await.unwrap().is_none());
        assert!(
            store.list("git/unreadable/main/").await.unwrap().is_empty(),
            "no bundle is uploaded when the old manifest cannot be read"
        );
    }

    /// The reconcile predicate adopts only a manifest that already names our
    /// tip at our seq; a stale, diverged or missing manifest rolls back.
    #[test]
    fn manifest_reconcile_adopts_only_our_landed_tip() {
        let landed = Manifest {
            version: MANIFEST_VERSION,
            seq: 3,
            main: Some(ManifestRef {
                sha: "a".repeat(40),
                bundle: bundle_key("s", &"a".repeat(40)),
            }),
        };
        let sha = "a".repeat(40);
        assert!(manifest_names_tip(Some(&landed), 3, &sha));
        assert!(!manifest_names_tip(Some(&landed), 4, &sha), "stale seq");
        assert!(
            !manifest_names_tip(Some(&landed), 3, &"b".repeat(40)),
            "diverged tip"
        );
        assert!(!manifest_names_tip(None, 3, &sha), "missing manifest");
        assert!(
            !manifest_names_tip(
                Some(&Manifest {
                    version: MANIFEST_VERSION,
                    seq: 3,
                    main: None
                }),
                3,
                &sha
            ),
            "empty manifest"
        );
        assert!(bundle_is_live(Some(&landed), &bundle_key("s", &sha)));
        assert!(!bundle_is_live(Some(&landed), "git/s/main/deadbeef.bundle"));
        assert!(!bundle_is_live(None, &bundle_key("s", &sha)));
    }
}
