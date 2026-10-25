//! Account-level domain paths (spec §6, §8.2, §13.1, §14.1): the account
//! summary behind Console → Usage, sign-out of other sessions, and account
//! deletion. One implementation serves the console endpoints
//! ([`crate::http::console_account`]) and the MCP `delete_account` tool, so
//! both delete exactly the same way.
//!
//! Numbers never originate here: plan limits come from the same functions
//! the quota checks use ([`crate::mcp::tools`]), and usage from the same
//! `usage_day` / `usage_month` rows the quota claims update.

use sqlx::SqlitePool;

use crate::auth::{normalize_email, now_ms};
use crate::mcp::{error::ToolError, tools};

/// Console-facing domain error: the §8.3 code plus the human message the
/// UI shows. HTTP status derives from the code in
/// [`crate::http::console_actions::tool_response`], the same envelope every
/// console mutation answers with.
#[derive(Debug)]
pub struct AccountError {
    pub code: &'static str,
    pub message: String,
    pub user_message: String,
}

impl AccountError {
    fn new(code: &'static str, message: String, user_message: String) -> Self {
        Self {
            code,
            message,
            user_message,
        }
    }

    pub fn not_found(message: String) -> Self {
        Self::new("not_found", message.clone(), message)
    }

    pub fn invalid_argument(message: String, user_message: String) -> Self {
        Self::new("invalid_argument", message, user_message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        let message = message.into();
        tracing::warn!(error = %message, "account domain internal error");
        Self::new(
            "internal",
            message,
            "Something broke on our side; try again in a minute.".to_string(),
        )
    }
}

impl From<ToolError> for AccountError {
    fn from(err: ToolError) -> Self {
        let user_message = err
            .user_message
            .clone()
            .unwrap_or_else(|| err.message.clone());
        Self::new(err.code, err.message, user_message)
    }
}

impl From<AccountError> for ToolError {
    fn from(err: AccountError) -> Self {
        Self::new(err.code, err.message).with_user(err.user_message)
    }
}

impl From<crate::auth::AuthError> for AccountError {
    fn from(err: crate::auth::AuthError) -> Self {
        Self::from(ToolError::from_auth(err))
    }
}

impl From<sqlx::Error> for AccountError {
    fn from(err: sqlx::Error) -> Self {
        Self::internal(format!("db: {err}"))
    }
}

/// One `account_action` row (migration 0005): the account-level equivalent
/// of an `agent_action` row, with the MCP tool name and client "Console".
/// A failed insert warns and the mutation result stands, like
/// [`crate::mcp::actions::record`].
pub async fn record_account_action(
    pool: &SqlitePool,
    user_id: &str,
    tool: &str,
    summary: &str,
    outcome: &str,
    error_code: Option<&str>,
    started_ms: i64,
) {
    let summary: String = summary.chars().take(2000).collect();
    if let Err(err) = sqlx::query(
        "INSERT INTO account_action (id, user_id, tool, summary, outcome, error_code, duration_ms, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(tool)
    .bind(&summary)
    .bind(outcome)
    .bind(error_code)
    .bind(now_ms().saturating_sub(started_ms).max(0))
    .bind(now_ms())
    .execute(pool)
    .await
    {
        tracing::warn!(user = %user_id, tool, error = %format!("{err:#}"), "account_action insert failed");
    }
}

/// UTC month (`usage_month.month`).
pub fn month_utc() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

/// Rollback budget used today by one owned app (same `rollback_day` rows
/// and limit [`tools::ROLLBACKS_PER_DAY`] the rollback quota check uses).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RollbackUsage {
    pub slug: String,
    pub used_today: u64,
    pub limit: u64,
}

/// Everything Console → Usage shows, from the single sources of truth.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountSummary {
    pub email: String,
    pub plan: String,
    pub plan_status: Option<String>,
    /// Unix seconds the past-due grace ends (`user.graceUntil`).
    pub grace_until: Option<i64>,
    /// Whether Stripe knows this account (portal available).
    pub has_customer: bool,
    pub member_since: i64,
    pub apps_owned: u64,
    pub apps_limit: u64,
    pub code_changes_today: u64,
    pub code_changes_limit: u64,
    /// Wall time the deploy pipeline charged today (ms, `usage_day`).
    pub build_ms_today: u64,
    pub rollbacks: Vec<RollbackUsage>,
    pub rollback_limit: u64,
    /// This month's proxy counts (`usage_month`); zero when nothing was
    /// tracked yet. No plan caps are shown for these: no quota check
    /// enforces them in code, and the UI must not invent numbers.
    pub requests_month: u64,
    pub storage_peak_bytes: u64,
    pub egress_bytes_month: u64,
    pub resets_at: String,
}

/// The account summary: plan from [`tools::plan_of`], usage from
/// `usage_day` / `usage_month`, limits from the quota functions.
pub async fn account_summary(
    pool: &SqlitePool,
    account_id: &str,
) -> Result<AccountSummary, AccountError> {
    #[derive(sqlx::FromRow)]
    struct SummaryRow {
        email: String,
        plan: String,
        #[sqlx(rename = "planStatus")]
        plan_status: Option<String>,
        #[sqlx(rename = "graceUntil")]
        grace_until: Option<i64>,
        #[sqlx(rename = "stripeCustomerId")]
        stripe_customer: Option<String>,
        #[sqlx(rename = "createdAt")]
        member_since: i64,
    }
    let row: Option<SummaryRow> = sqlx::query_as(
        "SELECT email, plan, planStatus, graceUntil, stripeCustomerId, createdAt FROM user WHERE id = ?1",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    let Some(SummaryRow {
        email,
        plan,
        plan_status,
        grace_until,
        stripe_customer,
        member_since,
    }) = row
    else {
        return Err(AccountError::not_found("no such account".to_string()));
    };
    let apps_owned = tools::apps_owned(pool, account_id).await?;
    let code_changes_today = tools::changes_used_today(pool, account_id).await?;
    let build_ms_today: Option<i64> =
        sqlx::query_scalar("SELECT build_ms FROM usage_day WHERE userId = ?1 AND day = ?2")
            .bind(account_id)
            .bind(tools::today_utc())
            .fetch_optional(pool)
            .await?;
    let month: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT requests, storage_peak_bytes, egress_bytes FROM usage_month WHERE userId = ?1 AND month = ?2",
    )
    .bind(account_id)
    .bind(month_utc())
    .fetch_optional(pool)
    .await?;
    let owned: Vec<(String, String)> =
        sqlx::query_as("SELECT id, slug FROM app WHERE owner_id = ?1 ORDER BY slug")
            .bind(account_id)
            .fetch_all(pool)
            .await?;
    let mut rollbacks = Vec::with_capacity(owned.len());
    for (app_id, slug) in &owned {
        let used: Option<i64> =
            sqlx::query_scalar("SELECT count FROM rollback_day WHERE app_id = ?1 AND day = ?2")
                .bind(app_id)
                .bind(tools::today_utc())
                .fetch_optional(pool)
                .await?;
        rollbacks.push(RollbackUsage {
            slug: slug.clone(),
            used_today: used.unwrap_or(0).max(0) as u64,
            limit: tools::ROLLBACKS_PER_DAY,
        });
    }
    let (requests_month, storage_peak_bytes, egress_bytes_month) = month
        .map(|(r, s, e)| (r.max(0) as u64, s.max(0) as u64, e.max(0) as u64))
        .unwrap_or_default();
    Ok(AccountSummary {
        email,
        apps_owned,
        apps_limit: tools::app_limit(&plan),
        code_changes_today,
        code_changes_limit: tools::change_limit(&plan),
        build_ms_today: build_ms_today.unwrap_or(0).max(0) as u64,
        rollbacks,
        rollback_limit: tools::ROLLBACKS_PER_DAY,
        requests_month,
        storage_peak_bytes,
        egress_bytes_month,
        resets_at: tools::resets_at_utc(),
        plan,
        plan_status,
        grace_until,
        has_customer: stripe_customer.is_some(),
        member_since,
    })
}

/// Sign out every web session except the current one. Returns how many
/// sessions were deleted; the current session row is untouched.
pub async fn sign_out_others(
    pool: &SqlitePool,
    account_id: &str,
    current_session_hash: &str,
) -> Result<u64, AccountError> {
    let result = sqlx::query("DELETE FROM session WHERE userId = ?1 AND token != ?2")
        .bind(account_id)
        .bind(current_session_hash)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// What `delete_account` needs beyond the pool: the same pieces the MCP
/// `delete_app` tool builds its [`crate::apps::delete::DeleteDeps`] from,
/// plus the per-app locks that serialize a deletion against deploys,
/// rollbacks and wakes.
pub struct DeleteAccountDeps<'a> {
    pub pool: &'a SqlitePool,
    pub store: &'a crate::store::Store,
    pub git: &'a crate::git::GitConfig,
    pub sync: &'a crate::git::GitSync,
    pub fleets: &'a crate::fleet::manager::FleetManager,
    pub deploys: &'a crate::mcp::Deploys,
    pub rollback_locks: &'a crate::mcp::RollbackLocks,
    pub delete_cfg: &'a crate::apps::delete::DeleteConfig,
}

/// What a finished account deletion hands back.
#[derive(Debug)]
pub struct DeleteAccountOutput {
    pub email: String,
    pub deleted_apps: Vec<String>,
    pub signed_out_sessions: u64,
}

/// Delete the account (spec §8.2 `delete_account`): `confirm` must equal
/// the account email exactly (normalized like every email comparison).
/// Deletes owned apps through the SAME [`crate::apps::delete::delete_app`]
/// path the MCP tool uses (fleet stopped, bucket prefixes wiped, slug
/// quarantined), leaves apps the account only collaborates on
/// (`app_collaborator.userId` goes NULL), then deletes the sessions, usage
/// rows and the user row (API keys, OAuth grants and tokens cascade).
/// The `account_action` row for the deletion itself is written first and
/// survives, since it carries no foreign key.
pub async fn delete_account(
    deps: &DeleteAccountDeps<'_>,
    account_id: &str,
    confirm: &str,
) -> Result<DeleteAccountOutput, AccountError> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT id, email FROM user WHERE id = ?1")
        .bind(account_id)
        .fetch_optional(deps.pool)
        .await?;
    let Some((id, email)) = row else {
        return Err(AccountError::not_found("no such account".to_string()));
    };
    if normalize_email(confirm) != normalize_email(&email) {
        return Err(AccountError::invalid_argument(
            format!("confirm {confirm:?} does not match the account email"),
            "To delete your account, type your email address exactly.".to_string(),
        ));
    }
    let owned: Vec<(String, String)> =
        sqlx::query_as("SELECT id, slug FROM app WHERE owner_id = ?1 ORDER BY slug")
            .bind(&id)
            .fetch_all(deps.pool)
            .await?;
    let mut deleted_apps = Vec::with_capacity(owned.len());
    for (app_id, slug) in &owned {
        let rollback = deps.rollback_locks.lock(app_id).await;
        let deploy = deps.deploys.app_lock(app_id).lock_owned().await;
        let wake_key = deps.fleets.wake_lock(slug).await;
        let wake = wake_key.lock_owned().await;
        let app_deps = crate::apps::delete::DeleteDeps {
            pool: deps.pool,
            store: deps.store,
            git: deps.git,
            sync: deps.sync,
            fleets: Some(deps.fleets),
        };
        let locks = crate::apps::delete::DeleteLocks {
            deploy: &deploy,
            rollback: Some(&rollback),
            wake: Some(&wake),
        };
        match crate::apps::delete::delete_app(&app_deps, deps.delete_cfg, slug, slug, locks).await {
            Ok(_) => deleted_apps.push(slug.clone()),
            Err(crate::apps::delete::DeleteError::NotFound(_)) => {}
            Err(crate::apps::delete::DeleteError::ConfirmMismatch(_, _)) => {
                return Err(AccountError::internal(format!(
                    "refusing to delete owned app {slug:?}: internal confirm mismatch"
                )));
            }
            Err(crate::apps::delete::DeleteError::Internal(err)) => {
                return Err(AccountError::internal(format!(
                    "deleting owned app {slug:?} failed: {err:#}"
                )));
            }
        }
    }
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session WHERE userId = ?1")
        .bind(&id)
        .fetch_optional(deps.pool)
        .await?
        .unwrap_or(0);
    record_account_action(
        deps.pool,
        &id,
        "delete_account",
        &format!(
            "delete_account {} ({} owned app(s))",
            email,
            deleted_apps.len()
        ),
        "ok",
        None,
        now_ms(),
    )
    .await;
    sqlx::query("DELETE FROM session WHERE userId = ?1")
        .bind(&id)
        .execute(deps.pool)
        .await?;
    sqlx::query("DELETE FROM usage_day WHERE userId = ?1")
        .bind(&id)
        .execute(deps.pool)
        .await?;
    sqlx::query("DELETE FROM usage_month WHERE userId = ?1")
        .bind(&id)
        .execute(deps.pool)
        .await?;
    sqlx::query("DELETE FROM verification WHERE identifier = ?1")
        .bind(normalize_email(&email))
        .execute(deps.pool)
        .await?;
    sqlx::query("DELETE FROM user WHERE id = ?1")
        .bind(&id)
        .execute(deps.pool)
        .await?;
    Ok(DeleteAccountOutput {
        email,
        deleted_apps,
        signed_out_sessions: sessions.max(0) as u64,
    })
}
