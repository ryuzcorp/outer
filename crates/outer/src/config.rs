//! Configuration: one struct, read once from the environment at boot.
//!
//! Every variable has exactly one name, defaults live here, and `validate`
//! reports every problem at once instead of failing on the first.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

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

/// Cloudflare credentials for minting scoped R2 temporary credentials
/// (spec §12.6). `None` means the parent R2 keys are used directly, which is
/// allowed only on localhost dev.
#[derive(Clone, PartialEq, Eq)]
pub struct CloudflareConfig {
    /// `CLOUDFLARE_ACCOUNT_ID`.
    pub account_id: String,
    /// `CLOUDFLARE_API_TOKEN`.
    pub api_token: String,
    /// `CLOUDFLARE_ZONE_ID` (optional): cache purge and custom hostnames
    /// target this zone. `None` skips the purge with a warn (localhost dev).
    pub zone_id: Option<String>,
    /// `OUTER_R2_CREDENTIAL_TTL_S`: minted credential lifetime.
    pub credential_ttl: Duration,
    /// `credential_ttl` minus one day, at least `credential_ttl / 2`: fleets
    /// older than this restart with fresh credentials before they expire.
    pub credential_refresh_after: Duration,
}

impl std::fmt::Debug for CloudflareConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareConfig")
            .field("account_id", &self.account_id)
            .field("api_token", &"<redacted>")
            .field("zone_id", &self.zone_id)
            .field("credential_ttl", &self.credential_ttl)
            .field("credential_refresh_after", &self.credential_refresh_after)
            .finish()
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("domain", &self.domain)
            .field("data_dir", &self.data_dir)
            .field("site_dir", &self.site_dir)
            .field("listen", &self.listen)
            .field("tls_cert", &self.tls_cert)
            .field("tls_key", &self.tls_key)
            .field("origin_pull_ca", &self.origin_pull_ca)
            .field("host_id", &self.host_id)
            .field("store", &self.store)
            .field("cloudflare", &self.cloudflare)
            .field("signup_open", &self.signup_open)
            .field("ai_gateway", &self.ai_gateway)
            .field("admin_emails", &self.admin_emails)
            .field("oauth_access_ttl", &self.oauth_access_ttl)
            .field("oauth_refresh_ttl", &self.oauth_refresh_ttl)
            .field("tool_wait", &self.tool_wait)
            .field("log_filter", &self.log_filter)
            .field("runtime", &self.runtime)
            .finish()
    }
}

pub struct Config {
    /// Platform host; apps live at `<slug>.<domain>` (`OUTER_DOMAIN`).
    pub domain: String,
    /// Built docs site served on the platform host (`OUTER_SITE_DIR`).
    pub site_dir: PathBuf,
    /// SQLite, git mirrors, worktrees and caches (`OUTER_DATA`).
    pub data_dir: PathBuf,
    /// Listener (`OUTER_LISTEN`); TLS when the TLS triple below is set.
    pub listen: SocketAddr,
    /// Origin-cert PEM chain (`OUTER_TLS_CERT`); all three TLS paths set or none.
    pub tls_cert: Option<PathBuf>,
    /// Origin-cert private key PEM (`OUTER_TLS_KEY`).
    pub tls_key: Option<PathBuf>,
    /// Origin-pull client-CA PEM (`OUTER_ORIGIN_PULL_CA`).
    pub origin_pull_ca: Option<PathBuf>,
    /// This host's identity for the snapshot owner fence (`OUTER_HOST_ID`).
    pub host_id: String,
    /// `OUTER_STORE=r2|memory` plus the R2 variables.
    pub store: StoreKind,
    /// Cloudflare pair for scoped R2 credentials (`None` = parent keys
    /// directly, localhost dev only). Read only with `OUTER_STORE=r2`.
    pub cloudflare: Option<CloudflareConfig>,
    /// `OUTER_SIGNUP=open|closed` (default closed): open admits anyone passing
    /// the abuse checks, closed admits only allowlisted and admin emails.
    pub signup_open: bool,
    /// Jev classification gateway (`OUTER_AI_GATEWAY`, default `outer`).
    pub ai_gateway: String,
    /// `OUTER_ADMIN_EMAILS`: comma-separated, normalized to lowercase.
    pub admin_emails: Vec<String>,
    /// `OUTER_TERMS_VERSION`: changing it re-asks every account at next sign-in.
    pub terms_version: String,
    /// `OUTER_EMAIL_FROM` (default `Outer <hello@outer.now>`).
    pub email_from: String,
    /// `OUTER_OAUTH_ACCESS_TTL_S` (default 3600).
    pub oauth_access_ttl: Duration,
    /// `OUTER_OAUTH_REFRESH_TTL_D` (default 90).
    pub oauth_refresh_ttl: Duration,
    /// `OUTER_TOOL_WAIT_S` (default 50): cap on one awaited tool call.
    pub tool_wait: Duration,
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
        let site_dir = PathBuf::from(get("OUTER_SITE_DIR").unwrap_or("./dist"));
        let listen = match get("OUTER_LISTEN").unwrap_or(DEFAULT_LISTEN).parse() {
            Ok(addr) => addr,
            Err(_) => {
                problems.push(format!(
                    "OUTER_LISTEN={:?} is not a socket address (expected host:port, e.g. {DEFAULT_LISTEN})",
                    get("OUTER_LISTEN").unwrap_or_default()
                ));
                DEFAULT_LISTEN
                    .parse()
                    .expect("default listen address parses")
            }
        };
        let opt_path = |name: &str| get(name).map(PathBuf::from);
        let tls_cert = opt_path("OUTER_TLS_CERT");
        let tls_key = opt_path("OUTER_TLS_KEY");
        let origin_pull_ca = opt_path("OUTER_ORIGIN_PULL_CA");
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

        // Scoped R2 credentials (spec §12.6): read only with OUTER_STORE=r2,
        // so a stray CLOUDFLARE_* in a local memory-store env stays silent.
        let cloudflare = match &store {
            StoreKind::R2(_) => {
                let account_id = get("CLOUDFLARE_ACCOUNT_ID").map(str::to_string);
                let api_token = get("CLOUDFLARE_API_TOKEN").map(str::to_string);
                let ttl_secs = parse_num(
                    get("OUTER_R2_CREDENTIAL_TTL_S"),
                    "OUTER_R2_CREDENTIAL_TTL_S",
                    604_800u64,
                    &mut problems,
                );
                if !(3600..=604_800).contains(&ttl_secs) {
                    problems.push(format!(
                        "OUTER_R2_CREDENTIAL_TTL_S={ttl_secs} must be between 3600 and 604800 (Cloudflare caps temporary-credential TTL at 7 days)"
                    ));
                }
                let ttl = Duration::from_secs(ttl_secs);
                match (account_id, api_token) {
                    (Some(account_id), Some(api_token)) => Some(CloudflareConfig {
                        account_id,
                        api_token,
                        zone_id: get("CLOUDFLARE_ZONE_ID").map(str::to_string),
                        credential_ttl: ttl,
                        credential_refresh_after: credential_refresh_after(ttl),
                    }),
                    (None, None) => {
                        if !is_local_domain(&domain) {
                            for name in ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN"] {
                                problems.push(format!(
                                    "{name} is required when OUTER_STORE=r2 off localhost (fleets mint scoped R2 credentials; the parent keys never leave outer)"
                                ));
                            }
                        }
                        None
                    }
                    _ => {
                        problems.push(
                            "CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN must both be set or both be empty (got one of two)".to_string(),
                        );
                        None
                    }
                }
            }
            StoreKind::Memory => None,
        };
        let signup_open = match get("OUTER_SIGNUP").unwrap_or("closed") {
            "open" => true,
            "closed" => false,
            other => {
                problems.push(format!(
                    "OUTER_SIGNUP={other:?} is not one of: closed, open"
                ));
                false
            }
        };
        let ai_gateway = get("OUTER_AI_GATEWAY").unwrap_or("outer").to_string();
        let admin_emails = get("OUTER_ADMIN_EMAILS")
            .map(|raw| {
                raw.split(',')
                    .map(|part| part.trim().to_ascii_lowercase())
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let terms_version = get("OUTER_TERMS_VERSION").map(str::to_string);
        let email_from = get("OUTER_EMAIL_FROM")
            .unwrap_or("Outer <hello@outer.now>")
            .to_string();
        let oauth_access_ttl = Duration::from_secs(parse_num(
            get("OUTER_OAUTH_ACCESS_TTL_S"),
            "OUTER_OAUTH_ACCESS_TTL_S",
            3600u64,
            &mut problems,
        ));
        let oauth_refresh_ttl = Duration::from_secs(
            parse_num(
                get("OUTER_OAUTH_REFRESH_TTL_D"),
                "OUTER_OAUTH_REFRESH_TTL_D",
                90u64,
                &mut problems,
            ) * 86400,
        );
        let tool_wait = Duration::from_secs(parse_num(
            get("OUTER_TOOL_WAIT_S"),
            "OUTER_TOOL_WAIT_S",
            50u64,
            &mut problems,
        ));
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
            site_dir,
            listen,
            tls_cert,
            tls_key,
            origin_pull_ca,
            host_id,
            store,
            cloudflare,
            signup_open,
            ai_gateway,
            admin_emails,
            terms_version: terms_version.unwrap_or_else(|| "dev".to_string()),
            email_from,
            oauth_access_ttl,
            oauth_refresh_ttl,
            tool_wait,
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
        let tls = [
            ("OUTER_TLS_CERT", &self.tls_cert),
            ("OUTER_TLS_KEY", &self.tls_key),
            ("OUTER_ORIGIN_PULL_CA", &self.origin_pull_ca),
        ];
        let tls_set = tls.iter().filter(|(_, p)| p.is_some()).count();
        if tls_set > 0 && tls_set < 3 {
            let missing = tls
                .iter()
                .filter(|(_, p)| p.is_none())
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ");
            problems.push(format!(
                "partial TLS config: OUTER_TLS_CERT, OUTER_TLS_KEY and OUTER_ORIGIN_PULL_CA must all be set or all be empty (missing {missing})"
            ));
        }
        if !self.is_local() && tls_set < 3 {
            problems.push(format!(
                "OUTER_TLS_CERT, OUTER_TLS_KEY and OUTER_ORIGIN_PULL_CA are required when OUTER_DOMAIN={} is not a localhost dev domain",
                self.domain
            ));
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
        if !self.is_local() && self.terms_version == "dev" {
            problems.push(
                "OUTER_TERMS_VERSION is required when OUTER_DOMAIN is not a localhost dev domain (changing it re-asks every account at next sign-in)"
                    .to_string(),
            );
        }
        if self.oauth_access_ttl.is_zero() {
            problems.push("OUTER_OAUTH_ACCESS_TTL_S must be at least 1".to_string());
        }
        if self.oauth_refresh_ttl.is_zero() {
            problems.push("OUTER_OAUTH_REFRESH_TTL_D must be at least 1".to_string());
        }
        if self.tool_wait.is_zero() {
            problems.push("OUTER_TOOL_WAIT_S must be at least 1".to_string());
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
        is_local_domain(&self.domain)
    }

    /// TLS terminates locally with an origin-pull client-CA: the full triple is set.
    pub fn tls_enabled(&self) -> bool {
        self.tls_cert.is_some() && self.tls_key.is_some() && self.origin_pull_ca.is_some()
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("outer.sqlite")
    }
}

/// The `localhost` family: bare `localhost` plus anything under it.
fn is_local_domain(domain: &str) -> bool {
    domain == "localhost" || domain.ends_with(".localhost")
}

/// How long after a fleet starts it restarts with fresh credentials: the
/// minted TTL minus a day, floored at half the TTL so a short TTL still
/// leaves a sane margin. Production: 604800 s − 86400 s = 518400 s (6 days).
fn credential_refresh_after(ttl: Duration) -> Duration {
    ttl.saturating_sub(Duration::from_secs(86400)).max(ttl / 2)
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
    fn site_dir_defaults_to_dist_and_overrides() {
        let cfg = Config::from_vars(&vars(&[("OUTER_STORE", "memory")])).unwrap();
        assert_eq!(cfg.site_dir, PathBuf::from("./dist"));
        let cfg = Config::from_vars(&vars(&[
            ("OUTER_STORE", "memory"),
            ("OUTER_SITE_DIR", "/usr/share/outer/site"),
        ]))
        .unwrap();
        assert_eq!(cfg.site_dir, PathBuf::from("/usr/share/outer/site"));
    }

    #[test]
    fn ai_gateway_defaults_to_outer_and_overrides() {
        let cfg = Config::from_vars(&vars(&[("OUTER_STORE", "memory")])).unwrap();
        assert_eq!(cfg.ai_gateway, "outer");
        let cfg = Config::from_vars(&vars(&[
            ("OUTER_STORE", "memory"),
            ("OUTER_AI_GATEWAY", "staging"),
        ]))
        .unwrap();
        assert_eq!(cfg.ai_gateway, "staging");
    }

    #[test]
    fn local_dev_runs_on_the_memory_store_with_defaults() {
        let cfg = Config::from_vars(&vars(&[("OUTER_STORE", "memory")])).unwrap();
        assert_eq!(cfg.domain, "outer.localhost");
        assert_eq!(cfg.store, StoreKind::Memory);
        assert_eq!(cfg.listen, "127.0.0.1:8080".parse().unwrap());
        assert!(!cfg.tls_enabled());
        assert_eq!(cfg.db_path(), PathBuf::from("./data/outer.sqlite"));
    }

    #[test]
    fn production_refuses_the_memory_store() {
        let err = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            ("OUTER_STORE", "memory"),
            ("OUTER_TLS_CERT", "/tls/cert.pem"),
            ("OUTER_TLS_KEY", "/tls/key.pem"),
            ("OUTER_ORIGIN_PULL_CA", "/tls/origin-pull-ca.pem"),
        ]))
        .unwrap_err();
        assert!(
            err.iter().any(|p| p.contains("OUTER_STORE=memory")),
            "{err:?}"
        );
    }

    #[test]
    fn r2_reports_every_missing_variable_at_once() {
        let err = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            ("OUTER_TLS_CERT", "/tls/cert.pem"),
            ("OUTER_TLS_KEY", "/tls/key.pem"),
            ("OUTER_ORIGIN_PULL_CA", "/tls/origin-pull-ca.pem"),
        ]))
        .unwrap_err();
        for name in [
            "OUTER_R2_ENDPOINT",
            "OUTER_R2_BUCKET",
            "R2_ACCESS_KEY_ID",
            "R2_SECRET_ACCESS_KEY",
            "CLOUDFLARE_ACCOUNT_ID",
            "CLOUDFLARE_API_TOKEN",
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
            ("CLOUDFLARE_ACCOUNT_ID", "acct"),
            ("CLOUDFLARE_API_TOKEN", "cf-token"),
            ("OUTER_TLS_CERT", "/tls/cert.pem"),
            ("OUTER_TLS_KEY", "/tls/key.pem"),
            ("OUTER_ORIGIN_PULL_CA", "/tls/origin-pull-ca.pem"),
            ("OUTER_TERMS_VERSION", "2026-10-23"),
            ("OUTER_ADMIN_EMAILS", " Owner@Example.com ,"),
            ("OUTER_SIGNUP", "closed"),
        ]))
        .unwrap();
        let StoreKind::R2(r2) = &cfg.store else {
            panic!("expected r2")
        };
        assert_eq!(r2.bucket, "outer");
        let cf = cfg.cloudflare.as_ref().expect("cloudflare pair set");
        assert_eq!(cf.account_id, "acct");
        assert_eq!(cf.credential_ttl, Duration::from_secs(604_800));
        assert_eq!(cf.credential_refresh_after, Duration::from_secs(518_400));
        assert_eq!(cfg.admin_emails, vec!["owner@example.com"]);
        assert!(!cfg.signup_open);
        assert_eq!(cfg.terms_version, "2026-10-23");
        assert_eq!(cfg.oauth_access_ttl, Duration::from_secs(3600));
        assert_eq!(cfg.oauth_refresh_ttl, Duration::from_secs(90 * 86400));
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("s3cret"), "{shown}");
        assert!(!shown.contains("cf-token"), "{shown}");
    }

    #[test]
    fn auth_vars_default_for_localhost_and_validate_off_it() {
        let cfg = Config::from_vars(&vars(&[("OUTER_STORE", "memory")])).unwrap();
        assert!(!cfg.signup_open, "closed beta is the default");
        assert!(cfg.admin_emails.is_empty());
        assert_eq!(cfg.terms_version, "dev");
        assert_eq!(cfg.email_from, "Outer <hello@outer.now>");

        let err = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            ("OUTER_STORE", "memory"),
            ("OUTER_TLS_CERT", "/tls/cert.pem"),
            ("OUTER_TLS_KEY", "/tls/key.pem"),
            ("OUTER_ORIGIN_PULL_CA", "/tls/origin-pull-ca.pem"),
            ("OUTER_SIGNUP", "sometimes"),
            ("OUTER_OAUTH_ACCESS_TTL_S", "0"),
            ("OUTER_OAUTH_REFRESH_TTL_D", "0"),
        ]))
        .unwrap_err();
        for name in [
            "OUTER_SIGNUP",
            "OUTER_TERMS_VERSION",
            "OUTER_OAUTH_ACCESS_TTL_S",
            "OUTER_OAUTH_REFRESH_TTL_D",
        ] {
            assert!(
                err.iter().any(|p| p.contains(name)),
                "missing {name} in {err:?}"
            );
        }

        let cfg = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            ("OUTER_STORE", "memory"),
            ("OUTER_TLS_CERT", "/tls/cert.pem"),
            ("OUTER_TLS_KEY", "/tls/key.pem"),
            ("OUTER_ORIGIN_PULL_CA", "/tls/origin-pull-ca.pem"),
            ("OUTER_TERMS_VERSION", "2026-10-23"),
            ("OUTER_SIGNUP", "open"),
            ("OUTER_OAUTH_ACCESS_TTL_S", "600"),
            ("OUTER_OAUTH_REFRESH_TTL_D", "30"),
        ]));
        // Memory store still refuses off localhost; the auth vars must not add
        // problems of their own.
        let err = cfg.unwrap_err();
        assert!(
            !err.iter()
                .any(|p| p.contains("SIGNUP") || p.contains("TERMS") || p.contains("OAUTH")),
            "valid auth vars must not add problems: {err:?}"
        );
    }

    #[test]
    fn r2_localhost_without_cloudflare_uses_the_parent_keys() {
        let cfg = Config::from_vars(&vars(&[
            ("OUTER_R2_ENDPOINT", "http://127.0.0.1:19000"),
            ("OUTER_R2_BUCKET", "outer-dev"),
            ("R2_ACCESS_KEY_ID", "key"),
            ("R2_SECRET_ACCESS_KEY", "secret"),
        ]))
        .unwrap();
        assert_eq!(cfg.domain, "outer.localhost");
        assert!(cfg.cloudflare.is_none());
    }

    #[test]
    fn r2_off_localhost_without_cloudflare_refuses_to_boot() {
        let err = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            (
                "OUTER_R2_ENDPOINT",
                "https://acct.eu.r2.cloudflarestorage.com",
            ),
            ("OUTER_R2_BUCKET", "outer"),
            ("R2_ACCESS_KEY_ID", "key"),
            ("R2_SECRET_ACCESS_KEY", "secret"),
            ("OUTER_TLS_CERT", "/tls/cert.pem"),
            ("OUTER_TLS_KEY", "/tls/key.pem"),
            ("OUTER_ORIGIN_PULL_CA", "/tls/origin-pull-ca.pem"),
        ]))
        .unwrap_err();
        for name in ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN"] {
            assert!(err.iter().any(|p| p.starts_with(name)), "{err:?}");
        }
    }

    #[test]
    fn cloudflare_half_pair_is_rejected() {
        for present in ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN"] {
            let pairs = vec![
                ("OUTER_R2_ENDPOINT", "http://127.0.0.1:19000"),
                ("OUTER_R2_BUCKET", "outer-dev"),
                ("R2_ACCESS_KEY_ID", "key"),
                ("R2_SECRET_ACCESS_KEY", "secret"),
                (present, "something"),
            ];
            let err = Config::from_vars(&vars(&pairs)).unwrap_err();
            assert!(
                err.iter()
                    .any(|p| p.contains("both be set or both be empty")),
                "half pair with {present} set must be refused: {err:?}"
            );
        }
    }

    #[test]
    fn credential_ttl_defaults_and_derives_refresh() {
        fn ttl_vars(ttl: Option<&str>) -> BTreeMap<String, String> {
            let mut pairs = vec![
                (
                    "OUTER_R2_ENDPOINT".to_string(),
                    "http://127.0.0.1:19000".to_string(),
                ),
                ("OUTER_R2_BUCKET".to_string(), "outer-dev".to_string()),
                ("R2_ACCESS_KEY_ID".to_string(), "key".to_string()),
                ("R2_SECRET_ACCESS_KEY".to_string(), "secret".to_string()),
                ("CLOUDFLARE_ACCOUNT_ID".to_string(), "acct".to_string()),
                ("CLOUDFLARE_API_TOKEN".to_string(), "token".to_string()),
            ];
            if let Some(ttl) = ttl {
                pairs.push(("OUTER_R2_CREDENTIAL_TTL_S".to_string(), ttl.to_string()));
            }
            pairs.into_iter().collect()
        }
        // Default 7 days, refresh 6 days.
        let cf = Config::from_vars(&ttl_vars(None))
            .unwrap()
            .cloudflare
            .unwrap();
        assert_eq!(cf.credential_ttl, Duration::from_secs(604_800));
        assert_eq!(cf.credential_refresh_after, Duration::from_secs(518_400));
        // Minimum TTL: a day no longer fits, so refresh floors at half.
        let cf = Config::from_vars(&ttl_vars(Some("3600")))
            .unwrap()
            .cloudflare
            .unwrap();
        assert_eq!(cf.credential_refresh_after, Duration::from_secs(1800));
        // A 1-day TTL refreshes at half a day; a 2-day TTL at a day.
        let cf = Config::from_vars(&ttl_vars(Some("86400")))
            .unwrap()
            .cloudflare
            .unwrap();
        assert_eq!(cf.credential_refresh_after, Duration::from_secs(43_200));
        let cf = Config::from_vars(&ttl_vars(Some("172800")))
            .unwrap()
            .cloudflare
            .unwrap();
        assert_eq!(cf.credential_refresh_after, Duration::from_secs(86_400));
        // Out of range (and garbage) refuse to boot.
        for bad in ["0", "3599", "604801", "9999999", "an-hour"] {
            let err = Config::from_vars(&ttl_vars(Some(bad))).unwrap_err();
            assert!(
                err.iter().any(|p| p.contains("OUTER_R2_CREDENTIAL_TTL_S")),
                "TTL {bad:?} must be refused: {err:?}"
            );
        }
    }

    #[test]
    fn rejects_bad_listen_address_and_bad_domain_together() {
        let err = Config::from_vars(&vars(&[
            ("OUTER_STORE", "memory"),
            ("OUTER_DOMAIN", "Not A Host"),
            ("OUTER_LISTEN", "eighty"),
        ]))
        .unwrap_err();
        assert!(err.iter().any(|p| p.starts_with("OUTER_LISTEN")), "{err:?}");
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

    #[test]
    fn partial_tls_triple_is_rejected() {
        for missing in ["OUTER_TLS_CERT", "OUTER_TLS_KEY", "OUTER_ORIGIN_PULL_CA"] {
            let mut pairs = vec![
                ("OUTER_STORE", "memory"),
                ("OUTER_TLS_CERT", "/tls/cert.pem"),
                ("OUTER_TLS_KEY", "/tls/key.pem"),
                ("OUTER_ORIGIN_PULL_CA", "/tls/origin-pull-ca.pem"),
            ];
            pairs.retain(|(name, _)| *name != missing);
            let err = Config::from_vars(&vars(&pairs)).unwrap_err();
            assert!(
                err.iter().any(|p| p.contains("partial TLS config")),
                "missing partial-triple problem without {missing}: {err:?}"
            );
            assert!(
                err.iter().any(|p| p.contains(missing)),
                "missing variable name {missing} in {err:?}"
            );
        }
    }

    #[test]
    fn non_localhost_domain_requires_tls() {
        let err = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            ("OUTER_STORE", "memory"),
            ("OUTER_TLS_CERT", "/tls/cert.pem"),
            ("OUTER_TLS_KEY", "/tls/key.pem"),
            ("OUTER_ORIGIN_PULL_CA", "/tls/origin-pull-ca.pem"),
        ]))
        .unwrap_err();
        assert!(
            !err.iter().any(|p| p.contains("OUTER_TLS_CERT")),
            "full triple on a non-localhost domain must not raise TLS problems: {err:?}"
        );
        let err = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            ("OUTER_STORE", "memory"),
        ]))
        .unwrap_err();
        assert!(
            err.iter()
                .any(|p| p.contains("OUTER_TLS_CERT") && p.contains("outer.now")),
            "non-localhost domain without TLS must be refused: {err:?}"
        );
    }

    #[test]
    fn localhost_without_tls_stays_valid() {
        let cfg = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "apps.localhost"),
            ("OUTER_STORE", "memory"),
        ]))
        .unwrap();
        assert!(!cfg.tls_enabled());
    }
}
