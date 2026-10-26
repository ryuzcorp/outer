//! Plans, Stripe links and webhook transitions (spec §3, §14.2).
//!
//! The plan comes only from webhooks: nothing else upgrades an account.
//! `stripe.rs` holds the Stripe REST client and signature check; this module
//! holds everything around them: whether billing is enabled, the per-account
//! checkout-starter links refusals carry, the `user` row transitions, the
//! downgrade path, and the past-due grace sweep.
//!
//! Downgrade (spec §3, details in `downgrade.rs`): the oldest app stays
//! live and the rest stop (`desired_state`), stamped with the downgrade
//! date for the 30-day retention. Custom hostnames are deleted on every
//! owned app, public apps go back to `unlisted` (stored `public` must not
//! survive on Free), and Pro bindings fail the next deploy with the usual
//! clear error. Collaborator rows stay but grant nothing until re-upgrade
//! (enforced by `collaborators::role`); keep-warm flags stay stored but
//! ignored on Free.
pub mod downgrade;
pub mod stripe;
pub use downgrade::RetentionDeps;

use sqlx::SqlitePool;

use crate::auth::EmailSender;
use crate::config::StripeConfig;
use hmac::{Hmac, KeyInit, Mac};

/// Seven days of Pro after a missed payment, then the downgrade rules
/// (spec §14.2). Stored as `user.graceUntil` (unix seconds).
pub const GRACE_SECS: i64 = 7 * 24 * 3600;

/// How long a refusal's checkout-starter link stays valid. Long enough to
/// survive an agent relaying it to a human; the click mints a fresh Stripe
/// session, so expiry only bounds replay, not the subscription.
pub const CHECKOUT_TOKEN_SECS: i64 = 7 * 24 * 3600;

/// The day-5 reminder goes out when this much grace is left. The hourly
/// sweep fires once inside the one-hour window.
pub const REMINDER_LOW_SECS: i64 = 47 * 3600;
pub const REMINDER_HIGH_SECS: i64 = 48 * 3600;

/// Billing as the tools, console and webhook see it. Built once in `main`
/// from config; never global.
#[derive(Debug, Clone)]
pub enum BillingState {
    /// `STRIPE_*` unset (localhost dev): checkout, portal and the webhook
    /// refuse, and refusals point at the console plan card.
    Disabled { platform_base: String },
    /// Billing live: Stripe calls plus the per-account link signer.
    Enabled {
        client: stripe::StripeClient,
        /// `STRIPE_WEBHOOK_SECRET`: verifying `Stripe-Signature`.
        webhook_secret: String,
        platform_base: String,
        sign_key: Vec<u8>,
    },
}

impl BillingState {
    /// From config plus the platform base URL
    /// (`{scheme}://{domain}{port_suffix}`).
    pub fn from_config(
        stripe: &StripeConfig,
        platform_base: String,
        sign_key: Vec<u8>,
    ) -> anyhow::Result<Self> {
        if !stripe.enabled() {
            return Ok(Self::Disabled { platform_base });
        }
        let client = stripe::StripeClient::new(
            stripe.secret_key.clone().unwrap_or_default(),
            stripe.price_pro.clone().unwrap_or_default(),
            stripe.launch_coupon.clone(),
        )?;
        Ok(Self::Enabled {
            client,
            webhook_secret: stripe.webhook_secret.clone().unwrap_or_default(),
            platform_base,
            sign_key,
        })
    }

    pub fn enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    fn platform_base(&self) -> &str {
        match self {
            Self::Disabled { platform_base } => platform_base,
            Self::Enabled { platform_base, .. } => platform_base,
        }
    }

    /// The `upgrade_url` every `payment_required` and `quota_exceeded`
    /// refusal carries. When billing is live it is a per-account
    /// checkout-starter link: the click verifies the signed token and mints
    /// a fresh Stripe session, so the link never goes stale the way a stored
    /// `checkout.stripe.com` URL would. When billing is disabled it is the
    /// console plan card, which says billing isn't wired yet.
    pub fn upgrade_url(&self, account_id: &str, now_secs: i64) -> String {
        match self {
            Self::Disabled { platform_base } => format!("{platform_base}/console"),
            Self::Enabled {
                platform_base,
                sign_key,
                ..
            } => format!(
                "{}/billing/checkout?t={}",
                platform_base,
                sign_checkout_token(sign_key, account_id, now_secs + CHECKOUT_TOKEN_SECS)
            ),
        }
    }

    /// Mint a live Checkout Session URL for the human to open (the MCP
    /// `upgrade` tool and the console Plan card). `None` when disabled.
    pub async fn checkout_url(
        &self,
        account_id: &str,
        email: &str,
    ) -> Option<anyhow::Result<stripe::CheckoutSession>> {
        match self {
            Self::Disabled { .. } => None,
            Self::Enabled { client, .. } => {
                let success_url = format!("{}/billing/done", self.platform_base());
                Some(
                    client
                        .create_checkout_session(account_id, email, &success_url)
                        .await,
                )
            }
        }
    }

    /// Mint a live Customer Portal URL. `None` when disabled.
    pub async fn portal_url(
        &self,
        customer_id: &str,
    ) -> Option<anyhow::Result<stripe::CheckoutSession>> {
        match self {
            Self::Disabled { .. } => None,
            Self::Enabled { client, .. } => {
                let return_url = format!("{}/console", self.platform_base());
                Some(client.create_portal_session(customer_id, &return_url).await)
            }
        }
    }

    fn sign_key(&self) -> Option<&[u8]> {
        match self {
            Self::Disabled { .. } => None,
            Self::Enabled { sign_key, .. } => Some(sign_key),
        }
    }

    /// The secret the webhook route verifies `Stripe-Signature` with.
    /// `None` when billing is disabled — the webhook refuses outright.
    pub fn webhook_secret(&self) -> Option<&str> {
        match self {
            Self::Disabled { .. } => None,
            Self::Enabled { webhook_secret, .. } => Some(webhook_secret),
        }
    }

    /// Verify a `GET /billing/checkout?t=` token back to its account.
    pub fn verify_checkout_token(&self, token: &str, now_secs: i64) -> Option<String> {
        let key = self.sign_key()?;
        verify_checkout_token(key, token, now_secs)
    }
}

/// Sign `{account_id}.{exp}.{hmac}` for a checkout-starter link. The HMAC
/// covers a domain-separated body so a token minted for one purpose never
/// verifies as another (the alert unsubscribe tokens sign a different body).
pub fn sign_checkout_token(sign_key: &[u8], account_id: &str, exp_secs: i64) -> String {
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(sign_key).expect("hmac takes any key length");
    mac.update(b"billing-checkout-v1.");
    mac.update(account_id.as_bytes());
    mac.update(b".");
    mac.update(exp_secs.to_string().as_bytes());
    format!(
        "{account_id}.{exp_secs}.{}",
        hex::encode(mac.finalize().into_bytes())
    )
}

/// The reverse: the account the token was minted for, or `None` on any
/// mismatch, malformed token or expiry.
pub fn verify_checkout_token(sign_key: &[u8], token: &str, now_secs: i64) -> Option<String> {
    let (account_id, rest) = token.split_once('.')?;
    let (exp, _) = rest.split_once('.')?;
    if account_id.is_empty() || account_id.len() > 200 {
        return None;
    }
    let exp_secs: i64 = exp.parse().ok()?;
    if exp_secs <= now_secs {
        return None;
    }
    let expected = sign_checkout_token(sign_key, account_id, exp_secs);
    if constant_time_eq::constant_time_eq(token.as_bytes(), expected.as_bytes()) {
        Some(account_id.to_string())
    } else {
        None
    }
}

/// What a webhook call needs beyond the pool: the mailer for past-due mail,
/// billing for the upgrade links that mail carries, and the edge handles
/// the downgrade path strips Pro extras through (SaaS hostnames, cache
/// purges, app-host names).
pub struct WebhookCtx<'a> {
    pub pool: &'a SqlitePool,
    pub sender: &'a EmailSender,
    pub billing: &'a BillingState,
    pub saas: Option<crate::cloudflare::CustomHostnames>,
    pub purge: Option<crate::cloudflare::CachePurge>,
    pub apps_domain: String,
}

/// How the webhook answered: applied, or already seen. `stopped` names the
/// slugs the route must stop fleets for after a downgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied {
        account_id: Option<String>,
        stopped: Vec<String>,
    },
    Duplicate,
}

/// What one event did to a `user` row: the account, plus the slugs a
/// downgrade stopped.
struct Transition {
    account_id: Option<String>,
    stopped: Vec<String>,
}

impl Transition {
    fn kept(account_id: Option<String>) -> Self {
        Self {
            account_id,
            stopped: Vec::new(),
        }
    }
}

/// Record the event idempotently, then apply its transition (spec §14.2).
/// Unknown types are recorded and ignored so a new Stripe event never 500s.
/// Every handler resolves the account the same way: checkout objects carry
/// the account id, subscription/invoice objects resolve through the stored
/// customer/subscription ids.
pub async fn apply_event(
    ctx: &WebhookCtx<'_>,
    event_id: &str,
    event_type: &str,
    object: &serde_json::Value,
    now_secs: i64,
) -> anyhow::Result<ApplyOutcome> {
    let inserted = sqlx::query(
        "INSERT INTO stripe_event (id, type, received_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(event_id)
    .bind(event_type)
    .bind(now_secs)
    .execute(ctx.pool)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Ok(ApplyOutcome::Duplicate);
    }
    let transition = match event_type {
        "checkout.session.completed" => apply_checkout_completed(ctx.pool, object).await?,
        "customer.subscription.created" | "customer.subscription.updated" => {
            apply_subscription(ctx, object, now_secs).await?
        }
        "customer.subscription.deleted" => {
            apply_subscription_deleted(ctx, object, now_secs).await?
        }
        "invoice.paid" => apply_invoice_paid(ctx.pool, object).await?,
        "invoice.payment_failed" => apply_payment_failed(ctx, object, now_secs).await?,
        _ => Transition::kept(None),
    };
    Ok(ApplyOutcome::Applied {
        account_id: transition.account_id,
        stopped: transition.stopped,
    })
}

struct UserBilling {
    id: String,
    email: String,
    grace_until: Option<i64>,
}

/// A Stripe id-or-object field: `"cus_1"` or `{"id": "cus_1"}`.
fn str_or_id(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::String(id) => Some(id),
        serde_json::Value::Object(_) => value.get("id").and_then(serde_json::Value::as_str),
        _ => None,
    }
}

/// One `user` row as the transitions read it.
#[derive(sqlx::FromRow)]
struct UserRow {
    id: String,
    email: String,
    #[sqlx(rename = "graceUntil")]
    grace_until: Option<i64>,
}

fn to_billing(row: UserRow) -> UserBilling {
    UserBilling {
        id: row.id,
        email: row.email,
        grace_until: row.grace_until,
    }
}

async fn user_by_id(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<UserBilling>> {
    let row: Option<UserRow> =
        sqlx::query_as("SELECT id, email, graceUntil FROM user WHERE id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(to_billing))
}

async fn user_by_customer(
    pool: &SqlitePool,
    customer: &str,
) -> anyhow::Result<Option<UserBilling>> {
    let row: Option<UserRow> =
        sqlx::query_as("SELECT id, email, graceUntil FROM user WHERE stripeCustomerId = ?1")
            .bind(customer)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(to_billing))
}

fn object_str<'o>(object: &'o serde_json::Value, key: &str) -> Option<&'o str> {
    object.get(key).and_then(serde_json::Value::as_str)
}

async fn user_by_email(pool: &SqlitePool, email: &str) -> anyhow::Result<Option<UserBilling>> {
    let row: Option<UserRow> =
        sqlx::query_as("SELECT id, email, graceUntil FROM user WHERE email = ?1")
            .bind(email.to_ascii_lowercase())
            .fetch_optional(pool)
            .await?;
    Ok(row.map(to_billing))
}

async fn user_by_subscription(
    pool: &SqlitePool,
    subscription: &str,
) -> anyhow::Result<Option<UserBilling>> {
    let row: Option<UserRow> =
        sqlx::query_as("SELECT id, email, graceUntil FROM user WHERE stripeSubscriptionId = ?1")
            .bind(subscription)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(to_billing))
}

/// Promote to Pro with the Stripe ids attached. Upgrades take effect on the
/// next tool call because every quota gate reads the live `user` row.
/// Re-upgrading cancels a pending downgrade purge (the stamps clear) but
/// restarts nothing: stopped apps stay stopped until the owner wakes one.
async fn set_pro(
    pool: &SqlitePool,
    account_id: &str,
    customer: Option<&str>,
    subscription: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE user SET plan = 'pro', planStatus = 'active', \
         stripeCustomerId = COALESCE(?2, stripeCustomerId), \
         stripeSubscriptionId = COALESCE(?3, stripeSubscriptionId), \
         graceUntil = NULL WHERE id = ?1",
    )
    .bind(account_id)
    .bind(customer)
    .bind(subscription)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE app SET downgraded_stopped_at = NULL, \
         downgrade_purge_warned_at = NULL WHERE owner_id = ?1",
    )
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn apply_checkout_completed(
    pool: &SqlitePool,
    object: &serde_json::Value,
) -> anyhow::Result<Transition> {
    let customer = object.get("customer").and_then(str_or_id);
    let subscription = object.get("subscription").and_then(str_or_id);
    let account = object_str(object, "client_reference_id").or_else(|| {
        object
            .get("metadata")
            .and_then(|meta| object_str(meta, "account_id"))
    });
    let user = match account {
        Some(id) => user_by_id(pool, id).await?,
        None => None,
    };
    let user = match user {
        Some(user) => Some(user),
        None => match customer {
            Some(customer) => user_by_customer(pool, customer).await?,
            None => None,
        },
    };
    let user = match user {
        Some(user) => Some(user),
        None => match object_str(object, "customer_email") {
            Some(email) => user_by_email(pool, email).await?,
            None => None,
        },
    };
    let Some(user) = user else {
        return Ok(Transition::kept(None));
    };
    set_pro(pool, &user.id, customer, subscription).await?;
    Ok(Transition::kept(Some(user.id)))
}

async fn apply_subscription(
    ctx: &WebhookCtx<'_>,
    object: &serde_json::Value,
    now_secs: i64,
) -> anyhow::Result<Transition> {
    let pool = ctx.pool;
    let id = object_str(object, "id").unwrap_or_default();
    let customer = object.get("customer").and_then(str_or_id);
    let status = object_str(object, "status").unwrap_or_default();
    let cancel_at_period_end = object
        .get("cancel_at_period_end")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let user = match customer {
        Some(customer) => user_by_customer(ctx.pool, customer).await?,
        None => None,
    };
    let user = match user {
        Some(user) => Some(user),
        None => {
            if id.is_empty() {
                None
            } else {
                user_by_subscription(ctx.pool, id).await?
            }
        }
    };
    let Some(user) = user else {
        return Ok(Transition::kept(None));
    };
    // A downgrade stops every app but the oldest; the route stops their
    // fleets from the returned slugs.
    let mut stopped = Vec::new();
    match status {
        "active" | "trialing" => {
            set_pro(ctx.pool, &user.id, customer, Some(id)).await?;
        }
        "past_due" => {
            // First past-due event starts the 7-day grace clock; a repeat
            // keeps the earlier deadline instead of extending it.
            let grace = match user.grace_until {
                Some(existing) => existing,
                None => now_secs + GRACE_SECS,
            };
            sqlx::query(
                "UPDATE user SET plan = 'pro', planStatus = 'past_due', \
                 stripeCustomerId = COALESCE(?2, stripeCustomerId), \
                 stripeSubscriptionId = COALESCE(?3, stripeSubscriptionId), \
                 graceUntil = ?4 WHERE id = ?1",
            )
            .bind(&user.id)
            .bind(customer)
            .bind(if id.is_empty() { None } else { Some(id) })
            .bind(grace)
            .execute(pool)
            .await?;
        }
        "canceled" => {
            if cancel_at_period_end {
                // Canceled at period end: Pro until the `deleted` event
                // arrives, visibly marked so the plan card can say so.
                sqlx::query("UPDATE user SET planStatus = 'canceled' WHERE id = ?1")
                    .bind(&user.id)
                    .execute(pool)
                    .await?;
            } else {
                stopped = downgrade_account(ctx, &user.id, Some(id), now_secs).await?;
            }
        }
        // `unpaid` means Stripe exhausted its retries: the grace the
        // `invoice.payment_failed` events started is over in practice, so
        // the downgrade rules apply now (documented choice).
        "unpaid" => {
            stopped = downgrade_account(ctx, &user.id, Some(id), now_secs).await?;
        }
        // `incomplete`, `incomplete_expired`, `paused` and anything new:
        // recorded above, no plan change.
        _ => {}
    }
    Ok(Transition {
        account_id: Some(user.id),
        stopped,
    })
}

async fn apply_subscription_deleted(
    ctx: &WebhookCtx<'_>,
    object: &serde_json::Value,
    now_secs: i64,
) -> anyhow::Result<Transition> {
    let pool = ctx.pool;
    let id = object_str(object, "id").unwrap_or_default();
    let customer = object.get("customer").and_then(str_or_id);
    let user = match customer {
        Some(customer) => user_by_customer(pool, customer).await?,
        None => None,
    };
    let user = match user {
        Some(user) => Some(user),
        None => {
            if id.is_empty() {
                None
            } else {
                user_by_subscription(pool, id).await?
            }
        }
    };
    let Some(user) = user else {
        return Ok(Transition::kept(None));
    };
    let stopped = downgrade_account(ctx, &user.id, Some(id), now_secs).await?;
    Ok(Transition {
        account_id: Some(user.id),
        stopped,
    })
}

async fn apply_invoice_paid(
    pool: &SqlitePool,
    object: &serde_json::Value,
) -> anyhow::Result<Transition> {
    let customer = object.get("customer").and_then(str_or_id);
    let subscription = object.get("subscription").and_then(str_or_id);
    let user = match customer {
        Some(customer) => user_by_customer(pool, customer).await?,
        None => None,
    };
    let user = match user {
        Some(user) => Some(user),
        None => match subscription {
            Some(subscription) => user_by_subscription(pool, subscription).await?,
            None => None,
        },
    };
    let Some(user) = user else {
        return Ok(Transition::kept(None));
    };
    set_pro(pool, &user.id, customer, subscription).await?;
    Ok(Transition::kept(Some(user.id)))
}

async fn apply_payment_failed(
    ctx: &WebhookCtx<'_>,
    object: &serde_json::Value,
    now_secs: i64,
) -> anyhow::Result<Transition> {
    let customer = object.get("customer").and_then(str_or_id);
    let subscription = object.get("subscription").and_then(str_or_id);
    let user = match customer {
        Some(customer) => user_by_customer(ctx.pool, customer).await?,
        None => None,
    };
    let user = match user {
        Some(user) => Some(user),
        None => match subscription {
            Some(subscription) => user_by_subscription(ctx.pool, subscription).await?,
            None => None,
        },
    };
    let Some(user) = user else {
        return Ok(Transition::kept(None));
    };
    let grace = match user.grace_until {
        Some(existing) => existing,
        None => now_secs + GRACE_SECS,
    };
    sqlx::query(
        "UPDATE user SET plan = 'pro', planStatus = 'past_due', \
         stripeCustomerId = COALESCE(?2, stripeCustomerId), \
         stripeSubscriptionId = COALESCE(?3, stripeSubscriptionId), \
         graceUntil = ?4 WHERE id = ?1",
    )
    .bind(&user.id)
    .bind(customer)
    .bind(subscription)
    .bind(grace)
    .execute(ctx.pool)
    .await?;
    // Day-0 mail (spec §14.2): the send may fail and the plan change stands,
    // like the collaborator invite.
    let upgrade = ctx.billing.upgrade_url(&user.id, now_secs);
    let text = format!(
        "Your Outer Pro payment failed. Update your payment details within 7 days to keep Pro (10 apps, 500 changes a day): {upgrade}\n\nAfter 7 days your account returns to Free: every app but your oldest stops, and Pro features pause until you resubscribe. Your code and data stay."
    );
    if let Err(err) = ctx
        .sender
        .send_text(
            &user.email,
            "Outer Pro payment failed — 7 days to update",
            &text,
        )
        .await
    {
        tracing::warn!(account = %user.id, "past-due day-0 email failed: {err:#}");
    }
    Ok(Transition::kept(Some(user.id)))
}

/// Downgrade to Free (spec §3): custom hostnames go and public apps go
/// back to unlisted on every owned app (`downgrade::strip_pro_extras`),
/// the oldest app stays live and the rest stop, stamped with the downgrade
/// date for the 30-day retention. The subscription link clears, the
/// customer id stays for resubscribe and the portal. `expected_sub`
/// guards against a stale event clobbering a newer subscription: skip when
/// the row names a different subscription. Returns the stopped slugs so
/// the caller can stop their fleets.
pub async fn downgrade_account(
    ctx: &WebhookCtx<'_>,
    account_id: &str,
    expected_sub: Option<&str>,
    now_secs: i64,
) -> anyhow::Result<Vec<String>> {
    let pool = ctx.pool;
    if let Some(expected) = expected_sub {
        let current: Option<Option<String>> =
            sqlx::query_scalar("SELECT stripeSubscriptionId FROM user WHERE id = ?1")
                .bind(account_id)
                .fetch_optional(pool)
                .await?;
        if let Some(Some(current)) = current {
            if current != expected {
                return Ok(Vec::new());
            }
        }
    }
    sqlx::query(
        "UPDATE user SET plan = 'free', planStatus = 'canceled', graceUntil = NULL, \
         stripeSubscriptionId = NULL WHERE id = ?1",
    )
    .bind(account_id)
    .execute(pool)
    .await?;
    downgrade::strip_pro_extras(
        pool,
        ctx.saas.as_ref(),
        ctx.purge.as_ref(),
        &ctx.apps_domain,
        account_id,
    )
    .await?;
    #[derive(sqlx::FromRow)]
    struct AppRow {
        id: String,
        slug: String,
    }
    let apps: Vec<AppRow> = sqlx::query_as(
        "SELECT id, slug FROM app WHERE owner_id = ?1 ORDER BY created_at ASC, id ASC",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    // The kept oldest app leaves retention: a previously stamped app that
    // is now the oldest (the rest were deleted) has its purge canceled.
    if let Some(kept) = apps.first() {
        sqlx::query(
            "UPDATE app SET downgraded_stopped_at = NULL, \
             downgrade_purge_warned_at = NULL WHERE id = ?1",
        )
        .bind(&kept.id)
        .execute(pool)
        .await?;
    }
    let mut stopped = Vec::new();
    for app in apps.iter().skip(1) {
        sqlx::query(
            "UPDATE app SET desired_state = 'stopped', downgraded_stopped_at = ?1, \
             downgrade_purge_warned_at = NULL WHERE id = ?2",
        )
        .bind(now_secs)
        .bind(&app.id)
        .execute(pool)
        .await?;
        stopped.push(app.slug.clone());
    }
    Ok(stopped)
}

/// One grace-sweep pass (called hourly): day-5 reminders inside the
/// one-hour window, downgrades at expiry. Returns `(reminded, downgraded)`.
pub async fn run_grace_sweep(
    billing: &BillingState,
    pool: &SqlitePool,
    sender: &EmailSender,
    delete_cfg: &crate::apps::delete::DeleteConfig,
    now_secs: i64,
) -> anyhow::Result<(usize, usize)> {
    if !billing.enabled() {
        return Ok((0, 0));
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        id: String,
        email: String,
        #[sqlx(rename = "graceUntil")]
        grace_until: Option<i64>,
    }
    let users: Vec<Row> = sqlx::query_as(
        "SELECT id, email, graceUntil FROM user \
         WHERE plan = 'pro' AND planStatus = 'past_due' AND graceUntil IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut reminded = 0;
    let mut downgraded = 0;
    for user in users {
        let Some(grace_until) = user.grace_until else {
            continue;
        };
        let remaining = grace_until - now_secs;
        if remaining <= 0 {
            let ctx = WebhookCtx {
                pool,
                sender,
                billing,
                saas: crate::cloudflare::CustomHostnames::from_purge(
                    delete_cfg.cache_purge.as_ref(),
                ),
                purge: delete_cfg.cache_purge.clone(),
                apps_domain: delete_cfg.apps_domain.clone(),
            };
            downgrade_account(&ctx, &user.id, None, now_secs).await?;
            downgraded += 1;
            let upgrade = billing.upgrade_url(&user.id, now_secs);
            let text = format!(
                "Your Outer Pro grace period ended and your account is back on Free: every app but your oldest stopped, and Pro features paused. Your code and data are kept. Resubscribe anytime: {upgrade}"
            );
            if let Err(err) = sender
                .send_text(&user.email, "Outer Pro ended — you're on Free", &text)
                .await
            {
                tracing::warn!(account = %user.id, "downgrade email failed: {err:#}");
            }
        } else if remaining > REMINDER_LOW_SECS && remaining <= REMINDER_HIGH_SECS {
            let upgrade = billing.upgrade_url(&user.id, now_secs);
            let text = format!(
                "Your Outer Pro payment is still failing — 2 days left before your account returns to Free. Update payment details: {upgrade}"
            );
            if let Err(err) = sender
                .send_text(&user.email, "Outer Pro ends in 2 days", &text)
                .await
            {
                tracing::warn!(account = %user.id, "past-due day-5 email failed: {err:#}");
            } else {
                reminded += 1;
            }
        }
    }
    Ok((reminded, downgraded))
}

/// The hourly past-due sweep, plus the downgrade-retention pass (day-23
/// warnings, day-30 purges through the shared `delete_app` path). A missed
/// tick only delays mail and the downgrade; the next tick catches up.
pub fn spawn_grace_sweep(
    billing: BillingState,
    pool: SqlitePool,
    sender: EmailSender,
    retention: RetentionDeps,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(3600));
        // The first tick fires immediately: `interval` is constructed
        // `MissedTickBehavior::Burst` by default, so a restart catches up.
        loop {
            timer.tick().await;
            let now_secs = chrono::Utc::now().timestamp();
            match run_grace_sweep(&billing, &pool, &sender, &retention.delete_cfg, now_secs).await {
                Ok((0, 0)) => {}
                Ok((reminded, downgraded)) => {
                    tracing::info!(
                        reminded,
                        downgraded,
                        "billing grace sweep: reminders and downgrades"
                    );
                }
                Err(err) => tracing::warn!("billing grace sweep failed: {err:#}"),
            }
            match downgrade::run_downgrade_retention(&pool, &sender, &billing, &retention, now_secs)
                .await
            {
                Ok((0, 0)) => {}
                Ok((warned, purged)) => {
                    tracing::info!(
                        warned,
                        purged,
                        "billing retention sweep: downgrade warnings and purges"
                    );
                }
                Err(err) => tracing::warn!("billing retention sweep failed: {err:#}"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::stripe::sign_test_header;
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    const NOW: i64 = 1_700_000_000;

    async fn pool() -> SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("billing.sqlite"))
            .await
            .unwrap();
        std::mem::forget(dir);
        pool
    }

    async fn seed_user(pool: &SqlitePool, id: &str, email: &str) {
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) \
             VALUES (?1, ?2, 0, 0, 0)",
        )
        .bind(id)
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_app(pool: &SqlitePool, id: &str, slug: &str, owner: &str, created_at: i64) {
        // Ports, like `create_app` assigns: `delete_app` needs them.
        let port: i64 = 20000 + (created_at % 1000);
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
             last_activity_at, created_at) \
             VALUES (?1, ?2, ?3, abs(random()) % 100000 + 300000, ?4, ?4 + 1, 0, ?5)",
        )
        .bind(id)
        .bind(slug)
        .bind(owner)
        .bind(port)
        .bind(created_at)
        .execute(pool)
        .await
        .unwrap();
    }

    fn capture() -> (EmailSender, Arc<Mutex<HashMap<String, String>>>) {
        let codes = Arc::new(Mutex::new(HashMap::new()));
        (
            EmailSender::Capture {
                codes: codes.clone(),
            },
            codes,
        )
    }

    fn enabled_billing() -> BillingState {
        BillingState::Enabled {
            client: stripe::StripeClient::new(
                "sk_test_1".to_string(),
                "price_pro".to_string(),
                Some("launch50".to_string()),
            )
            .unwrap(),
            webhook_secret: "whsec_test".to_string(),
            platform_base: "https://outer.now".to_string(),
            sign_key: b"test-sign-key".to_vec(),
        }
    }

    fn ctx<'a>(
        pool: &'a SqlitePool,
        sender: &'a EmailSender,
        billing: &'a BillingState,
    ) -> WebhookCtx<'a> {
        WebhookCtx {
            pool,
            sender,
            billing,
            saas: None,
            purge: None,
            apps_domain: "outer.now".to_string(),
        }
    }

    fn test_delete_cfg(data_dir: std::path::PathBuf) -> crate::apps::delete::DeleteConfig {
        crate::apps::delete::DeleteConfig {
            data_dir,
            cgroups: None,
            apps_domain: "outer.now".to_string(),
            cache_purge: None,
        }
    }

    async fn plan_of(pool: &SqlitePool, id: &str) -> (String, Option<String>, Option<i64>) {
        sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
            "SELECT plan, planStatus, graceUntil FROM user WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn apply(
        pool: &SqlitePool,
        sender: &EmailSender,
        billing: &BillingState,
        id: &str,
        kind: &str,
        object: serde_json::Value,
    ) -> ApplyOutcome {
        apply_event(&ctx(pool, sender, billing), id, kind, &object, NOW)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn checkout_completed_promotes_by_client_reference() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        let object = serde_json::json!({
            "id": "cs_1", "client_reference_id": "a1",
            "customer": "cus_1", "subscription": "sub_1",
            "customer_email": "a@example.com", "metadata": {"account_id": "a1"},
        });
        let outcome = apply(
            &pool,
            &sender,
            &billing,
            "evt_1",
            "checkout.session.completed",
            object,
        )
        .await;
        assert!(matches!(
            outcome,
            ApplyOutcome::Applied {
                account_id: Some(_),
                ..
            }
        ));
        assert_eq!(
            plan_of(&pool, "a1").await,
            ("pro".to_string(), Some("active".to_string()), None)
        );
        let (customer, sub): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT stripeCustomerId, stripeSubscriptionId FROM user WHERE id = 'a1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(customer.as_deref(), Some("cus_1"));
        assert_eq!(sub.as_deref(), Some("sub_1"));
    }

    #[tokio::test]
    async fn checkout_completed_falls_back_to_email() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        let object = serde_json::json!({
            "id": "cs_2", "customer": "cus_9", "subscription": "sub_9",
            "customer_email": "A@EXAMPLE.COM",
        });
        apply(
            &pool,
            &sender,
            &billing,
            "evt_2",
            "checkout.session.completed",
            object,
        )
        .await;
        assert_eq!(plan_of(&pool, "a1").await.0, "pro");
    }

    #[tokio::test]
    async fn repeat_delivery_is_idempotent() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        sqlx::query(
            "UPDATE user SET plan = 'pro', planStatus = 'active', stripeCustomerId = 'cus_1', \
             stripeSubscriptionId = 'sub_1' WHERE id = 'a1'",
        )
        .execute(&pool)
        .await
        .unwrap();
        seed_app(&pool, "app1", "old", "a1", 1).await;
        seed_app(&pool, "app2", "new", "a1", 2).await;
        let object = serde_json::json!({"id": "sub_1", "customer": "cus_1", "status": "canceled"});
        let first = apply(
            &pool,
            &sender,
            &billing,
            "evt_9",
            "customer.subscription.deleted",
            object.clone(),
        )
        .await;
        let ApplyOutcome::Applied { stopped, .. } = first else {
            panic!("expected applied, got {first:?}");
        };
        // Oldest (`old`) stays live, `new` stops.
        assert_eq!(stopped, vec!["new".to_string()]);
        let state: String = sqlx::query_scalar("SELECT desired_state FROM app WHERE id = 'app2'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "stopped");
        let second = apply(
            &pool,
            &sender,
            &billing,
            "evt_9",
            "customer.subscription.deleted",
            object,
        )
        .await;
        assert_eq!(second, ApplyOutcome::Duplicate);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stripe_event WHERE id = 'evt_9'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn stale_deleted_does_not_clobber_a_new_subscription() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        sqlx::query(
            "UPDATE user SET plan = 'pro', planStatus = 'active', \
             stripeCustomerId = 'cus_1', stripeSubscriptionId = 'sub_2' WHERE id = 'a1'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let object = serde_json::json!({"id": "sub_1", "customer": "cus_1", "status": "canceled"});
        apply(
            &pool,
            &sender,
            &billing,
            "evt_8",
            "customer.subscription.deleted",
            object,
        )
        .await;
        // Still Pro on the new subscription.
        assert_eq!(plan_of(&pool, "a1").await.0, "pro");
    }

    #[tokio::test]
    async fn past_due_starts_grace_and_repeats_keep_it() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        sqlx::query("UPDATE user SET stripeCustomerId = 'cus_1' WHERE id = 'a1'")
            .execute(&pool)
            .await
            .unwrap();
        let object = serde_json::json!({"id": "sub_1", "customer": "cus_1", "status": "past_due"});
        apply(
            &pool,
            &sender,
            &billing,
            "evt_3",
            "customer.subscription.updated",
            object.clone(),
        )
        .await;
        assert_eq!(
            plan_of(&pool, "a1").await,
            (
                "pro".to_string(),
                Some("past_due".to_string()),
                Some(NOW + GRACE_SECS)
            )
        );
        // A repeat event for the same subscription does not extend the clock.
        sqlx::query("UPDATE user SET graceUntil = ?1 WHERE id = 'a1'")
            .bind(NOW + 100)
            .execute(&pool)
            .await
            .unwrap();
        apply(
            &pool,
            &sender,
            &billing,
            "evt_4",
            "customer.subscription.updated",
            object,
        )
        .await;
        assert_eq!(plan_of(&pool, "a1").await.2, Some(NOW + 100));
    }

    #[tokio::test]
    async fn cancel_at_period_end_marks_but_keeps_pro() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        sqlx::query(
            "UPDATE user SET plan = 'pro', planStatus = 'active', stripeCustomerId = 'cus_1', \
             stripeSubscriptionId = 'sub_1' WHERE id = 'a1'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let object = serde_json::json!({
            "id": "sub_1", "customer": "cus_1", "status": "canceled", "cancel_at_period_end": true,
        });
        apply(
            &pool,
            &sender,
            &billing,
            "evt_5",
            "customer.subscription.updated",
            object,
        )
        .await;
        assert_eq!(
            plan_of(&pool, "a1").await,
            ("pro".to_string(), Some("canceled".to_string()), None)
        );
    }

    #[tokio::test]
    async fn invoice_paid_restores_active_and_clears_grace() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        sqlx::query(
            "UPDATE user SET plan = 'pro', planStatus = 'past_due', graceUntil = ?1, \
             stripeCustomerId = 'cus_1' WHERE id = 'a1'",
        )
        .bind(NOW + 10)
        .execute(&pool)
        .await
        .unwrap();
        let object =
            serde_json::json!({"id": "in_1", "customer": "cus_1", "subscription": "sub_1"});
        apply(&pool, &sender, &billing, "evt_6", "invoice.paid", object).await;
        assert_eq!(
            plan_of(&pool, "a1").await,
            ("pro".to_string(), Some("active".to_string()), None)
        );
    }

    #[tokio::test]
    async fn payment_failed_starts_grace_and_mails_day_zero() {
        let pool = pool().await;
        let (sender, captured) = capture();
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        sqlx::query("UPDATE user SET stripeCustomerId = 'cus_1' WHERE id = 'a1'")
            .execute(&pool)
            .await
            .unwrap();
        let object =
            serde_json::json!({"id": "in_2", "customer": "cus_1", "subscription": "sub_7"});
        apply(
            &pool,
            &sender,
            &billing,
            "evt_7",
            "invoice.payment_failed",
            object,
        )
        .await;
        assert_eq!(
            plan_of(&pool, "a1").await,
            (
                "pro".to_string(),
                Some("past_due".to_string()),
                Some(NOW + GRACE_SECS)
            )
        );
        let mail = captured
            .lock()
            .unwrap()
            .get("a@example.com")
            .cloned()
            .unwrap();
        assert!(mail.contains("7 days"), "{mail}");
        assert!(
            mail.contains("https://outer.now/billing/checkout?t="),
            "{mail}"
        );
    }

    #[tokio::test]
    async fn unpaid_downgrades_immediately() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        sqlx::query(
            "UPDATE user SET plan = 'pro', planStatus = 'past_due', stripeSubscriptionId = 'sub_1' \
             WHERE id = 'a1'",
        )
        .execute(&pool)
        .await
        .unwrap();
        seed_app(&pool, "app1", "solo", "a1", 1).await;
        let object = serde_json::json!({"id": "sub_1", "customer": "cus_1", "status": "unpaid"});
        let outcome = apply(
            &pool,
            &sender,
            &billing,
            "evt_10",
            "customer.subscription.updated",
            object,
        )
        .await;
        assert!(matches!(outcome, ApplyOutcome::Applied { stopped, .. } if stopped.is_empty()));
        assert_eq!(
            plan_of(&pool, "a1").await,
            ("free".to_string(), Some("canceled".to_string()), None)
        );
    }

    #[tokio::test]
    async fn unknown_events_are_recorded_and_ignored() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        let outcome = apply(
            &pool,
            &sender,
            &billing,
            "evt_x",
            "customer.tax_id.created",
            serde_json::json!({}),
        )
        .await;
        assert!(matches!(
            outcome,
            ApplyOutcome::Applied {
                account_id: None,
                ..
            }
        ));
        assert_eq!(plan_of(&pool, "a1").await.0, "free");
    }

    #[tokio::test]
    async fn grace_sweep_reminds_once_then_downgrades() {
        let pool = pool().await;
        let (sender, captured) = capture();
        let billing = enabled_billing();
        let delete_cfg = test_delete_cfg(std::env::temp_dir());
        // Day-5 window: 47.5 h of grace left.
        seed_user(&pool, "a1", "a@example.com").await;
        sqlx::query(
            "UPDATE user SET plan = 'pro', planStatus = 'past_due', graceUntil = ?1 WHERE id = 'a1'",
        )
        .bind(NOW + 47 * 3600 + 1800)
        .execute(&pool)
        .await
        .unwrap();
        let (reminded, downgraded) = run_grace_sweep(&billing, &pool, &sender, &delete_cfg, NOW)
            .await
            .unwrap();
        assert_eq!((reminded, downgraded), (1, 0));
        assert_eq!(plan_of(&pool, "a1").await.0, "pro");
        let mail = captured
            .lock()
            .unwrap()
            .get("a@example.com")
            .cloned()
            .unwrap();
        assert!(mail.contains("2 days"), "{mail}");
        // Past expiry the next tick downgrades.
        sqlx::query("UPDATE user SET graceUntil = ?1 WHERE id = 'a1'")
            .bind(NOW - 1)
            .execute(&pool)
            .await
            .unwrap();
        seed_app(&pool, "app1", "solo", "a1", 1).await;
        let (reminded, downgraded) = run_grace_sweep(&billing, &pool, &sender, &delete_cfg, NOW)
            .await
            .unwrap();
        assert_eq!((reminded, downgraded), (0, 1));
        assert_eq!(plan_of(&pool, "a1").await.0, "free");
    }

    #[tokio::test]
    async fn grace_sweep_is_a_noop_when_disabled() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = BillingState::Disabled {
            platform_base: "https://outer.now".to_string(),
        };
        let delete_cfg = test_delete_cfg(std::env::temp_dir());
        seed_user(&pool, "a1", "a@example.com").await;
        sqlx::query("UPDATE user SET plan = 'pro', planStatus = 'past_due', graceUntil = ?1 WHERE id = 'a1'")
        .bind(NOW - 1)
        .execute(&pool)
        .await
        .unwrap();
        let (reminded, downgraded) = run_grace_sweep(&billing, &pool, &sender, &delete_cfg, NOW)
            .await
            .unwrap();
        assert_eq!((reminded, downgraded), (0, 0));
    }

    #[test]
    fn checkout_tokens_round_trip_and_expire() {
        let key = b"test-sign-key";
        let token = sign_checkout_token(key, "a1", NOW + CHECKOUT_TOKEN_SECS);
        assert_eq!(
            verify_checkout_token(key, &token, NOW),
            Some("a1".to_string())
        );
        // Tampered account does not verify.
        assert_eq!(
            verify_checkout_token(key, &token.replace("a1.", "a2."), NOW),
            None
        );
        // Expired tokens never verify.
        assert_eq!(
            verify_checkout_token(key, &token, NOW + CHECKOUT_TOKEN_SECS + 1),
            None
        );
    }

    #[test]
    fn upgrade_url_points_at_checkout_or_console() {
        let enabled = enabled_billing();
        let url = enabled.upgrade_url("a1", NOW);
        assert!(
            url.starts_with("https://outer.now/billing/checkout?t=a1."),
            "{url}"
        );
        let disabled = BillingState::Disabled {
            platform_base: "https://outer.now".to_string(),
        };
        assert_eq!(disabled.upgrade_url("a1", NOW), "https://outer.now/console");
    }

    #[tokio::test]
    async fn fifty_first_change_is_refused_with_an_upgrade_url() {
        use crate::mcp::tools::{change_limit, claim_change};
        let pool = pool().await;
        let billing = enabled_billing();
        seed_user(&pool, "a1", "a@example.com").await;
        let limit = change_limit("free") as usize;
        for _ in 0..limit {
            claim_change(&pool, &billing, "a1").await.unwrap();
        }
        let err = claim_change(&pool, &billing, "a1").await.unwrap_err();
        assert_eq!(err.code, "quota_exceeded");
        let url = err
            .extra
            .get("upgrade_url")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(
            url.starts_with("https://outer.now/billing/checkout?t=a1."),
            "{url}"
        );
        // The agent-relayed sentence names the upgrade too (spec §8.3).
        let user = err.user_message.unwrap();
        assert!(user.contains(url), "{user}");
        // Disabled billing points at the console plan card instead.
        let disabled = BillingState::Disabled {
            platform_base: "https://outer.now".to_string(),
        };
        let err = claim_change(&pool, &disabled, "a1").await.unwrap_err();
        assert_eq!(
            err.extra
                .get("upgrade_url")
                .and_then(serde_json::Value::as_str),
            Some("https://outer.now/console")
        );
    }

    #[test]
    fn signed_payload_form_matches_stripe_docs() {
        // The signed payload `t.payload` and `t=…,v1=…` header form from
        // <https://docs.stripe.com/webhooks/signature>.
        let payload = br#"{"id": "evt_test_webhook", "object": "event"}"#;
        let header = sign_test_header("whsec_test", payload, NOW);
        assert!(header.starts_with("t=1700000000,v1="));
        stripe::verify_signature("whsec_test", Some(&header), payload, NOW).unwrap();
    }

    async fn seed_pro_with_apps(pool: &SqlitePool) {
        seed_user(pool, "a1", "a@example.com").await;
        sqlx::query(
            "UPDATE user SET plan = 'pro', planStatus = 'active', stripeCustomerId = 'cus_1', \
             stripeSubscriptionId = 'sub_1' WHERE id = 'a1'",
        )
        .execute(pool)
        .await
        .unwrap();
        seed_app(pool, "app1", "old", "a1", 1).await;
        seed_app(pool, "app2", "new", "a1", 2).await;
    }

    async fn app_state_of(
        pool: &SqlitePool,
        id: &str,
    ) -> (String, String, Option<i64>, Option<i64>) {
        sqlx::query_as::<_, (String, String, Option<i64>, Option<i64>)>(
            "SELECT desired_state, visibility, downgraded_stopped_at, \
             downgrade_purge_warned_at FROM app WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn downgrade_strips_domains_and_public_visibility() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_pro_with_apps(&pool).await;
        // `new` is public with a custom hostname; `old` is unlisted.
        sqlx::query("UPDATE app SET visibility = 'public' WHERE id = 'app2'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO app_domain (hostname, app_id, cf_custom_hostname_id, status, created_at) \
             VALUES ('shop.example.com', 'app2', 'ch_1', 'active', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let object = serde_json::json!({"id": "sub_1", "customer": "cus_1", "status": "canceled"});
        let outcome = apply(
            &pool,
            &sender,
            &billing,
            "evt_20",
            "customer.subscription.deleted",
            object,
        )
        .await;
        // Oldest stays live, `new` stops.
        assert!(
            matches!(outcome, ApplyOutcome::Applied { stopped, .. } if stopped == vec!["new".to_string()])
        );
        assert_eq!(
            app_state_of(&pool, "app1").await,
            ("running".to_string(), "unlisted".to_string(), None, None),
            "the kept oldest app stays live outside retention"
        );
        assert_eq!(
            app_state_of(&pool, "app2").await,
            (
                "stopped".to_string(),
                "unlisted".to_string(),
                Some(NOW),
                None
            ),
            "public goes back to unlisted and the stop is stamped for retention"
        );
        let domains: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app_domain")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(domains, 0, "the downgrade deletes the custom hostname");
    }

    #[tokio::test]
    async fn downgrade_deletes_the_hostname_at_cloudflare() {
        let pool = pool().await;
        let (sender, _) = capture();
        let billing = enabled_billing();
        seed_pro_with_apps(&pool).await;
        sqlx::query(
            "INSERT INTO app_domain (hostname, app_id, cf_custom_hostname_id, status, created_at) \
             VALUES ('shop.example.com', 'app1', 'ch_9', 'active', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let fake = crate::cloudflare::fake_saas::FakeSaaS::start().await;
        let ctx = WebhookCtx {
            pool: &pool,
            sender: &sender,
            billing: &billing,
            saas: Some(fake.cf()),
            purge: None,
            apps_domain: "outer.now".to_string(),
        };
        let object = serde_json::json!({"id": "sub_1", "customer": "cus_1"});
        apply_subscription_deleted(&ctx, &object, NOW)
            .await
            .unwrap();
        assert_eq!(
            fake.calls("DELETE").len(),
            1,
            "the SaaS hostname delete goes out on downgrade, same as remove_domain"
        );
        let domains: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app_domain")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(domains, 0);
    }

    fn retention_deps(
        dir: &std::path::Path,
    ) -> (
        RetentionDeps,
        EmailSender,
        Arc<Mutex<HashMap<String, String>>>,
    ) {
        let (sender, mailed) = capture();
        let data_dir = dir.to_path_buf();
        let deps = RetentionDeps {
            store: crate::store::Store::memory(),
            git: crate::git::GitConfig::new(data_dir.join("git"), None),
            sync: crate::git::GitSync::default(),
            fleets: crate::fleet::manager::FleetManager::new(
                crate::fleet::manager::ManagerSettings {
                    data_dir: data_dir.clone(),
                    celld_bin: "celld".to_string(),
                    storage: crate::fleet::manager::FleetStorage {
                        endpoint: "http://127.0.0.1:1".to_string(),
                        region: "us-east-1".to_string(),
                        bucket: "outer-billing-test".to_string(),
                        credentials: crate::store::credentials::CredentialSource::Static {
                            access_key_id: "outeraccess".to_string(),
                            secret_access_key: "outersecretoutersecretoutersecret12".to_string(),
                        },
                    },
                    cgroups: None,
                    memory_max_bytes: 0,
                    cpu_max: "50000 100000".to_string(),
                    sleep_after: std::time::Duration::ZERO,
                    credential_refresh_after: std::time::Duration::from_secs(6 * 86400),
                    wake_timeout: std::time::Duration::from_secs(2),
                    wake_concurrency: 1,
                    always_on_max: 1,
                    preview_max: 1,
                },
            ),
            deploys: crate::mcp::Deploys::fake(crate::mcp::FakeOutcome::success()),
            rollback_locks: crate::mcp::RollbackLocks::default(),
            delete_cfg: test_delete_cfg(data_dir),
        };
        (deps, sender, mailed)
    }

    #[tokio::test]
    async fn downgrade_retention_warns_once_then_purges() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("retention.sqlite"))
            .await
            .unwrap();
        let billing = enabled_billing();
        let (deps, sender, mailed) = retention_deps(dir.path());
        seed_pro_with_apps(&pool).await;
        // Post-downgrade state: the account is Free, the stop is stamped.
        sqlx::query("UPDATE user SET plan = 'free' WHERE id = 'a1'")
            .execute(&pool)
            .await
            .unwrap();
        // Stopped 24 days ago: past the warning line, short of the purge.
        sqlx::query(
            "UPDATE app SET desired_state = 'stopped', downgraded_stopped_at = ?1 WHERE id = 'app2'",
        )
        .bind(NOW - 24 * 24 * 3600)
        .execute(&pool)
        .await
        .unwrap();
        let (warned, purged) =
            downgrade::run_downgrade_retention(&pool, &sender, &billing, &deps, NOW)
                .await
                .unwrap();
        assert_eq!((warned, purged), (1, 0));
        assert_eq!(app_state_of(&pool, "app2").await.3, Some(NOW));
        let mail = mailed
            .lock()
            .unwrap()
            .get("a@example.com")
            .cloned()
            .unwrap();
        assert!(mail.contains("new") && mail.contains("7 days"), "{mail}");
        // The warning fires only once.
        let (warned, purged) =
            downgrade::run_downgrade_retention(&pool, &sender, &billing, &deps, NOW + 1)
                .await
                .unwrap();
        assert_eq!((warned, purged), (0, 0));
        // Past day 30 the app is purged through the shared delete path.
        let (warned, purged) = downgrade::run_downgrade_retention(
            &pool,
            &sender,
            &billing,
            &deps,
            NOW + 7 * 24 * 3600,
        )
        .await
        .unwrap();
        assert_eq!((warned, purged), (0, 1));
        let missing: Option<String> = sqlx::query_scalar("SELECT slug FROM app WHERE id = 'app2'")
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert_eq!(missing, None);
        let quarantined: Option<i64> =
            sqlx::query_scalar("SELECT until FROM slug_quarantine WHERE slug = 'new'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert!(
            quarantined.is_some_and(|until| until > NOW + 7 * 24 * 3600),
            "the purge quarantines the slug like delete_app"
        );
    }

    #[tokio::test]
    async fn reupgrade_cancels_the_pending_purge() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("reupgrade.sqlite"))
            .await
            .unwrap();
        let billing = enabled_billing();
        let (deps, sender, _) = retention_deps(dir.path());
        seed_pro_with_apps(&pool).await;
        sqlx::query(
            "UPDATE app SET desired_state = 'stopped', downgraded_stopped_at = ?1, \
             downgrade_purge_warned_at = ?1 WHERE id = 'app2'",
        )
        .bind(NOW - 24 * 24 * 3600)
        .execute(&pool)
        .await
        .unwrap();
        // Resubscribe: the stamps clear and the retention pass stays quiet,
        // but the app is not restarted.
        let object = serde_json::json!({
            "id": "sub_2", "customer": "cus_1", "subscription": "sub_2",
            "customer_email": "a@example.com",
        });
        apply(
            &pool,
            &sender,
            &billing,
            "evt_21",
            "checkout.session.completed",
            object,
        )
        .await;
        assert_eq!(app_state_of(&pool, "app2").await.2, None);
        let (warned, purged) = downgrade::run_downgrade_retention(
            &pool,
            &sender,
            &billing,
            &deps,
            NOW + 40 * 24 * 3600,
        )
        .await
        .unwrap();
        assert_eq!((warned, purged), (0, 0));
        assert_eq!(
            app_state_of(&pool, "app2").await.0,
            "stopped",
            "re-upgrading restarts nothing"
        );
    }
}
