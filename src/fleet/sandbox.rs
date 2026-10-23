// Derived from noite apps/runner/src/host/exec.rs @ 58a977a (Apache-2.0).
// Outer change: one uid per app (no shared build uid, no jup/npm/pnpm/yarn
// env); the reserved env-name filter lives here as a pure function; the
// child is moved into its cgroup leaf right after spawn.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;

/// Cap on captured output per stream: a runaway step cannot OOM the caller.
/// Over-long output is truncated at a UTF-8 boundary.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Explicit base environment for sandboxed steps. Nothing is inherited:
/// `run_sandboxed` starts from `env_clear()` plus these, then the caller's
/// vars. Every cache lives under the app's own cache root: bun's in `bun/`
/// and `HOME` in `home/`, so no two apps share a cache.
pub fn base_env(work_tmp: &str, cache_root: &Path) -> Vec<(String, String)> {
    vec![
        ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        (
            "HOME".into(),
            cache_root.join("home").to_string_lossy().into_owned(),
        ),
        ("TMPDIR".into(), work_tmp.into()),
        ("LANG".into(), "C.UTF-8".into()),
        ("CI".into(), "1".into()),
        // `CI` alone turns colors on in picocolors (Vite); the deploy log is
        // plain text.
        ("NO_COLOR".into(), "1".into()),
        ("NODE_ENV".into(), "production".into()),
        (
            "BUN_INSTALL_CACHE_DIR".into(),
            cache_root.join("bun").to_string_lossy().into_owned(),
        ),
    ]
}

/// Reserved prefixes/names the platform owns: app secrets with these names
/// are dropped (logged) rather than injected into build/fleet env.
/// `OUTER_*` replaces Noite's `NOITE_*`/`RUNNER_*`; jup/npm/pnpm/yarn
/// specifics are gone with the package managers.
pub fn is_reserved_env_name(name: &str) -> bool {
    name == "PORT"
        || name == "HOST"
        || name.starts_with("AWS_")
        || name.starts_with("S3_")
        || name.starts_with("CELLD_")
        || name.starts_with("OUTER_")
        || name.starts_with("CLOUDFLARE_")
        || name.starts_with("R2_")
        || name.starts_with("STRIPE_")
        || name.starts_with("RESEND_")
        || name.starts_with("JEV_")
        || name.starts_with("LD_")
        || name == "NODE_OPTIONS"
        || (name.starts_with("BUN_") && name != "BUN_INSTALL_CACHE_DIR")
}

/// Sandboxed step: uid/gid drop plus explicit env.
/// `uid: None` runs as the current user.
/// `cache_root` is the app's persistent cache dir; `cgroup_leaf` moves the
/// child into that leaf right after spawn. A leaf means cgroups are active:
/// a refused move fails the step (the group is killed) instead of running
/// uncapped. `None` is dev (no cgroups), where no move is attempted.
pub struct Sandbox<'a> {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub env: &'a [(String, String)],
    pub cache_root: &'a Path,
    pub cgroup_leaf: Option<&'a Path>,
}

/// Resolve a requested uid drop to what this process may actually set.
/// Root (or an already-matching uid) drops; anything else runs as the
/// current user and warns once, so dev machines without privileges still run.
pub(crate) fn resolve_drop_id(want: u32) -> Option<u32> {
    let euid = unsafe { libc::geteuid() };
    if want == euid || euid == 0 {
        return Some(want);
    }
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            uid = want,
            "not root; running sandboxed command as the current user"
        );
    });
    None
}

/// Recursively hand `path` to (uid, gid) without following symlinks: an app
/// tree can link at platform paths, and a following chown run as root would
/// hand the app the platform database. Returns false when any entry could
/// not be changed.
#[cfg(unix)]
pub(crate) fn lchown_tree(path: &Path, uid: u32, gid: u32) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let mut ok = true;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let meta = match std::fs::symlink_metadata(&p) {
            Ok(meta) => meta,
            Err(_) => {
                ok = false;
                continue;
            }
        };
        match CString::new(p.as_os_str().as_bytes()) {
            Ok(path) => {
                if unsafe { libc::lchown(path.as_ptr(), uid, gid) } != 0 {
                    ok = false;
                }
            }
            Err(_) => ok = false,
        }
        // `symlink_metadata` never reports a symlink as a dir, so links are
        // never descended into.
        if meta.file_type().is_dir() {
            match std::fs::read_dir(&p) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        stack.push(entry.path());
                    }
                }
                Err(_) => ok = false,
            }
        }
    }
    ok
}

/// Move `pid` into a cgroup leaf: the child's whole tree is contained from
/// here, since containment is inherited across fork and exec. Returns the
/// refusal so the caller fails the step instead of running uncapped.
fn move_into_cgroup(leaf: &Path, pid: u32) -> anyhow::Result<()> {
    std::fs::write(leaf.join("cgroup.procs"), pid.to_string())
        .with_context(|| format!("move {pid} into {}", leaf.display()))?;
    Ok(())
}

/// Whether `error` is the kernel reporting a gone process (ESRCH): a move
/// target that exited mid-move. Only that case means nothing runs uncapped;
/// every other refusal (EACCES, EPERM, read-only mount, no delegation) is a
/// real containment miss.
fn move_target_gone(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| io.raw_os_error() == Some(libc::ESRCH))
}

/// Truncate captured output at a UTF-8 boundary.
fn cap_output(output: &[u8]) -> String {
    let mut capped = output;
    if capped.len() > MAX_OUTPUT_BYTES {
        capped = &capped[..MAX_OUTPUT_BYTES];
        while !capped.is_empty() && std::str::from_utf8(capped).is_err() {
            capped = &capped[..capped.len() - 1];
        }
    }
    String::from_utf8_lossy(capped).into_owned()
}

/// Sandboxed step: `env_clear`, base env plus caller vars, uid/gid drop, its
/// own process group (the whole tree dies on timeout via killpg), rlimits
/// (NPROC, FSIZE 4 GiB), and capped output. Network egress for the sandbox
/// uid is closed by the nft policy in `super::nft`, which also covers this
/// uid.
pub async fn run_sandboxed(
    program: &str,
    args: &[&str],
    cwd: &Path,
    sb: &Sandbox<'_>,
    timeout: Duration,
) -> anyhow::Result<String> {
    use std::os::unix::process::CommandExt;
    let tmp = cwd.join(".tmp-sandbox");
    let _ = std::fs::create_dir_all(&tmp);
    for sub in ["bun", "home"] {
        let _ = std::fs::create_dir_all(sb.cache_root.join(sub));
    }
    // Created by outer after the worktree was handed over: give them to the
    // sandbox user too, or TMPDIR and the cache are unwritable.
    if let Some(uid) = sb.uid {
        let gid = sb.gid.unwrap_or(uid);
        lchown_tree(&tmp, uid, gid);
        lchown_tree(sb.cache_root, uid, gid);
    }
    let uid = sb.uid.and_then(resolve_drop_id);
    let gid = sb.gid.or(sb.uid).and_then(resolve_drop_id);
    let drops_uid = uid.is_some();
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in base_env(&tmp.to_string_lossy(), sb.cache_root) {
        cmd.env(key, value);
    }
    for (key, value) in sb.env {
        if is_reserved_env_name(key) {
            tracing::warn!(name = %key, "sandbox env reserved; skipping");
            continue;
        }
        cmd.env(key, value);
    }
    // setgroups needs privilege; an unprivileged (dev) run keeps its own
    // groups, which it cannot change anyway.
    let privileged = unsafe { libc::geteuid() == 0 };
    let drops_priv = privileged && (uid.is_some() || gid.is_some());
    {
        let std_cmd = cmd.as_std_mut();
        std_cmd.process_group(0);
        // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
        // libc calls. The ids are set here (not via CommandExt::uid/gid,
        // which would apply them before this hook, after which setgroups is
        // EPERM): setgroups first so the child never keeps outer's
        // supplementary groups, then setgid, then setuid. setrlimit bounds
        // fork bombs (NPROC) and runaway outputs (FSIZE 4 GiB).
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
                // RLIMIT_NPROC counts every process of the real uid, so it is
                // only meaningful for a dedicated app uid; on outer's own uid
                // (dev) it would cap outer and its fleets too.
                if drops_uid {
                    let nproc = libc::rlimit {
                        rlim_cur: 512,
                        rlim_max: 512,
                    };
                    libc::setrlimit(libc::RLIMIT_NPROC, &nproc);
                }
                let fsize = libc::rlimit {
                    rlim_cur: 4 * 1024 * 1024 * 1024,
                    rlim_max: 4 * 1024 * 1024 * 1024,
                };
                libc::setrlimit(libc::RLIMIT_FSIZE, &fsize);
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
    let pid = child.id();
    if let (Some(leaf), Some(pid)) = (sb.cgroup_leaf, pid) {
        if let Err(e) = move_into_cgroup(leaf, pid) {
            if move_target_gone(&e) {
                // Exited between spawn and the move: the wait below still
                // reaps it, and nothing runs uncapped.
                tracing::warn!(leaf = %leaf.display(), pid, "sandbox step exited before its cgroup move");
            } else {
                // Refused for a live pid: fail closed, like the fleet move.
                // SAFETY: signals only the process group this call created.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                return Err(e);
            }
        }
    }
    let result = tokio::time::timeout(timeout, child.wait_with_output()).await;
    // Whatever happened, nothing the step started may outlive it: a step
    // that leaves a watcher behind would keep running as the app uid between
    // steps. The group id is the child's pid (process_group(0)); ESRCH when
    // the group is already empty is fine.
    if let Some(pid) = pid {
        // SAFETY: signals only the process group this call created.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    match result {
        Ok(Ok(output)) => {
            if !output.status.success() {
                let err = cap_output(&output.stderr);
                let out = cap_output(&output.stdout);
                // stdout first: a build's progress goes there and its error to
                // stderr, and the deploy log keeps the tail.
                anyhow::bail!(
                    "{program} {args:?} exit {:?}\n{out}{err}",
                    output.status.code()
                );
            }
            Ok(cap_output(&output.stdout))
        }
        Ok(Err(e)) => Err(e).context("wait output"),
        Err(_) => Err(anyhow::anyhow!(
            "{program} {args:?} timed out after {timeout:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_names_cover_the_platform() {
        for name in [
            "PORT",
            "HOST",
            "AWS_SECRET_ACCESS_KEY",
            "S3_ENDPOINT",
            "CELLD_WATCH",
            "OUTER_SECRET",
            "CLOUDFLARE_API_TOKEN",
            "R2_ACCESS_KEY_ID",
            "STRIPE_SECRET_KEY",
            "RESEND_API_KEY",
            "JEV_API_KEY",
            "LD_PRELOAD",
            "NODE_OPTIONS",
            "BUN_VERSION",
        ] {
            assert!(is_reserved_env_name(name), "{name} must be reserved");
        }
        // The cache dir override is set by the sandbox itself, not secret.
        assert!(!is_reserved_env_name("BUN_INSTALL_CACHE_DIR"));
        // Dropped with jup/npm/pnpm/yarn: no longer platform-owned.
        for name in ["NOITE_TOKEN", "RUNNER_TOKEN", "JUP_HOME", "NPM_TOKEN"] {
            assert!(!is_reserved_env_name(name), "{name} must not be reserved");
        }
        assert!(!is_reserved_env_name("DATABASE_URL"));
    }

    #[test]
    fn base_env_points_caches_at_the_app_root() {
        let cache = Path::new("/data/cache/grocery");
        let env: std::collections::HashMap<_, _> =
            base_env("/data/work/grocery/.tmp-sandbox", cache)
                .into_iter()
                .collect();
        assert_eq!(env["PATH"], "/usr/local/bin:/usr/bin:/bin");
        assert_eq!(env["HOME"], "/data/cache/grocery/home");
        assert_eq!(env["BUN_INSTALL_CACHE_DIR"], "/data/cache/grocery/bun");
        assert_eq!(env["TMPDIR"], "/data/work/grocery/.tmp-sandbox");
        assert_eq!(env["LANG"], "C.UTF-8");
        assert_eq!(env["NODE_ENV"], "production");
    }

    /// `env_clear` is honored: only the base env plus caller vars are
    /// visible, never outer's own environment.
    #[tokio::test]
    async fn sandboxed_env_is_exactly_base_plus_caller() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let caller = vec![("GROCERY_LIST".to_string(), "milk".to_string())];
        let sb = Sandbox {
            uid: None,
            gid: None,
            env: &caller,
            cache_root: &cache,
            cgroup_leaf: None,
        };
        let out = run_sandboxed("env", &[], dir.path(), &sb, Duration::from_secs(10))
            .await
            .unwrap();
        let seen: std::collections::HashMap<&str, &str> = out
            .lines()
            .map(|line| line.split_once('=').unwrap())
            .collect();
        let base = base_env("", Path::new(""));
        let mut expected: std::collections::HashSet<&str> =
            base.iter().map(|(key, _)| key.as_str()).collect();
        expected.insert("GROCERY_LIST");
        let seen_keys: std::collections::HashSet<&str> = seen.keys().copied().collect();
        assert_eq!(seen_keys, expected);
        assert_eq!(seen["GROCERY_LIST"], "milk");
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

    /// A timed-out step must not leave grandchildren: `sh` backgrounds a
    /// `sleep` (the step's grandchild, in its process group) and waits, the
    /// timeout fires, and the group kill must reap the whole tree.
    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        // A sleep duration no other process on the machine can plausibly
        // share, so the /proc scan below cannot false-positive.
        let marker = "31337911";
        let script = format!("sleep {marker} & sleep {marker}");
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let sb = Sandbox {
            uid: None,
            gid: None,
            env: &[],
            cache_root: &cache,
            cgroup_leaf: None,
        };
        let err = run_sandboxed(
            "sh",
            &["-c", &script],
            dir.path(),
            &sb,
            Duration::from_secs(2),
        )
        .await
        .expect_err("sleep must hit the timeout");
        assert!(err.to_string().contains("timed out"), "{err:#}");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while cmdline_has(marker) {
            if std::time::Instant::now() >= deadline {
                panic!("timed-out step left a grandchild behind (sleep {marker} still live)");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    #[ignore = "needs root to setuid to an app uid"]
    async fn drops_to_the_app_uid() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let sb = Sandbox {
            uid: Some(200000),
            gid: Some(200000),
            env: &[],
            cache_root: &cache,
            cgroup_leaf: None,
        };
        let out = run_sandboxed("id", &["-u"], dir.path(), &sb, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.trim(), "200000");
    }
    /// Reserved names in `sb.env` never reach the child: the platform owns
    /// them, and the value is never logged.
    #[tokio::test]
    async fn sandboxed_env_drops_reserved_names() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let caller = vec![
            ("GROCERY_LIST".to_string(), "milk".to_string()),
            ("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string()),
            ("OUTER_SECRET".to_string(), "s3cr3t".to_string()),
        ];
        let sb = Sandbox {
            uid: None,
            gid: None,
            env: &caller,
            cache_root: &cache,
            cgroup_leaf: None,
        };
        let out = run_sandboxed("env", &[], dir.path(), &sb, Duration::from_secs(10))
            .await
            .unwrap();
        let seen: std::collections::HashMap<&str, &str> = out
            .lines()
            .map(|line| line.split_once('=').unwrap())
            .collect();
        assert_eq!(seen["GROCERY_LIST"], "milk");
        assert!(
            !seen.contains_key("LD_PRELOAD"),
            "reserved env leaked into sandbox env"
        );
        assert!(
            !seen.contains_key("OUTER_SECRET"),
            "reserved env leaked into sandbox env"
        );
    }
}
