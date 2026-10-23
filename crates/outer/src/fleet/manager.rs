//! One celld fleet per app, started on demand and slept when idle.
//!
//! `FleetManager` keeps the running fleets: `ensure_awake` starts the fleet
//! for an app (coalesced per slug, bounded across slugs), `sleep_idle` stops
//! fleets whose app has gone quiet, and the proxy records activity through
//! `crate::apps::touch_request`.

use std::collections::HashMap;
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
            wake_permits: tokio::sync::Semaphore::new(settings.wake_concurrency.max(1)),
            fleets: tokio::sync::Mutex::new(HashMap::new()),
            wakes: tokio::sync::Mutex::new(HashMap::new()),
        };
        Self {
            inner: Arc::new(inner),
        }
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
        let spawn_start = std::time::Instant::now();
        let fleet = supervisor::start(&spec, &opts).await?;
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
            .spawn_and_gate(app, false)
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
    pub async fn sleep_idle(&self, pool: &SqlitePool) -> anyhow::Result<Vec<String>> {
        let now = apps::now_ms();
        let slugs: Vec<String> = self.inner.fleets.lock().await.keys().cloned().collect();
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
            if should_sleep(
                app.last_request_at,
                app.created_at,
                now,
                live.started.elapsed(),
                self.inner.sleep_after,
            ) {
                let Some(live) = fleets.remove(&slug) else {
                    continue;
                };
                let mut fleet = live.fleet;
                drop(fleets);
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
            let mut fleet = live.fleet;
            drop(fleets);
            fleet.stop(StopGrace::NORMAL).await;
            // The restart forces a fresh mint: restarting on the cached grant
            // would restart on the same near-expiry credentials.
            match self.spawn_and_gate(&app, true).await {
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

    /// A tokio interval loop that sleeps idle fleets. Failures log and the
    /// loop continues on the next tick.
    pub fn spawn_sleep_sweep(&self, pool: SqlitePool, every: Duration) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(every);
            loop {
                interval.tick().await;
                if let Err(err) = this.sleep_idle(&pool).await {
                    tracing::warn!(error = %err, "sleep sweep failed");
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
        sqlx::query("INSERT INTO account (id, email, last_active_at, created_at) VALUES ('a1', 'w@example.com', 0, 0)")
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
        sqlx::query("INSERT INTO account (id, email, last_active_at, created_at) VALUES ('a1', 'o@example.com', 0, 0)")
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
        });
        let app = apps::get_by_slug(&pool, "real").await.unwrap().unwrap();
        manager.ensure_awake(&pool, &app).await.unwrap();
        assert!(manager.logs("real").is_some());
        manager.stop_all().await;
    }
}
