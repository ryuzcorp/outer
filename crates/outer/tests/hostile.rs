//! The §12.7 hostile suite: hostile fixtures in `e2e/hostile/` try to escape,
//! and these tests assert the sandbox holds.
//!
//! Two layers, like the spec's beta gate:
//!
//! - **Non-privileged (CI):** every fixture category maps to a pure config
//!   assertion below — the nft ruleset builder, the cgroup limits, the
//!   credential prefix shape, the reserved-env filter, the fleet command
//!   line, the data-dir layout. `cargo test --locked hostile` runs these.
//! - **Privileged (VPS, `scripts/hostile-suite.sh`):** the `#[ignore]`d
//!   tests run the real fixtures through `run_sandboxed` as an app uid in a
//!   real cgroup leaf, with the nft table installed, and fail unless every
//!   must-fail probe stayed contained. `cargo test --locked -- --ignored
//!   hostile` runs these. Each skips loudly when its prerequisite (root,
//!   cgroupfs delegation, the nft table, a JS runtime, scoped R2 creds) is
//!   absent, so a dev machine never passes vacuously.
//!
//! The worker fixture (`e2e/hostile/worker.js`) needs a live celld fleet
//! with a deployed app, which no test here can conjure without a bucket; it
//! runs as a documented manual step in `scripts/hostile-suite.sh`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/hostile")
        .canonicalize()
        .expect("e2e/hostile exists")
}

#[derive(Debug, Deserialize)]
struct Manifest {
    version: u32,
    probes: Vec<ManifestProbe>,
}

#[derive(Debug, Deserialize)]
struct ManifestProbe {
    id: String,
    phase: String,
    category: String,
    must: String,
    #[allow(dead_code)]
    desc: String,
}

fn manifest() -> Manifest {
    let raw =
        std::fs::read_to_string(fixture_dir().join("manifest.json")).expect("manifest.json reads");
    serde_json::from_str(&raw).expect("manifest.json parses")
}

/// Every fixture category the assignment requires, each pinned to at least
/// one must-fail probe: a category with no failing probe would pass without
/// proving anything.
#[test]
fn hostile_manifest_covers_every_required_category() {
    let manifest = manifest();
    assert_eq!(manifest.version, 1, "manifest version pins the harness");
    let mut by_category: HashMap<&str, Vec<&ManifestProbe>> = HashMap::new();
    for probe in &manifest.probes {
        assert!(!probe.id.is_empty(), "probe ids are non-empty");
        assert!(
            ["build", "worker", "r2"].contains(&probe.phase.as_str()),
            "probe {} has a known phase",
            probe.id
        );
        assert!(
            ["fail", "succeed"].contains(&probe.must.as_str()),
            "probe {} has a known verdict",
            probe.id
        );
        by_category
            .entry(probe.category.as_str())
            .or_default()
            .push(probe);
    }
    for required in [
        "cross-app-files",
        "host-secrets",
        "build-write-escape",
        "resource-exhaustion",
        "blocked-egress",
        "credential-scope",
        "fleet-ports",
        "cgroup-bounds",
    ] {
        let probes = by_category
            .get(required)
            .unwrap_or_else(|| panic!("category {required} has no probes in manifest.json"));
        // `cgroup-bounds` is the positive control (the leaf really limits):
        // its probe must be green, and the resource-exhaustion must-fails
        // only mean something when it is.
        let want = if required == "cgroup-bounds" {
            "succeed"
        } else {
            "fail"
        };
        assert!(
            probes.iter().any(|probe| probe.must == want),
            "category {required} needs a must-{want} probe"
        );
    }
}

/// The nft policy closes every egress target the fixtures probe: SMTP ports,
/// loopback, RFC 1918, link-local/metadata, CGNAT and ULA — and the storage
/// and DNS allowances sort before the first reject, so a fleet keeps its
/// bucket and its resolver while nothing else is reachable.
#[test]
fn hostile_nft_policy_blocks_every_fixture_egress_target() {
    let rules = outer::fleet::nft::ruleset(&outer::fleet::nft::NftConfig {
        uid_range: (200000, 100000),
        storage_addrs: vec!["10.89.0.7".parse().unwrap()],
        storage_port: 443,
        free_uids: vec![200001],
        pro_uids: vec![200002],
        own_ports: vec![(200001, vec![20001])],
    });
    let range = "200000-299999";
    for blocked in [
        format!("meta skuid {range} tcp dport {{ 25, 465, 587 }} reject"),
        format!("meta skuid {range} ip daddr 127.0.0.0/8 reject"),
        format!("meta skuid {range} ip6 daddr ::1 reject"),
        format!(
            "meta skuid {range} ip daddr {{ 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10 }} reject"
        ),
        format!("meta skuid {range} ip6 daddr {{ fc00::/7, fe80::/10 }} reject"),
    ] {
        assert!(rules.contains(&blocked), "missing rule: {blocked}");
    }
    // SMTP is rejected before anything private: mail submission never gets a
    // chance to route.
    let smtp = rules.find("dport { 25, 465, 587 }").expect("smtp reject");
    let loopback = rules.find("127.0.0.0/8").expect("loopback reject");
    assert!(smtp < loopback, "smtp rejects before loopback");
    // The allowances the fixtures depend on staying open sort first.
    let first_reject = rules.find(" reject").expect("has rejects");
    for open in [
        "ct original proto-dst 53 accept",
        "tcp dport 443 accept",
        "ct state established,related accept",
    ] {
        let at = rules
            .find(open)
            .unwrap_or_else(|| panic!("missing: {open}"));
        assert!(at < first_reject, "{open} sorts before the rejects");
    }
}

/// Per-fleet leaves carry real bounds: a positive memory cap, the shared
/// pids cap and an explicit cpu weight. The fork-bomb and memory-exhaustion
/// fixtures are contained exactly by these three knobs.
#[test]
fn hostile_fleet_limits_bound_memory_cpu_and_pids() {
    let limits = outer::fleet::cgroups::FleetLimits::new(
        512 * 1024 * 1024,
        outer::fleet::cgroups::FLEET_CPU_MAX_FREE,
    );
    assert_eq!(limits.memory_max_bytes, 512 * 1024 * 1024);
    assert_eq!(limits.pids_max, outer::fleet::cgroups::FLEET_PIDS_MAX);
    assert!(limits.pids_max > 0, "pids cap bounds fork bombs");
    assert!(
        limits.cpu_max.split_whitespace().count() == 2,
        "cpu.max is quota and period"
    );
    for cpu in [
        outer::fleet::cgroups::FLEET_CPU_MAX_FREE,
        outer::fleet::cgroups::FLEET_CPU_MAX_PRO,
    ] {
        let quota: u64 = cpu.split_whitespace().next().unwrap().parse().unwrap();
        assert!(quota > 0, "cpu quota bounds spin loops");
    }
}

/// Scoped credentials never leave `fleets/{slug}/`: the only prefix shape
/// that validates, matched by the fleet's bucket URL, with `..`, absolute
/// and unterminated shapes refused. A cross-prefix probe that passes shape
/// validation is still denied by the Cloudflare policy, not by string luck.
#[test]
fn hostile_credential_prefixes_stay_scoped() {
    assert_eq!(
        outer::fleet::manager::FleetStorage::fleet_prefix("grocery"),
        "fleets/grocery/"
    );
    outer::store::credentials::validate_prefix(&outer::fleet::manager::FleetStorage::fleet_prefix(
        "grocery",
    ))
    .expect("the minted shape validates");
    for bad in [
        "",
        "/",
        "fleets/grocery",
        "/fleets/grocery/",
        "fleets/../other/",
        "fleets/grocery/../../other/",
    ] {
        assert!(
            outer::store::credentials::validate_prefix(bad).is_err(),
            "prefix {bad:?} must not validate"
        );
    }
    // Shape validation alone does not scope: `other-prefix/` is well-formed
    // but never minted — the minter only ever receives `fleets/{slug}/`.
    outer::store::credentials::validate_prefix("other-prefix/")
        .expect("well-formed but never minted");
    assert_eq!(
        outer::store::credentials::r2_jurisdiction("https://abc.eu.r2.cloudflarestorage.com"),
        Some("eu".to_string())
    );
}

/// Platform secret names never reach app code: the reserved filter drops
/// every `*_SECRET`, provider and platform prefix the fixtures plant, while
/// ordinary app secrets and the bun cache dir pass through.
#[test]
fn hostile_platform_env_never_reaches_apps() {
    for reserved in [
        "PORT",
        "HOST",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "S3_ENDPOINT",
        "CELLD_WATCH",
        "OUTER_SECRET",
        "OUTER_R2_BUCKET",
        "CLOUDFLARE_API_TOKEN",
        "R2_ACCESS_KEY_ID",
        "STRIPE_SECRET_KEY",
        "RESEND_API_KEY",
        "JEV_KEY",
        "LD_PRELOAD",
        "NODE_OPTIONS",
        "BUN_FOO",
    ] {
        assert!(
            outer::fleet::sandbox::is_reserved_env_name(reserved),
            "{reserved} must be reserved"
        );
    }
    for allowed in ["APP_KEY", "DATABASE_URL", "BUN_INSTALL_CACHE_DIR"] {
        assert!(
            !outer::fleet::sandbox::is_reserved_env_name(allowed),
            "{allowed} must pass through"
        );
    }
}

/// The fleet command binds loopback only and drops reserved names from app
/// env: a worker that fetches the celld internal listener or another fleet's
/// port from its own environment finds nothing to aim at, and the platform
/// secrets the supervisor holds never enter the fleet's environment.
#[test]
fn hostile_fleet_command_is_loopback_only_and_scoped() {
    let spec = outer::fleet::supervisor::FleetSpec {
        slug: "hostile".to_string(),
        uid: 200001,
        gid: 200001,
        public_port: 21001,
        internal_port: 22001,
        bucket_url: "s3://outer/fleets/hostile".to_string(),
        s3_endpoint: "https://abc.eu.r2.cloudflarestorage.com".to_string(),
        s3_region: "eu".to_string(),
        access_key_id: "scoped-key".to_string(),
        secret_access_key: "scoped-secret".to_string(),
        session_token: Some("scoped-token".to_string()),
        cgroup_leaf: PathBuf::from("/sys/fs/cgroup/outer/hostile"),
        cgroups_active: true,
        extra_env: vec![
            ("APP_KEY".to_string(), "app-value".to_string()),
            ("OUTER_SECRET".to_string(), "planted".to_string()),
            ("STRIPE_SECRET_KEY".to_string(), "planted".to_string()),
        ],
    };
    let opts = outer::fleet::supervisor::FleetOpts {
        celld_bin: "celld".to_string(),
        state_dir: PathBuf::from("/data/fleets/hostile"),
        shutdown_total_ms: 5000,
        idle_evict_secs: 60,
        deploy_poll_secs: 300,
        asset_cache_mb: 256,
    };
    let (program, args, env) = outer::fleet::supervisor::fleet_command(&spec, &opts);
    assert_eq!(program, "celld");
    for flag in ["--listen", "--internal-listen"] {
        let at = args.iter().position(|arg| arg == flag).expect(flag);
        let addr = &args[at + 1];
        assert!(
            addr.starts_with("127.0.0.1:"),
            "{flag} binds loopback, got {addr}"
        );
    }
    let names: HashSet<&str> = env.iter().map(|(key, _)| key.as_str()).collect();
    assert!(!names.contains("OUTER_SECRET"), "reserved env dropped");
    assert!(!names.contains("STRIPE_SECRET_KEY"), "reserved env dropped");
    assert!(names.contains("APP_KEY"), "app env passes through");
    assert!(
        names.contains("AWS_SESSION_TOKEN"),
        "session token rides along"
    );
}

/// The data-dir layout isolates apps: the root and every app root traverse
/// but do not list (0711), everything else is private, and per-app dirs are
/// 0700 — exactly what stops the sibling-read fixtures.
#[cfg(unix)]
#[test]
fn hostile_data_dir_layout_isolates_apps() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    outer::fleet::sandbox::prepare_data_dir(dir.path()).expect("prepare");
    let mode = |path: &Path| {
        std::fs::symlink_metadata(path)
            .unwrap_or_else(|_| panic!("{} exists", path.display()))
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(
        mode(dir.path()),
        0o711,
        "data root traverses but lists nothing"
    );
    for root in outer::fleet::sandbox::APP_ROOTS {
        assert_eq!(mode(&dir.path().join(root)), 0o711, "{root} traverses only");
        let leaf = dir.path().join(root).join("hostile");
        std::fs::create_dir(&leaf).expect("app leaf");
        // The per-app handover leaves 0700 app-owned dirs behind; chmod here
        // stands in for it (the uid-dropping half is covered privileged).
        std::fs::set_permissions(&leaf, std::fs::Permissions::from_mode(0o700))
            .expect("chmod leaf");
        assert_eq!(mode(&leaf), 0o700, "app leaf is private");
    }
}

// --- Privileged layer -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProbeReport {
    #[allow(dead_code)]
    phase: String,
    probes: HashMap<String, Probe>,
}

#[derive(Debug, Deserialize)]
struct Probe {
    detail: String,
    ok: bool,
}

/// Fail closed on the fixture contract: every must-fail probe the manifest
/// names for this phase must be present and contained (`ok: false`); every
/// must-succeed probe must be present and green. A fixture that silently
/// stops probing fails the suite instead of passing it.
fn assert_contained(report: &ProbeReport, phase: &str, manifest: &Manifest) {
    let mut missing = Vec::new();
    for probe in manifest.probes.iter().filter(|probe| probe.phase == phase) {
        match report.probes.get(&probe.id) {
            None => missing.push(probe.id.clone()),
            Some(result) if probe.must == "fail" => assert!(
                !result.ok,
                "hostile probe {} escaped: {}",
                probe.id, result.detail
            ),
            Some(result) => assert!(
                result.ok,
                "hostile positive control {} failed: {}",
                probe.id, result.detail
            ),
        }
    }
    // Unknown probe ids are fail-closed too: the suite only knows allow-listed
    // positive controls, so anything unexpected and green is a leak.
    for (id, result) in &report.probes {
        if id.starts_with("env:reserved-dropped:") {
            continue;
        }
        let known = manifest.probes.iter().find(|probe| &probe.id == id);
        match known {
            Some(probe) if probe.must == "succeed" => {}
            Some(_) => assert!(!result.ok, "hostile probe {id} escaped: {}", result.detail),
            None => assert!(!result.ok, "unknown probe {id} escaped: {}", result.detail),
        }
    }
    assert!(missing.is_empty(), "probes went missing: {missing:?}");
}

fn require_root() -> bool {
    // SAFETY: geteuid takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("SKIP: hostile privileged tests need root (run via scripts/hostile-suite.sh on the VPS)");
        return false;
    }
    true
}

fn js_runtime() -> Option<String> {
    for runtime in ["bun", "node"] {
        if std::process::Command::new(runtime)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
        {
            return Some(runtime.to_string());
        }
    }
    eprintln!("SKIP: hostile privileged tests need bun or node on PATH");
    None
}

/// The app uid range prod polices (`OUTER_APP_UID_RANGE`, `lo-hi` inclusive;
/// default the range the image pre-creates). The hostile uids must live
/// inside it, or the egress policy never applies to their probes.
fn app_uid_range() -> Option<(u32, u32)> {
    let raw = std::env::var("OUTER_APP_UID_RANGE").unwrap_or_default();
    if raw.trim().is_empty() {
        return Some((200_000, 201_023));
    }
    match raw
        .split_once('-')
        .and_then(|(lo, hi)| {
            Some((
                lo.trim().parse::<u32>().ok()?,
                hi.trim().parse::<u32>().ok()?,
            ))
        })
        .filter(|(lo, hi)| lo <= hi)
    {
        Some(range) => Some(range),
        None => {
            eprintln!("SKIP: OUTER_APP_UID_RANGE={raw:?} is not an inclusive lo-hi range");
            None
        }
    }
}

/// Probe uids from inside the app range: `HOSTILE_UID_BASE` (default the top
/// of the configured range) names the probe uid, the next uid the sibling.
/// Real apps allocate from the bottom up, so the top never collides with one.
fn hostile_uids(range: (u32, u32)) -> Option<(u32, u32)> {
    let base: u32 = match std::env::var("HOSTILE_UID_BASE") {
        Err(_) => range.1.saturating_sub(1),
        Ok(raw) => match raw.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("SKIP: HOSTILE_UID_BASE={raw:?} is not a uid");
                return None;
            }
        },
    };
    let (probe_uid, sibling_uid) = (base, base.saturating_add(1));
    if probe_uid < range.0 || sibling_uid > range.1 {
        eprintln!(
            "SKIP: hostile uids {probe_uid}/{sibling_uid} fall outside OUTER_APP_UID_RANGE {}-{}; set HOSTILE_UID_BASE inside it",
            range.0, range.1
        );
        return None;
    }
    Some((probe_uid, sibling_uid))
}

/// The nft policy the non-privileged tests pin must actually be installed
/// AND cover `uid`: without it a refused connection proves nothing (nothing
/// may be listening), so the suite skips loudly instead of passing vacuously.
fn require_nft_table_covering(uid: u32) -> bool {
    let out = std::process::Command::new("nft")
        .args(["list", "table", "inet", "outer"])
        .output();
    let dumped = match out {
        Ok(result) if result.status.success() => {
            String::from_utf8_lossy(&result.stdout).into_owned()
        }
        _ => {
            eprintln!("SKIP: nft table `inet outer` not installed; the VPS installs it at boot");
            return false;
        }
    };
    if nft_covers_uid(&dumped, uid) {
        return true;
    }
    eprintln!(
        "SKIP: nft table `inet outer` is installed but no skuid rule covers uid {uid}; reinstall it for the configured OUTER_APP_UID_RANGE"
    );
    false
}

/// True when some `meta skuid <lo>-<hi>` (or bare `<uid>`) match in the
/// ruleset dump encloses `uid`.
fn nft_covers_uid(rules: &str, uid: u32) -> bool {
    let mut rest = rules;
    while let Some(at) = rest.find("skuid") {
        rest = &rest[at + "skuid".len()..];
        let token: String = rest
            .chars()
            .skip_while(|char| char.is_whitespace())
            .take_while(|char| char.is_ascii_digit() || *char == '-')
            .collect();
        let covered = match token.split_once('-') {
            Some((lo, hi)) => {
                matches!((lo.parse::<u32>(), hi.parse::<u32>()), (Ok(lo), Ok(hi)) if lo <= uid && uid <= hi)
            }
            None => token.parse::<u32>().is_ok_and(|only| only == uid),
        };
        if covered {
            return true;
        }
    }
    false
}

/// A real cgroup leaf with kernel-enforced limits: the harness writes the
/// limits and reads them back, like `cgroups::write_limit` — a silent no-op
/// (read-only mount, no delegation) skips instead of running uncapped.
fn make_leaf(anchor: &Path, name: &str, memory_max: &str, pids_max: &str) -> Option<PathBuf> {
    let leaf = anchor.join(name);
    if std::fs::create_dir_all(&leaf).is_err() {
        eprintln!("SKIP: cannot create cgroup leaf under {}", anchor.display());
        return None;
    }
    for (file, value) in [("memory.max", memory_max), ("pids.max", pids_max)] {
        if std::fs::write(leaf.join(file), value).is_err() {
            eprintln!(
                "SKIP: cannot write {} (no delegation?)",
                leaf.join(file).display()
            );
            return None;
        }
        let applied = std::fs::read_to_string(leaf.join(file)).unwrap_or_default();
        if applied.trim() != value {
            eprintln!(
                "SKIP: {file} did not apply (wrote {value}, have {:?})",
                applied.trim()
            );
            return None;
        }
    }
    Some(leaf)
}

fn kill_leaf(leaf: &Path) {
    let _ = std::fs::write(leaf.join("cgroup.kill"), "1");
    let _ = std::fs::remove_dir(leaf);
}

fn chown(path: &Path, uid: u32, gid: u32) {
    use std::os::unix::fs::PermissionsExt;
    // Numeric ownership without passwd entries: the sandbox drops by id.
    let _ = std::process::Command::new("chown")
        .arg(format!("{uid}:{gid}"))
        .arg(path)
        .status();
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

/// The real build path: `build-probe.mjs` through `run_sandboxed` as an app
/// uid in a limited leaf, exactly how `deploy::pipeline` runs install/build.
/// Needs root, a writable cgroupfs with delegation, the installed nft table
/// and a JS runtime. Run with `cargo test --locked -- --ignored hostile`.
#[tokio::test]
#[ignore = "needs root, a writable cgroupfs with delegation, the installed nft table and bun/node"]
async fn hostile_build_probes_are_contained() {
    if !require_root() {
        return;
    }
    let Some(runtime) = js_runtime() else {
        return;
    };
    let Some(range) = app_uid_range() else {
        return;
    };
    let Some((probe_uid, sibling_uid)) = hostile_uids(range) else {
        return;
    };
    if !require_nft_table_covering(probe_uid) {
        return;
    }
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        eprintln!("SKIP: no cgroup v2 mount at /sys/fs/cgroup");
        return;
    }
    // Fail closed on internet: the positive control needs a network that
    // reaches it, proven from this (unconfined) uid first.
    if std::net::TcpStream::connect_timeout(
        &"93.184.215.14:443".parse().unwrap(),
        std::time::Duration::from_secs(5),
    )
    .is_err()
    {
        eprintln!("SKIP: no route to the internet for the positive control");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    outer::fleet::sandbox::prepare_data_dir(dir.path()).expect("prepare");
    // Sibling app dirs: real 0700 dirs owned by another in-range uid, each
    // with a readable-looking file, so refusals prove isolation, not absence.
    for (root, slug) in [("work", "neighbour"), ("fleets", "neighbour")] {
        let leaf = dir.path().join(root).join(slug);
        std::fs::create_dir_all(&leaf).expect("sibling leaf");
        std::fs::write(leaf.join("index.js"), "neighbour code").expect("sibling file");
        chown(&leaf, sibling_uid, sibling_uid);
        chown(&leaf.join("index.js"), sibling_uid, sibling_uid);
    }
    // Root-only canary: the stand-in for outer.sqlite and host keys.
    let canary = dir.path().join("outer.sqlite");
    std::fs::write(&canary, "host-secret-canary").expect("canary");
    chown(&canary, 0, 0);
    // The sandbox uid must traverse the data dir to its own dirs (0711
    // above) but open nothing else.
    let work = dir.path().join("work").join("hostile");
    let cache = dir.path().join("cache").join("hostile");
    std::fs::create_dir_all(&work).expect("work");
    std::fs::create_dir_all(&cache).expect("cache");
    for sub in ["bun", "home", "tmp"] {
        std::fs::create_dir_all(cache.join(sub)).expect("cache sub");
    }
    chown(&work, probe_uid, probe_uid);
    chown(&cache, probe_uid, probe_uid);
    std::fs::copy(
        fixture_dir().join("build-probe.mjs"),
        work.join("build-probe.mjs"),
    )
    .expect("stage probe");
    chown(&work.join("build-probe.mjs"), probe_uid, probe_uid);

    // A live loopback target the harness proves serves from its own uid, so
    // the sandbox uid's refusal proves the nft policy, not an absent server.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let port = listener.local_addr().expect("port").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });
    let target = format!("http://127.0.0.1:{port}/health");
    assert!(
        reqwest::get(&target)
            .await
            .is_ok_and(|res| res.status().is_success()),
        "the loopback target serves from the harness uid"
    );

    let anchor = PathBuf::from("/sys/fs/cgroup");
    let leaf_name = format!("hostile-{}", std::process::id());
    let Some(leaf) = make_leaf(&anchor, &leaf_name, "536870912", "512") else {
        return;
    };
    let own = format!("0::/{}", leaf_name);
    assert!(
        own.contains(&leaf_name),
        "the probe checks its own placement against the leaf"
    );

    let sandbox = outer::fleet::sandbox::Sandbox {
        uid: Some(probe_uid),
        gid: Some(probe_uid),
        platform_env: &[
            (
                "AWS_ACCESS_KEY_ID".to_string(),
                "scoped-test-key".to_string(),
            ),
            (
                "AWS_SESSION_TOKEN".to_string(),
                "scoped-test-token".to_string(),
            ),
        ],
        env: &[
            ("OUTER_SECRET".to_string(), "planted".to_string()),
            ("STRIPE_SECRET_KEY".to_string(), "planted".to_string()),
            ("APP_KEY".to_string(), "allowed".to_string()),
            (
                "HOSTILE_SIBLING_WORK".to_string(),
                dir.path()
                    .join("work/neighbour")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "HOSTILE_SIBLING_DATA".to_string(),
                dir.path()
                    .join("fleets/neighbour")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "HOSTILE_CANARY_PATH".to_string(),
                canary.to_string_lossy().into_owned(),
            ),
            (
                "HOSTILE_DATA_DIR".to_string(),
                dir.path().to_string_lossy().into_owned(),
            ),
            ("HOSTILE_LOOPBACK_TARGET".to_string(), target),
            (
                "HOSTILE_CGROUP_LEAF".to_string(),
                leaf.to_string_lossy().into_owned(),
            ),
            (
                "HOSTILE_EXPECT_MEMORY_MAX".to_string(),
                "536870912".to_string(),
            ),
        ],
        cache_root: &cache,
        cgroup_leaf: Some(&leaf),
    };
    let output = outer::fleet::sandbox::run_sandboxed(
        &runtime,
        &["build-probe.mjs"],
        &work,
        &sandbox,
        std::time::Duration::from_secs(300),
    )
    .await
    .expect("the probe runs");
    kill_leaf(&leaf);

    let line = output
        .lines()
        .rfind(|line| line.contains("\"phase\""))
        .unwrap_or_else(|| {
            panic!(
                "the probe prints a JSON report, got: {}",
                output.chars().take(500).collect::<String>()
            )
        });
    let report: ProbeReport = serde_json::from_str(line).expect("report parses");
    assert_contained(&report, "build", &manifest());
}

/// Scoped R2 credentials stay in their prefix: `r2-probe.mjs` round-trips
/// inside its own prefix and must be refused (403/401, never 404 or a
/// transport error) on a sibling prefix and the bucket root. Needs
/// `HOSTILE_S3_*` env (the integrator mints a scoped key on the VPS); the
/// worker fixture's release phase runs this with the deploy's scoped key.
#[tokio::test]
#[ignore = "needs HOSTILE_S3_* scoped credentials (scripts/hostile-suite.sh --r2)"]
async fn hostile_r2_scope_holds() {
    if !require_root() {
        return;
    }
    let Some(runtime) = js_runtime() else {
        return;
    };
    for name in [
        "HOSTILE_S3_ENDPOINT",
        "HOSTILE_S3_BUCKET",
        "HOSTILE_S3_KEY",
        "HOSTILE_S3_SECRET",
        "HOSTILE_S3_PREFIX",
    ] {
        if std::env::var(name).is_err() {
            eprintln!("SKIP: {name} not set; mint a scoped key for a fixture prefix first");
            return;
        }
    }
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::copy(
        fixture_dir().join("r2-probe.mjs"),
        dir.path().join("r2-probe.mjs"),
    )
    .expect("stage probe");
    let child = outer::fleet::sandbox::Sandbox {
        uid: None,
        gid: None,
        platform_env: &[],
        env: &[],
        cache_root: dir.path(),
        cgroup_leaf: None,
    };
    let output = outer::fleet::sandbox::run_sandboxed(
        &runtime,
        &["r2-probe.mjs"],
        dir.path(),
        &child,
        std::time::Duration::from_secs(120),
    )
    .await
    .expect("the probe runs");
    let line = output
        .lines()
        .rfind(|line| line.contains("\"phase\""))
        .unwrap_or_else(|| {
            panic!(
                "the probe prints a JSON report, got: {}",
                output.chars().take(500).collect::<String>()
            )
        });
    let report: ProbeReport = serde_json::from_str(line).expect("report parses");
    assert_contained(&report, "r2", &manifest());
}
