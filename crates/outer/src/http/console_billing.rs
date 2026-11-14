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
        .route("/billing/portal", get(billing_portal))
        .route("/billing/done", get(billing_done))
        .route("/api/console/billing/checkout", post(api_console_checkout))
        .route("/api/console/billing/portal", post(api_console_portal))
}

/// One `user` row as the billing starters read it: who pays, which plan,
/// and the Stripe ids that decide between a fresh checkout and the portal.
struct BillingAccount {
    email: String,
    plan: String,
    customer: Option<String>,
    subscription: Option<String>,
}

async fn billing_account(pool: &sqlx::SqlitePool, account_id: &str) -> Option<BillingAccount> {
    let row: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT email, plan, stripeCustomerId, stripeSubscriptionId FROM user WHERE id = ?1",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    row.map(|(email, plan, customer, subscription)| BillingAccount {
        email,
        plan,
        customer,
        subscription,
    })
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
    // Stripe does not guarantee webhook order, so refetch the live object
    // (docs: https://docs.stripe.com/api/subscriptions/retrieve) before
    // `apply_event` opens its transaction (no network inside the DB lock).
    // A fetch failure 500s with nothing recorded, and Stripe retries.
    // `checkout.session.completed` carries no line items, so its session
    // refetches the subscription separately for the O-5 price check.
    let mut object = object;
    let mut checkout_subscription: Option<serde_json::Value> = None;
    if matches!(
        kind,
        "customer.subscription.created" | "customer.subscription.updated"
    ) {
        if let Some(sub_id) = object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        {
            match mcp.billing.retrieve_subscription(&sub_id).await {
                Some(Ok(fresh)) => object = fresh,
                Some(Err(err)) => {
                    tracing::warn!(event = %id, kind = %kind, "stripe webhook: refetch failed: {err:#}");
                    return nostore_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "unavailable",
                        "could not apply the event right now",
                    );
                }
                None => {}
            }
        }
    } else if kind == "checkout.session.completed" {
        if let Some(sub_id) = object
            .get("subscription")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        {
            match mcp.billing.retrieve_subscription(&sub_id).await {
                Some(Ok(fresh)) => checkout_subscription = Some(fresh),
                Some(Err(err)) => {
                    tracing::warn!(event = %id, kind = %kind, "stripe webhook: checkout subscription refetch failed: {err:#}");
                    return nostore_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "unavailable",
                        "could not apply the event right now",
                    );
                }
                None => {}
            }
        }
    }
    let ctx = WebhookCtx {
        pool: &mcp.pool,
        sender: &mcp.auth.config.email,
        billing: &mcp.billing,
        saas: crate::cloudflare::CustomHostnames::from_purge(mcp.delete_cfg.cache_purge.as_ref()),
        purge: mcp.delete_cfg.cache_purge.clone(),
        apps_domain: mcp.delete_cfg.apps_domain.clone(),
        egress: mcp.egress.clone(),
    };
    match crate::billing::apply_event(
        &ctx,
        id,
        kind,
        &object,
        checkout_subscription.as_ref(),
        now_secs,
    )
    .await
    {
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
/// never goes stale the way a stored `checkout.stripe.com` URL would. A
/// stored customer reuses their Stripe customer; an account with a live
/// subscription goes to the Customer Portal instead, so the click can
/// never open the second subscription that double-bills.
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
    let Some(account) = billing_account(&state.mcp.pool, &account_id).await else {
        return text(
            StatusCode::NOT_FOUND,
            "This account is gone. Ask your agent to sign you in again.",
        );
    };
    if state.mcp.billing.enabled()
        && crate::billing::has_live_subscription(&account.plan, account.subscription.as_deref())
    {
        let Some(customer) = account.customer.as_deref() else {
            return text(
                StatusCode::SERVICE_UNAVAILABLE,
                "This account is already on Pro. Ask your agent for a billing link.",
            );
        };
        return match state.mcp.billing.portal_url(customer).await {
            Some(Ok(session)) => Redirect::to(&session.url).into_response(),
            _ => text(
                StatusCode::SERVICE_UNAVAILABLE,
                "The billing portal is unavailable right now; try again in a minute.",
            ),
        };
    }
    match state
        .mcp
        .billing
        .checkout_url(&account_id, &account.email, account.customer.as_deref())
        .await
    {
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

/// `GET /billing/portal?t=`: the link the past-due mails carry. Verifies
/// the signed token, mints a fresh Customer Portal session for that
/// account, and redirects to Stripe. Fresh per click, because portal
/// session URLs are short-lived and a stored one would go stale.
async fn billing_portal(
    State(state): State<super::AppState>,
    Query(query): Query<CheckoutQuery>,
) -> Response {
    let now_secs = chrono::Utc::now().timestamp();
    let Some(token) = query.t.filter(|token| !token.is_empty()) else {
        return text(
            StatusCode::BAD_REQUEST,
            "This portal link is missing. Ask your agent for a fresh billing link.",
        );
    };
    let Some(account_id) = state.mcp.billing.verify_portal_token(&token, now_secs) else {
        return text(
            StatusCode::BAD_REQUEST,
            "This portal link is bad or expired. Ask your agent for a fresh billing link.",
        );
    };
    // Decode the nullable column as `Option`: sqlx reads a NULL into a bare
    // `String` as "", which would mint a portal session for no customer.
    let customer: Option<String> =
        sqlx::query_scalar::<_, Option<String>>("SELECT stripeCustomerId FROM user WHERE id = ?1")
            .bind(&account_id)
            .fetch_optional(&state.mcp.pool)
            .await
            .ok()
            .flatten()
            .flatten();
    let Some(customer) = customer else {
        return text(
            StatusCode::NOT_FOUND,
            "This account has no billing history yet. Ask your agent for a fresh upgrade link.",
        );
    };
    match state.mcp.billing.portal_url(&customer).await {
        Some(Ok(session)) => Redirect::to(&session.url).into_response(),
        _ => text(
            StatusCode::SERVICE_UNAVAILABLE,
            "The billing portal is unavailable right now; try again in a minute.",
        ),
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
/// token-authed. Audited as `upgrade` with client "Console". An account
/// with a live subscription is refused: the Upgrade button only shows for
/// Free accounts, and a second Checkout Session would double-bill, so the
/// UI must send these humans to Manage billing instead.
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
    let stored = billing_account(&mcp.pool, &account.id).await;
    let live = stored.as_ref().is_some_and(|row| {
        crate::billing::has_live_subscription(&row.plan, row.subscription.as_deref())
    });
    if live && mcp.billing.enabled() {
        record_account_action(
            &mcp.pool,
            &account.id,
            "upgrade",
            "upgrade",
            "error",
            Some("invalid_argument"),
            started,
        )
        .await;
        return tool_response(&ToolError::invalid_argument(
            "this account already has an active Pro subscription; use Manage billing to update payment details",
        ));
    }
    let customer = stored.and_then(|row| row.customer);
    match mcp
        .billing
        .checkout_url(&account.id, &account.email, customer.as_deref())
        .await
    {
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
        sqlx::query_scalar::<_, Option<String>>("SELECT stripeCustomerId FROM user WHERE id = ?1")
            .bind(&account.id)
            .fetch_optional(&mcp.pool)
            .await
            .ok()
            .flatten()
            .flatten();
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
    /// the raw form so the route test pins the spec params end to end. It
    /// also serves `GET /v1/subscriptions/{id}` from a settable object so
    /// the webhook refetch tests control what is current. Every POST path
    /// is recorded so the double-checkout test can prove no new Checkout
    /// Session was minted; portal POSTs answer a canned portal URL.
    struct FakeStripe {
        base: String,
        body: Arc<Mutex<Option<String>>>,
        posts: Arc<Mutex<Vec<String>>>,
        sub: Arc<Mutex<serde_json::Value>>,
        fail_sub: Arc<Mutex<bool>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeStripe {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakeStripe {
        async fn start() -> Self {
            Self::start_with(serde_json::json!({
                "id": "sub_1", "customer": "cus_1", "status": "active",
                "cancel_at_period_end": false,
                "items": {"data": [{"price": {"id": "price_pro"}}]},
            }))
            .await
        }

        async fn start_with(sub: serde_json::Value) -> Self {
            use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen = Arc::new(Mutex::new(None));
            let body = seen.clone();
            let posts = Arc::new(Mutex::new(Vec::new()));
            let posted = posts.clone();
            let sub = Arc::new(Mutex::new(sub));
            let current = sub.clone();
            let fail_sub = Arc::new(Mutex::new(false));
            let failing = fail_sub.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((conn, _)) = listener.accept().await else {
                        return;
                    };
                    let seen = seen.clone();
                    let posted = posted.clone();
                    let current = current.clone();
                    let failing = failing.clone();
                    tokio::spawn(async move {
                        let mut lines = tokio::io::BufReader::new(conn);
                        let mut request_line = String::new();
                        if lines.read_line(&mut request_line).await.is_err() {
                            return;
                        }
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("")
                            .to_string();
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
                        // Subscription refetches are GETs with no form body;
                        // only the checkout/portal POSTs are recorded. The
                        // promo switch reads the coupon here: live, so the
                        // starter test pins the couponed checkout end to end.
                        let reply = if path.starts_with("/v1/coupons/") {
                            let coupon = "{\"id\":\"launch50\",\"object\":\"coupon\",\"valid\":true,\"percent_off\":50,\"duration\":\"forever\"}";
                            format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{coupon}", coupon.len())
                        } else if path.starts_with("/v1/subscriptions/") {
                            if *failing.lock().unwrap() {
                                let err = r#"{"error":{"message":"boom"}}"#;
                                format!("HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{err}", err.len())
                            } else {
                                let sub = current.lock().unwrap().to_string();
                                format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sub}", sub.len())
                            }
                        } else if path.starts_with("/v1/billing_portal/sessions") {
                            posted.lock().unwrap().push(path);
                            *seen.lock().unwrap() =
                                Some(String::from_utf8_lossy(&raw).into_owned());
                            let session = "{\"id\":\"bps_test_9\",\"object\":\"billing_portal.session\",\"url\":\"https://billing.stripe.com/p/session/test_portal_9\"}";
                            format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{session}", session.len())
                        } else {
                            posted.lock().unwrap().push(path);
                            *seen.lock().unwrap() =
                                Some(String::from_utf8_lossy(&raw).into_owned());
                            let session = "{\"id\":\"cs_test_9\",\"object\":\"checkout.session\",\"url\":\"https://checkout.stripe.com/pay/cs_test_9\"}";
                            format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{session}", session.len())
                        };
                        let mut stream = lines.into_inner();
                        let _ = stream.write_all(reply.as_bytes()).await;
                    });
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                body,
                posts,
                sub,
                fail_sub,
                task,
            }
        }

        fn set_subscription(&self, sub: serde_json::Value) {
            *self.sub.lock().unwrap() = sub;
        }

        fn set_fail(&self, fail: bool) {
            *self.fail_sub.lock().unwrap() = fail;
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
    fn subscription_object(cancel_at_period_end: bool) -> serde_json::Value {
        serde_json::json!({
            "id": "sub_1", "customer": "cus_1", "status": "active",
            "cancel_at_period_end": cancel_at_period_end,
            "items": {"data": [{"price": {"id": "price_pro"}}]},
        })
    }

    fn updated_payload(event_id: &str, cancel_at_period_end: bool) -> String {
        format!(
            "{{\"id\":\"{event_id}\",\"type\":\"customer.subscription.updated\",\
             \"data\":{{\"object\":{}}}}}",
            subscription_object(cancel_at_period_end),
        )
    }

    async fn post_signed_webhook(
        state: super::super::AppState,
        payload: &str,
    ) -> axum::response::Response {
        let signature =
            stripe::sign_test_header(SECRET, payload.as_bytes(), chrono::Utc::now().timestamp());
        let req = Request::post("/webhooks/stripe")
            .header("stripe-signature", signature)
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();
        super::super::router(state).oneshot(req).await.unwrap()
    }

    async fn seed_pro_subscription(pool: &SqlitePool) {
        seed_user(pool).await;
        sqlx::query(
            "UPDATE user SET plan = 'pro', planStatus = 'active', \
             stripeSubscriptionId = 'sub_1' WHERE id = 'a1'",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn plan_status(pool: &SqlitePool) -> (String, Option<String>) {
        sqlx::query_as("SELECT plan, planStatus FROM user WHERE id = 'a1'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Stripe does not promise webhook order, so the route applies the live
    /// subscription, not the event payload. Both arrival orders land here:
    /// each delivered event contradicts what the fake currently serves, and
    /// the stored plan follows the fake. Without the refetch both halves
    /// fail, storing the stale payload instead.
    #[tokio::test]
    async fn webhook_applies_current_subscription_not_stale_payload() {
        let fake = FakeStripe::start().await;
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_pro_subscription(&pool).await;
        // The portal cancel already landed, but the delivered event is the
        // older pre-cancel version: the plan must still say `canceled`.
        fake.set_subscription(subscription_object(true));
        let res = post_signed_webhook(state.clone(), &updated_payload("evt_stale_1", false)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            plan_status(&pool).await,
            ("pro".to_string(), Some("canceled".to_string()))
        );
        // The portal undo already landed, and the older still-flagged event
        // arrives late: the plan stays `active`.
        fake.set_subscription(subscription_object(false));
        let res = post_signed_webhook(state.clone(), &updated_payload("evt_stale_2", true)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            plan_status(&pool).await,
            ("pro".to_string(), Some("active".to_string()))
        );
    }

    /// A failed refetch 500s with nothing recorded, so Stripe retries; the
    /// plan is untouched and no `stripe_event` row claims the event.
    #[tokio::test]
    async fn webhook_refetch_failure_retries_without_recording() {
        let fake = FakeStripe::start().await;
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_pro_subscription(&pool).await;
        fake.set_fail(true);
        let res = post_signed_webhook(state, &updated_payload("evt_boom", false)).await;
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            plan_status(&pool).await,
            ("pro".to_string(), Some("active".to_string()))
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM stripe_event WHERE id = 'evt_boom'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    fn checkout_payload(event_id: &str, session: serde_json::Value) -> String {
        serde_json::json!({
            "id": event_id, "type": "checkout.session.completed",
            "data": {"object": session},
        })
        .to_string()
    }

    fn checkout_session(
        subscription: Option<&str>,
        mode: &str,
        payment_status: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": "cs_1", "mode": mode, "payment_status": payment_status,
            "client_reference_id": "a1", "metadata": {"account_id": "a1"},
            "customer": "cus_1", "subscription": subscription,
        })
    }

    async fn stored_subscription(pool: &SqlitePool) -> Option<String> {
        sqlx::query_scalar("SELECT stripeSubscriptionId FROM user WHERE id = 'a1'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The normal path end to end: Outer's reference, subscription mode, a
    /// paid status and a Pro-priced refetch, so the account goes Pro.
    #[tokio::test]
    async fn webhook_checkout_completed_upgrades_with_pro_price() {
        let fake = FakeStripe::start().await;
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_user(&pool).await;
        let payload = checkout_payload(
            "evt_cs_ok",
            checkout_session(Some("sub_1"), "subscription", "paid"),
        );
        let res = post_signed_webhook(state, &payload).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            plan_status(&pool).await,
            ("pro".to_string(), Some("active".to_string()))
        );
        assert_eq!(stored_subscription(&pool).await.as_deref(), Some("sub_1"));
    }

    /// O-5 end to end: the refetch bills a foreign price — acknowledged
    /// (no retry), but nothing upgrades and no id is stored.
    #[tokio::test]
    async fn webhook_checkout_ignores_a_foreign_price() {
        let fake = FakeStripe::start().await;
        fake.set_subscription(serde_json::json!({
            "id": "sub_9", "customer": "cus_1", "status": "active",
            "items": {"data": [{"price": {"id": "price_other"}}]},
        }));
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_user(&pool).await;
        let payload = checkout_payload(
            "evt_cs_foreign",
            checkout_session(Some("sub_9"), "subscription", "paid"),
        );
        let res = post_signed_webhook(state, &payload).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(plan_status(&pool).await, ("free".to_string(), None));
        assert_eq!(stored_subscription(&pool).await, None);
    }

    /// O-5 end to end: a one-time payment session is acknowledged and
    /// ignored — no refetch, no Pro.
    #[tokio::test]
    async fn webhook_checkout_ignores_payment_mode() {
        let fake = FakeStripe::start().await;
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_user(&pool).await;
        let payload = checkout_payload("evt_cs_pay", checkout_session(None, "payment", "paid"));
        let res = post_signed_webhook(state, &payload).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(plan_status(&pool).await, ("free".to_string(), None));
    }

    /// A failed checkout refetch 500s like the subscription-event path.
    #[tokio::test]
    async fn webhook_checkout_refetch_failure_retries_without_recording() {
        let fake = FakeStripe::start().await;
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_user(&pool).await;
        fake.set_fail(true);
        let payload = checkout_payload(
            "evt_cs_boom",
            checkout_session(Some("sub_1"), "subscription", "paid"),
        );
        let res = post_signed_webhook(state, &payload).await;
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(plan_status(&pool).await, ("free".to_string(), None));
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM stripe_event WHERE id = 'evt_cs_boom'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
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
        // The stored customer is reused: no second customer record, and no
        // `customer_email` alongside it (Stripe rejects both together).
        assert!(body.contains("customer=cus_1"), "{body}");
        assert!(!body.contains("customer_email"), "{body}");
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

    /// A second checkout for an account with a live subscription must not
    /// mint another Checkout Session (the first subscription keeps billing
    /// and the portal would no longer show it): the starter sends the human
    /// to the Customer Portal instead.
    #[tokio::test]
    async fn second_checkout_for_active_pro_lands_on_portal() {
        let fake = FakeStripe::start().await;
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_pro_subscription(&pool).await;
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
        assert_eq!(
            location,
            "https://billing.stripe.com/p/session/test_portal_9"
        );
        let posts = fake.posts.lock().unwrap();
        assert!(
            posts
                .iter()
                .any(|path| path == "/v1/billing_portal/sessions"),
            "{posts:?}"
        );
        assert!(
            !posts.iter().any(|path| path == "/v1/checkout/sessions"),
            "{posts:?}"
        );
    }
    /// The portal starter mints a fresh portal session for the stored
    /// customer and redirects to it.
    #[tokio::test]
    async fn portal_starter_redirects_to_a_fresh_session() {
        let fake = FakeStripe::start().await;
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_user(&pool).await;
        let token = crate::billing::sign_portal_token(
            b"test-sign-key",
            "a1",
            chrono::Utc::now().timestamp() + 60,
        );
        let req = Request::get(format!("/billing/portal?t={token}"))
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
        assert_eq!(
            location,
            "https://billing.stripe.com/p/session/test_portal_9"
        );
        let body = fake.body.lock().unwrap().clone().unwrap();
        assert!(body.contains("customer=cus_1"), "{body}");
    }

    /// Bad or expired portal tokens, a checkout token on the portal route,
    /// and an account with no customer all refuse in plain text.
    #[tokio::test]
    async fn portal_starter_refuses_bad_tokens_and_missing_customers() {
        let fake = FakeStripe::start().await;
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_user(&pool).await;
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) \
             VALUES ('b1', 'b@example.com', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        for token in [
            "nope".to_string(),
            crate::billing::sign_portal_token(b"test-sign-key", "a1", now - 1),
            crate::billing::sign_checkout_token(b"test-sign-key", "a1", now + 60),
        ] {
            let req = Request::get(format!("/billing/portal?t={token}"))
                .body(Body::empty())
                .unwrap();
            let res = super::super::router(state.clone())
                .oneshot(req)
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{token}");
        }
        let token = crate::billing::sign_portal_token(b"test-sign-key", "b1", now + 60);
        let req = Request::get(format!("/billing/portal?t={token}"))
            .body(Body::empty())
            .unwrap();
        let res = super::super::router(state).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert!(
            fake.posts.lock().unwrap().is_empty(),
            "no Stripe call on refusal"
        );
    }

    /// The console Upgrade button for an account with a live subscription
    /// is refused instead of minting a second Checkout Session: the Upgrade
    /// button only shows for non-Pro accounts, and the UI must send these
    /// humans to Manage billing.
    #[tokio::test]
    async fn console_checkout_refuses_a_live_subscription() {
        let fake = FakeStripe::start().await;
        let (state, pool, _) = app_state(enabled_billing(fake.base.clone())).await;
        seed_pro_subscription(&pool).await;
        let id = crate::auth::create_session(&pool, "a1").await.unwrap();
        let req = Request::post("/api/console/billing/checkout")
            .header("content-type", "application/json")
            .header("sec-fetch-site", "same-origin")
            .header("cookie", format!("{}={id}", crate::auth::SESSION_COOKIE))
            .body(Body::from("{}"))
            .unwrap();
        let res = super::super::router(state).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "invalid_argument");
        assert!(
            fake.posts.lock().unwrap().is_empty(),
            "no Checkout Session minted"
        );
    }
}
