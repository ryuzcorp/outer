// Derived from noite apps/runner/src/host/state.rs @ 58a977a (Apache-2.0).
// Outer keeps the VACUUM INTO + zstd upload, the owner.json claim fence, the
// identical-hash skip and the poll/debounce/interval timers. Dropped:
// Noite's Config coupling, metrics, readiness plumbing and anything about an
// attached metrics.sqlite — Outer has one DB.

use std::path::{Path, PathBuf};
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
/// bucket (`{key_prefix}/outer.sqlite.zst` plus meta, owner, and `history/`
/// keys). Lineage: the bucket meta and a local `<db>.snapshot-sha` sidecar
/// both record the last uploaded/restored sha256; an upload lands only when
/// the bucket is empty or its sha matches the sidecar (or the bytes just
/// dumped) — otherwise the local file is the wrong database and the upload
/// is refused instead of overwriting the good snapshot.
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

    /// Hourly and release copies live here, never the live key.
    fn history_prefix(&self) -> String {
        format!("{}/history/", self.key_prefix)
    }

    /// sha256 of the snapshot this host last uploaded or restored. Written
    /// atomically only after those bytes are durable, so a crash leaves it
    /// stale but never ahead.
    fn sidecar_path(&self) -> PathBuf {
        let mut path = self.db_path.as_os_str().to_owned();
        path.push(".snapshot-sha");
        PathBuf::from(path)
    }
}

/// Hourly copies younger than this are all kept; older ones collapse to one
/// per day, days older than [`HISTORY_KEEP_DAILY_DAYS`] go, at most
/// [`HISTORY_PRUNE_MAX_DELETES`] deletions per pass (oldest first).
const HISTORY_KEEP_HOURLY: chrono::Duration = chrono::Duration::hours(48);
const HISTORY_KEEP_DAILY_DAYS: i64 = 30;
const HISTORY_PRUNE_MAX_DELETES: usize = 10;

/// The bucket's current snapshot identity, from its meta object (reused: no
/// new companion key; re-read fresh before every upload attempt).
enum BucketSnapshot {
    /// No meta object: nothing to overwrite, uploading is safe.
    Absent,
    /// A meta object exists; `sha` is its `sha256` when readable.
    Present { sha: Option<String> },
}

/// After this many consecutive lineage refusals `/ready` fails: nothing
/// routes traffic by it today (no healthcheck by design, mTLS), so this is
/// the monitoring tripwire that says backups stopped — with the refusal
/// reason attached, not just a stale age.
pub const READY_LINEAGE_REFUSAL_LIMIT: u64 = 30;

/// Upload only when the bucket has no snapshot, or its sha matches the local
/// sidecar (steady state, restore, takeover), or the bytes just dumped
/// (crash window: the retry re-sends identical bytes and repairs the
/// sidecar). No sidecar facing a snapshot is the wrong-bucket case: refuse.
fn check_lineage(
    bucket: &BucketSnapshot,
    sidecar: Option<&str>,
    local_hash: &str,
) -> Result<(), String> {
    match bucket {
        BucketSnapshot::Absent => Ok(()),
        BucketSnapshot::Present { sha: None } => Err(
            "the bucket holds a snapshot whose meta has no readable sha256; not overwriting"
                .to_string(),
        ),
        BucketSnapshot::Present {
            sha: Some(bucket_sha),
        } => {
            if sidecar == Some(bucket_sha.as_str()) || bucket_sha == local_hash {
                Ok(())
            } else {
                Err(format!(
                    "bucket snapshot {bucket_sha} matches neither the local sidecar nor this database; not overwriting"
                ))
            }
        }
    }
}

/// One prune pass, oldest first: hourly copies past 48 h collapse to the
/// first copy of each day, days past 30 d go. Release copies and unknown
/// keys are never selected. Pure over the key list and `now` for testing.
fn select_history_prune(
    keys: &[String],
    history_prefix: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    const SUFFIX: &str = ".sqlite.zst";
    let mut hourly: Vec<(chrono::DateTime<chrono::Utc>, &str)> = Vec::new();
    for key in keys {
        let Some(rest) = key.strip_prefix(history_prefix) else {
            continue;
        };
        let Some(stamp) = rest.strip_suffix(SUFFIX) else {
            continue;
        };
        if stamp.starts_with("release-") {
            continue;
        }
        let Some(at) = parse_hour_stamp(stamp) else {
            continue;
        };
        hourly.push((at, key.as_str()));
    }
    hourly.sort();
    let mut kept_days = std::collections::BTreeSet::new();
    let mut delete = Vec::new();
    for (at, key) in hourly {
        if now.signed_duration_since(at) <= HISTORY_KEEP_HOURLY {
            continue;
        }
        let oldest = (now - chrono::Duration::days(HISTORY_KEEP_DAILY_DAYS)).date_naive();
        if at.date_naive() < oldest {
            delete.push(key.to_string());
        } else if kept_days.insert(at.date_naive()) {
            // First surviving copy of this day: the daily keeper.
        } else {
            delete.push(key.to_string());
        }
    }
    delete.truncate(HISTORY_PRUNE_MAX_DELETES);
    delete
}

/// Parse an hourly stamp (`<UTC yyyy-mm-ddTHH>`). Split explicitly: chrono's
/// `NaiveDateTime` parser wants minutes too, so an hour-only format never
/// parses. Anything the writer never emits is not a history key.
fn parse_hour_stamp(stamp: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let (date, hour) = stamp.split_once('T')?;
    if hour.len() != 2 {
        return None;
    }
    let date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let h: u32 = hour.parse().ok()?;
    date.and_hms_opt(h, 0, 0).map(|naive| naive.and_utc())
}

/// Snapshot health for `/ready`: whether this host still owns the bucket
/// claim, how long ago the last upload finished, the last upload error
/// (cleared by the next success), and consecutive lineage refusals (cleared
/// by the next success; trips the `/ready` gate at
/// [`READY_LINEAGE_REFUSAL_LIMIT`]).
#[derive(Clone, Debug, serde::Serialize)]
pub struct SnapshotStatus {
    pub owner: bool,
    pub last_upload_secs_ago: Option<u64>,
    pub last_error: Option<String>,
    pub consecutive_lineage_refusals: u64,
}

/// Marker on a refused upload (see [`check_lineage`]): lets `snapshot_now`
/// count lineage refusals apart from transport or owner-fence failures.
#[derive(Debug, Clone, Copy)]
struct LineageRefused;

impl std::fmt::Display for LineageRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lineage refused")
    }
}

impl std::error::Error for LineageRefused {}

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
    /// Consecutive lineage refusals: incremented only by the gate,
    /// cleared by any success, untouched by transport or owner-fence
    /// failures (different outages, different signals).
    lineage_refusals: Arc<AtomicU64>,
}

/// What `adopt_lineage` decided: the sidecar now names the bucket sha,
/// or there was nothing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptOutcome {
    /// Sidecar written from the bucket meta sha.
    Adopted,
    /// A sidecar already exists: steady host, leave it alone.
    AlreadyAdopted,
}

/// Marker on an expected adoption refusal (no local DB, no bucket
/// snapshot, unreadable meta, foreign instance): the caller reports it
/// and moves on, instead of failing like a transport error. Boot warns
/// and continues; the CLI exits non-zero.
#[derive(Debug, Clone, Copy)]
struct AdoptSkip;

impl std::fmt::Display for AdoptSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "adoption skipped")
    }
}

impl std::error::Error for AdoptSkip {}

/// Write the lineage sidecar from the bucket meta `sha256`, but only
/// when the local database exists, no sidecar is present, and the meta
/// `instance` equals `expect_instance` — proof the bucket lineage was
/// written by the host whose data volume this is (a container id, which
/// changes across a release handoff, so the caller passes the *running*
/// container's hostname explicitly). Anything else is an error naming
/// the reason, except a present sidecar, which is exit-0 `AlreadyAdopted`.
/// Fsyncs like every other sidecar write.
pub async fn adopt_lineage(
    store: &Store,
    cfg: &SnapshotConfig,
    expect_instance: &str,
) -> anyhow::Result<AdoptOutcome> {
    if !cfg.db_path.exists() {
        return Err(anyhow::anyhow!(
            "no local database at {}; restore owns this case, nothing to adopt",
            cfg.db_path.display()
        )
        .context(AdoptSkip));
    }
    if read_sidecar(&cfg.sidecar_path()).await.is_some() {
        return Ok(AdoptOutcome::AlreadyAdopted);
    }
    let Some(bytes) = store.get(&cfg.meta_key()).await? else {
        return Err(
            anyhow::anyhow!("no snapshot in the bucket; nothing to adopt").context(AdoptSkip),
        );
    };
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let sha = v.get("sha256").and_then(|s| s.as_str());
    let instance = v.get("instance").and_then(|s| s.as_str());
    let (Some(sha), Some(instance)) = (sha, instance) else {
        return Err(anyhow::anyhow!(
            "bucket meta has no usable snapshot identity (missing sha256/instance); not adopting"
        )
        .context(AdoptSkip));
    };
    if instance != expect_instance {
        return Err(anyhow::anyhow!(
            "bucket snapshot was written by instance {instance:?}, expected {expect_instance:?}; refusing to adopt"
        )
        .context(AdoptSkip));
    }
    write_sidecar(&cfg.sidecar_path(), sha).await?;
    Ok(AdoptOutcome::Adopted)
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
            lineage_refusals: Arc::new(AtomicU64::new(0)),
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

    /// Snapshot health for `/ready`: the gate trips on
    /// [`READY_LINEAGE_REFUSAL_LIMIT`] consecutive lineage refusals (see
    /// `ready`); everything else stays additive detail.
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
            consecutive_lineage_refusals: self.lineage_refusals.load(Ordering::Relaxed),
        }
    }

    /// One-shot upgrade adoption, at boot only, before `claim`: a same-host
    /// restart whose database predates sidecars adopts via [`adopt_lineage`]
    /// (this host's own id as the expected author). A container handoff
    /// gets a new id, so `release.sh` adopts with the running container's
    /// hostname instead (see `state adopt-lineage`). Expected refusals
    /// (absent snapshot, unreadable meta, foreign instance) warn with the
    /// reason and continue; transport errors fail the boot. Returns true
    /// when it adopted.
    pub async fn adopt_sidecar_at_boot(
        &self,
        store: &Store,
        cfg: &SnapshotConfig,
    ) -> anyhow::Result<bool> {
        if !cfg.db_path.exists() {
            return Ok(false);
        }
        if read_sidecar(&cfg.sidecar_path()).await.is_some() {
            return Ok(false);
        }
        match adopt_lineage(store, cfg, self.instance.as_str()).await {
            Ok(AdoptOutcome::Adopted) => {
                tracing::warn!(
                    bucket = store.bucket_label(),
                    key = cfg.state_key(),
                    "adopted snapshot sidecar from this host's bucket meta (upgrade without sidecar); uploads resume"
                );
                Ok(true)
            }
            Ok(AdoptOutcome::AlreadyAdopted) => Ok(false),
            Err(e) if e.downcast_ref::<AdoptSkip>().is_some() => {
                tracing::warn!(
                    bucket = store.bucket_label(),
                    key = cfg.state_key(),
                    reason = format!("{e:#}"),
                    "not adopting snapshot sidecar at boot"
                );
                Ok(false)
            }
            Err(e) => Err(e),
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
                self.lineage_refusals.store(0, Ordering::Relaxed);
            }
            // Skipped as identical: `last_hash` is only recorded once state
            // and meta both landed, so the bucket already matches. Nothing
            // was uploaded, so the upload age stays the real one; an older
            // error no longer describes the bucket.
            Ok(_) => {
                *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                self.lineage_refusals.store(0, Ordering::Relaxed);
            }
            Err(e) => {
                self.dirty.store(true, Ordering::Relaxed);
                // Only the lineage gate feeds the `/ready` tripwire; a
                // transport or owner-fence failure leaves the count alone.
                if e.downcast_ref::<LineageRefused>().is_some() {
                    self.lineage_refusals.fetch_add(1, Ordering::Relaxed);
                }
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
        let hash = sha256_hex(&raw);
        // Lineage gate, before the identical-skip (so a wrong-bucket config
        // refuses loudly instead of silently skipping on unchanged bytes).
        let sidecar = read_sidecar(&cfg.sidecar_path()).await;
        let bucket = bucket_snapshot(store, cfg).await?;
        if let Err(reason) = check_lineage(&bucket, sidecar.as_deref(), &hash) {
            tracing::error!(
                bucket = store.bucket_label(),
                key = cfg.state_key(),
                reason,
                "refusing state snapshot upload: bucket identity matches neither the local sidecar nor this database; not overwriting (wrong bucket/prefix?)"
            );
            let err = anyhow::anyhow!("refusing state snapshot upload: {reason}");
            return Err(err.context(LineageRefused));
        }
        // Crash window: the bucket holds these bytes but the sidecar predates
        // them. Repair now, including on the skip path, so a later change
        // uploads instead of wedging on a stale sidecar.
        let bucket_sha = match &bucket {
            BucketSnapshot::Present { sha: Some(sha) } => Some(sha.as_str()),
            _ => None,
        };
        if bucket_sha == Some(hash.as_str()) && sidecar.as_deref() != Some(hash.as_str()) {
            write_sidecar(&cfg.sidecar_path(), &hash).await?;
        }
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
        // The sidecar lands after state and meta both landed, and before
        // `last_hash`: a crash here retries the upload (the gate allows it)
        // instead of skipping as "identical" with a stale sidecar.
        write_sidecar(&cfg.sidecar_path(), &hash).await?;
        *self.last_hash.lock().unwrap_or_else(|e| e.into_inner()) = Some(hash);
        tracing::info!(
            bytes = packed_len,
            raw_bytes = raw.len(),
            generation,
            "outer state snapshot uploaded"
        );
        file_hourly_copy(store, cfg, &packed).await;
        prune_history(store, cfg).await;
        Ok(packed_len)
    }
}

/// sha256 of snapshot bytes, hex — the one name upload, restore and sidecar
/// all use.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The bucket's snapshot identity, from its meta (re-read fresh per attempt).
async fn bucket_snapshot(store: &Store, cfg: &SnapshotConfig) -> anyhow::Result<BucketSnapshot> {
    match store.get(&cfg.meta_key()).await? {
        None => Ok(BucketSnapshot::Absent),
        Some(bytes) => {
            let sha = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.get("sha256").and_then(|s| s.as_str().map(str::to_owned)));
            Ok(BucketSnapshot::Present { sha })
        }
    }
}

/// Missing/unreadable sidecar is `None` (the gate treats "no sidecar" facing
/// a snapshot as the wrong-bucket case); an empty file is never a sha.
async fn read_sidecar(path: &Path) -> Option<String> {
    let raw = tokio::fs::read_to_string(path).await.ok()?;
    let sha = raw.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Temp + rename after the named bytes are durable: a crash leaves it stale
/// but never ahead. The temp file is fsynced before the rename and the
/// parent dir after, so a power loss cannot leave a missing sidecar that
/// the next boot misreads as the wrong-bucket case.
async fn write_sidecar(path: &Path, sha: &str) -> anyhow::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    tokio::fs::write(&tmp, format!("{sha}\n")).await?;
    tokio::fs::File::open(&tmp).await?.sync_all().await?;
    tokio::fs::rename(&tmp, path).await?;
    fsync_parent(path).await?;
    Ok(())
}

/// fsync the directory holding `path`, so a just-renamed file (sidecar or
/// restored database) survives a power loss instead of vanishing from the
/// directory entry.
async fn fsync_parent(path: &Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::File::open(dir).await?.sync_all().await?;
    }
    Ok(())
}

/// Hourly copy, first per UTC hour wins. Best-effort (the live snapshot
/// already landed); a failure warns and the next upload retries.
async fn file_hourly_copy(store: &Store, cfg: &SnapshotConfig, packed: &[u8]) {
    let hour = chrono::Utc::now().format("%Y-%m-%dT%H");
    let key = format!("{}{hour}.sqlite.zst", cfg.history_prefix());
    match store.get(&key).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            if let Err(e) = store.put(&key, packed.to_vec()).await {
                tracing::warn!(key, error = %e, "state history copy failed; next upload retries");
            }
        }
        Err(e) => {
            tracing::warn!(key, error = %e, "state history check failed; next upload retries")
        }
    }
}

/// Prune only after a successful upload: never the live key, bounded per
/// pass, failures warn and continue.
async fn prune_history(store: &Store, cfg: &SnapshotConfig) {
    let prefix = cfg.history_prefix();
    let keys = match store.list(&prefix).await {
        Ok(keys) => keys,
        Err(e) => {
            tracing::warn!(error = %e, "state history prune list failed; next upload retries");
            return;
        }
    };
    for key in select_history_prune(&keys, &prefix, chrono::Utc::now()) {
        if let Err(e) = store.delete(&key).await {
            tracing::warn!(key, error = %e, "state history prune delete failed");
        }
    }
}

/// Pre-release copy for `deploy/release.sh`: files the live snapshot under
/// `{prefix}/history/release-<UTC timestamp>-<git sha>.sqlite.zst`, or
/// returns `None` (skipped) when the bucket holds no snapshot yet. No
/// server-side copy exists in the store layer, so download plus re-upload.
pub async fn release_copy(
    store: &Store,
    cfg: &SnapshotConfig,
    git_sha: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<Option<String>> {
    anyhow::ensure!(
        !git_sha.is_empty() && git_sha.chars().all(|c| c.is_ascii_hexdigit()),
        "release copy needs a hex git sha, got {git_sha:?}"
    );
    let Some(bytes) = store.get(&cfg.state_key()).await? else {
        tracing::warn!("no state snapshot in the bucket; skipping the pre-release copy");
        return Ok(None);
    };
    let key = format!(
        "{}release-{}-{git_sha}.sqlite.zst",
        cfg.history_prefix(),
        now.format("%Y-%m-%dT%H-%M-%SZ"),
    );
    store.put(&key, bytes).await?;
    tracing::info!(key, "state pre-release copy filed");
    Ok(Some(key))
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

/// Missing-key error that [`is_not_found`] recognizes: only a missing key
/// counts as "no snapshot", never a transport failure.
fn not_found(key: &str) -> anyhow::Error {
    super::s3::not_found_error(key)
}

/// Boot path: if the local DB file does not exist, download the snapshot
/// before connecting, and record its sha in the local sidecar so the first
/// upload's lineage gate recognizes this database as the bucket's. Returns
/// Ok(false) only when the bucket has no snapshot; any other failure is an
/// error the caller must not start past, because starting with an empty
/// database would upload it over the good snapshot on the next write. If
/// the local file exists, keep it.
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
/// transport error must never read as "start fresh". A restored database
/// records the sidecar, so it is provably the bucket's database when the
/// first upload's lineage gate runs.
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
    // a truncated database that "exists". The data must reach the disk
    // before the rename — without the fsync a power loss right after the
    // rename can leave a zero-length file, which reads as a valid empty
    // SQLite database and would then be uploaded over the good snapshot.
    let decoded = cfg.db_path.with_extension("sqlite.decoded");
    {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::File::create(&decoded).await?;
        f.write_all(&raw).await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(&decoded, &cfg.db_path).await?;
    // The rename itself must survive a power loss too: without the dir
    // fsync the new database can vanish and the next boot sees "no local
    // file", restores again (or, worse, starts fresh if the bucket moved).
    fsync_parent(&cfg.db_path).await?;
    // The bytes are durable now: record the sidecar so the lineage gate
    // adopts this database. Before the rename it would name bytes that may
    // never land; after, a crash leaves at worst a stale sidecar, which the
    // crash-window rule repairs.
    write_sidecar(&cfg.sidecar_path(), &sha256_hex(&raw)).await?;
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
        // Seeded once; a reopened database already has the table.
        let exists: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'kv'",
        )
        .fetch_one(&pool)
        .await
        .expect("table probe");
        if !exists {
            sqlx::query("CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT)")
                .execute(&pool)
                .await
                .expect("test table");
            sqlx::query("INSERT INTO kv (k, v) VALUES ('a', '1')")
                .execute(&pool)
                .await
                .expect("seed row");
        }
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
        // Boundaries are inclusive: exactly quiet / exactly due uploads.
        assert!(should_upload(
            true,
            Some(now - DEBOUNCE - std::time::Duration::from_nanos(1)),
            now - MIN_INTERVAL,
            now
        ));
    }

    fn sidecar_path(cfg: &super::SnapshotConfig) -> std::path::PathBuf {
        let mut path = cfg.db_path.as_os_str().to_owned();
        path.push(".snapshot-sha");
        std::path::PathBuf::from(path)
    }

    async fn live_bytes(store: &super::Store, cfg: &super::SnapshotConfig) -> Vec<u8> {
        let key = format!("{}/outer.sqlite.zst", cfg.key_prefix);
        store
            .get(&key)
            .await
            .expect("read live snapshot")
            .expect("live snapshot exists")
    }

    /// One claimed host with one uploaded snapshot; the tempdir is returned
    /// so the database file outlives the pool.
    async fn seed_store() -> (
        tempfile::TempDir,
        super::Store,
        super::SnapshotConfig,
        sqlx::SqlitePool,
    ) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = super::Store::memory();
        let cfg = test_cfg(dir.path(), "host-1");
        let pool = test_pool(&cfg.db_path).await;
        let sync = super::StateSync::new("host-1");
        sync.claim(&store, &cfg).await.expect("claim");
        sync.snapshot_now(&pool, &store, &cfg)
            .await
            .expect("first upload");
        (dir, store, cfg, pool)
    }

    /// Wrong bucket: the local DB and sidecar belong to bucket A, but the
    /// config now points at bucket B holding a different snapshot. The
    /// upload must be refused and B's snapshot left untouched.
    #[tokio::test]
    async fn wrong_bucket_upload_is_refused() {
        let (_dir, local, cfg, pool) = seed_store().await;
        let sync = super::StateSync::new("host-1");
        sync.claim(&local, &cfg).await.expect("claim");

        // Bucket B holds a different snapshot: seed, diverge the database,
        // upload again under a fresh sync (a rebooted host).
        let (_other_dir, remote, other_cfg, other_pool) = seed_store().await;
        sqlx::query("INSERT INTO kv (k, v) VALUES ('b', '2')")
            .execute(&other_pool)
            .await
            .expect("diverge");
        let other_sync = super::StateSync::new("host-1");
        other_sync.claim(&remote, &other_cfg).await.expect("claim");
        other_sync
            .snapshot_now(&other_pool, &remote, &other_cfg)
            .await
            .expect("remote upload");
        other_pool.close().await;
        let before = live_bytes(&remote, &other_cfg).await;

        // Same local DB, now pointed at bucket B: refuse, no write.
        let err = sync
            .snapshot_now(&pool, &remote, &cfg)
            .await
            .expect_err("wrong-bucket upload must be refused");
        assert!(
            format!("{err:#}").contains("refusing"),
            "unexpected: {err:#}"
        );
        assert_eq!(live_bytes(&remote, &cfg).await, before);
        pool.close().await;
    }

    /// A fresh local DB with no sidecar must not upload over a bucket that
    /// already holds a snapshot (the wrong-bucket boot that started fresh).
    #[tokio::test]
    async fn fresh_local_db_does_not_upload_over_a_bucket_snapshot() {
        let (_dir, store, cfg, pool) = seed_store().await;
        pool.close().await;
        let before = live_bytes(&store, &cfg).await;

        let fresh = tempfile::tempdir().expect("temp dir");
        let fresh_cfg = test_cfg(fresh.path(), "host-1");
        let fresh_pool = test_pool(&fresh_cfg.db_path).await;
        // A wrong-bucket boot starts a *different* database: same schema,
        // different bytes (otherwise the crash-window rule would — rightly —
        // read this as the bucket's own bytes).
        sqlx::query("INSERT INTO kv (k, v) VALUES ('fresh', '1')")
            .execute(&fresh_pool)
            .await
            .expect("fresh row");
        let fresh_sync = super::StateSync::new("host-1");
        let err = fresh_sync
            .snapshot_now(&fresh_pool, &store, &fresh_cfg)
            .await
            .expect_err("fresh local over a bucket snapshot must be refused");
        assert!(
            format!("{err:#}").contains("refusing"),
            "unexpected: {err:#}"
        );
        assert_eq!(live_bytes(&store, &cfg).await, before);
        fresh_pool.close().await;
    }

    /// Steady state advances the sidecar: after an upload it names the sha
    /// the bucket meta records. (Checked inside the history test, which
    /// already seeds and uploads.)
    async fn assert_sidecar_matches_meta(store: &super::Store, cfg: &super::SnapshotConfig) {
        let sidecar = tokio::fs::read_to_string(sidecar_path(cfg))
            .await
            .expect("sidecar written after upload");
        let meta_key = format!("{}/outer.sqlite.meta.json", cfg.key_prefix);
        let meta: serde_json::Value = serde_json::from_slice(
            &store
                .get(&meta_key)
                .await
                .expect("read meta")
                .expect("meta exists"),
        )
        .expect("meta json");
        assert_eq!(
            sidecar.trim(),
            meta.get("sha256")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
        );
    }

    /// A crash between the upload and the sidecar write must not wedge the
    /// next upload (the bucket already holds these bytes: the retry repairs
    /// the sidecar) — but a changed database with a stale sidecar stays
    /// refused, since the bucket holds something this host cannot prove it
    /// wrote.
    #[tokio::test]
    async fn crash_window_recovers_then_changed_db_refuses() {
        let (_dir, store, cfg, pool) = seed_store().await;
        pool.close().await;
        let stale = "0".repeat(64);

        // Reboot with a stale sidecar and an unchanged database: allowed,
        // and the sidecar is repaired to the bucket's sha.
        tokio::fs::write(sidecar_path(&cfg), &stale)
            .await
            .expect("stale sidecar");
        let pool = test_pool(&cfg.db_path).await;
        let reboot = super::StateSync::new("host-1");
        reboot.claim(&store, &cfg).await.expect("claim");
        reboot
            .snapshot_now(&pool, &store, &cfg)
            .await
            .expect("crash recovery must not wedge uploads");
        assert_sidecar_matches_meta(&store, &cfg).await;

        // ... but a local change with a stale sidecar stays refused, and the
        // bucket keeps the pre-crash snapshot.
        let before = live_bytes(&store, &cfg).await;
        tokio::fs::write(sidecar_path(&cfg), &stale)
            .await
            .expect("stale sidecar");
        sqlx::query("INSERT INTO kv (k, v) VALUES ('b', '2')")
            .execute(&pool)
            .await
            .expect("local change after the crash");
        let reboot = super::StateSync::new("host-1");
        reboot.claim(&store, &cfg).await.expect("claim");
        let err = reboot
            .snapshot_now(&pool, &store, &cfg)
            .await
            .expect_err("changed db with a stale sidecar must be refused");
        assert!(
            format!("{err:#}").contains("refusing"),
            "unexpected: {err:#}"
        );
        assert_eq!(live_bytes(&store, &cfg).await, before);
        pool.close().await;
    }

    fn history_key_at(days_ago: i64, hour: u32) -> String {
        let day = (chrono::Utc::now() - chrono::Duration::days(days_ago))
            .format("%Y-%m-%d")
            .to_string();
        format!("outer/state/history/{day}T{hour:02}.sqlite.zst")
    }
    /// An upload files an hourly history copy (first copy per hour wins)
    /// while the live key keeps advancing, and the same upload prunes: old
    /// hourly copies collapse to one per day, days past 30 d go, release
    /// copies and unknown keys stay, and the live key is never touched.
    #[tokio::test]
    async fn hourly_history_copy_and_prune() {
        let (_dir, store, cfg, pool) = seed_store().await;
        let sync = super::StateSync::new("host-1");
        sync.claim(&store, &cfg).await.expect("claim");
        let first_live = live_bytes(&store, &cfg).await;

        // Seeded old copies the coming upload must prune (or keep).
        let seeds = [(3, 6), (3, 7), (10, 6), (31, 6)];
        for &(days, hour) in &seeds {
            store
                .put(&history_key_at(days, hour), b"old".to_vec())
                .await
                .expect("seed old history");
        }
        let release = "outer/state/history/release-2026-01-01T00-00-00Z-abc123.sqlite.zst";
        store
            .put(release, b"release".to_vec())
            .await
            .expect("seed release copy");
        store
            .put("outer/state/history/junk.sqlite.zst", b"junk".to_vec())
            .await
            .expect("seed unknown key");

        sqlx::query("INSERT INTO kv (k, v) VALUES ('b', '2')")
            .execute(&pool)
            .await
            .expect("new row");
        sync.snapshot_now(&pool, &store, &cfg)
            .await
            .expect("second upload");
        let second_live = live_bytes(&store, &cfg).await;
        assert_ne!(second_live, first_live);

        let keys = store
            .list("outer/state/history/")
            .await
            .expect("list history");
        // Only copies the two uploads wrote: the seeded old keys are checked
        // by the prune asserts below. At most one per hour — the current
        // hour keeps its first copy (an hour flip mid-test files a second
        // one with the new bytes instead).
        let seeds: Vec<String> = seeds
            .into_iter()
            .map(|(d, h)| history_key_at(d, h))
            .collect();
        let hourly: Vec<&String> = keys
            .iter()
            .filter(|k| {
                !k.contains("release-") && !k.ends_with("junk.sqlite.zst") && !seeds.contains(k)
            })
            .collect();
        let mut saw_first = false;
        for key in &hourly {
            let bytes = store
                .get(key.as_str())
                .await
                .expect("read history")
                .expect("history copy");
            assert!(
                bytes == first_live || bytes == second_live,
                "history holds only uploaded bytes: {key}"
            );
            saw_first = saw_first || bytes == first_live;
        }
        assert!(saw_first, "first copy survives: {keys:?}");
        // Prune: one daily keeper per old day, old days go, release and
        // unknown keys stay, the live key advances untouched.
        for kept in [
            history_key_at(3, 6),
            history_key_at(10, 6),
            release.to_string(),
            "outer/state/history/junk.sqlite.zst".to_string(),
        ] {
            assert!(keys.contains(&kept), "kept {kept}; have {keys:?}");
        }
        for dropped in [history_key_at(3, 7), history_key_at(31, 6)] {
            assert!(!keys.contains(&dropped), "dropped {dropped}; have {keys:?}");
        }
        assert_sidecar_matches_meta(&store, &cfg).await;
        pool.close().await;
    }

    /// A store failure during restore is fatal, never "no snapshot": booting
    /// past it would start fresh and upload over the good snapshot.
    #[tokio::test]
    async fn restore_refuses_boot_on_store_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = super::Store::memory();
        let cfg = test_cfg(dir.path(), "host-1");
        store.fail_next(1, "html proxy page");
        let partial = dir.path().join("x.restoring");
        let err = super::try_restore_once(&store, &cfg, &partial)
            .await
            .expect_err("store errors must fail the restore");
        assert!(
            format!("{err:#}").contains("html proxy page"),
            "unexpected: {err:#}"
        );
    }
    /// The lineage gate, all cases: absent bucket uploads, a matching
    /// sidecar uploads, a matching local hash uploads (crash window), and
    /// everything else — including a snapshot with no readable sha and a
    /// sidecar-less database facing a snapshot — refuses.
    #[test]
    fn lineage_gate_decisions() {
        use super::{check_lineage, BucketSnapshot};
        assert!(check_lineage(&BucketSnapshot::Absent, None, "abc").is_ok());
        assert!(check_lineage(&BucketSnapshot::Absent, Some("stale"), "abc").is_ok());
        let present = |sha: Option<&str>| BucketSnapshot::Present {
            sha: sha.map(str::to_string),
        };
        assert!(check_lineage(&present(Some("abc")), Some("abc"), "abc").is_ok());
        // Same bytes, stale sidecar: the crash window recovers.
        assert!(check_lineage(&present(Some("abc")), Some("stale"), "abc").is_ok());
        assert!(check_lineage(&present(Some("abc")), None, "abc").is_ok());
        // Wrong bucket: neither matches.
        assert!(check_lineage(&present(Some("abc")), Some("stale"), "xyz").is_err());
        assert!(check_lineage(&present(Some("abc")), None, "xyz").is_err());
        // A snapshot whose identity cannot be read is never safe to
        // overwrite, even when the local database claims it.
        assert!(check_lineage(&present(None), Some("abc"), "abc").is_err());
        assert!(check_lineage(&present(None), None, "abc").is_err());
    }

    /// Prune boundaries with an injected clock: exactly 48 h is kept,
    /// 48 h + 1 s collapses to one per day, day 30 is kept, day 31 goes,
    /// release copies and unknown keys are never selected, and deletions
    /// are bounded oldest-first.
    #[test]
    fn history_prune_boundaries() {
        use super::select_history_prune;
        use chrono::Duration;
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-14T15:00:00Z")
            .expect("fixed now")
            .to_utc();
        let prefix = "outer/state/history/";
        let hour_key = |at: chrono::DateTime<chrono::Utc>| {
            format!("{prefix}{}.sqlite.zst", at.format("%Y-%m-%dT%H"))
        };
        let keys = vec![
            hour_key(now - Duration::hours(48)),
            hour_key(now - Duration::hours(48) - Duration::seconds(1)),
            hour_key(now - Duration::hours(49)),
            hour_key(now - Duration::days(30)),
            hour_key(now - Duration::days(30) - Duration::hours(1)),
            hour_key(now - Duration::days(31)),
            format!("{prefix}release-2026-01-01T00-00-00Z-abc123.sqlite.zst"),
            format!("{prefix}junk.sqlite.zst"),
        ];
        let delete = select_history_prune(&keys, prefix, now);
        // 48 h + 1 s and 49 h share a UTC day: the earlier hour is the
        // daily keeper, the later one goes. Day 30 keeps its earliest
        // hour; day 31 goes. Sorted oldest-first.
        assert_eq!(
            delete,
            vec![
                hour_key(now - Duration::days(31)),
                hour_key(now - Duration::days(30)),
                hour_key(now - Duration::hours(48) - Duration::seconds(1)),
            ]
        );
        // A day with more deletions than the bound keeps the oldest-first
        // prefix of the deletion list.
        let many: Vec<String> = (0..15)
            .map(|h| {
                hour_key(
                    (now - Duration::days(10))
                        .date_naive()
                        .and_hms_opt(h, 0, 0)
                        .expect("hour")
                        .and_utc(),
                )
            })
            .collect();
        let delete = select_history_prune(&many, prefix, now);
        assert_eq!(delete.len(), super::HISTORY_PRUNE_MAX_DELETES);
        assert_eq!(delete, many[1..11].to_vec());
    }

    /// The pre-release copy files the live snapshot under a
    /// `history/release-<timestamp>-<sha>` key, and skips (no error) when
    /// the bucket holds no snapshot yet.
    #[tokio::test]
    async fn release_copy_files_and_skips() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = super::Store::memory();
        let cfg = test_cfg(dir.path(), "host-1");
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-14T15:04:22Z")
            .expect("fixed now")
            .to_utc();
        assert_eq!(
            super::release_copy(&store, &cfg, "abc123", now)
                .await
                .expect("skip is not an error"),
            None
        );
        let pool = test_pool(&cfg.db_path).await;
        let sync = super::StateSync::new("host-1");
        sync.claim(&store, &cfg).await.expect("claim");
        sync.snapshot_now(&pool, &store, &cfg)
            .await
            .expect("upload");
        pool.close().await;
        let key = super::release_copy(&store, &cfg, "abc123", now)
            .await
            .expect("copy")
            .expect("key");
        assert_eq!(
            key,
            "outer/state/history/release-2026-11-14T15-04-22Z-abc123.sqlite.zst"
        );
        assert_eq!(
            store
                .get(&key)
                .await
                .expect("read copy")
                .expect("copy bytes"),
            live_bytes(&store, &cfg).await
        );
        assert!(super::release_copy(&store, &cfg, "not a sha!!", now)
            .await
            .is_err());
    }
    /// Upgrade wedge: a database that predates sidecars adopts the bucket
    /// lineage at boot (same host id) and the next upload lands — including
    /// when the local database has diverged since the last upload.
    #[tokio::test]
    async fn upgrade_adoption_resumes_uploads() {
        let (_dir, store, cfg, pool) = seed_store().await;
        pool.close().await;
        // The pre-sidecar host: local DB exists, no sidecar file.
        assert!(cfg.db_path.exists());
        tokio::fs::remove_file(sidecar_path(&cfg))
            .await
            .expect("drop sidecar");
        // Diverge locally after the last upload.
        let pool = test_pool(&cfg.db_path).await;
        sqlx::query("INSERT INTO kv (k, v) VALUES ('b', '2')")
            .execute(&pool)
            .await
            .expect("local change");
        let sync = super::StateSync::new("host-1");
        assert!(
            sync.adopt_sidecar_at_boot(&store, &cfg)
                .await
                .expect("adopt"),
            "same-host bucket lineage must adopt"
        );
        assert_sidecar_matches_meta(&store, &cfg).await;
        sync.claim(&store, &cfg).await.expect("claim");
        assert!(
            sync.snapshot_now(&pool, &store, &cfg)
                .await
                .expect("upload resumes")
                > 0
        );
        pool.close().await;
    }

    /// Same state, but the bucket was written by another instance: no
    /// adoption, and the upload still refuses.
    #[tokio::test]
    async fn foreign_bucket_is_never_adopted() {
        let (_dir, store, seed_cfg, pool) = seed_store().await;
        pool.close().await;
        // Rewrite the bucket meta to name another instance: same bytes, a
        // foreign author. Adoption keys off the author, not the bytes.
        let meta_key = format!("{}/outer.sqlite.meta.json", seed_cfg.key_prefix);
        let mut meta: serde_json::Value = serde_json::from_slice(
            &store
                .get(&meta_key)
                .await
                .expect("read meta")
                .expect("meta exists"),
        )
        .expect("meta json");
        meta["instance"] = serde_json::Value::String("host-2".to_string());
        store
            .put(&meta_key, serde_json::to_vec(&meta).expect("meta bytes"))
            .await
            .expect("rewrite meta");
        // A local database facing that foreign bucket, with no sidecar — and
        // diverged bytes, so the crash-window rule cannot mistake it for the
        // bucket's own.
        let local = tempfile::tempdir().expect("temp dir");
        let cfg = test_cfg(local.path(), "host-1");
        let pool = test_pool(&cfg.db_path).await;
        sqlx::query("INSERT INTO kv (k, v) VALUES ('local', '1')")
            .execute(&pool)
            .await
            .expect("local divergence");
        let sync = super::StateSync::new("host-1");
        assert!(
            !sync
                .adopt_sidecar_at_boot(&store, &cfg)
                .await
                .expect("adopt"),
            "another instance's lineage must not adopt"
        );
        assert!(!sidecar_path(&cfg).exists());
        let before = live_bytes(&store, &seed_cfg).await;
        // Claim first (as boot would), so the refusal below is the lineage
        // gate's, not the owner fence's.
        sync.claim(&store, &cfg).await.expect("claim");
        let err = sync
            .snapshot_now(&pool, &store, &cfg)
            .await
            .expect_err("foreign bucket must refuse");
        assert!(
            format!("{err:#}").contains("refusing"),
            "unexpected: {err:#}"
        );
        assert_eq!(live_bytes(&store, &seed_cfg).await, before);
        pool.close().await;
    }

    /// `adopt_lineage` writes the sidecar from the bucket meta when the
    /// instance matches, refuses loudly on a mismatch or a missing database,
    /// and no-ops when the sidecar already exists.
    #[tokio::test]
    async fn adopt_lineage_decisions() {
        let (_dir, store, cfg, pool) = seed_store().await;
        pool.close().await;
        tokio::fs::remove_file(sidecar_path(&cfg))
            .await
            .expect("drop sidecar");

        // Happy path: instance matches, sidecar written from the meta sha.
        assert_eq!(
            super::adopt_lineage(&store, &cfg, "host-1")
                .await
                .expect("adopt"),
            super::AdoptOutcome::Adopted
        );
        assert_sidecar_matches_meta(&store, &cfg).await;

        // Sidecar present: exit-0 no-op, never rewritten.
        assert_eq!(
            super::adopt_lineage(&store, &cfg, "host-1")
                .await
                .expect("no-op"),
            super::AdoptOutcome::AlreadyAdopted
        );

        // Foreign instance: non-zero refusal naming both sides.
        tokio::fs::remove_file(sidecar_path(&cfg))
            .await
            .expect("drop sidecar");
        let err = super::adopt_lineage(&store, &cfg, "host-2")
            .await
            .expect_err("foreign instance must refuse");
        assert!(format!("{err:#}").contains("host-2"), "unexpected: {err:#}");
        assert!(!sidecar_path(&cfg).exists());

        // No local database: non-zero refusal, restore owns this case.
        let missing = test_cfg(std::path::Path::new("/nonexistent-dir-xyz"), "host-1");
        let err = super::adopt_lineage(&store, &missing, "host-1")
            .await
            .expect_err("missing DB must refuse");
        assert!(
            format!("{err:#}").contains("no local database"),
            "unexpected: {err:#}"
        );
    }

    /// Boot adoption with an empty bucket skips quietly: nothing to adopt,
    /// and the first upload is allowed anyway.
    #[tokio::test]
    async fn boot_adoption_skips_empty_bucket() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = super::Store::memory();
        let cfg = test_cfg(dir.path(), "host-1");
        let pool = test_pool(&cfg.db_path).await;
        pool.close().await;
        let sync = super::StateSync::new("host-1");
        assert!(!sync
            .adopt_sidecar_at_boot(&store, &cfg)
            .await
            .expect("skip"));
        assert!(!sidecar_path(&cfg).exists());
    }

    /// Takeover path: restore, modify, upload lands on the restored lineage,
    /// and the generation the claim seeded keeps counting up.
    #[tokio::test]
    async fn restore_modify_upload_keeps_generation_monotonic() {
        let (_dir, store, cfg, pool) = seed_store().await;
        pool.close().await;
        let first_gen = meta_generation(&store, &cfg).await;
        assert_eq!(first_gen, 1);

        // A new host with no local state restores the bucket snapshot.
        let fresh = tempfile::tempdir().expect("temp dir");
        let fresh_cfg = test_cfg(fresh.path(), "host-2");
        assert!(super::restore_if_missing(&store, &fresh_cfg)
            .await
            .expect("restore"));
        let pool = test_pool(&fresh_cfg.db_path).await;
        sqlx::query("INSERT INTO kv (k, v) VALUES ('b', '2')")
            .execute(&pool)
            .await
            .expect("new host writes");
        let sync = super::StateSync::new("host-2");
        sync.claim(&store, &fresh_cfg).await.expect("claim");
        assert!(
            sync.snapshot_now(&pool, &store, &fresh_cfg)
                .await
                .expect("takeover upload lands")
                > 0
        );
        assert_eq!(meta_generation(&store, &fresh_cfg).await, first_gen + 1);
        pool.close().await;
    }

    async fn meta_generation(store: &super::Store, cfg: &super::SnapshotConfig) -> u64 {
        let meta_key = format!("{}/outer.sqlite.meta.json", cfg.key_prefix);
        serde_json::from_slice::<serde_json::Value>(
            &store
                .get(&meta_key)
                .await
                .expect("read meta")
                .expect("meta exists"),
        )
        .expect("meta json")
        .get("generation")
        .and_then(|v| v.as_u64())
        .unwrap_or_default()
    }
}
