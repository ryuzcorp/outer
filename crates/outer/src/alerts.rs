//! Email alerts (wave 3, feature #2): the app owner (owner-only in v1)
//! gets a plain-text email when a deploy fails, when errors spike, and
//! when the daily code-change quota hits 80 % and 100 %.
//!
//! The three triggers run where they fit: deploy failures inline after
//! the pipeline result in [`crate::mcp::tools::run_deploy`], quota
//! thresholds inline after a successful `commit` claim, and spikes in a
//! [`spawn_sweep`] loop over [`error_issue`]. Every send is debounced
//! through `alert_send` and gated by the per-account `alert_prefs` row
//! (absent means all three on). Unsubscribe links carry an HMAC token so
//! one click opts out without a login.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use sqlx::SqlitePool;

use crate::apps::App;
use crate::auth::{now_ms, EmailSender};

/// Debounce: at most one deploy-failure email per app per hour.
pub const DEPLOY_DEBOUNCE_MS: i64 = 3_600_000;
/// Debounce: at most one error-spike email per app per 6 h.
pub const SPIKE_DEBOUNCE_MS: i64 = 6 * 3_600_000;
/// Debounce: at most one quota email per threshold per day.
pub const QUOTA_DEBOUNCE_MS: i64 = 24 * 3_600_000;
/// Error events counted as "recent" for the spike rules.
pub const SPIKE_WINDOW_MS: i64 = 15 * 60_000;
/// Burst rule: this many new error events in [`SPIKE_WINDOW_MS`].
pub const SPIKE_BURST_COUNT: i64 = 10;
/// Quiet-period rule: no error seen in this long before a new fingerprint.
pub const QUIET_MS: i64 = 24 * 3_600_000;

pub const KIND_DEPLOY_FAILED: &str = "deploy_failed";
pub const KIND_ERROR_SPIKE: &str = "error_spike";
pub const KIND_QUOTA_80: &str = "quota_80";
pub const KIND_QUOTA_100: &str = "quota_100";

/// Unsubscribe scopes: one alert kind, or everything at once.
pub const SCOPE_ALL: &str = "all";

/// Per-account toggles. Absence of a row means all three on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertPrefs {
    pub deploy_failed: bool,
    pub error_spike: bool,
    pub quota: bool,
}

impl Default for AlertPrefs {
    fn default() -> Self {
        Self {
            deploy_failed: true,
            error_spike: true,
            quota: true,
        }
    }
}

/// Partial update for the prefs endpoint: `None` leaves the column alone.
#[derive(Debug, Clone, Default)]
pub struct PrefsPatch {
    pub deploy_failed: Option<bool>,
    pub error_spike: Option<bool>,
    pub quota: Option<bool>,
}

/// Everything one alert send needs: explicit dependencies, no globals.
/// Call sites build it from their state (`McpState` or the sweep's owned
/// handles).
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub pool: &'a SqlitePool,
    pub sender: &'a EmailSender,
    pub issuer: &'a str,
    pub key: &'a [u8],
}

/// The toggles for `user_id`, defaulting to all-on when no row exists.
pub async fn get_prefs(pool: &SqlitePool, user_id: &str) -> anyhow::Result<AlertPrefs> {
    let row: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT deploy_failed, error_spike, quota FROM alert_prefs WHERE user_id = ?1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        None => AlertPrefs::default(),
        Some((deploy_failed, error_spike, quota)) => AlertPrefs {
            deploy_failed: deploy_failed != 0,
            error_spike: error_spike != 0,
            quota: quota != 0,
        },
    })
}

/// Upsert the provided toggles, leaving the rest (or the all-on default)
/// alone. Returns the effective row.
pub async fn set_prefs(
    pool: &SqlitePool,
    user_id: &str,
    patch: &PrefsPatch,
    now: i64,
) -> anyhow::Result<AlertPrefs> {
    let current = get_prefs(pool, user_id).await?;
    let next = AlertPrefs {
        deploy_failed: patch.deploy_failed.unwrap_or(current.deploy_failed),
        error_spike: patch.error_spike.unwrap_or(current.error_spike),
        quota: patch.quota.unwrap_or(current.quota),
    };
    sqlx::query(
        "INSERT INTO alert_prefs (user_id, deploy_failed, error_spike, quota, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(user_id) DO UPDATE SET deploy_failed = ?2, error_spike = ?3, quota = ?4, updated_at = ?5",
    )
    .bind(user_id)
    .bind(next.deploy_failed as i64)
    .bind(next.error_spike as i64)
    .bind(next.quota as i64)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(next)
}

/// True when no `kind` send to `subject` happened inside the window.
async fn should_send(
    pool: &SqlitePool,
    subject: &str,
    kind: &str,
    now: i64,
    window_ms: i64,
) -> anyhow::Result<bool> {
    let recent: Option<i64> = sqlx::query_scalar(
        "SELECT sent_at FROM alert_send WHERE subject = ?1 AND kind = ?2 AND sent_at > ?3 \
         ORDER BY sent_at DESC LIMIT 1",
    )
    .bind(subject)
    .bind(kind)
    .bind(now - window_ms)
    .fetch_optional(pool)
    .await?;
    Ok(recent.is_none())
}

async fn record_send(pool: &SqlitePool, subject: &str, kind: &str, now: i64) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO alert_send (subject, kind, sent_at) VALUES (?1, ?2, ?3)")
        .bind(subject)
        .bind(kind)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

async fn owner_email(pool: &SqlitePool, owner_id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT email FROM user WHERE id = ?1")
        .bind(owner_id)
        .fetch_optional(pool)
        .await?)
}

/// `https://outer.now/console?app={slug}&t={view}` (or the localhost
/// equivalent in dev): the console reads its view from `?t=`.
pub fn console_app_url(issuer: &str, slug: &str, view: &str) -> String {
    format!(
        "{}/console?app={}&t={}",
        issuer.trim_end_matches('/'),
        slug,
        view
    )
}

/// `https://outer.now/console?t={view}` for account-level pages.
pub fn console_url(issuer: &str, view: &str) -> String {
    format!("{}/console?t={}", issuer.trim_end_matches('/'), view)
}

pub fn unsubscribe_url(issuer: &str, token: &str) -> String {
    format!(
        "{}/console/alerts/unsubscribe?token={}",
        issuer.trim_end_matches('/'),
        token
    )
}

/// Sign a one-click unsubscribe token for `user_id` + `scope`
/// (`all` or one alert kind). HMAC-SHA256 over a domain-separated
/// message, so tokens from other features never verify here.
pub fn sign_unsubscribe(key: &[u8], user_id: &str, scope: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(format!("outer-alert-unsub-v1\n{user_id}\n{scope}").as_bytes());
    let tag = mac.finalize().into_bytes();
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "v1.{}.{}.{}",
        engine.encode(user_id.as_bytes()),
        scope,
        engine.encode(tag)
    )
}

/// The inverse of [`sign_unsubscribe`]: the `(user_id, scope)` the token
/// carries, or `None` when malformed, out-of-scope or forged.
pub fn verify_unsubscribe(key: &[u8], token: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = token.split('.').collect();
    let ["v1", user_b64, scope, tag_b64] = parts.as_slice() else {
        return None;
    };
    if !matches!(
        *scope,
        SCOPE_ALL | KIND_DEPLOY_FAILED | KIND_ERROR_SPIKE | "quota"
    ) {
        return None;
    }
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let user_bytes = engine.decode(user_b64).ok()?;
    let tag = engine.decode(tag_b64).ok()?;
    let user_id = String::from_utf8(user_bytes).ok()?;
    if user_id.is_empty() || user_id.len() > 200 {
        return None;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(format!("outer-alert-unsub-v1\n{user_id}\n{scope}").as_bytes());
    mac.verify_slice(&tag).ok()?;
    Some((user_id, scope.to_string()))
}

/// Turn `scope` off for `user_id` (one click, no login). `all` clears all
/// three toggles. Returns the effective row.
pub async fn apply_unsubscribe(
    pool: &SqlitePool,
    user_id: &str,
    scope: &str,
    now: i64,
) -> anyhow::Result<AlertPrefs> {
    let patch = match scope {
        SCOPE_ALL => PrefsPatch {
            deploy_failed: Some(false),
            error_spike: Some(false),
            quota: Some(false),
        },
        KIND_DEPLOY_FAILED => PrefsPatch {
            deploy_failed: Some(false),
            ..PrefsPatch::default()
        },
        KIND_ERROR_SPIKE => PrefsPatch {
            error_spike: Some(false),
            ..PrefsPatch::default()
        },
        _ => PrefsPatch {
            quota: Some(false),
            ..PrefsPatch::default()
        },
    };
    set_prefs(pool, user_id, &patch, now).await
}

fn truncate_middle(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    chars[..max_chars].iter().collect()
}

/// Send one alert, recording the debounce row only on success so a
/// failed send retries on the next trigger instead of going silent.
async fn send_alert(
    ctx: &Ctx<'_>,
    to: &str,
    subject: &str,
    body: &str,
    debounce_subject: &str,
    kind: &str,
    now: i64,
) {
    match ctx.sender.send_text(to, subject, body).await {
        Ok(()) => {
            if let Err(err) = record_send(ctx.pool, debounce_subject, kind, now).await {
                tracing::warn!(kind, error = %format!("{err:#}"), "alert send ledger insert failed");
            }
        }
        Err(err) => {
            tracing::warn!(kind, to, error = %err.to_string(), "alert email failed");
        }
    }
}

/// Email the app owner after the agent's commit produced a failed deploy.
/// At most one email per app per hour; never raises: a dead mailer must
/// not fail the deploy response.
pub async fn notify_deploy_failed(
    ctx: &Ctx<'_>,
    app: &App,
    failed_step: Option<&str>,
    error: Option<&str>,
    now: i64,
) {
    let prefs = match get_prefs(ctx.pool, &app.owner_id).await {
        Ok(prefs) => prefs,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "deploy alert prefs lookup failed");
            return;
        }
    };
    if !prefs.deploy_failed {
        return;
    }
    let debounced = match should_send(
        ctx.pool,
        &app.id,
        KIND_DEPLOY_FAILED,
        now,
        DEPLOY_DEBOUNCE_MS,
    )
    .await
    {
        Ok(debounced) => debounced,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "deploy alert debounce lookup failed");
            return;
        }
    };
    if !debounced {
        return;
    }
    let Some(to) = (match owner_email(ctx.pool, &app.owner_id).await {
        Ok(email) => email,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "deploy alert owner lookup failed");
            return;
        }
    }) else {
        return;
    };
    let step = failed_step.unwrap_or("unknown step");
    let detail = truncate_middle(error.unwrap_or("no detail recorded"), 500);
    let subject = format!("Deploy failed for {}", app.slug);
    let body = format!(
        "The latest deploy of {slug} failed at step \"{step}\":\n\n{detail}\n\n\
         See what happened:\n{console}\n\n\
         ---\n\
         You get this because deploy alerts are on for your account.\n\
         Turn off deploy emails:\n{unsub}\n\
         Manage all email alerts:\n{manage}\n",
        slug = app.slug,
        console = console_app_url(ctx.issuer, &app.slug, "deploys"),
        unsub = unsubscribe_url(
            ctx.issuer,
            &sign_unsubscribe(ctx.key, &app.owner_id, KIND_DEPLOY_FAILED)
        ),
        manage = console_url(ctx.issuer, "account"),
    );
    send_alert(ctx, &to, &subject, &body, &app.id, KIND_DEPLOY_FAILED, now).await;
}

/// Email the account when a claimed change crosses 80 % or 100 % of the
/// daily code-change quota. At most one email per threshold per day.
pub async fn notify_quota(ctx: &Ctx<'_>, user_id: &str, used: u64, limit: u64, now: i64) {
    if limit == 0 {
        return;
    }
    let kind = if used >= limit {
        KIND_QUOTA_100
    } else if used * 100 >= limit * 80 {
        KIND_QUOTA_80
    } else {
        return;
    };
    let prefs = match get_prefs(ctx.pool, user_id).await {
        Ok(prefs) => prefs,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "quota alert prefs lookup failed");
            return;
        }
    };
    if !prefs.quota {
        return;
    }
    let debounced = match should_send(ctx.pool, user_id, kind, now, QUOTA_DEBOUNCE_MS).await {
        Ok(debounced) => debounced,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "quota alert debounce lookup failed");
            return;
        }
    };
    if !debounced {
        return;
    }
    let Some(to) = (match owner_email(ctx.pool, user_id).await {
        Ok(email) => email,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "quota alert owner lookup failed");
            return;
        }
    }) else {
        return;
    };
    let (subject, line) = if kind == KIND_QUOTA_100 {
        (
            "Outer code-change quota reached (100%)".to_string(),
            format!(
                "You have used all {limit} of today's code changes. They reset at midnight UTC."
            ),
        )
    } else {
        (
            "Outer code-change quota at 80%".to_string(),
            format!(
                "You have used {used} of today's {limit} code changes. They reset at midnight UTC."
            ),
        )
    };
    let body = format!(
        "{line}\n\n\
         See your usage:\n{console}\n\n\
         ---\n\
         You get this because quota alerts are on for your account.\n\
         Turn off quota emails:\n{unsub}\n\
         Manage all email alerts:\n{manage}\n",
        console = console_url(ctx.issuer, "usage"),
        unsub = unsubscribe_url(ctx.issuer, &sign_unsubscribe(ctx.key, user_id, "quota")),
        manage = console_url(ctx.issuer, "account"),
    );
    send_alert(ctx, &to, &subject, &body, user_id, kind, now).await;
}

/// One app's spike verdict from [`error_issue`]: which rule fired, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpikeVerdict {
    /// [`SPIKE_BURST_COUNT`] or more new error events in
    /// [`SPIKE_WINDOW_MS`].
    Burst { events: i64 },
    /// A new fingerprint after [`QUIET_MS`] without errors.
    NewAfterQuiet { fingerprints: Vec<String> },
}

/// Evaluate the spike rules for one app from `error_issue`. Ignored
/// issues stay silent (ignoring is the owner's explicit opt-out); only
/// `open` issues count.
pub async fn spike_verdict(
    pool: &SqlitePool,
    app_id: &str,
    now: i64,
) -> anyhow::Result<Option<SpikeVerdict>> {
    // Burst: issues born inside the window carry (almost) all their
    // events inside it, so their summed count is the window's new events.
    let fresh_events: Option<i64> = sqlx::query_scalar(
        "SELECT SUM(count) FROM error_issue \
         WHERE app_id = ?1 AND status = 'open' AND first_seen >= ?2",
    )
    .bind(app_id)
    .bind(now - SPIKE_WINDOW_MS)
    .fetch_optional(pool)
    .await?
    .flatten();
    if fresh_events.unwrap_or(0) >= SPIKE_BURST_COUNT {
        return Ok(Some(SpikeVerdict::Burst {
            events: fresh_events.unwrap_or(0),
        }));
    }
    // Quiet period: a new fingerprint while nothing else fired recently.
    let fresh: Vec<String> = sqlx::query_scalar(
        "SELECT fingerprint FROM error_issue \
         WHERE app_id = ?1 AND status = 'open' AND first_seen >= ?2 \
         ORDER BY first_seen DESC LIMIT 5",
    )
    .bind(app_id)
    .bind(now - SPIKE_WINDOW_MS)
    .fetch_all(pool)
    .await?;
    if fresh.is_empty() {
        return Ok(None);
    }
    let recent_old: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM error_issue \
         WHERE app_id = ?1 AND status = 'open' AND first_seen < ?2 AND last_seen >= ?3",
    )
    .bind(app_id)
    .bind(now - SPIKE_WINDOW_MS)
    .bind(now - QUIET_MS)
    .fetch_optional(pool)
    .await?
    .flatten();
    if recent_old.unwrap_or(0) == 0 {
        return Ok(Some(SpikeVerdict::NewAfterQuiet {
            fingerprints: fresh,
        }));
    }
    Ok(None)
}

/// The sweep body, run every few minutes from [`spawn_sweep`]: one email
/// per app per 6 h when a spike rule fires. Never raises.
pub async fn check_error_spikes(ctx: &Ctx<'_>, now: i64) {
    let apps: Vec<(String, String, String)> =
        match sqlx::query_as("SELECT id, slug, owner_id FROM app ORDER BY slug")
            .fetch_all(ctx.pool)
            .await
        {
            Ok(apps) => apps,
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "spike sweep app list failed");
                return;
            }
        };
    for (app_id, slug, owner_id) in &apps {
        // Error state only changes on the sleep sweep's flush, so a
        // verdict needs re-checking only when recent issues exist.
        let recent: i64 = match sqlx::query_scalar(
            "SELECT COUNT(*) FROM error_issue \
             WHERE app_id = ?1 AND status = 'open' AND last_seen >= ?2",
        )
        .bind(app_id)
        .bind(now - SPIKE_WINDOW_MS)
        .fetch_one(ctx.pool)
        .await
        {
            Ok(count) => count,
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "spike sweep recency check failed");
                continue;
            }
        };
        if recent == 0 {
            continue;
        }
        let verdict = match spike_verdict(ctx.pool, app_id, now).await {
            Ok(verdict) => verdict,
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "spike verdict failed");
                continue;
            }
        };
        let Some(verdict) = verdict else { continue };
        let prefs = match get_prefs(ctx.pool, owner_id).await {
            Ok(prefs) => prefs,
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "spike prefs lookup failed");
                continue;
            }
        };
        if !prefs.error_spike {
            continue;
        }
        let debounced =
            match should_send(ctx.pool, app_id, KIND_ERROR_SPIKE, now, SPIKE_DEBOUNCE_MS).await {
                Ok(debounced) => debounced,
                Err(err) => {
                    tracing::warn!(error = %format!("{err:#}"), "spike debounce lookup failed");
                    continue;
                }
            };
        if !debounced {
            continue;
        }
        let owner = match owner_email(ctx.pool, owner_id).await {
            Ok(email) => email,
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "spike owner lookup failed");
                continue;
            }
        };
        let Some(to) = owner else { continue };
        let line = match &verdict {
            SpikeVerdict::Burst { events } => {
                format!(
                    "{events} new app errors in the last 15 minutes on {slug}.",
                    slug = slug
                )
            }
            SpikeVerdict::NewAfterQuiet { fingerprints } => {
                format!(
                    "new error{} on {} after a quiet day ({}).",
                    if fingerprints.len() == 1 { "" } else { "s" },
                    slug,
                    fingerprints.join(", ")
                )
            }
        };
        let subject = format!("Error spike on {slug}");
        let body = format!(
            "{line}\n\n\
             See the errors:\n{console}\n\n\
             ---\n\
             You get this because error alerts are on for your account.\n\
             Turn off error emails:\n{unsub}\n\
             Manage all email alerts:\n{manage}\n",
            console = console_app_url(ctx.issuer, slug, "errors"),
            unsub = unsubscribe_url(
                ctx.issuer,
                &sign_unsubscribe(ctx.key, owner_id, KIND_ERROR_SPIKE)
            ),
            manage = console_url(ctx.issuer, "account"),
        );
        send_alert(ctx, &to, &subject, &body, app_id, KIND_ERROR_SPIKE, now).await;
    }
}

/// Run [`check_error_spikes`] every `every`; each tick logs and continues.
pub fn spawn_sweep(
    pool: SqlitePool,
    sender: EmailSender,
    issuer: String,
    key: Vec<u8>,
    every: std::time::Duration,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        loop {
            interval.tick().await;
            let ctx = Ctx {
                pool: &pool,
                sender: &sender,
                issuer: &issuer,
                key: &key,
            };
            check_error_spikes(&ctx, now_ms()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::{test_config, test_pool};

    const KEY: &[u8] = b"outer-test-unsub-key";

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

    async fn seed_app(pool: &SqlitePool, id: &str, slug: &str, owner: &str) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(1);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let uid = 10000 + n;
        let port = 18100 + 10 * n;
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
                          last_activity_at, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0)",
        )
        .bind(id)
        .bind(slug)
        .bind(owner)
        .bind(uid as i64)
        .bind(port as i64)
        .bind((port + 1) as i64)
        .execute(pool)
        .await
        .unwrap();
    }

    fn test_app(id: &str, slug: &str, owner: &str) -> App {
        App {
            id: id.to_string(),
            slug: slug.to_string(),
            owner_id: owner.to_string(),
            name: None,
            uid: 10001,
            port: 18101,
            internal_port: 18102,
            visibility: crate::apps::Visibility::Unlisted,
            asleep_since: None,
            last_request_at: None,
            last_deploy_sha: None,
            created_at: 0,
        }
    }

    fn sent_to(
        codes: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
        email: &str,
    ) -> Option<String> {
        codes.lock().unwrap().get(email).cloned()
    }
    fn ctx<'a>(
        pool: &'a SqlitePool,
        sender: &'a crate::auth::EmailSender,
        issuer: &'a str,
    ) -> Ctx<'a> {
        Ctx {
            pool,
            sender,
            issuer,
            key: KEY,
        }
    }

    #[tokio::test]
    async fn deploy_failure_sends_once_per_hour_per_app() {
        let pool = test_pool().await;
        let (config, codes) = test_config();
        seed_user(&pool, "u1", "owner@example.com").await;
        let app = test_app("app1", "shop", "u1");
        let issuer = "http://outer.localhost:8080";

        let ctx = ctx(&pool, &config.email, issuer);
        notify_deploy_failed(&ctx, &app, Some("build"), Some("boom"), 1_000).await;
        let first = sent_to(&codes, "owner@example.com").expect("deploy failure emails the owner");
        assert!(
            first.contains("Deploy failed for shop"),
            "subject travels in the capture: {first}"
        );
        assert!(
            first.contains("/console?app=shop&t=deploys"),
            "links to the console view: {first}"
        );
        assert!(!first.contains("**"), "plain text, no markdown: {first}");

        // A repeat failure inside the hour stays silent …
        notify_deploy_failed(&ctx, &app, Some("build"), Some("boom again"), 2_000).await;
        assert!(
            !sent_to(&codes, "owner@example.com")
                .unwrap()
                .contains("boom again"),
            "debounced within the hour"
        );
        // … and sends again past the hour.
        notify_deploy_failed(
            &ctx,
            &app,
            Some("build"),
            Some("later"),
            DEPLOY_DEBOUNCE_MS + 2_000,
        )
        .await;
        assert!(sent_to(&codes, "owner@example.com")
            .unwrap()
            .contains("later"));
    }

    #[tokio::test]
    async fn deploy_failure_respects_the_toggle() {
        let pool = test_pool().await;
        let (config, codes) = test_config();
        seed_user(&pool, "u1", "owner@example.com").await;
        set_prefs(
            &pool,
            "u1",
            &PrefsPatch {
                deploy_failed: Some(false),
                ..PrefsPatch::default()
            },
            0,
        )
        .await
        .unwrap();
        let app = test_app("app1", "shop", "u1");
        let ctx = ctx(&pool, &config.email, "http://outer.localhost:8080");
        notify_deploy_failed(&ctx, &app, None, None, 1_000).await;
        assert_eq!(
            sent_to(&codes, "owner@example.com"),
            None,
            "toggled off: silent"
        );
    }

    #[tokio::test]
    async fn quota_thresholds_fire_once_per_day() {
        let pool = test_pool().await;
        let (config, codes) = test_config();
        seed_user(&pool, "u1", "owner@example.com").await;
        let issuer = "http://outer.localhost:8080";

        let ctx = ctx(&pool, &config.email, issuer);
        notify_quota(&ctx, "u1", 30, 50, 1_000).await;
        assert_eq!(
            sent_to(&codes, "owner@example.com"),
            None,
            "below 80%: silent"
        );

        notify_quota(&ctx, "u1", 40, 50, 2_000).await;
        let at_80 = sent_to(&codes, "owner@example.com").expect("80% emails");
        assert!(at_80.contains("80%"), "names the threshold: {at_80}");
        assert!(
            at_80.contains("/console?t=usage"),
            "links to usage: {at_80}"
        );

        notify_quota(&ctx, "u1", 45, 50, 3_000).await;
        assert!(
            !sent_to(&codes, "owner@example.com").unwrap().contains("45"),
            "80% debounced for the day"
        );

        notify_quota(&ctx, "u1", 50, 50, 4_000).await;
        let at_100 = sent_to(&codes, "owner@example.com").expect("100% emails");
        assert!(
            at_100.contains("100%"),
            "100% is its own threshold: {at_100}"
        );
    }

    #[tokio::test]
    async fn quota_respects_the_toggle() {
        let pool = test_pool().await;
        let (config, codes) = test_config();
        seed_user(&pool, "u1", "owner@example.com").await;
        set_prefs(
            &pool,
            "u1",
            &PrefsPatch {
                quota: Some(false),
                ..PrefsPatch::default()
            },
            0,
        )
        .await
        .unwrap();
        let ctx = ctx(&pool, &config.email, "http://outer.localhost:8080");
        notify_quota(&ctx, "u1", 50, 50, 1_000).await;
        assert_eq!(
            sent_to(&codes, "owner@example.com"),
            None,
            "toggled off: silent"
        );
    }

    async fn seed_issue(
        pool: &SqlitePool,
        app: &str,
        fp: &str,
        count: i64,
        first_seen: i64,
        last_seen: i64,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO error_issue (app_id, fingerprint, message, frame, count, \
                                  first_seen, last_seen, status) \
             VALUES (?1, ?2, 'Error: kaboom', 'at handler (x:1:2)', ?3, ?4, ?5, ?6)",
        )
        .bind(app)
        .bind(fp)
        .bind(count)
        .bind(first_seen)
        .bind(last_seen)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn spike_burst_rule_needs_ten_fresh_events() {
        let pool = test_pool().await;
        seed_user(&pool, "u1", "owner@example.com").await;
        seed_app(&pool, "app1", "shop", "u1").await;
        let now = 10_000_000;

        // Errors an hour ago keep this out of the quiet-period rule, so
        // only the burst bar matters here.
        seed_issue(
            &pool,
            "app1",
            "old",
            4,
            now - 2 * 3_600_000,
            now - 3_600_000,
            "open",
        )
        .await;
        seed_issue(&pool, "app1", "fp1", 5, now - 60_000, now - 60_000, "open").await;
        assert_eq!(
            spike_verdict(&pool, "app1", now).await.unwrap(),
            None,
            "five is quiet"
        );

        seed_issue(&pool, "app1", "fp2", 5, now - 60_000, now - 60_000, "open").await;
        assert_eq!(
            spike_verdict(&pool, "app1", now).await.unwrap(),
            Some(SpikeVerdict::Burst { events: 10 }),
            "ten fresh events trip the burst rule"
        );
    }

    #[tokio::test]
    async fn spike_new_fingerprint_after_quiet_day() {
        let pool = test_pool().await;
        seed_user(&pool, "u1", "owner@example.com").await;
        seed_app(&pool, "app1", "shop", "u1").await;
        let now = 100_000_000;

        // One stale, resolved-then-regressed-looking issue from two days
        // ago, and one fresh fingerprint now.
        seed_issue(
            &pool,
            "app1",
            "old",
            4,
            now - 2 * 86_400_000,
            now - 2 * 86_400_000,
            "open",
        )
        .await;
        seed_issue(&pool, "app1", "new", 1, now - 60_000, now - 60_000, "open").await;
        assert_eq!(
            spike_verdict(&pool, "app1", now).await.unwrap(),
            Some(SpikeVerdict::NewAfterQuiet {
                fingerprints: vec!["new".to_string()],
            }),
            "new fingerprint after a quiet day alerts"
        );
    }

    #[tokio::test]
    async fn spike_silent_when_errors_are_ongoing() {
        let pool = test_pool().await;
        seed_user(&pool, "u1", "owner@example.com").await;
        seed_app(&pool, "app1", "shop", "u1").await;
        let now = 100_000_000;

        // Errors an hour ago mean this is ongoing, not a new spike — and a
        // single fresh event is far below the burst bar.
        seed_issue(
            &pool,
            "app1",
            "old",
            4,
            now - 2 * 3_600_000,
            now - 3_600_000,
            "open",
        )
        .await;
        seed_issue(&pool, "app1", "new", 1, now - 60_000, now - 60_000, "open").await;
        assert_eq!(spike_verdict(&pool, "app1", now).await.unwrap(), None);
    }

    #[tokio::test]
    async fn spike_ignores_ignored_issues() {
        let pool = test_pool().await;
        seed_user(&pool, "u1", "owner@example.com").await;
        seed_app(&pool, "app1", "shop", "u1").await;
        let now = 100_000_000;

        seed_issue(
            &pool,
            "app1",
            "noisy",
            50,
            now - 60_000,
            now - 60_000,
            "ignored",
        )
        .await;
        assert_eq!(spike_verdict(&pool, "app1", now).await.unwrap(), None);
    }

    #[tokio::test]
    async fn spike_sweep_sends_and_debounces_per_app() {
        let pool = test_pool().await;
        let (config, codes) = test_config();
        seed_user(&pool, "u1", "owner@example.com").await;
        seed_app(&pool, "app1", "shop", "u1").await;
        seed_app(&pool, "app2", "blog", "u1").await;
        let now = 100_000_000;
        let issuer = "http://outer.localhost:8080";
        seed_issue(&pool, "app1", "fp1", 12, now - 60_000, now - 60_000, "open").await;

        let ctx = ctx(&pool, &config.email, issuer);
        check_error_spikes(&ctx, now).await;
        let mail = sent_to(&codes, "owner@example.com").expect("spike emails the owner");
        assert!(
            mail.contains("Error spike on shop"),
            "names the app: {mail}"
        );
        assert!(
            mail.contains("/console?app=shop&t=errors"),
            "links to errors: {mail}"
        );

        // Second tick inside 6 h: silent.
        seed_issue(&pool, "app1", "fp2", 12, now + 60_000, now + 60_000, "open").await;
        check_error_spikes(&ctx, now + 120_000).await;
        assert!(
            !sent_to(&codes, "owner@example.com")
                .unwrap()
                .contains("fp2"),
            "debounced per app per 6 h"
        );
    }

    #[tokio::test]
    async fn unsubscribe_token_round_trip() {
        let (user_id, scope) =
            verify_unsubscribe(KEY, &sign_unsubscribe(KEY, "u-9", SCOPE_ALL)).unwrap();
        assert_eq!((user_id.as_str(), scope.as_str()), ("u-9", SCOPE_ALL));

        // Tampered, wrong-key and out-of-scope tokens all fail.
        let mut bad = sign_unsubscribe(KEY, "u-9", KIND_DEPLOY_FAILED);
        bad.push('x');
        assert_eq!(verify_unsubscribe(KEY, &bad), None);
        assert_eq!(
            verify_unsubscribe(b"wrong-key", &sign_unsubscribe(KEY, "u-9", SCOPE_ALL)),
            None
        );
        assert_eq!(verify_unsubscribe(KEY, "v1.eA.evil.eA"), None);
        assert_eq!(verify_unsubscribe(KEY, "not-a-token"), None);
    }

    #[tokio::test]
    async fn unsubscribe_disables_only_its_scope() {
        let pool = test_pool().await;
        seed_user(&pool, "u1", "owner@example.com").await;

        let (user_id, scope) =
            verify_unsubscribe(KEY, &sign_unsubscribe(KEY, "u1", KIND_DEPLOY_FAILED)).unwrap();
        apply_unsubscribe(&pool, &user_id, &scope, 1_000)
            .await
            .unwrap();
        let prefs = get_prefs(&pool, "u1").await.unwrap();
        assert_eq!(
            prefs,
            AlertPrefs {
                deploy_failed: false,
                error_spike: true,
                quota: true,
            }
        );

        let (user_id, scope) =
            verify_unsubscribe(KEY, &sign_unsubscribe(KEY, "u1", SCOPE_ALL)).unwrap();
        apply_unsubscribe(&pool, &user_id, &scope, 2_000)
            .await
            .unwrap();
        assert_eq!(
            get_prefs(&pool, "u1").await.unwrap(),
            AlertPrefs {
                deploy_failed: false,
                error_spike: false,
                quota: false,
            }
        );
    }

    #[tokio::test]
    async fn prefs_default_on_and_patch_is_partial() {
        let pool = test_pool().await;
        seed_user(&pool, "u1", "owner@example.com").await;
        assert_eq!(get_prefs(&pool, "u1").await.unwrap(), AlertPrefs::default());
        let next = set_prefs(
            &pool,
            "u1",
            &PrefsPatch {
                quota: Some(false),
                ..PrefsPatch::default()
            },
            1_000,
        )
        .await
        .unwrap();
        assert!(!next.quota && next.deploy_failed && next.error_spike);
        assert_eq!(get_prefs(&pool, "u1").await.unwrap(), next);
    }
}
