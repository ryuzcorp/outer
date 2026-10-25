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
    wake_permits: tokio::sync::Semaphore,
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
/// and credential-refresh stops). The sweep flushes every tick, so this is
/// normally a handful of lines. A flush failure only warns: the fleet
/// still stops. Both callers hold the slug's wake lock (the sweep takes it
/// across evaluate-and-stop; `ensure_awake` holds it from its first
/// liveness check to its activity write), so a wake-spawned ring cannot be
/// orphaned while this snapshot flushes.
async fn flush_ring_before_stop(
    pool: &SqlitePool,
    app_id: &str,
    slug: &str,
    logs: &crate::fleet::supervisor::SharedLogRing,
) {
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
            wake_permits: tokio::sync::Semaphore::new(settings.wake_concurrency.max(1)),
            fleets: tokio::sync::Mutex::new(HashMap::new()),
            wakes: tokio::sync::Mutex::new(HashMap::new()),
        };
        Self {
            inner: Arc::new(inner),
        }
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
        if Self::alive(&self.inner.fleets, &app.slug).await {
            self.mark_wake_activity(pool, app).await?;
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
            self.mark_wake_activity(pool, app).await?;
            apps::set_asleep(pool, &app.id, None).await?;
            return Ok(());
        }
        let report = self
            .spawn_and_gate(pool, app, false)
            .await
            .map_err(|err| anyhow::anyhow!("failed wake for fleet {}: {err:#}", app.slug))?;
        self.mark_wake_activity(pool, app).await?;
        apps::set_asleep(pool, &app.id, None).await?;
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
    /// would sleep a fleet whose first requests are still in flight.
    async fn mark_wake_activity(&self, pool: &SqlitePool, app: &App) -> anyhow::Result<()> {
        apps::touch_request(pool, &app.id, apps::now_ms()).await
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
    /// at `always_on_max` awake, and bounded across slugs by the wake
    /// semaphore inside [`Self::ensure_awake`]. Failures warn and the next
    /// tick retries. Apps that never deployed are skipped: there is nothing
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
        for (id, slug) in rows {
            if Self::alive(&self.inner.fleets, &slug).await {
                continue;
            }
            if awake >= max {
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
                Err(err) => tracing::warn!(
                    slug = %slug,
                    error = %format!("{err:#}"),
                    "keep-warm wake failed; next tick retries"
                ),
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
            let app = match apps::get_by_slug(pool, &slug).await? {
                Some(app) => app,
                // The app row is gone: its fleet serves nothing, so stop it
                // without marking sleep.
                None => {
                    tracing::warn!(slug = %slug, "app row gone; stopping its orphan fleet");
                    let mut fleets = self.inner.fleets.lock().await;
                    if let Some(live) = fleets.remove(&slug) {
                        let mut fleet = live.fleet;
                        drop(fleets);
                        fleet.stop(StopGrace::NORMAL).await;
                    }
                    continue;
                }
            };
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
            // freedom from idleness, not from expiring credentials.
            if !exempt.contains(&slug)
                && should_sleep(
                    app.last_request_at,
                    app.created_at,
                    now,
                    live.started.elapsed(),
                    self.inner.sleep_after,
                )
            {
                let Some(live) = fleets.remove(&slug) else {
                    continue;
                };
                let mut fleet = live.fleet;
                drop(fleets);
                flush_ring_before_stop(pool, &app.id, &slug, &fleet.logs).await;
                fleet.stop(StopGrace::NORMAL).await;
                apps::set_asleep(pool, &app.id, Some(now)).await?;
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
            flush_ring_before_stop(pool, &app.id, &slug, &fleet.logs).await;
            fleet.stop(StopGrace::NORMAL).await;
            // The restart forces a fresh mint: restarting on the cached grant
            // would restart on the same near-expiry credentials.
            match self.spawn_and_gate(pool, &app, true).await {
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

    /// A tokio interval loop that sleeps idle fleets and flushes fleet logs
    /// to SQLite. Failures log and the loop continues on the next tick.
    /// Retention prunes run at most once a day (spec §3 log retention, §13
    /// deploys and errors, §17 TTLs); the timestamp lives in this task, not
    /// in global state.
    pub fn spawn_sleep_sweep(&self, pool: SqlitePool, store: crate::store::Store, every: Duration) {
        let this = self.clone();
        let store = store.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(every);
            let mut last_prune: Option<std::time::Instant> = None;
            loop {
                interval.tick().await;
                if let Err(err) = this.sleep_idle(&pool).await {
                    tracing::warn!(error = %err, "sleep sweep failed");
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
/// leftovers and stale DCR clients, releasable slug quarantines (spec §17),
/// deploys past the last 50 per app (spec §13), and code exports older
/// than 24 h (spec §7.5). Each prune logs its own failure and the rest
/// still run, so one bad table never blocks the others.
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
    if let Err(err) = crate::apps::prune_quarantine(pool, now).await {
        tracing::warn!(error = %format!("{err:#}"), "slug_quarantine prune failed");
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
/// (`max(last_request_at, created_at)`) is strictly older than `sleep_after`,
/// and the fleet itself is at least that old. The uptime guard keeps a
/// just-started fleet alive while its first requests are still in flight,
/// when the activity row can still look stale.
pub fn should_sleep(
    last_request_at: Option<i64>,
    created_at: i64,
    now_ms: i64,
    uptime: Duration,
    sleep_after: Duration,
) -> bool {
    if uptime < sleep_after {
        return false;
    }
    let activity = last_request_at.unwrap_or(created_at).max(created_at);
    now_ms.saturating_sub(activity) as i128 > sleep_after.as_millis() as i128
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
        assert!(!should_sleep(Some(1_000), 0, 1_000, old, after));
        // Missing requests fall back to creation time.
        assert!(!should_sleep(None, 1_000, 1_000, old, after));
        assert!(should_sleep(None, 0, 301_000, old, after));
        // Exactly at the bound is still awake: strictly older sleeps.
        assert!(!should_sleep(Some(0), 0, 300_000, old, after));
        assert!(should_sleep(Some(0), 0, 300_001, old, after));
        // The newest of request and creation wins either way.
        assert!(!should_sleep(Some(500_000), 0, 500_000, old, after));
        assert!(!should_sleep(Some(0), 500_000, 500_000, old, after));
        assert!(should_sleep(Some(0), 500_000, 800_001, old, after));
        // A young fleet stays awake even with stale activity: its first
        // requests may still be in flight.
        assert!(!should_sleep(Some(0), 0, 300_001, Duration::ZERO, after));
        assert!(!should_sleep(
            Some(0),
            0,
            300_001,
            Duration::from_secs(299),
            after
        ));
        assert!(should_sleep(
            Some(0),
            0,
            300_001,
            Duration::from_secs(300),
            after
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
            let pool = crate::db::open(&dir.path().join("outer.sqlite"))
                .await
                .unwrap();
            // A fleet binary that stays alive and ignores its argv: the
            // health gate is answered by the fake listener below.
            let celld = dir.path().join("celld");
            std::fs::write(&celld, "#!/bin/sh\nexec sleep 120\n").unwrap();
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
                wake_timeout: Duration::from_secs(10),
                wake_concurrency: 4,
                always_on_max,
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

        /// An unused port number, for seeding an app whose health gate the
        /// test answers itself (see `wake_then_idle`).
        async fn free_port() -> u16 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
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

        /// Start and health-gate the app's fleet, then backdate its
        /// activity so the sweep sees it as idle. The port answers 503
        /// until the wake is in flight: the zero-timeout probe can never
        /// mistake it for an already-healthy fleet, so `ensure_awake`
        /// always spawns (and tracks) the fake celld.
        async fn wake_then_idle(&self, slug: &str, last_request_at: i64) {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::Arc;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let port: i64 = sqlx::query_scalar("SELECT port FROM app WHERE slug = ?1")
                .bind(slug)
                .fetch_one(&self.pool)
                .await
                .unwrap();
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port as u16))
                .await
                .unwrap();
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
        let port = WarmRig::free_port().await as i64;
        rig.seed_app(
            "app1",
            "warm",
            "a1",
            220101,
            port,
            true,
            None,
            Some("sha"),
            None,
        )
        .await;
        rig.wake_then_idle("warm", 0).await;
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
        let port = WarmRig::free_port().await as i64;
        rig.seed_app(
            "app1",
            "cold",
            "a1",
            220101,
            port,
            true,
            None,
            Some("sha"),
            None,
        )
        .await;
        // The sweep ignores the flag on Free: the fleet sleeps normally.
        rig.wake_then_idle("cold", 0).await;
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
        let old_port = WarmRig::free_port().await as i64;
        let new_port = WarmRig::free_port().await as i64;
        rig.seed_app(
            "app1",
            "old",
            "a1",
            220101,
            old_port,
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
            new_port,
            true,
            None,
            Some("sha"),
            None,
        )
        .await;
        rig.wake_then_idle("old", 0).await;
        // Stale too (so it would sleep unprotected), but newer than `old`.
        rig.wake_then_idle("new", 1_000).await;
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
}
