//! Custom domains (spec §7.7, Pro, one per app): validation, the
//! `app_domain` rows, and the Cloudflare for SaaS calls behind the MCP
//! `add_domain` / `remove_domain` / `list_domains` tools, the console
//! Settings card, the app proxy, and `delete_app`.
//!
//! `add_domain` registers the hostname as a Cloudflare for SaaS custom
//! hostname with HTTP DCV, stores the row as `pending`, and returns the DNS
//! records the user creates at their provider
//! (`CNAME <hostname> → <cname_target>`). `refresh_domain` re-reads the
//! hostname from Cloudflare (the GET endpoint is the source of truth for
//! onboarding state) and moves the row `pending → active|failed`. The proxy
//! serves only `active` rows; `pending`/`failed` ones get a setup page.

use crate::cloudflare::purge_host_cache;
use crate::cloudflare::{CustomHostname, CustomHostnames};

/// One custom domain on an app, as agents and the console see it.
#[derive(Debug, Clone)]
pub struct DomainView {
    pub hostname: String,
    pub status: String,
    pub ssl_status: String,
    /// Where the user's CNAME points (`domains.<platform domain>`).
    pub cname_target: String,
    /// TXT records to create at the user's DNS (ownership + DCV), if known.
    pub txt_records: Vec<TxtRecord>,
    /// Why a `failed` domain failed (the Cloudflare status text).
    pub reason: Option<String>,
}

/// One TXT record the user creates at their DNS provider.
#[derive(Debug, Clone)]
pub struct TxtRecord {
    pub name: String,
    pub value: String,
}

/// Why a domain call was refused. Every variant carries the human text: the
/// MCP layer maps it to a `ToolError`, the console to `{error, message,
/// user_message}` with the same codes.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// The hostname breaks the shape rules or names something reserved.
    #[error("{0}")]
    InvalidArgument(String),
    /// Free plan: custom domains are Pro-only (spec §7.7).
    #[error("custom domains need Outer Pro")]
    ProRequired,
    /// The app already has its one domain.
    #[error("app {0:?} already has custom domain {1:?}")]
    AlreadyHas(String, String),
    /// The hostname is taken: by another app here, or by some zone at
    /// Cloudflare already.
    #[error("hostname {0:?} is already registered")]
    Conflict(String),
    /// No such domain on the app.
    #[error("no custom domain {0:?} on app {1:?}")]
    NotFound(String, String),
    /// The server has no Cloudflare zone configured, or Cloudflare failed.
    #[error("{0}")]
    Unavailable(String),
    /// The database or something else broke mid-call.
    #[error("internal: {0}")]
    Internal(String),
}

pub fn internal(err: impl Into<anyhow::Error>) -> DomainError {
    DomainError::Internal(format!("{:#}", err.into()))
}

impl DomainError {
    /// The spec §8.3 code the MCP tool and the console endpoint report.
    pub fn code(&self) -> &'static str {
        match self {
            DomainError::InvalidArgument(_) => "invalid_argument",
            DomainError::ProRequired => "payment_required",
            DomainError::AlreadyHas(_, _) => "invalid_argument",
            DomainError::Conflict(_) => "invalid_argument",
            DomainError::NotFound(_, _) => "not_found",
            DomainError::Unavailable(_) => "internal",
            DomainError::Internal(_) => "internal",
        }
    }

    /// The human sentence the agent relays word for word (spec §8.3).
    /// `launch_discount_active` is the live Stripe promo state: the $4
    /// launch price is named only while it applies.
    pub fn user_message(&self, launch_discount_active: bool) -> String {
        match self {
            DomainError::InvalidArgument(_)
            | DomainError::AlreadyHas(_, _)
            | DomainError::Conflict(_)
            | DomainError::NotFound(_, _) => self.to_string(),
            DomainError::ProRequired if launch_discount_active => "Custom domains need Outer Pro ($8/month, $4 at the launch price). Upgrade in billing, then call add_domain again.".to_string(),
            DomainError::ProRequired => "Custom domains need Outer Pro ($8/month). Upgrade in billing, then call add_domain again.".to_string(),
            DomainError::Unavailable(_) => self.to_string(),
            DomainError::Internal(_) => "Something broke on Outer's side; try again in a bit.".to_string(),
        }
    }
}

/// Lowercase, strip one trailing dot, then enforce the DNS shape: ≤ 253
/// chars total, labels of 1–63 `[a-z0-9-]` not starting or ending with `-`.
/// Non-ASCII is refused with a pointer at the punycode form (no IDN
/// conversion server-side: what the user registers must be exactly what the
/// DNS and the `Host` header carry). IPs never qualify (a custom domain is
/// a name the user points at us with a CNAME).
pub fn normalize_hostname(raw: &str) -> Result<String, DomainError> {
    let host = raw.trim().trim_matches('.').to_lowercase();
    let bad = |why: &str| DomainError::InvalidArgument(format!("bad hostname {raw:?}: {why}"));
    if host.is_empty() || host.len() > 253 {
        return Err(bad("use a DNS name of 1-253 characters"));
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Err(bad(
            "use a DNS name, not an IP address (point a CNAME at us instead)",
        ));
    }
    if !host.is_ascii() {
        return Err(bad(
            "international names must use their punycode (xn--) form, exactly as registered in DNS",
        ));
    }
    let mut labels = 0;
    for label in host.split('.') {
        labels += 1;
        if label.is_empty() || label.len() > 63 {
            return Err(bad("each dot-separated label must be 1-63 characters"));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(bad("labels may only hold letters, digits and hyphens"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(bad("labels may not start or end with a hyphen"));
        }
    }
    if labels < 2 {
        return Err(bad("use a full domain name like app.example.com"));
    }
    Ok(host)
}

/// Refuse our own platform and app hosts: a custom domain may never be
/// `outer.now`, a subdomain of it, an app host, or the CNAME target
/// itself (which would loop back onto the fallback origin).
pub fn check_platform(
    hostname: &str,
    platform_domain: &str,
    apps_domain: &str,
    cname_target: &str,
) -> Result<(), DomainError> {
    let clean = |d: &str| d.trim().trim_matches('.').to_lowercase();
    let platform = clean(platform_domain);
    let apps = clean(apps_domain);
    let under = |suffix: &str| {
        !suffix.is_empty() && (hostname == suffix || hostname.ends_with(&format!(".{suffix}")))
    };
    if under(&platform) || (apps != platform && under(&apps)) {
        return Err(DomainError::InvalidArgument(format!(
            "bad hostname {hostname:?}: custom domains can't be the platform domain or a subdomain of it"
        )));
    }
    if under(&cname_target.to_lowercase()) {
        return Err(DomainError::InvalidArgument(format!(
            "bad hostname {hostname:?}: that name serves Outer itself"
        )));
    }
    Ok(())
}

/// Where the user's CNAME points (spec §7.7): the zone's CNAME target,
/// which the operator points at the fallback origin.
pub fn cname_target(platform_domain: &str) -> String {
    format!("domains.{}", platform_domain.trim().trim_matches('.'))
}

fn view_of(
    hostname: String,
    status: String,
    ssl_status: String,
    reason: Option<String>,
    cname: String,
    live: Option<&CustomHostname>,
) -> DomainView {
    let mut txt_records = Vec::new();
    if let Some(live) = live {
        for rec in [&live.ownership_txt, &live.dcv_txt].into_iter().flatten() {
            if !rec.name.is_empty() && !rec.value.is_empty() {
                txt_records.push(TxtRecord {
                    name: rec.name.clone(),
                    value: rec.value.clone(),
                });
            }
        }
    }
    DomainView {
        hostname,
        status,
        ssl_status,
        reason,
        cname_target: cname,
        txt_records,
    }
}

/// The stored row for `hostname`, if any.
pub async fn stored(
    pool: &sqlx::SqlitePool,
    hostname: &str,
) -> Result<Option<StoredDomain>, DomainError> {
    sqlx::query_as::<_, StoredDomain>(
        "SELECT hostname, app_id, cf_custom_hostname_id, status, created_at FROM app_domain WHERE hostname = ?1",
    )
    .bind(hostname)
    .fetch_optional(pool)
    .await
    .map_err(internal)
}

/// One `app_domain` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredDomain {
    pub hostname: String,
    pub app_id: String,
    pub cf_custom_hostname_id: String,
    pub status: String,
    pub created_at: i64,
}

/// The app's domain row, if it has one (one per app, spec §7.7).
pub async fn stored_for_app(
    pool: &sqlx::SqlitePool,
    app_id: &str,
) -> Result<Option<StoredDomain>, DomainError> {
    sqlx::query_as::<_, StoredDomain>(
        "SELECT hostname, app_id, cf_custom_hostname_id, status, created_at FROM app_domain WHERE app_id = ?1",
    )
    .bind(app_id)
    .fetch_optional(pool)
    .await
    .map_err(internal)
}

/// Add `hostname` to the app: Pro gate, shape rules, the one-per-app cap,
/// then the Cloudflare custom hostname (HTTP DCV). Returns the view with
/// the DNS records to show the user. The row lands `pending` (Cloudflare
/// validates the hostname and issues the certificate asynchronously);
/// `refresh_domain` moves it to `active`.
pub async fn add_domain(
    pool: &sqlx::SqlitePool,
    cf: Option<&CustomHostnames>,
    platform_domain: &str,
    apps_domain: &str,
    plan: &str,
    app: &crate::apps::App,
    hostname_raw: &str,
) -> Result<DomainView, DomainError> {
    if plan != "pro" {
        return Err(DomainError::ProRequired);
    }
    let hostname = normalize_hostname(hostname_raw)?;
    let target = cname_target(platform_domain);
    check_platform(&hostname, platform_domain, apps_domain, &target)?;
    if let Some(existing) = stored_for_app(pool, &app.id).await? {
        return Err(DomainError::AlreadyHas(app.slug.clone(), existing.hostname));
    }
    if stored(pool, &hostname).await?.is_some() {
        return Err(DomainError::Conflict(hostname));
    }
    let Some(cf) = cf else {
        return Err(DomainError::Unavailable(
            "custom domains aren't configured on this server (no Cloudflare zone)".to_string(),
        ));
    };
    let created = cf.create(&hostname).await.map_err(map_cf)?;
    // The hostname may have raced onto another app between the check and
    // the insert: the UNIQUE hostname keeps one row, and a lost race
    // removes the Cloudflare hostname it just created so nothing dangles.
    let now = crate::apps::now_ms();
    let inserted = sqlx::query(
        "INSERT INTO app_domain (hostname, app_id, cf_custom_hostname_id, status, created_at) VALUES (?1, ?2, ?3, 'pending', ?4)",
    )
    .bind(&hostname)
    .bind(&app.id)
    .bind(&created.id)
    .bind(now)
    .execute(pool)
    .await;
    if inserted.is_err() {
        let _ = cf.delete(&created.id).await;
        return Err(DomainError::Conflict(hostname));
    }
    // The POST may omit validation records: one GET fills in the DNS
    // records to show the user. A failed GET keeps the pending row; the
    // records arrive on the next refresh.
    let live = cf.get(&created.id).await.ok();
    let (status, ssl_status) = live
        .as_ref()
        .map(status_of)
        .unwrap_or_else(|| ("pending".to_string(), created.ssl_status.clone()));
    if status != "pending" {
        set_status(pool, &hostname, &status).await?;
    }
    Ok(view_of(
        hostname,
        status,
        ssl_status,
        None,
        target,
        live.as_ref(),
    ))
}

/// Re-read the app's domain from Cloudflare and move the stored status.
/// Returns the fresh view (with DNS records while still pending).
pub async fn refresh_domain(
    pool: &sqlx::SqlitePool,
    cf: Option<&CustomHostnames>,
    platform_domain: &str,
    app: &crate::apps::App,
) -> Result<DomainView, DomainError> {
    let row = stored_for_app(pool, &app.id)
        .await?
        .ok_or_else(|| DomainError::NotFound(String::new(), app.slug.clone()))?;
    let target = cname_target(platform_domain);
    let Some(cf) = cf else {
        return Ok(view_of(
            row.hostname,
            row.status,
            String::new(),
            None,
            target,
            None,
        ));
    };
    match cf.get(&row.cf_custom_hostname_id).await {
        Ok(live) => {
            let (status, ssl_status) = status_of(&live);
            let reason = (!live.is_active() && live.is_failed())
                .then(|| reason_of(&live))
                .flatten();
            if status != row.status {
                set_status(pool, &row.hostname, &status).await?;
            }
            Ok(view_of(
                row.hostname,
                status,
                ssl_status,
                reason,
                target,
                Some(&live),
            ))
        }
        Err(crate::cloudflare::CfError::NotFound) => {
            // Deleted at Cloudflare outside Outer: the row can never go
            // active. Mark it failed so the user sees why, instead of
            // polling a hostname that no longer exists.
            set_status(pool, &row.hostname, "failed").await?;
            Ok(view_of(
                row.hostname,
                "failed".to_string(),
                String::new(),
                Some("the custom hostname was removed at Cloudflare; remove it here and add it again".to_string()),
                target,
                None,
            ))
        }
        Err(err) => Err(map_cf(err)),
    }
}

/// The app's domain with stored status only (no Cloudflare round trip),
/// for `list_domains` when the caller wants it cheap. The console and MCP
/// list paths use [`refresh_domain`] instead so the badges are live.
pub async fn list_domains(
    pool: &sqlx::SqlitePool,
    platform_domain: &str,
    app: &crate::apps::App,
) -> Result<Vec<DomainView>, DomainError> {
    let target = cname_target(platform_domain);
    let row = stored_for_app(pool, &app.id).await?;
    Ok(row
        .map(|r| view_of(r.hostname, r.status, String::new(), None, target, None))
        .into_iter()
        .collect())
}

/// Remove the app's domain: best-effort Cloudflare delete (a 404 there is
/// success — the hostname is gone either way), then the row, then the edge
/// cache for the host so a cached page can't outlive the removal.
/// `expected` is the caller's `hostname`, when given: it must match the
/// stored row (normalized both sides) or the call is refused.
pub async fn remove_domain(
    pool: &sqlx::SqlitePool,
    cf: Option<&CustomHostnames>,
    purge: Option<&crate::cloudflare::CachePurge>,
    app: &crate::apps::App,
    expected: Option<&str>,
) -> Result<String, DomainError> {
    let row = stored_for_app(pool, &app.id)
        .await?
        .ok_or_else(|| DomainError::NotFound(String::new(), app.slug.clone()))?;
    if let Some(want) = expected {
        let want = normalize_hostname(want)?;
        if want != row.hostname {
            return Err(DomainError::InvalidArgument(format!(
                "{:?} has custom domain {:?}, not {want:?}",
                app.slug, row.hostname
            )));
        }
    }
    if let Some(cf) = cf {
        // Best effort: the row goes regardless, so a Cloudflare outage
        // can't wedge the removal (the warn names the dangling hostname
        // for the operator log).
        if let Err(err) = cf.delete(&row.cf_custom_hostname_id).await {
            tracing::warn!(
                hostname = %row.hostname,
                error = %format!("{err:#}"),
                "custom hostname delete failed; dropping the row anyway"
            );
        }
    }
    sqlx::query("DELETE FROM app_domain WHERE hostname = ?1")
        .bind(&row.hostname)
        .execute(pool)
        .await
        .map_err(internal)?;
    purge_host_cache(purge, &row.hostname).await;
    Ok(row.hostname)
}

/// Delete every domain on `app_id`, best-effort at Cloudflare, rows always.
/// `delete_app` calls this before dropping the app row; the slug rename
/// path needs no call (rows key on the app id, which a rename keeps), but
/// it is public for any path that must strip domains explicitly.
pub async fn remove_all_for_app(
    pool: &sqlx::SqlitePool,
    cf: Option<&CustomHostnames>,
    purge: Option<&crate::cloudflare::CachePurge>,
    app_id: &str,
) -> u64 {
    let rows: Vec<StoredDomain> = sqlx::query_as::<_, StoredDomain>(
        "SELECT hostname, app_id, cf_custom_hostname_id, status, created_at FROM app_domain WHERE app_id = ?1",
    )
    .bind(app_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut removed = 0;
    for row in &rows {
        if let Some(cf) = cf {
            if let Err(err) = cf.delete(&row.cf_custom_hostname_id).await {
                tracing::warn!(
                    hostname = %row.hostname,
                    error = %format!("{err:#}"),
                    "custom hostname delete failed during app cleanup; dropping the row anyway"
                );
            }
        }
        if sqlx::query("DELETE FROM app_domain WHERE hostname = ?1")
            .bind(&row.hostname)
            .execute(pool)
            .await
            .is_ok()
        {
            removed += 1;
        }
        purge_host_cache(purge, &row.hostname).await;
    }
    removed
}

/// The app an `active` custom domain routes to, if `host` is one. Pending
/// and failed domains never serve (the proxy answers a setup page).
pub async fn resolve_active(
    pool: &sqlx::SqlitePool,
    host: &str,
) -> Result<Option<crate::apps::App>, DomainError> {
    let host = host.trim().trim_matches('.').to_lowercase();
    let app_id: Option<String> = sqlx::query_scalar(
        "SELECT app_id FROM app_domain WHERE hostname = ?1 AND status = 'active'",
    )
    .bind(&host)
    .fetch_optional(pool)
    .await
    .map_err(internal)?;
    match app_id {
        None => Ok(None),
        Some(id) => crate::apps::get_by_id(pool, &id).await.map_err(internal),
    }
}

/// The app's domain status for the proxy's setup page (`None` = no domain
/// at all, or a differently-cased unknown host).
pub async fn pending_status(
    pool: &sqlx::SqlitePool,
    host: &str,
) -> Result<Option<String>, DomainError> {
    let host = host.trim().trim_matches('.').to_lowercase();
    sqlx::query_scalar::<_, String>("SELECT status FROM app_domain WHERE hostname = ?1")
        .bind(&host)
        .fetch_optional(pool)
        .await
        .map_err(internal)
}

/// Every `active` custom hostname on `app_id`, for the deploy purge.
pub async fn active_hosts(pool: &sqlx::SqlitePool, app_id: &str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT hostname FROM app_domain WHERE app_id = ?1 AND status = 'active'",
    )
    .bind(app_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

async fn set_status(
    pool: &sqlx::SqlitePool,
    hostname: &str,
    status: &str,
) -> Result<(), DomainError> {
    sqlx::query("UPDATE app_domain SET status = ?1 WHERE hostname = ?2")
        .bind(status)
        .bind(hostname)
        .execute(pool)
        .await
        .map_err(internal)?;
    Ok(())
}

/// Stored status from the live Cloudflare state: `active` only when the
/// hostname AND the certificate are both live; `failed` when either side
/// reports failure; else still `pending`.
fn status_of(live: &CustomHostname) -> (String, String) {
    let status = if live.is_active() {
        "active"
    } else if live.is_failed() {
        "failed"
    } else {
        "pending"
    }
    .to_string();
    (status, live.ssl_status.clone())
}

fn reason_of(live: &CustomHostname) -> Option<String> {
    if live.status == "active" {
        match live.ssl_status.as_str() {
            "validation_timed_out" => Some(
                "the certificate wasn't validated in time — check the DNS records below, then refresh"
                    .to_string(),
            ),
            other => Some(format!("certificate issue: {other} — check the DNS records below, then refresh")),
        }
    } else {
        Some(format!(
            "hostname validation issue: {} — check the DNS records below, then refresh",
            live.status
        ))
    }
}

fn map_cf(err: crate::cloudflare::CfError) -> DomainError {
    use crate::cloudflare::CfError as E;
    match err {
        E::Conflict(detail) => DomainError::Conflict(detail),
        E::Rejected(reason) => {
            DomainError::InvalidArgument(format!("Cloudflare won't accept this hostname: {reason}"))
        }
        E::NotFound => DomainError::Unavailable("the custom hostname is gone at Cloudflare".to_string()),
        E::Auth(detail) => DomainError::Unavailable(format!(
            "Cloudflare refused the custom-hostname request (check the API token permissions): {detail}"
        )),
        E::RateLimited => {
            DomainError::Unavailable("Cloudflare rate-limited the request; try again in a bit".to_string())
        }
        E::Api(detail) | E::Transport(detail) => {
            DomainError::Unavailable(format!("Cloudflare request failed: {detail}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloudflare::fake_saas::FakeSaaS;

    /// Cloudflare's own refusal of a hostname (HTTP 400, e.g. a reserved
    /// domain) is the caller's input problem: `invalid_argument` with
    /// Cloudflare's reason, not an `internal` outage.
    #[test]
    fn cloudflare_rejections_are_invalid_arguments() {
        let err = map_cf(crate::cloudflare::CfError::Rejected(
            "Custom hostnames ending in example.com, example.net, or example.org are prohibited."
                .to_string(),
        ));
        assert_eq!(err.code(), "invalid_argument");
        assert!(
            err.user_message(false).contains("example.com"),
            "{}",
            err.user_message(false)
        );
        assert!(
            !err.user_message(false).contains("400"),
            "{}",
            err.user_message(false)
        );
    }
    #[test]
    fn pro_refusal_names_the_launch_price_only_while_it_runs() {
        assert!(DomainError::ProRequired
            .user_message(true)
            .contains("$4 at the launch price"),);
        let over = DomainError::ProRequired.user_message(false);
        assert!(over.contains("Outer Pro ($8/month)"), "{over}");
        assert!(!over.contains("$4"), "{over}");
    }

    async fn pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        (dir, pool)
    }

    async fn app(pool: &sqlx::SqlitePool, id: &str, slug: &str, owner: &str) -> crate::apps::App {
        sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?1, ?2, 0, 0, 0)")
            .bind(format!("u-{owner}")).bind(format!("{owner}@example.com"))
            .execute(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, last_activity_at, created_at) \
             VALUES (?1, ?2, ?3, 200001, 21001, 31001, 'unlisted', 0, 0)",
        )
        .bind(id).bind(slug).bind(format!("u-{owner}"))
        .execute(pool).await.unwrap();
        crate::apps::get_by_slug(pool, slug).await.unwrap().unwrap()
    }

    #[test]
    fn hostnames_normalize_and_reject() {
        assert_eq!(
            normalize_hostname("Shop.Example.COM.").unwrap(),
            "shop.example.com"
        );
        for bad in [
            "",
            "a",
            "1.2.3.4",
            "::1",
            "has space.com",
            "under_score.com",
            "-lead.com",
            "trail-.com",
            "two..dots.com",
        ] {
            assert!(normalize_hostname(bad).is_err(), "{bad:?}");
        }
        // Non-ASCII is refused with a pointer at the punycode form.
        assert!(normalize_hostname("münchen.de").is_err());
        // The punycode form itself is plain ASCII labels, so it is fine.
        assert_eq!(
            normalize_hostname("xn--mnchen-3ya.de").unwrap(),
            "xn--mnchen-3ya.de"
        );
        // Platform, app hosts and the CNAME target never qualify.
        let bad = |h: &str| check_platform(h, "outer.now", "outer.localhost", "domains.outer.now");
        assert!(bad("outer.now").is_err());
        assert!(bad("a.outer.now").is_err());
        assert!(bad("grocery.outer.localhost").is_err());
        assert!(bad("outer.localhost").is_err());
        assert!(bad("domains.outer.now").is_err());
        assert!(bad("shop.example.com").is_ok());
        assert_eq!(cname_target("outer.now"), "domains.outer.now");
        assert_eq!(cname_target(".outer.now."), "domains.outer.now");
    }

    #[tokio::test]
    async fn create_pending_then_active_then_removed() {
        let (_dir, pool) = pool().await;
        let app = app(&pool, "app-1", "grocery", "ana").await;
        let fake = FakeSaaS::start().await;
        let cf = fake.cf();

        // Free plan: refused before anything reaches Cloudflare.
        let err = add_domain(
            &pool,
            Some(&cf),
            "outer.now",
            "outer.localhost",
            "free",
            &app,
            "shop.example.com",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DomainError::ProRequired));
        assert_eq!(err.code(), "payment_required");
        assert!(fake.calls("POST").is_empty());

        // Pro: pending row + POST shape + DNS records from the follow-up GET.
        let view = add_domain(
            &pool,
            Some(&cf),
            "outer.now",
            "outer.localhost",
            "pro",
            &app,
            "Shop.Example.COM.",
        )
        .await
        .unwrap();
        assert_eq!(view.hostname, "shop.example.com");
        assert_eq!(view.status, "pending");
        assert_eq!(view.cname_target, "domains.outer.now");
        assert!(!view.txt_records.is_empty(), "GET fills in the TXT records");
        let posts = fake.calls("POST");
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0]["hostname"], "shop.example.com");
        assert_eq!(posts[0]["ssl"]["method"], "http");

        // One per app: the second name is refused locally (no new POST).
        let err = add_domain(
            &pool,
            Some(&cf),
            "outer.now",
            "outer.localhost",
            "pro",
            &app,
            "other.example.com",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DomainError::AlreadyHas(_, _)));
        assert_eq!(fake.calls("POST").len(), 1);

        // Pending hostnames never route.
        assert!(resolve_active(&pool, "shop.example.com")
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            pending_status(&pool, "shop.example.com")
                .await
                .unwrap()
                .as_deref(),
            Some("pending")
        );

        // Cloudflare finishes: refresh flips the row to active.
        fake.state.lock().unwrap().status.insert(
            "cf-shop.example.com".to_string(),
            ("active".to_string(), "active".to_string()),
        );
        let view = refresh_domain(&pool, Some(&cf), "outer.now", &app)
            .await
            .unwrap();
        assert_eq!(view.status, "active");
        assert!(resolve_active(&pool, "shop.example.com")
            .await
            .unwrap()
            .is_some());

        // A wrong hostname is refused before anything is deleted.
        let err = remove_domain(&pool, Some(&cf), None, &app, Some("other.example.com"))
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::InvalidArgument(_)));
        assert!(fake.calls("DELETE").is_empty());
        // Removal deletes at Cloudflare, drops the row, and stops routing.
        let removed = remove_domain(&pool, Some(&cf), None, &app, Some("shop.example.com"))
            .await
            .unwrap();
        assert_eq!(removed, "shop.example.com");
        assert_eq!(fake.calls("DELETE").len(), 1);
        assert!(stored_for_app(&pool, &app.id).await.unwrap().is_none());
        assert!(resolve_active(&pool, "shop.example.com")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn hostname_taken_elsewhere_is_a_conflict() {
        let (_dir, pool) = pool().await;
        let a = app(&pool, "app-1", "grocery", "ana").await;
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, last_activity_at, created_at) \
             VALUES ('app-2', 'deli', 'u-ana', 200002, 21002, 31002, 'unlisted', 0, 0)",
        )
        .execute(&pool).await.unwrap();
        let b = crate::apps::get_by_slug(&pool, "deli")
            .await
            .unwrap()
            .unwrap();
        let fake = FakeSaaS::start().await;
        let cf = fake.cf();

        add_domain(
            &pool,
            Some(&cf),
            "outer.now",
            "outer.localhost",
            "pro",
            &a,
            "shop.example.com",
        )
        .await
        .unwrap();
        // Another app's hostname: refused locally, no second POST.
        let err = add_domain(
            &pool,
            Some(&cf),
            "outer.now",
            "outer.localhost",
            "pro",
            &b,
            "shop.example.com",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)));
        assert_eq!(fake.calls("POST").len(), 1);

        // Cloudflare-side conflict (hostname on some other zone): surfaces
        // as Conflict, and no row lands.
        fake.state.lock().unwrap().fail_with.insert(
            "POST".to_string(),
            (409, serde_json::json!({"success": false, "errors": [{"code": 1013, "message": "hostname already exists"}]})),
        );
        let err = add_domain(
            &pool,
            Some(&cf),
            "outer.now",
            "outer.localhost",
            "pro",
            &b,
            "fresh.example.com",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)));
        assert!(stored(&pool, "fresh.example.com").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cloudflare_failures_map_to_unavailable() {
        let (_dir, pool) = pool().await;
        let app = app(&pool, "app-1", "grocery", "ana").await;
        let fake = FakeSaaS::start().await;
        let cf = fake.cf();

        // No zone configured (localhost dev): refused without a request.
        let err = add_domain(
            &pool,
            None,
            "outer.now",
            "outer.localhost",
            "pro",
            &app,
            "shop.example.com",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DomainError::Unavailable(_)));

        // Bad token: the permission hint reaches the operator message.
        fake.state.lock().unwrap().fail_with.insert(
            "POST".to_string(),
            (403, serde_json::json!({"success": false, "errors": [{"code": 10000, "message": "auth failed"}]})),
        );
        let err = add_domain(
            &pool,
            Some(&cf),
            "outer.now",
            "outer.localhost",
            "pro",
            &app,
            "shop.example.com",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DomainError::Unavailable(_)));
        assert!(err.to_string().contains("API token"));

        // Refresh of a hostname deleted at Cloudflare: failed, with a reason.
        add_domain(
            &pool,
            Some(&cf),
            "outer.now",
            "outer.localhost",
            "pro",
            &app,
            "kept.example.com",
        )
        .await
        .unwrap();
        fake.state.lock().unwrap().fail_with.insert(
            "GET".to_string(),
            (404, serde_json::json!({"success": false, "errors": []})),
        );
        let view = refresh_domain(&pool, Some(&cf), "outer.now", &app)
            .await
            .unwrap();
        assert_eq!(view.status, "failed");
        assert!(view.reason.unwrap().contains("removed at Cloudflare"));
    }

    #[tokio::test]
    async fn remove_all_for_app_cleans_everything() {
        let (_dir, pool) = pool().await;
        let app = app(&pool, "app-1", "grocery", "ana").await;
        let fake = FakeSaaS::start().await;
        let cf = fake.cf();
        add_domain(
            &pool,
            Some(&cf),
            "outer.now",
            "outer.localhost",
            "pro",
            &app,
            "shop.example.com",
        )
        .await
        .unwrap();
        let removed = remove_all_for_app(&pool, Some(&cf), None, &app.id).await;
        assert_eq!(removed, 1);
        assert_eq!(fake.calls("DELETE").len(), 1);
        assert!(stored_for_app(&pool, &app.id).await.unwrap().is_none());
    }
}
