//! The platform database: one SQLite file, migrated at boot.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

/// Open (creating if missing) the platform database at `path` and apply every
/// pending migration from `migrations/`.
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reopening_applies_migrations_once_and_enforces_foreign_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outer.sqlite");

        let pool = open(&path).await.unwrap();
        sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('a1', 'ana@example.com', 0, 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let pool = open(&path).await.unwrap();
        let (emails,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            emails, 1,
            "a reopen keeps data and does not re-run migrations"
        );

        let orphan = sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, last_activity_at, created_at) VALUES ('p1', 'grocery', 'nobody', 200000, 0, 0)",
        )
        .execute(&pool)
        .await;
        assert!(orphan.is_err(), "an app must reference an existing user");

        let violations: Vec<(i64, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(
            violations.is_empty(),
            "a fresh database has no foreign-key violations: {violations:?}"
        );
    }
}
