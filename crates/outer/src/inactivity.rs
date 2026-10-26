//! Inactive-app deletion (spec §7.8): Free apps whose owner went quiet are
//! reminded, then stopped with an `expired` page, then purged.
//!
//! The clock is owner activity only: any authenticated request by the owner
//! (OAuth or OTP sign-in, any MCP tool call, any Console session request)
//! stamps `user.lastActiveAt`, and a collaborator's tool call on an app
//! stamps that app's `last_activity_at`. Visitor traffic never touches
//! either column (the proxy only writes `last_request_at`). Both stamps are
//! throttled to one write per hour per account/app, so hot paths don't write
//! on every request.
//!
//! One daily pass ([`run_sweep`], called from the sleep sweep's daily block)
//! moves each Free-owned app along: reminders at 30, 7 and 1 day before the
//! deadline (tracked once each in the `app.inactivity_notices` bitmask, so a
//! crash between the send and the stamp only risks one duplicate), expiry at
//! the deadline (fleet stopped, edge cache purged, `expired_at` stamped —
//! backdated to the deadline when the sweep is catching up, so the purge
//! date never drifts), restore while the grace window is open (any activity
//! past the stamp clears it and the notice bits), and purge through the
//! shared `delete_app` path once the grace window closes. Downgrade-stopped
//! apps are skipped: the downgrade-retention pass owns them until day 30,
//! and the downgrade itself restarts the inactivity clock (spec Q3). Pro
//! owners are exempt while subscribed; re-upgrading clears the stamps.

use sqlx::SqlitePool;

use crate::auth::EmailSender;
use crate::billing::downgrade::RetentionDeps;

/// Throttle: at most one activity write per hour per account/app, so hot
/// paths don't write on every request.
pub const ACTIVITY_TOUCH_MS: i64 = 3_600_000;

/// One day in milliseconds: every timestamp in the schema is unix millis.
pub const DAY_MS: i64 = 86_400_000;

/// `app.inactivity_notices` bits: which deadline reminders already went out.
/// A set bit means "sent", so each reminder fires exactly once even across
/// restarts (a crash between the send and the stamp only risks one dup).
pub const NOTICE_30D: i64 = 1;
pub const NOTICE_7D: i64 = 1 << 1;
pub const NOTICE_1D: i64 = 1 << 2;

/// The sweep windows, from `OUTER_INACTIVE_DAYS` /
/// `OUTER_INACTIVE_GRACE_DAYS` (defaults 90, 7). Passed in (never read here)
/// so tests inject deterministic clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Windows {
    /// Inactivity before the app stops: the reminder marks count back from this.
    pub inactive_ms: i64,
    /// Grace after expiry: qualifying activity still restores the app.
    pub grace_ms: i64,
}

impl Windows {
    /// Days from config (or the spec defaults) to sweep math.
    pub fn new(inactive_days: u64, grace_days: u64) -> Self {
        Self {
            inactive_ms: inactive_days as i64 * DAY_MS,
            grace_ms: grace_days as i64 * DAY_MS,
        }
    }
}
impl Default for Windows {
    /// The spec defaults: 90 days to expiry, 7 days of grace.
    fn default() -> Self {
        Self::new(90, 7)
    }
}

/// What the sweep needs beyond the pool and the clock: the mail handles plus
/// every handle `delete_app` takes, owned. Same lock discipline as the MCP
/// `delete_app` path (deploy + rollback + wake across the whole deletion).
pub struct SweepDeps {
    pub sender: EmailSender,
    /// Platform issuer (`https://outer.now`): console links in the mails.
    pub issuer: String,
    /// HMAC key for the one-click unsubscribe tokens.
    pub unsub_key: Vec<u8>,
    /// The `delete_app` handles, shared with the downgrade-retention pass.
    pub retention: RetentionDeps,
    /// The sweep windows, from `OUTER_INACTIVE_DAYS` /
    /// `OUTER_INACTIVE_GRACE_DAYS`.
    pub windows: Windows,
}

/// One sweep's outcome, for the info log and the tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub reminded: usize,
    pub expired: usize,
    pub restored: usize,
    pub purged: usize,
}

/// Stamp owner activity, throttled to one write per hour per account. Never
/// raises: a failed touch only warns, so recording never breaks a request.
pub async fn touch_account(pool: &SqlitePool, account_id: &str, now_ms: i64) {
    if let Err(err) = sqlx::query(
        "UPDATE user SET lastActiveAt = ?1, updatedAt = ?1 \
         WHERE id = ?2 AND lastActiveAt < ?3",
    )
    .bind(now_ms)
    .bind(account_id)
    .bind(now_ms - ACTIVITY_TOUCH_MS)
    .execute(pool)
    .await
    {
        tracing::warn!(account = %account_id, error = %format!("{err:#}"), "activity touch failed");
    }
}

/// Stamp app activity (a collaborator's tool call on the app, or the owner's
/// own call), throttled to one write per hour per app. Never raises.
pub async fn touch_app(pool: &SqlitePool, app_id: &str, now_ms: i64) {
    if let Err(err) =
        sqlx::query("UPDATE app SET last_activity_at = ?1 WHERE id = ?2 AND last_activity_at < ?3")
            .bind(now_ms)
            .bind(app_id)
            .bind(now_ms - ACTIVITY_TOUCH_MS)
            .execute(pool)
            .await
    {
        tracing::warn!(app = %app_id, error = %format!("{err:#}"), "app activity touch failed");
    }
}

/// The owner's plan and activity clock for the `deletes_at` surfaces.
/// Unknown owners read as idle Free (the foreign key makes that
/// unreachable); surfaces must never fail on it.
pub async fn owner_clock(pool: &SqlitePool, owner_id: &str) -> (String, i64) {
    sqlx::query_as::<_, (String, i64)>("SELECT plan, lastActiveAt FROM user WHERE id = ?1")
        .bind(owner_id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None)
        .unwrap_or_else(|| ("free".to_string(), 0))
}

/// The caller's soonest inactivity deadline across owned apps, for `whoami`.
/// `None` on Pro (exempt while subscribed) or with no owned apps. Never
/// raises: surfaces must never fail on it.
pub async fn account_deletes_at(
    pool: &SqlitePool,
    account_id: &str,
    owner_plan: &str,
    owner_last_ms: i64,
    windows: Windows,
) -> Option<i64> {
    if owner_plan == "pro" {
        return None;
    }
    let rows: Vec<(i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT last_activity_at, expired_at, downgraded_stopped_at FROM app WHERE owner_id = ?1",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .filter_map(|(app_last, expired, stopped)| {
            deletes_at_ms(
                app_last,
                owner_last_ms,
                owner_plan,
                expired,
                stopped,
                windows,
            )
        })
        .min()
}

/// Record one authenticated MCP call: always the owner's account stamp, plus
/// the app stamp when `app_slug` names an app the caller has a role on.
/// Unknown and foreign slugs touch nothing (a `not_found` probe leaves no
/// trace). Never raises and never alters the call's outcome.
pub async fn record_call(
    pool: &SqlitePool,
    account_id: &str,
    account_email: &str,
    app_slug: Option<&str>,
    now_ms: i64,
) {
    touch_account(pool, account_id, now_ms).await;
    let Some(slug) = app_slug else { return };
    let app: Option<(String, String)> =
        sqlx::query_as("SELECT id, owner_id FROM app WHERE slug = ?1")
            .bind(slug)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
    let Some((app_id, owner_id)) = app else {
        return;
    };
    match crate::collaborators::role(pool, &app_id, &owner_id, account_id, account_email).await {
        Ok(Some(_)) => touch_app(pool, &app_id, now_ms).await,
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(app = %app_id, error = %format!("{err:#}"), "activity role lookup failed")
        }
    }
}

/// The `deletes_at` a surface shows for one app (`whoami`, `list_apps`,
/// `get_app`, the console): when the app goes away if nobody acts. `None`
/// while it doesn't apply (a Pro owner is exempt while subscribed). The
/// earliest of the two retentions wins: a downgrade-stopped extra app purges
/// at day 30 through the downgrade-retention pass even though its inactivity
/// clock runs longer.
pub fn deletes_at_ms(
    app_last_ms: i64,
    owner_last_ms: i64,
    owner_plan: &str,
    expired_at: Option<i64>,
    downgraded_stopped_at_secs: Option<i64>,
    windows: Windows,
) -> Option<i64> {
    if owner_plan == "pro" {
        return None;
    }
    let mut out = if let Some(stamped) = expired_at {
        stamped.saturating_add(windows.grace_ms)
    } else {
        app_last_ms
            .max(owner_last_ms)
            .saturating_add(windows.inactive_ms)
    };
    if let Some(stopped) = downgraded_stopped_at_secs {
        let purge = stopped
            .saturating_mul(1000)
            .saturating_add(crate::billing::downgrade::DOWNGRADE_PURGE_AFTER_SECS * 1000);
        out = out.min(purge);
    }
    Some(out)
}

/// One reminder mark: how far before the deadline it fires, its bit, and its
/// subject/body label. Ordered most-urgent first, so a catch-up pass sends
/// the most urgent due reminder first (each still only once).
const REMINDERS: [(i64, i64, &str); 3] = [
    (1, NOTICE_1D, "1 day"),
    (7, NOTICE_7D, "7 days"),
    (30, NOTICE_30D, "30 days"),
];

/// One sweep pass over inactive apps: reminders, expiry, restores and purges.
/// Idempotent and crash-safe: reminders are bit-gated (a crash between the
/// send and the stamp only risks one duplicate), expiry stamps are
/// conditional, and a failed delete keeps the row for the next tick.
/// Returns what moved. Never raises per-app errors past the warn log: one
/// app's failure continues with the next.
pub async fn run_sweep(pool: &SqlitePool, deps: &SweepDeps, now_ms: i64) -> anyhow::Result<Counts> {
    let windows = deps.windows;
    #[derive(sqlx::FromRow)]
    struct Row {
        id: String,
        slug: String,
        owner_id: String,
        email: String,
        plan: String,
        banned: i64,
        last_activity_at: i64,
        inactivity_notices: i64,
        expired_at: Option<i64>,
        downgraded_stopped_at: Option<i64>,
        #[sqlx(rename = "lastActiveAt")]
        owner_last_active: i64,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT a.id AS id, a.slug AS slug, a.owner_id AS owner_id, u.email AS email, \
                u.plan AS plan, u.banned AS banned, a.last_activity_at AS last_activity_at, \
                a.inactivity_notices AS inactivity_notices, a.expired_at AS expired_at, \
                a.downgraded_stopped_at AS downgraded_stopped_at, \
                u.lastActiveAt AS lastActiveAt \
         FROM app AS a JOIN user AS u ON u.id = a.owner_id",
    )
    .fetch_all(pool)
    .await?;
    let mut counts = Counts::default();
    let alert_ctx = crate::alerts::Ctx {
        pool,
        sender: &deps.sender,
        issuer: &deps.issuer,
        key: &deps.unsub_key,
    };
    for row in &rows {
        if row.plan == "pro" {
            // Exempt while subscribed; `set_pro` clears any stamps left
            // from before, so there is nothing to heal here.
            continue;
        }
        if row.downgraded_stopped_at.is_some() {
            // Owned by the downgrade-retention pass: it warns at day 23 and
            // purges at day 30, and activity doesn't cancel it.
            continue;
        }
        let effective = row.last_activity_at.max(row.owner_last_active);
        if let Some(stamped) = row.expired_at {
            if effective > stamped && effective <= stamped.saturating_add(windows.grace_ms) {
                // Qualifying activity during grace: clear the stamp and the
                // notice bits, so the next deadline reminds again. The fleet
                // stays stopped until the next request wakes it.
                let restored = sqlx::query(
                    "UPDATE app SET expired_at = NULL, inactivity_notices = 0 \
                     WHERE id = ?1 AND expired_at IS NOT NULL",
                )
                .bind(&row.id)
                .execute(pool)
                .await?;
                if restored.rows_affected() > 0 {
                    counts.restored += 1;
                }
            } else if now_ms >= stamped.saturating_add(windows.grace_ms) {
                match crate::billing::downgrade::delete_retained(
                    pool,
                    &deps.retention,
                    &row.id,
                    &row.slug,
                )
                .await
                {
                    Ok(true) => counts.purged += 1,
                    Ok(false) => {}
                    Err(err) => tracing::warn!(
                        slug = %row.slug,
                        error = %format!("{err:#}"),
                        "inactivity purge failed; the next tick retries"
                    ),
                }
            }
            continue;
        }
        let deadline = effective.saturating_add(windows.inactive_ms);
        if now_ms >= deadline {
            // Past the deadline: stamp the expiry at the deadline itself
            // (not at now, so a late sweep keeps the purge date stable),
            // stop the fleet, and purge the host's edge cache. Past grace
            // too, purge in the same pass.
            let stamped =
                sqlx::query("UPDATE app SET expired_at = ?1 WHERE id = ?2 AND expired_at IS NULL")
                    .bind(deadline)
                    .bind(&row.id)
                    .execute(pool)
                    .await?;
            if stamped.rows_affected() == 0 {
                continue;
            }
            deps.retention.fleets.stop_app(&row.slug).await;
            crate::cloudflare::purge_host_cache(
                deps.retention.delete_cfg.cache_purge.as_ref(),
                &format!("{}.{}", row.slug, deps.retention.delete_cfg.apps_domain),
            )
            .await;
            if now_ms >= deadline.saturating_add(windows.grace_ms) {
                match crate::billing::downgrade::delete_retained(
                    pool,
                    &deps.retention,
                    &row.id,
                    &row.slug,
                )
                .await
                {
                    Ok(true) => counts.purged += 1,
                    Ok(false) => {}
                    Err(err) => tracing::warn!(
                        slug = %row.slug,
                        error = %format!("{err:#}"),
                        "inactivity purge failed; the next tick retries"
                    ),
                }
            } else {
                counts.expired += 1;
            }
            continue;
        }
        if row.banned != 0 {
            // Banned accounts get no reminder mail; the expiry and purge
            // above still clear their data on schedule.
            continue;
        }
        // Before the deadline: send the due reminders, most urgent first.
        // Each bit gates its reminder, so a pass sends each at most once.
        let age = now_ms.saturating_sub(effective);
        for (days_before, bit, label) in REMINDERS {
            if age < windows.inactive_ms - days_before * DAY_MS {
                continue;
            }
            if row.inactivity_notices & bit != 0 {
                continue;
            }
            if crate::alerts::notify_inactivity(
                &alert_ctx,
                &crate::alerts::InactiveNotice {
                    app_id: &row.id,
                    slug: &row.slug,
                    owner_id: &row.owner_id,
                    to: &row.email,
                    days_left: label,
                    deadline_ms: deadline,
                    touched_ms: effective,
                    now: now_ms,
                },
            )
            .await
            {
                if let Err(err) = sqlx::query(
                    "UPDATE app SET inactivity_notices = inactivity_notices | ?1 WHERE id = ?2",
                )
                .bind(bit)
                .bind(&row.id)
                .execute(pool)
                .await
                {
                    tracing::warn!(slug = %row.slug, error = %format!("{err:#}"), "inactivity notice stamp failed");
                } else {
                    counts.reminded += 1;
                }
            }
            // Only one reminder per app per pass: the next tick (or the
            // next threshold) sends the rest.
            break;
        }
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    const NOW: i64 = 1_700_000_000_000;
    const T0: i64 = NOW - 100 * DAY_MS;

    async fn pool() -> SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("inactivity.sqlite"))
            .await
            .unwrap();
        std::mem::forget(dir);
        pool
    }

    async fn seed_user(pool: &SqlitePool, id: &str, email: &str, plan: &str) {
        sqlx::query(
            "INSERT INTO user (id, email, plan, lastActiveAt, createdAt, updatedAt) \
             VALUES (?1, ?2, ?3, 0, 0, 0)",
        )
        .bind(id)
        .bind(email)
        .bind(plan)
        .execute(pool)
        .await
        .unwrap();
    }

    static NEXT_PORT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(20000);

    async fn seed_app(pool: &SqlitePool, id: &str, slug: &str, owner: &str, last_activity: i64) {
        // Ports, like `create_app` assigns: `delete_app` needs them. Drawn
        // from a counter so seeds never collide with each other.
        let port = NEXT_PORT.fetch_add(2, std::sync::atomic::Ordering::SeqCst);
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
             last_activity_at, created_at) \
             VALUES (?1, ?2, ?3, abs(random()) % 100000 + 300000, ?4, ?4 + 1, ?5, ?5)",
        )
        .bind(id)
        .bind(slug)
        .bind(owner)
        .bind(port)
        .bind(last_activity)
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

    fn sweep_deps(dir: &std::path::Path, sender: EmailSender) -> SweepDeps {
        let data_dir = dir.to_path_buf();
        SweepDeps {
            sender,
            issuer: "https://outer.now".to_string(),
            unsub_key: b"test-unsub-key".to_vec(),
            retention: RetentionDeps {
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
                            bucket: "outer-inactivity-test".to_string(),
                            credentials: crate::store::credentials::CredentialSource::Static {
                                access_key_id: "outeraccess".to_string(),
                                secret_access_key: "outersecretoutersecretoutersecret12"
                                    .to_string(),
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
                delete_cfg: crate::apps::delete::DeleteConfig {
                    data_dir,
                    cgroups: None,
                    apps_domain: "outer.now".to_string(),
                    cache_purge: None,
                },
            },
            windows: Windows::new(90, 7),
        }
    }

    async fn notices_of(pool: &SqlitePool, id: &str) -> i64 {
        sqlx::query_scalar("SELECT inactivity_notices FROM app WHERE id = ?1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn expired_of(pool: &SqlitePool, id: &str) -> Option<i64> {
        sqlx::query_scalar("SELECT expired_at FROM app WHERE id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap()
            .flatten()
    }

    fn mail_to(mailed: &Arc<Mutex<HashMap<String, String>>>, email: &str) -> Option<String> {
        mailed.lock().unwrap().get(email).cloned()
    }

    #[tokio::test]
    async fn reminders_fire_once_each_then_expiry_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool().await;
        seed_user(&pool, "a1", "ana@example.com", "free").await;
        seed_app(&pool, "app1", "groceries", "a1", T0).await;
        let (sender, mailed) = capture();
        let deps = sweep_deps(dir.path(), sender);

        // Day 60: the 30-day reminder goes out once.
        let at = T0 + 60 * DAY_MS;
        assert_eq!(
            run_sweep(&pool, &deps, at).await.unwrap(),
            Counts {
                reminded: 1,
                ..Counts::default()
            }
        );
        assert_eq!(notices_of(&pool, "app1").await, NOTICE_30D);
        let mail = mail_to(&mailed, "ana@example.com").unwrap();
        assert!(
            mail.contains("groceries will be deleted in 30 days"),
            "{mail}"
        );
        assert!(mail.contains("hasn't been touched since"), "{mail}");
        assert!(mail.contains("will be deleted on"), "{mail}");
        assert!(mail.contains("any request counts"), "{mail}");
        assert!(
            mail.contains("/console/alerts/unsubscribe?token="),
            "{mail}"
        );
        assert!(!mail.contains("visits this month"), "{mail}");
        // Same tick: nothing resends.
        assert_eq!(
            run_sweep(&pool, &deps, at).await.unwrap(),
            Counts::default()
        );

        // Day 83: the 7-day reminder; day 89: the 1-day reminder with the
        // visit count from the cheap `visitor_day` source.
        assert_eq!(
            run_sweep(&pool, &deps, T0 + 83 * DAY_MS).await.unwrap(),
            Counts {
                reminded: 1,
                ..Counts::default()
            }
        );
        assert!(mail_to(&mailed, "ana@example.com")
            .unwrap()
            .contains("will be deleted in 7 days"));
        sqlx::query("INSERT INTO visitor_day (app_id, day, visitors) VALUES ('app1', ?1, 42)")
            .bind((T0 + 89 * DAY_MS).div_euclid(DAY_MS))
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            run_sweep(&pool, &deps, T0 + 89 * DAY_MS).await.unwrap(),
            Counts {
                reminded: 1,
                ..Counts::default()
            }
        );
        let mail = mail_to(&mailed, "ana@example.com").unwrap();
        assert!(mail.contains("will be deleted in 1 day"), "{mail}");
        assert!(mail.contains("It had 42 visits this month."), "{mail}");
        assert_eq!(
            notices_of(&pool, "app1").await,
            NOTICE_30D | NOTICE_7D | NOTICE_1D
        );

        // Day 90: the app expires (stamped at the deadline, kept serving
        // nothing until restore or purge). Day 91: quiet.
        assert_eq!(
            run_sweep(&pool, &deps, T0 + 90 * DAY_MS).await.unwrap(),
            Counts {
                expired: 1,
                ..Counts::default()
            }
        );
        assert_eq!(expired_of(&pool, "app1").await, Some(T0 + 90 * DAY_MS));
        assert_eq!(
            run_sweep(&pool, &deps, T0 + 91 * DAY_MS).await.unwrap(),
            Counts::default()
        );

        // Day 92: a sign-in (owner activity past the stamp but in grace)
        // restores the app and clears the notice bits for the next cycle.
        sqlx::query("UPDATE user SET lastActiveAt = ?1 WHERE id = 'a1'")
            .bind(T0 + 92 * DAY_MS)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            run_sweep(&pool, &deps, T0 + 92 * DAY_MS).await.unwrap(),
            Counts {
                restored: 1,
                ..Counts::default()
            }
        );
        assert_eq!(expired_of(&pool, "app1").await, None);
        assert_eq!(notices_of(&pool, "app1").await, 0);
    }
    #[tokio::test]
    async fn purge_at_grace_end_reuses_the_delete_path() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool().await;
        seed_user(&pool, "a1", "ana@example.com", "free").await;
        seed_app(&pool, "app1", "groceries", "a1", T0).await;
        let (sender, _) = capture();
        let deps = sweep_deps(dir.path(), sender);
        // Leftover state across every prefix the delete path owns.
        for key in [
            "fleets/groceries/deploy/current.json",
            "git/groceries/tip.bundle",
            "outer/exports/groceries/abc1234-x.zip",
            "outer/backups/groceries/1-DB.sql",
        ] {
            deps.retention.store.put(key, b"x".to_vec()).await.unwrap();
        }
        assert_eq!(
            run_sweep(&pool, &deps, T0 + 90 * DAY_MS).await.unwrap(),
            Counts {
                expired: 1,
                ..Counts::default()
            }
        );
        assert_eq!(
            run_sweep(&pool, &deps, T0 + 97 * DAY_MS).await.unwrap(),
            Counts {
                purged: 1,
                ..Counts::default()
            }
        );
        let gone: Option<String> = sqlx::query_scalar("SELECT id FROM app WHERE id = 'app1'")
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert_eq!(gone, None);
        for prefix in [
            "fleets/groceries/",
            "git/groceries/",
            "outer/exports/groceries/",
            "outer/backups/groceries/",
        ] {
            assert!(
                deps.retention.store.list(prefix).await.unwrap().is_empty(),
                "{prefix} wiped"
            );
        }
    }

    #[tokio::test]
    async fn far_past_grace_purges_on_the_catch_up_pass() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool().await;
        seed_user(&pool, "a1", "ana@example.com", "free").await;
        seed_app(&pool, "app1", "groceries", "a1", T0).await;
        let (sender, _) = capture();
        let deps = sweep_deps(dir.path(), sender);
        // A sweep that missed the whole window expires backdated and purges
        // in the same pass: the purge date never drifts past the schedule.
        assert_eq!(
            run_sweep(&pool, &deps, T0 + 100 * DAY_MS).await.unwrap(),
            Counts {
                purged: 1,
                ..Counts::default()
            }
        );
        let gone: Option<String> = sqlx::query_scalar("SELECT id FROM app WHERE id = 'app1'")
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert_eq!(gone, None);
    }
    #[tokio::test]
    async fn pro_owners_are_exempt_while_subscribed() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool().await;
        seed_user(&pool, "a1", "ana@example.com", "pro").await;
        seed_app(&pool, "app1", "groceries", "a1", T0 - 200 * DAY_MS).await;
        let (sender, mailed) = capture();
        let deps = sweep_deps(dir.path(), sender);
        assert_eq!(
            run_sweep(&pool, &deps, NOW).await.unwrap(),
            Counts::default()
        );
        assert!(mailed.lock().unwrap().is_empty());
        assert_eq!(expired_of(&pool, "app1").await, None);
    }

    #[tokio::test]
    async fn downgrade_stopped_apps_belong_to_downgrade_retention() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool().await;
        seed_user(&pool, "a1", "ana@example.com", "free").await;
        seed_app(&pool, "app1", "groceries", "a1", T0).await;
        // A downgrade-stopped extra app, long past even the inactivity
        // deadline: the downgrade-retention pass owns it (warns day 23,
        // purges day 30), so this sweep stays quiet.
        sqlx::query(
            "UPDATE app SET desired_state = 'stopped', downgraded_stopped_at = ?1 \
             WHERE id = 'app1'",
        )
        .bind(T0 / 1000)
        .execute(&pool)
        .await
        .unwrap();
        let (sender, mailed) = capture();
        let deps = sweep_deps(dir.path(), sender);
        assert_eq!(
            run_sweep(&pool, &deps, T0 + 100 * DAY_MS).await.unwrap(),
            Counts::default()
        );
        assert!(mailed.lock().unwrap().is_empty());
        assert_eq!(expired_of(&pool, "app1").await, None);
        let kept: Option<String> = sqlx::query_scalar("SELECT id FROM app WHERE id = 'app1'")
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert_eq!(kept.as_deref(), Some("app1"));
    }

    async fn last_stamps(pool: &SqlitePool) -> (i64, i64, i64) {
        let owner: i64 = sqlx::query_scalar("SELECT lastActiveAt FROM user WHERE id = 'a1'")
            .fetch_one(pool)
            .await
            .unwrap();
        let app: i64 = sqlx::query_scalar("SELECT last_activity_at FROM app WHERE id = 'app1'")
            .fetch_one(pool)
            .await
            .unwrap();
        let collab: i64 = sqlx::query_scalar("SELECT lastActiveAt FROM user WHERE id = 'c1'")
            .fetch_one(pool)
            .await
            .unwrap();
        (owner, app, collab)
    }

    #[tokio::test]
    async fn owner_calls_touch_account_and_app_collaborator_touches_app_only() {
        let pool = pool().await;
        // Collaborators only hold a role while the owner is Pro: the app is
        // Pro-owned, and the assertions below pin whose clock moves.
        seed_user(&pool, "a1", "ana@example.com", "pro").await;
        seed_user(&pool, "c1", "collab@example.com", "free").await;
        seed_app(&pool, "app1", "groceries", "a1", 0).await;
        sqlx::query(
            "INSERT INTO app_collaborator (app_id, email, userId, added_at) \
             VALUES ('app1', 'collab@example.com', 'c1', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // A collaborator's tool call on the app counts for that app and for
        // the collaborator's own clock — but never for the owner's clock, so
        // it can't keep the owner's other apps alive.
        record_call(&pool, "c1", "collab@example.com", Some("groceries"), NOW).await;
        assert_eq!(last_stamps(&pool).await, (0, NOW, NOW));
        // Throttled: a second call inside the hour writes nothing.
        record_call(
            &pool,
            "c1",
            "collab@example.com",
            Some("groceries"),
            NOW + 30 * 60 * 1000,
        )
        .await;
        assert_eq!(last_stamps(&pool).await, (0, NOW, NOW));
        // An hour later the write goes through again.
        record_call(
            &pool,
            "c1",
            "collab@example.com",
            Some("groceries"),
            NOW + 2 * 3_600_000,
        )
        .await;
        assert_eq!(
            last_stamps(&pool).await,
            (0, NOW + 2 * 3_600_000, NOW + 2 * 3_600_000)
        );
        // The owner's call stamps both clocks; a foreign slug stamps
        // nothing (a `not_found` probe leaves no trace).
        record_call(&pool, "a1", "ana@example.com", Some("groceries"), NOW).await;
        let (owner, app, _) = last_stamps(&pool).await;
        assert_eq!((owner, app), (NOW, NOW + 2 * 3_600_000));
        record_call(&pool, "a1", "ana@example.com", Some("nope"), NOW).await;
        record_call(&pool, "a1", "ana@example.com", None, NOW).await;
        let (owner_after, app_after, _) = last_stamps(&pool).await;
        assert_eq!((owner_after, app_after), (NOW, NOW + 2 * 3_600_000));
    }

    #[test]
    fn deletes_at_math() {
        let w = Windows::new(90, 7);
        assert_eq!(
            deletes_at_ms(100, 50, "free", None, None, w),
            Some(100 + 90 * DAY_MS)
        );
        assert_eq!(
            deletes_at_ms(50, 100, "free", None, None, w),
            Some(100 + 90 * DAY_MS)
        );
        assert_eq!(deletes_at_ms(0, 0, "pro", None, None, w), None);
        assert_eq!(
            deletes_at_ms(0, 0, "free", Some(1_000), None, w),
            Some(1_000 + 7 * DAY_MS)
        );
        // A downgrade-stopped extra app purges at day 30 through the
        // downgrade-retention pass: the earlier date wins.
        assert_eq!(
            deletes_at_ms(0, 0, "free", None, Some(1_000), w),
            Some(1_000_000 + 30 * DAY_MS)
        );
        assert_eq!(
            deletes_at_ms(0, 0, "free", Some(10_000_000_000), Some(1_000), w),
            Some(1_000_000 + 30 * DAY_MS)
        );
    }

    #[tokio::test]
    async fn account_deletes_at_is_the_soonest_free_deadline() {
        let pool = pool().await;
        seed_user(&pool, "a1", "ana@example.com", "free").await;
        seed_user(&pool, "p1", "pro@example.com", "pro").await;
        seed_app(&pool, "app1", "groceries", "a1", 1_000).await;
        seed_app(&pool, "app2", "pantry", "a1", 2_000).await;
        sqlx::query("UPDATE app SET expired_at = ?1 WHERE id = 'app2'")
            .bind(3_000)
            .execute(&pool)
            .await
            .unwrap();
        let w = Windows::new(90, 7);
        // The expired app purges first (its grace date beats the other's
        // deadline).
        assert_eq!(
            account_deletes_at(&pool, "a1", "free", 500, w).await,
            Some(3_000 + 7 * DAY_MS)
        );
        // Pro: exempt with or without apps. No apps: nothing to delete.
        assert_eq!(account_deletes_at(&pool, "p1", "pro", 0, w).await, None);
        assert_eq!(
            account_deletes_at(&pool, "nobody", "free", 0, w).await,
            None
        );
    }
}
