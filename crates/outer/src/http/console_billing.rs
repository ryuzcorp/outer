//! Billing HTTP surface (spec §4.2, §14.2): the Stripe webhook, the
//! per-account checkout-starter links refusals carry, the upgrade
//! confirmation page, and the console Plan-card endpoints. New endpoints
//! live here (not in `console.rs`); `console.rs` only gains the `.merge`
//! line.
//!
//! Rules (same as `CONSOLE-ACTIONS-CONTRACT.md`): session cookie only for
//! the console API, `Sec-Fetch-Site: same-origin`, account-action audit
//! rows with the MCP tool name and client "Console", `{error, message,
//! user_message?}` errors. The webhook is the exception: Stripe signs it,
//! so it authenticates with `Stripe-Signature` instead of a session.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;

use super::console::{guard, nostore_json, ok_json};
use super::console_actions::tool_response;
use crate::accounts::record_account_action;
use crate::auth::now_ms;
use crate::billing::{stripe, ApplyOutcome, WebhookCtx};
use crate::mcp::error::ToolError;

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/webhooks/stripe", post(api_webhook))
        .route("/billing/checkout", get(billing_checkout))
        .route("/billing/done", get(billing_done))
        .route("/api/console/billing/checkout", post(api_console_checkout))
        .route("/api/console/billing/portal", post(api_console_portal))
}

/// `POST /webhooks/stripe` on the platform host (spec §14.2): verify
/// `Stripe-Signature` (HMAC-SHA256 over `t.payload`, 5-minute tolerance,
/// constant-time compare, every `v1` tried), then record the event
/// idempotently and apply its plan transition. Unknown event types are
/// recorded and ignored. Downgraded apps stop here too: rows plus fleets.
async fn api_webhook(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mcp = &state.mcp;
    let Some(secret) = mcp.billing.webhook_secret() else {
        return nostore_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "billing is not enabled on this host",
        );
    };
    let signature = headers
        .get("stripe-signature")
        .and_then(|value| value.to_str().ok());
    let now_secs = chrono::Utc::now().timestamp();
    if let Err(err) = stripe::verify_signature(secret, signature, &body, now_secs) {
        tracing::warn!("stripe webhook: bad signature: {err}");
        return nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_signature",
            &err.to_string(),
        );
    }
    let event: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(_) => {
            return nostore_json(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                "webhook body is not JSON",
            );
        }
    };
    let (Some(id), Some(kind)) = (
        event.get("id").and_then(serde_json::Value::as_str),
        event.get("type").and_then(serde_json::Value::as_str),
    ) else {
        return nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "webhook event has no id or type",
        );
    };
    let object = event
        .get("data")
        .and_then(|data| data.get("object"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let ctx = WebhookCtx {
        pool: &mcp.pool,
        sender: &mcp.auth.config.email,
        billing: &mcp.billing,
    };
    match crate::billing::apply_event(&ctx, id, kind, &object, now_secs).await {
        Err(err) => {
            tracing::warn!(event = %id, kind = %kind, "stripe webhook: apply failed: {err:#}");
            nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not apply the event right now",
            )
        }
        Ok(ApplyOutcome::Duplicate) => ok_json(json!({"received": true, "duplicate": true})),
        Ok(ApplyOutcome::Applied {
            account_id,
            stopped,
        }) => {
            for slug in &stopped {
                mcp.fleets.stop_app(slug).await;
            }
            tracing::info!(event = %id, kind = %kind, account = ?account_id, stopped = ?stopped, "stripe webhook applied");
            ok_json(json!({"received": true}))
        }
    }
}

#[derive(Debug, Deserialize)]
struct CheckoutQuery {
    t: Option<String>,
}

/// `GET /billing/checkout?t=`: the link every refusal's `upgrade_url`
/// carries. Verifies the signed token, mints a fresh Checkout Session for
/// that account, and redirects to Stripe. Fresh per click, so the link
/// never goes stale the way a stored `checkout.stripe.com` URL would.
async fn billing_checkout(
    State(state): State<super::AppState>,
    Query(query): Query<CheckoutQuery>,
) -> Response {
    let now_secs = chrono::Utc::now().timestamp();
    let Some(token) = query.t.filter(|token| !token.is_empty()) else {
        return text(
            StatusCode::BAD_REQUEST,
            "This checkout link is missing. Ask your agent for a fresh upgrade link.",
        );
    };
    let Some(account_id) = state.mcp.billing.verify_checkout_token(&token, now_secs) else {
        return text(
            StatusCode::BAD_REQUEST,
            "This checkout link is bad or expired. Ask your agent for a fresh upgrade link.",
        );
    };
    let email: Option<String> = sqlx::query_scalar("SELECT email FROM user WHERE id = ?1")
        .bind(&account_id)
        .fetch_optional(&state.mcp.pool)
        .await
        .unwrap_or(None);
    let Some(email) = email else {
        return text(
            StatusCode::NOT_FOUND,
            "This account is gone. Ask your agent to sign you in again.",
        );
    };
    match state.mcp.billing.checkout_url(&account_id, &email).await {
        None => text(
            StatusCode::SERVICE_UNAVAILABLE,
            "Billing is not enabled on this host yet.",
        ),
        Some(Err(err)) => {
            tracing::warn!(account = %account_id, "checkout-starter: Stripe refused: {err:#}");
            text(
                StatusCode::SERVICE_UNAVAILABLE,
                "Checkout is unavailable right now; try again in a minute.",
            )
        }
        Some(Ok(session)) => Redirect::to(&session.url).into_response(),
    }
}

/// `GET /billing/done` (spec §4.2): where Stripe returns the payer. Plain
/// text on purpose: the human's next step is with their agent.
async fn billing_done() -> Response {
    text(
        StatusCode::OK,
        "You're upgraded to Outer Pro — go back to your agent. The new limits apply on its next call.",
    )
}

fn text(status: StatusCode, body: &str) -> Response {
    (
        status,
        [("content-type", "text/plain; charset=utf-8")],
        body.to_string(),
    )
        .into_response()
}

/// `POST /api/console/billing/checkout`: the Plan card's Upgrade button.
/// Same live session as the MCP `upgrade` tool, session-authed instead of
/// token-authed. Audited as `upgrade` with client "Console".
async fn api_console_checkout(
    State(state): State<super::AppState>,
    headers: HeaderMap,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let started = now_ms();
    match mcp.billing.checkout_url(&account.id, &account.email).await {
        None => {
            record_account_action(
                &mcp.pool,
                &account.id,
                "upgrade",
                "upgrade",
                "error",
                Some("unavailable"),
                started,
            )
            .await;
            tool_response(&ToolError::internal(
                "billing is not enabled on this host yet; every account stays on Free",
            ))
        }
        Some(Err(err)) => {
            tracing::warn!(account = %account.id, "console checkout: Stripe refused: {err:#}");
            record_account_action(
                &mcp.pool,
                &account.id,
                "upgrade",
                "upgrade",
                "error",
                Some("unavailable"),
                started,
            )
            .await;
            tool_response(&ToolError::internal(
                "checkout is unavailable right now; try again in a minute",
            ))
        }
        Some(Ok(session)) => {
            record_account_action(
                &mcp.pool,
                &account.id,
                "upgrade",
                "upgrade",
                "ok",
                None,
                started,
            )
            .await;
            ok_json(json!({"checkout_url": session.url}))
        }
    }
}

/// `POST /api/console/billing/portal`: the Plan card's Manage billing
/// button. Same live session as the MCP `manage_billing` tool. Audited as
/// `manage_billing` with client "Console".
async fn api_console_portal(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let started = now_ms();
    let customer: Option<String> =
        sqlx::query_scalar("SELECT stripeCustomerId FROM user WHERE id = ?1")
            .bind(&account.id)
            .fetch_optional(&mcp.pool)
            .await
            .unwrap_or(None);
    let Some(customer) = customer else {
        record_account_action(
            &mcp.pool,
            &account.id,
            "manage_billing",
            "manage_billing",
            "error",
            Some("invalid_argument"),
            started,
        )
        .await;
        return tool_response(&ToolError::invalid_argument(
            "no billing history on this account yet; upgrade to Pro first",
        ));
    };
    match mcp.billing.portal_url(&customer).await {
        None => {
            record_account_action(
                &mcp.pool,
                &account.id,
                "manage_billing",
                "manage_billing",
                "error",
                Some("unavailable"),
                started,
            )
            .await;
            tool_response(&ToolError::internal(
                "billing is not enabled on this host yet; every account stays on Free",
            ))
        }
        Some(Err(err)) => {
            tracing::warn!(account = %account.id, "console portal: Stripe refused: {err:#}");
            record_account_action(
                &mcp.pool,
                &account.id,
                "manage_billing",
                "manage_billing",
                "error",
                Some("unavailable"),
                started,
            )
            .await;
            tool_response(&ToolError::internal(
                "the billing portal is unavailable right now; try again in a minute",
            ))
        }
        Some(Ok(session)) => {
            record_account_action(
                &mcp.pool,
                &account.id,
                "manage_billing",
                "manage_billing",
                "ok",
                None,
                started,
            )
            .await;
            ok_json(json!({"portal_url": session.url}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::extract::Request;
    use tower::ServiceExt as _;

    const SECRET: &str = "whsec_test";

    fn enabled_billing(base: String) -> crate::billing::BillingState {
        crate::billing::BillingState::Enabled {
            client: stripe::StripeClient::new(
                "sk_test_1".to_string(),
                "price_pro".to_string(),
                Some("launch50".to_string()),
            )
            .unwrap()
            .with_base(base),
            webhook_secret: SECRET.to_string(),
            platform_base: "https://outer.now".to_string(),
            sign_key: b"test-sign-key".to_vec(),
        }
    }

    async fn app_state(
        billing: crate::billing::BillingState,
    ) -> (
        super::super::AppState,
        SqlitePool,
        Arc<Mutex<HashMap<String, String>>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("webhook.sqlite"))
            .await
            .unwrap();
        std::mem::forget(dir);
        let (config, codes) = crate::auth::test_support::test_config();
        let auth = crate::auth::AuthState {
            pool: pool.clone(),
            config,
        };
        let mcp = crate::mcp::McpState::for_tests(pool.clone(), auth.clone()).with_billing(billing);
        let state = super::super::AppState {
            pool: pool.clone(),
            sync: crate::store::snapshot::StateSync::new("test"),
            auth,
            mcp,
            site: crate::http::site::SiteDir::new(std::path::Path::new("/nonexistent")),
        };
        (state, pool, codes)
    }

    async fn seed_user(pool: &SqlitePool) {
        sqlx::query(
            "INSERT INTO user (id, email, stripeCustomerId, lastActiveAt, createdAt, updatedAt) \
             VALUES ('a1', 'a@example.com', 'cus_1', 0, 0, 0)",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// A loopback double answering one canned Checkout Session, recording
    /// the raw form so the route test pins the spec params end to end.
    struct FakeStripe {
        base: String,
        body: Arc<Mutex<Option<String>>>,
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
            let seen = Arc::new(Mutex::new(None));
            let body = seen.clone();
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
                        let mut raw = vec![0u8; content_length];
                        if lines.read_exact(&mut raw).await.is_err() {
                            return;
                        }
                        *seen.lock().unwrap() = Some(String::from_utf8_lossy(&raw).into_owned());
                        let session = "{\"id\":\"cs_test_9\",\"object\":\"checkout.session\",\"url\":\"https://checkout.stripe.com/pay/cs_test_9\"}";
                        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{session}", session.len());
                        let mut stream = lines.into_inner();
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                body,
                task,
            }
        }
    }

    #[tokio::test]
    async fn webhook_applies_payment_failed_and_mails() {
        let fake = FakeStripe::start().await;
        let (state, pool, codes) = app_state(enabled_billing(fake.base.clone())).await;
        seed_user(&pool).await;
        let payload = r#"{"id":"evt_1","type":"invoice.payment_failed","data":{"object":{"id":"in_1","customer":"cus_1","subscription":"sub_1"}}}"#;
        let post = || {
            let fresh = stripe::sign_test_header(
                SECRET,
                payload.as_bytes(),
                chrono::Utc::now().timestamp(),
            );
            Request::post("/webhooks/stripe")
                .header("stripe-signature", fresh)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap()
        };
        let res = super::super::router(state.clone())
            .oneshot(post())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let (plan, status): (String, Option<String>) =
            sqlx::query_as("SELECT plan, planStatus FROM user WHERE id = 'a1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(plan, "pro");
        assert_eq!(status.as_deref(), Some("past_due"));
        assert!(codes.lock().unwrap().get("a@example.com").is_some());
        // A redelivery is a 200 duplicate with no second row.
        let res = super::super::router(state).oneshot(post()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stripe_event WHERE id = 'evt_1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn webhook_rejects_a_bad_signature() {
        let (state, _, _) = app_state(enabled_billing("http://127.0.0.1:1".to_string())).await;
        let req = Request::post("/webhooks/stripe")
            .header("stripe-signature", "t=1700000000,v1=00")
            .body(Body::from("{}"))
            .unwrap();
        let res = super::super::router(state).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn webhook_refuses_cleanly_when_disabled() {
        let billing = crate::billing::BillingState::Disabled {
            platform_base: "https://outer.now".to_string(),
        };
        let (state, _, _) = app_state(billing).await;
        let req = Request::post("/webhooks/stripe")
            .body(Body::from("{}"))
            .unwrap();
        let res = super::super::router(state).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn checkout_starter_redirects_to_a_fresh_session() {
        let fake = FakeStripe::start().await;
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_user(&pool).await;
        let token = crate::billing::sign_checkout_token(
            b"test-sign-key",
            "a1",
            chrono::Utc::now().timestamp() + 60,
        );
        let req = Request::get(format!("/billing/checkout?t={token}"))
            .body(Body::empty())
            .unwrap();
        let res = super::super::router(state).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let location = res
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(location, "https://checkout.stripe.com/pay/cs_test_9");
        // The click minted a subscription checkout with tax and the coupon.
        let body = fake.body.lock().unwrap().clone().unwrap();
        assert!(body.contains("mode=subscription"), "{body}");
        assert!(body.contains("automatic_tax%5Benabled%5D=true"), "{body}");
        assert!(
            body.contains("discounts%5B0%5D%5Bcoupon%5D=launch50"),
            "{body}"
        );
        assert!(body.contains("client_reference_id=a1"), "{body}");
    }

    #[tokio::test]
    async fn checkout_starter_rejects_bad_tokens() {
        let (state, _, _) = app_state(enabled_billing("http://127.0.0.1:1".to_string())).await;
        let req = Request::get("/billing/checkout?t=nope")
            .body(Body::empty())
            .unwrap();
        let res = super::super::router(state).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
