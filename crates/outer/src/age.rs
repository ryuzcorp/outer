//! Per-app adults-only (18+) gate: the owner marks the app 18+ and the
//! proxy asks every visitor to confirm they are an adult before serving it.
//!
//! The account's own minimum age (16) lives in the Terms, not in code; this
//! module only carries the app's `adults_only` flag and the confirmation
//! cookie the proxy mints when a visitor clicks through the interstitial
//! (`v1.<app_id>.<expiry_ms>.<host_hash>`, HMACed with its own HKDF subkey
//! of `OUTER_SECRET`, domain-separated from the access gate's). The proxy
//! enforces it after the access gate (a private app still unlocks first),
//! on every host that serves the app, with no bypass for previews or for
//! owner/collaborator views: the same rule everywhere keeps it predictable.

use hkdf::Hkdf;
use sha2::Sha256;
use sqlx::SqlitePool;

/// HKDF `info`: the distinct label separating the age-cookie key from every
/// other use of `OUTER_SECRET` (the access gate uses its own).
pub const KEY_INFO: &[u8] = b"outer app age gate v1";

/// The confirmation cookie name. `__Host-` prefixed like the other platform
/// cookies, so the browser only sends it back over HTTPS to the exact host
/// that set it (no `Domain`, `Path=/`, `Secure` required): live, preview
/// and custom-domain hosts each hold their own cookie, and the app never
/// sees it (the proxy strips it).
pub const COOKIE_NAME: &str = "__Host-outer-age";
/// Cookie lifetime: 30 days, like the unlock cookie.
pub const COOKIE_TTL_MS: i64 = 30 * 24 * 3600 * 1000;
/// Cookie value version prefix. v1 binds the request host (lowercased, port
/// stripped) into the payload, so a cookie confirmed on the live host is
/// rejected on the preview host and custom domains (and vice versa).
const COOKIE_V1: &str = "v1";

/// The HMAC key for age cookies. `Debug` redacts: the key must never reach
/// a log.
#[derive(Clone)]
pub struct AgeKey([u8; 32]);

impl AgeKey {
    /// Derive the age-cookie key from `OUTER_SECRET`.
    pub fn derive(outer_secret: &str) -> Self {
        let hk = Hkdf::<Sha256>::new(None, outer_secret.as_bytes());
        let mut key = [0u8; 32];
        hk.expand(KEY_INFO, &mut key)
            .expect("HKDF-SHA256 expands to 32 bytes");
        Self(key)
    }

    /// The shared non-production key for unit tests. `#[cfg(test)]` keeps
    /// it out of production binaries: production always derives from config.
    #[cfg(test)]
    pub fn test_key() -> Self {
        Self::derive("outer-test-age (never production)")
    }
}

impl std::fmt::Debug for AgeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AgeKey(<redacted>)")
    }
}

use hmac::{Hmac, KeyInit, Mac};

/// The short host scope inside the cookie: 16 hex chars of SHA-256 over the
/// lowercased bare host (no port). Same shape as the access gate's scope,
/// so a confirmed cookie stays on the host that minted it.
fn host_hash(host: &str) -> String {
    use sha2::Digest as _;
    hex::encode(&sha2::Sha256::digest(host.to_lowercase().as_bytes())[..8])
}

/// The cookie payload: `v1.<app_id>.<expiry_ms>.<host_hash>`, MACed below.
/// `app_id` is a uuid (no dots), so splitting on `.` is unambiguous.
fn payload(app_id: &str, expiry_ms: i64, host: &str) -> String {
    format!("{COOKIE_V1}.{app_id}.{expiry_ms}.{}", host_hash(host))
}

fn mac(key: &AgeKey, payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&key.0).expect("HMAC-SHA256 takes any key size");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Mint a confirmation cookie value for `app_id` on `host` at `now_ms`: 30
/// days. Binds the app AND the host: a cookie confirmed on one app or host
/// (live, preview, custom domain) refuses to verify on any other. `host`
/// is the bare request host (port already stripped); case is normalized
/// here.
pub fn mint_cookie(key: &AgeKey, app_id: &str, now_ms: i64, host: &str) -> String {
    let body = payload(app_id, now_ms + COOKIE_TTL_MS, host);
    format!("{body}.{}", mac(key, &body))
}

/// Check a cookie value against (`app_id`, `host`) at `now_ms`: version
/// prefix, app, expiry, host scope and MAC must all match, in constant time
/// on the MAC. Never throws: anything malformed is just invalid.
pub fn verify_cookie(key: &AgeKey, value: &str, app_id: &str, now_ms: i64, host: &str) -> bool {
    let mut parts = value.split('.');
    let (Some(tag), Some(id), Some(expiry), Some(scope), Some(_mac), None) = (
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
    if scope != host_hash(host) {
        return false;
    }
    let Some(expiry_ms) = expiry.parse::<i64>().ok() else {
        return false;
    };
    if expiry_ms <= now_ms {
        return false;
    }
    let body = payload(app_id, expiry_ms, host);
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

/// Whether one `Cookie` pair IS the age cookie: the name matches ignoring
/// optional whitespace around it (RFC 9110 §5.6.1.1 list rules) and case.
/// The single place the age cookie is recognized, shared by the gate check
/// and the forward strip so both agree on every spelling.
pub fn is_age_pair(pair: &str) -> bool {
    pair.split_once('=')
        .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case(COOKIE_NAME))
}

/// The age cookie's values across every `Cookie` header, whitespace around
/// each pair already trimmed.
pub fn age_cookie_values(headers: &axum::http::HeaderMap) -> impl Iterator<Item = &str> + '_ {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|raw| {
            raw.split(';').filter_map(|pair| {
                let (name, value) = pair.split_once('=')?;
                name.trim()
                    .eq_ignore_ascii_case(COOKIE_NAME)
                    .then(|| value.trim())
            })
        })
}

/// Forget the age cookie in forwarded `Cookie` headers, keeping every other
/// pair byte-identical: pairs pass through untouched except leading
/// whitespace (the old separator), which normalizes to `"; "`. The age
/// cookie is Outer's, never the app's — upstream must never see it.
/// Recognition shares [`is_age_pair`] with the gate check, so the spaced
/// spelling confirms AND is stripped here.
pub fn strip_age_cookie(headers: &mut axum::http::HeaderMap) {
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
                    .filter(|pair| !is_age_pair(pair))
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

/// Flip an app's adults-only flag. Owner-only (the MCP/console layers
/// enforce the role). Returns whether the row changed, so the callers know
/// whether to purge the edge cache. Available on every plan: confirming an
/// age is not a Pro feature.
pub async fn set_adults_only(
    pool: &SqlitePool,
    app_id: &str,
    adults_only: bool,
) -> anyhow::Result<bool> {
    let done = sqlx::query("UPDATE app SET adults_only = ?1 WHERE id = ?2 AND adults_only != ?1")
        .bind(i64::from(adults_only))
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Render the age interstitial: the app's name, "This app is for adults
/// (18+)", one confirm button and Leave. Same bare-bones design language
/// as the other proxy-owned pages (no external assets), plus a strict CSP
/// — the page carries no styles or scripts at all, so `default-src 'none'`
/// holds. Leave points at `about:blank`: no script, no platform host to
/// know, just away from the app.
pub fn render_age_page(app_name: &str, next: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title></head>\
         <body><main><h1>{title}</h1><p>This app is for adults (18+).</p>\
         <form method=\"post\" action=\"/.outer/age\">\
         <input type=\"hidden\" name=\"next\" value=\"{next}\">\
         <p><button type=\"submit\">I am 18 or older</button></p></form>\
         <p><a href=\"about:blank\">Leave</a></p></main></body></html>",
        title = html_escape(app_name),
        next = html_escape(next),
    )
}

fn html_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_round_trips_and_binds_app_and_host() {
        let key = AgeKey::test_key();
        let now = 1_800_000_000_000i64;
        let value = mint_cookie(&key, "app1", now, "shop.outer.test");
        assert!(verify_cookie(&key, &value, "app1", now, "shop.outer.test"));
        // Case-insensitive host, like the request host normalization.
        assert!(verify_cookie(&key, &value, "app1", now, "SHOP.outer.test"));
        // Another app, another host (preview, custom domain): refused.
        assert!(!verify_cookie(&key, &value, "app2", now, "shop.outer.test"));
        assert!(!verify_cookie(
            &key,
            &value,
            "app1",
            now,
            "shop--preview.outer.test"
        ));
        // Expired, forged and malformed: refused, never throwing.
        assert!(!verify_cookie(
            &key,
            &value,
            "app1",
            now + COOKIE_TTL_MS + 1,
            "shop.outer.test"
        ));
        assert!(!verify_cookie(
            &AgeKey::derive("outer-test-age (someone else)"),
            &value,
            "app1",
            now,
            "shop.outer.test"
        ));
        for bad in ["", "v1", "v1.app1.1.2.3.extra", "v2.app1.1.2.3"] {
            assert!(
                !verify_cookie(&key, bad, "app1", now, "shop.outer.test"),
                "{bad}"
            );
        }
    }

    #[test]
    fn strip_age_cookie_keeps_everything_else_byte_identical() {
        use axum::http::HeaderMap;
        let key = AgeKey::test_key();
        let age = mint_cookie(&key, "app1", 1_800_000_000_000, "shop.outer.test");
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            format!("session=abc; {COOKIE_NAME}={age}; theme=dark")
                .parse()
                .unwrap(),
        );
        strip_age_cookie(&mut headers);
        assert_eq!(headers.get("cookie").unwrap(), "session=abc; theme=dark");
        // Only the age cookie: no empty `Cookie` header is left behind.
        let mut only = HeaderMap::new();
        only.insert("cookie", format!("{COOKIE_NAME}={age}").parse().unwrap());
        strip_age_cookie(&mut only);
        assert!(only.get("cookie").is_none());
        // The spaced spelling confirms AND is stripped.
        let mut spaced = HeaderMap::new();
        spaced.insert("cookie", format!("{COOKIE_NAME} = {age}").parse().unwrap());
        assert!(age_cookie_values(&spaced).next().is_some());
        strip_age_cookie(&mut spaced);
        assert!(spaced.get("cookie").is_none());
    }

    #[test]
    fn age_page_names_the_app_and_the_confirm_target() {
        let page = render_age_page("Shop", "/items?page=2");
        assert!(page.contains("This app is for adults (18+)"));
        assert!(page.contains("I am 18 or older"));
        assert!(page.contains("action=\"/.outer/age\""));
        assert!(page.contains("Leave"));
        assert!(page.contains("Shop"));
    }

    #[tokio::test]
    async fn set_adults_only_flips_the_row_and_reports_changes() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('a1', 'o@example.com', 0, 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, last_activity_at, created_at) \
             VALUES ('app1', 'shop', 'a1', 200001, 11001, 21001, 'unlisted', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(set_adults_only(&pool, "app1", true).await.unwrap());
        let flag: i64 = sqlx::query_scalar("SELECT adults_only FROM app WHERE id = 'app1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(flag, 1);
        assert!(!set_adults_only(&pool, "app1", true).await.unwrap());
        assert!(set_adults_only(&pool, "app1", false).await.unwrap());
    }
}
