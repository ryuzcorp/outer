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

/// One celld fleet per app. `Debug` redacts the minted secret, the session
/// token and `extra_env` values.
#[derive(Clone)]
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
    /// Whether `cgroups::setup()` succeeded: the caller sets this from
    /// `setup().is_some()`. Active means the leaf carries real limits, so a
    /// refused move fails the start instead of running uncapped; inactive
    /// (dev machines) only warns.
    pub cgroups_active: bool,
    pub extra_env: Vec<(String, String)>,
}

impl std::fmt::Debug for FleetSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetSpec")
            .field("slug", &self.slug)
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("public_port", &self.public_port)
            .field("internal_port", &self.internal_port)
            .field("bucket_url", &self.bucket_url)
            .field("s3_endpoint", &self.s3_endpoint)
            .field("s3_region", &self.s3_region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .field("cgroup_leaf", &self.cgroup_leaf)
            .field("cgroups_active", &self.cgroups_active)
            .field(
                "extra_env",
                &self
                    .extra_env
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
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
        format!("127.0.0.1:{}", spec.public_port),
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
/// `pgid` is the group id saved at spawn (the child's pid): `child.id()` is
/// `None` once the child is reaped (see `is_alive`), so `stop` must use the
/// saved id for its group kills instead of skipping them after an early reap.
pub struct RunningFleet {
    pub slug: String,
    pub internal_port: u16,
    pub logs: SharedLogRing,
    child: tokio::process::Child,
    pgid: u32,
}

/// Cap on one stored log line: a hostile fleet printing a newline-less flood
/// must not grow memory without bound. The stored line keeps this many bytes
/// (cut at a UTF-8 boundary) plus a truncation mark; the rest of the
/// over-long line is discarded until the next newline.
pub const MAX_LINE_BYTES: usize = 16 * 1024;

/// What `next_capped_line` appends once a line passes the cap.
const TRUNC_MARK: &str = "…[truncated]";

/// One line from a capture pipe, bounded: reads until the next newline (or
/// EOF), keeps at most `MAX_LINE_BYTES`, and discards the rest of an
/// over-long line. Returns `None` on EOF with nothing buffered.
async fn next_capped_line<R>(reader: &mut R) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut line: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let (consumed, done) = {
            let chunk = reader.fill_buf().await?;
            if chunk.is_empty() {
                // EOF: a trailing partial line still counts.
                if line.is_empty() && !truncated {
                    return Ok(None);
                }
                (0, true)
            } else {
                let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
                    Some(pos) => (pos, true),
                    None => (chunk.len(), false),
                };
                if !truncated {
                    let room = MAX_LINE_BYTES.saturating_sub(line.len());
                    if take <= room {
                        line.extend_from_slice(&chunk[..take]);
                    } else {
                        line.extend_from_slice(&chunk[..room]);
                        truncated = true;
                    }
                }
                (if done { take + 1 } else { take }, done)
            }
        };
        reader.consume(consumed);
        if done {
            break;
        }
    }
    // Strip a trailing CR (a `\r\n` line ending), then mark truncation.
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if truncated {
        // Drop only a multi-byte char split by the cap; earlier invalid
        // bytes are left to `from_utf8_lossy`.
        let keep = super::sandbox::trim_split_tail(&line).len();
        line.truncate(keep);
        line.extend_from_slice(TRUNC_MARK.as_bytes());
    }
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

fn spawn_capture<R>(pipe: R, stream: LogStream, logs: SharedLogRing)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::BufReader;
        let mut reader = BufReader::new(pipe);
        while let Ok(Some(line)) = next_capped_line(&mut reader).await {
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
    // celld runs as the app user: the state dir outer created must be its
    // own. A zero gid means "same as the uid". Only the dir itself: a
    // recursive walk as root over an app-writable tree is a symlink race,
    // and contents already belong to the app user from previous runs.
    let gid = if spec.gid == 0 { spec.uid } else { spec.gid };
    if let Some(uid) = super::sandbox::resolve_drop_id(spec.uid) {
        if !super::sandbox::lchown_one(&opts.state_dir, uid, gid) {
            tracing::warn!(fleet = %spec.slug, "could not hand the fleet state dir to the app user");
        }
        super::sandbox::make_private_dir(&opts.state_dir);
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
    let uid = super::sandbox::resolve_drop_id(spec.uid);
    let gid = super::sandbox::resolve_drop_id(gid);
    // setgroups needs privilege; an unprivileged (dev) start keeps its own
    // groups, which it cannot change anyway.
    let privileged = unsafe { libc::geteuid() == 0 };
    let drops_priv = privileged && (uid.is_some() || gid.is_some());
    {
        let std_cmd = cmd.as_std_mut();
        // Its own process group: the stops below kill the whole tree, so an
        // orphaned worker can never keep the fleet's port into the next
        // spawn.
        std_cmd.process_group(0);
        // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
        // libc calls. The ids are set here (not via CommandExt::uid/gid,
        // which would apply them before this hook, after which setgroups is
        // EPERM): setgroups first so the child never keeps outer's
        // supplementary groups, then setgid, then setuid.
        unsafe {
            std_cmd.pre_exec(move || {
                if drops_priv {
                    if libc::setgroups(0, std::ptr::null()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if let Some(gid) = gid {
                        if libc::setgid(gid) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    if let Some(uid) = uid {
                        if libc::setuid(uid) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
    // Read before any wait: `id()` goes `None` once the child is reaped (see
    // `is_alive`), and the group kills in `stop` need this pid. A spawn that
    // already has no pid never really started.
    let pgid = child
        .id()
        .with_context(|| format!("fleet {} has no pid right after spawn", spec.slug))?;
    // Move the fleet into its leaf at once: between spawn and this write a
    // sliver of startup memory is charged to the shared cgroup instead —
    // negligible next to the fleet's lifetime, and documented, not hidden.
    {
        let pid = pgid;
        if let Err(e) = std::fs::write(spec.cgroup_leaf.join("cgroup.procs"), pid.to_string()) {
            if spec.cgroups_active {
                // Fail closed when the leaf carries real limits: a refused
                // move must never run uncapped. Kill the whole tree (the
                // fleet leads its own group, so a worker it forked before
                // the move dies with it instead of serving from the shared
                // cgroup), reap, and fail the start.
                // SAFETY: signals only the process group this start created
                // (process_group(0) makes the group id the child's pid).
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                if e.raw_os_error() == Some(libc::ESRCH) {
                    anyhow::bail!("fleet {} exited before its cgroup move", spec.slug);
                }
                return Err(e)
                    .with_context(|| format!("fleet {} refused its cgroup move", spec.slug));
            }
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
        pgid,
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
    /// — a worker that ignored SIGTERM — cannot outlive the stop. The group
    /// id is the pid saved at spawn: `child.id()` is `None` once an earlier
    /// `is_alive` reaped the child, and that must not skip the group kills,
    /// or an orphaned worker keeps the fleet's port into the next spawn.
    pub async fn stop(&mut self, grace: StopGrace) {
        let pid = self.pgid;
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
            // Dev-like default: no active cgroups, so a refused move warns.
            cgroups_active: false,
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
        // The proxy is the only way in: the public listener binds loopback.
        assert!(argv.contains("--listen 127.0.0.1:22001"), "{argv}");
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
    fn debug_never_prints_minted_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let (spec, _) = test_spec(dir.path());
        let shown = format!("{spec:?}");
        assert!(!shown.contains("secret\""), "{shown}");
        assert!(!shown.contains("\"token\""), "{shown}");
        assert!(!shown.contains("milk"), "extra_env values: {shown}");
        assert!(shown.contains("GROCERY"), "names stay visible: {shown}");
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

    fn fake_celld(dir: &tempfile::TempDir, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.path().join("celld");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin.to_string_lossy().into_owned()
    }

    /// Fail closed: with active cgroups a refused move kills the tree and
    /// fails the start instead of running uncapped.
    #[tokio::test]
    async fn refused_move_fails_the_start_when_cgroups_are_active() {
        let dir = tempfile::tempdir().unwrap();
        let (mut spec, mut opts) = test_spec(dir.path());
        opts.celld_bin = fake_celld(&dir, "#!/bin/sh\nsleep 30\n");
        // Nowhere to move into: the write is refused (ENOENT).
        spec.cgroup_leaf = dir.path().join("no-such-leaf");
        spec.cgroups_active = true;
        let Err(err) = start(&spec, &opts).await else {
            panic!("an active refused move must fail");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cgroup move"),
            "fail-closed error names the move: {msg}"
        );
    }

    /// Warn-only path: without active cgroups (dev) the fleet still starts.
    #[tokio::test]
    async fn refused_move_warns_only_when_cgroups_are_inactive() {
        let dir = tempfile::tempdir().unwrap();
        let (mut spec, mut opts) = test_spec(dir.path());
        opts.celld_bin = fake_celld(&dir, "#!/bin/sh\nsleep 30\n");
        spec.cgroup_leaf = dir.path().join("no-such-leaf");
        spec.cgroups_active = false;
        let mut fleet = start(&spec, &opts).await.unwrap();
        assert!(fleet.is_alive());
        fleet
            .stop(StopGrace {
                term: Duration::from_secs(5),
                kill: Duration::from_secs(5),
            })
            .await;
        assert!(!fleet.is_alive());
    }

    /// A newline-less 1 MiB flood is stored capped and marked, and the line
    /// after it arrives intact: unbounded `lines()` would keep the whole
    /// megabyte in one ring entry.
    #[tokio::test]
    async fn capture_bounds_a_newline_less_flood() {
        let dir = tempfile::tempdir().unwrap();
        let (mut spec, mut opts) = test_spec(dir.path());
        opts.celld_bin = fake_celld(
            &dir,
            "#!/bin/sh\nhead -c 1048576 /dev/zero | tr '\\0' 'x'; echo; echo hello; sleep 30\n",
        );
        spec.cgroup_leaf = dir.path().to_path_buf();
        let mut fleet = start(&spec, &opts).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let tail = loop {
            let tail = fleet.logs.lock().await.tail(10);
            if tail.len() >= 2 || std::time::Instant::now() >= deadline {
                break tail;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(tail.len(), 2, "flood line plus the line after it");
        assert!(
            tail[0].line.len() <= MAX_LINE_BYTES + TRUNC_MARK.len(),
            "flood stored capped, got {} bytes",
            tail[0].line.len()
        );
        assert!(tail[0].line.ends_with(TRUNC_MARK), "flood marked truncated");
        assert_eq!(tail[1].line, "hello");
        fleet
            .stop(StopGrace {
                term: Duration::from_secs(5),
                kill: Duration::from_secs(5),
            })
            .await;
    }
    fn cmdline_has(marker: &str) -> bool {
        let Ok(proc) = std::fs::read_dir("/proc") else {
            return false;
        };
        for entry in proc.flatten() {
            if let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) {
                if bytes
                    .windows(marker.len())
                    .any(|window| window == marker.as_bytes())
                {
                    return true;
                }
            }
        }
        false
    }

    /// A flood line with an early invalid byte is stored capped with a
    /// replacement char, not trimmed back to the invalid byte; the line
    /// after it still arrives intact.
    #[tokio::test]
    async fn capture_keeps_early_invalid_bytes_in_a_flood_line() {
        let mut flood = vec![0xFF];
        flood.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES + 100));
        flood.push(b'\n');
        flood.extend_from_slice(b"hello\n");
        let mut reader = tokio::io::BufReader::new(&flood[..]);
        let first = next_capped_line(&mut reader).await.unwrap().unwrap();
        assert!(
            first.starts_with('\u{FFFD}'),
            "early invalid byte must survive as a replacement char"
        );
        assert!(first.ends_with(TRUNC_MARK), "flood marked truncated");
        assert!(
            first.len() <= MAX_LINE_BYTES + TRUNC_MARK.len() + 3,
            "flood stored capped, got {} bytes",
            first.len()
        );
        let second = next_capped_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(second, "hello");
    }

    /// A fleet whose leader already exited (reaped via `is_alive`, which
    /// flips `child.id()` to `None`) must still have its group killed on
    /// stop: the orphaned sleeper keeps the group — and the fleet's port —
    /// alive, and the old early return left it running.
    #[tokio::test]
    async fn stop_after_reap_still_kills_the_group() {
        // A sleep duration no other process on the machine can plausibly
        // share, so the /proc scan below cannot false-positive.
        let marker = "31337977";
        let dir = tempfile::tempdir().unwrap();
        let (mut spec, mut opts) = test_spec(dir.path());
        // The leader exits at once, leaving a group member behind.
        opts.celld_bin = fake_celld(&dir, &format!("#!/bin/sh\nsleep {marker} &\n"));
        spec.cgroup_leaf = dir.path().to_path_buf();
        let mut fleet = start(&spec, &opts).await.unwrap();
        assert_eq!(fleet.pgid, fleet.child.id().unwrap_or(fleet.pgid));
        // Reap the exited leader, like a liveness poll would.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while fleet.is_alive() {
            if std::time::Instant::now() >= deadline {
                panic!("fake celld never exited");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        fleet
            .stop(StopGrace {
                term: Duration::from_secs(2),
                kill: Duration::from_secs(2),
            })
            .await;
        assert!(!fleet.is_alive());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while cmdline_has(marker) {
            if std::time::Instant::now() >= deadline {
                panic!("orphaned sleep {marker} survived the stop");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
