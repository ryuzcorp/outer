//! Abuse limits on the `rate_limit_events` ledger: OTP sends (5/h per
//! email, 20/h per IP), DCR registrations (20/h per IP), account creation
//! (3/day per IP). Rows older than the window stay until the daily prune.

use sqlx::SqlitePool;

/// Longest window `check_and_record` is called with (account creation,
/// 3/day per IP): rows older than `now - RETENTION_MS` are outside every
/// window, so the daily sweep may delete them. Bump it when adding a wider
/// window.
pub const RETENTION_MS: i64 = 86_400_000;

/// Count events for `subject` + `action` inside the window; when under the
/// limit, record this attempt and return `Ok(None)`. When the limit is hit,
/// return `Ok(Some(retry_after_s))`: the seconds until the oldest event in
/// the window ages out.
pub async fn check_and_record(
    pool: &SqlitePool,
    subject: &str,
    action: &str,
    limit: u32,
    window_ms: i64,
    now: i64,
) -> anyhow::Result<Option<u64>> {
    debug_assert!(
        window_ms <= RETENTION_MS,
        "window wider than the daily prune keeps: bump RETENTION_MS"
    );
    let since = now - window_ms;
    let (count, oldest): (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*), MIN(created_at) FROM rate_limit_events \
         WHERE subject = ? AND action = ? AND created_at > ?",
    )
    .bind(subject)
    .bind(action)
    .bind(since)
    .fetch_one(pool)
    .await?;
    if count >= i64::from(limit) {
        let retry_after_s = oldest
            .map(|first| (first + window_ms - now).max(1))
            .map(|wait_ms| ((wait_ms + 999) / 1000).max(1) as u64)
            .unwrap_or(1);
        return Ok(Some(retry_after_s));
    }
    sqlx::query("INSERT INTO rate_limit_events (subject, action, created_at) VALUES (?, ?, ?)")
        .bind(subject)
        .bind(action)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(None)
}

/// Delete ledger rows older than `older_than_ms` (the daily prune).
pub async fn prune(pool: &SqlitePool, older_than_ms: i64) -> anyhow::Result<u64> {
    let result = sqlx::query("DELETE FROM rate_limit_events WHERE created_at < ?")
        .bind(older_than_ms)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::auth::test_support::test_pool().await
    }

    #[tokio::test]
    async fn limit_hits_after_n_attempts_with_retry_after() {
        let pool = pool().await;
        for _ in 0..5 {
            let limited =
                check_and_record(&pool, "email:a@x.y", "otp_send", 5, 3_600_000, 1_000_000)
                    .await
                    .unwrap();
            assert_eq!(limited, None);
        }
        let limited = check_and_record(
            &pool,
            "email:a@x.y",
            "otp_send",
            5,
            3_600_000,
            1_000_000 + 1_000,
        )
        .await
        .unwrap();
        // Oldest event ages out one hour after the first send.
        assert_eq!(limited, Some(3599));
        // Other subjects are unaffected.
        let limited = check_and_record(
            &pool,
            "email:b@x.y",
            "otp_send",
            5,
            3_600_000,
            1_000_000 + 1_000,
        )
        .await
        .unwrap();
        assert_eq!(limited, None);
    }

    #[tokio::test]
    async fn window_sliding_readmits_after_expiry() {
        let pool = pool().await;
        let limited = check_and_record(&pool, "ip:1.2.3.4", "otp_send", 1, 60_000, 0)
            .await
            .unwrap();
        assert_eq!(limited, None);
        let limited = check_and_record(&pool, "ip:1.2.3.4", "otp_send", 1, 60_000, 30_000)
            .await
            .unwrap();
        assert_eq!(limited, Some(30));
        let limited = check_and_record(&pool, "ip:1.2.3.4", "otp_send", 1, 60_000, 61_000)
            .await
            .unwrap();
        assert_eq!(limited, None);
    }

    #[tokio::test]
    async fn prune_deletes_only_rows_older_than_the_cutoff() {
        let pool = pool().await;
        for created_at in [10, 99_999, 100_000] {
            sqlx::query(
                "INSERT INTO rate_limit_events (subject, action, created_at) VALUES ('ip:1', 'otp_send', ?)",
            )
            .bind(created_at)
            .execute(&pool)
            .await
            .unwrap();
        }
        let removed = prune(&pool, 100_000).await.unwrap();
        assert_eq!(removed, 2);
        let kept: Vec<i64> = sqlx::query_scalar("SELECT created_at FROM rate_limit_events")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(kept, vec![100_000]);
    }
}
