//! The admin moderation domain (spec §15.3 + §13 `admin_stats`).
//!
//! The single implementation behind the console Admin endpoints (and the
//! future MCP `admin_*` tools, which do not exist yet): the queue, review,
//! suspend/restore, ban and stats queries below own every SQL statement, so
//! no second copy may grow elsewhere. Endpoints add the guard, the fleet
//! side-effects and the audit rows; this module only reads and writes the
//! database.
//!
//! Queue semantics: a blocked deploy stays queued while it is its app's
//! latest deploy (a newer deploy supersedes it — the gate re-checked the
//! new text, so the old row is history, not work). Held/suspended apps and
//! unreviewed non-gate checks (crawl/report stages) list alongside.
//!
//! Beta validation signals (spec §1.3) ride inside [`Stats::beta`].
//!
//! Two honest limitations:
//! (1) `visitor_day` counts every distinct hashed IP per UTC day, owner
//! included — the database cannot tell the owner apart from a visitor, so
//! `shared_week1` is an upper bound;
//! (2) the attempt signal counts deploy attempts, not commits: `usage_day`
//! aggregates per user-day and cannot be attributed to one app, while
//! `deploy` is the durable per-app record.
//!
//! `shared_week1` reads the first seven days of `visitor_day`, which
//! [`crate::observe::metrics::prune`] keeps past retention for exactly this
//! reason. Apps created before that exemption existed (2026-11-06) may have
//! lost the rows under the Free 24 h retention and then read as not shared;
//! Pro apps kept 30 days, so theirs are complete.

use serde::Serialize;
use sqlx::SqlitePool;
use std::cmp::Reverse;

use crate::mcp::error::ToolError;

fn parse_json_or_null(raw: Option<String>) -> serde_json::Value {
    raw.and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// One blocked deploy with its latest gate check (`queue`).
type BlockedRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
);
/// The latest check context for a held/suspended app (`queue`).
type CheckRow = (Option<String>, Option<String>, Option<String>, Option<i64>);
/// One unreviewed non-gate check (`queue`).
type FlaggedRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    i64,
);

/// One actionable row on the moderation queue.
#[derive(Debug, Clone, Serialize)]
pub struct QueueItem {
    /// `blocked_deploy` | `held_app` | `suspended_app` | `flagged_check`.
    pub kind: String,
    /// The deploy id, app id or check id, matching `kind`.
    pub id: String,
    pub app_id: String,
    pub app_slug: String,
    pub owner_email: Option<String>,
    pub sha: Option<String>,
    pub reason: Option<String>,
    pub findings: serde_json::Value,
    pub scores: serde_json::Value,
    /// Who already looked (`deny` leaves the block in place).
    pub reviewed_by: Option<String>,
    pub reviewed_at: Option<i64>,
    pub created_at: i64,
}

/// The moderation queue: the latest blocked deploy per app, every
/// held/suspended app, and every unreviewed non-gate check, newest first.
pub async fn queue(pool: &SqlitePool) -> anyhow::Result<Vec<QueueItem>> {
    let mut items = Vec::new();
    // Blocked deploys that no newer deploy supersedes, with the latest gate
    // check's scores and review state (later checks sort first per deploy).
    let blocked: Vec<BlockedRow> = sqlx::query_as(
        "SELECT d.id, d.sha, d.findings, c.scores, d.created_at, \
                a.id, a.slug, u.email, c.reviewed_by, c.reviewed_at \
         FROM deploy d \
         JOIN app a ON a.id = d.app_id \
         JOIN user u ON u.id = a.owner_id \
         LEFT JOIN moderation_check c ON c.deploy_id = d.id AND c.stage = 'gate' \
         WHERE d.status = 'blocked' \
           AND NOT EXISTS (SELECT 1 FROM deploy newer \
                           WHERE newer.app_id = d.app_id \
                             AND (newer.created_at, newer.rowid) > (d.created_at, d.rowid)) \
         ORDER BY d.created_at DESC, c.created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    let mut seen_deploys = std::collections::HashSet::new();
    for (id, sha, findings, scores, created_at, app_id, slug, email, reviewed_by, reviewed_at) in
        blocked
    {
        if !seen_deploys.insert(id.clone()) {
            continue;
        }
        // Decided: an admin already reviewed this block, so it no longer
        // needs attention.
        if reviewed_by.is_some() {
            continue;
        }
        items.push(QueueItem {
            kind: "blocked_deploy".to_string(),
            id,
            app_id,
            app_slug: slug,
            owner_email: email,
            sha: Some(sha),
            reason: Some("blocked".to_string()),
            findings: parse_json_or_null(findings),
            scores: parse_json_or_null(scores),
            reviewed_by,
            reviewed_at,
            created_at,
        });
    }
    // Held and suspended apps, with their latest check (any stage) for
    // context when one exists.
    let held: Vec<(String, String, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT a.id, a.slug, a.moderation_state, u.email, a.created_at \
         FROM app a JOIN user u ON u.id = a.owner_id \
         WHERE a.moderation_state != 'ok' ORDER BY a.created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    for (app_id, slug, state, email, created_at) in held {
        let check: Option<CheckRow> = sqlx::query_as(
            "SELECT findings, scores, reviewed_by, reviewed_at FROM moderation_check \
                 WHERE app_id = ?1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&app_id)
        .fetch_optional(pool)
        .await?;
        let (findings, scores, reviewed_by, reviewed_at) =
            check.unwrap_or((None, None, None, None));
        items.push(QueueItem {
            kind: if state == "suspended" {
                "suspended_app".to_string()
            } else {
                "held_app".to_string()
            },
            id: app_id.clone(),
            app_id,
            app_slug: slug,
            owner_email: email,
            sha: None,
            reason: Some(state),
            findings: parse_json_or_null(findings),
            scores: parse_json_or_null(scores),
            reviewed_by,
            reviewed_at,
            created_at,
        });
    }
    // Unreviewed crawl/report checks: the after-live stages have no deploy
    // row of their own, so they queue on their own id.
    let flagged: Vec<FlaggedRow> = sqlx::query_as(
        "SELECT c.id, c.app_id, c.sha, c.findings, c.scores, a.slug, u.email, c.created_at \
         FROM moderation_check c \
         JOIN app a ON a.id = c.app_id JOIN user u ON u.id = a.owner_id \
         WHERE c.reviewed_by IS NULL AND c.stage != 'gate' AND c.verdict NOT IN ('pass', 'error') ORDER BY c.created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    for (id, app_id, sha, findings, scores, slug, email, created_at) in flagged {
        items.push(QueueItem {
            kind: "flagged_check".to_string(),
            id,
            app_id,
            app_slug: slug,
            owner_email: email,
            sha,
            reason: None,
            findings: parse_json_or_null(findings),
            scores: parse_json_or_null(scores),
            reviewed_by: None,
            reviewed_at: None,
            created_at,
        });
    }
    items.sort_by_key(|item| Reverse(item.created_at));
    Ok(items)
}

/// What is being reviewed: a blocked deploy (by deploy id) or a held app
/// (by slug). `allow` on a deploy clears that app's chunk hashes and asks
/// the caller to redeploy the blocked sha; on an app it lifts the hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewKind {
    Deploy,
    App,
}

/// Allow the publish/hold to lift, or deny it (the block stays, recorded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny,
}

/// The decision's database effects. `redeploy` + `sha` tell the caller to
/// run the blocked sha again through the shared deploy machinery.
#[derive(Debug, Clone)]
pub struct ReviewApplied {
    pub app_id: String,
    pub app_slug: String,
    pub sha: Option<String>,
    pub redeploy: bool,
}

pub async fn apply_review(
    pool: &SqlitePool,
    kind: ReviewKind,
    id: &str,
    verdict: Verdict,
    reviewer: &str,
    now: i64,
) -> Result<ReviewApplied, ToolError> {
    match kind {
        ReviewKind::Deploy => {
            let row: Option<(String, String, String, String, String)> = sqlx::query_as(
                "SELECT d.id, d.sha, d.status, d.app_id, a.slug FROM deploy d \
                 JOIN app a ON a.id = d.app_id WHERE d.id = ?1",
            )
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(|err| ToolError::internal(format!("review lookup failed: {err:#}")))?;
            let Some((deploy_id, sha, status, app_id, slug)) = row else {
                return Err(ToolError::not_found(format!("no deploy {id:?}")));
            };
            if status != "blocked" {
                return Err(ToolError::invalid_argument(format!(
                    "deploy {deploy_id:?} is {status}, only blocked deploys can be reviewed"
                )));
            }
            if verdict == Verdict::Allow {
                // Cleared chunks skip the gate, so the redeploy below lands
                // instead of blocking on the same text again.
                sqlx::query(
                    "UPDATE moderation_text SET verdict = 'allowed', checked_at = ?1 \
                     WHERE app_id = ?2",
                )
                .bind(now)
                .bind(&app_id)
                .execute(pool)
                .await
                .map_err(|err| ToolError::internal(format!("review update failed: {err:#}")))?;
            }
            sqlx::query(
                "UPDATE moderation_check SET reviewed_by = ?1, reviewed_at = ?2 \
                 WHERE deploy_id = ?3 AND stage = 'gate'",
            )
            .bind(reviewer)
            .bind(now)
            .bind(&deploy_id)
            .execute(pool)
            .await
            .map_err(|err| ToolError::internal(format!("review update failed: {err:#}")))?;
            Ok(ReviewApplied {
                app_id,
                app_slug: slug,
                sha: Some(sha),
                redeploy: verdict == Verdict::Allow,
            })
        }
        ReviewKind::App => {
            let row: Option<(String, String, String)> =
                sqlx::query_as("SELECT id, slug, moderation_state FROM app WHERE slug = ?1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await
                    .map_err(|err| ToolError::internal(format!("review lookup failed: {err:#}")))?;
            let Some((app_id, slug, _state)) = row else {
                return Err(ToolError::not_found(format!("no app {id:?}")));
            };
            if verdict == Verdict::Allow {
                sqlx::query("UPDATE app SET moderation_state = 'ok' WHERE id = ?1")
                    .bind(&app_id)
                    .execute(pool)
                    .await
                    .map_err(|err| ToolError::internal(format!("review update failed: {err:#}")))?;
            }
            sqlx::query(
                "UPDATE moderation_check SET reviewed_by = ?1, reviewed_at = ?2 \
                 WHERE app_id = ?3 AND reviewed_by IS NULL",
            )
            .bind(reviewer)
            .bind(now)
            .bind(&app_id)
            .execute(pool)
            .await
            .map_err(|err| ToolError::internal(format!("review update failed: {err:#}")))?;
            Ok(ReviewApplied {
                app_id,
                app_slug: slug,
                sha: None,
                redeploy: false,
            })
        }
    }
}

/// Flip an app's moderation state (`suspended` or back to `ok`). The caller
/// stops the fleet and purges the edge on suspend; the wake sweep already
/// skips non-`ok` apps, so restore needs nothing else.
pub async fn set_moderation_state(
    pool: &SqlitePool,
    slug: &str,
    state: &str,
) -> Result<(String, String), ToolError> {
    if state != "suspended" && state != "ok" {
        return Err(ToolError::invalid_argument(format!(
            "state must be suspended or ok, got {state:?}"
        )));
    }
    let done = sqlx::query("UPDATE app SET moderation_state = ?1 WHERE slug = ?2")
        .bind(state)
        .bind(slug)
        .execute(pool)
        .await
        .map_err(|err| ToolError::internal(format!("moderation update failed: {err:#}")))?;
    if done.rows_affected() == 0 {
        return Err(ToolError::not_found(format!("no app {slug:?}")));
    }
    let app_id: String = sqlx::query_scalar("SELECT id FROM app WHERE slug = ?1")
        .bind(slug)
        .fetch_one(pool)
        .await
        .map_err(|err| ToolError::internal(format!("moderation lookup failed: {err:#}")))?;
    Ok((app_id, slug.to_string()))
}

/// The outcome of a ban: whose apps the caller must stop.
#[derive(Debug, Clone)]
pub struct BanOutcome {
    pub user_id: String,
    pub email: String,
    pub app_slugs: Vec<String>,
}

/// Ban an account (spec §15.3): sign-in and sessions already refuse banned
/// rows, so the ban flips the flag and parks every owned app as `stopped`.
/// The caller stops the live fleets; restore is per-app plus clearing the
/// flag elsewhere (no self-serve unban here).
pub async fn ban_account(
    pool: &SqlitePool,
    email_or_id: &str,
    reason: &str,
) -> Result<BanOutcome, ToolError> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(ToolError::invalid_argument(
            "a ban reason is required".to_string(),
        ));
    }
    let reason: String = reason.chars().take(500).collect();
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id, email FROM user WHERE email = ?1 OR id = ?1")
            .bind(email_or_id)
            .fetch_optional(pool)
            .await
            .map_err(|err| ToolError::internal(format!("ban lookup failed: {err:#}")))?;
    let Some((user_id, email)) = row else {
        return Err(ToolError::not_found(format!("no account {email_or_id:?}")));
    };
    sqlx::query("UPDATE user SET banned = 1, banReason = ?1, banExpires = NULL WHERE id = ?2")
        .bind(&reason)
        .bind(&user_id)
        .execute(pool)
        .await
        .map_err(|err| ToolError::internal(format!("ban update failed: {err:#}")))?;
    let slugs: Vec<(String,)> =
        sqlx::query_as("SELECT slug FROM app WHERE owner_id = ?1 ORDER BY slug")
            .bind(&user_id)
            .fetch_all(pool)
            .await
            .map_err(|err| ToolError::internal(format!("ban lookup failed: {err:#}")))?;
    let app_slugs: Vec<String> = slugs.into_iter().map(|(slug,)| slug).collect();
    sqlx::query("UPDATE app SET desired_state = 'stopped' WHERE owner_id = ?1")
        .bind(&user_id)
        .execute(pool)
        .await
        .map_err(|err| ToolError::internal(format!("ban update failed: {err:#}")))?;
    Ok(BanOutcome {
        user_id,
        email,
        app_slugs,
    })
}

/// One plan row of the overview.
#[derive(Debug, Clone, Serialize)]
pub struct PlanCount {
    pub plan: String,
    pub accounts: i64,
}

/// The `admin_stats` overview (spec §13): accounts, waitlist, fleets,
/// recent deploy health, the moderation queue depth and the spec §1.3
/// beta validation signals in `beta`.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub accounts_total: i64,
    pub accounts_by_plan: Vec<PlanCount>,
    pub waitlist_size: i64,
    pub apps_total: i64,
    pub apps_awake: i64,
    pub apps_asleep: i64,
    pub apps_held: i64,
    pub apps_suspended: i64,
    pub blocked_deploys: i64,
    pub unreviewed_checks: i64,
    pub deploys_7d: i64,
    pub deploy_failure_rate_7d: f64,
    pub deploy_p50_ms_7d: Option<i64>,
    pub deploy_p95_ms_7d: Option<i64>,
    pub beta: BetaSignals,
}

/// numerator/denominator for one §1.3 signal. `pct` is 0.0 when `of == 0`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Rate {
    pub met: i64,
    pub of: i64,
    pub pct: f64,
}

/// The spec §1.3 validation signals, measured from the tables the beta
/// already writes. Windows that have not elapsed are excluded from the
/// denominator so a fresh sign-up cannot depress the rate.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BetaSignals {
    pub first_session: Rate,
    pub shared_week1: Rate,
    pub return_week2: Rate,
    pub launch_price: Rate,
    pub build_failures_7d: i64,
    pub build_failure_rate_7d: f64,
    pub attempts_before_first_success: Option<f64>,
}

/// One entry of the deploy `steps` JSON (`[{name, ms, skipped}]`).
#[derive(Debug, serde::Deserialize)]
struct DeployStep {
    name: String,
}

fn rate(met: i64, of: i64) -> Rate {
    Rate {
        met,
        of,
        pct: if of == 0 {
            0.0
        } else {
            100.0 * met as f64 / of as f64
        },
    }
}

async fn count(pool: &SqlitePool, sql: &'static str) -> anyhow::Result<i64> {
    let (n,): (i64,) = sqlx::query_as(sql).fetch_one(pool).await?;
    Ok(n)
}

pub async fn stats(pool: &SqlitePool, now: i64) -> anyhow::Result<Stats> {
    let accounts_total = count(pool, "SELECT COUNT(*) FROM user").await?;
    let plans: Vec<(String, i64)> =
        sqlx::query_as("SELECT plan, COUNT(*) FROM user GROUP BY plan ORDER BY plan")
            .fetch_all(pool)
            .await?;
    let waitlist_size = count(pool, "SELECT COUNT(*) FROM waitlist").await?;
    let apps_total = count(pool, "SELECT COUNT(*) FROM app").await?;
    let apps_awake = count(pool, "SELECT COUNT(*) FROM app WHERE asleep_since IS NULL").await?;
    let apps_asleep = apps_total - apps_awake;
    let apps_held = count(
        pool,
        "SELECT COUNT(*) FROM app WHERE moderation_state = 'held'",
    )
    .await?;
    let apps_suspended = count(
        pool,
        "SELECT COUNT(*) FROM app WHERE moderation_state = 'suspended'",
    )
    .await?;
    let blocked_deploys = count(
        pool,
        "SELECT COUNT(*) FROM deploy WHERE status = 'blocked' AND NOT EXISTS \
         (SELECT 1 FROM deploy newer WHERE newer.app_id = deploy.app_id \
          AND (newer.created_at, newer.rowid) > (deploy.created_at, deploy.rowid)) \
         AND NOT EXISTS (SELECT 1 FROM moderation_check c \
          WHERE c.deploy_id = deploy.id AND c.stage = 'gate' AND c.reviewed_by IS NOT NULL)",
    )
    .await?;
    let unreviewed_checks = count(
        pool,
        "SELECT COUNT(*) FROM moderation_check \
         WHERE reviewed_by IS NULL AND stage != 'gate' AND verdict != 'pass'",
    )
    .await?;
    let week_ago = now - 7 * 24 * 60 * 60 * 1000;
    let week: Vec<(String, i64, Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT status, created_at, finished_at, steps FROM deploy WHERE created_at >= ?1",
    )
    .bind(week_ago)
    .fetch_all(pool)
    .await?;
    let deploys_7d = week.len() as i64;
    let failed = week
        .iter()
        .filter(|(status, _, _, _)| status == "failed" || status == "blocked")
        .count() as i64;
    let build_failures_7d = week
        .iter()
        .filter(|(status, _, _, steps)| {
            if status != "failed" {
                return false;
            }
            // The pipeline pushes the failing step before returning, so the
            // last entry names the stage that failed.
            let Some(raw) = steps.as_deref() else {
                return false;
            };
            let Ok(parsed) = serde_json::from_str::<Vec<DeployStep>>(raw) else {
                return false;
            };
            matches!(
                parsed.last().map(|s| s.name.as_str()),
                Some("worktree" | "install" | "build")
            )
        })
        .count() as i64;
    let build_failure_rate_7d = if deploys_7d == 0 {
        0.0
    } else {
        build_failures_7d as f64 / deploys_7d as f64
    };
    let mut durations: Vec<i64> = week
        .iter()
        .filter_map(|(_, created, finished, _)| finished.map(|at| (at - created).max(0)))
        .collect();
    durations.sort_unstable();
    let percentile = |p: f64| {
        if durations.is_empty() {
            None
        } else {
            let at = ((p * durations.len() as f64).ceil() as usize).saturating_sub(1);
            Some(durations[at.min(durations.len() - 1)])
        }
    };
    let (first_of, first_met): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(CASE WHEN EXISTS (SELECT 1 FROM deploy d JOIN app a ON a.id = d.app_id \
         WHERE a.owner_id = u.id AND d.status = 'success' AND d.created_at <= u.createdAt + 86400000) \
         THEN 1 ELSE 0 END), 0) FROM user u WHERE u.createdAt <= ?1 - 86400000",
    )
    .bind(now)
    .fetch_one(pool)
    .await?;
    let (shared_of, shared_met): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(CASE WHEN (SELECT COALESCE(SUM(v.visitors), 0) FROM visitor_day v \
         WHERE v.app_id = a.id AND v.day >= a.created_at / 86400000 AND v.day < a.created_at / 86400000 + 7) >= 3 \
         THEN 1 ELSE 0 END), 0) FROM app a WHERE a.created_at <= ?1 - 604800000",
    )
    .bind(now)
    .fetch_one(pool)
    .await?;
    let (return_of, return_met): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(CASE WHEN EXISTS (SELECT 1 FROM usage_day ud WHERE ud.userId = u.id \
         AND ud.code_changes > 0 AND ud.day >= date(u.createdAt / 1000, 'unixepoch', '+7 days') \
         AND ud.day < date(u.createdAt / 1000, 'unixepoch', '+14 days')) THEN 1 ELSE 0 END), 0) \
         FROM user u WHERE u.createdAt <= ?1 - 1209600000",
    )
    .bind(now)
    .fetch_one(pool)
    .await?;
    let (launch_met, launch_of): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(CASE WHEN plan = 'pro' AND planStatus = 'active' THEN 1 ELSE 0 END), 0), \
         COUNT(*) FROM user",
    )
    .fetch_one(pool)
    .await?;
    // The first success is picked by `(created_at, rowid)`: two deploys can
    // share a millisecond, and the later one must not count as an attempt
    // before the first success.
    let attempts: Vec<(i64,)> = sqlx::query_as(
        "WITH first AS (SELECT app_id, MIN(created_at) AS t FROM deploy WHERE status = 'success' \
         GROUP BY app_id), \
         picked AS (SELECT f.app_id, f.t, MIN(d.rowid) AS r FROM first f JOIN deploy d \
         ON d.app_id = f.app_id AND d.created_at = f.t AND d.status = 'success' GROUP BY f.app_id) \
         SELECT (SELECT COUNT(*) FROM deploy d WHERE d.app_id = picked.app_id \
         AND (d.created_at < picked.t OR (d.created_at = picked.t AND d.rowid <= picked.r))) \
         FROM picked",
    )
    .fetch_all(pool)
    .await?;
    let mut attempts: Vec<i64> = attempts.into_iter().map(|(n,)| n).collect();
    attempts.sort_unstable();
    let attempts_before_first_success = if attempts.is_empty() {
        None
    } else if attempts.len() % 2 == 1 {
        Some(attempts[attempts.len() / 2] as f64)
    } else {
        let mid = attempts.len() / 2;
        Some((attempts[mid - 1] + attempts[mid]) as f64 / 2.0)
    };
    let beta = BetaSignals {
        first_session: rate(first_met, first_of),
        shared_week1: rate(shared_met, shared_of),
        return_week2: rate(return_met, return_of),
        launch_price: rate(launch_met, launch_of),
        build_failures_7d,
        build_failure_rate_7d,
        attempts_before_first_success,
    };
    Ok(Stats {
        accounts_total,
        accounts_by_plan: plans
            .into_iter()
            .map(|(plan, accounts)| PlanCount { plan, accounts })
            .collect(),
        waitlist_size,
        apps_total,
        apps_awake,
        apps_asleep,
        apps_held,
        apps_suspended,
        blocked_deploys,
        unreviewed_checks,
        deploys_7d,
        deploy_failure_rate_7d: if deploys_7d == 0 {
            0.0
        } else {
            failed as f64 / deploys_7d as f64
        },
        deploy_p50_ms_7d: percentile(0.5),
        deploy_p95_ms_7d: percentile(0.95),
        beta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::test_pool;

    const DAY_MS: i64 = 86_400_000;

    async fn seed_user(
        pool: &SqlitePool,
        id: &str,
        email: &str,
        created_at: i64,
        plan: &str,
        plan_status: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt, plan, planStatus) \
             VALUES (?1, ?2, ?3, ?3, ?3, ?4, ?5)",
        )
        .bind(id)
        .bind(email)
        .bind(created_at)
        .bind(plan)
        .bind(plan_status)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_app(
        pool: &SqlitePool,
        id: &str,
        slug: &str,
        owner: &str,
        created_at: i64,
        n: i64,
    ) {
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        )
        .bind(id)
        .bind(slug)
        .bind(owner)
        .bind(400_000 + n)
        .bind(28_000 + n)
        .bind(28_001 + n)
        .bind(created_at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_deploy(
        pool: &SqlitePool,
        id: &str,
        app: &str,
        status: &str,
        steps: Option<&str>,
        created_at: i64,
    ) {
        sqlx::query(
            "INSERT INTO deploy (id, app_id, sha, status, steps, created_at, finished_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        )
        .bind(id)
        .bind(app)
        .bind(format!("sha-{id}"))
        .bind(status)
        .bind(steps)
        .bind(created_at)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn first_session_counts_success_within_24h() {
        let pool = test_pool().await;
        let now = 1_750_000_000_000;
        seed_user(
            &pool,
            "u-met",
            "met@x.test",
            now - 48 * 3_600_000,
            "free",
            None,
        )
        .await;
        seed_user(
            &pool,
            "u-late",
            "late@x.test",
            now - 48 * 3_600_000,
            "free",
            None,
        )
        .await;
        seed_user(
            &pool,
            "u-new",
            "new@x.test",
            now - 2 * 3_600_000,
            "free",
            None,
        )
        .await;
        seed_app(&pool, "a-met", "met", "u-met", now - 48 * 3_600_000, 1).await;
        seed_app(&pool, "a-late", "late", "u-late", now - 48 * 3_600_000, 2).await;
        let t0 = now - 48 * 3_600_000;
        seed_deploy(&pool, "d-met", "a-met", "success", None, t0 + 3_600_000).await;
        seed_deploy(
            &pool,
            "d-late",
            "a-late",
            "success",
            None,
            t0 + 30 * 3_600_000,
        )
        .await;
        let s = stats(&pool, now).await.unwrap();
        assert_eq!(
            s.beta.first_session,
            Rate {
                met: 1,
                of: 2,
                pct: 50.0
            }
        );
    }

    #[tokio::test]
    async fn shared_week1_sums_first_seven_days() {
        let pool = test_pool().await;
        let now = 1_750_000_000_000;
        seed_user(&pool, "u-1", "one@x.test", now - 20 * DAY_MS, "free", None).await;
        let old = now - 10 * DAY_MS;
        let fresh = now - 3 * DAY_MS;
        seed_app(&pool, "a-met", "met", "u-1", old, 1).await;
        seed_app(&pool, "a-quiet", "quiet", "u-1", old, 2).await;
        seed_app(&pool, "a-fresh", "fresh", "u-1", fresh, 3).await;
        let d0 = old.div_euclid(DAY_MS);
        for (app, day, visitors) in [
            ("a-met", d0, 3),
            ("a-met", d0 + 6, 1),
            ("a-quiet", d0, 2),
            ("a-quiet", d0 + 10, 5),
        ] {
            sqlx::query("INSERT INTO visitor_day (app_id, day, visitors) VALUES (?1, ?2, ?3)")
                .bind(app)
                .bind(day)
                .bind(visitors)
                .execute(&pool)
                .await
                .unwrap();
        }
        let s = stats(&pool, now).await.unwrap();
        assert_eq!(
            s.beta.shared_week1,
            Rate {
                met: 1,
                of: 2,
                pct: 50.0
            }
        );
    }

    #[tokio::test]
    async fn return_week2_needs_code_changes_in_second_week() {
        let pool = test_pool().await;
        let now = 1_750_000_000_000;
        let old = now - 15 * DAY_MS;
        let young = now - 10 * DAY_MS;
        seed_user(&pool, "u-met", "met@x.test", old, "free", None).await;
        seed_user(&pool, "u-early", "early@x.test", old, "free", None).await;
        seed_user(&pool, "u-young", "young@x.test", young, "free", None).await;
        for (user, created, offset) in [
            ("u-met", old, "+8 days"),
            ("u-early", old, "+3 days"),
            ("u-young", young, "+8 days"),
        ] {
            sqlx::query(
                "INSERT INTO usage_day (userId, day, code_changes) \
                 VALUES (?1, date(?2 / 1000, 'unixepoch', ?3), 1)",
            )
            .bind(user)
            .bind(created)
            .bind(offset)
            .execute(&pool)
            .await
            .unwrap();
        }
        let s = stats(&pool, now).await.unwrap();
        assert_eq!(
            s.beta.return_week2,
            Rate {
                met: 1,
                of: 2,
                pct: 50.0
            }
        );
    }

    #[tokio::test]
    async fn build_failures_count_last_step_in_worktree_install_build() {
        let pool = test_pool().await;
        let now = 1_750_000_000_000;
        seed_user(&pool, "u-1", "one@x.test", now - 20 * DAY_MS, "free", None).await;
        seed_app(&pool, "a-1", "one", "u-1", now - 20 * DAY_MS, 1).await;
        let build = r#"[{"name":"install","ms":1},{"name":"build","ms":2}]"#;
        let migrate = r#"[{"name":"build","ms":1},{"name":"migrate","ms":2}]"#;
        seed_deploy(&pool, "d-build", "a-1", "failed", Some(build), now - DAY_MS).await;
        seed_deploy(
            &pool,
            "d-migrate",
            "a-1",
            "failed",
            Some(migrate),
            now - DAY_MS,
        )
        .await;
        seed_deploy(&pool, "d-null", "a-1", "failed", None, now - DAY_MS).await;
        seed_deploy(
            &pool,
            "d-old",
            "a-1",
            "failed",
            Some(build),
            now - 8 * DAY_MS,
        )
        .await;
        let s = stats(&pool, now).await.unwrap();
        assert_eq!(s.beta.build_failures_7d, 1);
        assert_eq!(s.deploys_7d, 3);
        assert!((s.beta.build_failure_rate_7d - 1.0 / 3.0).abs() < 1e-12);
    }

    #[tokio::test]
    async fn attempts_before_first_success_takes_median() {
        let pool = test_pool().await;
        let now = 1_750_000_000_000;
        seed_user(&pool, "u-1", "one@x.test", now - 20 * DAY_MS, "free", None).await;
        seed_app(&pool, "a-one", "one", "u-1", now - 20 * DAY_MS, 1).await;
        seed_app(&pool, "a-three", "three", "u-1", now - 20 * DAY_MS, 2).await;
        seed_deploy(&pool, "d-a1", "a-one", "success", None, now - 3 * DAY_MS).await;
        seed_deploy(&pool, "d-b1", "a-three", "failed", None, now - 3 * DAY_MS).await;
        seed_deploy(&pool, "d-b2", "a-three", "failed", None, now - 2 * DAY_MS).await;
        seed_deploy(&pool, "d-b3", "a-three", "success", None, now - DAY_MS).await;
        let s = stats(&pool, now).await.unwrap();
        assert_eq!(s.beta.attempts_before_first_success, Some(2.0));

        let empty = test_pool().await;
        let s = stats(&empty, now).await.unwrap();
        assert_eq!(s.beta.attempts_before_first_success, None);
    }

    #[tokio::test]
    async fn launch_price_counts_active_pro_over_every_account() {
        let pool = test_pool().await;
        let now = 1_750_000_000_000;
        seed_user(
            &pool,
            "u-pro",
            "pro@x.test",
            now - 20 * DAY_MS,
            "pro",
            Some("active"),
        )
        .await;
        seed_user(
            &pool,
            "u-canceled",
            "canceled@x.test",
            now - 20 * DAY_MS,
            "pro",
            Some("canceled"),
        )
        .await;
        seed_user(
            &pool,
            "u-free",
            "free@x.test",
            now - 20 * DAY_MS,
            "free",
            None,
        )
        .await;
        let s = stats(&pool, now).await.unwrap();
        assert_eq!(
            s.beta.launch_price,
            Rate {
                met: 1,
                of: 3,
                pct: 100.0 / 3.0
            }
        );
    }

    #[tokio::test]
    async fn attempts_before_first_success_breaks_timestamp_ties_by_rowid() {
        let pool = test_pool().await;
        let now = 1_750_000_000_000;
        seed_user(&pool, "u-1", "one@x.test", now - 20 * DAY_MS, "free", None).await;
        seed_app(&pool, "a-tie", "tie", "u-1", now - 20 * DAY_MS, 1).await;
        seed_app(&pool, "a-early", "early", "u-1", now - 20 * DAY_MS, 2).await;
        // Same millisecond: the success is inserted first, so the failed row
        // after it is not an attempt made before the first success.
        seed_deploy(&pool, "d-tie-ok", "a-tie", "success", None, now - DAY_MS).await;
        seed_deploy(&pool, "d-tie-bad", "a-tie", "failed", None, now - DAY_MS).await;
        // Same millisecond the other way round: two attempts.
        seed_deploy(
            &pool,
            "d-early-bad",
            "a-early",
            "failed",
            None,
            now - DAY_MS,
        )
        .await;
        seed_deploy(
            &pool,
            "d-early-ok",
            "a-early",
            "success",
            None,
            now - DAY_MS,
        )
        .await;
        let s = stats(&pool, now).await.unwrap();
        assert_eq!(s.beta.attempts_before_first_success, Some(1.5));
    }

    #[tokio::test]
    async fn stats_json_carries_beta_wire_contract() {
        let pool = test_pool().await;
        let s = stats(&pool, 1_750_000_000_000).await.unwrap();
        let v = serde_json::to_value(&s).unwrap();
        let beta = &v["beta"];
        for key in [
            "first_session",
            "shared_week1",
            "return_week2",
            "launch_price",
        ] {
            assert!(beta[key]["met"].is_number(), "{key}.met");
            assert!(beta[key]["of"].is_number(), "{key}.of");
            assert!(beta[key]["pct"].is_number(), "{key}.pct");
        }
        assert!(beta["build_failures_7d"].is_number());
        assert!(beta["build_failure_rate_7d"].is_number());
        assert!(beta.get("attempts_before_first_success").is_some());
    }
}
