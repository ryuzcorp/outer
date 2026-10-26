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
/// while `STRIPE_LAUNCH_COUPON` is set. Pure so tests pin the exact params
/// against a fake Stripe instead of the network.
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

/// Stripe API calls with the secret key. `api_base` is
/// `https://api.stripe.com` in production and a test double in tests.
#[derive(Clone)]
pub struct StripeClient {
    http: reqwest::Client,
    secret_key: String,
    price_pro: String,
    launch_coupon: Option<String>,
    api_base: String,
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
        })
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

    /// One Pro subscription checkout for `account_id` (spec §14.2). The
    /// human finishes on Stripe's page and the webhook upgrades the account;
    /// nothing here touches the `user` row.
    pub async fn create_checkout_session(
        &self,
        account_id: &str,
        email: &str,
        success_url: &str,
    ) -> anyhow::Result<CheckoutSession> {
        let form = checkout_form(
            &self.price_pro,
            account_id,
            email,
            success_url,
            self.launch_coupon.as_deref(),
        );
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

    struct FakeStripe {
        base: String,
        calls: std::sync::Arc<std::sync::Mutex<Vec<RecordedCall>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeStripe {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakeStripe {
        async fn start() -> Self {
            use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = calls.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((conn, _)) = listener.accept().await else {
                        return;
                    };
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        let mut lines = tokio::io::BufReader::new(conn);
                        let mut request_line = String::new();
                        if lines.read_line(&mut request_line).await.is_err() {
                            return;
                        }
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or_default()
                            .to_string();
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
