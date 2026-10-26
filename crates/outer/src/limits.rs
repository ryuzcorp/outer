//! Plan limits (spec §3, owner-set 2026-10-26): one table feeding
//! enforcement, `whoami`, `/llms.txt` and error messages.
//!
//! Every number below is the approved 2026-10-26 table. Enforcement reads
//! these functions, never a second copy of the number:
//! - apps/changes/build minutes/collaborators/storage: hard refusals
//!   (`quota_exceeded` + `upgrade_url`, §8.3) at the call sites in
//!   [`crate::mcp::tools`], [`crate::apps`], [`crate::collaborators`],
//!   [`crate::deploy`] and the console data panels;
//! - requests: soft caps, one owner email per month, the app keeps serving;
//! - logs/backups: the daily prune sweeps read the retention numbers here.
//!
//! Abuse throttling: Outer never auto-pauses. Past 10× the monthly request
//! cap (sustained, not a spike) the operator reviews the app by hand; the
//! per-app nft counters stay observability only.

use std::time::Duration;

use chrono::Datelike as _;

use crate::auth::EmailSender;
use crate::billing::BillingState;
use crate::store::Store;

/// Free unless the `user` row says `pro`.
pub fn is_pro(plan: &str) -> bool {
    plan == "pro"
}

// ---------------------------------------------------------------------------
// Apps, code changes, build minutes
// ---------------------------------------------------------------------------

/// App slots: Free 2, Pro 20. Enforced in [`crate::apps::create`] and the
/// MCP early check.
pub const FREE_APPS: u64 = 2;
/// App slots: Free 2, Pro 20. Enforced in [`crate::apps::create`] and the
/// MCP early check.
pub const PRO_APPS: u64 = 20;

/// Code changes per UTC day: Free 40, Pro 1,000 (fair use).
pub const FREE_CHANGES_PER_DAY: u64 = 40;
/// Code changes per UTC day: Free 40, Pro 1,000 (fair use).
pub const PRO_CHANGES_PER_DAY: u64 = 1_000;

/// Build minutes per UTC day: Free 30, Pro 300. Charged from the deploy
/// pipeline's install+build wall time; a build in flight always finishes.
pub const FREE_BUILD_MINUTES_PER_DAY: u64 = 30;
/// Build minutes per UTC day: Free 30, Pro 300. Charged from the deploy
/// pipeline's install+build wall time; a build in flight always finishes.
pub const PRO_BUILD_MINUTES_PER_DAY: u64 = 300;

pub fn app_limit(plan: &str) -> u64 {
    if is_pro(plan) {
        PRO_APPS
    } else {
        FREE_APPS
    }
}

pub fn change_limit(plan: &str) -> u64 {
    if is_pro(plan) {
        PRO_CHANGES_PER_DAY
    } else {
        FREE_CHANGES_PER_DAY
    }
}

pub fn build_minutes_limit(plan: &str) -> u64 {
    if is_pro(plan) {
        PRO_BUILD_MINUTES_PER_DAY
    } else {
        FREE_BUILD_MINUTES_PER_DAY
    }
}

/// The build-minutes cap in ms, for comparing against `usage_day.build_ms`.
pub fn build_ms_limit(plan: &str) -> u64 {
    build_minutes_limit(plan) * 60_000
}

// ---------------------------------------------------------------------------
// Sleep
// ---------------------------------------------------------------------------

/// Free fleets sleep after 15 minutes idle (cold start on next request).
/// The sweep's startup guard stays on the host value, so a fleet younger
/// than that never sleeps whatever the plan says.
pub const FREE_SLEEP_AFTER: Duration = Duration::from_secs(15 * 60);

/// Pro fleets keep the existing keep-warm behaviour; a Pro app without
/// keep-warm keeps the current host idle time (the `OUTER_SLEEP_AFTER_S`
/// default, 30 minutes), passed in by the caller.
pub fn sleep_after(plan: &str, pro_default: Duration) -> Duration {
    if is_pro(plan) {
        pro_default
    } else {
        FREE_SLEEP_AFTER
    }
}

// ---------------------------------------------------------------------------
// Collaborators
// ---------------------------------------------------------------------------

/// Collaborators per app: Free 1, Pro 10. Downgrade keeps the rows; only the
/// oldest collaborator keeps access on Free (the rest pause until upgrade).
pub const FREE_COLLABORATORS: u64 = 1;
/// Collaborators per app: Free 1, Pro 10. Downgrade keeps the rows; only the
/// oldest collaborator keeps access on Free (the rest pause until upgrade).
pub const PRO_COLLABORATORS: u64 = 10;

pub fn collaborator_limit(plan: &str) -> u64 {
    if is_pro(plan) {
        PRO_COLLABORATORS
    } else {
        FREE_COLLABORATORS
    }
}

// ---------------------------------------------------------------------------
// Retention: logs, errors, backups
// ---------------------------------------------------------------------------

/// Agent-action log, metrics and error history: Free 24 h, Pro 30 days.
pub const FREE_LOG_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;
/// Agent-action log, metrics and error history: Free 24 h, Pro 30 days.
pub const PRO_LOG_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub fn log_retention_ms(plan: &str) -> i64 {
    if is_pro(plan) {
        PRO_LOG_RETENTION_MS
    } else {
        FREE_LOG_RETENTION_MS
    }
}

/// Daily D1 dumps kept: Free 3, Pro 30 (any-day restore from the kept set).
pub const FREE_BACKUPS_KEEP: i64 = 3;
/// Daily D1 dumps kept: Free 3, Pro 30 (any-day restore from the kept set).
pub const PRO_BACKUPS_KEEP: i64 = 30;

pub fn backups_keep(plan: &str) -> i64 {
    if is_pro(plan) {
        PRO_BACKUPS_KEEP
    } else {
        FREE_BACKUPS_KEEP
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// D1 bytes per app: Free 100 MB, Pro 5 GB. Writes refused at the cap;
/// nothing is ever deleted.
pub const FREE_D1_BYTES: u64 = 100 * 1024 * 1024;
/// D1 bytes per app: Free 100 MB, Pro 5 GB. Writes refused at the cap;
/// nothing is ever deleted.
pub const PRO_D1_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// R2 bytes per app: Free 250 MB, Pro 25 GB. Writes refused at the cap;
/// nothing is ever deleted.
pub const FREE_R2_BYTES: u64 = 250 * 1024 * 1024;
/// R2 bytes per app: Free 250 MB, Pro 25 GB. Writes refused at the cap;
/// nothing is ever deleted.
pub const PRO_R2_BYTES: u64 = 25 * 1024 * 1024 * 1024;

pub fn d1_cap_bytes(plan: &str) -> u64 {
    if is_pro(plan) {
        PRO_D1_BYTES
    } else {
        FREE_D1_BYTES
    }
}

pub fn r2_cap_bytes(plan: &str) -> u64 {
    if is_pro(plan) {
        PRO_R2_BYTES
    } else {
        FREE_R2_BYTES
    }
}

// ---------------------------------------------------------------------------
// Requests (soft)
// ---------------------------------------------------------------------------

/// Requests per calendar month: Free soft 100k, Pro soft 5M. At the cap the
/// owner gets one email per month with an upgrade ask; the app keeps
/// serving. No pausing.
pub const FREE_REQUESTS_PER_MONTH: u64 = 100_000;
/// Requests per calendar month: Free soft 100k, Pro soft 5M. At the cap the
/// owner gets one email per month with an upgrade ask; the app keeps
/// serving. No pausing.
pub const PRO_REQUESTS_PER_MONTH: u64 = 5_000_000;

/// Past this multiple of the monthly cap the operator reviews the app by
/// hand. Documented, never automatic.
pub const REQUESTS_ABUSE_MULTIPLE: u64 = 10;

pub fn requests_cap(plan: &str) -> u64 {
    if is_pro(plan) {
        PRO_REQUESTS_PER_MONTH
    } else {
        FREE_REQUESTS_PER_MONTH
    }
}

/// UTC day (`usage_day.day`).
fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Build milliseconds `account_id` already burned today (UTC).
pub async fn build_ms_used_today(pool: &sqlx::SqlitePool, account_id: &str) -> anyhow::Result<u64> {
    let used: Option<i64> =
        sqlx::query_scalar("SELECT build_ms FROM usage_day WHERE userId = ?1 AND day = ?2")
            .bind(account_id)
            .bind(today_utc())
            .fetch_optional(pool)
            .await?;
    Ok(used.unwrap_or(0).max(0) as u64)
}

/// Charge `ms` of install+build wall time to today's row. Never fails the
/// deploy: callers warn and continue.
pub async fn add_build_ms(
    pool: &sqlx::SqlitePool,
    account_id: &str,
    ms: u64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO usage_day (userId, day, build_ms) VALUES (?1, ?2, ?3) \
         ON CONFLICT(userId, day) DO UPDATE SET build_ms = build_ms + ?3",
    )
    .bind(account_id)
    .bind(today_utc())
    .bind(ms.min(i64::MAX as u64) as i64)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Storage freeze (read-only at the caps)
// ---------------------------------------------------------------------------

/// D1 bytes for `app_id`, estimated from the newest dump per binding
/// (`d1_backup.size_bytes`). A database that has never been backed up reads
/// as 0 until the daily backup sweep dumps it — the sweep orders backup
/// before sizing, so the gap closes within a day.
pub async fn d1_bytes_used(pool: &sqlx::SqlitePool, app_id: &str) -> anyhow::Result<u64> {
    let total: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT SUM(size_bytes) FROM d1_backup AS b WHERE b.app_id = ?1 \
         AND b.created_at = (SELECT MAX(created_at) FROM d1_backup \
                             WHERE app_id = b.app_id AND binding = b.binding)",
    )
    .bind(app_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(total.unwrap_or(0).max(0) as u64)
}

/// Whether `app_id` is frozen read-only at its storage cap
/// (`app.storage_frozen_at`, migration 0016).
pub async fn storage_frozen(pool: &sqlx::SqlitePool, app_id: &str) -> anyhow::Result<bool> {
    // The scalar is `Option<i64>` explicitly: a NULL flag must decode to
    // None, and sqlx-sqlite decodes NULL into a bare `i64` as 0.
    let frozen: Option<i64> =
        sqlx::query_scalar::<_, Option<i64>>("SELECT storage_frozen_at FROM app WHERE id = ?1")
            .bind(app_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(frozen.is_some())
}

/// Freeze `app_id` read-only. Returns true when the flag flipped (the caller
/// sends the owner email only then).
pub async fn freeze_storage(
    pool: &sqlx::SqlitePool,
    app_id: &str,
    detail: &str,
    now_ms: i64,
) -> anyhow::Result<bool> {
    let rows = sqlx::query(
        "UPDATE app SET storage_frozen_at = ?1, storage_frozen_detail = ?2 \
         WHERE id = ?3 AND storage_frozen_at IS NULL",
    )
    .bind(now_ms)
    .bind(detail)
    .bind(app_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// Clear a freeze (the app dropped back under its caps). Returns true when
/// the flag flipped.
pub async fn unfreeze_storage(pool: &sqlx::SqlitePool, app_id: &str) -> anyhow::Result<bool> {
    let rows = sqlx::query(
        "UPDATE app SET storage_frozen_at = NULL, storage_frozen_detail = NULL WHERE id = ?1",
    )
    .bind(app_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows > 0)
}

// ---------------------------------------------------------------------------
// Hourly sweep: request rollup + soft-cap email, storage freeze
// ---------------------------------------------------------------------------

/// What the hourly sweep needs: explicit dependencies, no globals.
pub struct LimitsSweep {
    pub pool: sqlx::SqlitePool,
    pub store: Store,
    pub sender: EmailSender,
    pub issuer: String,
    pub unsub_key: Vec<u8>,
    pub billing: BillingState,
}

/// Roll `metric_minute` request counts into `usage_month` (per owner,
/// current calendar month), advancing the `usage_rollup_cursor` watermark so
/// a minute is never counted twice. Runs hourly because Free minute rows
/// prune after 24 h — a daily rollup could lose a day to downtime.
pub async fn rollup_requests(pool: &sqlx::SqlitePool, month: &str) -> anyhow::Result<u64> {
    let cursor: Option<i64> =
        sqlx::query_scalar("SELECT last_minute FROM usage_rollup_cursor WHERE id = 'requests'")
            .fetch_optional(pool)
            .await?;
    let cursor = cursor.unwrap_or(0);
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT a.owner_id, SUM(m.requests) FROM metric_minute AS m \
         JOIN app AS a ON a.id = m.app_id \
         WHERE m.minute > ?1 GROUP BY a.owner_id",
    )
    .bind(cursor)
    .fetch_all(pool)
    .await?;
    let mut owners = 0u64;
    for (owner_id, requests) in &rows {
        sqlx::query(
            "INSERT INTO usage_month (userId, month, requests, storage_peak_bytes, egress_bytes) \
             VALUES (?1, ?2, ?3, 0, 0) \
             ON CONFLICT(userId, month) DO UPDATE SET requests = requests + ?3",
        )
        .bind(owner_id)
        .bind(month)
        .bind((*requests).max(0))
        .execute(pool)
        .await?;
        owners += 1;
    }
    let max_minute: Option<i64> =
        sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(minute) FROM metric_minute")
            .fetch_optional(pool)
            .await?
            .flatten();
    if let Some(max) = max_minute {
        sqlx::query(
            "INSERT INTO usage_rollup_cursor (id, last_minute) VALUES ('requests', ?1) \
             ON CONFLICT(id) DO UPDATE SET last_minute = ?1",
        )
        .bind(max.max(cursor))
        .execute(pool)
        .await?;
    }
    Ok(owners)
}

/// One owner email per month when `usage_month.requests` passes the plan's
/// soft cap (spec §3). The app keeps serving; nothing pauses.
pub async fn check_requests_caps(
    deps: &LimitsSweep,
    month: &str,
    now_ms: i64,
) -> anyhow::Result<u64> {
    #[derive(sqlx::FromRow)]
    struct OwnerRow {
        id: String,
        plan: String,
        requests: Option<i64>,
    }
    let rows: Vec<OwnerRow> = sqlx::query_as(
        "SELECT u.id AS id, u.plan AS plan, m.requests AS requests FROM user AS u \
         LEFT JOIN usage_month AS m ON m.userId = u.id AND m.month = ?1",
    )
    .bind(month)
    .fetch_all(&deps.pool)
    .await?;
    let now_secs = now_ms / 1000;
    let month_start_ms = month_start_ms(now_ms);
    let mut mailed = 0u64;
    for row in &rows {
        let used = row.requests.unwrap_or(0).max(0) as u64;
        let cap = requests_cap(&row.plan);
        if used < cap {
            continue;
        }
        let ctx = crate::alerts::Ctx {
            pool: &deps.pool,
            sender: &deps.sender,
            issuer: &deps.issuer,
            key: &deps.unsub_key,
        };
        crate::alerts::notify_requests_cap(
            &ctx,
            &row.id,
            used,
            cap,
            is_pro(&row.plan),
            &deps.billing.upgrade_url(&row.id, now_secs),
            month_start_ms,
            now_ms,
        )
        .await;
        mailed += 1;
    }
    Ok(mailed)
}

/// Freeze apps past their D1/R2 caps (read-only; never delete) and unfreeze
/// apps back under. The owner email fires only on the freeze transition.
///
/// What counts: D1 is the newest dump per binding (`d1_bytes_used`); R2 is
/// the bytes under `fleets/{slug}/r2/`. App code writes through celld with
/// scoped credentials, so Outer cannot refuse an in-runtime write at the
/// call — the sweep flips the flag within the hour and the Console, commit
/// and upload paths refuse while it is set.
pub async fn check_storage(deps: &LimitsSweep, now_ms: i64) -> anyhow::Result<(usize, usize)> {
    #[derive(sqlx::FromRow)]
    struct AppRow {
        id: String,
        slug: String,
        owner_id: String,
        plan: String,
        storage_frozen_at: Option<i64>,
    }
    let apps: Vec<AppRow> = sqlx::query_as(
        "SELECT a.id AS id, a.slug AS slug, a.owner_id AS owner_id, u.plan AS plan, \
         a.storage_frozen_at AS storage_frozen_at \
         FROM app AS a JOIN user AS u ON u.id = a.owner_id",
    )
    .fetch_all(&deps.pool)
    .await?;
    let now_secs = now_ms / 1000;
    let mut frozen = 0usize;
    let mut unfrozen = 0usize;
    for app in &apps {
        let d1 = match d1_bytes_used(&deps.pool, &app.id).await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(slug = %app.slug, error = %format!("{err:#}"), "storage sweep: D1 sizing failed");
                continue;
            }
        };
        let r2_prefix = format!("fleets/{}/r2/", app.slug);
        let r2 = match deps.store.prefix_bytes(&r2_prefix).await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(slug = %app.slug, error = %format!("{err:#}"), "storage sweep: R2 sizing failed");
                continue;
            }
        };
        let d1_cap = d1_cap_bytes(&app.plan);
        let r2_cap = r2_cap_bytes(&app.plan);
        let mut over = Vec::new();
        if d1 > d1_cap {
            over.push(format!(
                "D1 {} MB over the {} MB cap",
                d1 / 1_048_576,
                d1_cap / 1_048_576
            ));
        }
        if r2 > r2_cap {
            over.push(format!(
                "R2 {} MB over the {} MB cap",
                r2 / 1_048_576,
                r2_cap / 1_048_576
            ));
        }
        if over.is_empty() {
            if app.storage_frozen_at.is_some() && unfreeze_storage(&deps.pool, &app.id).await? {
                unfrozen += 1;
            }
            continue;
        }
        let detail = over.join("; ");
        if app.storage_frozen_at.is_none()
            && freeze_storage(&deps.pool, &app.id, &detail, now_ms).await?
        {
            frozen += 1;
            let ctx = crate::alerts::Ctx {
                pool: &deps.pool,
                sender: &deps.sender,
                issuer: &deps.issuer,
                key: &deps.unsub_key,
            };
            crate::alerts::notify_storage_frozen(
                &ctx,
                &app.owner_id,
                &app.slug,
                &detail,
                &deps.billing.upgrade_url(&app.owner_id, now_secs),
                now_ms,
            )
            .await;
        }
    }
    Ok((frozen, unfrozen))
}

/// UTC month (`usage_month.month`).
pub fn current_month() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

/// First millisecond of `now_ms`'s UTC month, for the monthly email window.
fn month_start_ms(now_ms: i64) -> i64 {
    let date = chrono::DateTime::from_timestamp_millis(now_ms)
        .map(|dt| dt.date_naive())
        .unwrap_or_else(|| chrono::Utc::now().date_naive());
    date.with_day(1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| t.and_utc().timestamp_millis())
        .unwrap_or(0)
}

/// The hourly plan-limits sweep: request rollup + soft-cap email, storage
/// freeze. A missed tick only delays mail and flags; the next tick catches
/// up (the rollup watermark never double-counts).
pub fn spawn_limits_sweep(deps: LimitsSweep) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            timer.tick().await;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let month = current_month();
            match rollup_requests(&deps.pool, &month).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(owners = n, "limits sweep: rolled up request counts"),
                Err(err) => tracing::warn!("limits sweep: request rollup failed: {err:#}"),
            }
            match check_requests_caps(&deps, &month, now_ms).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(mailed = n, "limits sweep: request cap emails"),
                Err(err) => tracing::warn!("limits sweep: request cap check failed: {err:#}"),
            }
            match check_storage(&deps, now_ms).await {
                Ok((0, 0)) => {}
                Ok((frozen, unfrozen)) => {
                    tracing::info!(frozen, unfrozen, "limits sweep: storage freeze pass")
                }
                Err(err) => tracing::warn!("limits sweep: storage check failed: {err:#}"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_is_the_default_plan_shape() {
        assert!(!is_pro("free"));
        assert!(!is_pro(""));
        assert!(!is_pro("team"));
        assert!(is_pro("pro"));
    }

    #[test]
    fn approved_table_numbers() {
        assert_eq!(app_limit("free"), 2);
        assert_eq!(app_limit("pro"), 20);
        assert_eq!(change_limit("free"), 40);
        assert_eq!(change_limit("pro"), 1_000);
        assert_eq!(build_minutes_limit("free"), 30);
        assert_eq!(build_minutes_limit("pro"), 300);
        assert_eq!(collaborator_limit("free"), 1);
        assert_eq!(collaborator_limit("pro"), 10);
        assert_eq!(backups_keep("free"), 3);
        assert_eq!(backups_keep("pro"), 30);
        assert_eq!(requests_cap("free"), 100_000);
        assert_eq!(requests_cap("pro"), 5_000_000);
    }

    #[test]
    fn sleep_is_15_min_free_and_host_default_pro() {
        let host = std::time::Duration::from_secs(1800);
        assert_eq!(
            sleep_after("free", host),
            std::time::Duration::from_secs(900)
        );
        assert_eq!(sleep_after("pro", host), host);
    }

    async fn pool() -> sqlx::SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("limits.sqlite"))
            .await
            .unwrap();
        std::mem::forget(dir);
        pool
    }

    async fn seed_user(pool: &sqlx::SqlitePool, id: &str, plan: &str) {
        sqlx::query("INSERT INTO user (id, email, plan, lastActiveAt, createdAt, updatedAt) VALUES (?1, ?2, ?3, 0, 0, 0)")
            .bind(id)
            .bind(format!("{id}@example.com"))
            .bind(plan)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn seed_app(pool: &sqlx::SqlitePool, id: &str, slug: &str, owner: &str) {
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
             last_activity_at, created_at) \
             VALUES (?1, ?2, ?3, 300001, 20001, 20002, 0, 0)",
        )
        .bind(id)
        .bind(slug)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    }

    fn sweep_deps(
        pool: sqlx::SqlitePool,
        store: crate::store::Store,
        sender: crate::auth::EmailSender,
    ) -> LimitsSweep {
        LimitsSweep {
            pool,
            store,
            sender,
            issuer: "https://outer.now".to_string(),
            unsub_key: vec![0u8; 32],
            billing: crate::billing::BillingState::Disabled {
                platform_base: "https://outer.now".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn rollup_never_double_counts_a_minute() {
        let pool = pool().await;
        seed_user(&pool, "a1", "free").await;
        seed_app(&pool, "app1", "shop", "a1").await;
        let minute = 1_700_000_000_000i64 / 60_000;
        sqlx::query("INSERT INTO metric_minute (app_id, minute, requests) VALUES ('app1', ?1, 7)")
            .bind(minute)
            .execute(&pool)
            .await
            .unwrap();
        let month = "2024-01".to_string();
        assert_eq!(rollup_requests(&pool, &month).await.unwrap(), 1);
        let total: i64 = sqlx::query_scalar(
            "SELECT requests FROM usage_month WHERE userId = 'a1' AND month = '2024-01'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(total, 7);
        // A second rollup counts nothing new.
        assert_eq!(rollup_requests(&pool, &month).await.unwrap(), 0);
        let total: i64 = sqlx::query_scalar(
            "SELECT requests FROM usage_month WHERE userId = 'a1' AND month = '2024-01'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(total, 7, "the watermark holds: no double count");
    }

    #[tokio::test]
    async fn d1_over_cap_freezes_and_back_under_unfreezes() {
        let pool = pool().await;
        seed_user(&pool, "a1", "free").await;
        seed_app(&pool, "app1", "shop", "a1").await;
        let store = crate::store::Store::memory();
        let (sender, _codes) = capture_sender();
        let deps = sweep_deps(pool.clone(), store, sender);
        // A fresh free app stays writable.
        assert_eq!(
            check_storage(&deps, 1_700_000_000_000).await.unwrap(),
            (0, 0)
        );
        assert!(!storage_frozen(&pool, "app1").await.unwrap());
        // A dump over the 100 MB Free D1 cap freezes the app read-only.
        sqlx::query(
            "INSERT INTO d1_backup (id, app_id, binding, store_key, created_at, size_bytes) \
             VALUES ('b1', 'app1', 'DB', 'outer/backups/shop/1-DB.sql', 1, 200 * 1024 * 1024)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            check_storage(&deps, 1_700_000_000_000).await.unwrap(),
            (1, 0)
        );
        assert!(storage_frozen(&pool, "app1").await.unwrap());
        // Re-running while over the cap does not re-freeze (no second mail).
        assert_eq!(
            check_storage(&deps, 1_700_000_000_001).await.unwrap(),
            (0, 0)
        );
        // Back under the cap unfreezes.
        sqlx::query("DELETE FROM d1_backup WHERE id = 'b1'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            check_storage(&deps, 1_700_000_000_002).await.unwrap(),
            (0, 1)
        );
        assert!(!storage_frozen(&pool, "app1").await.unwrap());
    }

    fn capture_sender() -> (
        crate::auth::EmailSender,
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    ) {
        let codes = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        (
            crate::auth::EmailSender::Capture {
                codes: codes.clone(),
            },
            codes,
        )
    }
}
