//! Cloudflare edge cache purge (spec §4.5).
//!
//! Apps are served at `https://<slug>.<apps_domain>` behind Cloudflare,
//! which caches unhashed static files by extension (`/favicon.svg`,
//! `/manifest.webmanifest`, `/sw.js`) for hours. After a deploy or a delete
//! the app's host is purged so visitors stop getting the old files.
//!
//! [`CachePurge`] carries the zone credentials; [`purge_host_cache`] fires
//! the purge. Like the other Cloudflare clients (email, Jev, R2 minting) the
//! base URL is overridable so tests point it at a local fake.

/// `https://api.cloudflare.com/client/v4`, overridable for tests.
const DEFAULT_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Cloudflare zone purge credentials (spec §4.5, §7.6).
#[derive(Clone)]
pub struct CachePurge {
    pub zone_id: String,
    pub api_token: String,
    api_base: String,
}

impl std::fmt::Debug for CachePurge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachePurge")
            .field("zone_id", &self.zone_id)
            .field("api_token", &"<redacted>")
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl CachePurge {
    /// The production purge: Cloudflare's API with the zone and token.
    pub fn new(zone_id: String, api_token: String) -> Self {
        Self {
            zone_id,
            api_token,
            api_base: DEFAULT_API_BASE.to_string(),
        }
    }

    /// Point the purge at a fake API base (tests only).
    #[cfg(test)]
    pub fn with_api_base(self, api_base: String) -> Self {
        Self { api_base, ..self }
    }
}

/// Purge `host` from the Cloudflare edge cache (spec §4.5). `None` skips
/// with a debug (localhost dev without `CLOUDFLARE_ZONE_ID`); failures warn
/// and continue — the entry expires on its own, so there is nothing to
/// retry against. Logs success at info.
pub async fn purge_host_cache(purge: Option<&CachePurge>, host: &str) {
    let Some(purge) = purge else {
        tracing::debug!(
            host,
            "no CLOUDFLARE_ZONE_ID: skipping edge cache purge (localhost dev)"
        );
        return;
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build();
    let Ok(client) = client else {
        tracing::warn!(host, "cache purge client failed to build; skipping purge");
        return;
    };
    let url = format!("{}/zones/{}/purge_cache", purge.api_base, purge.zone_id);
    let response = client
        .post(&url)
        .bearer_auth(&purge.api_token)
        .json(&serde_json::json!({ "hosts": [host] }))
        .send()
        .await;
    match response {
        Err(err) => tracing::warn!(
            host,
            error = %format!("{err:#}"),
            "edge cache purge failed; the entry expires on its own"
        ),
        Ok(response) => {
            let status = response.status();
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            if !status.is_success() || body.get("success").and_then(|v| v.as_bool()) != Some(true) {
                tracing::warn!(
                    host,
                    %status,
                    body = %body.to_string(),
                    "edge cache purge refused; the entry expires on its own"
                );
            } else {
                tracing::info!(host, "purged edge cache host");
            }
        }
    }
}

/// Cloudflare for SaaS custom-hostname credentials (spec §7.7): the same
/// zone and API token the purge uses. The zone must have SaaS enabled with
/// an active fallback origin plus a CNAME target (e.g.
/// `customers.outer.now` → the fallback); the token needs
/// `SSL and Certificates: Write` (and `Zone: Read` for status polling).
#[derive(Clone)]
pub struct CustomHostnames {
    zone_id: String,
    api_token: String,
    api_base: String,
}

impl std::fmt::Debug for CustomHostnames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomHostnames")
            .field("zone_id", &self.zone_id)
            .field("api_token", &"<redacted>")
            .finish()
    }
}

impl CustomHostnames {
    /// The production client: Cloudflare's API with the zone and token.
    pub fn new(zone_id: String, api_token: String) -> Self {
        Self {
            zone_id,
            api_token,
            api_base: DEFAULT_API_BASE.to_string(),
        }
    }

    /// Point the client at a fake API base (tests only).
    #[cfg(test)]
    pub fn with_api_base(self, api_base: String) -> Self {
        Self { api_base, ..self }
    }

    /// The same zone + token the purge carries, or `None` on localhost dev
    /// without `CLOUDFLARE_ZONE_ID` (custom domains need a real zone).
    pub fn from_purge(purge: Option<&CachePurge>) -> Option<Self> {
        // Same module, so the fake base tests override survives too.
        purge.map(|p| Self {
            zone_id: p.zone_id.clone(),
            api_token: p.api_token.clone(),
            api_base: p.api_base.clone(),
        })
    }

    fn client() -> Result<reqwest::Client, CfError> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| CfError::Transport(format!("{e:#}")))
    }

    /// `POST /zones/{zone}/custom_hostnames` with HTTP DCV (`ssl.method
    /// "http"`, type "dv"): validation completes once the customer's CNAME
    /// points at us. The POST response may omit `validation_records`, so
    /// callers must GET the hostname for the DNS records to show the user
    /// (see [`CustomHostnames::get`]).
    pub async fn create(&self, hostname: &str) -> Result<CustomHostname, CfError> {
        let client = Self::client()?;
        let url = format!("{}/zones/{}/custom_hostnames", self.api_base, self.zone_id);
        let response = client
            .post(&url)
            .bearer_auth(&self.api_token)
            .json(&serde_json::json!({
                "hostname": hostname,
                "ssl": {"method": "http", "type": "dv"},
            }))
            .send()
            .await
            .map_err(|e| CfError::Transport(format!("{e:#}")))?;
        Self::parse(url, response).await
    }

    /// `GET /zones/{zone}/custom_hostnames/{id}`: the source of truth for
    /// onboarding state (hostname `status` + `ssl.status` + records).
    pub async fn get(&self, id: &str) -> Result<CustomHostname, CfError> {
        let client = Self::client()?;
        let url = format!(
            "{}/zones/{}/custom_hostnames/{}",
            self.api_base, self.zone_id, id
        );
        let response = client
            .get(&url)
            .bearer_auth(&self.api_token)
            .send()
            .await
            .map_err(|e| CfError::Transport(format!("{e:#}")))?;
        Self::parse(url, response).await
    }

    /// `DELETE /zones/{zone}/custom_hostnames/{id}`. A 404 (already gone
    /// at Cloudflare) is success: the hostname is gone either way.
    pub async fn delete(&self, id: &str) -> Result<(), CfError> {
        let client = Self::client()?;
        let url = format!(
            "{}/zones/{}/custom_hostnames/{}",
            self.api_base, self.zone_id, id
        );
        let response = client
            .delete(&url)
            .bearer_auth(&self.api_token)
            .send()
            .await
            .map_err(|e| CfError::Transport(format!("{e:#}")))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        if !status.is_success() || body.get("success").and_then(|v| v.as_bool()) != Some(true) {
            return Err(Self::refused(status, &body));
        }
        Ok(())
    }

    async fn parse(url: String, response: reqwest::Response) -> Result<CustomHostname, CfError> {
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(CfError::NotFound);
        }
        if !status.is_success() || body.get("success").and_then(|v| v.as_bool()) != Some(true) {
            return Err(Self::refused(status, &body));
        }
        let result = &body["result"];
        CustomHostname::decode(result).ok_or_else(|| {
            CfError::Api(format!(
                "custom hostname response had no usable result: {url}"
            ))
        })
    }

    fn refused(status: reqwest::StatusCode, body: &serde_json::Value) -> CfError {
        let errors = body
            .get("errors")
            .and_then(|v| v.as_array())
            .map(|es| {
                es.iter()
                    .filter_map(|e| {
                        Some(format!(
                            "{}: {}",
                            e.get("code").and_then(|v| v.as_i64())?,
                            e.get("message").and_then(|v| v.as_str())?
                        ))
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();
        match status {
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => CfError::Auth(
                format!("Cloudflare refused the request ({status}): {errors}"),
            ),
            reqwest::StatusCode::TOO_MANY_REQUESTS => CfError::RateLimited,
            s if s == reqwest::StatusCode::CONFLICT
                || errors.contains("1013")
                || errors.contains("already exists")
                || errors.contains("already a custom hostname") =>
            {
                CfError::Conflict(errors)
            }
            // 400: Cloudflare validated the input and said no (a prohibited
            // or malformed hostname). That's the caller's input, not an outage.
            // Routing/request-shape codes (bad zone id, unroutable path) are
            // our configuration, not the hostname: keep those as Api errors.
            reqwest::StatusCode::BAD_REQUEST if !request_config_error(body) => CfError::Rejected(
                body.get("errors")
                    .and_then(|v| v.as_array())
                    .map(|es| {
                        es.iter()
                            .filter_map(|e| e.get("message").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .filter(|m| !m.is_empty())
                    .unwrap_or(errors),
            ),
            _ => CfError::Api(format!(
                "Cloudflare refused the request ({status}): {errors}"
            )),
        }
    }
}

/// Cloudflare error codes that mean the request itself was misrouted or
/// malformed (zone identifier, path), so a 400 carrying them is Outer's
/// configuration problem rather than a verdict on the customer's hostname.
const REQUEST_CONFIG_CODES: [i64; 4] = [1402, 1403, 7000, 7003];

fn request_config_error(body: &serde_json::Value) -> bool {
    body.get("errors")
        .and_then(|v| v.as_array())
        .is_some_and(|es| {
            es.iter().any(|e| {
                e.get("code")
                    .and_then(|v| v.as_i64())
                    .is_some_and(|c| REQUEST_CONFIG_CODES.contains(&c))
            })
        })
}

/// What went wrong talking to the custom-hostnames API. Callers map these
/// to agent-facing errors; the raw strings never reach agents.
#[derive(Debug, thiserror::Error)]
pub enum CfError {
    /// Cloudflare rejected the input (HTTP 400), with its own explanation.
    #[error("Cloudflare rejected the request: {0}")]
    Rejected(String),
    /// The hostname id is unknown at Cloudflare (deleted elsewhere).
    #[error("custom hostname not found at Cloudflare")]
    NotFound,
    /// The hostname is already a custom hostname on some zone.
    #[error("custom hostname conflict at Cloudflare: {0}")]
    Conflict(String),
    /// Bad token or missing permission.
    #[error("Cloudflare auth failed: {0}")]
    Auth(String),
    /// Slow down and retry.
    #[error("Cloudflare rate limited the request")]
    RateLimited,
    /// Anything else the API refused.
    #[error("Cloudflare API error: {0}")]
    Api(String),
    /// No response at all.
    #[error("Cloudflare unreachable: {0}")]
    Transport(String),
}

/// One DNS record the user must create at their provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsRecord {
    /// `CNAME` or `TXT`.
    pub record_type: &'static str,
    /// The record name (relative to the hostname's zone where shown).
    pub name: String,
    /// The record value.
    pub value: String,
}

/// A custom hostname as Cloudflare reports it, plus the DNS records the
/// user must create: the CNAME to our target, the hostname-ownership TXT
/// (pre-validation, works before DNS cutover), and the certificate DCV
/// records when the API returns them.
#[derive(Debug, Clone)]
pub struct CustomHostname {
    pub id: String,
    pub hostname: String,
    /// Hostname validation: Cloudflare can proxy traffic when `active`.
    pub status: String,
    /// Certificate issuance: issued and deployed when `active`.
    pub ssl_status: String,
    pub ssl_method: String,
    /// TXT DCV record (`ssl.validation_records`), if returned yet.
    pub dcv_txt: Option<DnsRecord>,
    /// Hostname-ownership TXT (`ownership_verification`), if returned yet.
    pub ownership_txt: Option<DnsRecord>,
    /// Hostname-ownership over HTTP (after the CNAME points at us).
    pub ownership_http_url: Option<String>,
}

impl CustomHostname {
    /// Ready for production traffic: proxied AND certified.
    pub fn is_active(&self) -> bool {
        self.status == "active" && self.ssl_status == "active"
    }

    /// Permanently stuck: show the reason and stop polling blindly.
    /// `validation_timed_out` is the common one (DCV never completed).
    pub fn is_failed(&self) -> bool {
        self.status == "failed"
            || matches!(
                self.ssl_status.as_str(),
                "validation_timed_out" | "failed" | "deleted" | "deactivated"
            )
    }

    fn decode(result: &serde_json::Value) -> Option<Self> {
        let get = |key: &str| result.get(key).and_then(|v| v.as_str()).map(str::to_string);
        // Optional: the POST response often omits `validation_records` —
        // only the follow-up GET carries them (per the docs).
        let dcv_txt = result
            .get("ssl")
            .and_then(|s| s.get("validation_records"))
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|r| {
                    (r.get("type").and_then(|v| v.as_str())? == "txt").then(|| DnsRecord {
                        record_type: "TXT",
                        name: r
                            .get("txt_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        value: r
                            .get("txt_value")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                })
            })
            .filter(|r| !r.name.is_empty() && !r.value.is_empty());
        let ownership = result.get("ownership_verification");
        let ownership_txt = ownership
            .and_then(|o| {
                (o.get("type").and_then(|v| v.as_str())? == "txt").then(|| DnsRecord {
                    record_type: "TXT",
                    name: o
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    value: o
                        .get("value")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
            })
            .filter(|r| !r.name.is_empty() && !r.value.is_empty());
        let ownership_http_url = result
            .get("ownership_verification_http")
            .and_then(|o| o.get("http_url").or_else(|| o.get("http_body")))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Some(Self {
            id: get("id")?,
            hostname: get("hostname")?,
            status: get("status")?,
            ssl_status: result
                .get("ssl")?
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            ssl_method: result
                .get("ssl")
                .and_then(|s| s.get("method"))
                .and_then(|v| v.as_str())
                .unwrap_or("http")
                .to_string(),
            dcv_txt,
            ownership_txt,
            ownership_http_url,
        })
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A hand-rolled Cloudflare purge stand-in on 127.0.0.1: records the
    //! request path, bearer token and body, always answers success. No
    //! framework, so assertions stay deterministic.

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    pub(crate) struct RecordedRequest {
        pub path: String,
        pub bearer: Option<String>,
        pub body: serde_json::Value,
    }

    pub(crate) struct FakePurge {
        pub base_url: String,
        pub requests: Arc<Mutex<Vec<RecordedRequest>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakePurge {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakePurge {
        pub(crate) async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let task = tokio::spawn(serve(listener, requests.clone()));
            Self {
                base_url: format!("http://127.0.0.1:{port}"),
                requests,
                task,
            }
        }

        pub(crate) fn calls(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        /// Poll until `n` requests arrived or `timeout` passes; returns what
        /// arrived. The purge fires from a detached task, so a plain assert
        /// would race it.
        pub(crate) async fn wait_for(&self, n: usize, timeout: std::time::Duration) -> usize {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let len = self.calls();
                if len >= n || tokio::time::Instant::now() >= deadline {
                    return len;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
    }

    async fn serve(listener: tokio::net::TcpListener, requests: Arc<Mutex<Vec<RecordedRequest>>>) {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                return;
            };
            let requests = requests.clone();
            tokio::spawn(async move {
                handle(conn, &requests).await;
            });
        }
    }

    async fn handle(conn: tokio::net::TcpStream, requests: &Arc<Mutex<Vec<RecordedRequest>>>) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        let mut lines = tokio::io::BufReader::new(conn);
        let mut request_line = String::new();
        if lines.read_line(&mut request_line).await.is_err() {
            return;
        }
        let mut headers = HashMap::new();
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            if lines.read_line(&mut line).await.is_err() {
                return;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 && lines.read_exact(&mut body).await.is_err() {
            return;
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let bearer = headers
            .get("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        requests.lock().unwrap().push(RecordedRequest {
            path,
            bearer,
            body: parsed,
        });
        let payload = r#"{"success":true,"errors":[],"result":{}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let mut conn = lines.into_inner();
        let _ = conn.write_all(response.as_bytes()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakePurge;
    use super::*;

    /// A 400 is the hostname's fault only when Cloudflare says so: a
    /// prohibited hostname is `Rejected` with Cloudflare's own words, a
    /// misrouted request (bad zone id) stays an `Api` error.
    #[test]
    fn bad_request_splits_hostname_verdicts_from_request_config() {
        let prohibited = serde_json::json!({"errors": [{"code": 1411,
            "message": "Custom hostnames ending in example.com, example.net, or example.org are prohibited."}]});
        match CustomHostnames::refused(reqwest::StatusCode::BAD_REQUEST, &prohibited) {
            CfError::Rejected(reason) => assert!(reason.contains("example.com"), "{reason}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
        let misrouted = serde_json::json!({"errors": [{"code": 7003,
            "message": "Could not route to /zones/nope/custom_hostnames, perhaps your object identifier is invalid?"}]});
        assert!(matches!(
            CustomHostnames::refused(reqwest::StatusCode::BAD_REQUEST, &misrouted),
            CfError::Api(_)
        ));
    }

    fn purge(base_url: &str) -> CachePurge {
        CachePurge::new("zone-1".to_string(), "token-1".to_string())
            .with_api_base(base_url.to_string())
    }

    #[tokio::test]
    async fn purge_posts_hosts_to_the_zone_endpoint() {
        let fake = FakePurge::start().await;
        purge_host_cache(Some(&purge(&fake.base_url)), "groceries.example.com").await;
        assert_eq!(fake.wait_for(1, std::time::Duration::from_secs(5)).await, 1);
        let recorded = fake.requests.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].path, "/zones/zone-1/purge_cache",
            "purge hits the zone endpoint"
        );
        assert_eq!(recorded[0].bearer.as_deref(), Some("token-1"));
        assert_eq!(
            recorded[0].body,
            serde_json::json!({"hosts": ["groceries.example.com"]})
        );
    }

    #[tokio::test]
    async fn no_zone_id_sends_no_request() {
        let fake = FakePurge::start().await;
        purge_host_cache(None, "groceries.example.com").await;
        // A settle wait: a stray spawn would have fired long before this.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(fake.calls(), 0, "None sends nothing");
    }
}

/// Test-only Cloudflare-for-SaaS stand-in on 127.0.0.1 (spec §7.7 flows
/// without the network): `POST /zones/{zone}/custom_hostnames` creates a
/// `pending` hostname (the POST omits validation records, per the docs);
/// `GET` reports the programmed `(status, ssl_status)` plus TXT records;
/// `DELETE` succeeds. Tests program `status` and `fail_with`, then assert
/// on `calls`.
#[cfg(test)]
pub(crate) mod fake_saas {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    pub(crate) struct FakeSaaS {
        pub base_url: String,
        pub state: Arc<Mutex<FakeState>>,
        task: tokio::task::JoinHandle<()>,
    }

    #[derive(Default)]
    pub(crate) struct FakeState {
        /// What GET reports per custom-hostname id.
        pub status: HashMap<String, (String, String)>,
        /// Next-call failures per method: `{"POST": (409, body)}`.
        pub fail_with: HashMap<String, (u16, serde_json::Value)>,
        pub calls: Vec<(String, String, serde_json::Value)>,
    }

    impl Drop for FakeSaaS {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakeSaaS {
        pub(crate) async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = Arc::new(Mutex::new(FakeState::default()));
            let task = tokio::spawn(serve(listener, state.clone()));
            Self {
                base_url: format!("http://127.0.0.1:{port}"),
                state,
                task,
            }
        }

        pub(crate) fn cf(&self) -> super::CustomHostnames {
            super::CustomHostnames::new("zone-1".to_string(), "token-1".to_string())
                .with_api_base(self.base_url.clone())
        }

        pub(crate) fn calls(&self, method: &str) -> Vec<serde_json::Value> {
            self.state
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|(m, _, _)| m == method)
                .map(|(_, _, b)| b.clone())
                .collect()
        }
    }

    async fn serve(listener: tokio::net::TcpListener, state: Arc<Mutex<FakeState>>) {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                return;
            };
            let state = state.clone();
            tokio::spawn(async move {
                handle_conn(conn, &state).await;
            });
        }
    }

    async fn handle_conn(conn: tokio::net::TcpStream, state: &Arc<Mutex<FakeState>>) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        let mut lines = tokio::io::BufReader::new(conn);
        let mut request_line = String::new();
        if lines.read_line(&mut request_line).await.is_err() {
            return;
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            if lines.read_line(&mut line).await.is_err() {
                return;
            }
            if line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 && lines.read_exact(&mut body).await.is_err() {
            return;
        }
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let (code, payload) = {
            let mut s = state.lock().unwrap();
            s.calls.push((method.clone(), path.clone(), parsed.clone()));
            if let Some((code, body)) = s.fail_with.remove(&method) {
                (code, body)
            } else {
                route(&method, &path, &parsed, &s)
            }
        };
        let payload = payload.to_string();
        let reason = if code == 200 { "OK" } else { "Error" };
        let response = format!(
            "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len(),
        );
        let mut conn = lines.into_inner();
        let _ = conn.write_all(response.as_bytes()).await;
    }

    fn route(
        method: &str,
        path: &str,
        body: &serde_json::Value,
        s: &FakeState,
    ) -> (u16, serde_json::Value) {
        let ok = |result: serde_json::Value| {
            (
                200,
                serde_json::json!({"success": true, "errors": [], "result": result}),
            )
        };
        if method == "POST" && path == "/zones/zone-1/custom_hostnames" {
            let hostname = body.get("hostname").and_then(|v| v.as_str()).unwrap_or("");
            // The POST omits validation records (per the docs): only the
            // follow-up GET carries them.
            return ok(serde_json::json!({
                "id": format!("cf-{hostname}"),
                "hostname": hostname,
                "status": "pending",
                "ssl": {"status": "pending", "method": "http"},
            }));
        }
        if let Some(id) = path.strip_prefix("/zones/zone-1/custom_hostnames/") {
            if method == "GET" {
                let (status, ssl) = s
                    .status
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| ("pending".to_string(), "pending".to_string()));
                return ok(serde_json::json!({
                    "id": id,
                    "hostname": id.strip_prefix("cf-").unwrap_or(id),
                    "status": status,
                    "ssl": {
                        "status": ssl,
                        "method": "http",
                        "validation_records": [{
                            "type": "txt",
                            "txt_name": "_acme-challenge.x",
                            "txt_value": "dcv-token",
                        }],
                    },
                    "ownership_verification": {
                        "type": "txt",
                        "name": "_cf-custom-hostname.x",
                        "value": "ownership-token",
                    },
                }));
            }
            if method == "DELETE" {
                return ok(serde_json::json!({"id": id}));
            }
        }
        (
            404,
            serde_json::json!({"success": false, "errors": [{"code": 7003, "message": "unknown"}]}),
        )
    }
}
