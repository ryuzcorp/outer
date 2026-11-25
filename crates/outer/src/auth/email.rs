//! Sign-in codes by email through Cloudflare Email Service.
//!
//! The sender reuses the `CLOUDFLARE_ACCOUNT_ID` + `CLOUDFLARE_API_TOKEN`
//! pair the config already reads: no extra secret. On localhost without the
//! pair the code is logged at INFO instead of emailed. Tests capture codes
//! in memory behind `#[cfg(test)]`.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::{Arc, Mutex};

/// `Outer <hello@outer.now>`: the display name plus the sending address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailFrom {
    pub name: String,
    pub address: String,
}

impl EmailFrom {
    /// Parse `Name <addr>` or a bare address. A bare address takes the
    /// local part as the display name.
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if let Some((name, rest)) = raw.split_once('<') {
            let name = name.trim().trim_matches('"').to_string();
            let address = rest.trim_end_matches('>').trim().to_string();
            if address.is_empty() {
                return Self::bare(raw);
            }
            let name = if name.is_empty() {
                address.clone()
            } else {
                name
            };
            Self { name, address }
        } else {
            Self::bare(raw)
        }
    }

    fn bare(raw: &str) -> Self {
        let address = raw.to_string();
        let name = raw
            .split('@')
            .next()
            .filter(|local| !local.is_empty())
            .unwrap_or(raw)
            .to_string();
        Self { name, address }
    }
}

/// The Cloudflare Email Service plan allows 3,000 sends per calendar month
/// (UTC). [`EMAIL_MONTHLY_CAP`] is that cap; the counter lives in the
/// `email_month` table (`month "YYYY-MM"`, `sent`), and the ops sweep alerts
/// at 80 % and 95 % ([`crate::ops_alerts`]). At the cap every send refuses
/// with [`EmailError::BudgetExhausted`] — including sign-in codes, which flow
/// untouched until the hard cap — and resumes next month.
pub const EMAIL_MONTHLY_CAP: u64 = 3_000;

/// The `email_month.month` for `now_ms`: the UTC calendar month (`"2026-11"`).
pub fn month_key(now_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(now_ms)
        .map(|t| t.format("%Y-%m").to_string())
        .unwrap_or_else(|| "1970-01".to_string())
}

/// Sends recorded for `month` (UTC `"YYYY-MM"`). Missing rows read as zero.
pub async fn sent_in_month(pool: &sqlx::SqlitePool, month: &str) -> u64 {
    sqlx::query_scalar::<_, i64>("SELECT sent FROM email_month WHERE month = ?1")
        .bind(month)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
        .max(0) as u64
}

/// Record one send in `month`. Best effort: a ledger failure never fails the
/// send it counts (the send already landed).
pub async fn record_sent(pool: &sqlx::SqlitePool, month: &str) {
    let _ = sqlx::query(
        "INSERT INTO email_month (month, sent) VALUES (?1, 1) \
         ON CONFLICT(month) DO UPDATE SET sent = sent + 1",
    )
    .bind(month)
    .execute(pool)
    .await;
}

/// Seconds until the next UTC month, for a budget refusal's `retry_after_s`:
/// the cap lifts on the first of the month, not at midnight.
pub fn secs_until_next_month_utc() -> u64 {
    use chrono::Datelike as _;
    let now = chrono::Utc::now();
    let date = now.date_naive();
    let first_next = if date.month() == 12 {
        chrono::NaiveDate::from_ymd_opt(date.year() + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1)
    };
    first_next
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| t.and_utc())
        .map(|midnight| (midnight - now).num_seconds().max(1) as u64)
        .unwrap_or(60)
}

/// How one send failed. 429 (Cloudflare code 10004) is retryable; anything
/// else, including a permanent bounce, is not. The API token never appears
/// in any variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailError {
    Retryable(String),
    Permanent(String),
    /// The month's budget is spent: no send until the next UTC month.
    /// Refuse with a retry-later message, never silently drop — the caller
    /// (sign-in included) tells the human when to retry.
    BudgetExhausted {
        month: String,
        sent: u64,
        cap: u64,
    },
}

impl std::fmt::Display for EmailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retryable(msg) | Self::Permanent(msg) => write!(f, "{msg}"),
            Self::BudgetExhausted { month, sent, cap } => write!(
                f,
                "email budget exhausted for {month} ({sent}/{cap} sent); try again next month"
            ),
        }
    }
}

impl std::error::Error for EmailError {}

/// The code sender. `Cloudflare` hits the Email Sending REST API;
/// `Log` prints the code for localhost dev; `Capture` keeps codes in
/// memory for deterministic tests.
#[derive(Clone)]
pub enum EmailSender {
    Cloudflare {
        client: reqwest::Client,
        api_base: String,
        account_id: String,
        api_token: String,
        from: EmailFrom,
        /// Operator-alerting failure hook (L-24): `None` unless the boot
        /// wired `with_ops_hook`. Only the Cloudflare transport reports;
        /// the localhost log and test capture senders have no provider.
        ops_hook: Option<crate::ops_alerts::OpsMailHook>,
        /// Monthly send budget (L-22): `None` unless the boot wired
        /// `with_budget`. Only the Cloudflare transport counts; the
        /// localhost log and test capture senders cost nothing.
        budget: Option<sqlx::SqlitePool>,
    },
    Log,
    #[cfg(test)]
    Capture {
        codes: Arc<Mutex<HashMap<String, String>>>,
    },
}

impl std::fmt::Debug for EmailSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cloudflare { from, ops_hook, .. } => f
                .debug_struct("Cloudflare")
                .field("from", from)
                .field("ops_hook", &ops_hook.is_some())
                .finish(),
            Self::Log => write!(f, "Log"),
            #[cfg(test)]
            Self::Capture { .. } => write!(f, "Capture"),
        }
    }
}

impl EmailSender {
    /// The production sender: Cloudflare Email Service on
    /// `https://api.cloudflare.com`.
    pub fn cloudflare(account_id: String, api_token: String, from: EmailFrom) -> Self {
        Self::Cloudflare {
            client: reqwest::Client::new(),
            api_base: "https://api.cloudflare.com".to_string(),
            account_id,
            api_token,
            from,
            ops_hook: None,
            budget: None,
        }
    }

    /// Record every Cloudflare send in the operator heartbeat table
    /// (L-24): successes clear the email streak, failures extend it. Wired
    /// once at boot; every clone of the sender carries it.
    pub fn with_ops_hook(self, pool: sqlx::SqlitePool) -> Self {
        match self {
            Self::Cloudflare {
                client,
                api_base,
                account_id,
                api_token,
                from,
                budget,
                ..
            } => Self::Cloudflare {
                client,
                api_base,
                account_id,
                api_token,
                from,
                ops_hook: Some(crate::ops_alerts::OpsMailHook { pool }),
                budget,
            },
            other => other,
        }
    }
    /// Count every Cloudflare send against the monthly plan budget (L-22)
    /// and refuse past the cap with [`EmailError::BudgetExhausted`]. Wired
    /// once at boot next to [`Self::with_ops_hook`]; every clone of the
    /// sender carries it. Sign-in codes flow untouched until the hard cap.
    pub fn with_budget(self, pool: sqlx::SqlitePool) -> Self {
        match self {
            Self::Cloudflare {
                client,
                api_base,
                account_id,
                api_token,
                from,
                ops_hook,
                ..
            } => Self::Cloudflare {
                client,
                api_base,
                account_id,
                api_token,
                from,
                ops_hook,
                budget: Some(pool),
            },
            other => other,
        }
    }

    /// Point the sender at a fake API base (tests only).
    #[cfg(test)]
    pub fn with_api_base(self, api_base: String) -> Self {
        match self {
            Self::Cloudflare {
                client,
                account_id,
                api_token,
                from,
                ops_hook,
                budget,
                ..
            } => Self::Cloudflare {
                client,
                api_base,
                account_id,
                api_token,
                from,
                ops_hook,
                budget,
            },
            other => other,
        }
    }

    /// Send one sign-in code. The code itself goes in the body; failures
    /// name the reason, never the token. The test sender captures the bare
    /// code (existing sign-in tests verify with it), while [`send_text`]
    /// captures subject plus body.
    pub async fn send_code(&self, to: &str, code: &str) -> Result<(), EmailError> {
        let subject = format!("Your Outer sign-in code is {code}");
        let text = format!(
            "Your Outer sign-in code is {code}. It expires in 10 minutes.\n\nIf you didn't ask for this, ignore this email."
        );
        match self {
            Self::Cloudflare { .. } => self.post(to, &subject, &text, &[]).await,
            Self::Log => {
                tracing::info!(email = %to, code = %code, "sign-in code (localhost log sender)");
                Ok(())
            }
            #[cfg(test)]
            Self::Capture { codes } => {
                codes
                    .lock()
                    .unwrap()
                    .insert(to.to_string(), code.to_string());
                Ok(())
            }
        }
    }

    /// Send one free-form message (collaborator invites). Same transport,
    /// same failure contract as [`send_code`].
    pub async fn send_text(&self, to: &str, subject: &str, text: &str) -> Result<(), EmailError> {
        self.send_text_with_headers(to, subject, text, &[]).await
    }

    /// [`send_text`] plus extra RFC headers. The Cloudflare Email Sending
    /// REST API accepts them as a `headers` object (used for
    /// `List-Unsubscribe` / `List-Unsubscribe-Post` on alert emails); the
    /// test capture and localhost log senders record them in the stored
    /// text.
    pub async fn send_text_with_headers(
        &self,
        to: &str,
        subject: &str,
        text: &str,
        headers: &[(&str, String)],
    ) -> Result<(), EmailError> {
        match self {
            Self::Cloudflare { .. } => self.post(to, subject, text, headers).await,
            Self::Log => {
                tracing::info!(email = %to, subject = %subject, "outbound email (localhost log sender)");
                Ok(())
            }
            #[cfg(test)]
            Self::Capture { codes } => {
                let mut stored = format!("{subject}\n\n{text}");
                for (name, value) in headers {
                    stored.push_str(&format!("\n{name}: {value}"));
                }
                codes.lock().unwrap().insert(to.to_string(), stored);
                Ok(())
            }
        }
    }

    /// The Cloudflare Email Sending POST behind [`send_code`] and
    /// [`send_text`]. Extra headers travel as the `headers` object the
    /// REST API documents for list management (`List-Unsubscribe`,
    /// `List-Unsubscribe-Post`); the object is omitted when empty.
    async fn post(
        &self,
        to: &str,
        subject: &str,
        text: &str,
        headers: &[(&str, String)],
    ) -> Result<(), EmailError> {
        let Self::Cloudflare {
            client,
            api_base,
            account_id,
            api_token,
            from,
            ops_hook,
            budget,
        } = self
        else {
            unreachable!("post is only called on the Cloudflare sender");
        };
        // The operator heartbeat hook (L-24), if the boot wired one: every
        // provider failure extends the `ops_email` streak, every success
        // clears it. Per-recipient bounces stay out (not a provider
        // outage), and recording never fails the send. Inline, not
        // spawned: callers observe the row when the send returns.
        let ops_pool = ops_hook.as_ref().map(|hook| hook.pool.clone());
        // Monthly plan budget (L-22): refuse before sending when this UTC
        // month is spent, so Cloudflare never bills over the 3,000 cap.
        // Warnings at 80 % / 95 % (ops sweep) never block; only the hard cap
        // refuses, sign-in codes included. A ledger lookup failure reads as
        // zero (fail open): sign-in must keep flowing through a local DB
        // hiccup, and the 429 path below still bounds real overshoot.
        let budget_pool = budget.clone();
        if let Some(pool) = &budget_pool {
            let month = month_key(crate::auth::now_ms());
            let sent = sent_in_month(pool, &month).await;
            if sent >= EMAIL_MONTHLY_CAP {
                return Err(EmailError::BudgetExhausted {
                    month,
                    sent,
                    cap: EMAIL_MONTHLY_CAP,
                });
            }
        }
        let url = format!("{api_base}/client/v4/accounts/{account_id}/email/sending/send");
        let mut payload = serde_json::json!({
            "to": to,
            "from": { "address": from.address, "name": from.name },
            "subject": subject,
            "text": text,
        });
        if !headers.is_empty() {
            payload["headers"] = headers
                .iter()
                .map(|(name, value)| (name.to_string(), serde_json::Value::String(value.clone())))
                .collect::<serde_json::Map<_, _>>()
                .into();
        }
        let response = match client
            .post(&url)
            .bearer_auth(api_token)
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                let err = EmailError::Retryable(scrubbed(&err.to_string()));
                if let Some(pool) = &ops_pool {
                    crate::ops_alerts::record_heartbeat_err(
                        pool,
                        crate::ops_alerts::KIND_EMAIL,
                        crate::auth::now_ms(),
                        &err.to_string(),
                    )
                    .await;
                }
                return Err(err);
            }
        };
        let status = response.status();
        let body: serde_json::Value = match response.json().await {
            Ok(body) => body,
            Err(err) => {
                let err =
                    EmailError::Retryable(format!("email API returned an unreadable body: {err}"));
                if let Some(pool) = &ops_pool {
                    crate::ops_alerts::record_heartbeat_err(
                        pool,
                        crate::ops_alerts::KIND_EMAIL,
                        crate::auth::now_ms(),
                        &err.to_string(),
                    )
                    .await;
                }
                return Err(err);
            }
        };
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || error_code(&body) == Some(10004) {
            let err = EmailError::Retryable(
                "email provider is rate limited, try again shortly".to_string(),
            );
            if let Some(pool) = &ops_pool {
                crate::ops_alerts::record_heartbeat_err(
                    pool,
                    crate::ops_alerts::KIND_EMAIL,
                    crate::auth::now_ms(),
                    &err.to_string(),
                )
                .await;
            }
            return Err(err);
        }
        if !status.is_success() || body.get("success").and_then(|v| v.as_bool()) != Some(true) {
            let detail = first_error(&body).unwrap_or_else(|| status.to_string());
            let err =
                EmailError::Permanent(format!("email provider refused the message ({detail})"));
            if let Some(pool) = &ops_pool {
                crate::ops_alerts::record_heartbeat_err(
                    pool,
                    crate::ops_alerts::KIND_EMAIL,
                    crate::auth::now_ms(),
                    &err.to_string(),
                )
                .await;
            }
            return Err(err);
        }
        if permanent_bounce(&body, to) {
            return Err(EmailError::Permanent(format!(
                "email to {to} permanently bounced"
            )));
        }
        if let Some(pool) = ops_pool {
            crate::ops_alerts::record_heartbeat_ok(
                &pool,
                crate::ops_alerts::KIND_EMAIL,
                crate::auth::now_ms(),
            )
            .await;
        }
        // The send landed: count it against this UTC month (best effort, like
        // the heartbeat above). Bounces count — they were sent.
        if let Some(pool) = &budget_pool {
            record_sent(pool, &month_key(crate::auth::now_ms())).await;
        }
        Ok(())
    }
}

/// The recipient counts as permanently bounced when it appears in
/// `result.permanent_bounces` or `result.suppressed_recipients`.
fn permanent_bounce(body: &serde_json::Value, to: &str) -> bool {
    let result = body.get("result");
    ["permanent_bounces", "suppressed_recipients"]
        .iter()
        .any(|key| {
            result
                .and_then(|result| result.get(*key))
                .and_then(|list| list.as_array())
                .is_some_and(|list| {
                    list.iter().any(|entry| {
                        entry.as_str() == Some(to)
                            || entry.get("address").and_then(|a| a.as_str()) == Some(to)
                            || entry.get("email").and_then(|a| a.as_str()) == Some(to)
                    })
                })
        })
}

/// First `errors[0]` as `code:message`, if the API answered `success:false`.
fn first_error(body: &serde_json::Value) -> Option<String> {
    body.get("errors")
        .and_then(|errors| errors.as_array())
        .and_then(|errors| errors.first())
        .map(|err| match err.get("code").and_then(|code| code.as_u64()) {
            Some(code) => format!(
                "{code}:{}",
                err.get("message").and_then(|m| m.as_str()).unwrap_or("?")
            ),
            None => err.to_string(),
        })
}

fn error_code(body: &serde_json::Value) -> Option<u64> {
    body.get("errors")
        .and_then(|errors| errors.as_array())
        .and_then(|errors| errors.first())
        .and_then(|err| err.get("code"))
        .and_then(|code| code.as_u64())
}

/// A transport error must never carry the bearer token back to the caller.
fn scrubbed(message: &str) -> String {
    if message.len() > 200 {
        format!("email send failed ({})", &message[..200])
    } else {
        format!("email send failed ({message})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parses_named_and_bare_forms() {
        let named = EmailFrom::parse("Outer <hello@outer.now>");
        assert_eq!(
            named,
            EmailFrom {
                name: "Outer".to_string(),
                address: "hello@outer.now".to_string(),
            }
        );
        let bare = EmailFrom::parse("hello@outer.now");
        assert_eq!(bare.address, "hello@outer.now");
        assert_eq!(bare.name, "hello");
    }

    #[test]
    fn permanent_bounce_matches_string_and_object_entries() {
        let body = serde_json::json!({ "result": { "permanent_bounces": ["ana@example.com"] } });
        assert!(permanent_bounce(&body, "ana@example.com"));
        assert!(!permanent_bounce(&body, "bob@example.com"));
        let body = serde_json::json!({ "result": { "suppressed_recipients": [{"address": "ana@example.com"}] } });
        assert!(permanent_bounce(&body, "ana@example.com"));
        let body = serde_json::json!({ "result": { "delivered": ["ana@example.com"] } });
        assert!(!permanent_bounce(&body, "ana@example.com"));
    }

    #[test]
    fn month_key_uses_utc_calendar_month() {
        let nov = chrono::DateTime::parse_from_rfc3339("2026-11-30T23:59:59Z")
            .unwrap()
            .timestamp_millis();
        let dec = chrono::DateTime::parse_from_rfc3339("2026-12-01T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(month_key(nov), "2026-11");
        assert_eq!(month_key(dec), "2026-12");
        assert_eq!(month_key(0), "1970-01");
    }

    #[tokio::test]
    async fn sent_in_month_counts_and_starts_at_zero() {
        let pool = crate::auth::test_support::test_pool().await;
        assert_eq!(sent_in_month(&pool, "2026-11").await, 0);
        record_sent(&pool, "2026-11").await;
        record_sent(&pool, "2026-11").await;
        record_sent(&pool, "2026-12").await;
        assert_eq!(sent_in_month(&pool, "2026-11").await, 2);
        assert_eq!(sent_in_month(&pool, "2026-12").await, 1);
    }

    #[tokio::test]
    async fn cloudflare_sender_counts_success_and_refuses_past_the_cap() {
        use std::sync::{Arc, Mutex};
        let delivered: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let route_delivered = delivered.clone();
        let app = axum::Router::new().route(
            "/client/v4/accounts/{account_id}/email/sending/send",
            axum::routing::post(
                move |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    route_delivered
                        .lock()
                        .unwrap()
                        .push(body["to"].as_str().unwrap_or("").to_string());
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "success": true, "errors": [],
                            "result": { "delivered": ["x"], "permanent_bounces": [], "queued": [] },
                        })),
                    )
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        let pool = crate::auth::test_support::test_pool().await;
        let sender = EmailSender::cloudflare(
            "acct-1".to_string(),
            "cf-secret-token".to_string(),
            EmailFrom::parse("Outer <hello@outer.now>"),
        )
        .with_api_base(base)
        .with_budget(pool.clone());
        sender.send_text("ana@example.com", "s", "b").await.unwrap();
        let month = month_key(crate::auth::now_ms());
        assert_eq!(sent_in_month(&pool, &month).await, 1);
        assert_eq!(delivered.lock().unwrap().len(), 1);
        // At the cap the send refuses before any HTTP, with a retry-next-month
        // message: the current month keeps its count and the fake sees nothing.
        sqlx::query("UPDATE email_month SET sent = ?1 WHERE month = ?2")
            .bind(EMAIL_MONTHLY_CAP as i64)
            .bind(&month)
            .execute(&pool)
            .await
            .unwrap();
        let err = sender
            .send_text("bob@example.com", "s", "b")
            .await
            .expect_err("the capped month refuses");
        let msg = err.to_string();
        assert!(msg.contains("try again next month"), "{msg}");
        assert!(
            matches!(
                err,
                EmailError::BudgetExhausted { sent, cap, .. }
                if sent == EMAIL_MONTHLY_CAP && cap == EMAIL_MONTHLY_CAP
            ),
            "{msg}"
        );
        assert_eq!(delivered.lock().unwrap().len(), 1, "no HTTP past the cap");
        assert_eq!(sent_in_month(&pool, &month).await, EMAIL_MONTHLY_CAP);
        // The localhost log sender is unwired and stays unlimited.
        EmailSender::Log
            .send_text("x@example.com", "s", "b")
            .await
            .unwrap();
        server.abort();
    }
}
