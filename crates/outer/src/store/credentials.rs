//! Scoped bucket credentials for fleets and `celld deploy` (spec §12.6).
//!
//! Fleets and deploy steps never hold the root bucket keys. With
//! `OUTER_STORE=r2` and the Cloudflare pair set, outer mints Cloudflare R2
//! temporary credentials scoped to one `fleets/{slug}/` prefix; on localhost
//! dev the parent keys are used directly (`CredentialSource::Static`).
//!
//! Fresh mints cost a Cloudflare round trip, so the minter caches the last
//! grant per prefix (inside the minter, behind a mutex; no globals). A spawn
//! or deploy reuses the cached grant while `refresh_after - age` still
//! covers a day, so a reused grant never forces a restart within a day;
//! otherwise it mints fresh and replaces the entry. The grant carries its
//! mint time so callers know the credential's age.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Credentials for a single app prefix. The secret and token are redacted in
/// `Debug` so a logged struct never leaks them.
///
/// `minted_at` is when Cloudflare minted them (`None` for Static, which never
/// expires); `reused` is whether this grant came from the per-prefix cache
/// rather than a fresh mint (always false for Static).
#[derive(Clone)]
pub struct ScopedCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub minted_at: Option<std::time::Instant>,
    pub reused: bool,
}

impl std::fmt::Debug for ScopedCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .field("minted_at", &self.minted_at)
            .field("reused", &self.reused)
            .finish()
    }
}

/// Where a fleet's or deploy's bucket credentials come from.
#[derive(Clone)]
pub enum CredentialSource {
    /// Local dev only: the parent R2 keys, unscoped, no session token.
    Static {
        access_key_id: String,
        secret_access_key: String,
    },
    /// Production: mint prefix-scoped temporary credentials per spawn/deploy.
    R2Temp(Arc<R2TempMinter>),
}

impl std::fmt::Debug for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialSource::Static { access_key_id, .. } => f
                .debug_struct("Static")
                .field("access_key_id", access_key_id)
                .field("secret_access_key", &"<redacted>")
                .finish(),
            CredentialSource::R2Temp(minter) => f.debug_tuple("R2Temp").field(minter).finish(),
        }
    }
}

impl CredentialSource {
    /// Credentials for `prefix` (e.g. `fleets/{slug}/`). Static returns the
    /// parent keys; R2Temp reuses the cached grant while it stays fresh
    /// enough (see [`should_reuse`]) and mints a fresh one otherwise.
    pub async fn for_prefix(&self, prefix: &str) -> anyhow::Result<ScopedCredentials> {
        validate_prefix(prefix)?;
        match self {
            CredentialSource::Static {
                access_key_id,
                secret_access_key,
            } => Ok(ScopedCredentials {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: None,
                minted_at: None,
                reused: false,
            }),
            CredentialSource::R2Temp(minter) => minter.mint_cached(prefix).await,
        }
    }

    /// Credentials for `prefix`, always freshly minted, replacing the cache
    /// entry. The credential-refresh sweep uses this: restarting on a cached
    /// grant would restart on the same near-expiry credentials.
    pub async fn for_prefix_fresh(&self, prefix: &str) -> anyhow::Result<ScopedCredentials> {
        validate_prefix(prefix)?;
        match self {
            CredentialSource::Static {
                access_key_id,
                secret_access_key,
            } => Ok(ScopedCredentials {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: None,
                minted_at: None,
                reused: false,
            }),
            CredentialSource::R2Temp(minter) => minter.mint_fresh(prefix).await,
        }
    }
    /// Drop the cached grant for `prefix`, if any. `delete_app` calls this
    /// after stopping the fleet: a deleted app must never hand out its old
    /// grant again (a recreated slug mints fresh). Static holds no cache.
    /// Best effort: an unknown prefix is a no-op, never an error.
    pub fn forget_prefix(&self, prefix: &str) {
        if let CredentialSource::R2Temp(minter) = self {
            minter.forget(prefix);
        }
    }
}

/// `fleets/{slug}/`: the only prefix shape outer ever mints for. Must be
/// non-empty, relative, slash-terminated, and free of `..`.
pub fn validate_prefix(prefix: &str) -> anyhow::Result<()> {
    if prefix.is_empty() {
        anyhow::bail!("credential prefix must not be empty");
    }
    if prefix.starts_with('/') {
        anyhow::bail!("credential prefix {prefix:?} must be relative (no leading '/')");
    }
    if !prefix.ends_with('/') {
        anyhow::bail!("credential prefix {prefix:?} must end with '/'");
    }
    if prefix.split('/').any(|segment| segment == "..") {
        anyhow::bail!("credential prefix {prefix:?} must not contain '..'");
    }
    Ok(())
}

/// Cloudflare R2 jurisdiction from an S3 endpoint: the subdomain between the
/// account id and `r2.cloudflarestorage.com` (`eu`, `fedramp`), if any.
pub fn r2_jurisdiction(endpoint: &str) -> Option<String> {
    let host = endpoint
        .rsplit("://")
        .next()?
        .split('/')
        .next()?
        .to_ascii_lowercase();
    let prefix = host.strip_suffix(".r2.cloudflarestorage.com")?;
    if prefix.is_empty() {
        return None;
    }
    if let Some(account) = prefix.strip_suffix(".eu") {
        if !account.is_empty() && !account.contains('.') {
            return Some("eu".to_string());
        }
        return None;
    }
    if let Some(account) = prefix.strip_suffix(".fedramp") {
        if !account.is_empty() && !account.contains('.') {
            return Some("fedramp".to_string());
        }
        return None;
    }
    None
}

/// Explicit inputs for [`R2TempMinter`]; built from the platform config in
/// `main.rs`, never from the environment directly.
pub struct R2TempConfig {
    /// `https://api.cloudflare.com/client/v4`.
    pub api_base: String,
    pub account_id: String,
    /// Cloudflare API token; never appears in errors or logs.
    pub api_token: String,
    pub bucket: String,
    /// `eu` for an EU bucket; sent as `cf-r2-jurisdiction` when set.
    pub jurisdiction: Option<String>,
    /// The parent R2 token's access key id (the S3 `R2_ACCESS_KEY_ID`).
    pub parent_access_key_id: String,
    /// Credential lifetime, 1 s to 7 days (Cloudflare caps TTL at 7 days).
    pub ttl: Duration,
    /// Credential age at which a fleet restarts (TTL minus a day in
    /// production). A cached grant is reused only while
    /// `refresh_after - age` still covers [`REUSE_MARGIN`].
    pub refresh_after: Duration,
}

/// Mints Cloudflare R2 temporary credentials scoped to a prefix list.
///
/// `POST {api_base}/accounts/{account_id}/r2/temp-access-credentials` with a
/// bearer token and body
/// `{"bucket","parentAccessKeyId","permission":"object-read-write",
/// "ttlSeconds","prefixes"}`. Errors carry Cloudflare's error codes/messages
/// and the HTTP status, never the token or the returned secrets.
pub struct R2TempMinter {
    client: reqwest::Client,
    config: R2TempConfig,
    /// Last grant per prefix. Entries hold no lock across the mint round
    /// trip: lookup-then-mint can double-mint under concurrent misses for
    /// one prefix, which the per-slug wake lock already serializes.
    cache: Mutex<HashMap<String, ScopedCredentials>>,
}

impl std::fmt::Debug for R2TempMinter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("R2TempMinter")
            .field("api_base", &self.config.api_base)
            .field("account_id", &self.config.account_id)
            .field("api_token", &"<redacted>")
            .field("bucket", &self.config.bucket)
            .field("jurisdiction", &self.config.jurisdiction)
            .field("parent_access_key_id", &self.config.parent_access_key_id)
            .field("ttl", &self.config.ttl)
            .field("refresh_after", &self.config.refresh_after)
            .field("cached_prefixes", &self.cache_lock().len())
            .finish()
    }
}

/// A reused grant must leave this much credential lifetime: a spawn or
/// deploy on reused credentials never forces a restart within a day.
const REUSE_MARGIN: Duration = Duration::from_secs(86400);

/// Whether a grant minted `age` ago may be reused: `refresh_after - age`
/// must still cover [`REUSE_MARGIN`]. Pure, boundary-tested below.
fn should_reuse(age: Duration, refresh_after: Duration) -> bool {
    refresh_after
        .checked_sub(age)
        .is_some_and(|left| left >= REUSE_MARGIN)
}

#[derive(serde::Deserialize)]
struct TempAccessResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<TempAccessError>,
    #[serde(default)]
    result: Option<TempAccessResult>,
}

#[derive(serde::Deserialize)]
struct TempAccessError {
    #[serde(default)]
    code: serde_json::Value,
    #[serde(default)]
    message: String,
}

#[derive(serde::Deserialize)]
struct TempAccessResult {
    #[serde(rename = "accessKeyId", default)]
    access_key_id: String,
    #[serde(rename = "secretAccessKey", default)]
    secret_access_key: String,
    #[serde(rename = "sessionToken", default)]
    session_token: Option<String>,
}

impl R2TempMinter {
    pub fn new(config: R2TempConfig) -> anyhow::Result<Self> {
        if config.api_base.is_empty() {
            anyhow::bail!("R2TempConfig.api_base must not be empty");
        }
        if config.account_id.is_empty() {
            anyhow::bail!("R2TempConfig.account_id must not be empty");
        }
        if config.api_token.is_empty() {
            anyhow::bail!("R2TempConfig.api_token must not be empty");
        }
        if config.bucket.is_empty() {
            anyhow::bail!("R2TempConfig.bucket must not be empty");
        }
        if config.parent_access_key_id.is_empty() {
            anyhow::bail!("R2TempConfig.parent_access_key_id must not be empty");
        }
        if config.ttl.is_zero() || config.ttl > Duration::from_secs(7 * 86400) {
            anyhow::bail!(
                "R2TempConfig.ttl of {}s is out of range (1s to 604800s / 7 days)",
                config.ttl.as_secs()
            );
        }
        // Reuse and the refresh restart both key off `refresh_after`: it must
        // land before Cloudflare expires the grant, or a cached credential
        // could be handed out (or a fleet kept running) past its TTL.
        if config.refresh_after.is_zero() || config.refresh_after >= config.ttl {
            anyhow::bail!(
                "R2TempConfig.refresh_after of {}s must be positive and below the {}s ttl",
                config.refresh_after.as_secs(),
                config.ttl.as_secs()
            );
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| anyhow::anyhow!("building R2 credential client: {e:#}"))?;
        Ok(Self {
            client,
            config,
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub async fn mint(&self, prefix: &str) -> anyhow::Result<ScopedCredentials> {
        validate_prefix(prefix)?;
        let url = format!(
            "{}/accounts/{}/r2/temp-access-credentials",
            self.config.api_base.trim_end_matches('/'),
            self.config.account_id
        );
        let body = serde_json::json!({
            "bucket": self.config.bucket,
            "parentAccessKeyId": self.config.parent_access_key_id,
            "permission": "object-read-write",
            "ttlSeconds": self.config.ttl.as_secs(),
            "prefixes": [prefix],
        });
        let mut request = self
            .client
            .post(&url)
            .bearer_auth(&self.config.api_token)
            .json(&body);
        if let Some(jurisdiction) = &self.config.jurisdiction {
            request = request.header("cf-r2-jurisdiction", jurisdiction);
        }
        let response = request.send().await.map_err(|e| {
            anyhow::anyhow!("Cloudflare temp-access-credentials request failed: {e:#}")
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|e| {
            anyhow::anyhow!("reading Cloudflare temp-access-credentials response: {e:#}")
        })?;
        let parsed: TempAccessResponse = serde_json::from_str(&text).map_err(|_| {
            anyhow::anyhow!(
                "Cloudflare temp-access-credentials returned HTTP {status} with a non-JSON body: {}",
                truncate(&text, 500)
            )
        })?;
        match (parsed.success, parsed.result) {
            (true, Some(result)) if !result.access_key_id.is_empty() => Ok(ScopedCredentials {
                access_key_id: result.access_key_id,
                secret_access_key: result.secret_access_key,
                session_token: result.session_token,
                minted_at: Some(std::time::Instant::now()),
                reused: false,
            }),
            _ => {
                let detail = parsed
                    .errors
                    .iter()
                    .map(|e| format!("[{}] {}", e.code, e.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                anyhow::bail!(
                    "Cloudflare temp-access-credentials failed (HTTP {status}): {}",
                    if detail.is_empty() {
                        "no error detail".to_string()
                    } else {
                        detail
                    }
                )
            }
        }
    }
    /// The cache lock, recovered from poisoning: the critical sections only
    /// do `HashMap` ops, so poison means a prior panic elsewhere, never
    /// here; dropping the entry (a re-mint) is always safe.
    fn cache_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ScopedCredentials>> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Credentials for `prefix`, reusing the cached grant while
    /// [`should_reuse`] holds, else minting fresh. A hit returns the
    /// original `minted_at` with `reused` set, so callers see the grant's
    /// true age.
    pub async fn mint_cached(&self, prefix: &str) -> anyhow::Result<ScopedCredentials> {
        validate_prefix(prefix)?;
        let cached = self.cache_lock().get(prefix).cloned();
        if let Some(mut creds) = cached {
            let fresh = creds
                .minted_at
                .is_some_and(|minted| should_reuse(minted.elapsed(), self.config.refresh_after));
            if fresh {
                creds.reused = true;
                return Ok(creds);
            }
        }
        self.mint_fresh(prefix).await
    }

    /// Credentials for `prefix`, always freshly minted, replacing the cache
    /// entry. Failures leave the old entry in place: a failed mint never
    /// evicts a grant a fleet may still be running on.
    pub async fn mint_fresh(&self, prefix: &str) -> anyhow::Result<ScopedCredentials> {
        validate_prefix(prefix)?;
        let creds = self.mint(prefix).await?;
        self.cache_lock().insert(prefix.to_string(), creds.clone());
        Ok(creds)
    }

    /// Drop the cached grant for `prefix`. `delete_app` calls this after the
    /// fleet stops; removing (never minting) cannot fail.
    pub fn forget(&self, prefix: &str) {
        self.cache_lock().remove(prefix);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn prefix_validation_rejects_shapes_outside_fleets() {
        assert!(validate_prefix("fleets/grocery/").is_ok());
        for bad in [
            "",
            "/fleets/grocery/",
            "fleets/grocery",
            "fleets",
            "fleets/../other/",
            "../fleets/grocery/",
            "fleets/../../etc/",
        ] {
            assert!(validate_prefix(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn jurisdiction_derivation_table() {
        assert_eq!(
            r2_jurisdiction("https://abc123.eu.r2.cloudflarestorage.com"),
            Some("eu".to_string())
        );
        assert_eq!(
            r2_jurisdiction("https://abc123.fedramp.r2.cloudflarestorage.com"),
            Some("fedramp".to_string())
        );
        assert_eq!(
            r2_jurisdiction("https://abc123.r2.cloudflarestorage.com"),
            None
        );
        assert_eq!(r2_jurisdiction("https://r2.cloudflarestorage.com"), None);
        // Case-insensitive host; trailing paths don't matter.
        assert_eq!(
            r2_jurisdiction("https://ABC123.EU.r2.CloudflareStorage.com/outer"),
            Some("eu".to_string())
        );
        assert_eq!(r2_jurisdiction("https://s3.example.com"), None);
        assert_eq!(
            r2_jurisdiction("https://evil.eu.r2.cloudflarestorage.com.evil.com"),
            None
        );
        assert_eq!(r2_jurisdiction("not a url at all"), None);
        assert_eq!(r2_jurisdiction(""), None);
    }

    #[test]
    fn debug_never_carries_secrets_or_tokens() {
        let creds = ScopedCredentials {
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "topsecret".to_string(),
            session_token: Some("sessiontoken".to_string()),
            minted_at: None,
            reused: false,
        };
        let shown = format!("{creds:?}");
        assert!(shown.contains("AKIAEXAMPLE"));
        assert!(!shown.contains("topsecret"));
        assert!(!shown.contains("sessiontoken"));

        let source = CredentialSource::Static {
            access_key_id: "parent".to_string(),
            secret_access_key: "parentsecret".to_string(),
        };
        let shown = format!("{source:?}");
        assert!(!shown.contains("parentsecret"));

        let minter = R2TempMinter::new(R2TempConfig {
            api_base: "https://api.cloudflare.com/client/v4".to_string(),
            account_id: "acct".to_string(),
            api_token: "cf-token-secret".to_string(),
            bucket: "outer".to_string(),
            jurisdiction: Some("eu".to_string()),
            parent_access_key_id: "parent".to_string(),
            ttl: Duration::from_secs(604800),
            refresh_after: Duration::from_secs(518400),
        })
        .unwrap();
        let shown = format!("{minter:?}");
        assert!(!shown.contains("cf-token-secret"));
    }

    #[test]
    fn minter_rejects_an_out_of_range_ttl() {
        let config = |ttl: Duration| R2TempConfig {
            api_base: "https://api.cloudflare.com/client/v4".to_string(),
            account_id: "acct".to_string(),
            api_token: "token".to_string(),
            bucket: "outer".to_string(),
            jurisdiction: None,
            parent_access_key_id: "parent".to_string(),
            ttl,
            refresh_after: Duration::from_secs(1800),
        };
        assert!(R2TempMinter::new(config(Duration::ZERO)).is_err());
        assert!(R2TempMinter::new(config(Duration::from_secs(7 * 86400 + 1))).is_err());
        assert!(R2TempMinter::new(config(Duration::from_secs(7 * 86400))).is_ok());
        assert!(R2TempMinter::new(config(Duration::from_secs(3600))).is_ok());
    }

    #[test]
    fn minter_rejects_a_refresh_at_or_past_the_ttl() {
        let config = |ttl: u64, refresh_after: u64| R2TempConfig {
            api_base: "https://api.cloudflare.com/client/v4".to_string(),
            account_id: "acct".to_string(),
            api_token: "token".to_string(),
            bucket: "outer".to_string(),
            jurisdiction: None,
            parent_access_key_id: "parent".to_string(),
            ttl: Duration::from_secs(ttl),
            refresh_after: Duration::from_secs(refresh_after),
        };
        assert!(
            R2TempMinter::new(config(3600, 6 * 86400)).is_err(),
            "refresh past ttl"
        );
        assert!(
            R2TempMinter::new(config(3600, 3600)).is_err(),
            "refresh at ttl"
        );
        assert!(R2TempMinter::new(config(3600, 0)).is_err(), "zero refresh");
        assert!(
            R2TempMinter::new(config(7 * 86400, 6 * 86400)).is_ok(),
            "production shape"
        );
    }

    /// What the fake Cloudflare API saw, how many mints it served, and how
    /// it should answer.
    #[derive(Default)]
    struct Seen {
        path: String,
        authorization: Option<String>,
        jurisdiction: Option<String>,
        body: serde_json::Value,
        count: usize,
    }

    #[derive(Clone)]
    enum FakeMode {
        Ok {
            access: String,
            secret: String,
            token: String,
        },
        /// A fresh unique grant per mint (`minted-id-{n}`), so tests can
        /// tell a reused grant (same id) from a fresh mint (new id).
        Unique,
        Fail {
            code: i64,
            message: String,
        },
        Garbage(axum::http::StatusCode),
    }

    #[derive(Clone)]
    struct FakeState {
        seen: Arc<Mutex<Seen>>,
        mode: FakeMode,
    }

    async fn fake_cloudflare(mode: FakeMode) -> (String, Arc<Mutex<Seen>>) {
        use axum::extract::{Path, State};
        use axum::http::HeaderMap;
        use axum::response::IntoResponse;
        use axum::routing::post;

        let seen = Arc::new(Mutex::new(Seen::default()));
        let state = FakeState {
            seen: seen.clone(),
            mode,
        };
        async fn handler(
            State(state): State<FakeState>,
            Path(account): Path<String>,
            headers: HeaderMap,
            body: axum::Json<serde_json::Value>,
        ) -> axum::response::Response {
            let count = {
                let mut seen = state.seen.lock().unwrap();
                seen.path = format!("/client/v4/accounts/{account}/r2/temp-access-credentials");
                seen.authorization = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                seen.jurisdiction = headers
                    .get("cf-r2-jurisdiction")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                seen.body = body.0.clone();
                seen.count += 1;
                seen.count
            };
            match state.mode {
                FakeMode::Ok {
                    access,
                    secret,
                    token,
                } => axum::Json(serde_json::json!({
                    "success": true,
                    "errors": [],
                    "result": {
                        "accessKeyId": access,
                        "secretAccessKey": secret,
                        "sessionToken": token,
                    },
                }))
                .into_response(),
                FakeMode::Unique => axum::Json(serde_json::json!({
                    "success": true,
                    "errors": [],
                    "result": {
                        "accessKeyId": format!("minted-id-{count}"),
                        "secretAccessKey": format!("minted-secret-{count}"),
                        "sessionToken": format!("minted-token-{count}"),
                    },
                }))
                .into_response(),
                FakeMode::Fail { code, message } => axum::Json(serde_json::json!({
                    "success": false,
                    "errors": [{"code": code, "message": message}],
                }))
                .into_response(),
                FakeMode::Garbage(status) => (status, "upstream exploded").into_response(),
            }
        }
        let app = axum::Router::new()
            .route(
                "/client/v4/accounts/{account}/r2/temp-access-credentials",
                post(handler),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/client/v4"), seen)
    }

    fn minter_for(
        api_base: &str,
        with_jurisdiction: bool,
        refresh_after: Duration,
    ) -> R2TempMinter {
        R2TempMinter::new(R2TempConfig {
            api_base: api_base.to_string(),
            account_id: "test-acct".to_string(),
            api_token: "cf-test-token".to_string(),
            bucket: "outer".to_string(),
            jurisdiction: with_jurisdiction.then(|| "eu".to_string()),
            parent_access_key_id: "parent-key-id".to_string(),
            ttl: Duration::from_secs(7 * 86400),
            refresh_after,
        })
        .unwrap()
    }

    /// Production-shaped window: 7-day TTL, refresh after 6 days.
    fn production_minter_for(api_base: &str) -> R2TempMinter {
        minter_for(api_base, true, Duration::from_secs(6 * 86400))
    }

    #[tokio::test]
    async fn mint_posts_the_exact_body_with_bearer_and_jurisdiction() {
        let (api_base, seen) = fake_cloudflare(FakeMode::Ok {
            access: "minted-id".to_string(),
            secret: "minted-secret".to_string(),
            token: "minted-token".to_string(),
        })
        .await;
        let minter = minter_for(&api_base, true, Duration::from_secs(1800));
        let creds = minter.mint("fleets/grocery/").await.unwrap();
        assert_eq!(creds.access_key_id, "minted-id");
        assert_eq!(creds.secret_access_key, "minted-secret");
        assert_eq!(creds.session_token.as_deref(), Some("minted-token"));

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.path,
            "/client/v4/accounts/test-acct/r2/temp-access-credentials"
        );
        assert_eq!(seen.authorization.as_deref(), Some("Bearer cf-test-token"));
        assert_eq!(seen.jurisdiction.as_deref(), Some("eu"));
        assert_eq!(
            seen.body,
            serde_json::json!({
                "bucket": "outer",
                "parentAccessKeyId": "parent-key-id",
                "permission": "object-read-write",
                "ttlSeconds": 604800,
                "prefixes": ["fleets/grocery/"],
            })
        );
    }

    #[tokio::test]
    async fn mint_omits_the_jurisdiction_header_without_one() {
        let (api_base, seen) = fake_cloudflare(FakeMode::Ok {
            access: "id".to_string(),
            secret: "secret".to_string(),
            token: "token".to_string(),
        })
        .await;
        minter_for(&api_base, false, Duration::from_secs(1800))
            .mint("fleets/grocery/")
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.jurisdiction, None);
        assert_eq!(seen.authorization.as_deref(), Some("Bearer cf-test-token"));
    }

    #[tokio::test]
    async fn cloudflare_failure_carries_the_message_but_not_the_token() {
        let (api_base, _) = fake_cloudflare(FakeMode::Fail {
            code: 10001,
            message: "parent key lacks permission".to_string(),
        })
        .await;
        let err = minter_for(&api_base, true, Duration::from_secs(1800))
            .mint("fleets/grocery/")
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("parent key lacks permission"), "{text}");
        assert!(text.contains("10001"), "{text}");
        assert!(!text.contains("cf-test-token"), "{text}");
    }

    #[tokio::test]
    async fn non_json_5xx_is_an_error() {
        let (api_base, _) =
            fake_cloudflare(FakeMode::Garbage(axum::http::StatusCode::BAD_GATEWAY)).await;
        let err = minter_for(&api_base, false, Duration::from_secs(1800))
            .mint("fleets/grocery/")
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("502"), "{text}");
        assert!(!text.contains("cf-test-token"), "{text}");
    }

    #[tokio::test]
    async fn static_source_returns_the_parent_keys_without_a_token() {
        let source = CredentialSource::Static {
            access_key_id: "parent".to_string(),
            secret_access_key: "parentsecret".to_string(),
        };
        let creds = source.for_prefix("fleets/grocery/").await.unwrap();
        assert_eq!(creds.access_key_id, "parent");
        assert_eq!(creds.secret_access_key, "parentsecret");
        assert_eq!(creds.session_token, None);
    }

    #[tokio::test]
    async fn for_prefix_validates_before_any_network() {
        let (api_base, seen) = fake_cloudflare(FakeMode::Ok {
            access: "id".to_string(),
            secret: "secret".to_string(),
            token: "token".to_string(),
        })
        .await;
        let source = CredentialSource::R2Temp(Arc::new(minter_for(
            &api_base,
            true,
            Duration::from_secs(1800),
        )));
        assert!(source.for_prefix("../escape/").await.is_err());
        // Rejected before the request: the fake saw nothing.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.body, serde_json::Value::Null);
        assert_eq!(seen.count, 0);
    }

    #[test]
    fn reuse_window_at_its_boundaries() {
        let refresh = Duration::from_secs(6 * 86400);
        let day = Duration::from_secs(86400);
        // Fresh grants reuse; exactly a day of lifetime left still reuses.
        assert!(should_reuse(Duration::ZERO, refresh));
        assert!(should_reuse(refresh - day, refresh));
        // A second past the margin mints fresh, as do older grants.
        assert!(!should_reuse(
            refresh - day + Duration::from_secs(1),
            refresh
        ));
        assert!(!should_reuse(refresh, refresh));
        assert!(!should_reuse(refresh + Duration::from_secs(1), refresh));
        // A refresh window under a day never reuses: always mint fresh.
        assert!(!should_reuse(Duration::ZERO, Duration::from_secs(1800)));
        assert!(!should_reuse(Duration::ZERO, day - Duration::from_secs(1)));
        assert!(should_reuse(Duration::ZERO, day));
        assert!(!should_reuse(Duration::from_secs(1), day));
    }

    #[tokio::test]
    async fn spawns_within_the_window_share_one_mint() {
        let (api_base, seen) = fake_cloudflare(FakeMode::Unique).await;
        let source = CredentialSource::R2Temp(Arc::new(production_minter_for(&api_base)));
        let first = source.for_prefix("fleets/grocery/").await.unwrap();
        let second = source.for_prefix("fleets/grocery/").await.unwrap();
        assert_eq!(seen.lock().unwrap().count, 1, "one mint serves both");
        assert!(!first.reused);
        assert!(second.reused);
        assert_eq!(first.access_key_id, second.access_key_id);
        assert_eq!(first.minted_at, second.minted_at);
        assert!(first.minted_at.is_some());
    }

    #[tokio::test]
    async fn stale_cached_grant_mints_again() {
        let (api_base, seen) = fake_cloudflare(FakeMode::Unique).await;
        let source = CredentialSource::R2Temp(Arc::new(production_minter_for(&api_base)));
        let first = source.for_prefix("fleets/grocery/").await.unwrap();
        // Age the entry past the reuse window (6 days old with a 6-day
        // refresh: no lifetime margin left).
        if let CredentialSource::R2Temp(minter) = &source {
            let mut cache = minter.cache_lock();
            let entry = cache.get_mut("fleets/grocery/").unwrap();
            entry.minted_at = Some(std::time::Instant::now() - Duration::from_secs(6 * 86400));
        } else {
            unreachable!();
        }
        let second = source.for_prefix("fleets/grocery/").await.unwrap();
        assert_eq!(seen.lock().unwrap().count, 2);
        assert!(!second.reused);
        assert_ne!(first.access_key_id, second.access_key_id);
    }

    #[tokio::test]
    async fn cache_is_per_prefix_and_never_shared_across_slugs() {
        let (api_base, seen) = fake_cloudflare(FakeMode::Unique).await;
        let source = CredentialSource::R2Temp(Arc::new(production_minter_for(&api_base)));
        let a = source.for_prefix("fleets/grocery/").await.unwrap();
        let b = source.for_prefix("fleets/other/").await.unwrap();
        assert_eq!(seen.lock().unwrap().count, 2);
        // Critical: two slugs never share credentials.
        assert_ne!(a.access_key_id, b.access_key_id);
        assert_ne!(a.secret_access_key, b.secret_access_key);
        let a_again = source.for_prefix("fleets/grocery/").await.unwrap();
        assert_eq!(seen.lock().unwrap().count, 2, "a reuses its own entry");
        assert!(a_again.reused);
        assert_eq!(a_again.access_key_id, a.access_key_id);
        let b_again = source.for_prefix("fleets/other/").await.unwrap();
        assert_eq!(seen.lock().unwrap().count, 2, "b reuses its own entry");
        assert!(b_again.reused);
        assert_eq!(b_again.access_key_id, b.access_key_id);
    }

    #[tokio::test]
    async fn static_source_never_caches_or_ages() {
        let source = CredentialSource::Static {
            access_key_id: "parent".to_string(),
            secret_access_key: "parentsecret".to_string(),
        };
        let first = source.for_prefix("fleets/grocery/").await.unwrap();
        let second = source.for_prefix("fleets/grocery/").await.unwrap();
        for creds in [&first, &second] {
            assert_eq!(creds.access_key_id, "parent");
            assert_eq!(creds.session_token, None);
            assert_eq!(creds.minted_at, None);
            assert!(!creds.reused);
        }
        // No expiry, no age: Static grants never trigger a refresh.
        assert_eq!(second.access_key_id, first.access_key_id);
    }
}
