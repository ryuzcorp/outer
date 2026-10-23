//! Configuration: one struct, read once from the environment at boot.
//!
//! Every variable has exactly one name, defaults live here, and `validate`
//! reports every problem at once instead of failing on the first.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Where platform objects (git bundles, app storage, the state snapshot) live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreKind {
    /// Cloudflare R2 through its S3 API. Required off localhost.
    R2(R2Config),
    /// In-process map, lost on restart. Local development only.
    Memory,
}

#[derive(Clone, PartialEq, Eq)]
pub struct R2Config {
    /// `https://<account>.eu.r2.cloudflarestorage.com` for an EU bucket.
    pub endpoint: String,
    /// `auto` for R2 (`OUTER_R2_REGION`); S3-compatible dev stores want a real one.
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl std::fmt::Debug for R2Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("R2Config")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Platform host; apps live at `<slug>.<domain>` (`OUTER_DOMAIN`).
    pub domain: String,
    /// SQLite, git mirrors, worktrees and caches (`OUTER_DATA`).
    pub data_dir: PathBuf,
    /// Plain-HTTP listener (`OUTER_HTTP_LISTEN`).
    pub http_listen: SocketAddr,
    /// This host's identity for the snapshot owner fence (`OUTER_HOST_ID`).
    pub host_id: String,
    /// `OUTER_STORE=r2|memory` plus the R2 variables.
    pub store: StoreKind,
    /// Log filter directives (`RUST_LOG`, default `info`).
    pub log_filter: String,
    /// Running apps: allocation ranges, binaries, limits, sleep.
    pub runtime: RuntimeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// Apps live at `<slug>.<apps_domain>` (`OUTER_APPS_DOMAIN`, default `OUTER_DOMAIN`).
    pub apps_domain: String,
    /// Two loopback ports per app (`OUTER_FLEET_PORTS`, inclusive `lo-hi`).
    pub fleet_ports: (u16, u16),
    /// One uid per app (`OUTER_APP_UID_RANGE`, inclusive; the image pre-creates it).
    pub app_uid_range: (u32, u32),
    /// Idle time before an app's fleet is stopped (`OUTER_SLEEP_AFTER_S`).
    pub sleep_after_secs: u64,
    /// `OUTER_CELLD_BIN`, `OUTER_BUN_BIN`, `OUTER_ESBUILD_BIN` (passed to celld).
    pub celld_bin: String,
    pub bun_bin: String,
    pub esbuild_bin: Option<String>,
    /// Global build slots (`OUTER_BUILD_CONCURRENCY`, default half the CPUs).
    pub build_concurrency: usize,
    /// Concurrent cold starts (`OUTER_WAKE_CONCURRENCY`).
    pub wake_concurrency: usize,
    /// Per-fleet `memory.max` (`OUTER_FLEET_MEMORY_MB`) and `cpu.max` (`OUTER_FLEET_CPU_MAX`).
    pub fleet_memory_mb: u64,
    pub fleet_cpu_max: String,
}

const DEFAULT_DOMAIN: &str = "outer.localhost";
const DEFAULT_LISTEN: &str = "127.0.0.1:8080";

impl Config {
    pub fn from_env() -> Result<Config, Vec<String>> {
        let vars: BTreeMap<String, String> = std::env::vars().collect();
        Config::from_vars(&vars)
    }

    /// Parse and validate. Pure, so tests pass a map instead of the process env.
    pub fn from_vars(vars: &BTreeMap<String, String>) -> Result<Config, Vec<String>> {
        let get = |name: &str| vars.get(name).map(|v| v.trim()).filter(|v| !v.is_empty());
        let mut problems = Vec::new();

        let domain = get("OUTER_DOMAIN")
            .unwrap_or(DEFAULT_DOMAIN)
            .to_ascii_lowercase();
        let data_dir = PathBuf::from(get("OUTER_DATA").unwrap_or("./data"));
        let http_listen = match get("OUTER_HTTP_LISTEN").unwrap_or(DEFAULT_LISTEN).parse() {
            Ok(addr) => addr,
            Err(_) => {
                problems.push(format!(
                    "OUTER_HTTP_LISTEN={:?} is not a socket address (expected host:port, e.g. {DEFAULT_LISTEN})",
                    get("OUTER_HTTP_LISTEN").unwrap_or_default()
                ));
                DEFAULT_LISTEN
                    .parse()
                    .expect("default listen address parses")
            }
        };
        let host_id = get("OUTER_HOST_ID")
            .map(str::to_string)
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|h| h.trim().to_string())
            })
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "outer".to_string());

        let store = match get("OUTER_STORE").unwrap_or("r2") {
            "memory" => StoreKind::Memory,
            "r2" => {
                let mut required = |name: &str| match get(name) {
                    Some(v) => v.to_string(),
                    None => {
                        problems.push(format!("{name} is required when OUTER_STORE=r2"));
                        String::new()
                    }
                };
                StoreKind::R2(R2Config {
                    endpoint: required("OUTER_R2_ENDPOINT"),
                    region: get("OUTER_R2_REGION").unwrap_or("auto").to_string(),
                    bucket: required("OUTER_R2_BUCKET"),
                    access_key_id: required("R2_ACCESS_KEY_ID"),
                    secret_access_key: required("R2_SECRET_ACCESS_KEY"),
                })
            }
            other => {
                problems.push(format!("OUTER_STORE={other:?} is not one of: r2, memory"));
                StoreKind::Memory
            }
        };

        let log_filter = get("RUST_LOG").unwrap_or("info").to_string();
        if let Err(err) = tracing_subscriber::EnvFilter::try_new(&log_filter) {
            problems.push(format!(
                "RUST_LOG={log_filter:?} is not a valid filter: {err}"
            ));
        }

        let runtime = RuntimeConfig {
            apps_domain: get("OUTER_APPS_DOMAIN")
                .unwrap_or(domain.as_str())
                .to_ascii_lowercase(),
            fleet_ports: parse_range(
                get("OUTER_FLEET_PORTS"),
                "OUTER_FLEET_PORTS",
                (20000, 29999),
                &mut problems,
            ),
            app_uid_range: parse_range(
                get("OUTER_APP_UID_RANGE"),
                "OUTER_APP_UID_RANGE",
                (200_000, 201_023),
                &mut problems,
            ),
            sleep_after_secs: parse_num(
                get("OUTER_SLEEP_AFTER_S"),
                "OUTER_SLEEP_AFTER_S",
                1800,
                &mut problems,
            ),
            celld_bin: resolve_bin(
                get("OUTER_CELLD_BIN").unwrap_or("celld"),
                vars.get("PATH").map(String::as_str).unwrap_or(""),
            ),
            bun_bin: resolve_bin(
                get("OUTER_BUN_BIN").unwrap_or("bun"),
                vars.get("PATH").map(String::as_str).unwrap_or(""),
            ),
            esbuild_bin: get("OUTER_ESBUILD_BIN")
                .map(|bin| resolve_bin(bin, vars.get("PATH").map(String::as_str).unwrap_or(""))),
            build_concurrency: parse_num(
                get("OUTER_BUILD_CONCURRENCY"),
                "OUTER_BUILD_CONCURRENCY",
                std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1)),
                &mut problems,
            ),
            wake_concurrency: parse_num(
                get("OUTER_WAKE_CONCURRENCY"),
                "OUTER_WAKE_CONCURRENCY",
                4,
                &mut problems,
            ),
            fleet_memory_mb: parse_num(
                get("OUTER_FLEET_MEMORY_MB"),
                "OUTER_FLEET_MEMORY_MB",
                512,
                &mut problems,
            ),
            fleet_cpu_max: get("OUTER_FLEET_CPU_MAX")
                .unwrap_or("50000 100000")
                .to_string(),
        };

        let config = Config {
            domain,
            data_dir,
            http_listen,
            host_id,
            store,
            log_filter,
            runtime,
        };
        problems.extend(config.validate());
        if problems.is_empty() {
            Ok(config)
        } else {
            Err(problems)
        }
    }

    /// Cross-field rules that keep a production boot from running on dev settings.
    fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !is_hostname(&self.domain) {
            problems.push(format!("OUTER_DOMAIN={:?} is not a hostname", self.domain));
        }
        if self.store == StoreKind::Memory && !self.is_local() {
            problems.push(format!(
                "OUTER_STORE=memory loses every app on restart and is refused for OUTER_DOMAIN={}; use r2",
                self.domain
            ));
        }
        if let StoreKind::R2(r2) = &self.store {
            if !r2.endpoint.is_empty() && !r2.endpoint.starts_with("https://") && !self.is_local() {
                problems.push(format!("OUTER_R2_ENDPOINT={:?} must be https", r2.endpoint));
            }
        }
        let rt = &self.runtime;
        if !is_hostname(&rt.apps_domain) {
            problems.push(format!(
                "OUTER_APPS_DOMAIN={:?} is not a hostname",
                rt.apps_domain
            ));
        }
        if rt.fleet_ports.1 - rt.fleet_ports.0 < 1 {
            problems.push("OUTER_FLEET_PORTS must hold at least two ports (one app)".to_string());
        }
        if rt.build_concurrency == 0 || rt.wake_concurrency == 0 {
            problems.push(
                "OUTER_BUILD_CONCURRENCY and OUTER_WAKE_CONCURRENCY must be at least 1".to_string(),
            );
        }
        if rt.fleet_memory_mb < 64 {
            problems.push(format!(
                "OUTER_FLEET_MEMORY_MB={} is below 64",
                rt.fleet_memory_mb
            ));
        }
        if !is_cpu_max(&rt.fleet_cpu_max) {
            problems.push(format!(
                "OUTER_FLEET_CPU_MAX={:?} must be cgroup v2 cpu.max syntax: \"<quota> <period>\" or \"max <period>\"",
                rt.fleet_cpu_max
            ));
        }
        problems
    }

    /// The `localhost` family: dev-only settings are allowed here and nowhere else.
    pub fn is_local(&self) -> bool {
        self.domain == "localhost" || self.domain.ends_with(".localhost")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("outer.sqlite")
    }
}

/// `lo-hi`, inclusive, `lo <= hi`. A malformed value is reported and the
/// default used, so every other problem still gets reported in the same run.
fn parse_range<T>(
    raw: Option<&str>,
    name: &str,
    default: (T, T),
    problems: &mut Vec<String>,
) -> (T, T)
where
    T: std::str::FromStr + PartialOrd + Copy + std::fmt::Display,
{
    let Some(raw) = raw else { return default };
    let parsed = raw
        .split_once('-')
        .and_then(|(lo, hi)| Some((lo.trim().parse::<T>().ok()?, hi.trim().parse::<T>().ok()?)))
        .filter(|(lo, hi)| lo <= hi);
    match parsed {
        Some(range) => range,
        None => {
            problems.push(format!(
                "{name}={raw:?} is not an inclusive range lo-hi (default {}-{})",
                default.0, default.1
            ));
            default
        }
    }
}

fn parse_num<T>(raw: Option<&str>, name: &str, default: T, problems: &mut Vec<String>) -> T
where
    T: std::str::FromStr + Copy,
{
    let Some(raw) = raw else { return default };
    match raw.parse() {
        Ok(n) => n,
        Err(_) => {
            problems.push(format!("{name}={raw:?} is not a non-negative integer"));
            default
        }
    }
}

fn is_cpu_max(s: &str) -> bool {
    let mut parts = s.split_whitespace();
    let (Some(quota), Some(period), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    (quota == "max" || quota.parse::<u64>().is_ok_and(|q| q > 0))
        && period.parse::<u64>().is_ok_and(|p| p > 0)
}

fn is_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Resolve `bin` against `path` (the `PATH` search list) at load: bare names
/// become absolute when an executable is found, explicit paths pass through
/// as given, and anything missing keeps its raw name so `check-config` and
/// unit tests never depend on installed tools.
fn resolve_bin(bin: &str, path: &str) -> String {
    if bin.contains('/') {
        return bin.to_string();
    }
    let search = if path.is_empty() {
        "/usr/local/bin:/usr/bin:/bin".to_string()
    } else {
        path.to_string()
    };
    for dir in std::env::split_paths(&search) {
        let candidate = dir.join(bin);
        if is_executable(&candidate) {
            return candidate.to_string_lossy().into_owned();
        }
    }
    bin.to_string()
}

fn is_executable(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn local_dev_runs_on_the_memory_store_with_defaults() {
        let cfg = Config::from_vars(&vars(&[("OUTER_STORE", "memory")])).unwrap();
        assert_eq!(cfg.domain, "outer.localhost");
        assert_eq!(cfg.store, StoreKind::Memory);
        assert_eq!(cfg.http_listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(cfg.db_path(), PathBuf::from("./data/outer.sqlite"));
    }

    #[test]
    fn production_refuses_the_memory_store() {
        let err = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            ("OUTER_STORE", "memory"),
        ]))
        .unwrap_err();
        assert!(
            err.iter().any(|p| p.contains("OUTER_STORE=memory")),
            "{err:?}"
        );
    }

    #[test]
    fn r2_reports_every_missing_variable_at_once() {
        let err = Config::from_vars(&vars(&[("OUTER_DOMAIN", "outer.now")])).unwrap_err();
        for name in [
            "OUTER_R2_ENDPOINT",
            "OUTER_R2_BUCKET",
            "R2_ACCESS_KEY_ID",
            "R2_SECRET_ACCESS_KEY",
        ] {
            assert!(
                err.iter().any(|p| p.starts_with(name)),
                "missing {name} in {err:?}"
            );
        }
    }

    #[test]
    fn r2_config_parses_and_never_prints_the_secret() {
        let cfg = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            (
                "OUTER_R2_ENDPOINT",
                "https://acct.eu.r2.cloudflarestorage.com",
            ),
            ("OUTER_R2_BUCKET", "outer"),
            ("R2_ACCESS_KEY_ID", "key"),
            ("R2_SECRET_ACCESS_KEY", "s3cret"),
        ]))
        .unwrap();
        let StoreKind::R2(r2) = &cfg.store else {
            panic!("expected r2")
        };
        assert_eq!(r2.bucket, "outer");
        assert!(!format!("{cfg:?}").contains("s3cret"));
    }

    #[test]
    fn rejects_bad_listen_address_and_bad_domain_together() {
        let err = Config::from_vars(&vars(&[
            ("OUTER_STORE", "memory"),
            ("OUTER_DOMAIN", "Not A Host"),
            ("OUTER_HTTP_LISTEN", "eighty"),
        ]))
        .unwrap_err();
        assert!(
            err.iter().any(|p| p.starts_with("OUTER_HTTP_LISTEN")),
            "{err:?}"
        );
        assert!(err.iter().any(|p| p.starts_with("OUTER_DOMAIN")), "{err:?}");
    }

    #[test]
    fn runtime_defaults_match_the_image_and_ranges_parse() {
        let cfg = Config::from_vars(&vars(&[("OUTER_STORE", "memory")])).unwrap();
        assert_eq!(cfg.runtime.apps_domain, "outer.localhost");
        assert_eq!(
            cfg.runtime.app_uid_range,
            (200_000, 201_023),
            "keep in step with docker/Dockerfile"
        );
        assert_eq!(cfg.runtime.fleet_ports, (20000, 29999));

        let cfg = Config::from_vars(&vars(&[
            ("OUTER_STORE", "memory"),
            ("OUTER_FLEET_PORTS", "30000-30009"),
            ("OUTER_APPS_DOMAIN", "apps.localhost"),
        ]))
        .unwrap();
        assert_eq!(cfg.runtime.fleet_ports, (30000, 30009));
        assert_eq!(cfg.runtime.apps_domain, "apps.localhost");
    }

    #[test]
    fn runtime_reports_bad_ranges_numbers_and_cpu_max_together() {
        let err = Config::from_vars(&vars(&[
            ("OUTER_STORE", "memory"),
            ("OUTER_FLEET_PORTS", "30010-30000"),
            ("OUTER_APP_UID_RANGE", "lots"),
            ("OUTER_SLEEP_AFTER_S", "-5"),
            ("OUTER_FLEET_CPU_MAX", "half"),
            ("OUTER_WAKE_CONCURRENCY", "0"),
        ]))
        .unwrap_err();
        for name in [
            "OUTER_FLEET_PORTS",
            "OUTER_APP_UID_RANGE",
            "OUTER_SLEEP_AFTER_S",
            "OUTER_FLEET_CPU_MAX",
            "OUTER_BUILD_CONCURRENCY and OUTER_WAKE_CONCURRENCY",
        ] {
            assert!(
                err.iter().any(|p| p.starts_with(name)),
                "missing {name} in {err:?}"
            );
        }
    }

    #[test]
    fn tool_bins_resolve_against_path_but_never_fail() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("bun");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).unwrap();
        }
        let path = dir.path().to_string_lossy().into_owned();
        let cfg = Config::from_vars(&vars(&[
            ("OUTER_STORE", "memory"),
            ("PATH", path.as_str()),
            ("OUTER_ESBUILD_BIN", "/opt/esbuild"),
        ]))
        .unwrap();
        assert_eq!(cfg.runtime.bun_bin, fake.to_string_lossy().into_owned());
        // Missing tools and explicit paths keep their raw names: config load
        // (and check-config) never depends on installed tools.
        assert_eq!(cfg.runtime.celld_bin, "celld");
        assert_eq!(cfg.runtime.esbuild_bin.as_deref(), Some("/opt/esbuild"));
    }
}
