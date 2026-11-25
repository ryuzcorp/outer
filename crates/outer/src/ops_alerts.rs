//! Operator alerts (L-24): mail to `OUTER_ADMIN_EMAILS` when the platform
//! itself needs attention. Owner-facing mail lives in [`crate::alerts`]
//! (per-account prefs, one-click unsubscribe); this module is the other
//! side: no prefs, no unsubscribe link, one shared cooldown per alert kind
//! (at most one mail per kind per hour by default), and bodies that never
//! carry secrets — counts, slugs and truncated error summaries only, never
//! tokens, keys or recipient addresses.
//!
//! The eight triggers:
//!
//! | kind | source |
//! | --- | --- |
//! | `ops_snapshot` | the state-sync tick's last error (polled, not hooked) |
//! | `ops_disk` | free space on `OUTER_DATA` below a ratio or a byte floor |
//! | `ops_email` | Cloudflare Email Service failures, 429 included |
//! | `ops_jev` | the moderation classifier failing for over 15 minutes |
//! | `ops_suspended` | an app in `suspended` moderation state |
//! | `ops_queue` | non-terminal deploy rows over a depth threshold |
//! | `ops_email_budget_warn` | this UTC month's sends at 80 % of the 3,000 cap |
//! | `ops_email_budget_crit` | this UTC month's sends at 95 % of the 3,000 cap |
//!
//! Jev and email outages are tracked in the `ops_heartbeat` table: the
//! classify call sites and the email sender record ok/err, and the sweep
//! below decides from `first_err_ms` + `consecutive_failures`. Every
//! decision function takes `now` explicitly; tests run them on a fake
//! clock, following the [`crate::alerts`] pattern.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::SqlitePool;

use crate::auth::EmailSender;
use crate::store::snapshot::{SnapshotStatus, StateSync};

/// Dedupe ledger kinds. The shared `alert_send` table carries them, with an
/// `ops:` subject namespace so operator rows never collide with owner rows.
pub const KIND_SNAPSHOT: &str = "ops_snapshot";
pub const KIND_DISK: &str = "ops_disk";
pub const KIND_EMAIL: &str = "ops_email";
pub const KIND_JEV: &str = "ops_jev";
pub const KIND_SUSPENDED: &str = "ops_suspended";
pub const KIND_QUEUE: &str = "ops_queue";
/// The monthly email budget crossed 80 % (`ops:email-budget-80` subject).
pub const KIND_EMAIL_BUDGET_WARN: &str = "ops_email_budget_warn";
/// The monthly email budget crossed 95 % (`ops:email-budget-95` subject).
pub const KIND_EMAIL_BUDGET_CRIT: &str = "ops_email_budget_crit";

/// Thresholds for the operator sweep. Parsed once from the environment in
/// [`crate::config`] (`OUTER_OPS_*`); tests build it literally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpsAlertConfig {
    /// Alert when free space on `OUTER_DATA` drops below this percent.
    pub disk_free_pct: u64,
    /// Alert when free space on `OUTER_DATA` drops below these bytes.
    pub disk_free_min_bytes: u64,
    /// Alert when non-terminal deploy rows reach this count.
    pub deploy_queue_depth: i64,
    /// Alert when Jev has failed continuously for this long.
    pub jev_outage: Duration,
    /// At most one mail per alert kind per this window.
    pub cooldown: Duration,
}

impl Default for OpsAlertConfig {
    fn default() -> Self {
        Self {
            disk_free_pct: 10,
            disk_free_min_bytes: 5 * 1024 * 1024 * 1024,
            deploy_queue_depth: 50,
            jev_outage: Duration::from_secs(900),
            cooldown: Duration::from_secs(3600),
        }
    }
}

/// Everything one sweep tick needs: explicit dependencies, no globals.
pub struct OpsCtx<'a> {
    pub pool: &'a SqlitePool,
    pub sync: &'a StateSync,
    pub sender: &'a EmailSender,
    pub admins: &'a [String],
    pub cfg: &'a OpsAlertConfig,
    pub data_dir: &'a Path,
}

/// The email sender's failure hook: [`EmailSender::post`] records every
/// Cloudflare send into `ops_heartbeat(kind = 'ops_email')` through this.
/// Success clears the streak; any transport, 429 or provider refusal starts
/// or extends it. Per-recipient bounces are not provider outages and stay
/// out. Best effort: recording never fails the send.
#[derive(Clone, Debug)]
pub struct OpsMailHook {
    pub pool: SqlitePool,
}

/// One heartbeat row: the last success plus the current failure streak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    pub last_ok_ms: Option<i64>,
    pub first_err_ms: Option<i64>,
    pub last_err_ms: Option<i64>,
    pub consecutive_failures: i64,
    pub detail: Option<String>,
}

/// The failure streak is live: another failure since the last success.
#[allow(clippy::type_complexity)]
pub async fn heartbeat(pool: &SqlitePool, kind: &str) -> Option<Heartbeat> {
    let row: Option<(Option<i64>, Option<i64>, Option<i64>, i64, Option<String>)> = sqlx::query_as(
        "SELECT last_ok_ms, first_err_ms, last_err_ms, consecutive_failures, detail \
         FROM ops_heartbeat WHERE kind = ?1",
    )
    .bind(kind)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    row.map(
        |(last_ok_ms, first_err_ms, last_err_ms, consecutive_failures, detail)| Heartbeat {
            last_ok_ms,
            first_err_ms,
            last_err_ms,
            consecutive_failures,
            detail,
        },
    )
}

/// Record a success: clears the failure streak, keeps no detail.
pub async fn record_heartbeat_ok(pool: &SqlitePool, kind: &str, now: i64) {
    if let Err(err) = sqlx::query(
        "INSERT INTO ops_heartbeat (kind, last_ok_ms, first_err_ms, last_err_ms, consecutive_failures, detail) \
         VALUES (?1, ?2, NULL, NULL, 0, NULL) \
         ON CONFLICT(kind) DO UPDATE SET last_ok_ms = ?2, first_err_ms = NULL, last_err_ms = NULL, \
         consecutive_failures = 0, detail = NULL",
    )
    .bind(kind)
    .bind(now)
    .execute(pool)
    .await
    {
        tracing::warn!(kind, error = %format!("{err:#}"), "ops heartbeat ok write failed");
    }
}

/// Record a failure: extends the streak, stamping `first_err_ms` on its
/// first failure. The detail is truncated (bodies carry it) and must never
/// hold a secret; callers pass already-scrubbed provider errors.
pub async fn record_heartbeat_err(pool: &SqlitePool, kind: &str, now: i64, detail: &str) {
    let detail: String = detail.chars().take(300).collect();
    let current = heartbeat(pool, kind).await;
    let first_err_ms = match current {
        Some(hb) if hb.consecutive_failures > 0 => hb.first_err_ms.unwrap_or(now),
        _ => now,
    };
    if let Err(err) = sqlx::query(
        "INSERT INTO ops_heartbeat (kind, last_ok_ms, first_err_ms, last_err_ms, consecutive_failures, detail) \
         VALUES (?1, NULL, ?2, ?3, 1, ?4) \
         ON CONFLICT(kind) DO UPDATE SET first_err_ms = ?2, last_err_ms = ?3, \
         consecutive_failures = consecutive_failures + 1, detail = ?4",
    )
    .bind(kind)
    .bind(first_err_ms)
    .bind(now)
    .bind(&detail)
    .execute(pool)
    .await
    {
        tracing::warn!(kind, error = %format!("{err:#}"), "ops heartbeat err write failed");
    }
}

/// Disk alert decision: free space below the percent threshold OR below the
/// byte floor. A zero-size filesystem (unstatable in tests) never alerts.
pub fn disk_low(free_bytes: u64, total_bytes: u64, cfg: &OpsAlertConfig) -> bool {
    if total_bytes == 0 {
        return false;
    }
    free_bytes.saturating_mul(100) < cfg.disk_free_pct.saturating_mul(total_bytes)
        || free_bytes < cfg.disk_free_min_bytes
}

/// Queue alert decision: non-terminal deploy rows at or over the threshold.
pub fn queue_deep(depth: i64, cfg: &OpsAlertConfig) -> bool {
    depth >= cfg.deploy_queue_depth
}

/// Jev outage decision: a live failure streak older than the outage window.
/// A cleared streak (`consecutive == 0`) or a missing stamp never alerts.
pub fn jev_outage(
    first_err_ms: Option<i64>,
    consecutive_failures: i64,
    now: i64,
    cfg: &OpsAlertConfig,
) -> bool {
    if consecutive_failures <= 0 {
        return false;
    }
    let Some(first) = first_err_ms else {
        return false;
    };
    now.saturating_sub(first) >= cfg.jev_outage.as_millis() as i64
}

/// Email outage decision: a live failure streak whose latest failure is
/// recent (within two cooldowns). Stale streaks stay silent: with no sends
/// attempted there is nothing new to report, and the next success clears
/// the row.
pub fn email_outage(hb: &Heartbeat, now: i64, cfg: &OpsAlertConfig) -> bool {
    if hb.consecutive_failures <= 0 {
        return false;
    }
    let Some(last) = hb.last_err_ms else {
        return false;
    };
    now.saturating_sub(last) <= cfg.cooldown.as_millis() as i64 * 2
}

/// Email budget decisions (L-22): pure in sent/cap, tested on literals.
/// `warn` fires in [80 %, 95 %); `crit` fires at 95 % and above, including
/// the hard cap itself. A zero cap never alerts (unconfigured, not empty).
pub fn email_budget_warn(sent: u64, cap: u64) -> bool {
    cap > 0 && sent * 100 >= cap * 80 && sent * 100 < cap * 95
}

/// Email budget decisions (L-22): pure in sent/cap, tested on literals. See
/// [`email_budget_warn`]: the crit edge at 95 % supersedes the warn.
pub fn email_budget_crit(sent: u64, cap: u64) -> bool {
    cap > 0 && sent * 100 >= cap * 95
}

/// Snapshot failure decision: the sync tick's last error, if any.
pub fn snapshot_failed(status: &SnapshotStatus) -> Option<&str> {
    status.last_error.as_deref()
}

/// Free vs total bytes on `path` via `statvfs`. `None` when the path cannot
/// be stated (the sweep skips the disk check, it never alerts on that).
pub fn disk_free(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let free = stat.f_bavail as u64 * stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * stat.f_frsize as u64;
    Some((free, total))
}

/// Non-terminal deploy rows: everything that is not success, failed or
/// blocked. A stuck worker, a wedged build slot and a retry storm all read
/// as depth here.
pub async fn deploy_queue_depth(pool: &SqlitePool) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM deploy WHERE status NOT IN ('success', 'failed', 'blocked')",
    )
    .fetch_one(pool)
    .await?)
}

/// Slugs currently in the `suspended` moderation state.
pub async fn suspended_slugs(pool: &SqlitePool) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT slug FROM app WHERE moderation_state = 'suspended' ORDER BY slug",
    )
    .fetch_all(pool)
    .await?)
}

fn human_gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
}

fn minutes_ago(now: i64, since: i64) -> i64 {
    now.saturating_sub(since) / 60_000
}

/// Send one operator mail to every admin, recording the dedupe row only
/// when at least one send landed (a dead mailer retries on the next tick
/// instead of going silent). No unsubscribe headers: admins cannot opt out
/// of platform pages. Never raises.
async fn notify_ops(ctx: &OpsCtx<'_>, kind: &str, key: &str, subject: &str, body: &str, now: i64) {
    if ctx.admins.is_empty() {
        return;
    }
    let debounced = match crate::alerts::should_send(
        ctx.pool,
        key,
        kind,
        now,
        ctx.cfg.cooldown.as_millis() as i64,
    )
    .await
    {
        Ok(debounced) => debounced,
        Err(err) => {
            tracing::warn!(kind, error = %format!("{err:#}"), "ops alert debounce lookup failed");
            return;
        }
    };
    if !debounced {
        return;
    }
    let mut sent = 0;
    for admin in ctx.admins {
        match ctx.sender.send_text(admin, subject, body).await {
            Ok(()) => sent += 1,
            Err(err) => {
                tracing::warn!(kind, admin, error = %err.to_string(), "operator alert failed");
            }
        }
    }
    if sent > 0 {
        if let Err(err) = crate::alerts::record_send(ctx.pool, key, kind, now).await {
            tracing::warn!(kind, error = %format!("{err:#}"), "ops alert ledger insert failed");
        }
    }
}

async fn check_snapshot(ctx: &OpsCtx<'_>, now: i64) {
    let status = ctx.sync.snapshot_status();
    let Some(err) = snapshot_failed(&status) else {
        return;
    };
    let detail: String = err.chars().take(300).collect();
    notify_ops(
        ctx,
        KIND_SNAPSHOT,
        "ops:snapshot",
        "Outer state snapshot failing",
        &format!(
            "The platform state snapshot is failing, so SQLite writes are not reaching the bucket: {detail}\n\n\
             Writes keep serving locally, but a host loss now loses them. Fix the bucket or the owner fence; \
             this repeats hourly until the snapshot lands again."
        ),
        now,
    )
    .await;
}

async fn check_disk(ctx: &OpsCtx<'_>, now: i64) {
    let Some((free, total)) = disk_free(ctx.data_dir) else {
        tracing::warn!("ops disk check: cannot stat OUTER_DATA");
        return;
    };
    if !disk_low(free, total, ctx.cfg) {
        return;
    }
    notify_ops(
        ctx,
        KIND_DISK,
        "ops:disk",
        "Outer disk low on OUTER_DATA",
        &format!(
            "Free space on the platform data dir is {free} of {total} total. \
             Snapshots, D1 backups and deploy worktrees all write here; free space or grow the volume.",
            free = human_gib(free),
            total = human_gib(total),
        ),
        now,
    )
    .await;
}

async fn check_email(ctx: &OpsCtx<'_>, now: i64) {
    let hb = match heartbeat(ctx.pool, KIND_EMAIL).await {
        Some(hb) => hb,
        None => return,
    };
    if !email_outage(&hb, now, ctx.cfg) {
        return;
    }
    let detail = hb.detail.as_deref().unwrap_or("no detail recorded");
    notify_ops(
        ctx,
        KIND_EMAIL,
        "ops:email",
        "Outer email sending is failing",
        &format!(
            "{n} consecutive email sends failed ({m} since the last success): {detail}\n\n\
             Sign-in codes, billing mail and alerts all go through Cloudflare Email Service; \
             a 429 here usually means the service quota is spent.",
            n = hb.consecutive_failures,
            m = hb
                .last_ok_ms
                .map(|ok| format!("{} minutes", minutes_ago(now, ok)))
                .unwrap_or_else(|| "never".to_string()),
        ),
        now,
    )
    .await;
}

async fn check_jev(ctx: &OpsCtx<'_>, now: i64) {
    let hb = match heartbeat(ctx.pool, KIND_JEV).await {
        Some(hb) => hb,
        None => return,
    };
    if !jev_outage(hb.first_err_ms, hb.consecutive_failures, now, ctx.cfg) {
        return;
    }
    let detail = hb.detail.as_deref().unwrap_or("no detail recorded");
    notify_ops(
        ctx,
        KIND_JEV,
        "ops:jev",
        "Outer moderation classifier (Jev) unavailable",
        &format!(
            "Jev classification has failed {n} times in a row for {m} minutes: {detail}\n\n\
             Deploys fail closed at the gate while Jev is down; after-live crawls fail open. \
             Check the Cloudflare AI Gateway account and balance.",
            n = hb.consecutive_failures,
            m = hb
                .first_err_ms
                .map(|first| minutes_ago(now, first))
                .unwrap_or(0),
        ),
        now,
    )
    .await;
}

async fn check_suspended(ctx: &OpsCtx<'_>, now: i64) {
    let slugs = match suspended_slugs(ctx.pool).await {
        Ok(slugs) => slugs,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "ops suspended list failed");
            return;
        }
    };
    for slug in &slugs {
        notify_ops(
            ctx,
            KIND_SUSPENDED,
            &format!("ops:suspended:{slug}"),
            &format!("Outer app suspended: {slug}"),
            &format!(
                "{slug} is suspended and visitors see the suspended page. \
                 Review it in the admin moderation queue; this repeats hourly while it stays suspended."
            ),
            now,
        )
        .await;
    }
}

async fn check_queue(ctx: &OpsCtx<'_>, now: i64) {
    let depth = match deploy_queue_depth(ctx.pool).await {
        Ok(depth) => depth,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "ops queue depth lookup failed");
            return;
        }
    };
    if !queue_deep(depth, ctx.cfg) {
        return;
    }
    notify_ops(
        ctx,
        KIND_QUEUE,
        "ops:queue",
        "Outer deploy queue deep",
        &format!(
            "{depth} deploys are queued or in flight (threshold {t}). \
             A stuck worker or a wedged build slot reads as depth here; check the deploy pipeline.",
            t = ctx.cfg.deploy_queue_depth,
        ),
        now,
    )
    .await;
}

/// The monthly email budget check (L-22): warn at 80 %, crit at 95 %. Both
/// read this UTC month's `email_month` count; neither blocks any send —
/// sign-in codes flow until the [`crate::auth::email::EMAIL_MONTHLY_CAP`]
/// hard cap, where the sender itself refuses with a retry-next-month
/// message. `now` is injected (tests run it on a fake clock).
async fn check_email_budget(ctx: &OpsCtx<'_>, now: i64) {
    use crate::auth::email::{month_key, sent_in_month, EMAIL_MONTHLY_CAP};
    let month = month_key(now);
    let sent = sent_in_month(ctx.pool, &month).await;
    let cap = EMAIL_MONTHLY_CAP;
    if email_budget_crit(sent, cap) {
        notify_ops(
            ctx,
            KIND_EMAIL_BUDGET_CRIT,
            "ops:email-budget-95",
            "Outer email budget critical (95 %)",
            &format!(
                "{sent}/{cap} emails sent in {month} (Cloudflare plan cap). \
                 Sign-in codes keep flowing until the {cap} hard cap; at the cap every send \
                 refuses with a retry-next-month message. Raise the Cloudflare plan or wait \
                 for the month to turn; this repeats hourly while over 95 %."
            ),
            now,
        )
        .await;
        return;
    }
    if email_budget_warn(sent, cap) {
        notify_ops(
            ctx,
            KIND_EMAIL_BUDGET_WARN,
            "ops:email-budget-80",
            "Outer email budget at 80 %",
            &format!(
                "{sent}/{cap} emails sent in {month} (Cloudflare plan cap). \
                 Sign-in codes keep flowing until the {cap} hard cap; at the cap every send \
                 refuses with a retry-next-month message. Watch usage or raise the Cloudflare \
                 plan; this repeats hourly while over 80 %."
            ),
            now,
        )
        .await;
    }
}

/// One sweep tick: all seven checks. Never raises; a dead mailer or a failed
/// lookup only logs.
pub async fn check_all(ctx: &OpsCtx<'_>, now: i64) {
    check_snapshot(ctx, now).await;
    check_disk(ctx, now).await;
    check_email(ctx, now).await;
    check_email_budget(ctx, now).await;
    check_jev(ctx, now).await;
    check_suspended(ctx, now).await;
    check_queue(ctx, now).await;
}

/// Run [`check_all`] every `every`; each tick logs and continues.
pub fn spawn_ops_sweep(
    pool: SqlitePool,
    sync: StateSync,
    sender: EmailSender,
    admins: Vec<String>,
    cfg: OpsAlertConfig,
    data_dir: PathBuf,
    every: Duration,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        loop {
            interval.tick().await;
            let ctx = OpsCtx {
                pool: &pool,
                sync: &sync,
                sender: &sender,
                admins: &admins,
                cfg: &cfg,
                data_dir: &data_dir,
            };
            check_all(&ctx, crate::auth::now_ms()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::{test_config, test_pool};

    const NOW: i64 = 1_800_000_000_000;

    fn cfg() -> OpsAlertConfig {
        OpsAlertConfig::default()
    }

    fn ctx<'a>(
        pool: &'a SqlitePool,
        sync: &'a StateSync,
        sender: &'a EmailSender,
        admins: &'a [String],
        cfg: &'a OpsAlertConfig,
        data_dir: &'a Path,
    ) -> OpsCtx<'a> {
        OpsCtx {
            pool,
            sync,
            sender,
            admins,
            cfg,
            data_dir,
        }
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

    async fn seed_app(pool: &SqlitePool, id: &str, slug: &str, owner: &str) {
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
                          last_activity_at, created_at) \
             VALUES (?1, ?2, ?3, 10001, 18101, 18102, 0, 0)",
        )
        .bind(id)
        .bind(slug)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn ledger_count(pool: &SqlitePool, kind: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM alert_send WHERE kind = ?1")
            .bind(kind)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn disk_low_fires_on_ratio_or_floor() {
        let cfg = cfg();
        // 6 % free on a 100 GiB volume: under the 10 % ratio.
        assert!(disk_low(
            6 * 1024 * 1024 * 1024,
            100 * 1024 * 1024 * 1024,
            &cfg
        ));
        // 20 % free on a 10 GiB volume is 2 GiB: over the ratio, under the
        // 5 GiB floor.
        assert!(disk_low(
            2 * 1024 * 1024 * 1024,
            10 * 1024 * 1024 * 1024,
            &cfg
        ));
        // Plenty free on both axes: silent.
        assert!(!disk_low(
            20 * 1024 * 1024 * 1024,
            100 * 1024 * 1024 * 1024,
            &cfg
        ));
        // Exactly at the ratio with room on the floor: silent.
        assert!(!disk_low(
            10 * 1024 * 1024 * 1024,
            100 * 1024 * 1024 * 1024,
            &cfg
        ));
        // An unstatable filesystem never alerts.
        assert!(!disk_low(0, 0, &cfg));
    }

    #[test]
    fn queue_deep_fires_at_the_threshold() {
        let cfg = cfg();
        assert!(!queue_deep(cfg.deploy_queue_depth - 1, &cfg));
        assert!(queue_deep(cfg.deploy_queue_depth, &cfg));
        assert!(queue_deep(cfg.deploy_queue_depth + 40, &cfg));
    }

    #[test]
    fn jev_outage_needs_a_live_streak_past_the_window() {
        let cfg = cfg();
        let window = cfg.jev_outage.as_millis() as i64;
        assert!(!jev_outage(None, 0, NOW, &cfg));
        assert!(!jev_outage(None, 3, NOW, &cfg));
        assert!(!jev_outage(Some(NOW - window + 60_000), 3, NOW, &cfg));
        assert!(jev_outage(Some(NOW - window), 3, NOW, &cfg));
        assert!(jev_outage(Some(NOW - window - 1), 1, NOW, &cfg));
    }

    #[test]
    fn snapshot_failed_passes_the_error_through() {
        let failing = SnapshotStatus {
            owner: true,
            last_upload_secs_ago: None,
            last_error: Some("lineage refused".to_string()),
            consecutive_lineage_refusals: 3,
        };
        assert_eq!(snapshot_failed(&failing), Some("lineage refused"));
        let healthy = SnapshotStatus {
            owner: true,
            last_upload_secs_ago: Some(60),
            last_error: None,
            consecutive_lineage_refusals: 0,
        };
        assert_eq!(snapshot_failed(&healthy), None);
    }

    #[tokio::test]
    async fn heartbeat_streak_starts_continues_and_clears() {
        let pool = test_pool().await;
        assert_eq!(heartbeat(&pool, KIND_JEV).await, None);

        record_heartbeat_err(&pool, KIND_JEV, NOW, "timeout").await;
        let first = heartbeat(&pool, KIND_JEV).await.unwrap();
        assert_eq!(first.consecutive_failures, 1);
        assert_eq!(first.first_err_ms, Some(NOW));
        assert_eq!(first.last_err_ms, Some(NOW));

        record_heartbeat_err(&pool, KIND_JEV, NOW + 60_000, "timeout again").await;
        let second = heartbeat(&pool, KIND_JEV).await.unwrap();
        assert_eq!(second.consecutive_failures, 2);
        assert_eq!(second.first_err_ms, Some(NOW));
        assert_eq!(second.last_err_ms, Some(NOW + 60_000));

        record_heartbeat_ok(&pool, KIND_JEV, NOW + 120_000).await;
        let cleared = heartbeat(&pool, KIND_JEV).await.unwrap();
        assert_eq!(cleared.consecutive_failures, 0);
        assert_eq!(cleared.first_err_ms, None);
    }

    #[tokio::test]
    async fn ops_alert_dedupes_within_the_cooldown() {
        let pool = test_pool().await;
        let (config, codes) = test_config();
        let sync = StateSync::new("test");
        let admins = vec!["ops@example.com".to_string()];
        let cfg = cfg();
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&pool, &sync, &config.email, &admins, &cfg, dir.path());

        notify_ops(&ctx, KIND_QUEUE, "ops:queue", "subject", "body", NOW).await;
        assert_eq!(ledger_count(&pool, KIND_QUEUE).await, 1);
        assert!(codes.lock().unwrap().contains_key("ops@example.com"));

        // Inside the cooldown: silent, no new ledger row.
        codes.lock().unwrap().clear();
        notify_ops(
            &ctx,
            KIND_QUEUE,
            "ops:queue",
            "subject",
            "changed",
            NOW + 1_000,
        )
        .await;
        assert_eq!(ledger_count(&pool, KIND_QUEUE).await, 1);
        assert!(!codes.lock().unwrap().contains_key("ops@example.com"));

        // Past the cooldown: sends again.
        notify_ops(
            &ctx,
            KIND_QUEUE,
            "ops:queue",
            "subject",
            "changed",
            NOW + cfg.cooldown.as_millis() as i64,
        )
        .await;
        assert_eq!(ledger_count(&pool, KIND_QUEUE).await, 2);
    }

    #[tokio::test]
    async fn suspended_app_mails_once_per_hour_per_slug() {
        let pool = test_pool().await;
        let (config, _) = test_config();
        let sync = StateSync::new("test");
        let admins = vec!["ops@example.com".to_string()];
        let cfg = cfg();
        let dir = tempfile::tempdir().unwrap();
        seed_user(&pool, "u1", "owner@example.com").await;
        seed_app(&pool, "app1", "shop", "u1").await;
        sqlx::query("UPDATE app SET moderation_state = 'suspended' WHERE id = 'app1'")
            .execute(&pool)
            .await
            .unwrap();
        let ctx = ctx(&pool, &sync, &config.email, &admins, &cfg, dir.path());

        check_suspended(&ctx, NOW).await;
        assert_eq!(ledger_count(&pool, KIND_SUSPENDED).await, 1);

        check_suspended(&ctx, NOW + 1_000).await;
        assert_eq!(ledger_count(&pool, KIND_SUSPENDED).await, 1);

        // Lifting the suspension stops the hourly repeat.
        sqlx::query("UPDATE app SET moderation_state = 'ok' WHERE id = 'app1'")
            .execute(&pool)
            .await
            .unwrap();
        check_suspended(&ctx, NOW + cfg.cooldown.as_millis() as i64).await;
        assert_eq!(ledger_count(&pool, KIND_SUSPENDED).await, 1);
    }

    #[tokio::test]
    async fn queue_depth_counts_only_non_terminal_rows() {
        let pool = test_pool().await;
        seed_user(&pool, "u1", "owner@example.com").await;
        seed_app(&pool, "app1", "shop", "u1").await;
        for (id, status) in [
            ("d1", "queued"),
            ("d2", "building"),
            ("d3", "reviewing"),
            ("d4", "deploying"),
            ("d5", "success"),
            ("d6", "failed"),
            ("d7", "blocked"),
        ] {
            sqlx::query(
                "INSERT INTO deploy (id, app_id, sha, status, created_at) \
                 VALUES (?1, 'app1', 'abc', ?2, 0)",
            )
            .bind(id)
            .bind(status)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(deploy_queue_depth(&pool).await.unwrap(), 4);
        assert!(!queue_deep(4, &cfg()));
        let mut deep = cfg();
        deep.deploy_queue_depth = 4;
        assert!(queue_deep(4, &deep));
    }

    #[tokio::test]
    async fn email_outage_ignores_stale_streaks() {
        let pool = test_pool().await;
        let cfg = cfg();
        record_heartbeat_err(&pool, KIND_EMAIL, NOW, "email provider is rate limited").await;
        let fresh = heartbeat(&pool, KIND_EMAIL).await.unwrap();
        assert!(email_outage(&fresh, NOW + 1_000, &cfg));

        // No send attempted for over two cooldowns: stale, silent.
        let stale_at = NOW + cfg.cooldown.as_millis() as i64 * 2 + 1;
        assert!(!email_outage(&fresh, stale_at, &cfg));

        // A success clears the streak.
        record_heartbeat_ok(&pool, KIND_EMAIL, stale_at).await;
        let cleared = heartbeat(&pool, KIND_EMAIL).await.unwrap();
        assert!(!email_outage(&cleared, stale_at + 1_000, &cfg));
    }

    #[test]
    fn email_budget_fires_at_80_and_95() {
        // Edges: warn covers [80 %, 95 %), crit covers 95 % and up.
        assert!(!email_budget_warn(2399, 3000));
        assert!(email_budget_warn(2400, 3000));
        assert!(email_budget_warn(2849, 3000));
        assert!(!email_budget_warn(2850, 3000));
        assert!(!email_budget_crit(2849, 3000));
        assert!(email_budget_crit(2850, 3000));
        assert!(email_budget_crit(3000, 3000));
        assert!(
            !email_budget_warn(3000, 3000),
            "crit supersedes warn at the cap"
        );
        // A zero cap never alerts (unconfigured, not empty).
        assert!(!email_budget_warn(0, 0));
        assert!(!email_budget_crit(0, 0));
        assert!(!email_budget_crit(1, 0));
    }

    #[tokio::test]
    async fn email_budget_check_alerts_once_per_kind() {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        let pool = test_pool().await;
        let cfg = cfg();
        let sync = StateSync::new("test-budget");
        let sender = EmailSender::Capture {
            codes: Arc::new(Mutex::new(HashMap::new())),
        };
        let admins = vec!["ops@example.com".to_string()];
        let data_dir = tempfile::tempdir().unwrap();
        let data_path = data_dir.path().to_path_buf();
        std::mem::forget(data_dir);
        let month = crate::auth::email::month_key(NOW);
        async fn seed(pool: &SqlitePool, month: &str, sent: i64) {
            sqlx::query(
                "INSERT INTO email_month (month, sent) VALUES (?1, ?2) \
                 ON CONFLICT(month) DO UPDATE SET sent = ?2",
            )
            .bind(month)
            .bind(sent)
            .execute(pool)
            .await
            .unwrap();
        }
        // Below 80 %: silent.
        seed(&pool, &month, 2399).await;
        check_all(&ctx(&pool, &sync, &sender, &admins, &cfg, &data_path), NOW).await;
        assert_eq!(ledger_count(&pool, KIND_EMAIL_BUDGET_WARN).await, 0);
        assert_eq!(ledger_count(&pool, KIND_EMAIL_BUDGET_CRIT).await, 0);
        // At 80 %: one warn mail, no crit.
        seed(&pool, &month, 2400).await;
        check_all(&ctx(&pool, &sync, &sender, &admins, &cfg, &data_path), NOW).await;
        assert_eq!(ledger_count(&pool, KIND_EMAIL_BUDGET_WARN).await, 1);
        assert_eq!(ledger_count(&pool, KIND_EMAIL_BUDGET_CRIT).await, 0);
        // At 95 %: one crit mail, and the warn does not refire on the same tick.
        seed(&pool, &month, 2850).await;
        check_all(&ctx(&pool, &sync, &sender, &admins, &cfg, &data_path), NOW).await;
        assert_eq!(ledger_count(&pool, KIND_EMAIL_BUDGET_WARN).await, 1);
        assert_eq!(ledger_count(&pool, KIND_EMAIL_BUDGET_CRIT).await, 1);
    }

    #[tokio::test]
    async fn cloudflare_429_extends_the_email_streak() {
        let pool = test_pool().await;
        let (base, fake, server) = fake_email_server().await;
        let sender = EmailSender::cloudflare(
            "acct-1".to_string(),
            "cf-secret-token".to_string(),
            crate::auth::EmailFrom::parse("Outer <hello@outer.now>"),
        )
        .with_api_base(base)
        .with_ops_hook(pool.clone());
        // The fake 429s this address and delivers everything else.
        let err = sender
            .send_text("ops-ratelimit@example.com", "subject", "body")
            .await
            .expect_err("fake 429s the ratelimit address");
        assert!(matches!(err, crate::auth::email::EmailError::Retryable(_)));
        let streak = heartbeat(&pool, KIND_EMAIL).await.unwrap();
        assert_eq!(streak.consecutive_failures, 1);
        // The hook stamps the real clock, so the outage verdict runs on it
        // too (the fake NOW above is for the injected-clock decision fns).
        assert!(email_outage(&streak, crate::auth::now_ms(), &cfg()));

        sender
            .send_text("ops-ok@example.com", "subject", "body")
            .await
            .unwrap();
        let cleared = heartbeat(&pool, KIND_EMAIL).await.unwrap();
        assert_eq!(cleared.consecutive_failures, 0);
        assert_eq!(fake.delivered.lock().unwrap().len(), 1);
        server.abort();
    }

    /// Minimal Cloudflare Email Service fake: 429 for `ratelimit`
    /// recipients, success otherwise. Localhost only, like the sender
    /// fakes in [`crate::auth::tests`].
    async fn fake_email_server() -> (String, FakeInbox, tokio::task::JoinHandle<()>) {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::routing::post;
        let inbox = FakeInbox::default();
        let route_inbox = inbox.clone();
        let app = axum::Router::new().route(
            "/client/v4/accounts/{account_id}/email/sending/send",
            post(move |Json(body): Json<serde_json::Value>| async move {
                let to = body["to"].as_str().unwrap_or("").to_string();
                if to.contains("ratelimit") {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        Json(serde_json::json!({
                            "success": false,
                            "errors": [{ "code": 10004, "message": "rate limited" }],
                        })),
                    );
                }
                route_inbox.delivered.lock().unwrap().push(to);
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true, "errors": [],
                        "result": { "delivered": ["x"], "permanent_bounces": [], "queued": [] },
                    })),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        (format!("http://127.0.0.1:{port}"), inbox, handle)
    }

    #[derive(Clone, Default)]
    struct FakeInbox {
        delivered: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }
}
