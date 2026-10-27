//! Per-app egress firewall: install the `table inet outer` policy at boot,
//! refresh it when its inputs change, and refuse to start fleets or builds
//! when it is missing.
//!
//! The ruleset itself lives in [`super::nft`]: it rejects the whole reserved
//! app-uid range from loopback, RFC1918/link-local/CGNAT/ULA ranges and SMTP
//! ports 25/465/587, accepts DNS (matched on the connection's original
//! destination port, so it works whatever resolver the host uses —
//! systemd-resolved's 127.0.0.53 stub, Docker's embedded 127.0.0.11, or a
//! plain upstream), accepts the object-store endpoint first, and keeps
//! per-uid byte counters (no connection rate limit: see `nft::ruleset`). The chain
//! policy stays `accept`, so public-Internet egress (npm, R2, webhooks an
//! app calls) keeps working; replies are accepted first, so the proxy (which
//! runs as root, not as an app uid) reaching a fleet's loopback port is
//! unaffected. Only *new* connections *from* an app uid to the closed
//! destinations are rejected.
//!
//! What this module adds over `nft`:
//! - [`EgressMode`]: `required` (default) or `off` (`OUTER_EGRESS_FIREWALL`).
//!   `off` is refused for non-localhost domains at config time.
//! - [`classify_uids`]: the uid → plan-class inputs, read from the app and
//!   user tables (preview fleets share their app's uid, so they need no
//!   separate rows).
//! - [`EgressFirewall`]: the explicit handle `serve` builds at boot.
//!   [`EgressFirewall::boot`] installs before any fleet or build runs and
//!   fails closed in production; [`EgressFirewall::gate`] refuses wakes and
//!   deploys while the table is missing; [`EgressFirewall::refresh`]
//!   reinstalls best-effort after an app create/delete or a plan change (a
//!   failed refresh keeps the old table and logs loudly — the range rejects
//!   never go stale, only the per-uid rate classes lag).
//!
//! The storage allowance needs the endpoint's IPs: R2 is public HTTPS, so
//! the allowance is belt-and-braces for odd DNS, resolved at each install
//! via [`resolve_storage_addrs`].

use std::net::IpAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use sqlx::SqlitePool;

use super::nft::{self, NftConfig};

/// `OUTER_EGRESS_FIREWALL=required|off` (default `required`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressMode {
    /// Install at boot; refuse fleets and builds while it is missing.
    Required,
    /// Skip entirely. Local dev and tests only; config refuses it for any
    /// non-localhost domain.
    Off,
}

impl EgressMode {
    /// Parse the env value. Empty/missing means the default (`required`).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim).filter(|v| !v.is_empty()) {
            None | Some("required") => Ok(EgressMode::Required),
            Some("off") => Ok(EgressMode::Off),
            Some(other) => Err(format!(
                "OUTER_EGRESS_FIREWALL={other:?} is not one of: required, off"
            )),
        }
    }
}

/// `(lo, hi)` inclusive uid range → `(base, count)` for [`nft::ruleset`].
pub fn range_to_base_count(range: (u32, u32)) -> (u32, u32) {
    (range.0, range.1.saturating_sub(range.0).saturating_add(1))
}

/// Split an object-store endpoint URL into `(host, port)`. The endpoint is
/// always `https://…` in production (`config` enforces the scheme off
/// localhost); the port defaults to 443 when the URL names none.
pub fn split_host_port(endpoint: &str) -> (String, u16) {
    let after_scheme = endpoint.rsplit("://").next().unwrap_or(endpoint);
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    match host_port.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => match port.parse::<u16>() {
            Ok(port) => (host.to_string(), port),
            Err(_) => (host_port.to_string(), 443),
        },
        _ => (host_port.to_string(), 443),
    }
}

/// `nameserver` addresses from resolv.conf text (IPv4 and IPv6, comments
/// and other directives ignored). Pure so tests pin it.
pub fn parse_resolv_conf(text: &str) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("nameserver") else {
            continue;
        };
        let addr = rest.split([' ', '\t', '#']).find(|p| !p.is_empty());
        if let Some(addr) = addr.and_then(|a| a.parse::<IpAddr>().ok()) {
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
    }
    out
}

/// The host's resolvers from `/etc/resolv.conf`, best-effort (empty when
/// unreadable). Diagnostic only: the ruleset's DNS accept matches on the
/// connection's original destination port, so it already covers whatever
/// resolver this reports — the boot log prints both together for the
/// operator to eyeball.
pub fn system_resolvers() -> Vec<IpAddr> {
    std::fs::read_to_string("/etc/resolv.conf")
        .map(|text| parse_resolv_conf(&text))
        .unwrap_or_default()
}

/// Resolve the storage hostname to IPs for the endpoint allowance.
/// Best-effort: empty on DNS failure (the caller logs it). R2 is public
/// HTTPS, so even an empty list leaves production traffic working — the
/// allowance only matters if the endpoint ever resolves privately.
pub async fn resolve_storage_addrs(host: &str, port: u16) -> Vec<IpAddr> {
    let target = format!("{host}:{port}");
    match tokio::net::lookup_host(target).await {
        Ok(addrs) => {
            let mut out: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
            out.sort();
            out.dedup();
            out
        }
        Err(err) => {
            tracing::warn!(
                host = %host,
                error = %format!("{err:#}"),
                "storage endpoint did not resolve; installing without the storage allowance"
            );
            Vec::new()
        }
    }
}

/// What the ruleset needs from the live rows: every app's uid classed by its
/// owner's `user.plan` (missing account rows count as free, the same
/// default the MCP layer uses), sorted and deduplicated, plus each uid's own
/// loopback ports (live and preview, public and internal).
pub struct UidClasses {
    pub free: Vec<u32>,
    pub pro: Vec<u32>,
    pub own_ports: Vec<(u32, Vec<u16>)>,
}

pub async fn classify_uids(pool: &SqlitePool) -> anyhow::Result<UidClasses> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    )> = sqlx::query_as(
        "SELECT a.uid, u.plan, a.port, a.internal_port, a.preview_port, a.preview_internal_port \
             FROM app AS a LEFT JOIN user AS u ON u.id = a.owner_id",
    )
    .fetch_all(pool)
    .await?;
    let mut free = Vec::new();
    let mut pro = Vec::new();
    let mut own_ports = Vec::new();
    for (uid, plan, port, internal, preview, preview_internal) in rows {
        let Ok(uid) = u32::try_from(uid) else {
            continue;
        };
        if plan.as_deref() == Some("pro") {
            pro.push(uid);
        } else {
            free.push(uid);
        }
        let mut ports: Vec<u16> = [port, internal, preview, preview_internal]
            .into_iter()
            .flatten()
            .filter_map(|p| u16::try_from(p).ok())
            .filter(|p| *p != 0)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        own_ports.push((uid, ports));
    }
    free.sort_unstable();
    free.dedup();
    pro.sort_unstable();
    pro.dedup();
    own_ports.sort_unstable();
    Ok(UidClasses {
        free,
        pro,
        own_ports,
    })
}

#[derive(Clone)]
enum Apply {
    Live,
    Test(TestApply),
}

#[derive(Clone)]
struct TestApply {
    rules: Arc<Mutex<Vec<String>>>,
    fail: Arc<AtomicBool>,
}

/// The installed-policy handle. Clone shares one installation state.
/// Built once at boot from config; no globals.
#[derive(Clone)]
pub struct EgressFirewall {
    inner: Arc<Inner>,
}

struct Inner {
    mode: EgressMode,
    uid_range: (u32, u32),
    storage_host: String,
    storage_port: u16,
    /// Fixed endpoint IPs for tests (live DNS otherwise).
    fixed_addrs: Option<Vec<IpAddr>>,
    apply: Apply,
    /// False after [`EgressFirewall::stand_down`]: the localhost escape
    /// hatch after a failed install, never used in production.
    enforce: AtomicBool,
    installed: AtomicBool,
}

impl EgressFirewall {
    /// The production handle: live `nft` apply.
    pub fn new(
        mode: EgressMode,
        uid_range: (u32, u32),
        storage_host: String,
        storage_port: u16,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                enforce: AtomicBool::new(mode == EgressMode::Required),
                mode,
                uid_range,
                storage_host,
                storage_port,
                fixed_addrs: None,
                apply: Apply::Live,
                installed: AtomicBool::new(false),
            }),
        }
    }

    /// No isolation, no-op installs, the gate always passes. Tests and the
    /// operator CLI paths that must not fight the server's table.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(Inner {
                mode: EgressMode::Off,
                uid_range: (0, 0),
                storage_host: String::new(),
                storage_port: 443,
                fixed_addrs: Some(Vec::new()),
                apply: Apply::Test(TestApply {
                    rules: Arc::new(Mutex::new(Vec::new())),
                    fail: Arc::new(AtomicBool::new(false)),
                }),
                enforce: AtomicBool::new(false),
                installed: AtomicBool::new(false),
            }),
        }
    }

    /// A handle that records installed rulesets instead of calling `nft`.
    /// Returns the handle plus the recorded-rules log (and a fail switch:
    /// set it to make the next install fail).
    pub fn for_test(
        uid_range: (u32, u32),
        storage_host: &str,
        storage_port: u16,
        storage_addrs: Vec<IpAddr>,
    ) -> (Self, Arc<Mutex<Vec<String>>>, Arc<AtomicBool>) {
        let rules = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicBool::new(false));
        let handle = Self {
            inner: Arc::new(Inner {
                mode: EgressMode::Required,
                uid_range,
                storage_host: storage_host.to_string(),
                storage_port,
                fixed_addrs: Some(storage_addrs),
                apply: Apply::Test(TestApply {
                    rules: rules.clone(),
                    fail: fail.clone(),
                }),
                enforce: AtomicBool::new(true),
                installed: AtomicBool::new(false),
            }),
        };
        (handle, rules, fail)
    }

    pub fn mode(&self) -> EgressMode {
        self.inner.mode
    }

    /// True once an install has succeeded (test seam included).
    pub fn is_installed(&self) -> bool {
        self.inner.installed.load(Ordering::SeqCst)
    }

    /// Refuse `what` (a wake or a build) while the required policy is not
    /// installed. Cheap and sync: call it on the hot path.
    pub fn gate(&self, what: &str) -> anyhow::Result<()> {
        if self.inner.enforce.load(Ordering::SeqCst) && !self.is_installed() {
            anyhow::bail!(
                "per-app egress firewall (table inet outer) is not installed; refusing to {what}: \
                 fix nft/CAP_NET_ADMIN or set OUTER_EGRESS_FIREWALL=off (local dev only)"
            );
        }
        Ok(())
    }

    /// Localhost escape hatch after a failed boot install: loud at the call
    /// site, then the gate passes. Production never calls this.
    pub fn stand_down(&self) {
        self.inner.enforce.store(false, Ordering::SeqCst);
    }

    /// Build the ruleset from the live tables and install it (idempotent:
    /// the ruleset atomically replaces the table). `Off` is a no-op.
    /// Records success for [`EgressFirewall::gate`].
    pub async fn install(&self, pool: &SqlitePool) -> anyhow::Result<()> {
        if self.inner.mode == EgressMode::Off {
            return Ok(());
        }
        let UidClasses {
            free: free_uids,
            pro: pro_uids,
            own_ports,
        } = classify_uids(pool).await?;
        let storage_addrs = match &self.inner.fixed_addrs {
            Some(addrs) => addrs.clone(),
            None => resolve_storage_addrs(&self.inner.storage_host, self.inner.storage_port).await,
        };
        let rules = nft::ruleset(&NftConfig {
            uid_range: range_to_base_count(self.inner.uid_range),
            storage_addrs: storage_addrs.clone(),
            storage_port: self.inner.storage_port,
            free_uids: free_uids.clone(),
            pro_uids: pro_uids.clone(),
            own_ports,
        });
        match &self.inner.apply {
            Apply::Live => nft::apply(&rules).await?,
            Apply::Test(test) => {
                if test.fail.load(Ordering::SeqCst) {
                    anyhow::bail!("fake nft apply failed");
                }
                test.rules.lock().expect("rules log").push(rules);
            }
        }
        self.inner.installed.store(true, Ordering::SeqCst);
        tracing::info!(
            free_apps = free_uids.len(),
            pro_apps = pro_uids.len(),
            storage_host = %self.inner.storage_host,
            storage_addrs = ?storage_addrs,
            resolvers = ?system_resolvers(),
            "per-app egress firewall installed (table inet outer)"
        );
        Ok(())
    }

    /// Install at boot, before any fleet or build runs. Fails closed in
    /// production; on a localhost dev domain a failed install logs loudly
    /// and stands down (set `OUTER_EGRESS_FIREWALL=off` to silence it).
    pub async fn boot(&self, pool: &SqlitePool, local: bool) -> anyhow::Result<()> {
        match self.install(pool).await {
            Ok(()) => Ok(()),
            Err(err) if local && self.inner.mode == EgressMode::Required => {
                tracing::error!(
                    error = %format!("{err:#}"),
                    "per-app egress firewall NOT installed; continuing WITHOUT isolation \
                     because this is a localhost dev domain (production refuses to boot). \
                     Set OUTER_EGRESS_FIREWALL=off to silence this"
                );
                self.stand_down();
                Ok(())
            }
            Err(err) => Err(err.context(
                "per-app egress firewall install failed; refusing to boot without isolation \
                 (OUTER_EGRESS_FIREWALL=off is local-dev only)",
            )),
        }
    }

    /// Reinstall after an input changed (app create/delete, plan change).
    /// Best-effort: a failure keeps the old table (the range rejects never
    /// go stale, only per-uid rate classes lag) and logs loudly.
    pub async fn refresh(&self, pool: &SqlitePool) {
        if self.inner.mode == EgressMode::Off {
            return;
        }
        if let Err(err) = self.install(pool).await {
            tracing::error!(
                error = %format!("{err:#}"),
                "per-app egress firewall refresh failed; old table stays in place"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pool() -> SqlitePool {
        SqlitePool::connect_lazy("sqlite::memory:").expect("memory pool")
    }

    async fn seed(pool: &SqlitePool) {
        sqlx::query("CREATE TABLE user (id TEXT PRIMARY KEY, plan TEXT)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE app (id TEXT PRIMARY KEY, slug TEXT, owner_id TEXT, uid INTEGER, \
             port INTEGER, internal_port INTEGER, preview_port INTEGER, \
             preview_internal_port INTEGER, last_activity_at INTEGER, created_at INTEGER)",
        )
        .execute(pool)
        .await
        .unwrap();
        for (id, plan) in [("a1", "free"), ("a2", "pro"), ("a3", "pro")] {
            sqlx::query("INSERT INTO user (id, plan) VALUES (?1, ?2)")
                .bind(id)
                .bind(plan)
                .execute(pool)
                .await
                .unwrap();
        }
        // a3 owns two apps; ghost's account row is missing (counts as free).
        for (id, slug, owner, uid) in [
            ("app1", "one", "a1", 200001),
            ("app2", "two", "a2", 200002),
            ("app3", "three", "a3", 200003),
            ("app4", "four", "ghost", 200004),
        ] {
            sqlx::query(
                "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
                 last_activity_at, created_at) VALUES (?1, ?2, ?3, ?4, 0, 0, 0, 0)",
            )
            .bind(id)
            .bind(slug)
            .bind(owner)
            .bind(uid)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    #[test]
    fn mode_parses_with_a_required_default() {
        assert_eq!(EgressMode::parse(None).unwrap(), EgressMode::Required);
        assert_eq!(EgressMode::parse(Some("")).unwrap(), EgressMode::Required);
        assert_eq!(
            EgressMode::parse(Some("required")).unwrap(),
            EgressMode::Required
        );
        assert_eq!(EgressMode::parse(Some("off")).unwrap(), EgressMode::Off);
        assert!(EgressMode::parse(Some("sometimes")).is_err());
    }

    #[test]
    fn inclusive_range_becomes_base_and_count() {
        assert_eq!(range_to_base_count((200_000, 201_023)), (200_000, 1024));
        assert_eq!(range_to_base_count((7, 7)), (7, 1));
    }

    #[test]
    fn endpoint_splits_host_and_port() {
        assert_eq!(
            split_host_port("https://acct.eu.r2.cloudflarestorage.com"),
            ("acct.eu.r2.cloudflarestorage.com".to_string(), 443)
        );
        assert_eq!(
            split_host_port("http://127.0.0.1:19000/bucket"),
            ("127.0.0.1".to_string(), 19000)
        );
    }

    #[test]
    fn resolv_conf_parses_nameservers_only() {
        let text = "# stub\nnameserver 127.0.0.53\noptions edns0\nnameserver 8.8.8.8 # inline\nnameserver ::1\n";
        assert_eq!(
            parse_resolv_conf(text),
            vec![
                "127.0.0.53".parse::<IpAddr>().unwrap(),
                "8.8.8.8".parse::<IpAddr>().unwrap(),
                "::1".parse::<IpAddr>().unwrap(),
            ]
        );
        assert!(parse_resolv_conf("search lan\n").is_empty());
    }

    #[tokio::test]
    async fn classify_groups_uids_by_owner_plan() {
        let pool = test_pool();
        seed(&pool).await;
        let classes = classify_uids(&pool).await.unwrap();
        assert_eq!(classes.free, vec![200001, 200004]);
        assert_eq!(classes.pro, vec![200002, 200003]);
    }

    /// Each uid gets exactly its own live and preview ports; unset (NULL)
    /// and unassigned (0) ports are left out.
    #[tokio::test]
    async fn classify_collects_each_apps_own_ports() {
        let pool = test_pool();
        seed(&pool).await;
        sqlx::query(
            "UPDATE app SET port = 21001, internal_port = 20001, preview_port = 22001, \
             preview_internal_port = NULL WHERE id = 'app1'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let classes = classify_uids(&pool).await.unwrap();
        let own: std::collections::HashMap<u32, Vec<u16>> = classes.own_ports.into_iter().collect();
        assert_eq!(own[&200001], vec![20001, 21001, 22001]);
        assert!(own[&200002].is_empty());
    }

    #[tokio::test]
    async fn install_records_rules_with_plan_classes() {
        let pool = test_pool();
        seed(&pool).await;
        let (fw, rules, _fail) =
            EgressFirewall::for_test((200_000, 200_007), "r2.example.com", 443, vec![]);
        fw.install(&pool).await.unwrap();
        assert!(fw.is_installed());
        let rules = rules.lock().unwrap();
        assert_eq!(rules.len(), 1);
        assert!(rules[0].contains("meta skuid 200000-200007 "));
        // Per-uid rules are byte counters only, for every app uid, ghost
        // rows (no account) included.
        for uid in [200001, 200002, 200004] {
            assert!(rules[0].contains(&format!("counter app_{uid} {{")), "{uid}");
        }
    }

    #[tokio::test]
    async fn reinstall_after_create_and_plan_change_reclasses() {
        let pool = test_pool();
        seed(&pool).await;
        let (fw, log, _fail) =
            EgressFirewall::for_test((200_000, 200_007), "r2.example.com", 443, vec![]);
        fw.install(&pool).await.unwrap();
        // A new app's uid joins the free class on the next refresh.
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
             last_activity_at, created_at) VALUES ('app5', 'five', 'a1', 200005, 0, 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // And an upgrade moves the owner to the pro class.
        sqlx::query("UPDATE user SET plan = 'pro' WHERE id = 'a1'")
            .execute(&pool)
            .await
            .unwrap();
        fw.refresh(&pool).await;
        assert_eq!(log.lock().unwrap().len(), 2);
        assert!(log.lock().unwrap()[1].contains("counter app_200005 {"));
        // A delete drops the uid's rules on the next refresh.
        sqlx::query("DELETE FROM app WHERE id = 'app1'")
            .execute(&pool)
            .await
            .unwrap();
        fw.refresh(&pool).await;
        let rules = log.lock().unwrap();
        assert_eq!(rules.len(), 3);
        assert!(!rules[2].contains("meta skuid 200001 "));
    }

    #[tokio::test]
    async fn failed_install_fails_closed_until_stood_down() {
        let pool = test_pool();
        seed(&pool).await;
        let (fw, rules, fail) =
            EgressFirewall::for_test((200_000, 200_007), "r2.example.com", 443, vec![]);
        fail.store(true, Ordering::SeqCst);
        assert!(fw.install(&pool).await.is_err());
        assert!(!fw.is_installed());
        assert!(rules.lock().unwrap().is_empty());
        // Wakes and builds refuse while the required table is missing.
        assert!(fw.gate("wake the fleet").is_err());
        assert!(fw.gate("run the build").is_err());
        // The localhost escape hatch (boot() on a dev domain) stands down.
        fw.stand_down();
        assert!(fw.gate("wake the fleet").is_ok());
    }

    #[tokio::test]
    async fn boot_fails_closed_in_production() {
        let pool = test_pool();
        seed(&pool).await;
        let (fw, _rules, fail) =
            EgressFirewall::for_test((200_000, 200_007), "r2.example.com", 443, vec![]);
        fail.store(true, Ordering::SeqCst);
        assert!(fw.boot(&pool, false).await.is_err());
    }

    #[tokio::test]
    async fn boot_warns_and_stands_down_on_localhost() {
        let pool = test_pool();
        seed(&pool).await;
        let (fw, _rules, fail) =
            EgressFirewall::for_test((200_000, 200_007), "r2.example.com", 443, vec![]);
        fail.store(true, Ordering::SeqCst);
        assert!(fw.boot(&pool, true).await.is_ok());
        assert!(fw.gate("wake the fleet").is_ok());
    }

    #[tokio::test]
    async fn off_mode_never_installs_and_never_refuses() {
        let pool = test_pool();
        let off = EgressFirewall::disabled();
        assert!(off.install(&pool).await.is_ok());
        assert!(!off.is_installed());
        assert!(off.gate("anything").is_ok());
        off.refresh(&pool).await;
    }
}
