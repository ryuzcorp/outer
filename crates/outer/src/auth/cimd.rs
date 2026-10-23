//! Client identification: CIMD documents, DCR rows and known clients.
//!
//! The `client_id` is either an HTTPS URL (a client-instance metadata
//! document, fetched with HTTPS only to public IPs only, 3 s timeout,
//! ≤ 16 KiB, cached in `oauth_client`) or a `dcr_…` id from `POST
//! /oauth/register`. Known clients skip the fetch with pinned redirects and
//! a verified label; anything else shows as "Unverified app at `<host>`".

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use sqlx::SqlitePool;
use url::Url;

use super::{now_ms, AuthConfig};

/// ≤ 16 KiB per spec.
pub const MAX_DOC_BYTES: usize = 16 * 1024;
/// Cache TTL for a fetched document without a shorter `Cache-Control`.
const DOC_TTL_MS: i64 = 86_400_000;
/// CIMD fetch timeout.
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// A resolved client: what the consent page labels and where it may return.
#[derive(Debug, Clone)]
pub struct ClientDoc {
    /// The `client_id` as sent: an https URL or a `dcr_…` id.
    pub client_id: String,
    /// Consent-page label: the known name, or "Unverified app at `<host>`".
    pub name: String,
    /// Redirect host shown on the consent page.
    pub host: String,
    /// Allowed redirect URIs (CIMD doc, DCR row or pinned table).
    pub redirect_uris: Vec<String>,
    /// Known clients only.
    pub verified: bool,
    /// Every allowed redirect is loopback: extra warning on the consent page.
    pub loopback_only: bool,
}

/// Why a client or its redirect was refused. The authorize page renders
/// these as 400 error pages (the redirect target is untrusted, so Outer
/// never redirects a validation failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    UnknownClient,
    NonHttps,
    PrivateIp,
    Fetch(String),
    Oversize,
    BadDocument(String),
    ClientIdMismatch,
    RedirectMismatch,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownClient => write!(f, "unknown client"),
            Self::NonHttps => write!(f, "client_id must be an https URL"),
            Self::PrivateIp => write!(f, "client_id resolves to a non-public IP"),
            Self::Fetch(detail) => write!(f, "client metadata fetch failed: {detail}"),
            Self::Oversize => write!(f, "client metadata exceeds 16 KiB"),
            Self::BadDocument(detail) => write!(f, "client metadata is not usable: {detail}"),
            Self::ClientIdMismatch => write!(f, "document client_id does not match the URL"),
            Self::RedirectMismatch => write!(f, "redirect_uri is not registered for this client"),
        }
    }
}

/// Pinned clients: metadata URL → (display name, redirect URIs).
pub fn known_client(client_id: &str) -> Option<(&'static str, &'static [&'static str])> {
    match client_id {
        "https://claude.ai/oauth/mcp-oauth-client-metadata" => {
            Some(("Claude", &["https://claude.ai/api/mcp/auth_callback"]))
        }
        "https://claude.ai/oauth/claude-code-client-metadata" => {
            Some(("Claude Code", &["http://localhost/callback"]))
        }
        "https://chatgpt.com/oauth/client.json" => Some((
            "ChatGPT",
            &["https://chatgpt.com/connector_platform_oauth_redirect"],
        )),
        _ => None,
    }
}

/// Loopback hosts match with the port ignored (ephemeral CLI ports).
pub fn is_loopback_host(host: Option<&str>) -> bool {
    matches!(
        host.map(str::to_ascii_lowercase).as_deref(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    )
}

/// `redirect_uri` checks exactly against the registered URIs, except
/// loopback URIs match with the port ignored.
pub fn redirect_matches(registered: &str, requested: &str) -> bool {
    if registered == requested {
        return true;
    }
    let (Ok(reg), Ok(req)) = (Url::parse(registered), Url::parse(requested)) else {
        return false;
    };
    if reg.scheme() != req.scheme() {
        return false;
    }
    if reg.host_str() != req.host_str() {
        return false;
    }
    if !is_loopback_host(reg.host_str()) {
        return false;
    }
    reg.path() == req.path() && reg.query() == req.query()
}
/// Public IPs only: no loopback, private, link-local, multicast, CGNAT,
/// documentation or reserved ranges.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_documentation()
                || v.is_unspecified()
                || v.is_multicast()
                || (o[0] == 100 && o[1] & 0xC0 == 0x40)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || (o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
                || (o[0] == 203 && o[1] == 0 && o[2] == 113)
                || o[0] >= 240)
        }
        IpAddr::V6(v) => {
            // IPv4-mapped (::ffff:a.b.c.d) is judged as the IPv4 it names.
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s = v.segments();
            // Transition and translation prefixes can tunnel to private
            // IPv4 space: 6to4 2002::/16, Teredo 2001::/32, NAT64
            // 64:ff9b::/96 (+ local-use 64:ff9b:1::/48), discard 100::/64,
            // benchmarking 2001:2::/48.
            if s[0] == 0x2002
                || (s[0] == 0x2001 && s[1] == 0)
                || (s[0] == 0x2001 && s[1] == 2 && s[2] == 0)
                || (s[0] == 0x64 && s[1] == 0xff9b)
                || (s[0] == 0x100 && s[1] == 0 && s[2] == 0 && s[3] == 0)
            {
                return false;
            }
            !(v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (s[0] & 0xFFC0) == 0xFE80
                || (s[0] & 0xFE00) == 0xFC00
                || (s[0] == 0x2001 && s[1] == 0xDB8))
        }
    }
}

/// Resolve `client_id` and check `redirect_uri` against it.
pub async fn resolve_client(
    pool: &SqlitePool,
    cfg: &AuthConfig,
    client_id: &str,
    redirect_uri: &str,
) -> Result<ClientDoc, ClientError> {
    let now = now_ms();
    if let Some(row) = dcr_client(pool, client_id)
        .await
        .map_err(|err| ClientError::Fetch(err.to_string()))?
    {
        return check_redirect(row, redirect_uri);
    }
    if let Some((name, uris)) = known_client(client_id) {
        let redirect_uris = uris.iter().map(|uri| uri.to_string()).collect::<Vec<_>>();
        let host = redirect_host(
            redirect_uris
                .first()
                .map(String::as_str)
                .unwrap_or(client_id),
        );
        return check_redirect(
            ClientDoc {
                client_id: client_id.to_string(),
                name: name.to_string(),
                host,
                redirect_uris,
                verified: true,
                loopback_only: uris.iter().all(|uri| is_loopback_uri(uri)),
            },
            redirect_uri,
        );
    }
    let url = Url::parse(client_id).map_err(|_| ClientError::UnknownClient)?;
    // Tests run a fake CIMD on loopback over plain HTTP; production always
    // requires https. The flag never comes from the environment.
    let loopback_test = cfg.allow_loopback_fetch
        && url
            .host_str()
            .is_some_and(|host| is_loopback_host(Some(host)));
    if url.scheme() != "https" && !(loopback_test && url.scheme() == "http") {
        return Err(ClientError::NonHttps);
    }
    let doc = fetch_cimd(pool, cfg, client_id, &url, now).await?;
    check_redirect(doc, redirect_uri)
}

fn check_redirect(mut doc: ClientDoc, redirect_uri: &str) -> Result<ClientDoc, ClientError> {
    if !doc
        .redirect_uris
        .iter()
        .any(|uri| redirect_matches(uri, redirect_uri))
    {
        return Err(ClientError::RedirectMismatch);
    }
    // The consent page shows the redirect host the user is returning to.
    if let Ok(requested) = Url::parse(redirect_uri) {
        if let Some(host) = requested.host_str() {
            doc.host = host.to_string();
        }
    }
    Ok(doc)
}

async fn dcr_client(pool: &SqlitePool, client_id: &str) -> anyhow::Result<Option<ClientDoc>> {
    let row: Option<(String, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT client_id, name, host, redirect_uris FROM oauth_client \
         WHERE client_id = ? AND kind = 'dcr'",
    )
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(client_id, name, host, redirect_uris)| {
        let redirect_uris: Vec<String> = serde_json::from_str(&redirect_uris).unwrap_or_default();
        let host = host.unwrap_or_else(|| {
            redirect_host(
                redirect_uris
                    .first()
                    .map(String::as_str)
                    .unwrap_or(&client_id),
            )
        });
        let loopback_only =
            !redirect_uris.is_empty() && redirect_uris.iter().all(|uri| is_loopback_uri(uri));
        ClientDoc {
            name: name.unwrap_or_else(|| format!("Unverified app at {host}")),
            host,
            redirect_uris,
            verified: false,
            client_id,
            loopback_only,
        }
    }))
}

/// Fetch the CIMD document, or reuse the `oauth_client` cache entry. The
/// effective TTL is `min(max-age, 24 h)`, persisted by backdating
/// `fetched_at` (the schema has no TTL column): `fetched_at + 24 h` is the
/// exact expiry.
async fn fetch_cimd(
    pool: &SqlitePool,
    cfg: &AuthConfig,
    client_id: &str,
    url: &Url,
    now: i64,
) -> Result<ClientDoc, ClientError> {
    if let Some(cached) = cached_doc(pool, client_id, now)
        .await
        .map_err(|err| ClientError::Fetch(err.to_string()))?
    {
        return Ok(cached);
    }
    let host = url
        .host_str()
        .ok_or(ClientError::BadDocument("no host".to_string()))?
        .to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    // Resolve first and pin the connection to the vetted address (no DNS
    // rebinding between the check and the fetch).
    let vetted = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|err| ClientError::Fetch(format!("dns failed: {err}")))?
        .map(|addr| addr.ip())
        .find(|ip| cfg.allow_loopback_fetch || is_public_ip(*ip))
        .ok_or(ClientError::PrivateIp)?;
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        // A redirect would leave the vetted, pinned address (SSRF): CIMD
        // documents are served directly or not at all.
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, SocketAddr::new(vetted, port))
        .build()
        .map_err(|err| ClientError::Fetch(format!("client failed: {err}")))?;
    let response = client
        .get(client_id)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|err| ClientError::Fetch(scrubbed(&err)))?;
    if !response.status().is_success() {
        return Err(ClientError::Fetch(format!("http {}", response.status())));
    }
    let ttl_ms = cache_ttl(response.headers());
    if response
        .content_length()
        .is_some_and(|len| len > MAX_DOC_BYTES as u64)
    {
        return Err(ClientError::Oversize);
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| ClientError::Fetch(scrubbed(&err)))?;
    if bytes.len() > MAX_DOC_BYTES {
        return Err(ClientError::Oversize);
    }
    let doc: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| ClientError::BadDocument("not JSON".to_string()))?;
    let doc_id = doc
        .get("client_id")
        .and_then(|id| id.as_str())
        .unwrap_or("");
    if doc_id != client_id {
        return Err(ClientError::ClientIdMismatch);
    }
    let redirect_uris = doc
        .get("redirect_uris")
        .and_then(|uris| uris.as_array())
        .map(|uris| {
            uris.iter()
                .filter_map(|uri| uri.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|uris| !uris.is_empty())
        .ok_or_else(|| ClientError::BadDocument("redirect_uris missing".to_string()))?;
    let name = doc
        .get("client_name")
        .and_then(|name| name.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Unverified app at {host}"));
    let loopback_only = redirect_uris.iter().all(|uri| is_loopback_uri(uri));
    // Backdate so `fetched_at + 24 h` is the exact expiry.
    let fetched_at = now - (DOC_TTL_MS - ttl_ms);
    sqlx::query(
        "INSERT INTO oauth_client (client_id, kind, name, host, redirect_uris, verified, fetched_at, created_at) \
         VALUES (?, 'cimd', ?, ?, ?, 0, ?, ?) \
         ON CONFLICT(client_id) DO UPDATE SET name = excluded.name, host = excluded.host, \
           redirect_uris = excluded.redirect_uris, fetched_at = excluded.fetched_at",
    )
    .bind(client_id)
    .bind(&name)
    .bind(&host)
    .bind(serde_json::to_string(&redirect_uris).unwrap_or_default())
    .bind(fetched_at)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|err| ClientError::Fetch(err.to_string()))?;
    Ok(ClientDoc {
        client_id: client_id.to_string(),
        name,
        host,
        redirect_uris,
        verified: false,
        loopback_only,
    })
}

async fn cached_doc(
    pool: &SqlitePool,
    client_id: &str,
    now: i64,
) -> anyhow::Result<Option<ClientDoc>> {
    let row: Option<CachedDoc> = sqlx::query_as(
        "SELECT name, host, redirect_uris, fetched_at FROM oauth_client \
         WHERE client_id = ? AND kind = 'cimd'",
    )
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    let Some((name, host, redirect_uris, fetched_at)) = row else {
        return Ok(None);
    };
    if fetched_at.is_none_or(|fetched| now - fetched >= DOC_TTL_MS) {
        return Ok(None);
    }
    let redirect_uris: Vec<String> = serde_json::from_str(&redirect_uris).unwrap_or_default();
    let host = host.unwrap_or_else(|| {
        redirect_host(
            redirect_uris
                .first()
                .map(String::as_str)
                .unwrap_or(client_id),
        )
    });
    let loopback_only =
        !redirect_uris.is_empty() && redirect_uris.iter().all(|uri| is_loopback_uri(uri));
    Ok(Some(ClientDoc {
        client_id: client_id.to_string(),
        name: name.unwrap_or_else(|| format!("Unverified app at {host}")),
        host,
        redirect_uris,
        verified: false,
        loopback_only,
    }))
}

/// `min(Cache-Control max-age, 24 h)` in milliseconds.
fn cache_ttl(headers: &reqwest::header::HeaderMap) -> i64 {
    let max_age = headers
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .and_then(|cache| {
            cache.split(',').find_map(|directive| {
                directive
                    .trim()
                    .strip_prefix("max-age=")
                    .and_then(|seconds| seconds.trim().parse::<i64>().ok())
            })
        })
        .unwrap_or(DOC_TTL_MS / 1000);
    max_age.clamp(0, DOC_TTL_MS / 1000) * 1000
}

/// One cached CIMD row: name, host, redirect_uris JSON, fetched_at.
type CachedDoc = (Option<String>, Option<String>, String, Option<i64>);

fn redirect_host(uri: &str) -> String {
    Url::parse(uri)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| uri.to_string())
}

fn is_loopback_uri(uri: &str) -> bool {
    Url::parse(uri)
        .ok()
        .is_some_and(|url| is_loopback_host(url.host_str()))
}

/// A fetch error must never carry request details that could leak tokens.
fn scrubbed(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return "timed out after 3 s".to_string();
    }
    if err.is_connect() {
        return "connection failed".to_string();
    }
    "fetch failed".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_exact_except_loopback_ignores_port() {
        assert!(redirect_matches(
            "https://claude.ai/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callback"
        ));
        assert!(!redirect_matches(
            "https://claude.ai/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callback?x=1"
        ));
        assert!(!redirect_matches(
            "https://claude.ai/api/mcp/auth_callback",
            "https://evil.com/api/mcp/auth_callback"
        ));
        assert!(redirect_matches(
            "http://localhost/callback",
            "http://localhost:43123/callback"
        ));
        assert!(redirect_matches(
            "http://127.0.0.1:9/cb?x=1",
            "http://127.0.0.1:8080/cb?x=1"
        ));
        assert!(!redirect_matches(
            "http://localhost/callback",
            "http://localhost:9/other"
        ));
        assert!(!redirect_matches(
            "http://localhost/callback",
            "https://localhost/callback"
        ));
        assert!(!redirect_matches(
            "https://example.com/cb",
            "https://example.com:8443/cb"
        ));
    }

    #[test]
    fn public_ip_rejects_everything_non_routable() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "0.0.0.0",
            "224.0.0.1",
            "100.64.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "2001:db8::1",
            // IPv4-mapped and transition prefixes that tunnel to private v4.
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "2002:a00:1::1",
            "2001:0:4136:e378::1",
            "64:ff9b::a00:1",
            "100::1",
        ] {
            assert!(
                !is_public_ip(ip.parse().unwrap()),
                "{ip} must not count as public"
            );
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(
                is_public_ip(ip.parse().unwrap()),
                "{ip} must count as public"
            );
        }
    }

    #[test]
    fn cache_ttl_caps_at_24h() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(cache_ttl(&headers), DOC_TTL_MS);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("cache-control", "public, max-age=60".parse().unwrap());
        assert_eq!(cache_ttl(&headers), 60_000);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("cache-control", "max-age=99999999".parse().unwrap());
        assert_eq!(cache_ttl(&headers), DOC_TTL_MS);
    }
}
