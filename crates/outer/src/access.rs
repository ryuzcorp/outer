//! Pro app access gate (PIN/password): the platform lock the proxy enforces
//! before any request reaches the app.
//!
//! The gate is per-app state (`access_mode`, `access_hash`, `access_version`
//! on the app row), set by the owner over MCP/console and enforced by the
//! proxy on every host that serves the app. The PIN/password uses the Argon2id
//! hash ([`Argon2::default`]); the unlock cookie is an HMAC over
//! `v1.<app_id>.<version>.<expiry>`, keyed by a subkey derived from
//! `OUTER_SECRET` via HKDF-SHA256 with [`KEY_INFO`], domain-separated from
//! every other use of that secret (app secrets use their own label, API keys
//! and tokens use plain `sha256` — see [`crate::secrets`]).

use hkdf::Hkdf;
use sha2::Sha256;
use sqlx::SqlitePool;

/// HKDF `info`: the distinct label separating the access-cookie key from any
/// other key derived from `OUTER_SECRET`.
pub const KEY_INFO: &[u8] = b"outer app-access v1";

/// The unlock cookie name. `__Host-` prefixed, so the browser only sends it
/// back over HTTPS to the exact host that set it (no `Domain`, `Path=/`,
/// `Secure` required): live, preview and custom-domain hosts each hold their
/// own cookie, and the app never sees it (the proxy strips it).
pub const COOKIE_NAME: &str = "__Host-outer-access";
/// Cookie lifetime: 30 days.
pub const COOKIE_TTL_MS: i64 = 30 * 24 * 3600 * 1000;
/// Cookie value version prefix.
const COOKIE_V1: &str = "v1";

/// Failed unlocks allowed per client IP per app per 15 minutes.
pub const MAX_IP_ATTEMPTS: i64 = 10;
/// Window for [`MAX_IP_ATTEMPTS`], in milliseconds.
pub const IP_WINDOW_MS: i64 = 15 * 60 * 1000;
/// Failed unlocks allowed per app per hour (across IPs).
pub const MAX_APP_ATTEMPTS: i64 = 100;
/// Window for [`MAX_APP_ATTEMPTS`], in milliseconds.
pub const APP_WINDOW_MS: i64 = 3600 * 1000;

/// The `rate_limit_events` action for unlock failures.
const LEDGER_ACTION: &str = "access_fail";

/// Who may open the app without a cookie. `open` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    Open,
    Pin,
    Password,
}

impl AccessMode {
    /// The `app.access_mode` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            AccessMode::Open => "open",
            AccessMode::Pin => "pin",
            AccessMode::Password => "password",
        }
    }

    /// Parse the `app.access_mode` column value.
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw {
            "open" => Ok(AccessMode::Open),
            "pin" => Ok(AccessMode::Pin),
            "password" => Ok(AccessMode::Password),
            other => anyhow::bail!("unknown access_mode {other:?}"),
        }
    }

    /// The human noun for messages ("PIN", "password", "open").
    pub fn noun(self) -> &'static str {
        match self {
            AccessMode::Open => "open",
            AccessMode::Pin => "PIN",
            AccessMode::Password => "password",
        }
    }
}

/// Why [`set_access`] refused the change. Turning the gate off is always
/// allowed, so a downgraded owner can still open the app.
#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    /// A `pin`/`password` gate on a Free account.
    #[error("access gate needs Pro")]
    ProOnly,
    /// The mode/secret pair breaks the shape rule; the message says how.
    #[error("{0}")]
    BadSecret(String),
    /// Anything else: the database or the hasher failed.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Check the mode/secret pair shapes: a PIN is 4–8 digits, a password is
/// 8–128 chars with no control characters, and `open` takes no secret.
/// The message names the fix, like the other argument refusals.
pub fn check_secret(mode: AccessMode, secret: Option<&str>) -> Result<(), AccessError> {
    match mode {
        AccessMode::Open => {
            if secret.is_some() {
                return Err(AccessError::BadSecret(
                    "mode open takes no secret; omit it".to_string(),
                ));
            }
            Ok(())
        }
        AccessMode::Pin => {
            let secret = secret.ok_or_else(|| {
                AccessError::BadSecret("mode pin needs a secret: 4-8 digits".to_string())
            })?;
            if !(4..=8).contains(&secret.len()) || !secret.bytes().all(|b| b.is_ascii_digit()) {
                return Err(AccessError::BadSecret(
                    "the PIN must be 4-8 digits".to_string(),
                ));
            }
            Ok(())
        }
        AccessMode::Password => {
            let secret = secret.ok_or_else(|| {
                AccessError::BadSecret("mode password needs a secret: 8-128 chars".to_string())
            })?;
            let len = secret.chars().count();
            if !(8..=128).contains(&len) || secret.chars().any(|c| c.is_control()) {
                return Err(AccessError::BadSecret(
                    "the password must be 8-128 chars with no control characters".to_string(),
                ));
            }
            Ok(())
        }
    }
}

/// Hash a PIN/password with Argon2id ([`argon2::Argon2::default`]). Random
/// 128-bit salt per hash, PHC string out. Runs on the blocking pool: hashing
/// is deliberately slow, and the async runtime must never stall on it.
pub async fn hash_secret(secret: &str) -> anyhow::Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let owned = secret.to_string();
    tokio::task::spawn_blocking(move || {
        // 128-bit salt from the OS CSPRNG (`Uuid::new_v4`); `SaltString`
        // wants its own `rand_core`, so feed it raw bytes instead.
        let salt = SaltString::encode_b64(&uuid::Uuid::new_v4().into_bytes())
            .map_err(|err| anyhow::anyhow!("salting the secret: {err}"))?;
        argon2::Argon2::default()
            .hash_password(owned.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|err| anyhow::anyhow!("hashing the secret: {err}"))
    })
    .await?
}

/// Check a candidate against the stored PHC hash. `false` on mismatch AND on
/// an unparseable stored hash (fail closed). Blocking pool, like the hash.
pub async fn verify_secret(hash: &str, candidate: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let (hash, candidate) = (hash.to_string(), candidate.to_string());
    tokio::task::spawn_blocking(move || {
        let parsed = PasswordHash::new(&hash).ok();
        parsed.is_some_and(|expected| {
            argon2::Argon2::default()
                .verify_password(candidate.as_bytes(), &expected)
                .is_ok()
        })
    })
    .await
    .unwrap_or(false)
}

/// Set the app's access gate. Owner-only (the MCP/console layers enforce the
/// role); `plan` is the owner's plan. `pin`/`password` require Pro; `open`
/// never does. Every change bumps `access_version`, so a secret change signs
/// every issued cookie out. Returns the new mode.
pub async fn set_access(
    pool: &SqlitePool,
    app_id: &str,
    mode: AccessMode,
    secret: Option<&str>,
    plan: &str,
) -> Result<AccessMode, AccessError> {
    // Shapes first (bad values refuse on any plan, like the visibility
    // parse), then the Pro gate: a well-formed pin on Free names Pro.
    check_secret(mode, secret)?;
    if mode != AccessMode::Open && plan != "pro" {
        return Err(AccessError::ProOnly);
    }
    let hash = match (mode, secret) {
        (AccessMode::Open, _) => None,
        (_, Some(secret)) => Some(hash_secret(secret).await?),
        // `check_secret` already refused a missing secret for pin/password.
        (_, None) => return Err(AccessError::BadSecret("a secret is required".to_string())),
    };
    sqlx::query(
        "UPDATE app SET access_mode = ?1, access_hash = ?2, \
         access_version = access_version + 1 WHERE id = ?3",
    )
    .bind(mode.as_str())
    .bind(hash.as_deref())
    .bind(app_id)
    .execute(pool)
    .await
    .map_err(|err| AccessError::Internal(err.into()))?;
    Ok(mode)
}

/// The HMAC key for unlock cookies. `Debug` redacts: the key must never
/// reach a log.
#[derive(Clone)]
pub struct AccessKey([u8; 32]);

impl AccessKey {
    /// Derive the access-cookie key from `OUTER_SECRET`.
    pub fn derive(outer_secret: &str) -> Self {
        let hk = Hkdf::<Sha256>::new(None, outer_secret.as_bytes());
        let mut key = [0u8; 32];
        hk.expand(KEY_INFO, &mut key)
            .expect("HKDF-SHA256 expands to 32 bytes");
        Self(key)
    }

    /// The shared non-production key for unit, MCP and pipeline tests.
    /// Never leaves a test binary: production always derives from config.
    pub fn test_key() -> Self {
        Self::derive("outer-test-access (never production)")
    }
}

impl std::fmt::Debug for AccessKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccessKey(<redacted>)")
    }
}

use hmac::{Hmac, KeyInit, Mac};

/// The cookie payload: `v1.<app_id>.<version>.<expiry_ms>`, MACed below.
/// `app_id` is a uuid (no dots), so splitting on `.` is unambiguous.
fn payload(app_id: &str, version: i64, expiry_ms: i64) -> String {
    format!("{COOKIE_V1}.{app_id}.{version}.{expiry_ms}")
}

fn mac(key: &AccessKey, payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&key.0).expect("HMAC-SHA256 takes any key size");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Mint a cookie value for (`app_id`, `version`) at `now_ms`: 30 days.
/// Binds the app AND the version: a secret change (bumped version) or a
/// cookie moved to another app refuses to verify.
pub fn mint_cookie(key: &AccessKey, app_id: &str, version: i64, now_ms: i64) -> String {
    let body = payload(app_id, version, now_ms + COOKIE_TTL_MS);
    format!("{body}.{}", mac(key, &body))
}

/// Check a cookie value against (`app_id`, `version`) at `now_ms`: version
/// prefix, app, version, expiry and MAC must all match, in constant time on
/// the MAC. Never throws: anything malformed is just invalid.
pub fn verify_cookie(
    key: &AccessKey,
    value: &str,
    app_id: &str,
    version: i64,
    now_ms: i64,
) -> bool {
    let mut parts = value.split('.');
    let (Some(tag), Some(id), Some(got_version), Some(expiry), Some(_mac), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    if tag != COOKIE_V1 || id != app_id {
        return false;
    }
    if got_version.parse::<i64>().ok() != Some(version) {
        return false;
    }
    let Some(expiry_ms) = expiry.parse::<i64>().ok() else {
        return false;
    };
    if expiry_ms <= now_ms {
        return false;
    }
    let body = payload(app_id, version, expiry_ms);
    let expected = mac(key, &body);
    let (Some(a), Some(b)) = (hex::decode(expected).ok(), hex::decode(got_mac(value)).ok()) else {
        return false;
    };
    // Compare the MAC bytes in constant time so a valid cookie cannot be
    // forged byte by byte.
    a.len() == b.len() && constant_time_eq::constant_time_eq(&a, &b)
}

/// The MAC half of a well-formed-enough cookie value (the last `.` part).
/// Only called after the shape check above passed, so `rsplit` always hits.
fn got_mac(value: &str) -> &str {
    value.rsplit('.').next().unwrap_or("")
}

/// The `next` the unlock form carries back to: a relative path on the same
/// host, never an open redirect. Anything else (absolute URLs,
/// protocol-relative `//evil`, `/\evil`, non-paths) falls back to `/`.
pub fn sanitize_next(raw: &str) -> &str {
    let mut bytes = raw.bytes();
    match (bytes.next(), bytes.next()) {
        (Some(b'/'), Some(second)) if second != b'/' && second != b'\\' => raw,
        _ => "/",
    }
}

/// Render the locked-visitor page: the app's name, "This app is private",
/// one secret input and Unlock. Same bare-bones design language as the other
/// proxy-owned `/.outer/*` pages (no external assets), plus a strict CSP —
/// the page carries no styles or scripts at all, so `default-src 'none'`
/// holds.
pub fn render_lock_page(
    app_name: &str,
    mode: AccessMode,
    next: &str,
    error: Option<&str>,
) -> String {
    let (input, wrong) = match mode {
        AccessMode::Pin => (
            "<p><label>PIN<br><input name=\"secret\" inputmode=\"numeric\" \
             autocomplete=\"one-time-code\" required></label></p>",
            "Wrong PIN",
        ),
        _ => (
            "<p><label>Password<br><input name=\"secret\" type=\"password\" \
             autocomplete=\"current-password\" required></label></p>",
            "Wrong password",
        ),
    };
    let error_html = match error {
        Some(_) => format!("<p><strong>{wrong}</strong></p>"),
        None => String::new(),
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title></head>\
         <body><main><h1>{title}</h1><p>This app is private.</p>{error_html}\
         <form method=\"post\" action=\"/.outer/unlock\">\
         <input type=\"hidden\" name=\"next\" value=\"{next}\">{input}\
         <p><button type=\"submit\">Unlock</button></p></form></main></body></html>",
        title = html_escape(app_name),
        next = html_escape(next),
        error_html = error_html,
        input = input,
    )
}

fn html_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Forget the gate cookie in forwarded `Cookie` headers, keeping every other
/// pair byte-identical: pairs pass through untouched except leading
/// whitespace (the old separator), which normalizes to `"; "`. The gate
/// cookie is Outer's, never the app's — upstream must never see it.
pub fn strip_access_cookie(headers: &mut axum::http::HeaderMap) {
    let names: Vec<axum::http::HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().eq_ignore_ascii_case("cookie"))
        .cloned()
        .collect();
    for name in names {
        let kept: Vec<String> = headers
            .get_all(&name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|raw| {
                raw.split(';')
                    .map(str::trim_start)
                    .filter(|pair| {
                        let name = pair.split('=').next().unwrap_or("");
                        !name.eq_ignore_ascii_case(COOKIE_NAME)
                    })
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        headers.remove(&name);
        if !kept.is_empty() {
            // `join` only re-inserts the separators between kept pairs; every
            // kept pair itself is untouched.
            if let Ok(joined) = axum::http::HeaderValue::from_str(&kept.join("; ")) {
                headers.insert(name, joined);
            }
        }
    }
}

/// Ledger subject for one client's failures on one app.
fn ip_subject(ip: &str, app_id: &str) -> String {
    format!("access-ip:{ip}:{app_id}")
}

/// Ledger subject for every failure on one app.
fn app_subject(app_id: &str) -> String {
    format!("access-app:{app_id}")
}

/// Count failures for `subject` inside the window ending at `now_ms`, plus
/// the oldest event in the window (for the retry time). Mirrors
/// [`crate::auth::limits`] without recording: successes must not consume the
/// failure budget, so only failures insert (see [`record_failure`]).
async fn failures_in_window(
    pool: &SqlitePool,
    subject: &str,
    window_ms: i64,
    now_ms: i64,
) -> anyhow::Result<(i64, Option<i64>)> {
    let row: (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*), MIN(created_at) FROM rate_limit_events \
         WHERE subject = ? AND action = ? AND created_at > ?",
    )
    .bind(subject)
    .bind(LEDGER_ACTION)
    .bind(now_ms - window_ms)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// When `count` already hits `limit`, the seconds until the oldest event in
/// the window ages out. Same shape as the ledger helper's answer.
fn retry_after(oldest: Option<i64>, window_ms: i64, now_ms: i64) -> u64 {
    oldest
        .map(|first| (first + window_ms - now_ms).max(1))
        .map(|wait_ms| ((wait_ms + 999) / 1000).max(1) as u64)
        .unwrap_or(1)
}

/// Refuse the attempt when either failure budget is spent, WITHOUT recording:
/// returns the seconds until the oldest failure ages out. Successes never
/// touch the ledger; failures record through [`record_failure`].
pub async fn check_attempt(
    pool: &SqlitePool,
    client_ip: Option<&str>,
    app_id: &str,
    now_ms: i64,
) -> anyhow::Result<Result<(), u64>> {
    let mut limited: Option<u64> = None;
    if let Some(ip) = client_ip {
        let (count, oldest) =
            failures_in_window(pool, &ip_subject(ip, app_id), IP_WINDOW_MS, now_ms).await?;
        if count >= MAX_IP_ATTEMPTS {
            limited = Some(retry_after(oldest, IP_WINDOW_MS, now_ms));
        }
    }
    let (count, oldest) =
        failures_in_window(pool, &app_subject(app_id), APP_WINDOW_MS, now_ms).await?;
    if count >= MAX_APP_ATTEMPTS {
        limited = Some(
            limited.map_or(retry_after(oldest, APP_WINDOW_MS, now_ms), |prev| {
                prev.max(retry_after(oldest, APP_WINDOW_MS, now_ms))
            }),
        );
    }
    Ok(match limited {
        Some(retry) => Err(retry),
        None => Ok(()),
    })
}

/// Record one failed unlock on both budgets (the IP row only when the client
/// IP is known). Called for wrong secrets only, never for successes.
pub async fn record_failure(
    pool: &SqlitePool,
    client_ip: Option<&str>,
    app_id: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    if let Some(ip) = client_ip {
        sqlx::query("INSERT INTO rate_limit_events (subject, action, created_at) VALUES (?, ?, ?)")
            .bind(ip_subject(ip, app_id))
            .bind(LEDGER_ACTION)
            .bind(now_ms)
            .execute(pool)
            .await?;
    }
    sqlx::query("INSERT INTO rate_limit_events (subject, action, created_at) VALUES (?, ?, ?)")
        .bind(app_subject(app_id))
        .bind(LEDGER_ACTION)
        .bind(now_ms)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_shape() {
        assert!(check_secret(AccessMode::Pin, Some("1234")).is_ok());
        assert!(check_secret(AccessMode::Pin, Some("12345678")).is_ok());
        for bad in [
            None,
            Some("123"),
            Some("123456789"),
            Some("12a4"),
            Some(" 123"),
        ] {
            assert!(check_secret(AccessMode::Pin, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn password_shape() {
        assert!(check_secret(AccessMode::Password, Some("correct horse!")).is_ok());
        assert!(check_secret(AccessMode::Password, Some("12345678")).is_ok());
        assert!(check_secret(AccessMode::Password, Some("short")).is_err());
        assert!(check_secret(AccessMode::Password, Some("has\ttabbed")).is_err());
        assert!(check_secret(AccessMode::Password, Some(&"x".repeat(129))).is_err());
        assert!(check_secret(AccessMode::Password, None).is_err());
    }

    #[test]
    fn open_takes_no_secret() {
        assert!(check_secret(AccessMode::Open, None).is_ok());
        assert!(check_secret(AccessMode::Open, Some("1234")).is_err());
    }

    #[tokio::test]
    async fn hash_round_trip() {
        let hash = hash_secret("123456").await.unwrap();
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert!(!hash.contains("123456"), "the hash never echoes the secret");
        assert!(verify_secret(&hash, "123456").await);
        assert!(!verify_secret(&hash, "123457").await);
        assert!(!verify_secret("not-a-hash", "123456").await);
    }

    #[test]
    fn cookie_round_trip() {
        let key = AccessKey::test_key();
        let now = 1_700_000_000_000;
        let cookie = mint_cookie(&key, "app1", 3, now);
        assert!(verify_cookie(&key, &cookie, "app1", 3, now));
        // A secret change (bumped version) signs old cookies out.
        assert!(!verify_cookie(&key, &cookie, "app1", 4, now));
        // A cookie moved to another app is rejected.
        assert!(!verify_cookie(&key, &cookie, "app2", 3, now));
        // An expired cookie is rejected.
        assert!(!verify_cookie(
            &key,
            &cookie,
            "app1",
            3,
            now + COOKIE_TTL_MS
        ));
        // Malformed values never verify.
        for bad in ["", "v1.app1", "v1.app1.3.notanumber.x", "v2.app1.3.1.x"] {
            assert!(!verify_cookie(&key, bad, "app1", 3, now), "{bad}");
        }
        // Another key (another OUTER_SECRET) rejects the cookie.
        let other = AccessKey::derive("something else");
        assert!(!verify_cookie(&other, &cookie, "app1", 3, now));
    }

    #[test]
    fn next_sanitizing() {
        assert_eq!(sanitize_next("/items?page=2"), "/items?page=2");
        assert_eq!(sanitize_next("/"), "/");
        for evil in [
            "//evil.example",
            "https://evil.example",
            "/\\evil",
            "",
            "items",
        ] {
            assert_eq!(sanitize_next(evil), "/", "{evil}");
        }
    }

    #[test]
    fn cookie_stripping_keeps_the_rest() {
        use axum::http::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            HeaderValue::from_str(&format!("session=abc; {COOKIE_NAME}=v1.x; theme=dark")).unwrap(),
        );
        strip_access_cookie(&mut headers);
        assert_eq!(
            headers.get("cookie").unwrap(),
            "session=abc; theme=dark",
            "gate cookie gone, rest byte-identical"
        );
        let mut only = HeaderMap::new();
        only.insert(
            "cookie",
            HeaderValue::from_str(&format!("{COOKIE_NAME}=v1.x")).unwrap(),
        );
        strip_access_cookie(&mut only);
        assert!(only.get("cookie").is_none());
    }

    #[test]
    fn lock_page_shape() {
        let pin = render_lock_page("Pizza & Co", AccessMode::Pin, "/items", None);
        assert!(pin.contains("Pizza &amp; Co"), "app name, escaped");
        assert!(pin.contains("This app is private"));
        assert!(pin.contains("inputmode=\"numeric\""), "PIN input mode");
        assert!(pin.contains("autocomplete=\"one-time-code\""));
        assert!(!pin.contains("Wrong PIN"), "no error on first render");
        let wrong = render_lock_page("Shop", AccessMode::Password, "/", Some("nope"));
        assert!(wrong.contains("Wrong password"));
        assert!(wrong.contains("type=\"password\""));
        assert!(
            wrong.contains("action=\"/.outer/unlock\""),
            "same-origin post"
        );
        assert!(!pin.contains("<script"), "no scripts for the CSP");
    }

    #[tokio::test]
    async fn failure_budgets() {
        let pool = crate::auth::test_support::test_pool().await;
        let now = 1_700_000_000_000i64;
        for _ in 0..10 {
            assert!(check_attempt(&pool, Some("1.2.3.4"), "app1", now)
                .await
                .unwrap()
                .is_ok());
            record_failure(&pool, Some("1.2.3.4"), "app1", now)
                .await
                .unwrap();
        }
        // The 11th attempt in the window is refused, without recording.
        let retry = check_attempt(&pool, Some("1.2.3.4"), "app1", now)
            .await
            .unwrap()
            .expect_err("11th failure is limited");
        assert!(retry > 0);
        // A different IP still has its own budget; the app budget has room.
        assert!(check_attempt(&pool, Some("9.9.9.9"), "app1", now)
            .await
            .unwrap()
            .is_ok());
        // Successes never record: ten successes leave the budget untouched.
        for _ in 0..10 {
            assert!(check_attempt(&pool, Some("8.8.8.8"), "app1", now)
                .await
                .unwrap()
                .is_ok());
        }
        // The per-app budget trips at 100 across IPs.
        for n in 0..100 {
            let ip = format!("10.0.0.{n}");
            record_failure(&pool, Some(ip.as_str()), "app2", now)
                .await
                .unwrap();
        }
        assert!(check_attempt(&pool, Some("10.9.9.9"), "app2", now)
            .await
            .unwrap()
            .is_err());
    }
}
