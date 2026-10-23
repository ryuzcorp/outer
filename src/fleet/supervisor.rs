// Derived from noite apps/runner/src/host/supervisor.rs @ 58a977a (Apache-2.0)
// and host/logs.rs @ 58a977a (Apache-2.0).
// Outer change: one celld fleet per app, no environment fleets, no control
// fleet, no reconcile; logs are a per-app bounded ring with sequence numbers
// and timestamps; no OTel/telemetry env (outer reads logs from stdout).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

/// One celld fleet per app.
#[derive(Debug, Clone)]
pub struct FleetSpec {
    pub slug: String,
    pub uid: u32,
    pub gid: u32,
    pub public_port: u16,
    pub internal_port: u16,
    /// `s3://<bucket>/fleets/<slug>`.
    pub bucket_url: String,
    pub s3_endpoint: String,
    pub s3_region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub cgroup_leaf: PathBuf,
    pub extra_env: Vec<(String, String)>,
}

/// Tunables for the celld invocation that are not per-app identity.
#[derive(Debug, Clone)]
pub struct FleetOpts {
    pub celld_bin: String,
    pub state_dir: PathBuf,
    pub shutdown_total_ms: u64,
    pub idle_evict_secs: u64,
    pub deploy_poll_secs: u64,
    pub asset_cache_mb: u64,
}

/// Pure construction of the celld invocation: (program, argv, env). The
/// command line and `CELLD_*` env mirror Noite's tenant fleets: listeners,
/// `CELLD_DURABILITY=bucket`, the shutdown budget, and the asset-cache and
/// deploy-poll bounds. The S3 creds ride the same `AWS_*` env Noite passes,
/// plus `AWS_SESSION_TOKEN` when the minted credentials carry one. Reserved
/// names in `extra_env` are dropped (logged), never injected.
pub fn fleet_command(
    spec: &FleetSpec,
    opts: &FleetOpts,
) -> (String, Vec<String>, Vec<(String, String)>) {
    let internal = format!("127.0.0.1:{}", spec.internal_port);
    let args = vec![
        "--bucket".to_string(),
        spec.bucket_url.clone(),
        "--endpoint".to_string(),
        spec.s3_endpoint.clone(),
        "--region".to_string(),
        spec.s3_region.clone(),
        "--listen".to_string(),
        format!("0.0.0.0:{}", spec.public_port),
        // Advertise on loopback: a fleet is a single node, and outer already
        // reaches it at 127.0.0.1:{internal} (reload). A hostname advertise
        // would not resolve and breaks celld node discovery.
        "--internal-listen".to_string(),
        internal.clone(),
        "--advertise".to_string(),
        internal,
    ];
    let state_dir = opts.state_dir.to_string_lossy().into_owned();
    let mut env = vec![
        ("AWS_ACCESS_KEY_ID".to_string(), spec.access_key_id.clone()),
        (
            "AWS_SECRET_ACCESS_KEY".to_string(),
            spec.secret_access_key.clone(),
        ),
        ("AWS_REGION".to_string(), spec.s3_region.clone()),
        ("AWS_EC2_METADATA_DISABLED".to_string(), "true".to_string()),
        ("S3_ENDPOINT".to_string(), spec.s3_endpoint.clone()),
        ("CELLD_WATCH".to_string(), state_dir.clone()),
        ("CELLD_DURABILITY".to_string(), "bucket".to_string()),
        // Inside outer's stop budget, so celld seals its node log before
        // outer's own SIGKILL fallback.
        (
            "CELLD_SHUTDOWN_TOTAL_MS".to_string(),
            opts.shutdown_total_ms.to_string(),
        ),
        // Bounds: idle eviction returns memory when an app goes quiet.
        (
            "CELLD_IDLE_EVICT_S".to_string(),
            opts.idle_evict_secs.to_string(),
        ),
        // Outer POSTs /reload after every deploy, so the fleet's own poll
        // only covers a missed reload.
        (
            "CELLD_DEPLOY_POLL_S".to_string(),
            opts.deploy_poll_secs.to_string(),
        ),
        // Bound the per-fleet on-disk asset cache.
        (
            "CELLD_ASSET_CACHE_BYTES".to_string(),
            (opts.asset_cache_mb * 1024 * 1024).to_string(),
        ),
        // In the fleet's own state dir: celld's default is one directory for
        // every node, created 0700 by whichever starts first, so a fleet
        // serving as its own uid crash-looped on EACCES there.
        (
            "CELLD_ASSET_CACHE_DIR".to_string(),
            format!("{state_dir}/asset-cache"),
        ),
        ("CELLD_TRUST_FORWARDED_HEADERS".to_string(), "1".to_string()),
    ];
    if let Some(token) = &spec.session_token {
        env.push(("AWS_SESSION_TOKEN".to_string(), token.clone()));
    }
    for (key, value) in &spec.extra_env {
        if super::sandbox::is_reserved_env_name(key) {
            tracing::warn!(slug = %spec.slug, name = %key, "fleet env reserved; skipping");
            continue;
        }
        env.push((key.clone(), value.clone()));
    }
    (opts.celld_bin.clone(), args, env)
}

/// Which side a captured line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// One captured line: a sequence number (per ring, from 0), capture time,
/// stream, and the line itself.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub seq: u64,
    pub ts: i64,
    pub stream: LogStream,
    pub line: String,
}

/// Per-app bounded log ring. Oldest lines fall off the front; every entry
/// keeps a stable sequence number so readers can poll with `since`.
#[derive(Debug)]
pub struct LogRing {
    cap: usize,
    next: u64,
    entries: VecDeque<LogEntry>,
}

impl LogRing {
    /// Default bound: the last 2000 lines per app.
    pub const DEFAULT_CAP: usize = 2000;

    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            next: 0,
            entries: VecDeque::new(),
        }
    }

    pub fn push(&mut self, stream: LogStream, line: String) {
        if self.entries.len() >= self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back(LogEntry {
            seq: self.next,
            ts: chrono::Utc::now().timestamp(),
            stream,
            line,
        });
        self.next += 1;
    }

    /// Most recent `n` lines, chronological.
    pub fn tail(&self, n: usize) -> Vec<LogEntry> {
        let skip = self.entries.len().saturating_sub(n);
        self.entries.iter().skip(skip).cloned().collect()
    }

    /// Lines after `seq` (exclusive): pass back the last seen `seq` to poll
    /// for what arrived since.
    pub fn since(&self, seq: u64) -> Vec<LogEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.seq > seq)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A ring shared with the capture tasks a running fleet spawns.
pub type SharedLogRing = Arc<tokio::sync::Mutex<LogRing>>;

pub fn new_log_ring() -> SharedLogRing {
    Arc::new(tokio::sync::Mutex::new(LogRing::new(LogRing::DEFAULT_CAP)))
}

/// How long a fleet stop waits for graceful exit (SIGTERM) and then for the
/// SIGKILL reaping before giving up. celld shuts down gracefully on
/// SIGTERM/SIGINT (cancel work, prove durability, seal the node log);
/// SIGKILL is only a bounded fallback, delivered to the whole process group
/// so an orphaned worker can never keep the fleet's port into the next spawn.
#[derive(Debug, Clone, Copy)]
pub struct StopGrace {
    pub term: Duration,
    pub kill: Duration,
}

impl StopGrace {
    pub const NORMAL: Self = Self {
        term: Duration::from_secs(15),
        kill: Duration::from_secs(5),
    };
}

/// A running fleet: the child plus the ring its stdout/stderr drain into.
pub struct RunningFleet {
    pub slug: String,
    pub internal_port: u16,
    pub logs: SharedLogRing,
    child: tokio::process::Child,
}

fn spawn_capture<R>(pipe: R, stream: LogStream, logs: SharedLogRing)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            logs.lock().await.push(stream, line);
        }
    });
}

/// Start one celld fleet: state dir, uid drop, its own process group, its own
/// cgroup leaf, and stdout/stderr captured line by line into a fresh ring.
pub async fn start(spec: &FleetSpec, opts: &FleetOpts) -> anyhow::Result<RunningFleet> {
    use anyhow::Context;
    use std::os::unix::process::CommandExt;
    tokio::fs::create_dir_all(&opts.state_dir)
        .await
        .with_context(|| format!("create {}", opts.state_dir.display()))?;
    // celld runs as the app user: the whole state dir must be its own.
    // A zero gid means "same as the uid".
    let gid = if spec.gid == 0 { spec.uid } else { spec.gid };
    if let Some(uid) = super::sandbox::resolve_drop_id(spec.uid) {
        if !super::sandbox::lchown_tree(&opts.state_dir, uid, gid) {
            tracing::warn!(fleet = %spec.slug, "could not hand the fleet state dir to the app user");
        }
    }
    let (program, args, env) = fleet_command(spec, opts);
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in &env {
        cmd.env(key, value);
    }
    {
        let std_cmd = cmd.as_std_mut();
        if let Some(uid) = super::sandbox::resolve_drop_id(spec.uid) {
            std_cmd.uid(uid);
        }
        if super::sandbox::resolve_drop_id(gid).is_some() {
            std_cmd.gid(gid);
        }
        // Its own process group: the stops below kill the whole tree, so an
        // orphaned worker can never keep the fleet's port into the next
        // spawn. Uid drop above is untouched.
        std_cmd.process_group(0);
    }
    let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
    // Move the fleet into its leaf at once: between spawn and this write a
    // sliver of startup memory is charged to the shared cgroup instead —
    // negligible next to the fleet's lifetime, and documented, not hidden.
    if let Some(pid) = child.id() {
        if let Err(e) = std::fs::write(spec.cgroup_leaf.join("cgroup.procs"), pid.to_string()) {
            tracing::warn!(fleet = %spec.slug, leaf = %spec.cgroup_leaf.display(), error = %e, "fleet cgroup move failed; running uncapped");
        }
    }
    let logs = new_log_ring();
    if let Some(out) = child.stdout.take() {
        spawn_capture(out, LogStream::Stdout, logs.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_capture(err, LogStream::Stderr, logs.clone());
    }
    tracing::info!(fleet = %spec.slug, listen = spec.public_port, internal = spec.internal_port, "started celld");
    Ok(RunningFleet {
        slug: spec.slug.clone(),
        internal_port: spec.internal_port,
        logs,
        child,
    })
}

impl RunningFleet {
    /// Liveness: the child has not exited.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Reload a live fleet without a restart.
    pub async fn reload(&self) -> anyhow::Result<()> {
        let url = format!("http://127.0.0.1:{}/reload", self.internal_port);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        client.post(&url).send().await?.error_for_status()?;
        Ok(())
    }

    /// Stop the fleet: SIGTERM to the process group, a bounded grace, then
    /// SIGKILL to the group, then a final group SIGKILL so whatever survived
    /// — a worker that ignored SIGTERM — cannot outlive the stop.
    pub async fn stop(&mut self, grace: StopGrace) {
        let Some(pid) = self.child.id() else {
            let _ = self.child.wait().await;
            return;
        };
        // SAFETY: signals only the process group this supervisor created
        // (process_group(0) makes the group id the child's pid).
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGTERM);
        }
        if tokio::time::timeout(grace.term, self.child.wait())
            .await
            .is_err()
        {
            tracing::warn!(slug = %self.slug, "fleet did not stop in time; killing");
            // SAFETY: same group as above.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
            let _ = tokio::time::timeout(grace.kill, self.child.wait()).await;
        }
        // SAFETY: same group as above; ESRCH on a fully gone group is fine.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
        tracing::info!(slug = %self.slug, "stopped fleet");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn test_spec(state: &Path) -> (FleetSpec, FleetOpts) {
        let euid = unsafe { libc::geteuid() };
        let spec = FleetSpec {
            slug: "grocery".to_string(),
            // The test's own uid: dropping to self always succeeds, so this
            // exercises the spawn path with or without root.
            uid: euid,
            gid: euid,
            public_port: 22001,
            internal_port: 23001,
            bucket_url: "s3://outer/fleets/grocery".to_string(),
            s3_endpoint: "https://account.eu.r2.cloudflarestorage.com".to_string(),
            s3_region: "auto".to_string(),
            access_key_id: "key".to_string(),
            secret_access_key: "secret".to_string(),
            session_token: Some("token".to_string()),
            cgroup_leaf: state.to_path_buf(),
            extra_env: vec![
                ("GROCERY".to_string(), "milk".to_string()),
                ("OUTER_SECRET".to_string(), "nope".to_string()),
            ],
        };
        let opts = FleetOpts {
            celld_bin: "/usr/bin/celld".to_string(),
            state_dir: state.join("fleets").join("grocery"),
            shutdown_total_ms: 8000,
            idle_evict_secs: 300,
            deploy_poll_secs: 300,
            asset_cache_mb: 512,
        };
        (spec, opts)
    }

    #[test]
    fn command_line_and_env_match_noites_tenant_fleets() {
        let dir = tempfile::tempdir().unwrap();
        let (spec, opts) = test_spec(dir.path());
        let (program, args, env) = fleet_command(&spec, &opts);
        assert_eq!(program, "/usr/bin/celld");
        let argv = args.join(" ");
        assert!(
            argv.contains("--bucket s3://outer/fleets/grocery"),
            "{argv}"
        );
        assert!(
            argv.contains("--endpoint https://account.eu.r2.cloudflarestorage.com"),
            "{argv}"
        );
        assert!(argv.contains("--region auto"), "{argv}");
        assert!(argv.contains("--listen 0.0.0.0:22001"), "{argv}");
        assert!(argv.contains("--internal-listen 127.0.0.1:23001"), "{argv}");
        assert!(argv.contains("--advertise 127.0.0.1:23001"), "{argv}");
        let env: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(env["CELLD_DURABILITY"], "bucket");
        assert_eq!(env["CELLD_SHUTDOWN_TOTAL_MS"], "8000");
        assert_eq!(env["CELLD_IDLE_EVICT_S"], "300");
        assert_eq!(env["CELLD_DEPLOY_POLL_S"], "300");
        assert_eq!(
            env["CELLD_ASSET_CACHE_BYTES"],
            (512u64 * 1024 * 1024).to_string()
        );
        assert_eq!(env["CELLD_TRUST_FORWARDED_HEADERS"], "1");
        assert_eq!(env["AWS_ACCESS_KEY_ID"], "key");
        assert_eq!(env["AWS_SECRET_ACCESS_KEY"], "secret");
        assert_eq!(env["AWS_SESSION_TOKEN"], "token");
        assert_eq!(env["AWS_REGION"], "auto");
        assert_eq!(env["AWS_EC2_METADATA_DISABLED"], "true");
        assert_eq!(env["GROCERY"], "milk");
        // Reserved names never reach a fleet.
        assert!(
            !env.contains_key("OUTER_SECRET"),
            "reserved env leaked into fleet env"
        );
    }

    #[test]
    fn no_session_token_means_no_token_env() {
        let dir = tempfile::tempdir().unwrap();
        let (mut spec, opts) = test_spec(dir.path());
        spec.session_token = None;
        let (_, _, env) = fleet_command(&spec, &opts);
        let env: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert!(!env.contains_key("AWS_SESSION_TOKEN"));
    }

    #[test]
    fn ring_bounds_and_since() {
        let mut ring = LogRing::new(3);
        for line in ["a", "b", "c", "d"] {
            ring.push(LogStream::Stdout, line.to_string());
        }
        // Bounded: only the last 3 survive, with stable sequence numbers.
        let tail = ring.tail(10);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].line, "b");
        assert_eq!(tail[0].seq, 1);
        assert_eq!(tail[2].seq, 3);
        assert_eq!(ring.tail(2).len(), 2);
        // `since` is exclusive: what arrived after the given seq.
        let after = ring.since(1);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].line, "c");
        assert!(ring.since(3).is_empty());
        assert_eq!(ring.since(0).len(), 3);
    }

    /// Lifecycle without celld: a shell script that ignores its args stands
    /// in for the binary, so start/stop/is_alive run for real.
    #[tokio::test]
    async fn start_stop_and_liveness() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("celld");
        std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (mut spec, mut opts) = test_spec(dir.path());
        opts.celld_bin = bin.to_string_lossy().into_owned();
        spec.cgroup_leaf = dir.path().to_path_buf();
        let mut fleet = start(&spec, &opts).await.unwrap();
        assert!(fleet.is_alive());
        // Captured output drains into the ring (the fake celld is silent).
        assert!(fleet.logs.lock().await.is_empty());
        fleet
            .stop(StopGrace {
                term: Duration::from_secs(5),
                kill: Duration::from_secs(5),
            })
            .await;
        assert!(!fleet.is_alive());
    }
}
