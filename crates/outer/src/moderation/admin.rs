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
         WHERE c.reviewed_by IS NULL AND c.stage != 'gate' AND c.verdict != 'pass' ORDER BY c.created_at DESC",
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
/// recent deploy health and the moderation queue depth.
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
          AND (newer.created_at, newer.rowid) > (deploy.created_at, deploy.rowid))",
    )
    .await?;
    let unreviewed_checks = count(
        pool,
        "SELECT COUNT(*) FROM moderation_check WHERE reviewed_by IS NULL AND verdict != 'pass'",
    )
    .await?;
    let week_ago = now - 7 * 24 * 60 * 60 * 1000;
    let week: Vec<(String, i64, Option<i64>)> =
        sqlx::query_as("SELECT status, created_at, finished_at FROM deploy WHERE created_at >= ?1")
            .bind(week_ago)
            .fetch_all(pool)
            .await?;
    let deploys_7d = week.len() as i64;
    let failed = week
        .iter()
        .filter(|(status, _, _)| status == "failed" || status == "blocked")
        .count() as i64;
    let mut durations: Vec<i64> = week
        .iter()
        .filter_map(|(_, created, finished)| finished.map(|at| (at - created).max(0)))
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
    })
}
