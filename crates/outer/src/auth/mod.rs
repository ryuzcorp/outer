//! Accounts, sign-in (OAuth 2.1 + email OTP) and API keys.
//!
//! This module owns `account`, `beta_allowlist`, `waitlist`, `email_code`,
//! `api_key`, `oauth_*` and `web_session`. The MCP handler only calls the
//! functions below; names and shapes are binding.

pub mod cimd;
pub mod email;
pub mod limits;
pub mod oauth;
pub mod pages;
pub mod tokens;

use std::time::Duration;

use axum::http::HeaderValue;
use sqlx::SqlitePool;

pub use email::{EmailFrom, EmailSender};
pub use oauth::router;

pub use tokens::{ACCESS_PREFIX, API_KEY_PREFIX, REFRESH_PREFIX};

/// Milliseconds of wall clock. Tests set row timestamps directly instead of
/// sleeping, so every expiry test stays deterministic.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Built once in main.rs from config.rs; passed explicitly (no globals).
#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// `https://outer.now` (dev: `http://outer.localhost:8080`).
    pub issuer: String,
    /// Issuer + `/mcp`.
    pub resource: String,
    /// `OUTER_ADMIN_EMAILS`, normalized.
    pub admin_emails: Vec<String>,
    /// `OUTER_SIGNUP=open|closed` (default closed).
    pub signup_open: bool,
    /// `OUTER_TERMS_VERSION`.
    pub terms_version: String,
    /// `OUTER_OAUTH_ACCESS_TTL_S` (3600).
    pub access_ttl: Duration,
    /// `OUTER_OAUTH_REFRESH_TTL_D` (90 d).
    pub refresh_ttl: Duration,
    /// Cloudflare Email Service, or log-only on localhost without the pair.
    pub email: EmailSender,
    pub is_local: bool,
    /// CIMD fetches allow loopback/client images (tests only, never prod).
    pub allow_loopback_fetch: bool,
    /// Behind Cloudflare origin pull: rate limits key on `CF-Connecting-IP`,
    /// else on the TCP peer.
    pub origin_locked: bool,
}

/// Axum state for [`router`].
#[derive(Clone)]
pub struct AuthState {
    pub pool: SqlitePool,
    pub config: AuthConfig,
}

pub struct Principal {
    pub account_id: String,
    pub email: String,
    pub is_admin: bool,
    pub credential: Credential,
}

pub enum Credential {
    ApiKey { key_id: String },
    OAuth { grant_id: String },
}

/// Domain error a tool returns as `isError` (spec §8.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthError {
    pub code: &'static str,
    pub message: String,
    pub user_message: Option<String>,
    pub retry_after_s: Option<u64>,
}

impl AuthError {
    fn new(code: &'static str, message: String) -> Self {
        Self {
            code,
            message,
            user_message: None,
            retry_after_s: None,
        }
    }

    fn rate_limited(retry_after_s: u64) -> Self {
        Self {
            code: "rate_limited",
            message: format!("too many attempts, try again in {retry_after_s}s"),
            user_message: Some(format!("Too many attempts. Try again in {retry_after_s}s.")),
            retry_after_s: Some(retry_after_s),
        }
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AuthError {}

#[derive(Debug)]
pub struct SignInSent {
    pub expires_in_s: u64,
}

#[derive(Debug)]
pub struct Verified {
    pub api_key: String,
    pub account_id: String,
    pub email: String,
}

#[derive(Debug)]
pub struct NewApiKey {
    pub id: String,
    pub prefix: String,
    pub key: String,
}

#[derive(Debug)]
pub struct ApiKeyInfo {
    pub id: String,
    pub name: Option<String>,
    pub prefix: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub client_name: Option<String>,
    pub client_version: Option<String>,
}

#[derive(Debug)]
pub struct ConnectionInfo {
    pub grant_id: String,
    pub client_id: String,
    pub client_name: Option<String>,
    pub client_host: Option<String>,
    pub scope: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}
/// Lowercase and trim. Emails compare normalized everywhere: allowlist,
/// waitlist, codes and accounts.
pub fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

fn valid_email(email: &str) -> bool {
    let parts = email.split('@').collect::<Vec<_>>();
    parts.len() == 2 && !parts[0].is_empty() && parts[1].contains('.') && !parts[1].is_empty()
}

pub fn is_admin(cfg: &AuthConfig, email: &str) -> bool {
    cfg.admin_emails.iter().any(|admin| admin == email)
}

pub struct AccountRow {
    pub id: String,
    pub email: String,
    pub terms_version: Option<String>,
    pub banned_at: Option<i64>,
}

impl AccountRow {
    /// The account accepted the current terms version.
    pub fn terms_accepted(&self, cfg: &AuthConfig) -> bool {
        self.terms_version.as_deref() == Some(cfg.terms_version.as_str())
    }
}

async fn account_by_email(pool: &SqlitePool, email: &str) -> anyhow::Result<Option<AccountRow>> {
    let row: Option<(String, String, Option<String>, Option<i64>)> =
        sqlx::query_as("SELECT id, email, terms_version, banned_at FROM account WHERE email = ?")
            .bind(email)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id, email, terms_version, banned_at)| AccountRow {
        id,
        email,
        terms_version,
        banned_at,
    }))
}

/// Closed-beta gate: open signup, admin emails and allowlisted emails pass;
/// anyone else lands on the waitlist (counted) with the exact closed-beta
/// user message. Idempotent: repeat sends bump the waitlist count.
async fn check_beta(
    pool: &SqlitePool,
    cfg: &AuthConfig,
    email: &str,
    now: i64,
) -> Result<(), AuthError> {
    if cfg.signup_open || is_admin(cfg, email) {
        return Ok(());
    }
    let allowed: Option<(String,)> =
        sqlx::query_as("SELECT email FROM beta_allowlist WHERE email = ?")
            .bind(email)
            .fetch_optional(pool)
            .await
            .map_err(|err| AuthError::new("db", format!("allowlist lookup failed: {err}")))?;
    if allowed.is_some() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO waitlist (email, first_seen, attempts) VALUES (?, ?, 1) \
         ON CONFLICT(email) DO UPDATE SET attempts = attempts + 1",
    )
    .bind(email)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|err| AuthError::new("db", format!("waitlist record failed: {err}")))?;
    Err(AuthError {
        code: "closed_beta",
        message: format!("{email} is not on the beta allowlist"),
        user_message: Some("Outer is in closed beta. You're on the waitlist.".to_string()),
        retry_after_s: None,
    })
}

/// Shared send path for `otp_sign_in` and the OAuth email step: same codes,
/// same 10-minute expiry, same 5/h-per-email and 20/h-per-IP limits.
pub async fn send_code(
    pool: &SqlitePool,
    cfg: &AuthConfig,
    email: &str,
    client_ip: Option<&str>,
) -> Result<u64, AuthError> {
    const EXPIRES_IN_S: u64 = 600;
    let now = now_ms();
    let email = normalize_email(email);
    if !valid_email(&email) {
        return Err(AuthError::new(
            "invalid_email",
            format!("{email:?} is not an email address"),
        ));
    }
    check_beta(pool, cfg, &email, now).await?;
    if let Some(account) = account_by_email(pool, &email)
        .await
        .map_err(|err| AuthError::new("db", format!("account lookup failed: {err}")))?
    {
        if account.banned_at.is_some() {
            return Err(AuthError::new(
                "forbidden",
                "this account is banned".to_string(),
            ));
        }
    }
    match limits::check_and_record(
        pool,
        &format!("email:{email}"),
        "otp_send",
        5,
        3_600_000,
        now,
    )
    .await
    {
        Ok(Some(retry_after_s)) => return Err(AuthError::rate_limited(retry_after_s)),
        Ok(None) => {}
        Err(err) => return Err(AuthError::new("db", format!("{err}"))),
    }
    if let Some(ip) = client_ip {
        match limits::check_and_record(pool, &format!("ip:{ip}"), "otp_send", 20, 3_600_000, now)
            .await
        {
            Ok(Some(retry_after_s)) => return Err(AuthError::rate_limited(retry_after_s)),
            Ok(None) => {}
            Err(err) => return Err(AuthError::new("db", format!("{err}"))),
        }
    }
    let code = tokens::new_otp_code();
    sqlx::query(
        "INSERT INTO email_code (email, code_hash, expires_at, attempts, created_at) \
         VALUES (?, ?, ?, 0, ?) \
         ON CONFLICT(email) DO UPDATE SET code_hash = excluded.code_hash, \
           expires_at = excluded.expires_at, attempts = 0, created_at = excluded.created_at",
    )
    .bind(&email)
    .bind(tokens::sha256_hex(&code))
    .bind(now + (EXPIRES_IN_S as i64) * 1000)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|err| AuthError::new("db", format!("code store failed: {err}")))?;
    let sent = cfg.email.send_code(&email, &code).await;
    if let Err(err) = &sent {
        // The operator must see a broken sender; the address stays out of
        // the log line (the error detail carries no secret either).
        tracing::warn!(error = %err, "sign-in code email failed");
    }
    match sent {
        Ok(()) => Ok(EXPIRES_IN_S),
        Err(email::EmailError::Retryable(detail)) => Err(AuthError {
            code: "email_retry",
            message: format!("email send failed, retry: {detail}"),
            user_message: Some("Outer couldn't send the code. Try again shortly.".to_string()),
            retry_after_s: Some(30),
        }),
        Err(email::EmailError::Permanent(detail)) => Err(AuthError {
            code: "email_failed",
            message: format!("email send failed: {detail}"),
            user_message: Some("Outer couldn't send the code to that address.".to_string()),
            retry_after_s: None,
        }),
    }
}

/// Shared verify path. On success the code row is gone and the account
/// exists; `terms_ok` tells the caller whether the terms step still owes.
/// Account creation counts against 3/day per IP when the IP is known.
pub async fn verify_code_core(
    pool: &SqlitePool,
    cfg: &AuthConfig,
    email: &str,
    code: &str,
    client_ip: Option<&str>,
) -> Result<(String, bool), AuthError> {
    let db = |err: sqlx::Error| AuthError::new("db", format!("{err}"));
    let now = now_ms();
    let email = normalize_email(email);
    let row: Option<(String, i64, i64)> =
        sqlx::query_as("SELECT code_hash, expires_at, attempts FROM email_code WHERE email = ?")
            .bind(&email)
            .fetch_optional(pool)
            .await
            .map_err(db)?;
    let Some((code_hash, expires_at, attempts)) = row else {
        return Err(AuthError::new(
            "invalid_code",
            "no pending code for this email".to_string(),
        ));
    };
    if now > expires_at {
        sqlx::query("DELETE FROM email_code WHERE email = ?")
            .bind(&email)
            .execute(pool)
            .await
            .map_err(db)?;
        return Err(AuthError::new(
            "code_expired",
            "the code expired, ask for a new one".to_string(),
        ));
    }
    if attempts >= 5 {
        sqlx::query("DELETE FROM email_code WHERE email = ?")
            .bind(&email)
            .execute(pool)
            .await
            .map_err(db)?;
        return Err(AuthError::new(
            "too_many_attempts",
            "too many wrong codes, ask for a new one".to_string(),
        ));
    }
    if !tokens::ct_eq(&tokens::sha256_hex(code.trim()), &code_hash) {
        let attempts = attempts + 1;
        if attempts >= 5 {
            sqlx::query("DELETE FROM email_code WHERE email = ?")
                .bind(&email)
                .execute(pool)
                .await
                .map_err(db)?;
            return Err(AuthError::new(
                "too_many_attempts",
                "too many wrong codes, ask for a new one".to_string(),
            ));
        }
        sqlx::query("UPDATE email_code SET attempts = ? WHERE email = ?")
            .bind(attempts)
            .bind(&email)
            .execute(pool)
            .await
            .map_err(db)?;
        return Err(AuthError::new(
            "invalid_code",
            "that code doesn't match".to_string(),
        ));
    }
    sqlx::query("DELETE FROM email_code WHERE email = ?")
        .bind(&email)
        .execute(pool)
        .await
        .map_err(db)?;
    let account = match account_by_email(pool, &email)
        .await
        .map_err(|err| AuthError::new("db", format!("{err}")))?
    {
        Some(account) => account,
        None => {
            if let Some(ip) = client_ip {
                if let Some(retry_after_s) = limits::check_and_record(
                    pool,
                    &format!("ip:{ip}"),
                    "account_create",
                    3,
                    86_400_000,
                    now,
                )
                .await
                .map_err(|err| AuthError::new("db", format!("{err}")))?
                {
                    return Err(AuthError::rate_limited(retry_after_s));
                }
            }
            let id = tokens::new_id();
            sqlx::query(
                "INSERT INTO account (id, email, last_active_at, created_at) VALUES (?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(&email)
            .bind(now)
            .bind(now)
            .execute(pool)
            .await
            .map_err(db)?;
            AccountRow {
                id,
                email: email.clone(),
                terms_version: None,
                banned_at: None,
            }
        }
    };
    if account.banned_at.is_some() {
        return Err(AuthError::new(
            "forbidden",
            "this account is banned".to_string(),
        ));
    }
    sqlx::query("UPDATE account SET last_active_at = ? WHERE id = ?")
        .bind(now)
        .bind(&account.id)
        .execute(pool)
        .await
        .map_err(db)?;
    let terms_ok = account.terms_accepted(cfg);
    Ok((account.id, terms_ok))
}

/// Accept the current terms for `account_id` (first sign-in, or a version
/// change re-asks at next sign-in).
pub async fn record_terms_acceptance(
    pool: &SqlitePool,
    cfg: &AuthConfig,
    account_id: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE account SET terms_version = ?, terms_accepted_at = ? WHERE id = ?")
        .bind(&cfg.terms_version)
        .bind(now_ms())
        .bind(account_id)
        .execute(pool)
        .await?;
    sqlx::query("UPDATE account SET last_active_at = ? WHERE id = ?")
        .bind(now_ms())
        .bind(account_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// `sign_in` tool (§6.2): normalize, beta allowlist/waitlist (§6.5), limits
/// (5/h per email, 20/h per IP, §14.3), store sha256(code) 10 min / 5
/// attempts, send.
pub async fn otp_sign_in(
    pool: &SqlitePool,
    cfg: &AuthConfig,
    email: &str,
    client_ip: Option<&str>,
) -> Result<SignInSent, AuthError> {
    Ok(SignInSent {
        expires_in_s: send_code(pool, cfg, email, client_ip).await?,
    })
}

/// `verify_code` tool: creates the account on first use (`accept_terms`
/// required then; stores terms_version + accepted_at), mints an API key
/// returned once.
pub async fn otp_verify(
    pool: &SqlitePool,
    cfg: &AuthConfig,
    email: &str,
    code: &str,
    accept_terms: bool,
    key_name: Option<&str>,
    client_ip: Option<&str>,
) -> Result<Verified, AuthError> {
    let email = normalize_email(email);
    let (account_id, terms_ok) = verify_code_core(pool, cfg, &email, code, client_ip).await?;
    if !terms_ok && !accept_terms {
        return Err(AuthError {
            code: "terms_required",
            message: "first sign-in requires accept_terms".to_string(),
            user_message: Some(
                "Accept the Terms and Privacy Policy to finish signing in.".to_string(),
            ),
            retry_after_s: None,
        });
    }
    if accept_terms || !terms_ok {
        record_terms_acceptance(pool, cfg, &account_id)
            .await
            .map_err(|err| AuthError::new("db", format!("{err}")))?;
    }
    let key = create_api_key(pool, &account_id, key_name).await?;
    Ok(Verified {
        api_key: key.key,
        account_id,
        email,
    })
}

pub async fn create_api_key(
    pool: &SqlitePool,
    account_id: &str,
    name: Option<&str>,
) -> Result<NewApiKey, AuthError> {
    let db = |err: sqlx::Error| AuthError::new("db", format!("{err}"));
    let now = now_ms();
    let name = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| if name.len() > 100 { &name[..100] } else { name }.to_string());
    let (key, prefix, key_hash) = tokens::new_api_key();
    let id = tokens::new_id();
    sqlx::query(
        "INSERT INTO api_key (id, account_id, name, prefix, key_hash, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(account_id)
    .bind(&name)
    .bind(&prefix)
    .bind(&key_hash)
    .bind(now)
    .execute(pool)
    .await
    .map_err(db)?;
    Ok(NewApiKey { id, prefix, key })
}

/// One API key row: id, name, prefix, created_at, last_used_at, client name/version.
type ApiKeyRow = (
    String,
    Option<String>,
    String,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
);

pub async fn list_api_keys(pool: &SqlitePool, account_id: &str) -> anyhow::Result<Vec<ApiKeyInfo>> {
    let rows: Vec<ApiKeyRow> = sqlx::query_as(
        "SELECT id, name, prefix, created_at, last_used_at, client_name, client_version \
             FROM api_key WHERE account_id = ? AND revoked_at IS NULL ORDER BY created_at",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, name, prefix, created_at, last_used_at, client_name, client_version)| {
                ApiKeyInfo {
                    id,
                    name,
                    prefix,
                    created_at,
                    last_used_at,
                    client_name,
                    client_version,
                }
            },
        )
        .collect())
}

pub async fn revoke_api_key(
    pool: &SqlitePool,
    account_id: &str,
    prefix: &str,
) -> Result<bool, AuthError> {
    // Exactly one live key per prefix, or nothing: a (vanishingly rare)
    // prefix collision must never revoke a key the caller didn't mean.
    let result = sqlx::query(
        "UPDATE api_key SET revoked_at = ? \
         WHERE account_id = ? AND prefix = ? AND revoked_at IS NULL \
           AND (SELECT COUNT(*) FROM api_key \
                WHERE account_id = ? AND prefix = ? AND revoked_at IS NULL) = 1",
    )
    .bind(now_ms())
    .bind(account_id)
    .bind(prefix)
    .bind(account_id)
    .bind(prefix)
    .execute(pool)
    .await
    .map_err(|err| AuthError::new("db", format!("{err}")))?;
    Ok(result.rows_affected() > 0)
}

/// One grant row with the client label: grant id, client_id, grant/client names, host, scope, created, last used.
type ConnectionRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    i64,
    Option<i64>,
);

pub async fn list_connections(
    pool: &SqlitePool,
    account_id: &str,
) -> anyhow::Result<Vec<ConnectionInfo>> {
    let rows: Vec<ConnectionRow> =
        sqlx::query_as(
            "SELECT g.id, g.client_id, g.client_name, c.name, c.host, g.scope, g.created_at, g.last_used_at \
             FROM oauth_grant g LEFT JOIN oauth_client c ON c.client_id = g.client_id \
             WHERE g.account_id = ? AND g.revoked_at IS NULL ORDER BY g.created_at",
        )
        .bind(account_id)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                grant_id,
                client_id,
                grant_name,
                client_name,
                client_host,
                scope,
                created_at,
                last_used_at,
            )| {
                ConnectionInfo {
                    grant_id,
                    client_id,
                    client_name: grant_name.or(client_name),
                    client_host,
                    scope,
                    created_at,
                    last_used_at,
                }
            },
        )
        .collect())
}

pub async fn revoke_connection(
    pool: &SqlitePool,
    account_id: &str,
    grant_id: &str,
) -> Result<bool, AuthError> {
    let db = |err: sqlx::Error| AuthError::new("db", format!("{err}"));
    let now = now_ms();
    let result = sqlx::query(
        "UPDATE oauth_grant SET revoked_at = ? WHERE id = ? AND account_id = ? AND revoked_at IS NULL",
    )
    .bind(now)
    .bind(grant_id)
    .bind(account_id)
    .execute(pool)
    .await
    .map_err(db)?;
    if result.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("DELETE FROM oauth_token WHERE grant_id = ?")
        .bind(grant_id)
        .execute(pool)
        .await
        .map_err(db)?;
    Ok(true)
}

/// Record initialize.clientInfo per grant or key (§8.1).
pub async fn record_client_info(
    pool: &SqlitePool,
    credential: &Credential,
    name: &str,
    version: &str,
) -> anyhow::Result<()> {
    let name = name.trim().chars().take(200).collect::<String>();
    let version = version.trim().chars().take(100).collect::<String>();
    match credential {
        Credential::ApiKey { key_id } => {
            sqlx::query("UPDATE api_key SET client_name = ?, client_version = ? WHERE id = ?")
                .bind(&name)
                .bind(&version)
                .bind(key_id)
                .execute(pool)
                .await?;
        }
        Credential::OAuth { grant_id } => {
            sqlx::query("UPDATE oauth_grant SET client_name = ?, client_version = ? WHERE id = ?")
                .bind(&name)
                .bind(&version)
                .bind(grant_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// `Authorization: Bearer <token>` → who it is. Accepts `outer_at_…`
/// (OAuth access token: unexpired, grant not revoked) and `outer_…` API
/// keys (not revoked). Unknown/expired/revoked → `Ok(None)`. Updates
/// last_used_at at most once a minute. Never logs the token.
pub async fn resolve_bearer(
    pool: &SqlitePool,
    cfg: &AuthConfig,
    token: &str,
) -> anyhow::Result<Option<Principal>> {
    let token = token.trim();
    let token = token
        .strip_prefix("Bearer ")
        .or_else(|| token.strip_prefix("bearer "))
        .unwrap_or(token);
    if token.is_empty() {
        return Ok(None);
    }
    let now = now_ms();
    if let Some(body) = token.strip_prefix(ACCESS_PREFIX) {
        if body.is_empty() {
            return Ok(None);
        }
        let hash = tokens::sha256_hex(token);
        let row: Option<(String, String, String, i64, Option<i64>)> = sqlx::query_as(
            "SELECT g.id, g.account_id, a.email, g.created_at, g.last_used_at \
             FROM oauth_token t JOIN oauth_grant g ON g.id = t.grant_id \
             JOIN account a ON a.id = g.account_id \
             WHERE t.token_hash = ? AND t.kind = 'access' AND t.expires_at > ? \
               AND g.revoked_at IS NULL AND a.banned_at IS NULL",
        )
        .bind(&hash)
        .bind(now)
        .fetch_optional(pool)
        .await?;
        let Some((grant_id, account_id, email, _, last_used_at)) = row else {
            return Ok(None);
        };
        touch_grant(pool, &grant_id, last_used_at, now).await?;
        return Ok(Some(Principal {
            account_id: account_id.clone(),
            email: email.clone(),
            is_admin: is_admin(cfg, &email),
            credential: Credential::OAuth { grant_id },
        }));
    }
    if token.starts_with(REFRESH_PREFIX) {
        return Ok(None);
    }
    let Some(body) = token.strip_prefix(API_KEY_PREFIX) else {
        return Ok(None);
    };
    if body.is_empty() {
        return Ok(None);
    }
    let hash = tokens::sha256_hex(token);
    let row: Option<(String, String, String, Option<i64>)> = sqlx::query_as(
        "SELECT k.id, k.account_id, a.email, k.last_used_at \
         FROM api_key k JOIN account a ON a.id = k.account_id \
         WHERE k.key_hash = ? AND k.revoked_at IS NULL AND a.banned_at IS NULL",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await?;
    let Some((key_id, account_id, email, last_used_at)) = row else {
        return Ok(None);
    };
    if last_used_at.is_none_or(|last| now - last >= 60_000) {
        sqlx::query("UPDATE api_key SET last_used_at = ? WHERE id = ?")
            .bind(now)
            .bind(&key_id)
            .execute(pool)
            .await?;
    }
    Ok(Some(Principal {
        account_id: account_id.clone(),
        email: email.clone(),
        is_admin: is_admin(cfg, &email),
        credential: Credential::ApiKey { key_id },
    }))
}

async fn touch_grant(
    pool: &SqlitePool,
    grant_id: &str,
    last_used_at: Option<i64>,
    now: i64,
) -> anyhow::Result<()> {
    if last_used_at.is_none_or(|last| now - last >= 60_000) {
        sqlx::query("UPDATE oauth_grant SET last_used_at = ? WHERE id = ?")
            .bind(now)
            .bind(grant_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// The lazy-auth 401 header value (spec §6.1):
/// `Bearer error="invalid_token", error_description="Sign in to Outer to use
/// this tool",
/// resource_metadata="<issuer>/.well-known/oauth-protected-resource/mcp",
/// scope="apps"`.
pub fn www_authenticate(cfg: &AuthConfig) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "Bearer error=\"invalid_token\", error_description=\"Sign in to Outer to use this tool\", \
         resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\", scope=\"apps\"",
        cfg.issuer
    ))
    .expect("issuer is set by config, not the request")
}

/// Web sessions (`__Host-outer_sid`, 30 days). Only the hash is stored.
pub const SESSION_COOKIE: &str = "__Host-outer_sid";
pub const SESSION_TTL_MS: i64 = 30 * 86_400_000;

pub async fn create_session(pool: &SqlitePool, account_id: &str) -> anyhow::Result<String> {
    let now = now_ms();
    let (id, hash) = tokens::new_session_id();
    sqlx::query("INSERT INTO web_session (id_hash, account_id, expires_at) VALUES (?, ?, ?)")
        .bind(&hash)
        .bind(account_id)
        .bind(now + SESSION_TTL_MS)
        .execute(pool)
        .await?;
    Ok(id)
}

pub async fn account_from_session(
    pool: &SqlitePool,
    session_id: &str,
) -> anyhow::Result<Option<AccountRow>> {
    let now = now_ms();
    let hash = tokens::sha256_hex(session_id.trim());
    let row: Option<(String, String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT a.id, a.email, a.terms_version, a.banned_at FROM web_session s \
         JOIN account a ON a.id = s.account_id \
         WHERE s.id_hash = ? AND s.expires_at > ? AND a.banned_at IS NULL",
    )
    .bind(&hash)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, email, terms_version, banned_at)| AccountRow {
        id,
        email,
        terms_version,
        banned_at,
    }))
}

pub fn set_session_cookie(session_id: &str) -> String {
    format!(
        "{SESSION_COOKIE}={session_id}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Lax",
        SESSION_TTL_MS / 1000
    )
}

/// The session cookie value from a `Cookie` header, if present.
pub fn session_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name.trim() == SESSION_COOKIE {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

/// The `web_session.id_hash` for a presented session id (sign-out deletes
/// by hash; the raw id never reaches storage).
pub fn session_hash(session_id: &str) -> String {
    tokens::sha256_hex(session_id.trim())
}

/// Expire the session cookie in the browser (sign-out answers this alongside
/// the deleted row).
pub fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax")
}

/// Delete expired email codes and web sessions (the daily prune). Only rows
/// no reader can still accept go (`expires_at < now`, matching the
/// `now > expires_at` checks on the verify paths). Returns
/// `(email_codes, sessions)` removed.
pub async fn prune_expired(pool: &SqlitePool, now: i64) -> anyhow::Result<(u64, u64)> {
    let codes = sqlx::query("DELETE FROM email_code WHERE expires_at < ?")
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();
    let sessions = sqlx::query("DELETE FROM web_session WHERE expires_at < ?")
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();
    Ok((codes, sessions))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Deterministic test config: codes are captured, loopback CIMD allowed,
    /// real network never touched.
    pub fn test_config() -> (AuthConfig, Arc<Mutex<HashMap<String, String>>>) {
        let codes = Arc::new(Mutex::new(HashMap::new()));
        let config = AuthConfig {
            issuer: "http://outer.localhost:8080".to_string(),
            resource: "http://outer.localhost:8080/mcp".to_string(),
            admin_emails: vec!["owner@example.com".to_string()],
            signup_open: true,
            terms_version: "2026-10-23".to_string(),
            access_ttl: Duration::from_secs(3600),
            refresh_ttl: Duration::from_secs(90 * 86400),
            email: EmailSender::Capture {
                codes: codes.clone(),
            },
            is_local: true,
            allow_loopback_fetch: true,
            origin_locked: false,
        };
        (config, codes)
    }

    pub fn test_state(pool: SqlitePool) -> AuthState {
        let (config, _) = test_config();
        AuthState { pool, config }
    }

    pub async fn test_pool() -> SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("auth.sqlite"))
            .await
            .unwrap();
        // `TempDir` would delete the database while the pool is open; leak it
        // so oneshot tests keep a live database for their whole run.
        std::mem::forget(dir);
        pool
    }

    pub fn captured(codes: &Arc<Mutex<HashMap<String, String>>>, email: &str) -> String {
        codes
            .lock()
            .unwrap()
            .get(email)
            .cloned()
            .unwrap_or_else(|| panic!("no code sent to {email}"))
    }
}
