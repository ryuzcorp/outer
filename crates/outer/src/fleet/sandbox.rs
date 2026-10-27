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
/// vars. Every cache lives under the app's own cache root: bun's in `bun/`,
/// `HOME` in `home/`, and `TMPDIR` in `tmp/`, so no two apps share a cache —
/// and so `git clean` (which wipes the worktree between deploys) cannot wipe
/// them. In particular bun's node-compile-cache lives under `TMPDIR`: with
/// a worktree TMPDIR every build paid a cold transpiler cache (~6 s on the
/// template); with a persistent one warm builds reuse it.
pub fn base_env(cache_root: &Path) -> Vec<(String, String)> {
    vec![
        ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        (
            "HOME".into(),
            cache_root.join("home").to_string_lossy().into_owned(),
        ),
        (
            "TMPDIR".into(),
            cache_root.join("tmp").to_string_lossy().into_owned(),
        ),
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
/// `cache_root` is the app's persistent cache dir; `cgroup_leaf` contains
/// the child in that leaf. The child moves itself there in pre_exec (before
/// its first fork); the parent re-asserts right after spawn. A leaf means
/// cgroups are active: a refused move fails the step (the group is killed)
/// instead of running uncapped. `None` is dev (no cgroups), where no move is
/// attempted.
/// Each step needs its own leaf: cleanup kills the whole leaf (`cgroup.kill`
/// plus the process group), so a shared leaf would kill sibling steps.
///
/// Two env lists: `platform_env` is set by outer itself (PATH, the bucket
/// credentials `celld deploy` needs) and reaches the child as given; `env`
/// carries app-supplied values (secrets) and goes through
/// [`is_reserved_env_name`], so an app can never set `LD_PRELOAD`, `AWS_*` or
/// any platform name.
pub struct Sandbox<'a> {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub platform_env: &'a [(String, String)],
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

/// The per-app cache subdirs `run_sandboxed` maintains (`bun/`, `home/`,
/// `tmp/`, see [`base_env`]).
#[cfg(unix)]
const CACHE_SUBDIRS: [&str; 3] = ["bun", "home", "tmp"];

/// Create the cache subdirs without following app-planted links: each is
/// made with `mkdirat` relative to an `O_NOFOLLOW`-validated fd of the
/// cache root. An entry that exists but is not a real dir (symlink, file)
/// is removed with `unlinkat` (no follow) and recreated; anything else
/// fails the step closed rather than running on an attacker-planted tree.
#[cfg(unix)]
fn ensure_cache_subdirs(cache_root: &Path) -> anyhow::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::RawFd;
    if std::fs::symlink_metadata(cache_root).is_err() {
        // Fresh app: the parents (`<data>/cache`) are platform-owned, so
        // creating the root itself is safe; what it contains afterwards is
        // only ever touched through the fd below.
        std::fs::create_dir_all(cache_root)?;
    }
    let root = CString::new(cache_root.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("cache root has an interior NUL"))?;
    // SAFETY: `O_NOFOLLOW|O_DIRECTORY` fails rather than following a link;
    // the fd pins the root for the `mkdirat` calls below.
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
    for sub in CACHE_SUBDIRS {
        let name = CString::new(sub).expect("subdir names have no NUL");
        // SAFETY: `mkdirat` never follows the final component.
        if unsafe { libc::mkdirat(fd, name.as_ptr(), 0o700) } == 0 {
            continue;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("creating cache subdir {sub}"));
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fstatat` reports the entry itself; the buffer is ours.
        let stat_ok =
            unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0;
        if stat_ok && (st.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            continue;
        }
        // A planted link or file: remove the name itself (no follow) and
        // recreate the real dir. Anything else fails closed.
        // SAFETY: `unlinkat` without flags never follows the final
        // component; the retry only runs when the entry was not a dir.
        if unsafe { libc::unlinkat(fd, name.as_ptr(), 0) } != 0
            || unsafe { libc::mkdirat(fd, name.as_ptr(), 0o700) } != 0
        {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("replacing cache subdir {sub}"));
        }
    }
    Ok(())
}

/// Hand one path that outer itself created to (uid, gid), without following
/// it and without traversal: `lchown` never follows the final component, so
/// an app-uid process racing outer cannot redirect this outside the tree the
/// way a recursive walk could (a checked directory swapped for a symlink
/// mid-walk would have root list and chown entries outside the tree).
/// Contents already belong to the app user from previous steps; only dirs
/// outer creates need handing over. Returns false when the change failed.
#[cfg(unix)]
pub(crate) fn lchown_one(path: &Path, uid: u32, gid: u32) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `lchown` does not follow the final component.
    unsafe { libc::lchown(path.as_ptr(), uid, gid) == 0 }
}

/// Top-level dirs under the data dir that app uids must traverse into (their
/// own `<root>/<slug>` only). Everything else there is platform-private.
pub const APP_ROOTS: [&str; 4] = ["work", "cache", "fleets", "checks"];

/// Lock down the data dir before anything else touches it, when outer runs
/// as root (production). The dir is `0711`: app uids can reach their own
/// worktree, cache, fleet state and check scratch by path but cannot list
/// or open anything else. App roots are `0711` too (each `<slug>` below them is `0700` and
/// owned by the app uid); every other top-level entry (SQLite, git mirrors,
/// templates) is `0700` / `0600`. The process umask becomes `077`, so
/// whatever outer or a sandboxed step creates later starts private.
pub fn prepare_data_dir(data_dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: umask only swaps the process file-creation mask.
    unsafe { libc::umask(0o077) };
    std::fs::create_dir_all(data_dir)?;
    let set = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {mode:o} {}", path.display()))
    };
    set(data_dir, 0o711)?;
    for root in APP_ROOTS {
        let dir = data_dir.join(root);
        std::fs::create_dir_all(&dir)?;
        set(&dir, 0o711)?;
        // Outer's own sidecars next to the per-app dirs (`<slug>.index`,
        // `<slug>.install-hash`) have guessable names under a traversable
        // dir: private, whatever umask created them.
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                set(&entry.path(), 0o600)?;
            }
        }
    }
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if APP_ROOTS.iter().any(|root| name == *root) {
            continue;
        }
        // `file_type` does not follow links: a link is left alone.
        let kind = entry.file_type()?;
        if kind.is_dir() {
            set(&entry.path(), 0o700)?;
        } else if kind.is_file() {
            set(&entry.path(), 0o600)?;
        }
    }
    Ok(())
}

/// Make an outer-created per-app dir (`work/<slug>`, `cache/<slug>`,
/// `fleets/<slug>`) private to its owner. Never follows a link.
pub(crate) fn make_private_dir(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).is_ok()
        }
        _ => false,
    }
}

/// Move `pid` into a cgroup leaf: the child's whole tree is contained from
/// here, since containment is inherited across fork and exec. Returns the
/// refusal so the caller fails the step instead of running uncapped. This is
/// the post-spawn re-assert; the child also moves itself in pre_exec (see
/// `self_move_into_leaf`), which is what actually wins the race against the
/// child's first fork.
fn move_into_cgroup(leaf: &Path, pid: u32) -> anyhow::Result<()> {
    std::fs::write(leaf.join("cgroup.procs"), pid.to_string())
        .with_context(|| format!("move {pid} into {}", leaf.display()))?;
    Ok(())
}

/// The `cgroup.procs` file a child writes its own pid to, pre-built as a
/// `CString` so the pre_exec hook (which may not allocate) can use it.
/// `None` when the leaf path cannot be a C string (impossible for our
/// generated leaf names); the caller then falls back to the post-spawn move.
fn cgroup_procs_cstr(leaf: &Path) -> Option<std::ffi::CString> {
    std::ffi::CString::new(leaf.join("cgroup.procs").as_os_str().as_encoded_bytes()).ok()
}

/// Decimal `pid` into `buf`, returning the used length. Manual (no
/// allocation, no formatting machinery): the pre_exec hook may not allocate.
fn format_pid(mut pid: u32, buf: &mut [u8; 20]) -> usize {
    if pid == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut len = 0;
    while pid > 0 {
        buf[len] = b'0' + (pid % 10) as u8;
        pid /= 10;
        len += 1;
    }
    buf[..len].reverse();
    len
}

/// The child moves ITSELF into `procs` (a leaf `cgroup.procs`) before
/// dropping privilege or execing: nothing the child later forks can escape
/// the leaf, because containment is inherited at fork and the move precedes
/// the first fork by construction. (A post-spawn move from the parent races
/// the child's first fork: `bun run` forks a runner process during startup,
/// and on a loaded host the parent's write can land after it, leaving the
/// whole build in the parent cgroup. Builds then ran uncapped — no memory
/// or pids bound — while their empty leaves reported ~0 CPU.)
///
/// Only async-signal-safe calls (`open`, `write`, `close`, `getpid`): the
/// hook runs between fork and exec. Must run while still root: an app uid
/// cannot write a root-owned leaf's `cgroup.procs`.
fn self_move_into_leaf(procs: &std::ffi::CStr) -> std::io::Result<()> {
    // SAFETY: async-signal-safe libc calls only; the fd is closed on every
    // path, and the buffer is a stack array filled by `format_pid`.
    unsafe {
        let fd = libc::open(procs.as_ptr(), libc::O_WRONLY);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = [0u8; 20];
        let len = format_pid(libc::getpid() as u32, &mut buf);
        let mut written = 0;
        while written < len {
            let n = libc::write(
                fd,
                buf[written..len].as_ptr() as *const libc::c_void,
                (len - written) as libc::size_t,
            );
            if n < 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
            written += n as usize;
        }
        libc::close(fd);
        Ok(())
    }
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

/// Cut `capped` (already truncated to its cap) back to the last whole UTF-8
/// char. Only a multi-byte char split by the cut is dropped: earlier invalid
/// bytes are left for `from_utf8_lossy` to replace, not treated as a cut.
pub(crate) fn trim_split_tail(capped: &[u8]) -> &[u8] {
    if let Err(e) = std::str::from_utf8(capped) {
        if e.error_len().is_none() {
            return &capped[..e.valid_up_to()];
        }
    }
    capped
}

/// Truncate captured output at a UTF-8 boundary.
fn cap_output(output: &[u8]) -> String {
    let mut capped = output;
    if capped.len() > MAX_OUTPUT_BYTES {
        capped = trim_split_tail(&capped[..MAX_OUTPUT_BYTES]);
    }
    String::from_utf8_lossy(capped).into_owned()
}

/// Kill every process in a cgroup leaf. `killpg` cannot reach members that
/// called `setsid`/`setpgid` to leave the group, but nothing escapes the
/// leaf without a further move, so the leaf kill is the backstop keeping a
/// detached watcher from outliving the step as the app uid. Best-effort: a
/// missing leaf or a fully reaped one is success. `cgroup.kill` (Linux
/// 5.14+) kills the leaf in one write; older kernels fall back to signaling
/// each pid in `cgroup.procs` until it stays empty.
fn kill_leaf(leaf: &Path) {
    if std::fs::write(leaf.join("cgroup.kill"), "1").is_ok() {
        return;
    }
    // Pre-5.14: signal the listed members directly, re-reading until the
    // leaf stays empty. Zombies stay listed but cannot fork, so a bounded
    // number of rounds always terminates.
    for _ in 0..5 {
        let procs = std::fs::read_to_string(leaf.join("cgroup.procs")).unwrap_or_default();
        let mut any = false;
        for pid in procs
            .split_whitespace()
            .filter_map(|pid| pid.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        {
            any = true;
            // SAFETY: pids come from our own step leaf; SIGKILL only. ESRCH
            // (exited between listing and kill) is fine.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        if !any {
            return;
        }
    }
}

/// Remove the step's cgroup leaf after its processes are gone: per-step
/// leaves are killed but never removed, and stale leaves pile up under the
/// build parent. Best-effort: a leaf whose members are still reaping reports
/// busy, which is retried briefly, then left with a debug log instead of
/// failing the step.
async fn remove_step_leaf(leaf: &Path) {
    for _ in 0..10 {
        match std::fs::remove_dir(leaf) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e)
                if e.raw_os_error() == Some(libc::EBUSY)
                    || e.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                tracing::debug!(leaf = %leaf.display(), error = %e, "step leaf left behind");
                return;
            }
        }
    }
    tracing::debug!(leaf = %leaf.display(), "step leaf still busy; leaving it");
}

/// Sandboxed step: `env_clear`, base env plus caller vars, uid/gid drop, its
/// own process group (the whole tree dies on timeout via killpg), rlimits
/// (NPROC only without a leaf, FSIZE 4 GiB always), and capped output.
/// Network egress for the sandbox uid is closed by the nft policy in
/// `super::nft`, which also covers this uid.
pub async fn run_sandboxed(
    program: &str,
    args: &[&str],
    cwd: &Path,
    sb: &Sandbox<'_>,
    timeout: Duration,
) -> anyhow::Result<String> {
    use std::os::unix::process::CommandExt;
    // `tmp` persists across deploys (bun's node-compile-cache lives under
    // `TMPDIR`; a worktree TMPDIR was wiped by `git clean` every deploy and
    // every build repaid a cold transpiler cache). It is per-app, 0700 and
    // app-owned like `bun/` and `home/`, so no two apps share it.
    // No-follow: `create_dir_all(cache_root.join(sub))` as root would
    // create through an app-planted symlink. A planted non-dir is replaced
    // with a real dir; anything else fails the step closed.
    #[cfg(unix)]
    ensure_cache_subdirs(sb.cache_root)?;
    #[cfg(not(unix))]
    for sub in ["bun", "home", "tmp"] {
        let _ = std::fs::create_dir_all(sb.cache_root.join(sub));
    }
    // Created by outer after the worktree was handed over: give them to the
    // sandbox user too, or TMPDIR and the cache are unwritable. Only the
    // dirs outer itself created: a recursive walk as root over an
    // app-writable tree is a symlink race, and contents already belong to
    // the app user from previous steps. Nested root-owned files seeded by
    // other means stay root-owned (documented tradeoff).
    if let Some(uid) = sb.uid {
        let gid = sb.gid.unwrap_or(uid);
        lchown_one(sb.cache_root, uid, gid);
        make_private_dir(sb.cache_root);
        for sub in ["bun", "home", "tmp"] {
            lchown_one(&sb.cache_root.join(sub), uid, gid);
        }
    }
    let uid = sb.uid.and_then(resolve_drop_id);
    let gid = sb.gid.or(sb.uid).and_then(resolve_drop_id);
    let drops_uid = uid.is_some();
    // The uid-wide NPROC cap only where no leaf bounds the step (dev): with
    // a leaf, `pids.max` on the leaf caps the step without counting the
    // fleet's tasks, which share the app uid and would otherwise starve
    // builds with EAGAIN.
    let cap_nproc = drops_uid && sb.cgroup_leaf.is_none();
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in base_env(sb.cache_root) {
        cmd.env(key, value);
    }
    for (key, value) in sb.platform_env {
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
    // The child moves itself into its leaf in pre_exec (before any fork it
    // can do): the post-spawn move below re-asserts, but only the self-move
    // wins the race against the child's first fork. Pre-built here (no
    // allocation is allowed in the hook); a missing C string falls back to
    // the post-spawn move alone.
    let self_move = sb.cgroup_leaf.and_then(cgroup_procs_cstr);
    {
        let std_cmd = cmd.as_std_mut();
        std_cmd.process_group(0);
        // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
        // libc calls. The ids are set here (not via CommandExt::uid/gid,
        // which would apply them before this hook, after which setgroups is
        // supplementary groups, then setgid, then setuid. setrlimit bounds
        // runaway outputs (FSIZE 4 GiB) always and fork bombs (NPROC) where
        // no leaf bounds the step (with a leaf the pids.max cap applies).
        unsafe {
            std_cmd.pre_exec(move || {
                if let Some(procs) = self_move.as_ref() {
                    // While still root: an app uid cannot write the leaf's
                    // `cgroup.procs`. A refusal fails the spawn, so the step
                    // fails closed instead of running uncapped.
                    self_move_into_leaf(procs)?;
                }
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
                // only meaningful where no leaf bounds the step (dev); with a
                // leaf the step is capped by the leaf's `pids.max` instead,
                // which does not count the fleet's tasks sharing the app uid.
                if cap_nproc {
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
    // A step that called `setsid`/`setpgid` escaped the group but not the
    // leaf: kill the whole leaf so nothing outlives the step, then remove
    // the leaf itself so stale per-step leaves do not pile up. Each step
    // needs its own leaf — a shared leaf would kill sibling steps here.
    if let Some(leaf) = sb.cgroup_leaf {
        kill_leaf(leaf);
        remove_step_leaf(leaf).await;
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
        let env: std::collections::HashMap<_, _> = base_env(cache).into_iter().collect();
        assert_eq!(env["PATH"], "/usr/local/bin:/usr/bin:/bin");
        assert_eq!(env["HOME"], "/data/cache/grocery/home");
        assert_eq!(env["BUN_INSTALL_CACHE_DIR"], "/data/cache/grocery/bun");
        assert_eq!(env["TMPDIR"], "/data/cache/grocery/tmp");
        assert_eq!(env["LANG"], "C.UTF-8");
        assert_eq!(env["NODE_ENV"], "production");
    }

    /// TMPDIR must survive deploys: `git clean` wipes the worktree (where
    /// `.tmp-sandbox` used to live, taking bun's node-compile-cache with
    /// it), but never the cache root.
    #[test]
    fn base_env_tmpdir_survives_a_worktree_wipe() {
        let cache = Path::new("/data/cache/grocery");
        let env: std::collections::HashMap<_, _> = base_env(cache).into_iter().collect();
        assert!(
            !env["TMPDIR"].starts_with("/data/work/"),
            "TMPDIR must not live in the worktree: {}",
            env["TMPDIR"]
        );
        assert_eq!(env["TMPDIR"], "/data/cache/grocery/tmp");
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
            platform_env: &[],
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
        let base = base_env(Path::new(""));
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
            platform_env: &[],
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
            platform_env: &[],
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
            platform_env: &[],
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
    #[test]
    fn cap_output_keeps_early_invalid_bytes() {
        // One invalid byte near the start plus valid output past the cap:
        // only a split tail char may be trimmed, the early byte becomes
        // U+FFFD instead of eating the whole megabyte.
        let mut output = vec![0xFF];
        output.extend(std::iter::repeat_n(b'x', MAX_OUTPUT_BYTES));
        let capped = cap_output(&output);
        assert!(
            capped.starts_with('\u{FFFD}'),
            "early invalid byte must survive as a replacement char"
        );
        assert!(
            capped.len() >= MAX_OUTPUT_BYTES,
            "must not trim back to the early invalid byte, got {} bytes",
            capped.len()
        );
    }

    #[test]
    fn cap_output_trims_only_a_split_tail_char() {
        // The cut splits `é`: only its first byte is dropped.
        let mut output = vec![b'x'; MAX_OUTPUT_BYTES - 1];
        output.extend_from_slice("é".as_bytes());
        output.extend_from_slice(b"tail");
        assert_eq!(cap_output(&output), "x".repeat(MAX_OUTPUT_BYTES - 1));
    }

    #[test]
    fn data_dir_is_traversable_but_platform_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(data.join("git/apps")).unwrap();
        std::fs::create_dir_all(data.join("work/hello")).unwrap();
        std::fs::write(data.join("work/hello.index"), "index").unwrap();
        std::fs::set_permissions(
            data.join("work/hello.index"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::write(data.join("outer.sqlite"), "db").unwrap();
        for p in [&data, &data.join("git"), &data.join("work")] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::set_permissions(
            data.join("outer.sqlite"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let old = unsafe { libc::umask(0o022) };
        prepare_data_dir(&data).unwrap();
        let after = unsafe { libc::umask(old) };
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&data), 0o711, "traversable, not listable");
        for root in APP_ROOTS {
            assert_eq!(mode(&data.join(root)), 0o711, "{root}");
        }
        assert_eq!(mode(&data.join("git")), 0o700);
        assert_eq!(mode(&data.join("outer.sqlite")), 0o600);
        assert_eq!(mode(&data.join("work/hello.index")), 0o600, "sidecars too");
        assert_eq!(after, 0o077, "later files start private");

        assert!(make_private_dir(&data.join("work/hello")));
        assert_eq!(mode(&data.join("work/hello")), 0o700);
        let link = data.join("work/link");
        std::os::unix::fs::symlink(data.join("git"), &link).unwrap();
        assert!(!make_private_dir(&link), "links are never followed");
        assert!(!make_private_dir(&data.join("work/missing")));
    }

    #[test]
    fn lchown_one_hands_over_without_following() {
        let dir = tempfile::tempdir().unwrap();
        let euid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let target = dir.path().join("outside");
        std::fs::write(&target, "platform").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // The link itself is handed over; the target keeps its owner.
        assert!(lchown_one(&link, euid, gid));
        assert!(lchown_one(&target, euid, gid));
        assert!(lchown_one(dir.path(), euid, gid));
        // Nothing exists here: a clean false, never a panic.
        assert!(!lchown_one(&dir.path().join("missing"), euid, gid));
    }

    #[test]
    fn kill_leaf_prefers_cgroup_kill() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cgroup.kill"), "").unwrap();
        kill_leaf(dir.path());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cgroup.kill")).unwrap(),
            "1"
        );
    }

    #[test]
    fn format_pid_renders_decimal_without_alloc() {
        let mut buf = [0u8; 20];
        for (pid, want) in [
            (0, "0"),
            (7, "7"),
            (42, "42"),
            (2069, "2069"),
            (u32::MAX, "4294967295"),
        ] {
            let len = format_pid(pid, &mut buf);
            assert_eq!(&buf[..len], want.as_bytes(), "pid {pid}");
        }
    }

    #[test]
    fn cgroup_procs_cstr_joins_the_leaf() {
        let leaf = Path::new("/sys/fs/cgroup/outer/builds/abc-build");
        assert_eq!(
            cgroup_procs_cstr(leaf).unwrap().as_bytes(),
            b"/sys/fs/cgroup/outer/builds/abc-build/cgroup.procs"
        );
    }

    /// A refused self-move fails closed (the spawn fails, so the step never
    /// runs uncapped): a missing leaf surfaces `NotFound`, not success.
    #[test]
    fn self_move_into_a_missing_leaf_fails_closed() {
        let err = self_move_into_leaf(c"/definitely/missing/cgroup.procs").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn kill_leaf_falls_back_to_procs() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the kill file would go: the write fails, so the
        // per-pid fallback runs. Dead pids (ESRCH) are fine.
        std::fs::create_dir(dir.path().join("cgroup.kill")).unwrap();
        std::fs::write(dir.path().join("cgroup.procs"), "999999991\n999999992\n").unwrap();
        kill_leaf(dir.path());
        // An unreadable leaf is success too: nothing is known to outlive.
        kill_leaf(&dir.path().join("missing"));
    }
    /// A step running in its own leaf is capped by the leaf's `pids.max`,
    /// not by the uid-wide RLIMIT_NPROC (which also counts the fleet's
    /// tasks sharing the app uid): the step's "Max processes" must not be
    /// 512. Only root drops to an app uid, which is when the cap applies.
    #[tokio::test]
    #[ignore = "needs root: RLIMIT_NPROC is only set when dropping to an app uid"]
    async fn build_step_with_a_leaf_keeps_the_system_nproc() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let leaf = dir.path().join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        let sb = Sandbox {
            uid: Some(200_000),
            gid: Some(200_000),
            platform_env: &[],
            env: &[],
            cache_root: &cache,
            cgroup_leaf: Some(&leaf),
        };
        let out = run_sandboxed(
            "cat",
            &["/proc/self/limits"],
            dir.path(),
            &sb,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        let max_processes = out
            .lines()
            .find(|line| line.starts_with("Max processes"))
            .expect("limits list Max processes");
        assert!(
            !max_processes.split_whitespace().any(|field| field == "512"),
            "leaf step must not carry the uid-wide 512 cap: {max_processes}"
        );
    }

    /// End to end: a step that detaches with `setsid` still dies with the
    /// step when the step runs in its own real leaf.
    #[tokio::test]
    #[ignore = "needs a writable cgroupfs mount plus cgroup delegation to make a real leaf"]
    async fn cgroup_kill_reaps_setsid_escapees() {
        let root = Path::new("/sys/fs/cgroup");
        let leaf = root.join(format!("outer-kill-test-{}", std::process::id()));
        std::fs::create_dir_all(&leaf).unwrap();
        struct Remove<'a>(&'a Path);
        impl Drop for Remove<'_> {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir(self.0);
            }
        }
        let _rm = Remove(&leaf);
        let marker = "31337999";
        let script = format!("setsid sleep {marker} & sleep 2");
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let sb = Sandbox {
            uid: None,
            gid: None,
            platform_env: &[],
            env: &[],
            cache_root: &cache,
            cgroup_leaf: Some(&leaf),
        };
        let _ = run_sandboxed(
            "sh",
            &["-c", &script],
            dir.path(),
            &sb,
            Duration::from_secs(20),
        )
        .await;
        // Whatever the step result, nothing detached may outlive it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while cmdline_has(marker) {
            if std::time::Instant::now() >= deadline {
                panic!("setsid-detached sleep {marker} survived the step");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// A symlinked `tmp` (an app-planted link to platform files) is replaced
    /// with a real dir: `mkdirat` relative to the pinned cache root never
    /// creates through the link, and the link's target is untouched.
    #[test]
    #[cfg(unix)]
    fn ensure_cache_subdirs_replaces_a_planted_link() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let marker = outside.path().join("platform.txt");
        std::fs::write(&marker, "platform").unwrap();
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::os::unix::fs::symlink(outside.path(), cache.join("tmp")).unwrap();
        ensure_cache_subdirs(&cache).unwrap();
        for sub in ["bun", "home", "tmp"] {
            assert!(
                std::fs::symlink_metadata(cache.join(sub))
                    .unwrap()
                    .file_type()
                    .is_dir(),
                "{sub} is a real dir"
            );
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "platform");
    }

    /// A planted regular file and an intact dir: the file is replaced, the
    /// dir (and its contents) is kept, and a missing root is created.
    #[test]
    #[cfg(unix)]
    fn ensure_cache_subdirs_keeps_real_dirs_replaces_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        ensure_cache_subdirs(&cache).unwrap();
        std::fs::write(cache.join("keep.txt"), "keep").unwrap();
        std::fs::remove_dir(cache.join("bun")).unwrap();
        std::fs::write(cache.join("bun"), "planted").unwrap();
        std::fs::write(cache.join("home").join("saved"), "saved").unwrap();
        ensure_cache_subdirs(&cache).unwrap();
        assert!(
            std::fs::symlink_metadata(cache.join("bun"))
                .unwrap()
                .file_type()
                .is_dir(),
            "planted file replaced with a real dir"
        );
        assert_eq!(
            std::fs::read_to_string(cache.join("home").join("saved")).unwrap(),
            "saved",
            "real dir contents are kept"
        );
        assert_eq!(
            std::fs::read_to_string(cache.join("keep.txt")).unwrap(),
            "keep"
        );
    }

    /// A cache root that is itself a link fails closed instead of creating
    /// through it.
    #[test]
    #[cfg(unix)]
    fn ensure_cache_subdirs_rejects_a_linked_root() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        std::fs::create_dir_all(&target).unwrap();
        let link = dir.path().join("linked");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        ensure_cache_subdirs(&link).unwrap_err();
        assert!(
            std::fs::symlink_metadata(link.join("bun")).is_err(),
            "nothing was created through the link"
        );
    }

    /// A killed step leaf is removed: no stale per-step leaves pile up.
    /// A plain dir stands in for the leaf (no cgroupfs needed). `kill_leaf`
    /// is not in this loop: on a fake leaf its `cgroup.kill` write leaves a
    /// real file behind which blocks `rmdir` (on cgroupfs interface files
    /// do not block removing an empty group); `kill_leaf` has its own tests.
    #[tokio::test]
    async fn remove_step_leaf_removes_the_killed_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = dir.path().join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        remove_step_leaf(&leaf).await;
        assert!(!leaf.exists(), "the step leaf is gone");
        // A missing leaf is success too (already reaped, or dev).
        remove_step_leaf(&leaf).await;
    }

    /// End to end on real cgroupfs: a step in its own leaf leaves no leaf
    /// behind (killed and removed).
    #[tokio::test]
    #[ignore = "needs a writable cgroupfs mount plus cgroup delegation to make a real leaf"]
    async fn step_in_a_real_leaf_leaves_no_leaf_behind() {
        let root = Path::new("/sys/fs/cgroup");
        let leaf = root.join(format!("outer-leaf-rm-test-{}", std::process::id()));
        std::fs::create_dir_all(&leaf).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let sb = Sandbox {
            uid: None,
            gid: None,
            platform_env: &[],
            env: &[],
            cache_root: &cache,
            cgroup_leaf: Some(&leaf),
        };
        let out = run_sandboxed("true", &[], dir.path(), &sb, Duration::from_secs(20)).await;
        assert!(out.is_ok(), "step in a real leaf succeeds: {out:?}");
        assert!(!leaf.exists(), "the step leaf was removed");
    }
}
