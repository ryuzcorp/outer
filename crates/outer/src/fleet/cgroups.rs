// Derived from noite apps/runner/src/host/cgroups.rs @ 58a977a (Apache-2.0).
// Outer change: always multi tenancy; +cpu controller; a build parent group
// with a low cpu weight plus per-build leaves; a platform leaf with
// memory.min for outer itself; no install-wide status or tenancy plumbing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;

/// Controllers Outer enables on the fleet subtree.
const CONTROLLERS: &str = "+memory +pids +cpu";

/// Cap on a fleet's tasks (processes and threads — the pids controller counts
/// both). Generous on purpose: celld runs isolates in-process and a busy node
/// holds hundreds of threads; the binding constraint is memory.
pub const FLEET_PIDS_MAX: u64 = 4096;

/// Small knobs whose exact numbers come from the plan table in M1; the shape
/// (bytes for memory.max, `"MAX PERIOD"` for cpu.max) is what matters here.
pub const FLEET_CPU_MAX_FREE: &str = "50000 100000";
pub const FLEET_CPU_MAX_PRO: &str = "100000 100000";

/// Everything `setup` needs to find and lay out, so tests can point `root`
/// at a temp dir faking cgroupfs files.
#[derive(Debug, Clone)]
pub struct CgroupConfig {
    /// cgroupfs mount (`/sys/fs/cgroup` in production).
    pub root: PathBuf,
    /// Anchor holding every Outer group (`outer`).
    pub anchor_name: String,
    /// Leaf the `outer` process itself runs in (`platform`).
    pub platform_leaf: String,
    /// Parent group bounding all builds (`builds`).
    pub build_parent: String,
    /// `memory.min` for the platform leaf, bytes.
    pub memory_min_bytes: u64,
}

impl Default for CgroupConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/sys/fs/cgroup"),
            anchor_name: "outer".to_string(),
            platform_leaf: "platform".to_string(),
            build_parent: "builds".to_string(),
            memory_min_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Per-fleet limits: `memory.max` in bytes (0 leaves the kernel default),
/// `pids.max` and `cpu.max` (`"MAX PERIOD"`, e.g. `"50000 100000"`).
#[derive(Debug, Clone)]
pub struct FleetLimits {
    pub memory_max_bytes: u64,
    pub pids_max: u64,
    pub cpu_max: String,
}

impl FleetLimits {
    pub fn new(memory_max_bytes: u64, cpu_max: &str) -> Self {
        Self {
            memory_max_bytes,
            pids_max: FLEET_PIDS_MAX,
            cpu_max: cpu_max.to_string(),
        }
    }
}

/// Resolve outer's own cgroup under `root` from its kernel placement: a
/// private cgroup namespace reports `/` (the `0::/` line) and the mount root
/// is already outer's cgroup; otherwise the path names the scope to anchor
/// under. Accepts the raw `0::/…` line too.
fn own_cgroup(root: &Path, self_cgroup: &str) -> PathBuf {
    let trimmed = self_cgroup.trim();
    let path = trimmed.strip_prefix("0::").unwrap_or(trimmed);
    if path.is_empty() || path == "/" {
        return root.to_path_buf();
    }
    root.join(path.trim_start_matches('/'))
}

/// This process's cgroup path (the `0::/…` v2 line of `/proc/self/cgroup`).
fn read_self_cgroup() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    raw.lines()
        .find_map(|line| line.strip_prefix("0::").map(str::to_string))
}

/// Write one cgroup interface file. The OS error stays on the chain
/// (`with_context`, never a formatted message): the boot loop classifies a
/// contested enable off `EBUSY`, and any formatting here would turn that
/// classifier into dead code.
fn write_file(dir: &Path, name: &str, value: &str) -> anyhow::Result<()> {
    std::fs::write(dir.join(name), value)
        .with_context(|| format!("write {name} in {}", dir.display()))?;
    Ok(())
}

/// Write a cgroup limit and read it back: the kernel refuses what it cannot
/// apply, and a silent no-op (read-only mount after setup, a clamped value)
/// must surface here instead of running uncapped.
fn write_limit(dir: &Path, name: &str, value: &str) -> anyhow::Result<()> {
    write_file(dir, name, value)?;
    let applied = std::fs::read_to_string(dir.join(name))
        .map(|contents| contents.trim().to_string())
        .unwrap_or_default();
    if applied != value {
        anyhow::bail!("{name} did not apply (wrote {value}, have {applied:?})");
    }
    Ok(())
}

/// The controllers (`memory`, `pids`, `cpu`) are enabled on `cgroup`: a write
/// the kernel ignores (read-only mount, no delegation) reads back empty.
fn controllers_enabled(cgroup: &Path) -> anyhow::Result<()> {
    let enabled = std::fs::read_to_string(cgroup.join("cgroup.subtree_control"))
        .with_context(|| format!("read cgroup.subtree_control in {}", cgroup.display()))?;
    for want in ["memory", "pids", "cpu"] {
        if !enabled
            .split_whitespace()
            .any(|token| token.trim_start_matches('+') == want)
        {
            anyhow::bail!("controller {want} not enabled (have {enabled:?})");
        }
    }
    Ok(())
}

/// Enable the fleet controllers on `cgroup` and confirm they took.
fn enable_controllers(cgroup: &Path) -> anyhow::Result<()> {
    write_file(cgroup, "cgroup.subtree_control", CONTROLLERS)?;
    controllers_enabled(cgroup)
}

/// Move every member process of `cgroup` into `leaf`, best-effort per pid: a
/// member that exits between the listing and the move must not fail the
/// drain.
fn drain_to_leaf(cgroup: &Path, leaf: &Path) -> anyhow::Result<()> {
    let members = std::fs::read_to_string(cgroup.join("cgroup.procs"))
        .with_context(|| format!("read cgroup.procs in {}", cgroup.display()))?;
    for pid in members.split_whitespace() {
        if std::fs::write(leaf.join("cgroup.procs"), pid).is_err() {
            // An exit racing the move lands here as ESRCH; the drain check
            // below re-reads, so a stale entry never passes silently.
            tracing::debug!(pid, "cgroup move raced away");
        }
    }
    Ok(())
}

/// Whether `error` is the kernel refusing a contested write (EBUSY): a
/// member landed between the drain and the enable. Only that case is worth
/// another round; every other failure fails the same way every attempt.
fn is_busy(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| io.raw_os_error() == Some(libc::EBUSY))
}

/// This process's cgroup path is free of live member processes, so
/// controllers may be enabled. Entries that raced away do not count.
#[cfg(unix)]
fn is_drained(cgroup: &Path) -> bool {
    let members = std::fs::read_to_string(cgroup.join("cgroup.procs")).unwrap_or_default();
    !members.split_whitespace().any(blocks_drain)
}

/// Does `pid` block enabling controllers: alive and not a zombie? Missing or
/// unparseable state reads as gone — the pid raced away between the listing
/// and this read.
#[cfg(unix)]
fn blocks_drain(pid: &str) -> bool {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // `/proc/<pid>/stat`: `pid (comm) state …` — comm may hold spaces and
    // parens, so the state is the first field after the last `)`.
    let state = stat.rfind(')').map(|end| stat[end + 1..].trim_start());
    match state.and_then(|rest| rest.chars().next()) {
        None => false,
        Some('Z') => false,
        Some(_) => true,
    }
}

#[cfg(not(unix))]
fn is_drained(_cgroup: &Path) -> bool {
    true
}

/// Remount `root` read-write, preserving its other flags: Docker mounts the
/// container's cgroupfs read-only, and the multi compose override grants
/// `CAP_SYS_ADMIN` exactly so outer can flip its own cgroup subtree
/// writable. Without the grant this fails and setup degrades loudly.
/// Linux-only.
#[cfg(target_os = "linux")]
fn remount_rw(root: &Path) -> anyhow::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let options = mount_options(root).ok_or_else(|| {
        anyhow::anyhow!(
            "remount needs a cgroupfs mount: no /proc/self/mounts entry for {}",
            root.display()
        )
    })?;
    let target = CString::new(root.as_os_str().as_bytes())
        .map_err(|e| anyhow::anyhow!("cgroup root is not a C string: {e}"))?;
    let mut flags: libc::c_ulong = libc::MS_REMOUNT;
    // A remount replaces the flag set, so every flag worth keeping is
    // restated; only the read-only bit is dropped. Unknown options are the
    // filesystem's own (seclabel, nsdelegate) and need no restating.
    for option in options.split(',') {
        flags |= match option {
            "nosuid" => libc::MS_NOSUID,
            "nodev" => libc::MS_NODEV,
            "noexec" => libc::MS_NOEXEC,
            "relatime" => libc::MS_RELATIME,
            "strictatime" => libc::MS_STRICTATIME,
            "noatime" => libc::MS_NOATIME,
            "nodiratime" => libc::MS_NODIRATIME,
            "sync" => libc::MS_SYNCHRONOUS,
            "dirsync" => libc::MS_DIRSYNC,
            _ => 0,
        };
    }
    // SAFETY: null source/type/data with MS_REMOUNT only flips the flags of
    // this mount (a bind inside the container), touching nothing else.
    let rc = unsafe {
        libc::mount(
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(anyhow::anyhow!(
            "remount {} read-write: {}",
            root.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// This mount's option list from `/proc/self/mounts` (`source target fstype
/// options …`), so a remount can restate the flags it keeps. Octal escapes
/// (`\040`) decode to their byte; the comparison target has none, but a
/// container root under a spaced path must still match.
#[cfg(target_os = "linux")]
fn mount_options(root: &Path) -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/self/mounts").ok()?;
    find_mount_options(&mounts, root)
}

/// The option list for `root` in one `/proc/self/mounts` dump (`source
/// target fstype options …`). Pure so tests can pin the parsing.
#[cfg(target_os = "linux")]
fn find_mount_options(mounts: &str, root: &Path) -> Option<String> {
    let want = root.to_string_lossy();
    mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        fields.next()?;
        let target = fields.next()?.replace("\\040", " ");
        fields.next()?;
        let options = fields.next()?;
        (target.as_str() == want.as_ref()).then(|| options.to_string())
    })
}

/// The container's cgroupfs mount may be read-only: probe with a throwaway
/// directory, remount once when the probe says read-only, and bail with the
/// operator fix when the grant is missing.
fn ensure_writable(root: &Path, own: &Path) -> anyhow::Result<()> {
    let probe = own.join(".writable");
    if std::fs::create_dir(&probe).is_ok() {
        let _ = std::fs::remove_dir(&probe);
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if remount_rw(root).is_ok() && std::fs::create_dir(&probe).is_ok() {
        let _ = std::fs::remove_dir(&probe);
        tracing::info!(root = %root.display(), "cgroupfs remounted read-write");
        return Ok(());
    }
    Err(anyhow::anyhow!("cgroupfs is not writable"))
        .with_context(|| format!("remount {} refused", root.display()))
}

/// Set up the fleet anchor under `root`: capture outer's own cgroup BEFORE
/// moving (its path changes with the move), drain every member into the
/// platform leaf, enable the controllers on the way down, move this process
/// into the platform leaf, and apply `memory.min` there. Returns the anchor
/// on success (`None` when cgroups are unavailable and fleets share the
/// container cgroup): the `ensure_*` helpers need that path.
pub fn setup(cfg: &CgroupConfig) -> Option<PathBuf> {
    let placement = read_self_cgroup().unwrap_or_default();
    match setup_inner(cfg, &placement, std::process::id()) {
        Ok(anchor) => {
            tracing::info!(anchor = %anchor.display(), "per-fleet cgroups active");
            Some(anchor)
        }
        Err(e) => {
            tracing::warn!(
                "per-fleet cgroups unavailable ({e:#}); fleets share the container cgroup"
            );
            None
        }
    }
}

pub fn setup_inner(cfg: &CgroupConfig, placement: &str, pid: u32) -> anyhow::Result<PathBuf> {
    if !cfg.root.join("cgroup.controllers").is_file() {
        anyhow::bail!(
            "{:?} is not a cgroup v2 mount (no cgroup.controllers)",
            cfg.root
        );
    }
    let own = own_cgroup(&cfg.root, placement);
    if !own.is_dir() {
        anyhow::bail!("own cgroup {} missing under {:?}", own.display(), cfg.root);
    }
    ensure_writable(&cfg.root, &own)?;
    // Already inside an anchor's platform leaf: a second outer process in the
    // same container (`docker exec outer app deploy …` while `serve` runs,
    // which runc places in PID 1's cgroup) or a restarted serve. Reuse that
    // anchor as is: anchoring again would nest a new anchor under the
    // platform leaf and drain the running serve into it.
    if let Some(anchor) = existing_anchor(&own, cfg) {
        for dir in [
            &anchor.join(&cfg.platform_leaf),
            &anchor.join(&cfg.build_parent),
        ] {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        self_check(&anchor)?;
        return Ok(anchor);
    }
    let anchor = own.join(&cfg.anchor_name);
    let platform = anchor.join(&cfg.platform_leaf);
    let builds = anchor.join(&cfg.build_parent);
    for dir in [&anchor, &platform, &builds] {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // Fleet groups are world-traversable like the leaves below them; their
    // names and integer limits are operational, not secrets. The platform
    // leaf itself stays private.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&anchor, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", anchor.display()))?;
    }
    // The drain-and-enable below retries a contested enable: everything outer
    // forks after this point inherits the platform leaf, so the only racers
    // left are external (healthcheck execs joining the container cgroup),
    let mut last_busy: Option<anyhow::Error> = None;
    let mut enabled = false;
    for _ in 0..10 {
        drain_to_leaf(&own, &platform)?;
        if is_drained(&own) {
            match enable_controllers(&own) {
                Ok(()) => {
                    enabled = true;
                    break;
                }
                // EBUSY here is a member that landed between the drain and
                // the write — worth another round. Anything else fails the
                // same way every attempt: bail at once.
                Err(e) if is_busy(&e) => {
                    last_busy = Some(e);
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("enabling {}", own.display()));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !enabled {
        if let Some(e) = last_busy {
            return Err(e).with_context(|| format!("enabling {}", own.display()));
        }
        anyhow::bail!(
            "{} still holds live processes after the drain",
            own.display()
        );
    }
    enable_controllers(&anchor).with_context(|| format!("enabling {}", anchor.display()))?;
    // Outer itself runs in its own leaf from here on, so the kernel reclaims
    // from tenants first.
    std::fs::write(platform.join("cgroup.procs"), pid.to_string())
        .with_context(|| format!("move outer into {}", platform.display()))?;
    if cfg.memory_min_bytes > 0 {
        write_limit(&platform, "memory.min", &cfg.memory_min_bytes.to_string())?;
    }
    self_check(&anchor)?;
    Ok(anchor)
}

/// The anchor `own` already sits in, when `own` is `<anchor>/<platform_leaf>`
/// with the configured names.
fn existing_anchor(own: &Path, cfg: &CgroupConfig) -> Option<PathBuf> {
    if own.file_name()? != cfg.platform_leaf.as_str() {
        return None;
    }
    let anchor = own.parent()?;
    if anchor.file_name()? != cfg.anchor_name.as_str() || !anchor.starts_with(&cfg.root) {
        return None;
    }
    Some(anchor.to_path_buf())
}

/// Fail boot when the anchor cannot carry its limits: `/ready` answers 503
/// and nothing gets scheduled.
pub fn self_check(anchor: &Path) -> anyhow::Result<()> {
    controllers_enabled(anchor)
        .with_context(|| format!("cgroup self-check on {}", anchor.display()))
}

/// Directory name for a fleet key (`slug`): keys are slug-shaped by
/// construction, but a stray separator must never escape the anchor. `.`
/// maps to `_`, so the result is never `.` or `..`; anything else outside
/// `[0-9A-Za-z-_]` (including `/`) maps to `_` too, and an empty result
/// becomes `"_"`.
pub fn fleet_dir_name(key: &str) -> String {
    let mapped: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if mapped.is_empty() {
        "_".to_string()
    } else {
        mapped
    }
}

/// Create (or reuse) a fleet's cgroup leaf with its limits and return its
/// path. Reuse after a crash is safe: the limits are rewritten. `0` memory
/// leaves the kernel default.
pub fn ensure_fleet_leaf(
    anchor: &Path,
    key: &str,
    limits: &FleetLimits,
) -> anyhow::Result<PathBuf> {
    let dir = anchor.join(fleet_dir_name(key));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    }
    if limits.memory_max_bytes > 0 {
        write_limit(&dir, "memory.max", &limits.memory_max_bytes.to_string())?;
        // At the cap the kernel must OOM-kill inside the leaf, not swap the
        // fleet out. `memory.swap.max` is absent where the host has no swap
        // accounting — then there is no swap to escape into and the leaf is
        // contained without it.
        if dir.join("memory.swap.max").is_file() {
            write_limit(&dir, "memory.swap.max", "0")?;
        }
    }
    write_limit(&dir, "pids.max", &limits.pids_max.to_string())?;
    write_limit(&dir, "cpu.max", &limits.cpu_max)?;
    Ok(dir)
}

/// The parent group bounding every build: a shared `memory.max` and a low
/// `cpu.weight`, so builds as a whole cannot starve the fleets.
pub fn ensure_build_parent(
    anchor: &Path,
    cfg: &CgroupConfig,
    memory_max_bytes: u64,
    cpu_weight: u64,
) -> anyhow::Result<PathBuf> {
    let dir = anchor.join(&cfg.build_parent);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    }
    write_limit(&dir, "memory.max", &memory_max_bytes.to_string())?;
    write_limit(&dir, "cpu.weight", &cpu_weight.to_string())?;
    // Build leaves sit one level down: without the controllers enabled here
    // they have no memory.max / pids.max files to write.
    enable_controllers(&dir).with_context(|| format!("enabling {}", dir.display()))?;
    Ok(dir)
}

/// Cap on a build leaf's tasks: the fork-bomb cap the uid-wide RLIMIT_NPROC
/// used to provide before build and fleet shared one app uid. A leaf-level
/// cap does not count the fleet's tasks, so a busy fleet can never starve a
/// build with EAGAIN.
pub const BUILD_PIDS_MAX: u64 = 512;

/// One build's own leaf under the build parent, carrying its own
/// `memory.max` and `pids.max`.
pub fn ensure_build_leaf(
    parent: &Path,
    build_id: &str,
    memory_max_bytes: u64,
) -> anyhow::Result<PathBuf> {
    let dir = parent.join(fleet_dir_name(build_id));
    std::fs::create_dir_all(&dir)?;
    write_limit(&dir, "memory.max", &memory_max_bytes.to_string())?;
    write_limit(&dir, "pids.max", &BUILD_PIDS_MAX.to_string())?;
    Ok(dir)
}

/// The leaf `outer` itself runs in (created by `setup_inner`), carrying
/// `memory.min` so the kernel reclaims from tenants first.
pub fn ensure_platform_leaf(
    anchor: &Path,
    cfg: &CgroupConfig,
    memory_min_bytes: u64,
) -> anyhow::Result<PathBuf> {
    let dir = anchor.join(&cfg.platform_leaf);
    std::fs::create_dir_all(&dir)?;
    write_limit(&dir, "memory.min", &memory_min_bytes.to_string())?;
    Ok(dir)
}

/// Current `oom_kill` count of a group: how many times the kernel OOM-killed
/// inside it. Unreadable (no cgroups, already removed) reads as zero.
pub fn oom_kill_count(dir: &Path) -> u64 {
    let Ok(events) = std::fs::read_to_string(dir.join("memory.events")) else {
        return 0;
    };
    for line in events.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some("oom_kill") {
            if let Some(count) = parts.next().and_then(|count| count.parse().ok()) {
                return count;
            }
        }
    }
    0
}

/// Move a process into a group; its children inherit it, so the whole fleet
/// (or build) is contained together.
pub fn enter_cgroup(dir: &Path, pid: u32) -> anyhow::Result<()> {
    write_file(dir, "cgroup.procs", &pid.to_string())
}

/// Drop a stopped fleet's group, returning its high-water RSS first: the
/// removal deletes the leaf (and with it `memory.peak`), so a later read
/// could never recover the number. Best-effort: only an empty group removes.
pub fn remove_fleet_cgroup(dir: &Path) -> Option<u64> {
    let peak = fleet_peak_bytes(dir);
    if std::fs::remove_dir(dir).is_err() {
        tracing::debug!(dir = %dir.display(), "fleet cgroup kept (still populated or gone)");
    }
    peak
}

/// High-water RSS of a group in bytes (`memory.peak`).
pub fn fleet_peak_bytes(dir: &Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("memory.peak"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cgroup.controllers"), "memory pids cpu\n").unwrap();
        std::fs::write(dir.path().join("cgroup.procs"), "").unwrap();
        std::fs::write(dir.path().join("cgroup.subtree_control"), "").unwrap();
        dir
    }

    fn test_config(root: &Path) -> CgroupConfig {
        CgroupConfig {
            root: root.to_path_buf(),
            anchor_name: "outer".to_string(),
            platform_leaf: "platform".to_string(),
            build_parent: "builds".to_string(),
            memory_min_bytes: 256 * 1024 * 1024,
        }
    }

    #[test]
    fn setup_moves_outer_into_its_leaf_and_enables_controllers() {
        let dir = fake_root();
        let cfg = test_config(dir.path());
        let pid = std::process::id();
        let anchor = setup_inner(&cfg, "0::/", pid).unwrap();
        assert_eq!(anchor, dir.path().join("outer"));
        // Controllers enabled on the way down.
        for group in [&anchor, dir.path()] {
            let enabled = std::fs::read_to_string(group.join("cgroup.subtree_control")).unwrap();
            for want in ["memory", "pids", "cpu"] {
                assert!(
                    enabled
                        .split_whitespace()
                        .any(|token| token.trim_start_matches('+') == want),
                    "{want} missing in {}",
                    group.display()
                );
            }
        }
        // Outer itself sits in the platform leaf with memory.min applied.
        let platform = anchor.join("platform");
        let procs = std::fs::read_to_string(platform.join("cgroup.procs")).unwrap();
        assert!(procs
            .split_whitespace()
            .any(|entry| entry == pid.to_string()));
        assert_eq!(
            std::fs::read_to_string(platform.join("memory.min"))
                .unwrap()
                .trim(),
            (256u64 * 1024 * 1024).to_string()
        );
        self_check(&anchor).unwrap();
    }

    /// A second outer process placed in the platform leaf (a CLI command via
    /// `docker exec` while serve runs) reuses the anchor: no nested anchor,
    /// and the serve process is not drained anywhere.
    #[test]
    fn setup_inside_the_platform_leaf_reuses_the_anchor() {
        let dir = fake_root();
        let cfg = test_config(dir.path());
        let serve_pid = std::process::id();
        let anchor = setup_inner(&cfg, "0::/", serve_pid).unwrap();
        let platform = anchor.join("platform");
        let before = std::fs::read_to_string(platform.join("cgroup.procs")).unwrap();

        let again = setup_inner(&cfg, "0::/outer/platform", serve_pid + 1).unwrap();
        assert_eq!(again, anchor);
        assert!(!platform.join("outer").exists(), "no nested anchor");
        assert_eq!(
            std::fs::read_to_string(platform.join("cgroup.procs")).unwrap(),
            before,
            "nothing moved out of (or into) the platform leaf"
        );
        // A same-named group elsewhere is not an anchor.
        let elsewhere = dir.path().join("other").join("platform");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert!(existing_anchor(&elsewhere, &cfg).is_none());
    }

    #[test]
    fn fleet_leaf_carries_memory_swap_pids_and_cpu() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("outer");
        std::fs::create_dir_all(&anchor).unwrap();
        // Swap accounting present: capped at zero alongside memory.max.
        let limits = FleetLimits::new(512 * 1024 * 1024, FLEET_CPU_MAX_FREE);
        let leaf = ensure_fleet_leaf(&anchor, "grocery", &limits).unwrap();
        assert_eq!(
            std::fs::read_to_string(leaf.join("memory.max"))
                .unwrap()
                .trim(),
            (512u64 * 1024 * 1024).to_string()
        );
        assert_eq!(
            std::fs::read_to_string(leaf.join("pids.max"))
                .unwrap()
                .trim(),
            "4096"
        );
        assert_eq!(
            std::fs::read_to_string(leaf.join("cpu.max"))
                .unwrap()
                .trim(),
            FLEET_CPU_MAX_FREE
        );
        // No swap.max file: contained without it, still Ok.
        assert!(!leaf.join("memory.swap.max").exists());

        std::fs::write(leaf.join("memory.swap.max"), "").unwrap();
        ensure_fleet_leaf(&anchor, "grocery", &limits).unwrap();
        assert_eq!(
            std::fs::read_to_string(leaf.join("memory.swap.max"))
                .unwrap()
                .trim(),
            "0"
        );

        // Pro plans get a full CPU.
        let pro = FleetLimits::new(1024 * 1024 * 1024, FLEET_CPU_MAX_PRO);
        let pro_leaf = ensure_fleet_leaf(&anchor, "grocery-pro", &pro).unwrap();
        assert_eq!(
            std::fs::read_to_string(pro_leaf.join("cpu.max"))
                .unwrap()
                .trim(),
            FLEET_CPU_MAX_PRO
        );

        // A stray separator never escapes the anchor.
        let evil = ensure_fleet_leaf(&anchor, "../evil", &limits).unwrap();
        assert_eq!(evil, anchor.join("___evil"));
    }

    #[test]
    fn fleet_dir_name_never_escapes_the_anchor() {
        assert_eq!(fleet_dir_name("grocery"), "grocery");
        assert_eq!(fleet_dir_name("grocery-pro"), "grocery-pro");
        assert_eq!(fleet_dir_name(".."), "__");
        assert_eq!(fleet_dir_name("."), "_");
        assert_eq!(fleet_dir_name(""), "_");
        assert_eq!(fleet_dir_name("a/b"), "a_b");
        assert_eq!(fleet_dir_name("../../etc"), "______etc");
        // Whatever the input, the mapped name stays inside the anchor.
        for name in ["..", ".", "", "a/b", "a..b", ".hidden", "a b"] {
            let mapped = fleet_dir_name(name);
            assert!(!mapped.is_empty(), "{name:?} must not map empty");
            assert!(!mapped.contains('/'), "{name:?} must not keep a separator");
            assert!(
                mapped != "." && mapped != "..",
                "{name:?} must not stay dotty"
            );
            let anchor = Path::new("/anchor");
            assert_eq!(
                anchor.join(&mapped).parent(),
                Some(anchor),
                "{mapped:?} escapes"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_options_picks_this_mount() {
        let mounts = "cgroup2 /sys/fs/cgroup cgroup2 ro,nosuid,nodev,noexec,relatime,nsdelegate 0 0\ntmpfs /run tmpfs rw,nosuid,nodev 0 0\n";
        assert_eq!(
            find_mount_options(mounts, Path::new("/sys/fs/cgroup")),
            Some("ro,nosuid,nodev,noexec,relatime,nsdelegate".to_string())
        );
        assert_eq!(
            find_mount_options(mounts, Path::new("/run")),
            Some("rw,nosuid,nodev".to_string())
        );
        assert_eq!(find_mount_options(mounts, Path::new("/missing")), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_options_decodes_escaped_spaces() {
        let mounts = "cgroup2 /mnt/my\\040cgroup cgroup2 rw,nosuid 0 0\n";
        assert_eq!(
            find_mount_options(mounts, Path::new("/mnt/my cgroup")),
            Some("rw,nosuid".to_string())
        );
    }

    #[test]
    fn build_parent_and_leaf_bound_builds() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("outer");
        std::fs::create_dir_all(&anchor).unwrap();
        let cfg = test_config(dir.path());
        let parent = ensure_build_parent(&anchor, &cfg, 2 * 1024 * 1024 * 1024, 10).unwrap();
        assert_eq!(parent, anchor.join("builds"));
        controllers_enabled(&parent).expect("build leaves need the parent's controllers");
        assert_eq!(
            std::fs::read_to_string(parent.join("memory.max"))
                .unwrap()
                .trim(),
            (2u64 * 1024 * 1024 * 1024).to_string()
        );
        assert_eq!(
            std::fs::read_to_string(parent.join("cpu.weight"))
                .unwrap()
                .trim(),
            "10"
        );
        let leaf = ensure_build_leaf(&parent, "build-1", 1024 * 1024 * 1024).unwrap();
        assert_eq!(
            std::fs::read_to_string(leaf.join("memory.max"))
                .unwrap()
                .trim(),
            (1024u64 * 1024 * 1024).to_string()
        );
        // The leaf caps the build's own tasks (not the uid-wide count, which
        // includes the fleet sharing the app uid).
        assert_eq!(
            std::fs::read_to_string(leaf.join("pids.max"))
                .unwrap()
                .trim(),
            BUILD_PIDS_MAX.to_string()
        );
    }

    #[test]
    fn oom_kills_read_from_memory_events() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("memory.events"),
            "low 0\nhigh 1\nmax 2\noom 0\noom_kill 3\n",
        )
        .unwrap();
        assert_eq!(oom_kill_count(dir.path()), 3);
        // No cgroups (already removed): zero, never an error.
        assert_eq!(oom_kill_count(Path::new("/nonexistent-leaf")), 0);
    }

    #[test]
    #[ignore = "needs a writable cgroupfs mount and CAP_SYS_ADMIN"]
    fn setup_on_real_cgroupfs() {
        assert!(setup(&CgroupConfig::default()).is_some());
    }
}
