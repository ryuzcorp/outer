//! Stripe REST calls and webhook signature verification (spec §14.2).
//!
//! Raw `reqwest`, form-encoded, no SDK — the same posture as the Cloudflare
//! API calls. Docs: <https://docs.stripe.com/api/checkout/sessions/create>
//! and <https://docs.stripe.com/webhooks/signature>. The pinned API version
//! rides on every call as `Stripe-Version`.

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};

/// Pinned Stripe API version ([changelog](https://docs.stripe.com/changelog)).
/// Newer versions can rename or drop fields, so every call names the one
/// this code was written against instead of drifting with the account default.
pub const STRIPE_API_VERSION: &str = "2025-08-27.basil";

/// How far a webhook's `t=` may drift from now (spec §14.2: 5 minutes).
pub const SIGNATURE_TOLERANCE_SECS: i64 = 300;

/// What creating a Checkout Session hands back: the URL the human opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSession {
    pub id: String,
    pub url: String,
}

/// Percent-encode one `application/x-www-form-urlencoded` value: UTF-8
/// bytes, `A-Za-z0-9*-._` pass through, space becomes `+`, the rest `%XX`
/// uppercase. Only what the checkout/portal forms need — not a general URL
/// encoder.
pub fn form_encode(raw: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789*-._";
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        if UNRESERVED.contains(byte) {
            out.push(*byte as char);
        } else if *byte == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The `POST /v1/checkout/sessions` body (spec §14.2): subscription mode,
/// the Pro price, the account as client reference and metadata, the payer's
/// email, Stripe Tax on, billing address required, and the launch coupon
/// when one is passed (only while the promo runs in Stripe). Pure so tests
/// pin the exact params against a fake Stripe instead of the network.
pub fn checkout_form(
    price: &str,
    account_id: &str,
    email: &str,
    success_url: &str,
    coupon: Option<&str>,
) -> String {
    let mut pairs = vec![
        ("mode", "subscription".to_string()),
        ("line_items[0][price]", price.to_string()),
        ("line_items[0][quantity]", "1".to_string()),
        ("client_reference_id", account_id.to_string()),
        ("metadata[account_id]", account_id.to_string()),
        ("customer_email", email.to_string()),
        ("success_url", success_url.to_string()),
        ("automatic_tax[enabled]", "true".to_string()),
        ("billing_address_collection", "required".to_string()),
    ];
    if let Some(coupon) = coupon {
        pairs.push(("discounts[0][coupon]", coupon.to_string()));
    }
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", form_encode(key), form_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The `POST /v1/billing_portal/sessions` body: the customer plus where
/// Stripe returns them afterwards. Pure for the same reason as above.
pub fn portal_form(customer_id: &str, return_url: &str) -> String {
    format!(
        "customer={}&return_url={}",
        form_encode(customer_id),
        form_encode(return_url)
    )
}

/// Pull `{id, url}` out of a Checkout or Portal Session response. Stripe
/// answers more fields; only these two matter here.
pub fn session_url(body: &str) -> anyhow::Result<CheckoutSession> {
    let value: serde_json::Value = serde_json::from_str(body)?;
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("stripe session response has no id"))?;
    let url = value
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("stripe session response has no url"))?;
    Ok(CheckoutSession {
        id: id.to_string(),
        url: url.to_string(),
    })
}

/// How long a coupon lookup stays cached inside the client (spec §14.2):
/// the promo switch reads Stripe, not config, but a checkout must never
/// wait on Stripe twice in a row for the same answer.
const PROMO_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// The last coupon lookup: whether the launch promo was live, and when
/// Stripe said so. Lives inside [`StripeClient`] (shared through its
/// `Arc`), never global.
#[derive(Debug, Clone, Copy)]
struct PromoCache {
    active: bool,
    at: Option<std::time::Instant>,
}

/// Stripe API calls with the secret key. `api_base` is
/// `https://api.stripe.com` in production and a test double in tests.
#[derive(Clone)]
pub struct StripeClient {
    http: reqwest::Client,
    secret_key: String,
    price_pro: String,
    launch_coupon: Option<String>,
    api_base: String,
    promo: std::sync::Arc<std::sync::Mutex<PromoCache>>,
}

impl std::fmt::Debug for StripeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeClient")
            .field("price_pro", &self.price_pro)
            .field("secret_key", &"<redacted>")
            .field("launch_coupon", &self.launch_coupon)
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl StripeClient {
    pub fn new(
        secret_key: String,
        price_pro: String,
        launch_coupon: Option<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            secret_key,
            price_pro,
            launch_coupon,
            api_base: "https://api.stripe.com".to_string(),
            promo: std::sync::Arc::new(std::sync::Mutex::new(PromoCache {
                active: false,
                at: None,
            })),
        })
    }

    /// A sandbox key (`sk_test_…`): Checkout takes Stripe's test cards
    /// and nothing is charged.
    pub fn is_test_mode(&self) -> bool {
        self.secret_key.starts_with("sk_test_")
    }

    /// Tests point the client at a local double instead of Stripe.
    #[cfg(test)]
    pub fn with_base(mut self, base: String) -> Self {
        self.api_base = base;
        self
    }

    fn auth(&self) -> String {
        let credentials = format!("{}:", self.secret_key);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials)
        )
    }

    async fn post(&self, path: &str, form: &str) -> anyhow::Result<String> {
        let res = self
            .http
            .post(format!("{}{path}", self.api_base))
            .header("Authorization", self.auth())
            .header("Stripe-Version", STRIPE_API_VERSION)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form.to_string())
            .send()
            .await?;
        let status = res.status();
        let body = res.text().await?;
        if !status.is_success() {
            return Err(anyhow::anyhow!("stripe {path} answered {status}: {body}"));
        }
        Ok(body)
    }

    /// The current subscription object, `GET /v1/subscriptions/{id}` (docs:
    /// <https://docs.stripe.com/api/subscriptions/retrieve>). Webhook
    /// events can arrive out of order, so the route refetches the object
    /// for `customer.subscription.created/updated` and applies that instead
    /// of the (possibly stale) event payload. Same auth and `Stripe-Version`
    /// as the `post` calls above.
    pub async fn retrieve_subscription(&self, id: &str) -> anyhow::Result<serde_json::Value> {
        let res = self
            .http
            .get(format!("{}/v1/subscriptions/{id}", self.api_base))
            .header("Authorization", self.auth())
            .header("Stripe-Version", STRIPE_API_VERSION)
            .send()
            .await?;
        let status = res.status();
        let body = res.text().await?;
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "stripe /v1/subscriptions/{id} answered {status}: {body}"
            ));
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// Whether the launch promo is live in Stripe right now: a coupon id is
    /// configured and `GET /v1/coupons/{id}` says `valid: true` (docs:
    /// <https://docs.stripe.com/api/coupons/retrieve>). The owner ends the
    /// promo in Stripe (delete or expire the coupon); this read is the only
    /// switch. A 404 or `valid: false` means the promo is over, and any
    /// other failure (network, 5xx, unparseable body) also reads as over
    /// with a `warn`: a promo must never block a sale. Lookups cache for
    /// [`PROMO_CACHE_TTL`] inside the client, never globally.
    pub async fn launch_discount_active(&self) -> bool {
        let id = match self.launch_coupon.as_deref() {
            Some(id) => id.to_string(),
            None => return false,
        };
        if let Ok(cache) = self.promo.lock() {
            if let Some(at) = cache.at {
                if at.elapsed() < PROMO_CACHE_TTL {
                    return cache.active;
                }
            }
        }
        let active = self.coupon_is_valid(&id).await;
        if let Ok(mut cache) = self.promo.lock() {
            *cache = PromoCache {
                active,
                at: Some(std::time::Instant::now()),
            };
        }
        active
    }

    /// One coupon read behind [`Self::launch_discount_active`]: true only
    /// on a 200 whose body says `valid: true`. Never errors — every failure
    /// mode logs and reads as "promo over".
    async fn coupon_is_valid(&self, id: &str) -> bool {
        let res = self
            .http
            .get(format!("{}/v1/coupons/{id}", self.api_base))
            .header("Authorization", self.auth())
            .header("Stripe-Version", STRIPE_API_VERSION)
            .send()
            .await;
        let res = match res {
            Ok(res) => res,
            Err(err) => {
                tracing::warn!(
                    "stripe coupon lookup failed: {err:#}; checkout proceeds at full price"
                );
                return false;
            }
        };
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND {
            // Deleted in the dashboard: the promo is over, quietly.
            return false;
        }
        if !status.is_success() {
            tracing::warn!(
                "stripe coupon lookup answered {status}: {body}; checkout proceeds at full price"
            );
            return false;
        }
        match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(value) => value
                .get("valid")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            Err(err) => {
                tracing::warn!(
                    "stripe coupon lookup unparseable: {err:#}; checkout proceeds at full price"
                );
                false
            }
        }
    }

    /// One Pro subscription checkout for `account_id` (spec §14.2). The
    /// human finishes on Stripe's page and the webhook upgrades the account;
    /// nothing here touches the `user` row. The launch coupon rides along
    /// only while [`Self::launch_discount_active`] says the promo runs.
    pub async fn create_checkout_session(
        &self,
        account_id: &str,
        email: &str,
        success_url: &str,
    ) -> anyhow::Result<CheckoutSession> {
        let coupon = if self.launch_discount_active().await {
            self.launch_coupon.as_deref()
        } else {
            None
        };
        let form = checkout_form(&self.price_pro, account_id, email, success_url, coupon);
        session_url(&self.post("/v1/checkout/sessions", &form).await?)
    }
    /// One Customer Portal session for an existing subscriber.
    pub async fn create_portal_session(
        &self,
        customer_id: &str,
        return_url: &str,
    ) -> anyhow::Result<CheckoutSession> {
        let form = portal_form(customer_id, return_url);
        session_url(&self.post("/v1/billing_portal/sessions", &form).await?)
    }
}

/// Why a webhook call is refused before it reaches the event table.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    #[error("missing Stripe-Signature header")]
    Missing,
    #[error("malformed Stripe-Signature header")]
    Malformed,
    #[error("webhook timestamp is outside the 5-minute tolerance")]
    Stale,
    #[error("no v1 signature matches")]
    Mismatch,
}

/// Check `Stripe-Signature` over the raw body (spec §14.2): HMAC-SHA256 with
/// the webhook secret over `t.payload`, 5-minute tolerance, constant-time
/// compare, every `v1=` in the header tried. `header` is the raw header
/// value, `payload` the exact request bytes, `now_secs` unix time (a
/// parameter so tests pin expiry without sleeping).
pub fn verify_signature(
    secret: &str,
    header: Option<&str>,
    payload: &[u8],
    now_secs: i64,
) -> Result<(), SignatureError> {
    let header = header.ok_or(SignatureError::Missing)?;
    let mut timestamp: Option<i64> = None;
    let mut signatures: Vec<Vec<u8>> = Vec::new();
    for part in header.split(',') {
        let (key, value) = part.split_once('=').ok_or(SignatureError::Malformed)?;
        match key.trim() {
            "t" => {
                timestamp = Some(
                    value
                        .trim()
                        .parse()
                        .map_err(|_| SignatureError::Malformed)?,
                );
            }
            "v1" => {
                signatures.push(hex::decode(value.trim()).map_err(|_| SignatureError::Malformed)?);
            }
            _ => {}
        }
    }
    let timestamp = timestamp.ok_or(SignatureError::Malformed)?;
    if signatures.is_empty() {
        return Err(SignatureError::Malformed);
    }
    if (now_secs - timestamp).abs() > SIGNATURE_TOLERANCE_SECS {
        return Err(SignatureError::Stale);
    }
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("hmac takes any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    let expected = mac.finalize().into_bytes();
    if signatures
        .iter()
        .any(|sig| constant_time_eq::constant_time_eq(sig, expected.as_slice()))
    {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

/// Sign a test header: `t=…,v1=…` for `payload` at `now_secs`.
#[cfg(test)]
pub fn sign_test_header(secret: &str, payload: &[u8], now_secs: i64) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(now_secs.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    format!(
        "t={now_secs},v1={}",
        hex::encode(mac.finalize().into_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_form_pins_the_spec_params() {
        let form = checkout_form(
            "price_pro",
            "acct-1",
            "me@example.com",
            "https://outer.now/billing/done",
            Some("launch50"),
        );
        for param in [
            "mode=subscription",
            "line_items%5B0%5D%5Bprice%5D=price_pro",
            "line_items%5B0%5D%5Bquantity%5D=1",
            "client_reference_id=acct-1",
            "metadata%5Baccount_id%5D=acct-1",
            "customer_email=me%40example.com",
            "success_url=https%3A%2F%2Fouter.now%2Fbilling%2Fdone",
            "automatic_tax%5Benabled%5D=true",
            "billing_address_collection=required",
            "discounts%5B0%5D%5Bcoupon%5D=launch50",
        ] {
            assert!(form.contains(param), "missing {param} in {form}");
        }
    }

    #[test]
    fn checkout_form_omits_the_coupon_when_the_promo_is_over() {
        let form = checkout_form(
            "price_pro",
            "acct-1",
            "me@example.com",
            "https://outer.now/billing/done",
            None,
        );
        assert!(!form.contains("coupon"), "unexpected coupon in {form}");
        assert!(form.contains("automatic_tax%5Benabled%5D=true"), "{form}");
    }

    #[test]
    fn portal_form_carries_customer_and_return() {
        let form = portal_form("cus_1", "https://outer.now/console");
        assert_eq!(
            form,
            "customer=cus_1&return_url=https%3A%2F%2Fouter.now%2Fconsole"
        );
    }

    #[test]
    fn session_url_parses_id_and_url() {
        let session = session_url(
            r#"{"id":"cs_test_1","object":"checkout.session","url":"https://checkout.stripe.com/pay/cs_test_1"}"#,
        )
        .unwrap();
        assert_eq!(session.id, "cs_test_1");
        assert_eq!(session.url, "https://checkout.stripe.com/pay/cs_test_1");
    }

    #[test]
    fn signature_round_trip_verifies() {
        let payload = br#"{"id":"evt_1","type":"invoice.paid"}"#;
        let header = sign_test_header("whsec_test", payload, 1_700_000_000);
        verify_signature("whsec_test", Some(&header), payload, 1_700_000_000).unwrap();
    }

    #[test]
    fn signature_rejects_a_wrong_secret() {
        let payload = br#"{"id":"evt_1"}"#;
        let header = sign_test_header("whsec_right", payload, 1_700_000_000);
        assert_eq!(
            verify_signature("whsec_wrong", Some(&header), payload, 1_700_000_000),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn signature_rejects_an_expired_timestamp() {
        let payload = br#"{"id":"evt_1"}"#;
        let header = sign_test_header("whsec_test", payload, 1_700_000_000);
        assert_eq!(
            verify_signature(
                "whsec_test",
                Some(&header),
                payload,
                1_700_000_000 + SIGNATURE_TOLERANCE_SECS + 1
            ),
            Err(SignatureError::Stale)
        );
        // One second inside the tolerance still passes.
        verify_signature(
            "whsec_test",
            Some(&header),
            payload,
            1_700_000_000 + SIGNATURE_TOLERANCE_SECS,
        )
        .unwrap();
    }

    #[test]
    fn signature_accepts_any_matching_v1() {
        let payload = br#"{"id":"evt_1"}"#;
        let good = sign_test_header("whsec_test", payload, 1_700_000_000);
        let good_sig = good.split_once("v1=").unwrap().1;
        let header = format!("t=1700000000,v1={},v1={good_sig}", "0".repeat(64));
        verify_signature("whsec_test", Some(&header), payload, 1_700_000_000).unwrap();
    }

    #[test]
    fn signature_rejects_garbage() {
        assert_eq!(
            verify_signature("s", None, b"{}", 0),
            Err(SignatureError::Missing)
        );
        assert_eq!(
            verify_signature("s", Some("not-a-signature"), b"{}", 0),
            Err(SignatureError::Malformed)
        );
        assert_eq!(
            verify_signature("s", Some("t=abc,v1=00"), b"{}", 0),
            Err(SignatureError::Malformed)
        );
        assert_eq!(
            verify_signature("s", Some("t=1700000000"), b"{}", 1_700_000_000),
            Err(SignatureError::Malformed)
        );
    }

    /// A loopback double for `api.stripe.com`: records the request line,
    /// auth/version headers and raw form body, then answers the canned
    /// session JSON. Same posture as the Cloudflare `FakePurge`.
    struct RecordedCall {
        path: String,
        auth: Option<String>,
        version: Option<String>,
        body: String,
    }

    /// What the fake's `GET /v1/coupons/{id}` answers: the promo states the
    /// lookup must distinguish (live, deleted, expired, Stripe outage).
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CouponReply {
        Valid,
        Deleted,
        Invalid,
        Broken,
    }

    struct FakeStripe {
        base: String,
        calls: std::sync::Arc<std::sync::Mutex<Vec<RecordedCall>>>,
        coupon_calls: std::sync::Arc<std::sync::Mutex<Vec<RecordedCall>>>,
        coupon_hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeStripe {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakeStripe {
        async fn start() -> Self {
            Self::start_with(CouponReply::Valid).await
        }

        async fn start_with(coupon: CouponReply) -> Self {
            use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let coupon_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = calls.clone();
            let seen_coupons = coupon_calls.clone();
            let seen_hits = hits.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((conn, _)) = listener.accept().await else {
                        return;
                    };
                    let seen = seen.clone();
                    let seen_coupons = seen_coupons.clone();
                    let seen_hits = seen_hits.clone();
                    tokio::spawn(async move {
                        let mut lines = tokio::io::BufReader::new(conn);
                        let mut request_line = String::new();
                        if lines.read_line(&mut request_line).await.is_err() {
                            return;
                        }
                        let mut parts = request_line.split_whitespace();
                        let method = parts.next().unwrap_or_default().to_string();
                        let path = parts.next().unwrap_or_default().to_string();
                        let mut auth = None;
                        let mut version = None;
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
                                match name.trim().to_ascii_lowercase().as_str() {
                                    "authorization" => auth = Some(value.trim().to_string()),
                                    "stripe-version" => version = Some(value.trim().to_string()),
                                    "content-length" => {
                                        content_length = value.trim().parse().unwrap_or(0)
                                    }
                                    _ => {}
                                }
                            }
                        }
                        let mut body = vec![0u8; content_length];
                        if lines.read_exact(&mut body).await.is_err() {
                            return;
                        }
                        // The promo switch reads here; checkout POSTs record below.
                        if method == "GET" && path.starts_with("/v1/coupons/") {
                            seen_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            seen_coupons.lock().unwrap().push(RecordedCall {
                                path,
                                auth,
                                version,
                                body: String::new(),
                            });
                            let (status, payload) = match coupon {
                                CouponReply::Valid => ("HTTP/1.1 200 OK", "{\"id\":\"launch50\",\"object\":\"coupon\",\"valid\":true,\"percent_off\":50,\"duration\":\"forever\"}".to_string()),
                                CouponReply::Deleted => ("HTTP/1.1 404 Not Found", "{\"error\":{\"message\":\"No such coupon: 'launch50'\",\"type\":\"invalid_request_error\"}}".to_string()),
                                CouponReply::Invalid => ("HTTP/1.1 200 OK", "{\"id\":\"launch50\",\"object\":\"coupon\",\"valid\":false,\"percent_off\":50,\"duration\":\"forever\"}".to_string()),
                                CouponReply::Broken => ("HTTP/1.1 500 Internal Server Error", "{\"error\":{\"message\":\"boom\"}}".to_string()),
                            };
                            let response = format!("{status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}", payload.len());
                            let mut stream = lines.into_inner();
                            let _ = stream.write_all(response.as_bytes()).await;
                            return;
                        }
                        seen.lock().unwrap().push(RecordedCall {
                            path,
                            auth,
                            version,
                            body: String::from_utf8_lossy(&body).into_owned(),
                        });
                        let session = "{\"id\":\"cs_test_1\",\"object\":\"checkout.session\",\"url\":\"https://checkout.stripe.com/pay/cs_test_1\"}";
                        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{session}", session.len());
                        let mut stream = lines.into_inner();
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                calls,
                coupon_calls,
                coupon_hits: hits,
                task,
            }
        }

        fn only(&self) -> RecordedCall {
            let mut calls = self.calls.lock().unwrap();
            assert_eq!(
                calls.len(),
                1,
                "expected one Stripe call, got {}",
                calls.len()
            );
            calls.pop().unwrap()
        }

        fn coupon_lookups(&self) -> usize {
            self.coupon_hits.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn coupon_only(&self) -> RecordedCall {
            let mut calls = self.coupon_calls.lock().unwrap();
            assert_eq!(
                calls.len(),
                1,
                "expected one coupon lookup, got {}",
                calls.len()
            );
            calls.pop().unwrap()
        }
    }

    fn test_client(base: String, coupon: Option<&str>) -> StripeClient {
        StripeClient::new(
            "sk_test_1".to_string(),
            "price_pro".to_string(),
            coupon.map(str::to_string),
        )
        .unwrap()
        .with_base(base)
    }

    #[tokio::test]
    async fn checkout_posts_subscription_tax_and_coupon() {
        let fake = FakeStripe::start().await;
        let client = test_client(fake.base.clone(), Some("launch50"));
        let session = client
            .create_checkout_session("acct-1", "me@example.com", "https://outer.now/billing/done")
            .await
            .unwrap();
        assert_eq!(session.url, "https://checkout.stripe.com/pay/cs_test_1");
        let call = fake.only();
        assert_eq!(call.path, "/v1/checkout/sessions");
        // Secret key as HTTP Basic, pinned API version on every call.
        let expected_auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("sk_test_1:")
        );
        assert_eq!(call.auth.as_deref(), Some(expected_auth.as_str()));
        assert_eq!(call.version.as_deref(), Some(STRIPE_API_VERSION));
        // Spec §14.2 params: subscription, tax, address, coupon, account.
        for param in [
            "mode=subscription",
            "line_items%5B0%5D%5Bprice%5D=price_pro",
            "automatic_tax%5Benabled%5D=true",
            "billing_address_collection=required",
            "discounts%5B0%5D%5Bcoupon%5D=launch50",
            "client_reference_id=acct-1",
            "metadata%5Baccount_id%5D=acct-1",
            "customer_email=me%40example.com",
        ] {
            assert!(
                call.body.contains(param),
                "missing {param} in {}",
                call.body
            );
        }
    }

    #[tokio::test]
    async fn checkout_omits_the_coupon_when_the_promo_is_over() {
        let fake = FakeStripe::start().await;
        let client = test_client(fake.base.clone(), None);
        client
            .create_checkout_session("acct-1", "me@example.com", "https://outer.now/billing/done")
            .await
            .unwrap();
        let call = fake.only();
        assert!(
            !call.body.contains("coupon"),
            "unexpected coupon in {}",
            call.body
        );
        assert!(
            call.body.contains("automatic_tax%5Benabled%5D=true"),
            "{}",
            call.body
        );
    }

    /// A live coupon in Stripe rides on the checkout, and the lookup reuses
    /// the checkout auth and pinned API version.
    #[tokio::test]
    async fn checkout_applies_a_live_coupon() {
        let fake = FakeStripe::start_with(CouponReply::Valid).await;
        let client = test_client(fake.base.clone(), Some("launch50"));
        assert!(client.launch_discount_active().await);
        let session = client
            .create_checkout_session("acct-1", "me@example.com", "https://outer.now/billing/done")
            .await
            .unwrap();
        assert_eq!(session.url, "https://checkout.stripe.com/pay/cs_test_1");
        let lookup = fake.coupon_only();
        assert_eq!(lookup.path, "/v1/coupons/launch50");
        let expected_auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("sk_test_1:")
        );
        assert_eq!(lookup.auth.as_deref(), Some(expected_auth.as_str()));
        assert_eq!(lookup.version.as_deref(), Some(STRIPE_API_VERSION));
        let call = fake.only();
        assert_eq!(call.path, "/v1/checkout/sessions");
        assert!(
            call.body.contains("discounts%5B0%5D%5Bcoupon%5D=launch50"),
            "{}",
            call.body
        );
    }

    /// The owner deleted the coupon in the dashboard: checkout still
    /// succeeds, at full price.
    #[tokio::test]
    async fn checkout_ignores_a_deleted_coupon() {
        let fake = FakeStripe::start_with(CouponReply::Deleted).await;
        let client = test_client(fake.base.clone(), Some("launch50"));
        assert!(!client.launch_discount_active().await);
        let session = client
            .create_checkout_session("acct-1", "me@example.com", "https://outer.now/billing/done")
            .await
            .unwrap();
        assert_eq!(session.url, "https://checkout.stripe.com/pay/cs_test_1");
        let call = fake.only();
        assert!(
            !call.body.contains("coupon"),
            "unexpected coupon in {}",
            call.body
        );
        assert!(
            call.body.contains("automatic_tax%5Benabled%5D=true"),
            "{}",
            call.body
        );
    }

    /// An expired coupon (`valid: false`) ends the promo the same way.
    #[tokio::test]
    async fn checkout_ignores_an_invalid_coupon() {
        let fake = FakeStripe::start_with(CouponReply::Invalid).await;
        let client = test_client(fake.base.clone(), Some("launch50"));
        assert!(!client.launch_discount_active().await);
        client
            .create_checkout_session("acct-1", "me@example.com", "https://outer.now/billing/done")
            .await
            .unwrap();
        let call = fake.only();
        assert!(
            !call.body.contains("coupon"),
            "unexpected coupon in {}",
            call.body
        );
    }

    /// A Stripe 500 on the coupon lookup never blocks the sale: full price,
    /// checkout still minted.
    #[tokio::test]
    async fn checkout_survives_a_coupon_lookup_outage() {
        let fake = FakeStripe::start_with(CouponReply::Broken).await;
        let client = test_client(fake.base.clone(), Some("launch50"));
        assert!(!client.launch_discount_active().await);
        let session = client
            .create_checkout_session("acct-1", "me@example.com", "https://outer.now/billing/done")
            .await
            .unwrap();
        assert_eq!(session.url, "https://checkout.stripe.com/pay/cs_test_1");
        let call = fake.only();
        assert!(
            !call.body.contains("coupon"),
            "unexpected coupon in {}",
            call.body
        );
    }

    /// Back-to-back checkouts share one coupon lookup while the cache is
    /// fresh: the promo switch costs one Stripe read, not one per sale.
    #[tokio::test]
    async fn coupon_lookup_caches_inside_the_client() {
        let fake = FakeStripe::start_with(CouponReply::Valid).await;
        let client = test_client(fake.base.clone(), Some("launch50"));
        for _ in 0..2 {
            client
                .create_checkout_session(
                    "acct-1",
                    "me@example.com",
                    "https://outer.now/billing/done",
                )
                .await
                .unwrap();
        }
        assert_eq!(fake.coupon_lookups(), 1);
    }

    #[tokio::test]
    async fn portal_posts_customer_and_return_url() {
        let fake = FakeStripe::start().await;
        let client = test_client(fake.base.clone(), None);
        client
            .create_portal_session("cus_1", "https://outer.now/console")
            .await
            .unwrap();
        let call = fake.only();
        assert_eq!(call.path, "/v1/billing_portal/sessions");
        assert!(call.body.contains("customer=cus_1"), "{}", call.body);
        assert!(
            call.body
                .contains("return_url=https%3A%2F%2Fouter.now%2Fconsole"),
            "{}",
            call.body
        );
    }

    #[test]
    fn debug_redacts_the_secret_key() {
        let client = StripeClient::new(
            "sk_live_redact_me".to_string(),
            "price_pro".to_string(),
            None,
        )
        .unwrap();
        let shown = format!("{client:?}");
        assert!(!shown.contains("sk_live_redact_me"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }
}
