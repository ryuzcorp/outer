//! One celld fleet per app, started on demand and slept when idle.
//!
//! `FleetManager` keeps the running fleets: `ensure_awake` starts the fleet
//! for an app (coalesced per slug, bounded across slugs), `sleep_idle` stops
//! fleets whose app has gone quiet, and the proxy records activity through
//! `crate::apps::touch_request`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::apps::{self, App};
use crate::fleet::cgroups::{self, FleetLimits};
use crate::fleet::egress::EgressFirewall;
use crate::fleet::health;
use crate::fleet::supervisor::{
    self, FleetOpts, FleetSpec, RunningFleet, SharedLogRing, StopGrace,
};
use crate::store::credentials::ScopedCredentials;

/// What the manager needs that is not per-app identity. The integration
/// owner builds this from the platform config.
#[derive(Debug, Clone)]
pub struct ManagerSettings {
    /// `{data}`: fleet state lives at `{data}/fleets/{slug}`.
    pub data_dir: PathBuf,
    /// Celld binary (`celld` on `PATH` in production).
    pub celld_bin: String,
    /// Bucket and creds the fleet pulls its deploys from.
    pub storage: FleetStorage,
    /// Cgroup anchor when containment is active, else `None` (dev).
    pub cgroups: Option<PathBuf>,
    /// `memory.max` per fleet leaf, bytes (0 leaves the kernel default).
    pub memory_max_bytes: u64,
    /// `cpu.max` per fleet leaf (`"MAX PERIOD"`).
    pub cpu_max: String,
    /// Idle time after the last request before a fleet sleeps.
    pub sleep_after: Duration,
    /// Credential age after which a running fleet restarts with freshly
    /// minted credentials (prod: 6 days for a 7-day TTL). Applies to fleets
    /// that are not being put to sleep this sweep tick.
    pub credential_refresh_after: Duration,
    /// How long a wake waits for `/.well-known/celld/health`.
    pub wake_timeout: Duration,
    /// How many fleets may start at once.
    pub wake_concurrency: usize,
    /// At most this many Pro always-on fleets stay awake at once (from
    /// `OUTER_ALWAYS_ON_MAX`). Past the cap the oldest-idle flagged apps
    /// may still sleep, and the warm-up stops waking. 0 disables the
    /// keep-warm protection entirely.
    pub always_on_max: usize,
    /// At most this many apps hold preview state at once (from
    /// `OUTER_PREVIEW_MAX`). A preview commit past the cap refuses with a
    /// retryable error naming the cap, so one app's trials can't starve the
    /// host's fleet memory for everyone else.
    pub preview_max: usize,
}

/// Bucket and credentials a fleet pulls its deploys from. The credentials
/// are per-app: `CredentialSource::R2Temp` mints keys scoped to the app's
/// prefix and reuses the cached grant while it stays fresh (see
/// `store::credentials`), so fleets never hold the root bucket keys.
#[derive(Debug, Clone)]
pub struct FleetStorage {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub credentials: crate::store::credentials::CredentialSource,
}

impl FleetStorage {
    /// `s3://{bucket}/fleets/{slug}`.
    pub fn bucket_url(&self, slug: &str) -> String {
        format!("s3://{}/fleets/{slug}", self.bucket)
    }

    /// `fleets/{slug}/`: the prefix every scoped credential is minted for,
    /// matching [`FleetStorage::bucket_url`].
    pub fn fleet_prefix(slug: &str) -> String {
        format!("fleets/{slug}/")
    }
}

struct Inner {
    data_dir: PathBuf,
    celld_bin: String,
    storage: FleetStorage,
    cgroups: Option<PathBuf>,
    memory_max_bytes: u64,
    cpu_max: String,
    sleep_after: Duration,
    credential_refresh_after: Duration,
    wake_timeout: Duration,
    always_on_max: usize,
    preview_max: usize,
    wake_permits: tokio::sync::Semaphore,
    /// The installed per-app egress policy. Disabled until `serve` sets the
    /// boot-installed handle; the gates below refuse wakes while a required
    /// policy is missing.
    egress: std::sync::RwLock<EgressFirewall>,
    fleets: tokio::sync::Mutex<HashMap<String /* slug */, LiveFleet>>,
    wakes: tokio::sync::Mutex<HashMap<String /* slug */, Arc<tokio::sync::Mutex<()>>>>,
}

/// A tracked fleet, when this process started it, and when its bucket
/// credentials were minted (`None` for Static, which never expires and never
/// refreshes). The sweep never sleeps a fleet younger than `sleep_after`: a
/// just-woken fleet's activity row can still look stale while its first
/// requests are in flight.
struct LiveFleet {
    fleet: RunningFleet,
    started: std::time::Instant,
    credential_minted_at: Option<std::time::Instant>,
}

/// Phase timings for one fleet start, for the wake log line.
struct WakeReport {
    mint_ms: u64,
    spawn_ms: u64,
    health_ms: u64,
    total_ms: u64,
    /// The spawn ran on the cached grant: no Cloudflare round trip, so
    /// `mint_ms` is 0.
    reused_credentials: bool,
}

/// Persist a fleet's ring tail before the ring dies with the fleet (sleep
/// and credential-refresh stops), except preview fleets: their rings are
/// dropped so preview output never lands in the live log namespace. The
/// sweep flushes every tick, so this is normally a handful of lines. A flush
/// failure only warns: the fleet still stops. Both callers hold the slug's
/// wake lock (the sweep takes it across evaluate-and-stop; `ensure_awake`
/// holds it from its first liveness check to its activity write), so a
/// wake-spawned ring cannot be orphaned while this snapshot flushes.
async fn flush_ring_before_stop(
    pool: &SqlitePool,
    app_id: &str,
    slug: &str,
    logs: &crate::fleet::supervisor::SharedLogRing,
) {
    // Preview fleets share the base app id but track under their own
    // `<slug>--preview` key: persisting their ring under the base id would
    // advance the live log watermark and file preview errors as live app
    // issues. Preview output stays in its deploy log tail; the sweep drops
    // the ring tail here instead. Slugs can never end in `--preview`
    // (reserved), so the suffix test cannot misfire on a live fleet.
    if crate::preview::strip_preview_key(slug).is_some() {
        return;
    }
    let tail = crate::observe::logs::ring_snapshot(logs).await;
    if let Err(err) = crate::observe::logs::flush_fleet(pool, app_id, &tail).await {
        tracing::warn!(slug = %slug, error = %format!("{err:#}"), "pre-stop flush failed; tail lines lost");
    }
}

/// Running fleets, started on demand and slept when idle. Cheap to clone.
#[derive(Clone)]
pub struct FleetManager {
    inner: Arc<Inner>,
}

impl FleetManager {
    pub fn new(settings: ManagerSettings) -> Self {
        let inner = Inner {
            data_dir: settings.data_dir,
            celld_bin: settings.celld_bin,
            storage: settings.storage,
            cgroups: settings.cgroups,
            memory_max_bytes: settings.memory_max_bytes,
            cpu_max: settings.cpu_max,
            sleep_after: settings.sleep_after,
            credential_refresh_after: settings.credential_refresh_after,
            wake_timeout: settings.wake_timeout,
            always_on_max: settings.always_on_max,
            preview_max: settings.preview_max,
            wake_permits: tokio::sync::Semaphore::new(settings.wake_concurrency.max(1)),
            egress: std::sync::RwLock::new(EgressFirewall::disabled()),
            fleets: tokio::sync::Mutex::new(HashMap::new()),
            wakes: tokio::sync::Mutex::new(HashMap::new()),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// The boot-installed egress handle (`serve` only). Tests keep the
    /// disabled default, so wakes run without a policy there.
    pub fn set_egress(&self, egress: EgressFirewall) {
        *self.inner.egress.write().expect("egress lock") = egress;
    }

    fn egress(&self) -> EgressFirewall {
        self.inner.egress.read().expect("egress lock").clone()
    }

    /// The celld binary console data panels shell out to (`celld d1 …`,
    /// `celld cell …`). Cloned out so `console_data` can build its argv
    /// without reaching into the manager's internals.
    pub fn celld_bin(&self) -> String {
        self.inner.celld_bin.clone()
    }

    /// Bucket coordinates and the per-prefix credential source the data
    /// panels mint scoped grants from. The borrow lives as long as the
    /// manager; callers that need to own it (tests) clone it instead.
    pub fn storage(&self) -> &FleetStorage {
        &self.inner.storage
    }

    /// How many apps may hold preview state at once. The preview commit
    /// path refuses past this cap (retryable, naming the oldest expiry).
    pub fn preview_max(&self) -> usize {
        self.inner.preview_max
    }
    /// Platform data dir: per-slug work/cache/fleet state live under it.
    /// Preview discard wipes the preview key's share through this.
    pub(crate) fn data_dir(&self) -> std::path::PathBuf {
        self.inner.data_dir.clone()
    }

    /// Cgroup anchor when containment is active, else `None` (dev).
    pub(crate) fn cgroups_anchor(&self) -> Option<std::path::PathBuf> {
        self.inner.cgroups.clone()
    }

    async fn alive(fleets: &tokio::sync::Mutex<HashMap<String, LiveFleet>>, slug: &str) -> bool {
        let mut fleets = fleets.lock().await;
        let Some(live) = fleets.get_mut(slug) else {
            return false;
        };
        live.fleet.is_alive()
    }

    /// True when a wake currently holds this slug's wake lock. Peek only:
    /// never creates a map entry.
    fn wake_in_progress(&self, slug: &str) -> bool {
        match self.inner.wakes.try_lock() {
            Ok(wakes) => match wakes.get(slug) {
                Some(lock) => lock.try_lock().is_err(),
                None => false,
            },
            // A lock holder is mutating the map; treat as busy.
            Err(_) => true,
        }
    }

    /// The per-slug wake lock: concurrent `ensure_awake` calls for one app
    /// share a single start. Entries live as long as the manager; slugs are
    /// bounded by the app table. The sleep sweep takes the same lock, so a
    /// wake and a sleep for one app never interleave. `delete_app` holds it
    /// across the whole deletion so no wake can start mid-delete.
    pub(crate) async fn wake_lock(&self, slug: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut wakes = self.inner.wakes.lock().await;
        wakes
            .entry(slug.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Fleet invocation for `app`. Credentials come from the per-prefix
    /// cache when fresh (`fresh` forces a new mint, replacing the entry).
    /// A minting failure is a failed wake/refresh with a clear error: never
    /// a panic, never a fallback to the parent keys.
    async fn spec_for(
        &self,
        app: &App,
        fresh: bool,
    ) -> anyhow::Result<(FleetSpec, FleetOpts, ScopedCredentials)> {
        let state_dir = self.inner.data_dir.join("fleets").join(&app.slug);
        let (cgroup_leaf, cgroups_active) = match &self.inner.cgroups {
            Some(anchor) => {
                let leaf = cgroups::ensure_fleet_leaf(
                    anchor,
                    &app.slug,
                    &FleetLimits::new(self.inner.memory_max_bytes, &self.inner.cpu_max),
                )?;
                (leaf, true)
            }
            // No anchor (dev): the state dir stands in as the leaf. `start`
            // only warns when its cgroup move fails while inactive.
            None => (state_dir.clone(), false),
        };
        let storage = &self.inner.storage;
        let prefix = FleetStorage::fleet_prefix(&app.slug);
        let creds = if fresh {
            storage.credentials.for_prefix_fresh(&prefix).await
        } else {
            storage.credentials.for_prefix(&prefix).await
        }
        .map_err(|err| {
            anyhow::anyhow!(
                "minting scoped R2 credentials for fleet {}: {err:#}",
                app.slug
            )
        })?;
        let spec = FleetSpec {
            slug: app.slug.clone(),
            uid: app.uid,
            gid: app.uid,
            public_port: app.port,
            internal_port: app.internal_port,
            bucket_url: storage.bucket_url(&app.slug),
            s3_endpoint: storage.endpoint.clone(),
            s3_region: storage.region.clone(),
            access_key_id: creds.access_key_id.clone(),
            secret_access_key: creds.secret_access_key.clone(),
            session_token: creds.session_token.clone(),
            cgroup_leaf,
            cgroups_active,
            // Checked against `is_reserved_env_name`: RUST_LOG carries no
            // reserved prefix, so `fleet_command` passes it through.
            extra_env: vec![("RUST_LOG".to_string(), "warn,cell_console=info".to_string())],
        };
        let opts = FleetOpts {
            celld_bin: self.inner.celld_bin.clone(),
            state_dir,
            shutdown_total_ms: 20_000,
            idle_evict_secs: 120,
            deploy_poll_secs: 30,
            asset_cache_mb: 64,
        };
        Ok((spec, opts, creds))
    }

    /// Mint, start and health-gate the app's fleet, tracking it on success
    /// with its credential mint time. The caller holds the slug's wake lock.
    /// On an unhealthy fleet the child is stopped and removed: a failed
    /// start never leaves a tracked fleet behind.
    async fn spawn_and_gate(
        &self,
        pool: &SqlitePool,
        app: &App,
        fresh_credentials: bool,
    ) -> anyhow::Result<WakeReport> {
        let total_start = std::time::Instant::now();
        let mint_start = std::time::Instant::now();
        let (spec, opts, creds) = self.spec_for(app, fresh_credentials).await?;
        let mint_ms = if creds.reused {
            0
        } else {
            mint_start.elapsed().as_millis() as u64
        };
        // The ring continues past the persisted lines. A DB read failure
        // falls back to 0 rather than failing the wake: colliding new
        // lines are then dropped by the flush's INSERT OR IGNORE, a
        // bounded loss only in a path where SQLite already errored.
        let log_seq_start = crate::observe::logs::max_seq(pool, &app.id)
            .await
            .map(|max| max.unwrap_or(-1).saturating_add(1).max(0) as u64)
            .unwrap_or(0);
        let spawn_start = std::time::Instant::now();
        let fleet = supervisor::start(&spec, &opts, log_seq_start).await?;
        let spawn_ms = spawn_start.elapsed().as_millis() as u64;
        self.inner.fleets.lock().await.insert(
            app.slug.clone(),
            LiveFleet {
                fleet,
                started: std::time::Instant::now(),
                credential_minted_at: creds.minted_at,
            },
        );
        let health_start = std::time::Instant::now();
        if health::wait_healthy("127.0.0.1", app.port, self.inner.wake_timeout).await {
            return Ok(WakeReport {
                mint_ms,
                spawn_ms,
                health_ms: health_start.elapsed().as_millis() as u64,
                total_ms: total_start.elapsed().as_millis() as u64,
                reused_credentials: creds.reused,
            });
        }
        let mut fleets = self.inner.fleets.lock().await;
        // The sleep sweep may have removed it first (its activity row can
        // still look stale while no request has completed): stopping
        // nothing is still a failed start, never a panic.
        let Some(live) = fleets.remove(&app.slug) else {
            anyhow::bail!("fleet {} never became healthy", app.slug);
        };
        let mut fleet = live.fleet;
        drop(fleets);
        let tail = fleet.logs.lock().await.tail(20);
        let mut detail = tail
            .iter()
            .map(|entry| entry.line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if detail.is_empty() {
            detail = "(no log output)".to_string();
        }
        fleet.stop(StopGrace::NORMAL).await;
        anyhow::bail!(
            "fleet {} never became healthy; last log lines:\n{detail}",
            app.slug
        )
    }

    /// Start the app's fleet when it is not running and wait for its health
    /// gate. A live fleet returns at once; concurrent wakes for one app
    /// share a single start; concurrent wakes across apps are bounded by
    /// `wake_concurrency`.
    pub async fn ensure_awake(&self, pool: &SqlitePool, app: &App) -> anyhow::Result<()> {
        // Fail closed: no fleet starts while the required policy is missing.
        self.egress()
            .gate(&format!("wake the fleet for {}", app.slug))?;
        // The wake lock comes first: the alive checks and the activity write
        // below must not interleave with the sweep's evaluate-and-stop for
        // this slug.
        let key = self.wake_lock(&app.slug).await;
        let _wake = key.lock().await;
        // The caller loaded `app` before waiting on the lock; `delete_app`
        // holds the same lock while it removes the app. Re-read the row so a
        // request that queued behind a delete can't respawn the fleet (and
        // mint credentials) for an app that no longer exists.
        match apps::get_by_slug(pool, &app.slug).await? {
            Some(current) if current.id == app.id => {}
            _ => anyhow::bail!("app {} no longer exists", app.slug),
        }
        self.wake_checked(pool, app, false).await
    }

    /// Wake the preview fleet for `papp`, the synthetic preview identity
    /// ([`crate::preview::preview_app`]). The base row is re-read under the
    /// preview key's wake lock and must still name exactly this fleet
    /// (ports included): a request that queued behind a discard, an
    /// expiry, or a refreshed preview can't respawn the wrong fleet.
    /// Activity lands on the preview columns, never on the live row.
    pub async fn ensure_awake_preview(&self, pool: &SqlitePool, papp: &App) -> anyhow::Result<()> {
        // Fail closed: no fleet starts while the required policy is missing.
        self.egress()
            .gate(&format!("wake the preview fleet for {}", papp.slug))?;
        let base_slug = crate::preview::strip_preview_key(&papp.slug)
            .ok_or_else(|| anyhow::anyhow!("not a preview fleet: {}", papp.slug))?;
        let key = self.wake_lock(&papp.slug).await;
        let _wake = key.lock().await;
        let base = match apps::get_by_slug(pool, base_slug).await? {
            Some(base) => base,
            None => anyhow::bail!("preview base {base_slug} no longer exists"),
        };
        match crate::preview::preview_of(&base) {
            Some(preview)
                if base.id == papp.id
                    && preview.port == papp.port
                    && preview.internal_port == papp.internal_port => {}
            _ => anyhow::bail!("preview for {base_slug} changed or was discarded"),
        }
        self.wake_checked(pool, papp, true).await
    }

    /// The wake after the existence check: share one start per slug, reuse
    /// a healthy port, else spawn and health-gate. Preview wakes record
    /// activity on the preview columns and never clear the live asleep
    /// marker.
    async fn wake_checked(
        &self,
        pool: &SqlitePool,
        app: &App,
        is_preview: bool,
    ) -> anyhow::Result<()> {
        if Self::alive(&self.inner.fleets, &app.slug).await {
            self.mark_activity(pool, app, is_preview).await?;
            return Ok(());
        }
        let _permit = self
            .inner
            .wake_permits
            .acquire()
            .await
            .map_err(|err| anyhow::anyhow!("wake semaphore closed: {err}"))?;
        // A fleet can outlive outer: the child reparents on restart and
        // keeps serving while the new process's map is empty. Starting a
        // second celld would collide on the port, so one probe comes first;
        // a healthy port means awake. `wait_healthy` tries once before its
        // first sleep, so a zero timeout is a single probe, not a skip.
        if health::wait_healthy("127.0.0.1", app.port, Duration::ZERO).await {
            self.mark_activity(pool, app, is_preview).await?;
            if !is_preview {
                apps::set_asleep(pool, &app.id, None).await?;
            }
            return Ok(());
        }
        let report = self
            .spawn_and_gate(pool, app, false)
            .await
            .map_err(|err| anyhow::anyhow!("failed wake for fleet {}: {err:#}", app.slug))?;
        self.mark_activity(pool, app, is_preview).await?;
        if !is_preview {
            apps::set_asleep(pool, &app.id, None).await?;
        }
        // One INFO line per successful wake, for the VPS wake-time baseline:
        // `mint_ms` is 0 when the spawn reused the cached grant. No secrets
        // in the fields.
        tracing::info!(
            slug = %app.slug,
            mint_ms = report.mint_ms,
            spawn_ms = report.spawn_ms,
            health_ms = report.health_ms,
            total_ms = report.total_ms,
            reused_credentials = report.reused_credentials,
            "fleet wake complete",
        );
        Ok(())
    }

    /// A successful wake counts as activity: without this, the next sweep
    /// would sleep a fleet whose first requests are still in flight. A
    /// preview wake touches the preview columns, so preview traffic never
    /// extends the live app's activity (and vice versa).
    async fn mark_activity(
        &self,
        pool: &SqlitePool,
        app: &App,
        is_preview: bool,
    ) -> anyhow::Result<()> {
        if is_preview {
            crate::preview::touch_preview(pool, &app.id, apps::now_ms()).await
        } else {
            apps::touch_request(pool, &app.id, apps::now_ms()).await
        }
    }

    /// Reload a live fleet without a restart; a no-op while asleep.
    pub async fn reload(&self, app: &App) -> anyhow::Result<()> {
        let mut fleets = self.inner.fleets.lock().await;
        let Some(live) = fleets.get_mut(&app.slug) else {
            return Ok(());
        };
        if !live.fleet.is_alive() {
            return Ok(());
        }
        live.fleet.reload().await
    }
    /// Stop and untrack the fleet for `slug`, and drop its cached scoped
    /// credentials so a recreated slug mints fresh. A missing fleet is a
    /// no-op. Caller holds the slug's wake lock (see [`Self::wake_lock`]):
    /// `delete_app` takes it across the whole deletion, and this must not
    /// block on it again.
    pub(crate) async fn stop_tracked(&self, slug: &str) {
        let mut fleets = self.inner.fleets.lock().await;
        let Some(live) = fleets.remove(slug) else {
            drop(fleets);
            self.inner
                .storage
                .credentials
                .forget_prefix(&FleetStorage::fleet_prefix(slug));
            return;
        };
        let mut fleet = live.fleet;
        drop(fleets);
        fleet.stop(StopGrace::NORMAL).await;
        self.inner
            .storage
            .credentials
            .forget_prefix(&FleetStorage::fleet_prefix(slug));
    }

    /// Stop the fleet for `slug` (no-op when asleep or missing) and drop its
    /// cached scoped credentials. Takes the slug's wake lock; callers that
    /// already hold it use [`Self::stop_tracked`].
    pub async fn stop_app(&self, slug: &str) {
        let key = self.wake_lock(slug).await;
        let _wake = key.lock_owned().await;
        self.stop_tracked(slug).await;
    }

    /// Stop the preview fleet for `slug` (no-op when asleep or missing) and
    /// drop its cached scoped credentials. The row keeps its preview state:
    /// the fleet wakes on the next preview request until the preview is
    /// discarded or expires.
    pub async fn stop_preview(&self, slug: &str) {
        self.stop_app(&crate::preview::preview_key(slug)).await;
    }

    /// Stop every fleet whose app has been quieter than `sleep_after`, drop
    /// dead fleets from the map, restart the fleet before ITS credentials
    /// expire (credential age, not process uptime) with freshly minted
    /// credentials, and return the slugs put to sleep. The app's activity is
    /// read fresh from the DB per fleet. The sleep check comes first:
    /// refresh applies only to fleets that are not being put to sleep this
    /// tick.
    /// Slugs the sleep sweep must skip this tick: flagged always-on apps
    /// whose owner is still Pro, minus the oldest-idle excess past
    /// `always_on_max` ([`over_cap_victims`]). A downgraded (Free) owner
    /// keeps the flag stored but unprotected, so the sweep treats the app
    /// as ordinary. Read without locks — the per-slug loop re-checks the
    /// row under its wake lock — so a flag flipped mid-tick converges on
    /// the next tick at worst.
    async fn keep_warm_exempt(&self, pool: &SqlitePool) -> anyhow::Result<HashSet<String>> {
        let max = self.inner.always_on_max;
        if max == 0 {
            return Ok(HashSet::new());
        }
        let slugs: Vec<String> = self.inner.fleets.lock().await.keys().cloned().collect();
        let mut idle: Vec<(String, i64)> = Vec::new();
        for slug in slugs {
            let Some(app) = apps::get_by_slug(pool, &slug).await? else {
                continue;
            };
            if !app.always_on {
                continue;
            }
            if crate::apps::always_on::owner_plan(pool, &app.owner_id).await? != "pro" {
                continue;
            }
            // Dead fleets leave the map through the sweep's own path; only
            // live ones count toward the cap.
            if !Self::alive(&self.inner.fleets, &slug).await {
                continue;
            }
            let newest = app
                .last_request_at
                .unwrap_or(app.created_at)
                .max(app.created_at);
            idle.push((slug, newest));
        }
        let victims: HashSet<String> = over_cap_victims(idle.clone(), max).into_iter().collect();
        Ok(idle
            .into_iter()
            .map(|(slug, _)| slug)
            .filter(|slug| !victims.contains(slug))
            .collect())
    }

    /// How many Pro always-on fleets are awake (tracked and live) right
    /// now. The warm-up stops waking at `always_on_max`.
    async fn always_on_awake(&self, pool: &SqlitePool) -> anyhow::Result<usize> {
        let slugs: Vec<String> = self.inner.fleets.lock().await.keys().cloned().collect();
        let mut awake = 0;
        for slug in slugs {
            if !Self::alive(&self.inner.fleets, &slug).await {
                continue;
            }
            let Some(app) = apps::get_by_slug(pool, &slug).await? else {
                continue;
            };
            if !app.always_on {
                continue;
            }
            if crate::apps::always_on::owner_plan(pool, &app.owner_id).await? != "pro" {
                continue;
            }
            awake += 1;
        }
        Ok(awake)
    }

    /// Wake stopped always-on apps (Pro owner, `desired_state` running,
    /// moderation ok, neither paused nor expired, deployed at least once):
    /// after a restart or crash nothing is tracked, and without this they
    /// would sit asleep until a request. Most-recently-active first, stopping
    /// at `always_on_max` awake — and after that many failures, so crashing
    /// fleets can't stall the sweep — on top of the wake semaphore inside
    /// [`Self::ensure_awake`]. Failures warn and the next tick retries.
    /// Apps that never deployed are skipped: there is nothing
    /// to serve yet, and waking them would fail the health gate every tick.
    pub async fn warm_always_on(&self, pool: &SqlitePool) -> anyhow::Result<Vec<String>> {
        let max = self.inner.always_on_max;
        if max == 0 {
            return Ok(Vec::new());
        }
        // Eligibility mirrors cron's `due` filters: only apps that would
        // otherwise serve requests.
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT a.id AS id, a.slug AS slug FROM app AS a JOIN user AS u ON u.id = a.owner_id \
             WHERE a.always_on = 1 AND u.plan = 'pro' \
               AND a.desired_state = 'running' AND a.moderation_state = 'ok' \
               AND a.paused_reason IS NULL AND a.expired_at IS NULL \
               AND a.last_deploy_sha IS NOT NULL \
             ORDER BY COALESCE(a.last_request_at, a.created_at) DESC",
        )
        .fetch_all(pool)
        .await?;
        let mut awake = self.always_on_awake(pool).await?;
        let mut woken = Vec::new();
        // A crashing fleet fails every wake (up to `wake_timeout` each, one
        // after another), so bound failures per tick too: without this, a
        // fleet that never starts stalls the sweep's flush/drain/sleep work.
        // Failures warn and the next tick retries.
        let mut failed = 0usize;
        for (id, slug) in rows {
            if Self::alive(&self.inner.fleets, &slug).await {
                continue;
            }
            if awake >= max || failed >= max {
                break;
            }
            let Some(app) = apps::get_by_id(pool, &id).await? else {
                continue;
            };
            if self.wake_in_progress(&slug) {
                continue;
            }
            match self.ensure_awake(pool, &app).await {
                Ok(()) => {
                    awake += 1;
                    woken.push(slug);
                }
                Err(err) => {
                    failed += 1;
                    tracing::warn!(
                        slug = %slug,
                        error = %format!("{err:#}"),
                        "keep-warm wake failed; next tick retries"
                    );
                }
            }
        }
        Ok(woken)
    }

    /// Stop every fleet whose app has been quieter than `sleep_after`, drop
    /// dead fleets from the map, restart the fleet before ITS credentials
    /// expire (credential age, not process uptime) with freshly minted
    /// credentials, and return the slugs put to sleep. Always-on Pro apps
    /// are exempt up to `always_on_max` (oldest-idle sleep past the cap).
    /// The app's activity is read fresh from the DB per fleet. The sleep
    /// check comes first: refresh applies only to fleets that are not
    /// being put to sleep this tick.
    pub async fn sleep_idle(&self, pool: &SqlitePool) -> anyhow::Result<Vec<String>> {
        let now = apps::now_ms();
        let slugs: Vec<String> = self.inner.fleets.lock().await.keys().cloned().collect();
        // Keep-warm skips, computed once per tick (the loop re-checks the
        // row under each slug's wake lock).
        let exempt = self.keep_warm_exempt(pool).await?;
        let mut slept = Vec::new();
        for slug in slugs {
            // A wake holds this slug's wake lock from its first liveness
            // check to its activity write. Skipping while it is held (and
            // holding it across the evaluate-and-stop below) keeps a wake
            // and a sleep for one app from interleaving, even when
            // `sleep_after` is shorter than `wake_timeout`.
            // Fast path; the guard held across the act below is the real
            // guarantee (a wake starting after this peek still loses the
            // try_lock race and the sweep skips it).
            if self.wake_in_progress(&slug) {
                continue;
            }
            let key = self.wake_lock(&slug).await;
            let Ok(_wake) = key.try_lock() else {
                continue;
            };
            // A tracked key resolves to a live app row, a live preview (the
            // base row still names exactly this fleet), or nothing (orphan).
            // Previews sleep on the same idleness rule but from the preview
            // columns, are never keep-warm exempt, and never mark the live
            // row asleep.
            struct SweepFleet {
                app_id: String,
                idle_since: Option<i64>,
                created_at: i64,
                is_preview: bool,
                wake_app: App,
            }
            let resolved: Option<SweepFleet> = match apps::get_by_slug(pool, &slug).await? {
                Some(app) => Some(SweepFleet {
                    app_id: app.id.clone(),
                    idle_since: app.last_request_at,
                    created_at: app.created_at,
                    is_preview: false,
                    wake_app: app,
                }),
                None => match crate::preview::strip_preview_key(&slug) {
                    Some(base_slug) => match apps::get_by_slug(pool, base_slug).await? {
                        Some(base) => match crate::preview::preview_of(&base) {
                            Some(p) if crate::preview::preview_key(&base.slug) == slug => {
                                let mut fleet = base.clone();
                                fleet.slug = slug.clone();
                                fleet.port = p.port;
                                fleet.internal_port = p.internal_port;
                                fleet.last_request_at = Some(p.updated_at_ms);
                                Some(SweepFleet {
                                    app_id: base.id.clone(),
                                    idle_since: Some(p.updated_at_ms),
                                    created_at: base.created_at,
                                    is_preview: true,
                                    wake_app: fleet,
                                })
                            }
                            _ => None,
                        },
                        None => None,
                    },
                    None => None,
                },
            };
            let Some(resolved) = resolved else {
                // The app row is gone (or the preview was discarded): its
                // fleet serves nothing, so stop it without marking sleep.
                tracing::warn!(slug = %slug, "app row gone; stopping its orphan fleet");
                let mut fleets = self.inner.fleets.lock().await;
                if let Some(live) = fleets.remove(&slug) {
                    let mut fleet = live.fleet;
                    drop(fleets);
                    fleet.stop(StopGrace::NORMAL).await;
                }
                continue;
            };
            let SweepFleet {
                app_id,
                idle_since,
                created_at,
                is_preview,
                wake_app,
            } = resolved;
            let mut fleets = self.inner.fleets.lock().await;
            let Some(live) = fleets.get_mut(&slug) else {
                continue;
            };
            if !live.fleet.is_alive() {
                fleets.remove(&slug);
                continue;
            }
            // Always-on Pro apps (up to the cap) skip the sleep below and
            // fall through to the credential refresh: their flag only buys
            // freedom from idleness, not from expiring credentials. Previews
            // are never exempt: a quiet preview sleeps like any idle fleet
            // (its row keeps the preview state for wake-on-request), and a
            // preview idle past the TTL is discarded by the expiry step.
            // Idle timeout follows the app owner's plan (spec §3, owner-set
            // 2026-10-26): Free fleets sleep after 15 minutes; Pro keeps the
            // configured host idle time (keep-warm apps are exempt above).
            let owner_plan =
                match crate::apps::always_on::owner_plan(pool, &wake_app.owner_id).await {
                    Ok(plan) => plan,
                    Err(err) => {
                        tracing::warn!(
                            slug = %slug,
                            error = %format!("{err:#}"),
                            "sleep sweep: owner plan lookup failed; keeping the fleet awake"
                        );
                        continue;
                    }
                };
            let sleep_after = crate::limits::sleep_after(&owner_plan, self.inner.sleep_after);
            let idle = should_sleep(
                idle_since,
                created_at,
                now,
                live.started.elapsed(),
                sleep_after,
                self.inner.sleep_after,
            );
            if idle && (is_preview || !exempt.contains(&slug)) {
                let Some(live) = fleets.remove(&slug) else {
                    continue;
                };
                let mut fleet = live.fleet;
                drop(fleets);
                flush_ring_before_stop(pool, &app_id, &slug, &fleet.logs).await;
                fleet.stop(StopGrace::NORMAL).await;
                if !is_preview {
                    apps::set_asleep(pool, &app_id, Some(now)).await?;
                }
                slept.push(slug);
                continue;
            }
            // Credential refresh: the fleet restarts before ITS credentials
            // expire, so the decision uses credential age (when the grant
            // was minted), not process uptime. A fleet that woke on a reused
            // grant can be younger than its credentials. Only here, never on
            // the sleep path above, and under the same wake lock, so a wake
            // and a refresh for one app never interleave.
            //
            // Static fleets (`None`: no expiry) never refresh.
            let credential_age = live.credential_minted_at.map(|minted| minted.elapsed());
            let Some(age) = credential_age else {
                continue;
            };
            if !should_refresh(age, self.inner.credential_refresh_after) {
                continue;
            }
            // Like a wake, a restart takes a start permit; unlike a wake, the
            // sweep never waits for one — a full permit set means this tick
            // skips the refresh and the next tick retries.
            let Ok(_permit) = self.inner.wake_permits.try_acquire() else {
                continue;
            };
            let Some(live) = fleets.remove(&slug) else {
                continue;
            };
            // Persist the tail before the ring dies with the fleet (mirrors
            // the sleep path above): the sweep flushes every tick, so this
            // is normally a handful of lines. A flush failure still
            // restarts the fleet; the warn carries the slug.
            let mut fleet = live.fleet;
            drop(fleets);
            flush_ring_before_stop(pool, &app_id, &slug, &fleet.logs).await;
            fleet.stop(StopGrace::NORMAL).await;
            // The restart forces a fresh mint: restarting on the cached grant
            // would restart on the same near-expiry credentials.
            match self.spawn_and_gate(pool, &wake_app, true).await {
                Ok(report) => tracing::info!(
                    slug = %slug,
                    total_ms = report.total_ms,
                    "fleet restarted with freshly minted R2 credentials"
                ),
                Err(err) => tracing::warn!(
                    slug = %slug,
                    error = %format!("{err:#}"),
                    "credential refresh failed; fleet stays stopped until the next wake"
                ),
            }
        }
        Ok(slept)
    }

    /// A tokio interval loop that sleeps idle fleets, discards expired
    /// previews, and flushes fleet logs to SQLite. Failures log and the
    /// loop continues on the next tick. Retention prunes and the inactivity
    /// sweep (spec §7.8) run at most once a day; the timestamps live in this
    /// task, not in global state.
    pub fn spawn_sleep_sweep(
        &self,
        pool: SqlitePool,
        store: crate::store::Store,
        git: crate::git::GitConfig,
        sync: crate::git::GitSync,
        every: Duration,
        inactive: crate::inactivity::SweepDeps,
    ) {
        let this = self.clone();
        let store = store.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(every);
            let mut last_prune: Option<std::time::Instant> = None;
            loop {
                interval.tick().await;
                // The policy's per-uid classes drift on app create/delete and
                // plan change; the explicit refreshes at those sites cover the
                // common paths, and this tick is the backstop (best-effort, a
                // failure keeps the old table): operator-CLI mutations, R2 IP
                // rotation, anything a hook missed.
                this.egress().refresh(&pool).await;
                if let Err(err) = this.sleep_idle(&pool).await {
                    tracing::warn!(error = %err, "sleep sweep failed");
                }
                // Previews idle past the TTL are discarded: fleet stopped,
                // row cleared, mirror ref and bundles removed. One app's
                // failure warns and the rest still expire.
                if let Err(err) =
                    crate::preview::expire_due(&pool, &store, &git, &sync, &this).await
                {
                    tracing::warn!(error = %format!("{err:#}"), "preview expiry failed");
                }
                // Keep-warm: restart stopped always-on fleets (after a
                // process restart or crash nothing is tracked). The
                // interval's first tick fires at once, so a rebooted host
                // warms its Pro apps on boot, then every tick after.
                match this.warm_always_on(&pool).await {
                    Ok(woken) if !woken.is_empty() => {
                        tracing::info!(slugs = ?woken, "keep-warm wake complete");
                    }
                    Err(err) => {
                        tracing::warn!(error = %format!("{err:#}"), "keep-warm wake failed");
                    }
                    Ok(_) => {}
                }
                // Batched and off the hot path: every tracked fleet's new
                // ring lines persist to `app_log` and group into
                // `error_issue` once a tick.
                if let Err(err) = crate::observe::logs::flush_all(&pool, &this).await {
                    tracing::warn!(error = %format!("{err:#}"), "log flush failed");
                }
                // A crashed rename's leftover bucket prefixes: the drain is
                // idempotent (one indexed SELECT when empty) and holds each
                // slug's wake lock, so it can never race a live rename's
                // copy. Runs every tick — not just the daily prune — so an
                // old slug unblocks within minutes, not a day.
                if let Err(err) =
                    crate::apps::rename::drain_prefix_cleanup(&pool, &store, Some(&this)).await
                {
                    tracing::warn!(error = %format!("{err:#}"), "prefix cleanup drain failed");
                }
                let due = last_prune.is_none_or(|t| t.elapsed() >= Duration::from_secs(86_400));
                if due {
                    prune_retention(&pool, &store).await;
                    // Inactive apps (spec §7.8): reminders, expiry, restores
                    // and purges. Idempotent, so a missed day only delays
                    // mail and the deadline actions; the next tick catches up.
                    match crate::inactivity::run_sweep(&pool, &inactive, crate::apps::now_ms())
                        .await
                    {
                        Ok(counts) => {
                            if counts != crate::inactivity::Counts::default() {
                                tracing::info!(
                                    reminded = counts.reminded,
                                    expired = counts.expired,
                                    restored = counts.restored,
                                    purged = counts.purged,
                                    "inactivity sweep",
                                );
                            }
                        }
                        Err(err) => {
                            tracing::warn!(error = %format!("{err:#}"), "inactivity sweep failed")
                        }
                    }
                    last_prune = Some(std::time::Instant::now());
                }
            }
        });
    }

    /// Stop every fleet concurrently.
    pub async fn stop_all(&self) {
        let fleets: Vec<RunningFleet> = self
            .inner
            .fleets
            .lock()
            .await
            .drain()
            .map(|(_, live)| live.fleet)
            .collect();
        futures::future::join_all(fleets.into_iter().map(|mut fleet| async move {
            fleet.stop(StopGrace::NORMAL).await;
        }))
        .await;
    }

    /// The running fleet's log ring, if it is tracked.
    pub fn logs(&self, slug: &str) -> Option<SharedLogRing> {
        self.inner
            .fleets
            .try_lock()
            .ok()?
            .get(slug)
            .map(|live| live.fleet.logs.clone())
    }
}

/// Daily retention prunes: the agent action log per plan (spec §3), the
/// rate-limit ledger, expired email codes and web sessions, expired OAuth
/// leftovers and stale DCR clients, deploys past the last 50 per app
/// (spec §13), and code exports older than 24 h (spec §7.5). Each prune
/// logs its own failure and the rest still run, so one bad table never
/// blocks the others.
async fn prune_retention(pool: &SqlitePool, store: &crate::store::Store) {
    let now = crate::auth::now_ms();
    if let Err(err) = crate::mcp::actions::prune(pool, now).await {
        tracing::warn!(error = %format!("{err:#}"), "agent_action prune failed");
    }
    if let Err(err) =
        crate::auth::limits::prune(pool, now - crate::auth::limits::RETENTION_MS).await
    {
        tracing::warn!(error = %format!("{err:#}"), "rate_limit_events prune failed");
    }
    if let Err(err) = crate::auth::prune_expired(pool, now).await {
        tracing::warn!(error = %format!("{err:#}"), "email_code/web_session prune failed");
    }
    if let Err(err) = crate::auth::oauth::prune_expired(pool, now).await {
        tracing::warn!(error = %format!("{err:#}"), "oauth prune failed");
    }
    if let Err(err) = crate::deploy::prune_old(pool).await {
        tracing::warn!(error = %format!("{err:#}"), "deploy prune failed");
    }
    if let Err(err) = crate::observe::logs::prune_errors(pool, now).await {
        tracing::warn!(error = %format!("{err:#}"), "error_issue prune failed");
    }
    if let Err(err) = crate::git::export::prune_exports(pool, store, now).await {
        tracing::warn!(error = %format!("{err:#}"), "export prune failed");
    }
}

/// Whether the fleet should sleep: its newest activity
/// (`max(last_request_at, created_at)`) is strictly older than `idle_after`,
/// and the fleet itself is at least `uptime_guard` old. The uptime guard
/// keeps a just-started fleet alive while its first requests are still in
/// flight, when the activity row can still look stale. The guard stays on
/// the host value while the idle timeout follows the app owner's plan
/// (spec §3: Free 15 min, Pro the host default).
pub fn should_sleep(
    last_request_at: Option<i64>,
    created_at: i64,
    now_ms: i64,
    uptime: Duration,
    idle_after: Duration,
    uptime_guard: Duration,
) -> bool {
    if uptime < uptime_guard {
        return false;
    }
    let activity = last_request_at.unwrap_or(created_at).max(created_at);
    now_ms.saturating_sub(activity) as i128 > idle_after.as_millis() as i128
}

/// The excess past `max` keep-warm fleets that may still sleep: the
/// oldest-idle first. `idle` is `(slug, newest_activity_ms)`; the sort
/// breaks activity ties by slug so the victims are deterministic. Pure
/// for tests; the sweep feeds it the live protected set.
pub fn over_cap_victims(mut idle: Vec<(String, i64)>, max: usize) -> Vec<String> {
    idle.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let victims = idle.len().saturating_sub(max);
    idle.into_iter()
        .take(victims)
        .map(|(slug, _)| slug)
        .collect()
}

/// Whether the fleet should restart with freshly minted credentials: its
/// credentials' age has reached `refresh_after` (TTL minus a day in
/// production, so the restart lands before they expire). Unlike
/// [`should_sleep`], the bound itself refreshes: credentials exactly
/// `refresh_after` old restart this tick rather than the next.
pub fn should_refresh(credential_age: Duration, refresh_after: Duration) -> bool {
    credential_age >= refresh_after
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_decision_at_its_boundaries() {
        let after = Duration::from_secs(300);
        let old = Duration::from_secs(301);
        // Fresh activity stays awake.
        assert!(!should_sleep(Some(1_000), 0, 1_000, old, after, after));
        // Missing requests fall back to creation time.
        assert!(!should_sleep(None, 1_000, 1_000, old, after, after));
        assert!(should_sleep(None, 0, 301_000, old, after, after));
        // Exactly at the bound is still awake: strictly older sleeps.
        assert!(!should_sleep(Some(0), 0, 300_000, old, after, after));
        assert!(should_sleep(Some(0), 0, 300_001, old, after, after));
        // The newest of request and creation wins either way.
        assert!(!should_sleep(Some(500_000), 0, 500_000, old, after, after));
        assert!(!should_sleep(Some(0), 500_000, 500_000, old, after, after));
        assert!(should_sleep(Some(0), 500_000, 800_001, old, after, after));
        // A young fleet stays awake even with stale activity: its first
        // requests may still be in flight. The guard is independent of the
        // idle timeout: a short guard with a long timeout still sleeps
        // stale fleets.
        assert!(!should_sleep(
            Some(0),
            0,
            300_001,
            Duration::ZERO,
            after,
            after
        ));
        assert!(!should_sleep(
            Some(0),
            0,
            300_001,
            Duration::from_secs(299),
            after,
            after
        ));
        assert!(should_sleep(
            Some(0),
            0,
            300_001,
            Duration::from_secs(300),
            after,
            after
        ));
        assert!(should_sleep(
            Some(0),
            0,
            900_001,
            Duration::ZERO,
            Duration::from_secs(900),
            Duration::ZERO
        ));
    }

    #[test]
    fn refresh_by_credential_age_at_its_boundaries() {
        let after = Duration::from_secs(6 * 86400);
        // Young credentials keep serving, however old the process is: the
        // decision is credential age, not fleet uptime.
        assert!(!should_refresh(Duration::ZERO, after));
        assert!(!should_refresh(after - Duration::from_secs(1), after));
        // Exactly at the bound refreshes: no extra tick of expired creds.
        assert!(should_refresh(after, after));
        assert!(should_refresh(after + Duration::from_secs(1), after));
    }

    #[tokio::test]
    async fn wake_in_progress_peeks_without_creating_entries() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let manager = FleetManager::new(ManagerSettings {
            data_dir: dir.path().to_path_buf(),
            celld_bin: "/nonexistent/celld".to_string(),
            storage: FleetStorage {
                endpoint: "https://example.com".to_string(),
                region: "auto".to_string(),
                bucket: "outer-dev".to_string(),
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: "key".to_string(),
                    secret_access_key: "secret".to_string(),
                },
            },
            cgroups: None,
            memory_max_bytes: 0,
            cpu_max: "50000 100000".to_string(),
            sleep_after: Duration::from_secs(60),
            credential_refresh_after: Duration::from_secs(6 * 86400),
            wake_timeout: Duration::from_secs(2),
            wake_concurrency: 4,
            always_on_max: 10,
            preview_max: 20,
        });
        assert!(!manager.wake_in_progress("ghost"));
        // Peeking must not create a map entry either.
        assert!(!manager.wake_in_progress("ghost"));
        let key = manager.wake_lock("busy").await;
        assert!(!manager.wake_in_progress("busy"));
        let _held = key.lock().await;
        assert!(manager.wake_in_progress("busy"));
        drop(_held);
        assert!(!manager.wake_in_progress("busy"));
        pool.close().await;
    }

    /// A wake counts as activity: `ensure_awake` against an already-healthy
    /// port touches `last_request_at` without spawning celld. Uses a fake
    /// health endpoint, no fleet involved.
    #[tokio::test]
    async fn wake_marks_activity_without_spawning() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('a1', 'w@example.com', 0, 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        // A listener that answers 200 to anything: the manager's zero-timeout
        // probe treats it as an outlived healthy fleet and never spawns.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, last_activity_at, created_at)              VALUES ('app-w', 'wake', 'a1', 220001, 0, 0, 'unlisted', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Point the app at the fake healthy port.
        sqlx::query("UPDATE app SET port = ?1, internal_port = ?2 WHERE id = 'app-w'")
            .bind(port as i64)
            .bind(port as i64)
            .execute(&pool)
            .await
            .unwrap();
        let before = apps::now_ms();
        let manager = FleetManager::new(ManagerSettings {
            data_dir: dir.path().to_path_buf(),
            celld_bin: "/nonexistent/celld".to_string(),
            storage: FleetStorage {
                endpoint: "https://example.com".to_string(),
                region: "auto".to_string(),
                bucket: "outer-dev".to_string(),
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: "key".to_string(),
                    secret_access_key: "secret".to_string(),
                },
            },
            cgroups: None,
            memory_max_bytes: 0,
            cpu_max: "50000 100000".to_string(),
            sleep_after: Duration::from_secs(60),
            credential_refresh_after: Duration::from_secs(6 * 86400),
            wake_timeout: Duration::from_secs(2),
            wake_concurrency: 4,
            always_on_max: 10,
            preview_max: 20,
        });
        let app = apps::get_by_slug(&pool, "wake").await.unwrap().unwrap();
        manager.ensure_awake(&pool, &app).await.unwrap();
        let app = apps::get_by_slug(&pool, "wake").await.unwrap().unwrap();
        assert!(
            app.last_request_at.unwrap_or(0) >= before,
            "a wake must touch last_request_at"
        );
        assert!(app.asleep_since.is_none());

        // A request that loaded the app before a delete (which holds the same
        // wake lock) must not wake it afterwards: the row is gone.
        let stale = app.clone();
        apps::delete_row(&pool, &stale.id).await.unwrap();
        let err = manager.ensure_awake(&pool, &stale).await.unwrap_err();
        assert!(err.to_string().contains("no longer exists"), "{err}");
    }

    /// The sleep and credential-refresh stops share one helper that flushes
    /// the ring before the fleet (and its ring) dies: unflushed lines —
    /// including a grouped error — persist instead of being lost.
    #[tokio::test]
    async fn pre_stop_flush_persists_unflushed_lines() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let ring: crate::fleet::supervisor::SharedLogRing =
            std::sync::Arc::new(tokio::sync::Mutex::new(LogRing::new(64)));
        {
            let mut guard = ring.lock().await;
            guard.push(LogStream::Stdout, "serving on :8080".to_string());
            guard.push(
                LogStream::Stderr,
                "Error: refresh must not lose me".to_string(),
            );
        }
        flush_ring_before_stop(&pool, "app1", "slug1", &ring).await;
        let lines: Vec<String> =
            sqlx::query_scalar("SELECT line FROM app_log WHERE app_id = ?1 ORDER BY seq")
                .bind("app1")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(lines.len(), 2, "both unflushed lines persist");
        let issues = crate::observe::logs::issues(&pool, "app1", None)
            .await
            .unwrap();
        assert_eq!(
            issues.len(),
            1,
            "the error line groups an issue: {issues:?}"
        );
        assert!(issues[0].message.contains("must not lose me"));
        pool.close().await;
    }
    /// Preview fleets share the base app id: persisting their ring under it
    /// would advance the live watermark and surface preview errors as live
    /// issues. Preview output stays in the deploy log tail; the sweep drops
    /// it here instead.
    #[tokio::test]
    async fn pre_stop_flush_drops_preview_fleet_lines() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let ring: crate::fleet::supervisor::SharedLogRing =
            std::sync::Arc::new(tokio::sync::Mutex::new(LogRing::new(64)));
        {
            let mut guard = ring.lock().await;
            guard.push(LogStream::Stdout, "preview serving on :0".to_string());
            guard.push(
                LogStream::Stderr,
                "Error: preview must not pollute live logs".to_string(),
            );
        }
        flush_ring_before_stop(&pool, "app1", "slug1--preview", &ring).await;
        let lines: Vec<String> =
            sqlx::query_scalar("SELECT line FROM app_log WHERE app_id = ?1 ORDER BY seq")
                .bind("app1")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(lines.is_empty(), "preview lines are dropped: {lines:?}");
        assert!(
            crate::observe::logs::issues(&pool, "app1", None)
                .await
                .unwrap()
                .is_empty(),
            "preview errors never group into live issues"
        );
        pool.close().await;
    }

    /// Needs a real celld binary and an S3-compatible bucket (RustFS, with
    /// OUTER_TEST_S3_* set as in tests/store_live.rs): deploys a minimal
    /// worker, then starts the fleet for an app row and waits for its health
    /// gate.
    #[tokio::test]
    #[ignore = "needs celld + RustFS"]
    async fn wake_starts_a_real_fleet() {
        fn test_env(name: &str, fallback: &str) -> String {
            std::env::var(name).unwrap_or_else(|_| fallback.to_string())
        }
        let endpoint = test_env("OUTER_TEST_S3_ENDPOINT", "http://127.0.0.1:19000");
        let bucket = test_env("OUTER_TEST_S3_BUCKET", "outer-dev");
        let access_key = test_env("OUTER_TEST_S3_KEY", "outeraccess");
        let secret_key = test_env("OUTER_TEST_S3_SECRET", "outersecretoutersecret12");
        let celld = test_env("CELLD_BIN", "celld");
        // A minimal worker so the fleet has something to serve.
        let proj = tempfile::tempdir().unwrap();
        std::fs::write(
            proj.path().join("index.js"),
            "export default { fetch: () => new Response(\"real\") };\n",
        )
        .unwrap();
        std::fs::write(
            proj.path().join("wrangler.json"),
            "{\"name\": \"real\", \"main\": \"index.js\", \"compatibility_date\": \"2026-09-01\"}",
        )
        .unwrap();
        let deploy = tokio::process::Command::new(&celld)
            .arg("deploy")
            .arg(proj.path())
            .arg("--bucket")
            .arg(format!("s3://{bucket}/fleets/real"))
            .arg("--endpoint")
            .arg(&endpoint)
            .arg("--region")
            .arg("us-east-1")
            .env("AWS_ACCESS_KEY_ID", &access_key)
            .env("AWS_SECRET_ACCESS_KEY", &secret_key)
            .env("AWS_REGION", "us-east-1")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("S3_ENDPOINT", &endpoint)
            .env(
                "CELLD_ESBUILD",
                test_env("CELLD_ESBUILD", "/usr/local/bin/esbuild"),
            )
            .output()
            .await
            .expect("celld deploy runs");
        assert!(
            deploy.status.success(),
            "celld deploy failed: {}",
            String::from_utf8_lossy(&deploy.stderr)
        );
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
             VALUES ('app-real', 'real', 'a1', 210001, 32111, 32112, 'unlisted', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let manager = FleetManager::new(ManagerSettings {
            data_dir: dir.path().to_path_buf(),
            celld_bin: test_env("CELLD_BIN", "celld"),
            storage: FleetStorage {
                endpoint,
                region: "auto".to_string(),
                bucket,
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: access_key,
                    secret_access_key: secret_key,
                },
            },
            cgroups: None,
            memory_max_bytes: 256 * 1024 * 1024,
            cpu_max: "50000 100000".to_string(),
            sleep_after: Duration::from_secs(300),
            credential_refresh_after: Duration::from_secs(6 * 86400),
            wake_timeout: Duration::from_secs(60),
            wake_concurrency: 2,
            always_on_max: 10,
            preview_max: 20,
        });
        let app = apps::get_by_slug(&pool, "real").await.unwrap().unwrap();
        manager.ensure_awake(&pool, &app).await.unwrap();
        assert!(manager.logs("real").is_some());
        manager.stop_all().await;
    }

    /// Keep-warm rig: a fake celld (a script that sleeps) plus a fake
    /// healthy port, so `ensure_awake` tracks a live fleet with no real
    /// celld and no network.
    struct WarmRig {
        _dir: tempfile::TempDir,
        pool: SqlitePool,
        manager: FleetManager,
        listeners: Vec<tokio::task::JoinHandle<()>>,
    }

    impl WarmRig {
        async fn new(always_on_max: usize) -> Self {
            let dir = tempfile::tempdir().unwrap();
            Self::with_celld(
                dir,
                always_on_max,
                "#!/bin/sh\nexec sleep 120\n",
                Duration::from_secs(10),
            )
            .await
        }

        /// A rig whose fleet binary fails at once, logging each spawn's argv:
        /// every wake attempt fails, and the gate runs to `wake_timeout`, so
        /// the script always logs before the stop — tests can count attempts
        /// exactly. Returns the rig plus the spawn log path.
        async fn failing(always_on_max: usize) -> (Self, std::path::PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("spawns.log");
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 1\n",
                log.display()
            );
            (
                Self::with_celld(dir, always_on_max, &script, Duration::from_secs(2)).await,
                log,
            )
        }

        async fn with_celld(
            dir: tempfile::TempDir,
            always_on_max: usize,
            celld_script: &str,
            wake_timeout: Duration,
        ) -> Self {
            let pool = crate::db::open(&dir.path().join("outer.sqlite"))
                .await
                .unwrap();
            // A fleet binary that stays alive and ignores its argv: the
            // health gate is answered by the fake listener below.
            let celld = dir.path().join("celld");
            std::fs::write(&celld, celld_script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&celld, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let manager = FleetManager::new(ManagerSettings {
                data_dir: dir.path().to_path_buf(),
                celld_bin: celld.to_string_lossy().into_owned(),
                storage: FleetStorage {
                    endpoint: "https://example.com".to_string(),
                    region: "auto".to_string(),
                    bucket: "outer-dev".to_string(),
                    credentials: crate::store::credentials::CredentialSource::Static {
                        access_key_id: "key".to_string(),
                        secret_access_key: "secret".to_string(),
                    },
                },
                cgroups: None,
                memory_max_bytes: 0,
                cpu_max: "50000 100000".to_string(),
                // Zero: every stale app is sleepable, so an awake fleet at
                // the end of the test proves the keep-warm skip, not the
                // uptime guard.
                sleep_after: Duration::ZERO,
                credential_refresh_after: Duration::from_secs(6 * 86400),
                wake_timeout,
                wake_concurrency: 4,
                always_on_max,
                preview_max: 20,
            });
            Self {
                _dir: dir,
                pool,
                manager,
                listeners: Vec::new(),
            }
        }

        /// A listener that answers 200 to anything: the manager's health
        /// gate treats the port as a healthy fleet. Used for warm-up tests,
        /// where adoption (no spawn) is exactly the path under test.
        async fn healthy_port(&mut self) -> u16 {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            self.listeners.push(tokio::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        let _ = sock.read(&mut buf).await;
                        let _ = sock
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                            )
                            .await;
                    });
                }
            }));
            port
        }

        /// A port held open for the test, answering 503 to every probe: the
        /// health gate treats it as a dead fleet. The socket stays bound for
        /// the whole test, so no parallel test can claim the port mid-test
        /// and answer 200 — which the zero-timeout probe would mistake for
        /// an already-healthy fleet, adopting it without a spawn and breaking
        /// the attempt count.
        async fn dead_port(&mut self) -> u16 {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            self.listeners.push(tokio::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        let _ = sock.read(&mut buf).await;
                        let _ = sock
                            .write_all(
                                b"HTTP/1.1 503 Busy\r\nContent-Length: 5\r\nConnection: close\r\n\r\nbusy!",
                            )
                            .await;
                    });
                }
            }));
            port
        }

        async fn seed_user(&self, id: &str, email: &str, plan: &str) {
            sqlx::query(
                "INSERT INTO user (id, email, plan, lastActiveAt, createdAt, updatedAt) \
                 VALUES (?1, ?2, ?3, 0, 0, 0)",
            )
            .bind(id)
            .bind(email)
            .bind(plan)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        #[allow(clippy::too_many_arguments)]
        async fn seed_app(
            &self,
            id: &str,
            slug: &str,
            owner: &str,
            uid: i64,
            port: i64,
            always_on: bool,
            last_request_at: Option<i64>,
            deploy: Option<&str>,
            asleep: Option<i64>,
        ) {
            sqlx::query(
                "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, \
                 always_on, last_request_at, last_deploy_sha, asleep_since, \
                 last_activity_at, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, 'unlisted', ?6, ?7, ?8, ?9, 0, 0)",
            )
            .bind(id)
            .bind(slug)
            .bind(owner)
            .bind(uid)
            .bind(port)
            .bind(i64::from(always_on))
            .bind(last_request_at)
            .bind(deploy)
            .bind(asleep)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// A bound gate listener for an app whose health gate the test
        /// answers itself (see `wake_then_idle`): holds an OS-assigned port
        /// from seeding to the wake, so no parallel test can claim the
        /// number mid-test and break the wake (or answer 200 on a stolen
        /// number and get the wake adopted without a spawn).
        async fn gated_listener() -> (tokio::net::TcpListener, u16) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            (listener, port)
        }

        /// Start and health-gate the app's fleet, then backdate its
        /// activity so the sweep sees it as idle. Serves `listener`, bound
        /// up front by [`Self::gated_listener`] and held since seeding:
        /// rebinding a released port number here leaves a window where a
        /// parallel test claims the number and the bind fails. The port
        /// answers 503 until the wake is in flight: the zero-timeout probe
        /// can never mistake it for an already-healthy fleet, so
        /// `ensure_awake` always spawns (and tracks) the fake celld.
        async fn wake_then_idle(
            &self,
            listener: tokio::net::TcpListener,
            slug: &str,
            last_request_at: i64,
        ) {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::Arc;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let ready = Arc::new(AtomicBool::new(false));
            let gate = ready.clone();
            let serving = tokio::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let gate = gate.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        let _ = sock.read(&mut buf).await;
                        let body = if gate.load(Ordering::SeqCst) {
                            &b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                                [..]
                        } else {
                            &b"HTTP/1.1 503 Busy\r\nContent-Length: 5\r\nConnection: close\r\n\r\nbusy!"[..]
                        };
                        let _ = sock.write_all(body).await;
                    });
                }
            });
            let app = apps::get_by_slug(&self.pool, slug).await.unwrap().unwrap();
            let manager = self.manager.clone();
            let pool = self.pool.clone();
            let wake = tokio::spawn(async move { manager.ensure_awake(&pool, &app).await });
            // Let the spawn land (the fake celld starts instantly), then
            // answer the health gate.
            tokio::time::sleep(Duration::from_millis(500)).await;
            ready.store(true, Ordering::SeqCst);
            wake.await.unwrap().unwrap();
            serving.abort();
            sqlx::query("UPDATE app SET last_request_at = ?1 WHERE slug = ?2")
                .bind(last_request_at)
                .bind(slug)
                .execute(&self.pool)
                .await
                .unwrap();
        }
    }

    #[test]
    fn over_cap_victims_picks_the_oldest_idle() {
        let s = |slug: &str, at: i64| (slug.to_string(), at);
        assert!(over_cap_victims(vec![], 2).is_empty());
        assert!(over_cap_victims(vec![s("b", 5), s("a", 3)], 2).is_empty());
        assert_eq!(over_cap_victims(vec![s("b", 5), s("a", 3)], 1), vec!["a"]);
        // Activity ties break by slug, so the victims are deterministic.
        assert_eq!(over_cap_victims(vec![s("b", 5), s("a", 5)], 1), vec!["a"]);
        assert_eq!(
            over_cap_victims(vec![s("a", 1), s("b", 2), s("c", 3)], 1),
            vec!["a", "b"]
        );
        // Zero disables the protection: everything may sleep.
        assert_eq!(over_cap_victims(vec![s("a", 1)], 0), vec!["a"]);
    }

    #[tokio::test]
    async fn sleep_sweep_skips_protected_always_on() {
        let rig = WarmRig::new(10).await;
        rig.seed_user("a1", "a@example.com", "pro").await;
        // Held from seeding to the wake: no parallel test can claim the
        // number mid-test.
        let (gate, port) = WarmRig::gated_listener().await;
        rig.seed_app(
            "app1",
            "warm",
            "a1",
            220101,
            port as i64,
            true,
            None,
            Some("sha"),
            None,
        )
        .await;
        rig.wake_then_idle(gate, "warm", 0).await;
        let slept = rig.manager.sleep_idle(&rig.pool).await.unwrap();
        assert!(slept.is_empty(), "protected fleet sleeps: {slept:?}");
        assert!(rig.manager.logs("warm").is_some());
        let asleep: Option<i64> =
            sqlx::query_scalar("SELECT asleep_since FROM app WHERE slug = 'warm'")
                .fetch_one(&rig.pool)
                .await
                .unwrap();
        assert!(asleep.is_none());
        rig.manager.stop_all().await;
    }

    #[tokio::test]
    async fn sleep_sweep_sleeps_free_always_on() {
        let rig = WarmRig::new(10).await;
        rig.seed_user("a1", "a@example.com", "free").await;
        // Held from seeding to the wake: no parallel test can claim the
        // number mid-test.
        let (gate, port) = WarmRig::gated_listener().await;
        rig.seed_app(
            "app1",
            "cold",
            "a1",
            220101,
            port as i64,
            true,
            None,
            Some("sha"),
            None,
        )
        .await;
        // The sweep ignores the flag on Free: the fleet sleeps normally.
        rig.wake_then_idle(gate, "cold", 0).await;
        let slept = rig.manager.sleep_idle(&rig.pool).await.unwrap();
        assert_eq!(slept, vec!["cold"]);
        assert!(rig.manager.logs("cold").is_none());
        let asleep: Option<i64> =
            sqlx::query_scalar("SELECT asleep_since FROM app WHERE slug = 'cold'")
                .fetch_one(&rig.pool)
                .await
                .unwrap();
        assert!(asleep.is_some());
        rig.manager.stop_all().await;
    }

    #[tokio::test]
    async fn sleep_sweep_cap_sleeps_the_oldest_idle() {
        let rig = WarmRig::new(1).await;
        rig.seed_user("a1", "a@example.com", "pro").await;
        // Held from seeding to the wakes: no parallel test can claim either
        // number mid-test, and the two seeds can never share one.
        let (old_gate, old_port) = WarmRig::gated_listener().await;
        let (new_gate, new_port) = WarmRig::gated_listener().await;
        rig.seed_app(
            "app1",
            "old",
            "a1",
            220101,
            old_port as i64,
            true,
            None,
            Some("sha"),
            None,
        )
        .await;
        rig.seed_app(
            "app2",
            "new",
            "a1",
            220102,
            new_port as i64,
            true,
            None,
            Some("sha"),
            None,
        )
        .await;
        rig.wake_then_idle(old_gate, "old", 0).await;
        // Stale too (so it would sleep unprotected), but newer than `old`.
        rig.wake_then_idle(new_gate, "new", 1_000).await;
        let slept = rig.manager.sleep_idle(&rig.pool).await.unwrap();
        assert_eq!(slept, vec!["old"], "cap victim is the oldest idle");
        assert!(rig.manager.logs("new").is_some());
        assert!(rig.manager.logs("old").is_none());
        rig.manager.stop_all().await;
    }

    #[tokio::test]
    async fn warm_always_on_restarts_stopped_pro_fleets() {
        let mut rig = WarmRig::new(10).await;
        rig.seed_user("a1", "a@example.com", "pro").await;
        rig.seed_user("b1", "b@example.com", "free").await;
        let warm_port = rig.healthy_port().await as i64;
        let cold_port = rig.healthy_port().await as i64;
        let now = apps::now_ms();
        rig.seed_app(
            "app1",
            "warm",
            "a1",
            220101,
            warm_port,
            true,
            Some(now),
            Some("sha"),
            Some(now),
        )
        .await;
        // Free flag is stored but never warmed.
        rig.seed_app(
            "app2",
            "cold",
            "b1",
            220102,
            cold_port,
            true,
            Some(now),
            Some("sha"),
            Some(now),
        )
        .await;
        let woken = rig.manager.warm_always_on(&rig.pool).await.unwrap();
        assert_eq!(woken, vec!["warm"]);
        for slug in ["warm", "cold"] {
            let asleep: Option<i64> =
                sqlx::query_scalar("SELECT asleep_since FROM app WHERE slug = ?1")
                    .bind(slug)
                    .fetch_one(&rig.pool)
                    .await
                    .unwrap();
            assert_eq!(asleep.is_none(), slug == "warm", "{slug}");
        }
        rig.manager.stop_all().await;
    }

    #[tokio::test]
    async fn warm_skips_undeployed_apps_and_stops_at_the_cap() {
        let mut rig = WarmRig::new(1).await;
        rig.seed_user("a1", "a@example.com", "pro").await;
        let p1 = rig.healthy_port().await as i64;
        let p2 = rig.healthy_port().await as i64;
        let p3 = rig.healthy_port().await as i64;
        let now = apps::now_ms();
        // Never deployed: nothing to serve yet, so no wake (and no
        // per-minute health-gate failure loop).
        rig.seed_app(
            "app1",
            "nodeploy",
            "a1",
            220101,
            p1,
            true,
            Some(now),
            None,
            Some(now),
        )
        .await;
        // Most-recently-active first: `first` wins the single cap slot.
        rig.seed_app(
            "app2",
            "first",
            "a1",
            220102,
            p2,
            true,
            Some(now),
            Some("sha"),
            Some(now),
        )
        .await;
        rig.seed_app(
            "app3",
            "second",
            "a1",
            220103,
            p3,
            true,
            Some(now - 60_000),
            Some("sha"),
            Some(now),
        )
        .await;
        let woken = rig.manager.warm_always_on(&rig.pool).await.unwrap();
        assert_eq!(woken, vec!["first"]);
        let asleep: Option<i64> =
            sqlx::query_scalar("SELECT asleep_since FROM app WHERE slug = 'second'")
                .fetch_one(&rig.pool)
                .await
                .unwrap();
        assert!(asleep.is_some(), "over-cap app stays asleep");
        rig.manager.stop_all().await;
    }

    /// Crashing fleets must not stall the sweep: failed wakes stop at
    /// `always_on_max` per tick, so a fleet that never starts can't eat
    /// the tick's wake budget (each attempt would otherwise run to
    /// `wake_timeout`). The rest wait for the next tick.
    #[tokio::test]
    async fn warm_always_on_stops_after_max_failures() {
        let (mut rig, log) = WarmRig::failing(2).await;
        rig.seed_user("a1", "a@example.com", "pro").await;
        // Distinct activity so the most-recent-first order is total, not a
        // three-way `created_at` tie; `three` is deterministically last. The
        // ports stay bound (503) for the test, so no parallel test can claim
        // one mid-test and answer 200 — which the probe would mistake for an
        // already-healthy fleet.
        let now = apps::now_ms();
        for (i, slug) in ["one", "two", "three"].iter().enumerate() {
            let port = rig.dead_port().await as i64;
            rig.seed_app(
                &format!("app{i}"),
                slug,
                "a1",
                220101 + i as i64,
                port,
                true,
                Some(now - i as i64 * 60_000),
                Some("sha"),
                None,
            )
            .await;
        }
        let woken = rig.manager.warm_always_on(&rig.pool).await.unwrap();
        assert!(woken.is_empty(), "nothing can wake: {woken:?}");
        let attempts = std::fs::read_to_string(&log)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        assert_eq!(
            attempts, 2,
            "the third eligible app waits for the next tick"
        );
        rig.manager.stop_all().await;
    }
}
