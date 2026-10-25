//! Look at your app (wave 3 feature 1): render the live app page in a
//! real browser through Cloudflare and return a screenshot.
//!
//! The REST Quick Actions endpoint (`POST
//! `/accounts/{account_id}/browser-rendering/snapshot`, permission
//! `Browser Rendering Write` — renamed `Browser Run - Edit` in the
//! current dashboard) returns a base64 screenshot plus the page's HTTP
//! status and title in one call:
//! <https://developers.cloudflare.com/api/resources/browser_rendering/subresources/snapshot/methods/create/>.
//!
//! What REST cannot return is the gap this module documents: console
//! errors and failed sub-requests need a CDP/log stream, which the
//! Quick Actions REST API does not expose. The tool returns the
//! screenshot, the HTTP status and the title, and points at `app_logs` /
//! `app_errors` for server-side failures. Like the other Cloudflare
//! clients the base URL is overridable so tests point it at a local fake.
//!
//! SSRF guard: callers never supply a URL. The tool builds it from the
//! app's own host (slug host, or its active custom domain) plus a
//! validated path.

use std::time::Duration;

/// `https://api.cloudflare.com/client/v4`, overridable for tests.
const DEFAULT_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Looks one caller may take per UTC hour (task spec: 30/hour).
pub const LOOKS_PER_HOUR: u64 = 30;

/// Largest screenshot the tool returns: JPEG/WebP around a megabyte.
/// Anything bigger is refused so one MCP result stays small.
pub const MAX_IMAGE_BYTES: usize = 1_000_000;

/// Largest Cloudflare response body accepted (base64 inflates the image
/// plus the rendered HTML): 8 MiB.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Whole render budget: the browser navigation gets 15 s inside a 20 s
/// HTTP timeout (task spec: ~20 s).
const RENDER_TIMEOUT: Duration = Duration::from_secs(20);
const GOTO_TIMEOUT_MS: u64 = 15_000;

/// Browser-render credentials: the existing `CLOUDFLARE_ACCOUNT_ID` +
/// API token pair from `config.rs` (token needs `Browser Rendering
/// Write`, shown as `Browser Run - Edit` in the current dashboard).
#[derive(Clone)]
pub struct BrowserRender {
    account_id: String,
    api_token: String,
    api_base: String,
}

impl std::fmt::Debug for BrowserRender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserRender")
            .field("account_id", &self.account_id)
            .field("api_token", &"<redacted>")
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl BrowserRender {
    /// The production renderer: Cloudflare's API with the account and token.
    pub fn new(account_id: String, api_token: String) -> Self {
        Self {
            account_id,
            api_token,
            api_base: DEFAULT_API_BASE.to_string(),
        }
    }

    /// Point the renderer at a fake API base (tests only).
    #[cfg(test)]
    pub fn with_api_base(self, api_base: String) -> Self {
        Self { api_base, ..self }
    }
}

/// One rendered page: the screenshot bytes plus what the browser
/// reported about the navigation.
#[derive(Debug)]
pub struct Snapshot {
    pub image: Vec<u8>,
    /// The navigation's HTTP status, when the API reported one.
    pub http_status: Option<i64>,
    /// The page title, when the API reported one.
    pub title: Option<String>,
}

/// What went wrong rendering the page. Callers map these to
/// agent-facing errors; the raw strings never reach agents.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// HTTP, timeout or JSON: our side or the network broke.
    #[error("render request failed: {0}")]
    Transport(#[from] anyhow::Error),
    /// Cloudflare answered but refused the render (bad page, bad key,
    /// rate limit on their side). Carries a truncated excerpt for logs.
    #[error("browser rendering refused: {0}")]
    Refused(String),
    /// The screenshot decoded but is bigger than [`MAX_IMAGE_BYTES`].
    #[error("screenshot is {0} bytes, over the {1} byte cap")]
    TooLarge(usize, usize),
    /// The screenshot is missing, not base64, or not a JPEG.
    #[error("screenshot is not a usable JPEG")]
    BadImage,
}

/// What went wrong claiming hourly quota. Callers map exhaustion to
/// `rate_limited` and DB failures to `internal`.
#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    #[error("look quota exhausted")]
    Exhausted { used: u64, retry_after_s: u64 },
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The current UTC hour bucket (`YYYY-MM-DDTHH`).
fn hour_bucket() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H").to_string()
}

/// Seconds until the next UTC hour, for `retry_after_s`.
fn secs_until_next_hour() -> u64 {
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    3600 - now % 3600
}

/// Atomically claim one look against the caller's hourly budget: a single
/// UPSERT whose guard refuses the increment past [`LOOKS_PER_HOUR`], so
/// racing calls cannot both pass. Returns looks used including this one.
pub async fn claim_look(pool: &sqlx::SqlitePool, account_id: &str) -> Result<u64, QuotaError> {
    let rows = sqlx::query(
        "INSERT INTO look_hour (userId, hour, looks) VALUES (?1, ?2, 1) \
         ON CONFLICT(userId, hour) DO UPDATE SET looks = looks + 1 \
         WHERE looks < ?3",
    )
    .bind(account_id)
    .bind(hour_bucket())
    .bind(LOOKS_PER_HOUR as i64)
    .execute(pool)
    .await?
    .rows_affected();
    let used: i64 =
        sqlx::query_scalar("SELECT looks FROM look_hour WHERE userId = ?1 AND hour = ?2")
            .bind(account_id)
            .bind(hour_bucket())
            .fetch_optional(pool)
            .await?
            .unwrap_or(0);
    if rows == 0 {
        return Err(QuotaError::Exhausted {
            used: used.max(0) as u64,
            retry_after_s: secs_until_next_hour(),
        });
    }
    Ok(used.max(0) as u64)
}

/// Render `url` in Cloudflare's browser and return the screenshot plus
/// the navigation status/title. One `/snapshot` call with
/// `formats: ["screenshot", "content"]`: JPEG at quality 60 keeps the
/// result under [`MAX_IMAGE_BYTES`] without a second request.
pub async fn snapshot_page(
    render: &BrowserRender,
    url: &str,
    width: u32,
    full_page: bool,
) -> Result<Snapshot, RenderError> {
    let client = reqwest::Client::builder()
        .timeout(RENDER_TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|err| RenderError::Transport(anyhow::anyhow!(err)))?;
    let endpoint = format!(
        "{}/accounts/{}/browser-rendering/snapshot",
        render.api_base, render.account_id
    );
    let response = client
        .post(&endpoint)
        .bearer_auth(&render.api_token)
        .json(&serde_json::json!({
            "url": url,
            "formats": ["screenshot", "content"],
            "viewport": {"width": width, "height": 800},
            "screenshotOptions": {"type": "jpeg", "quality": 60, "fullPage": full_page},
            "gotoOptions": {"waitUntil": "networkidle2", "timeout": GOTO_TIMEOUT_MS},
        }))
        .send()
        .await
        .map_err(|err| RenderError::Transport(anyhow::anyhow!(err)))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|err| RenderError::Transport(anyhow::anyhow!(err)))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(RenderError::Transport(anyhow::anyhow!(
            "browser rendering answered {} bytes, over the {} byte cap",
            body.len(),
            MAX_BODY_BYTES
        )));
    }
    let parsed: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|err| RenderError::Transport(anyhow::anyhow!(err)))?;
    if !status.is_success() || parsed.get("success").and_then(|v| v.as_bool()) != Some(true) {
        let excerpt: String = parsed.to_string().chars().take(300).collect();
        tracing::warn!(%status, excerpt = %excerpt, url = %url, "browser rendering refused");
        return Err(RenderError::Refused(excerpt));
    }
    let encoded = parsed
        .pointer("/result/screenshot")
        .and_then(|v| v.as_str())
        .ok_or(RenderError::BadImage)?;
    let image = parse_jpeg_screenshot(encoded)?;
    if image.len() > MAX_IMAGE_BYTES {
        return Err(RenderError::TooLarge(image.len(), MAX_IMAGE_BYTES));
    }
    Ok(Snapshot {
        image,
        http_status: parsed.pointer("/meta/status").and_then(|v| v.as_i64()),
        title: parsed
            .pointer("/meta/title")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Decode the snapshot's base64 screenshot and check it is a JPEG in
/// the size budget. Magic bytes only — no image decoding on the server.
fn parse_jpeg_screenshot(encoded: &str) -> Result<Vec<u8>, RenderError> {
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| RenderError::BadImage)?;
    if image.len() < 4 || image[0..3] != [0xFF, 0xD8, 0xFF] {
        return Err(RenderError::BadImage);
    }
    Ok(image)
}

#[cfg(test)]
pub(crate) mod fake {
    //! A hand-rolled Browser Rendering stand-in on 127.0.0.1: records
    //! each request body, answers the programmed status + JSON. No
    //! network, following `cloudflare::fake_saas`.
    use std::sync::{Arc, Mutex};

    pub(crate) struct FakeRender {
        pub base_url: String,
        state: Arc<Mutex<FakeState>>,
        task: tokio::task::JoinHandle<()>,
    }

    #[derive(Default)]
    pub(crate) struct FakeState {
        pub status: u16,
        pub body: serde_json::Value,
        pub calls: Vec<serde_json::Value>,
    }

    impl Drop for FakeRender {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakeRender {
        pub(crate) async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = Arc::new(Mutex::new(FakeState {
                status: 200,
                body: serde_json::json!({"success": true}),
                calls: Vec::new(),
            }));
            let task = tokio::spawn(serve(listener, state.clone()));
            Self {
                base_url: format!("http://127.0.0.1:{port}"),
                state,
                task,
            }
        }

        pub(crate) fn render(&self) -> super::BrowserRender {
            super::BrowserRender::new("acct-1".to_string(), "token-1".to_string())
                .with_api_base(self.base_url.clone())
        }

        pub(crate) fn program(&self, status: u16, body: serde_json::Value) {
            let mut state = self.state.lock().unwrap();
            state.status = status;
            state.body = body;
        }

        pub(crate) fn calls(&self) -> Vec<serde_json::Value> {
            self.state.lock().unwrap().calls.clone()
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
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            if lines.read_line(&mut line).await.is_err() {
                return;
            }
            let trimmed = line.trim_end().to_ascii_lowercase();
            if trimmed.is_empty() {
                break;
            }
            if let Some(n) = trimmed.strip_prefix("content-length:") {
                content_length = n.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if lines.read_exact(&mut body).await.is_err() {
            return;
        }
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let (status, reply) = {
            let mut state = state.lock().unwrap();
            state.calls.push(parsed);
            (state.status, state.body.to_string())
        };
        let response = format!(
            "HTTP/1.1 {status} x\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
            reply.len()
        );
        let _ = lines.into_inner().write_all(response.as_bytes()).await;
    }
}

/// A minimal 1x1 JPEG (SOI + JFIF + EOI) for tests.
#[cfg(test)]
pub(crate) fn test_jpeg_bytes() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ]
}

#[cfg(test)]
mod tests {
    use super::fake::FakeRender;
    use super::*;

    fn jpeg_b64(extra: usize) -> String {
        use base64::Engine as _;
        let mut bytes = test_jpeg_bytes();
        bytes.extend(std::iter::repeat_n(0x00, extra));
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    }
    fn b64(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[tokio::test]
    async fn snapshot_parses_screenshot_status_and_title() {
        let fake = FakeRender::start().await;
        fake.program(
            200,
            serde_json::json!({
                "success": true,
                "result": {"screenshot": jpeg_b64(0), "content": "<html></html>"},
                "meta": {"status": 200, "title": "Groceries"},
            }),
        );
        let shot = snapshot_page(&fake.render(), "https://shop.outer.now/", 1280, false)
            .await
            .unwrap();
        assert_eq!(shot.image, test_jpeg_bytes());
        assert_eq!(shot.http_status, Some(200));
        assert_eq!(shot.title.as_deref(), Some("Groceries"));
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["url"], "https://shop.outer.now/");
        assert_eq!(calls[0]["viewport"]["width"], 1280);
        assert_eq!(
            calls[0]["formats"],
            serde_json::json!(["screenshot", "content"])
        );
        assert_eq!(calls[0]["screenshotOptions"]["type"], "jpeg");
    }

    #[tokio::test]
    async fn snapshot_refuses_when_cloudflare_says_no() {
        let fake = FakeRender::start().await;
        fake.program(
            400,
            serde_json::json!({"success": false, "errors": ["bad url"]}),
        );
        let err = snapshot_page(&fake.render(), "https://shop.outer.now/", 390, true)
            .await
            .unwrap_err();
        assert!(matches!(err, RenderError::Refused(_)), "{err:?}");
    }

    #[tokio::test]
    async fn snapshot_rejects_non_jpeg_screenshots() {
        let fake = FakeRender::start().await;
        for screenshot in [
            serde_json::json!("!!!not-base64!!!"),
            serde_json::json!(b64(b"plain text")),
        ] {
            fake.program(
                200,
                serde_json::json!({"success": true, "result": {"screenshot": screenshot}}),
            );
            let err = snapshot_page(&fake.render(), "https://shop.outer.now/", 390, false)
                .await
                .unwrap_err();
            assert!(matches!(err, RenderError::BadImage), "{err:?}");
        }
    }

    #[tokio::test]
    async fn snapshot_rejects_oversize_screenshots() {
        let fake = FakeRender::start().await;
        fake.program(
            200,
            serde_json::json!({"success": true, "result": {"screenshot": jpeg_b64(MAX_IMAGE_BYTES)}}),
        );
        let err = snapshot_page(&fake.render(), "https://shop.outer.now/", 390, false)
            .await
            .unwrap_err();
        assert!(matches!(err, RenderError::TooLarge(_, _)), "{err:?}");
    }

    #[tokio::test]
    async fn quota_claims_up_to_thirty_then_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("quota.sqlite"))
            .await
            .unwrap();
        for want in 1..=LOOKS_PER_HOUR {
            assert_eq!(claim_look(&pool, "acct-a").await.unwrap(), want);
        }
        let err = claim_look(&pool, "acct-a").await.unwrap_err();
        match err {
            QuotaError::Exhausted {
                used,
                retry_after_s,
            } => {
                assert_eq!(used, LOOKS_PER_HOUR);
                assert!((1..=3600).contains(&retry_after_s));
            }
            other => panic!("expected exhaustion, got {other:?}"),
        }
        // A second account has its own budget.
        assert_eq!(claim_look(&pool, "acct-b").await.unwrap(), 1);
    }
}
