//! Keep-warm flag (feature #6): per-app `always_on`, Pro only.
//!
//! The sweep skips flagged apps on Pro owners (up to the host cap; the
//! oldest-idle sleep past it) and the warm-up wakes flagged apps that
//! stopped. Downgrade to Free leaves the flag stored but ignored.

use sqlx::SqlitePool;

/// Why [`set_always_on`] refused the change. Turning the flag on is a Pro
/// feature; turning it off is always allowed, so a downgraded owner can
/// still shed load.
#[derive(Debug, thiserror::Error)]
pub enum AlwaysOnError {
    #[error("keeping {slug} warm needs Pro; turn it off any time")]
    ProOnly { slug: String },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// The owner's plan (`"pro"` or anything else), defaulting to free for a
/// missing account row — the same default the MCP layer uses.
pub async fn owner_plan(pool: &SqlitePool, owner_id: &str) -> anyhow::Result<String> {
    let plan: Option<String> = sqlx::query_scalar("SELECT plan FROM user WHERE id = ?1")
        .bind(owner_id)
        .fetch_optional(pool)
        .await?;
    Ok(plan.unwrap_or_else(|| "free".to_string()))
}

/// Set the keep-warm flag. Owner-only (the MCP/console layers enforce the
/// role); `plan` is the owner's plan. `on` requires Pro; `off` never does.
pub async fn set_always_on(
    pool: &SqlitePool,
    app_id: &str,
    on: bool,
    plan: &str,
) -> Result<(), AlwaysOnError> {
    if on && plan != "pro" {
        let slug: Option<String> = sqlx::query_scalar("SELECT slug FROM app WHERE id = ?1")
            .bind(app_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| AlwaysOnError::Internal(e.into()))?;
        return Err(AlwaysOnError::ProOnly {
            slug: slug.unwrap_or_else(|| app_id.to_string()),
        });
    }
    sqlx::query("UPDATE app SET always_on = ?1 WHERE id = ?2")
        .bind(i64::from(on))
        .bind(app_id)
        .execute(pool)
        .await
        .map_err(|e| AlwaysOnError::Internal(e.into()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed(pool: &SqlitePool, plan: &str) {
        sqlx::query(
            "INSERT INTO user (id, email, plan, lastActiveAt, createdAt, updatedAt) \
             VALUES ('a1', 'a@example.com', ?1, 0, 0, 0)",
        )
        .bind(plan)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, \
             last_activity_at, created_at) \
             VALUES ('app1', 'groceries', 'a1', 200001, 21001, 21002, 'unlisted', 0, 0)",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pro_owner_may_turn_keep_warm_on() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        seed(&pool, "pro").await;
        set_always_on(&pool, "app1", true, "pro").await.unwrap();
        let flag: i64 = sqlx::query_scalar("SELECT always_on FROM app WHERE id = 'app1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(flag, 1);
    }

    #[tokio::test]
    async fn free_owner_is_refused_on_but_may_turn_off() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        seed(&pool, "free").await;
        let err = set_always_on(&pool, "app1", true, "free")
            .await
            .unwrap_err();
        assert!(
            matches!(err, AlwaysOnError::ProOnly { .. }),
            "Free on is ProOnly, got {err:?}"
        );
        // The flag stays as it was.
        let flag: i64 = sqlx::query_scalar("SELECT always_on FROM app WHERE id = 'app1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(flag, 0);
        // Off is always allowed, even on Free.
        set_always_on(&pool, "app1", false, "free").await.unwrap();
    }

    #[tokio::test]
    async fn missing_account_defaults_to_free() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        assert_eq!(owner_plan(&pool, "ghost").await.unwrap(), "free");
    }
}
