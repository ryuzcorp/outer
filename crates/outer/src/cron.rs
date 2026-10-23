//! Cron wakes (spec §12.3): `outer` keeps each app's `triggers.crons`
//! from its deployed wrangler config in `app_cron` and wakes the app
//! shortly before each due time. The running fleet then fires the
//! Worker's `scheduled` handler itself; a sleeping app fires nothing.
//!
//! The tick loop wakes due apps through
//! [`FleetManager::ensure_awake`](crate::fleet::manager::FleetManager::ensure_awake).
//! A wake counts as activity, so an app with a frequent cron stays awake —
//! there is no plan limit on cron frequency (spec §3 lists one, but the
//! deploy pipeline enforces no plan limits yet).

use std::collections::HashMap;
use std::str::FromStr as _;
use std::time::Duration;

use chrono::DateTime;
use croner::Cron;
use sqlx::SqlitePool;

use crate::apps::{self, App};
use crate::fleet::manager::FleetManager;

/// How often the cron loop looks for due apps.
pub const TICK_EVERY: Duration = Duration::from_secs(15);
/// How far ahead of a due time the loop wakes the app, so the fleet is
/// running when the cron fires.
pub const WAKE_LEAD: Duration = Duration::from_secs(90);

/// Parse one Cloudflare cron expression: 5 fields, UTC. Cloudflare also
/// allows `L`, `W` and `#`, which croner supports in the day fields.
fn parse(expr: &str) -> anyhow::Result<Cron> {
    if expr.split_whitespace().count() != 5 {
        anyhow::bail!("invalid cron expression {expr:?}: Cloudflare crons have 5 fields (minute hour day-of-month month day-of-week)");
    }
    Cron::from_str(expr).map_err(|err| anyhow::anyhow!("invalid cron expression {expr:?}: {err}"))
}

/// The `triggers.crons` list from a deployed wrangler config, validated.
/// Missing `triggers` (or missing `crons`) means no schedules. Anything
/// present but misshapen fails the deploy, since it could never fire live.
pub fn crons_from_config(value: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    let Some(triggers) = value.get("triggers") else {
        return Ok(Vec::new());
    };
    let Some(crons) = triggers.get("crons") else {
        return Ok(Vec::new());
    };
    let Some(list) = crons.as_array() else {
        anyhow::bail!("wrangler triggers.crons must be an array of strings");
    };
    let mut out = Vec::with_capacity(list.len());
    for cron in list {
        let Some(expr) = cron.as_str() else {
            anyhow::bail!("wrangler triggers.crons must be an array of strings");
        };
        parse(expr)?;
        if !out.contains(&expr.to_string()) {
            out.push(expr.to_string());
        }
    }
    Ok(out)
}

/// The first firing of `expr` strictly after `after_ms` (unix millis, UTC).
/// Strictly-after means advancing past a fired due time never re-fires it.
pub fn next_after(expr: &str, after_ms: i64) -> anyhow::Result<i64> {
    let cron = parse(expr)?;
    let Some(after) = DateTime::from_timestamp_millis(after_ms) else {
        anyhow::bail!("timestamp {after_ms} is out of range");
    };
    let next: chrono::DateTime<chrono::Utc> = cron
        .find_next_occurrence(&after, false)
        .map_err(|err| anyhow::anyhow!("invalid cron expression {expr:?}: {err}"))?;
    Ok(next.timestamp_millis())
}

/// Replace the app's rows with `exprs`, computing each `next_at` from
/// `now_ms`. An empty list deletes the app's rows. One transaction, so a
/// redeploy never leaves a mix of old and new schedules.
pub async fn sync_app_crons(
    pool: &SqlitePool,
    app_id: &str,
    exprs: &[String],
    now_ms: i64,
) -> anyhow::Result<()> {
    let mut rows = Vec::with_capacity(exprs.len());
    for expr in exprs {
        rows.push((expr, next_after(expr, now_ms)?));
    }
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM app_cron WHERE app_id = ?1")
        .bind(app_id)
        .execute(&mut *tx)
        .await?;
    for (expr, next_at) in rows {
        sqlx::query("INSERT INTO app_cron (app_id, expr, next_at) VALUES (?1, ?2, ?3)")
            .bind(app_id)
            .bind(expr)
            .bind(next_at)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// One `app_cron` row plus its app's slug, for the due scan.
#[derive(Debug, sqlx::FromRow)]
struct DueRow {
    app_id: String,
    next_at: i64,
}

/// Apps with a schedule due at or before `now_ms + lead_ms`, earliest due
/// time first, one entry per app. Only apps that would otherwise serve
/// requests: `desired_state` running, `moderation_state` ok, neither paused
/// nor expired. Deleted apps vanish through the join.
pub async fn due(pool: &SqlitePool, now_ms: i64, lead_ms: i64) -> anyhow::Result<Vec<(App, i64)>> {
    let horizon = now_ms.saturating_add(lead_ms);
    let rows: Vec<DueRow> = sqlx::query_as(
        "SELECT c.app_id AS app_id, c.next_at AS next_at \
         FROM app_cron AS c JOIN app AS a ON a.id = c.app_id \
         WHERE c.next_at <= ?1 \
           AND a.desired_state = 'running' \
           AND a.moderation_state = 'ok' \
           AND a.paused_reason IS NULL \
           AND a.expired_at IS NULL \
         ORDER BY c.next_at",
    )
    .bind(horizon)
    .fetch_all(pool)
    .await?;
    let mut earliest: HashMap<&str, i64> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for row in &rows {
        if let std::collections::hash_map::Entry::Vacant(entry) =
            earliest.entry(row.app_id.as_str())
        {
            entry.insert(row.next_at);
            order.push(row.app_id.as_str());
        }
    }
    let mut out = Vec::with_capacity(order.len());
    for app_id in order {
        let Some(app) = apps::get_by_id(pool, app_id).await? else {
            continue;
        };
        out.push((app, earliest[app_id]));
    }
    Ok(out)
}

/// This app's `(expr, next_at)` rows, in expression order.
pub async fn app_rows(pool: &SqlitePool, app_id: &str) -> anyhow::Result<Vec<(String, i64)>> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT expr, next_at FROM app_cron WHERE app_id = ?1 ORDER BY expr")
            .bind(app_id)
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

/// Move one row past its fired due time: `next_at` becomes the first
/// occurrence strictly after `after_ms`. When that occurrence already
/// passed — outer was down past the due time — the missed window is skipped
/// with a warn log and `next_at` jumps to the first occurrence after now,
/// so there is no catch-up storm.
pub async fn advance(
    pool: &SqlitePool,
    app_id: &str,
    expr: &str,
    after_ms: i64,
) -> anyhow::Result<()> {
    let direct = next_after(expr, after_ms)?;
    let now = apps::now_ms();
    let next_at = if direct <= now {
        tracing::warn!(
            app_id,
            expr,
            after_ms,
            "cron due time missed while outer was down; skipping the missed window"
        );
        next_after(expr, now)?
    } else {
        direct
    };
    sqlx::query("UPDATE app_cron SET next_at = ?1 WHERE app_id = ?2 AND expr = ?3")
        .bind(next_at)
        .bind(app_id)
        .bind(expr)
        .execute(pool)
        .await?;
    Ok(())
}

/// One tick: wake every due app, then advance its fired rows past their due
/// times. A failed wake leaves the rows alone, so the next tick retries; a
/// failed advance for one row never blocks the other apps.
async fn tick(pool: &SqlitePool, fleets: &FleetManager, lead: Duration) -> anyhow::Result<()> {
    let now = apps::now_ms();
    let lead_ms = i64::try_from(lead.as_millis()).unwrap_or(i64::MAX);
    let horizon = now.saturating_add(lead_ms);
    for (app, _) in due(pool, now, lead_ms).await? {
        let rows = app_rows(pool, &app.id).await?;
        let firing: Vec<(String, i64)> = rows
            .into_iter()
            .filter(|(_, next_at)| *next_at <= horizon)
            .collect();
        if firing.is_empty() {
            continue;
        }
        if let Err(err) = fleets.ensure_awake(pool, &app).await {
            tracing::warn!(
                slug = %app.slug,
                error = %format!("{err:#}"),
                "cron wake failed; the next tick retries"
            );
            continue;
        }
        for (expr, next_at) in firing {
            if let Err(err) = advance(pool, &app.id, &expr, next_at).await {
                tracing::warn!(
                    slug = %app.slug,
                    expr,
                    error = %format!("{err:#}"),
                    "cron advance failed; the next tick retries"
                );
            }
        }
    }
    Ok(())
}

/// Wake apps shortly before their cron due times. Failures log and the loop
/// continues on the next tick.
pub fn spawn_cron_task(pool: SqlitePool, fleets: FleetManager, every: Duration, lead: Duration) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        loop {
            interval.tick().await;
            if let Err(err) = tick(&pool, &fleets, lead).await {
                tracing::warn!(error = %format!("{err:#}"), "cron tick failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60 * 1000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;

    /// Unix millis for a UTC date and time.
    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis()
    }

    async fn setup() -> (tempfile::TempDir, SqlitePool, String) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let owner = apps::ensure_account(&pool, "ana@example.com")
            .await
            .unwrap();
        (dir, pool, owner)
    }

    fn alloc() -> apps::Allocation {
        apps::Allocation {
            uid_range: (210000, 210009),
            port_range: (18000, 18019),
        }
    }

    async fn app(pool: &SqlitePool, owner: &str, slug: &str) -> App {
        apps::create(pool, owner, slug, None, &alloc(), 10)
            .await
            .unwrap()
    }

    #[test]
    fn quarter_hour_math_from_a_fixed_time() {
        // 2026-10-23 is a Friday; the clock value is fixed, not now.
        assert_eq!(
            next_after("*/15 * * * *", ms(2026, 10, 23, 0, 7)).unwrap(),
            ms(2026, 10, 23, 0, 15)
        );
        assert_eq!(
            next_after("*/15 * * * *", ms(2026, 10, 23, 0, 15)).unwrap(),
            ms(2026, 10, 23, 0, 30),
            "an exact firing instant advances strictly after itself"
        );
    }

    #[test]
    fn weekday_name_and_month_boundary() {
        // 2026-10-25 is a Sunday.
        assert_eq!(
            next_after("0 9 * * MON", ms(2026, 10, 25, 0, 0)).unwrap(),
            ms(2026, 10, 26, 9, 0)
        );
        assert_eq!(
            next_after("0 0 1 * *", ms(2026, 1, 31, 12, 0)).unwrap(),
            ms(2026, 2, 1, 0, 0),
            "the 1st-of-month schedule crosses the month boundary"
        );
    }

    #[test]
    fn bad_expressions_name_themselves() {
        for bad in ["not a cron", "*/15 * * *", "* * * * * *", "61 * * * *", ""] {
            let err = format!("{:?}", next_after(bad, 0).unwrap_err());
            assert!(
                err.contains(bad),
                "error names the expression {bad:?}: {err}"
            );
        }
        assert!(
            next_after("0 0 30 2 *", ms(2026, 1, 1, 0, 0)).is_err(),
            "February 30th never fires"
        );
    }

    #[test]
    fn config_extraction_validates_and_dedupes() {
        let value = serde_json::json!({"triggers": {"crons": ["*/15 * * * *", "0 9 * * MON", "*/15 * * * *"]}});
        assert_eq!(
            crons_from_config(&value).unwrap(),
            ["*/15 * * * *", "0 9 * * MON"]
        );
        assert_eq!(
            crons_from_config(&serde_json::json!({"name": "x"})).unwrap(),
            Vec::<String>::new(),
        );
        assert_eq!(
            crons_from_config(&serde_json::json!({"triggers": {}})).unwrap(),
            Vec::<String>::new(),
        );
        let bad = serde_json::json!({"triggers": {"crons": ["nope"]}});
        let err = format!("{:#}", crons_from_config(&bad).unwrap_err());
        assert!(err.contains("nope"), "invalid cron fails naming it: {err}");
        let bad_shape = serde_json::json!({"triggers": {"crons": "*/15 * * * *"}});
        assert!(crons_from_config(&bad_shape).is_err());
    }

    #[tokio::test]
    async fn sync_replaces_rows_and_empty_deletes() {
        let (_dir, pool, owner) = setup().await;
        let app = app(&pool, &owner, "grocery").await;
        let now = apps::now_ms();

        sync_app_crons(&pool, &app.id, &["*/15 * * * *".to_string()], now)
            .await
            .unwrap();
        sync_app_crons(
            &pool,
            &app.id,
            &["0 9 * * MON".to_string(), "0 0 1 * *".to_string()],
            now,
        )
        .await
        .unwrap();
        let rows = app_rows(&pool, &app.id).await.unwrap();
        assert_eq!(
            rows.iter().map(|(e, _)| e.clone()).collect::<Vec<_>>(),
            ["0 0 1 * *", "0 9 * * MON"],
            "redeploy replaces the old rows"
        );
        for (expr, next_at) in &rows {
            assert_eq!(*next_at, next_after(expr, now).unwrap());
        }

        sync_app_crons(&pool, &app.id, &[], now).await.unwrap();
        assert!(app_rows(&pool, &app.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn due_respects_lead_and_dedupes_per_app() {
        let (_dir, pool, owner) = setup().await;
        let a = app(&pool, &owner, "aaa").await;
        let b = app(&pool, &owner, "bbb").await;
        let now = apps::now_ms();

        sqlx::query("INSERT INTO app_cron (app_id, expr, next_at) VALUES (?1, ?2, ?3)")
            .bind(&a.id)
            .bind("0 0 * * *")
            .bind(now + 10 * MIN)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO app_cron (app_id, expr, next_at) VALUES (?1, ?2, ?3)")
            .bind(&a.id)
            .bind("0 12 * * *")
            .bind(now + 20 * MIN)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO app_cron (app_id, expr, next_at) VALUES (?1, ?2, ?3)")
            .bind(&b.id)
            .bind("0 0 * * *")
            .bind(now + 50 * MIN)
            .execute(&pool)
            .await
            .unwrap();

        let got = due(&pool, now, 30 * MIN).await.unwrap();
        assert_eq!(got.len(), 1, "only the app due inside the lead");
        assert_eq!(got[0].0.id, a.id);
        assert_eq!(got[0].1, now + 10 * MIN, "due_at is the earliest row");

        let got = due(&pool, now, 60 * MIN).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0.id, a.id, "earliest due app first");
        assert_eq!(got[1].0.id, b.id);
    }

    #[tokio::test]
    async fn advance_moves_past_the_due_time_and_skips_missed_windows() {
        let (_dir, pool, owner) = setup().await;
        let app = app(&pool, &owner, "grocery").await;
        let now = apps::now_ms();

        let due_at = next_after("* * * * *", now).unwrap();
        sqlx::query("INSERT INTO app_cron (app_id, expr, next_at) VALUES (?1, ?2, ?3)")
            .bind(&app.id)
            .bind("* * * * *")
            .bind(due_at)
            .execute(&pool)
            .await
            .unwrap();
        advance(&pool, &app.id, "* * * * *", due_at).await.unwrap();
        let rows = app_rows(&pool, &app.id).await.unwrap();
        assert_eq!(
            rows[0].1,
            next_after("* * * * *", due_at).unwrap(),
            "strictly after the fired due time, never re-firing it"
        );

        // Outer was down for a week past a due time: one jump past now.
        let stale = now - 7 * DAY;
        advance(&pool, &app.id, "* * * * *", stale).await.unwrap();
        let rows = app_rows(&pool, &app.id).await.unwrap();
        assert!(
            rows[0].1 > apps::now_ms(),
            "the missed week is skipped, not replayed minute by minute"
        );
        assert_eq!(
            rows[0].1,
            next_after("* * * * *", rows[0].1 - MIN).unwrap_or(rows[0].1),
            "lands on a real firing time"
        );
    }

    #[tokio::test]
    async fn due_skips_apps_that_must_not_run() {
        let (_dir, pool, owner) = setup().await;
        let now = apps::now_ms();
        let live = app(&pool, &owner, "live").await;

        let stopped = app(&pool, &owner, "stopped").await;
        let held = app(&pool, &owner, "held").await;
        let suspended = app(&pool, &owner, "suspended").await;
        let paused = app(&pool, &owner, "paused").await;
        let expired = app(&pool, &owner, "expired").await;
        for (id, sql) in [
            (
                &stopped.id,
                "UPDATE app SET desired_state = 'stopped' WHERE id = ?1",
            ),
            (
                &held.id,
                "UPDATE app SET moderation_state = 'held' WHERE id = ?1",
            ),
            (
                &suspended.id,
                "UPDATE app SET moderation_state = 'suspended' WHERE id = ?1",
            ),
            (
                &paused.id,
                "UPDATE app SET paused_reason = 'over quota' WHERE id = ?1",
            ),
            (&expired.id, "UPDATE app SET expired_at = ?2 WHERE id = ?1"),
        ] {
            sqlx::query(sql)
                .bind(id)
                .bind(now)
                .execute(&pool)
                .await
                .unwrap();
        }
        for id in [
            &live.id,
            &stopped.id,
            &held.id,
            &suspended.id,
            &paused.id,
            &expired.id,
        ] {
            sqlx::query(
                "INSERT INTO app_cron (app_id, expr, next_at) VALUES (?1, '0 * * * *', ?2)",
            )
            .bind(id)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        }

        let got = due(&pool, now, HOUR).await.unwrap();
        assert_eq!(
            got.iter().map(|(a, _)| a.slug.clone()).collect::<Vec<_>>(),
            ["live"],
            "stopped, held, suspended, paused and expired apps are not woken"
        );
        assert_eq!(got[0].0.id, live.id);
    }
}
