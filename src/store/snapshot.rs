// Derived from noite apps/runner/src/host/state.rs @ 58a977a (Apache-2.0).
// Outer keeps the VACUUM INTO + zstd upload, the owner.json claim fence, the
// identical-hash skip and the poll/debounce/interval timers. Dropped:
// Noite's Config coupling, metrics, readiness plumbing and anything about an
// attached metrics.sqlite — Outer has one DB.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::SqlitePool;

use super::{s3::is_not_found, Store};

/// Snapshot cadence: poll data_version every 2 s, wait 5 s for bursts to
/// settle, upload at most every 15 s. Worst-case loss on an ungraceful crash
/// is about 22 s of mutations; the identical-hash skip below keeps idle
/// hosts at zero uploads.
const POLL: Duration = Duration::from_secs(2);
const DEBOUNCE: Duration = Duration::from_secs(5);
const MIN_INTERVAL: Duration = Duration::from_secs(15);

/// Which local database to snapshot and where the snapshot lives in the
/// bucket (`{key_prefix}/outer.sqlite.zst` plus the meta and owner keys).
#[derive(Clone, Debug)]
pub struct SnapshotConfig {
    /// Local SQLite file, e.g. `/data/outer.sqlite`.
    pub db_path: PathBuf,
    /// Bucket prefix for the snapshot keys, e.g. `"outer/state"`.
    pub key_prefix: String,
    /// This host's claim on the bucket state; the newest claimant wins.
    pub host_id: String,
}

impl SnapshotConfig {
    fn state_key(&self) -> String {
        format!("{}/outer.sqlite.zst", self.key_prefix)
    }

    fn meta_key(&self) -> String {
        format!("{}/outer.sqlite.meta.json", self.key_prefix)
    }

    fn owner_key(&self) -> String {
        format!("{}/owner.json", self.key_prefix)
    }
}

/// Snapshot health for `/ready`: whether this host still owns the bucket
/// claim, how long ago the last upload finished, and the last upload error
/// (cleared by the next success).
#[derive(Clone, Debug, serde::Serialize)]
pub struct SnapshotStatus {
    pub owner: bool,
    pub last_upload_secs_ago: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StateSync {
    dirty: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    /// This host's claim on the bucket state (see module docs).
    instance: Arc<String>,
    /// False once another host took the claim; uploads stop.
    owner: Arc<AtomicBool>,
    /// sha256 of the last uploaded snapshot: an identical
    /// `VACUUM INTO` output skips the upload entirely.
    last_hash: Arc<std::sync::Mutex<Option<String>>>,
    /// Serializes concurrent snapshots: the sync tick and the shutdown
    /// snapshot share one host and one bucket, so without this two runs
    /// interleave on one temp path and upload each other's partial bytes.
    /// Unique temp names per run (see `snapshot_tmp_paths`) are the second
    /// half of the fix.
    snapshot_lock: Arc<tokio::sync::Mutex<()>>,
    /// When the last upload finished (wall age for `/ready`).
    last_upload: Arc<std::sync::Mutex<Option<Instant>>>,
    /// Last upload failure, cleared by the next success.
    last_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl StateSync {
    pub fn new(host_id: impl Into<String>) -> Self {
        Self {
            dirty: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
            instance: Arc::new(host_id.into()),
            owner: Arc::new(AtomicBool::new(true)),
            last_hash: Arc::new(std::sync::Mutex::new(None)),
            snapshot_lock: Arc::new(tokio::sync::Mutex::new(())),
            last_upload: Arc::new(std::sync::Mutex::new(None)),
            last_error: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Mark dirty explicitly (API mutations; the poller catches the rest).
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Whether this host still owns the bucket state (for /ready detail).
    pub fn is_owner(&self) -> bool {
        self.owner.load(Ordering::Relaxed)
    }

    /// Snapshot health for `/ready` (additive: never fails the gate).
    pub fn snapshot_status(&self) -> SnapshotStatus {
        let last_upload_secs_ago = self
            .last_upload
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .map(|at| at.elapsed().as_secs());
        let last_error = self.last_error.lock().ok().and_then(|guard| guard.clone());
        SnapshotStatus {
            owner: self.is_owner(),
            last_upload_secs_ago,
            last_error,
        }
    }

    /// Claim the bucket state for this host (boot). Continues the stored
    /// generation so it stays monotonic across restarts. Only a missing
    /// meta object counts as "first boot" — a transport error refuses the
    /// boot instead of seeding generation 0, which would defeat the
    /// identical-skip and let the next upload overwrite a good snapshot with
    /// an empty database.
    pub async fn claim(&self, store: &Store, cfg: &SnapshotConfig) -> anyhow::Result<()> {
        match download_text(store, &cfg.meta_key()).await {
            Ok(text) => {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                if let Some(g) = v.get("generation").and_then(serde_json::Value::as_u64) {
                    self.generation.store(g, Ordering::Relaxed);
                }
                // Seed the identical-skip so the first pass after a
                // restart doesn't re-upload an unchanged snapshot.
                if let Some(h) = v.get("sha256").and_then(|x| x.as_str()) {
                    *self.last_hash.lock().unwrap_or_else(|e| e.into_inner()) = Some(h.to_string());
                }
            }
            Err(e) if is_not_found(&e) => {}
            Err(e) => {
                return Err(e.context(
                    "outer state meta download failed; refusing to claim the bucket state",
                ));
            }
        }
        let claim = serde_json::json!({
            "instance": self.instance.as_str(),
            "claimed_at": chrono::Utc::now().to_rfc3339(),
        });
        upload_text(store, &cfg.owner_key(), &claim.to_string()).await?;
        tracing::info!(instance = %self.instance, "claimed outer state in the bucket");
        Ok(())
    }

    async fn still_owner(&self, store: &Store, cfg: &SnapshotConfig) -> bool {
        let current = download_text(store, &cfg.owner_key())
            .await
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| {
                v.get("instance")
                    .and_then(|i| i.as_str().map(str::to_owned))
            });
        let mine = match current {
            Some(instance) => instance == *self.instance,
            // Unreadable claim: keep uploading rather than silently stop.
            None => true,
        };
        if !mine && self.owner.swap(false, Ordering::Relaxed) {
            tracing::error!(
                instance = %self.instance,
                "another host claimed this bucket's state; this host stops uploading snapshots (two installs share one bucket?)"
            );
        }
        mine
    }

    /// Snapshot now: VACUUM INTO a temp file, upload, update meta. Serialized
    /// by `snapshot_lock`: the sync tick and the shutdown path share this
    /// host, and overlapping runs used to interleave on one fixed temp path
    /// and upload each other's partial bytes.
    pub async fn snapshot_now(
        &self,
        pool: &SqlitePool,
        store: &Store,
        cfg: &SnapshotConfig,
    ) -> anyhow::Result<u64> {
        let _snapshot = self.snapshot_lock.lock().await;
        if !self.still_owner(store, cfg).await {
            anyhow::bail!("outer state is owned by another host; not uploading");
        }
        // Clear first: a write that lands while we snapshot sets it again and
        // is picked up by the next pass instead of being lost.
        self.dirty.store(false, Ordering::Relaxed);
        let result = self.upload_snapshot(pool, store, cfg).await;
        match &result {
            // An upload landed: the bucket now holds this state and its meta.
            Ok(bytes) if *bytes > 0 => {
                *self.last_upload.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            // Skipped as identical: `last_hash` is only recorded once state
            // and meta both landed, so the bucket already matches. Nothing
            // was uploaded, so the upload age stays the real one; an older
            // error no longer describes the bucket.
            Ok(_) => {
                *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            Err(e) => {
                self.dirty.store(true, Ordering::Relaxed);
                let msg: String = format!("{e:#}").chars().take(300).collect();
                *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
            }
        }
        result
    }

    async fn upload_snapshot(
        &self,
        pool: &SqlitePool,
        store: &Store,
        cfg: &SnapshotConfig,
    ) -> anyhow::Result<u64> {
        let live = &cfg.db_path;
        let dir = live.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        // Unique temp names per run — a fixed `.snapshot.sqlite` lets a
        // second concurrent run truncate the first's VACUUM output mid-read.
        // (Runs are also serialized by `snapshot_lock`; the names are defense
        // in depth against any future unlocked caller.)
        let (tmp, src) = snapshot_tmp_paths(&dir);
        let Some(target) = tmp.to_str() else {
            anyhow::bail!("snapshot path is not valid UTF-8");
        };
        // `VACUUM INTO` copies the database file; no attached databases are
        // snapshotted — Outer has one DB.
        let sql = format!("VACUUM INTO '{}'", target.replace('\'', "''"));
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        let raw = match tokio::fs::read(&tmp).await {
            Ok(raw) => raw,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e.into());
            }
        };
        let _ = tokio::fs::remove_file(&tmp).await;
        // Skip identical snapshots: writes that don't change bytes (e.g. an
        // upsert of the same row) upload nothing.
        let hash: String = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(&raw);
            h.finalize().iter().map(|b| format!("{b:02x}")).collect()
        };
        let unchanged = self
            .last_hash
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .as_deref()
            == Some(hash.as_str());
        if unchanged {
            tracing::debug!(hash = %hash, "outer state unchanged; skipping snapshot upload");
            return Ok(0);
        }
        // zstd level 3: saves bucket storage and upload bytes.
        let packed = zstd::encode_all(&raw[..], 3)?;
        let packed_len = packed.len() as u64;
        if let Err(e) = tokio::fs::write(&src, &packed).await {
            let _ = tokio::fs::remove_file(&src).await;
            return Err(e.into());
        }
        let uploaded = store.put_file(&cfg.state_key(), &src).await;
        let _ = tokio::fs::remove_file(&src).await;
        uploaded?;
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let meta = serde_json::json!({
            "generation": generation,
            "written_at": chrono::Utc::now().to_rfc3339(),
            "codec": "zstd",
            "sha256": hash.as_str(),
            "bytes": packed_len,
            "raw_bytes": raw.len(),
            "instance": self.instance.as_str(),
        });
        upload_text(
            store,
            &cfg.meta_key(),
            &serde_json::to_string_pretty(&meta)?,
        )
        .await?;
        // Only now is this hash in the bucket with its meta. Recorded earlier,
        // a failed meta upload would make the retry skip as "identical" and
        // leave stale meta behind, which `claim` seeds the next host from.
        *self.last_hash.lock().unwrap_or_else(|e| e.into_inner()) = Some(hash);
        tracing::info!(
            bytes = packed_len,
            raw_bytes = raw.len(),
            generation,
            "outer state snapshot uploaded"
        );
        Ok(packed_len)
    }
}

/// Unique temp paths for one snapshot run: the sqlite dump and its
/// zstd packing never share names across runs, so overlapping sync-tick
/// and shutdown snapshots cannot truncate or read each other's bytes.
fn snapshot_tmp_paths(dir: &std::path::Path) -> (PathBuf, PathBuf) {
    let tag = uuid::Uuid::new_v4();
    (
        dir.join(format!(".snapshot-{tag}.sqlite")),
        dir.join(format!(".snapshot-{tag}.sqlite.zst")),
    )
}

async fn download_text(store: &Store, key: &str) -> anyhow::Result<String> {
    match store.get(key).await? {
        Some(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        None => Err(not_found(key)),
    }
}

async fn upload_text(store: &Store, key: &str, text: &str) -> anyhow::Result<()> {
    store.put(key, text.as_bytes().to_vec()).await
}

/// Missing-key error shaped so [`is_not_found`] recognizes it: only a
/// missing key counts as "no snapshot", never a transport failure.
fn not_found(key: &str) -> anyhow::Error {
    anyhow::anyhow!("s3 download {key} failed: HTTP 404 NoSuchKey")
}

/// Boot path: if the local DB file does not exist, download the snapshot
/// before connecting. Returns Ok(false) only when the bucket has no
/// snapshot; any other failure is an error the caller must not start past,
/// because starting with an empty database would upload it over the good
/// snapshot on the next write. If the local file exists, keep it.
pub async fn restore_if_missing(store: &Store, cfg: &SnapshotConfig) -> anyhow::Result<bool> {
    if cfg.db_path.exists() {
        return Ok(false);
    }
    if let Some(parent) = cfg.db_path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    match download_snapshot(store, cfg).await {
        Ok(restored) => Ok(restored),
        Err(e) => {
            Err(e.context("outer state restore failed; refusing to start with an empty database"))
        }
    }
}

async fn download_snapshot(store: &Store, cfg: &SnapshotConfig) -> anyhow::Result<bool> {
    let partial = cfg.db_path.with_extension("sqlite.restoring");
    let mut last_err = None;
    for attempt in 1..=5u64 {
        match try_restore_once(store, cfg, &partial).await {
            Ok(restored) => {
                let _ = tokio::fs::remove_file(&partial).await;
                return Ok(restored);
            }
            Err(e) if is_not_found(&e) => {
                tracing::info!("no outer state snapshot in the bucket; starting fresh");
                return Ok(false);
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "outer state restore failed; retrying");
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(attempt * 3)).await;
            }
        }
    }
    let _ = tokio::fs::remove_file(&partial).await;
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("restore failed")))
}

/// One restore attempt. Only a missing key counts as no-snapshot — a
/// transport error must never read as "start fresh".
async fn try_restore_once(
    store: &Store,
    cfg: &SnapshotConfig,
    partial: &std::path::Path,
) -> anyhow::Result<bool> {
    if !store.get_to_file(&cfg.state_key(), partial).await? {
        return Err(not_found(&cfg.state_key()));
    }
    let packed = tokio::fs::read(partial).await?;
    let raw = zstd::decode_all(&packed[..])?;
    // Rename into place only once complete: a crash mid-write must not leave
    // a truncated database that "exists".
    let decoded = cfg.db_path.with_extension("sqlite.decoded");
    tokio::fs::write(&decoded, raw).await?;
    tokio::fs::rename(&decoded, &cfg.db_path).await?;
    tracing::info!(path = %cfg.db_path.display(), "outer state restored from bucket");
    Ok(true)
}

/// Background sync: poll `data_version` on a dedicated connection, then
/// snapshot on the debounce / min-interval schedule from the module docs.
pub fn spawn_sync_task(sync: StateSync, pool: SqlitePool, store: Store, cfg: SnapshotConfig) {
    tokio::spawn(async move {
        let mut conn = match pool.acquire().await {
            Ok(c) => c.detach(),
            Err(e) => {
                tracing::error!(error = %e, "state sync: no dedicated connection; snapshots disabled");
                return;
            }
        };
        let mut last_version: Option<i64> = None;
        let mut dirty_since: Option<Instant> = None;
        let mut last_upload = Instant::now() - MIN_INTERVAL;
        loop {
            tokio::time::sleep(POLL).await;
            if let Ok(v) = sqlx::query_scalar::<_, i64>("PRAGMA data_version")
                .fetch_one(&mut conn)
                .await
            {
                if last_version.is_some_and(|prev| prev != v) {
                    sync.mark_dirty();
                }
                last_version = Some(v);
            }
            if !sync.is_dirty() {
                dirty_since = None;
                continue;
            }
            let since = dirty_since.get_or_insert_with(Instant::now);
            if !should_upload(true, Some(*since), last_upload, Instant::now()) {
                continue;
            }
            match sync.snapshot_now(&pool, &store, &cfg).await {
                Ok(_) => {
                    last_upload = Instant::now();
                    dirty_since = None;
                }
                Err(e) => tracing::warn!(error = %e, "state snapshot failed"),
            }
        }
    });
}

/// Pure upload gate (injected clock, no I/O): dirty, quiet for DEBOUNCE, and
/// MIN_INTERVAL since the last upload. Unit-tested with explicit Instants.
fn should_upload(
    dirty: bool,
    dirty_since: Option<Instant>,
    last_upload: Instant,
    now: Instant,
) -> bool {
    dirty
        && dirty_since.is_some_and(|since| now.duration_since(since) >= DEBOUNCE)
        && now.duration_since(last_upload) >= MIN_INTERVAL
}

/// Final snapshot on graceful shutdown.
pub async fn final_snapshot(
    sync: &StateSync,
    pool: &SqlitePool,
    store: &Store,
    cfg: &SnapshotConfig,
) {
    if let Err(e) = sync.snapshot_now(pool, store, cfg).await {
        tracing::warn!(error = %e, "final state snapshot failed");
    }
}

#[cfg(test)]
mod tests {
    use super::{should_upload, snapshot_tmp_paths, DEBOUNCE, MIN_INTERVAL};
    use std::time::Instant;

    fn test_cfg(dir: &std::path::Path, host: &str) -> super::SnapshotConfig {
        super::SnapshotConfig {
            db_path: dir.join("outer.sqlite"),
            key_prefix: "outer/state".into(),
            host_id: host.into(),
        }
    }

    async fn test_pool(path: &std::path::Path) -> sqlx::SqlitePool {
        let url = format!("sqlite:{0}?mode=rwc", path.display());
        let pool = sqlx::SqlitePool::connect(&url)
            .await
            .expect("test database");
        sqlx::query("CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT)")
            .execute(&pool)
            .await
            .expect("test table");
        sqlx::query("INSERT INTO kv (k, v) VALUES ('a', '1')")
            .execute(&pool)
            .await
            .expect("seed row");
        pool
    }

    /// Snapshot then restore round-trips the database through the store:
    /// delete the local file, boot again, the row is back.
    #[tokio::test]
    async fn snapshot_restore_roundtrip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = super::Store::memory();
        let cfg = test_cfg(dir.path(), "host-1");
        let pool = test_pool(&cfg.db_path).await;
        let sync = super::StateSync::new("host-1");
        sync.claim(&store, &cfg).await.expect("claim");
        let bytes = sync
            .snapshot_now(&pool, &store, &cfg)
            .await
            .expect("snapshot");
        assert!(bytes > 0, "a seeded database uploads bytes");
        pool.close().await;
        tokio::fs::remove_file(&cfg.db_path)
            .await
            .expect("remove local db");
        assert!(super::restore_if_missing(&store, &cfg)
            .await
            .expect("restore"));
        let pool =
            sqlx::SqlitePool::connect(&format!("sqlite:{0}?mode=rwc", cfg.db_path.display()))
                .await
                .expect("reopen");
        let v: String = sqlx::query_scalar("SELECT v FROM kv WHERE k = 'a'")
            .fetch_one(&pool)
            .await
            .expect("row restored");
        assert_eq!(v, "1");
        pool.close().await;
    }

    /// Unchanged bytes upload nothing: the second snapshot skips and keeps
    /// the real upload age instead of refreshing it.
    #[tokio::test]
    async fn identical_snapshot_skips_upload() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = super::Store::memory();
        let cfg = test_cfg(dir.path(), "host-1");
        let pool = test_pool(&cfg.db_path).await;
        let sync = super::StateSync::new("host-1");
        sync.claim(&store, &cfg).await.expect("claim");
        assert!(sync.snapshot_now(&pool, &store, &cfg).await.expect("first") > 0);
        let first_upload = sync.snapshot_status().last_upload_secs_ago;
        assert!(first_upload.is_some());
        assert_eq!(
            sync.snapshot_now(&pool, &store, &cfg)
                .await
                .expect("resnapshot"),
            0
        );
        assert!(sync.snapshot_status().last_error.is_none());
        // A write changes the bytes, so the next snapshot uploads again.
        sqlx::query("INSERT INTO kv (k, v) VALUES ('b', '2')")
            .execute(&pool)
            .await
            .expect("new row");
        sync.mark_dirty();
        assert!(
            sync.snapshot_now(&pool, &store, &cfg)
                .await
                .expect("after write")
                > 0
        );
        pool.close().await;
    }

    /// The owner fence: once another host claims the bucket, this host's
    /// uploads refuse instead of overwriting the new owner's state.
    #[tokio::test]
    async fn owner_fence_refuses_after_another_host_claims() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = super::Store::memory();
        let cfg = test_cfg(dir.path(), "host-1");
        let pool = test_pool(&cfg.db_path).await;
        let first = super::StateSync::new("host-1");
        first.claim(&store, &cfg).await.expect("first claim");
        assert!(
            first
                .snapshot_now(&pool, &store, &cfg)
                .await
                .expect("first upload")
                > 0
        );
        let second = super::StateSync::new("host-2");
        second.claim(&store, &cfg).await.expect("second claim");
        let err = first
            .snapshot_now(&pool, &store, &cfg)
            .await
            .expect_err("fenced upload must fail");
        assert!(
            format!("{err:#}").contains("owned by another host"),
            "unexpected: {err:#}"
        );
        assert!(!first.is_owner());
        // The new owner's first upload of unchanged data is an identical-hash
        // skip (it seeded the hash from the meta it claimed over), so change
        // the data to prove it can upload.
        sqlx::query("CREATE TABLE after_handover (x INTEGER)")
            .execute(&pool)
            .await
            .expect("write");
        assert!(
            second
                .snapshot_now(&pool, &store, &cfg)
                .await
                .expect("owner uploads")
                > 0
        );
        pool.close().await;
    }

    /// Boot keeps the local file when it exists: no download, no overwrite.
    #[tokio::test]
    async fn restore_skipped_when_local_exists() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = super::Store::memory();
        let cfg = test_cfg(dir.path(), "host-1");
        // A snapshot exists in the bucket, but the local file wins.
        let pool = test_pool(&cfg.db_path).await;
        let sync = super::StateSync::new("host-1");
        sync.claim(&store, &cfg).await.expect("claim");
        sync.snapshot_now(&pool, &store, &cfg)
            .await
            .expect("snapshot");
        pool.close().await;
        let sentinel = b"local file stays".to_vec();
        tokio::fs::write(&cfg.db_path, &sentinel)
            .await
            .expect("sentinel");
        assert!(!super::restore_if_missing(&store, &cfg).await.expect("skip"));
        assert_eq!(tokio::fs::read(&cfg.db_path).await.expect("read"), sentinel);
    }

    /// No snapshot in the bucket starts fresh: restore reports false, and a
    /// later claim seeds generation 0 without refusing the boot.
    #[tokio::test]
    async fn restore_empty_bucket_starts_fresh() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = super::Store::memory();
        let cfg = test_cfg(dir.path(), "host-1");
        assert!(!super::restore_if_missing(&store, &cfg)
            .await
            .expect("fresh start"));
        super::StateSync::new("host-1")
            .claim(&store, &cfg)
            .await
            .expect("first-boot claim");
    }

    /// Every snapshot run gets its own temp names — two runs never share a
    /// path, so overlapping sync-tick and shutdown snapshots cannot
    /// truncate or read each other's bytes.
    #[test]
    fn snapshot_tmp_paths_are_unique_per_run() {
        let dir = std::path::Path::new("/data");
        let (a_sqlite, a_zst) = snapshot_tmp_paths(dir);
        let (b_sqlite, b_zst) = snapshot_tmp_paths(dir);
        assert_ne!(a_sqlite, b_sqlite);
        assert_ne!(a_zst, b_zst);
        assert_ne!(a_sqlite, a_zst);
        for path in [&a_sqlite, &a_zst, &b_sqlite, &b_zst] {
            assert_eq!(path.parent(), Some(dir));
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            assert!(name.starts_with(".snapshot-"), "{name}");
        }
    }

    /// Injected-clock gate: nothing uploads before its time, whatever the
    /// wall clock says. Pure function on explicit Instants — no sleeps.
    #[test]
    fn upload_gate_needs_quiet_and_interval() {
        let now = Instant::now();
        let old_upload = now - MIN_INTERVAL;
        // Clean is never uploaded, however stale the last one is.
        assert!(!should_upload(false, None, old_upload, now));
        assert!(!should_upload(false, Some(now - DEBOUNCE), old_upload, now));
        // Dirty but still settling.
        assert!(!should_upload(true, Some(now), old_upload, now));
        // Quiet long enough, but the last upload was just now.
        assert!(!should_upload(true, Some(now - DEBOUNCE), now, now));
        // No first observation yet: the tick is still dating the change.
        assert!(!should_upload(true, None, old_upload, now));
        // Quiet past the debounce and an old enough last upload: go.
        assert!(should_upload(true, Some(now - DEBOUNCE), old_upload, now));
        // Boundaries are inclusive: exactly quiet / exactly due uploads.
        assert!(should_upload(
            true,
            Some(now - DEBOUNCE - std::time::Duration::from_nanos(1)),
            now - MIN_INTERVAL,
            now
        ));
    }
}
