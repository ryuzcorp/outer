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
