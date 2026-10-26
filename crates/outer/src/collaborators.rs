//! Collaborators (spec §7.4, plan table owner-set 2026-10-26): Free 1 per
//! app, Pro 10.
//!
//! The owner (the account that created the app) invites by email. The invite
//! allowlists the email during the closed beta and emails it the spec
//! wording; the row's `userId` links when that email signs in (or is already
//! an account). Removal revokes access immediately but keeps nothing else:
//! the row is gone, so the next call answers `not_found` again.
//!
//! Collaborator access follows the owner's plan: on downgrade the rows stay
//! and the oldest collaborator keeps access (Free allows one); the rest
//! grant nothing until re-upgrade (spec §3). Everything a collaborator does
//! counts against the owner's plan (spec §3), so quota checks take the
//! owner's id.

use crate::apps::App;
use crate::auth::{normalize_email, EmailSender};

/// What the caller may do with an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Collaborator,
}
/// Why `add` refused. Callers map these to the spec §8.3 codes; nothing
/// here carries a server path or token.
#[derive(Debug, thiserror::Error)]
pub enum AddError {
    /// The address is blank or has no usable shape.
    #[error("email {0:?} is not a usable address")]
    InvalidEmail(String),
    /// The owner invited themselves.
    #[error("that address already owns the app")]
    SelfInvite,
    /// The owner's plan allows no more (`limit` per app: Free 1, Pro 10).
    #[error("this app already has its {limit} collaborator(s)")]
    LimitReached { limit: u64 },
    /// The address is already on the list (joined or pending).
    #[error("that address is already a collaborator")]
    Duplicate,
    /// The database refused the insert or lookup.
    #[error("collaborator store failed: {0}")]
    Internal(#[from] anyhow::Error),
}

/// One invite row: `joined` is true once the email has signed in and linked.
#[derive(Debug, Clone)]
pub struct Collaborator {
    pub email: String,
    pub joined: bool,
    pub added_at: i64,
}

/// The caller's [`Role`] on the app, or `None` when the slug should read as
/// missing (spec §8.2: a slug the caller has no role on answers `not_found`,
/// exactly like a missing one). Access follows the owner's plan: Pro grants
/// every row; Free grants only the oldest collaborator (the Free allowance
/// is one per app), so a downgrade pauses the rest without deleting them.
pub async fn role(
    pool: &sqlx::SqlitePool,
    app_id: &str,
    owner_id: &str,
    account_id: &str,
    account_email: &str,
) -> anyhow::Result<Option<Role>> {
    if owner_id == account_id {
        return Ok(Some(Role::Owner));
    }
    let email = normalize_email(account_email);
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT userId FROM app_collaborator WHERE app_id = ?1 AND (userId = ?2 OR email = ?3)",
    )
    .bind(app_id)
    .bind(account_id)
    .bind(&email)
    .fetch_optional(pool)
    .await?;
    let Some(_) = row else {
        return Ok(None);
    };
    if owner_plan(pool, owner_id).await? == "pro" {
        return Ok(Some(Role::Collaborator));
    }
    // Free: only the oldest row keeps access. `added_at` is ms-resolution,
    // so same-millisecond invites tie — `rowid` breaks the tie by insertion
    // order, keeping the choice stable across downgrades.
    let oldest: Option<String> = sqlx::query_scalar(
        "SELECT email FROM app_collaborator WHERE app_id = ?1 ORDER BY added_at ASC, rowid ASC LIMIT 1",
    )
    .bind(app_id)
    .fetch_optional(pool)
    .await?;
    Ok(if oldest.as_deref() == Some(email.as_str()) {
        Some(Role::Collaborator)
    } else {
        None
    })
}

/// The app's collaborators, oldest first.
pub async fn list(pool: &sqlx::SqlitePool, app_id: &str) -> anyhow::Result<Vec<Collaborator>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        email: String,
        #[sqlx(rename = "userId")]
        user_id: Option<String>,
        added_at: i64,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT email, userId, added_at FROM app_collaborator \
         WHERE app_id = ?1 ORDER BY added_at ASC, rowid ASC",
    )
    .bind(app_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Collaborator {
            email: row.email,
            joined: row.user_id.is_some(),
            added_at: row.added_at,
        })
        .collect())
}

/// Invite `raw_email` to `app`: the owner's plan cap (Free 1, Pro 10), the
/// insert (linking `userId` when the address already has an account), the
/// closed-beta allowlist, and the spec-worded email. Returns the stored row.
pub async fn add(
    pool: &sqlx::SqlitePool,
    sender: &EmailSender,
    mcp_url: &str,
    app: &App,
    owner_email: &str,
    raw_email: &str,
) -> Result<Collaborator, AddError> {
    let email = normalize_email(raw_email);
    if !crate::auth::valid_email(&email) {
        return Err(AddError::InvalidEmail(raw_email.to_string()));
    }
    if email == normalize_email(owner_email) {
        return Err(AddError::SelfInvite);
    }
    let plan = owner_plan(pool, &app.owner_id)
        .await
        .map_err(AddError::Internal)?;
    let limit = crate::limits::collaborator_limit(&plan);
    // The count and the insert run in one immediate-write transaction: two
    // racing invites cannot both read under the cap and both insert. The
    // lock is per-database (SQLite takes the write lock up front), so this
    // closes the race across processes too, not just across tasks.
    let mut conn = pool
        .acquire()
        .await
        .map_err(|err| AddError::Internal(err.into()))?;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *conn)
        .await
        .map_err(|err| AddError::Internal(err.into()))?;
    let outcome = add_inner(&mut conn, app, &email, limit).await;
    match outcome {
        Ok((user_id, now)) => {
            if let Err(err) = sqlx::query("COMMIT").execute(&mut *conn).await {
                return Err(AddError::Internal(err.into()));
            }
            finish_invite(
                pool,
                sender,
                mcp_url,
                app,
                owner_email,
                &email,
                user_id,
                now,
            )
            .await
        }
        Err(err) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(err)
        }
    }
}

/// The count check and the row insert, inside the caller's `BEGIN IMMEDIATE`
/// transaction. Returns the linked `userId` and the invite timestamp for the
/// email the caller sends after commit.
async fn add_inner(
    conn: &mut sqlx::SqliteConnection,
    app: &App,
    email: &str,
    limit: u64,
) -> Result<(Option<String>, i64), AddError> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app_collaborator WHERE app_id = ?1")
        .bind(&app.id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|err| AddError::Internal(err.into()))?
        .unwrap_or(0);
    if count.max(0) as u64 >= limit {
        return Err(AddError::LimitReached { limit });
    }
    // An existing account links immediately; a future sign-in links through
    // [`link_user`].
    let user_id: Option<String> = sqlx::query_scalar("SELECT id FROM user WHERE email = ?1")
        .bind(email)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|err| AddError::Internal(err.into()))?;
    let now = now_ms();
    let inserted = sqlx::query(
        "INSERT INTO app_collaborator (app_id, email, userId, added_at) \
         VALUES (?1, ?2, ?3, ?4) ON CONFLICT(app_id, email) DO NOTHING",
    )
    .bind(&app.id)
    .bind(email)
    .bind(&user_id)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|err| AddError::Internal(err.into()))?;
    if inserted.rows_affected() == 0 {
        return Err(AddError::Duplicate);
    }
    Ok((user_id, now))
}

/// The post-commit half of [`add`]: the closed-beta allowlist and the
/// spec-worded invite email. Runs after `COMMIT` so a rolled-back invite
/// never allowlists or mails.
#[allow(clippy::too_many_arguments)]
async fn finish_invite(
    pool: &sqlx::SqlitePool,
    sender: &EmailSender,
    mcp_url: &str,
    app: &App,
    owner_email: &str,
    email: &str,
    user_id: Option<String>,
    now: i64,
) -> Result<Collaborator, AddError> {
    // A Pro invite admits the email for the closed beta (spec §7.4). The
    // invite never grants codes: allowlisted emails join with none (§6.5.1).
    sqlx::query("INSERT INTO beta_allowlist (email, added_at) VALUES (?1, ?2) ON CONFLICT(email) DO NOTHING")
        .bind(email)
        .bind(now)
        .execute(pool)
        .await
        .map_err(|err| AddError::Internal(err.into()))?;
    let owner_name = owner_email
        .split('@')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(owner_email);
    let subject = format!("You've been added to {} on Outer", app.slug);
    let text = format!(
        "{owner_name} added you to {} on Outer. Ask your agent to connect to {mcp_url}.",
        app.slug
    );
    if let Err(err) = sender.send_text(email, &subject, &text).await {
        tracing::warn!(app = %app.slug, email = %email, "collaborator invite email failed: {err}");
    }
    Ok(Collaborator {
        email: email.to_string(),
        joined: user_id.is_some(),
        added_at: now,
    })
}

/// Remove `raw_email` from `app`. True when a row was there: removal revokes
/// access on the next call, since [`role`] reads this table every time. The
/// beta allowlist keeps the email (admission is sticky; re-invites resend).
pub async fn remove(
    pool: &sqlx::SqlitePool,
    app_id: &str,
    raw_email: &str,
) -> anyhow::Result<bool> {
    let email = normalize_email(raw_email);
    let deleted = sqlx::query("DELETE FROM app_collaborator WHERE app_id = ?1 AND email = ?2")
        .bind(app_id)
        .bind(&email)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Link `app_collaborator.userId` when an address becomes an account. Called
/// from account creation (both the email-code and OAuth paths create rows
/// there); invites to existing accounts link in [`add`] instead.
pub async fn link_user(pool: &sqlx::SqlitePool, email: &str, user_id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE app_collaborator SET userId = ?1 WHERE email = ?2 AND userId IS NULL")
        .bind(user_id)
        .bind(normalize_email(email))
        .execute(pool)
        .await?;
    Ok(())
}

async fn owner_plan(pool: &sqlx::SqlitePool, owner_id: &str) -> anyhow::Result<String> {
    let plan: Option<String> = sqlx::query_scalar("SELECT plan FROM user WHERE id = ?1")
        .bind(owner_id)
        .fetch_optional(pool)
        .await?;
    Ok(plan.unwrap_or_else(|| "free".to_string()))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> (tempfile::TempDir, sqlx::SqlitePool, App, EmailSender) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO user (id, email, plan, lastActiveAt, createdAt, updatedAt) \
             VALUES ('owner', 'owner@example.com', 'free', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
             last_activity_at, created_at) \
             VALUES ('app1', 'shop', 'owner', 300001, 20001, 20002, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let app = crate::apps::get_by_slug(&pool, "shop")
            .await
            .unwrap()
            .unwrap();
        let sender = EmailSender::Capture {
            codes: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        };
        (dir, pool, app, sender)
    }

    #[tokio::test]
    async fn concurrent_invites_share_one_free_slot() {
        let (_dir, pool, app, sender) = setup().await;
        // Eight invites race for the single Free slot: the count and the
        // insert are one atomic step, so exactly one wins and the rest hit
        // the cap — never two rows.
        let mut tasks = Vec::new();
        for i in 0..8 {
            let (pool, app, sender) = (pool.clone(), app.clone(), sender.clone());
            tasks.push(tokio::spawn(async move {
                add(
                    &pool,
                    &sender,
                    "https://outer.now",
                    &app,
                    "owner@example.com",
                    &format!("racer{i}@example.com"),
                )
                .await
            }));
        }
        let mut wins = 0;
        let mut limited = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => wins += 1,
                Err(AddError::LimitReached { limit: 1 }) => limited += 1,
                Err(other) => panic!("unexpected invite outcome: {other:?}"),
            }
        }
        assert_eq!(wins, 1, "exactly one racing invite takes the Free slot");
        assert_eq!(limited, 7, "the rest hit the cap, none slip past it");
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM app_collaborator WHERE app_id = 'app1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn same_millisecond_invites_keep_a_stable_oldest() {
        let (_dir, pool, app, _) = setup().await;
        // Two invites stamped in the same millisecond tie on `added_at`;
        // insertion order (`rowid`) decides, deterministically.
        for email in ["first@example.com", "second@example.com"] {
            sqlx::query(
                "INSERT INTO app_collaborator (app_id, email, userId, added_at) \
                 VALUES ('app1', ?1, NULL, 1_700_000_000_000)",
            )
            .bind(email)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(
            role(&pool, &app.id, &app.owner_id, "nobody", "first@example.com")
                .await
                .unwrap(),
            Some(Role::Collaborator),
            "the first-inserted tied invite keeps access on Free"
        );
        assert_eq!(
            role(
                &pool,
                &app.id,
                &app.owner_id,
                "nobody",
                "second@example.com"
            )
            .await
            .unwrap(),
            None,
            "the second-inserted tied invite pauses on Free"
        );
        let listed = list(&pool, &app.id).await.unwrap();
        let order: Vec<&str> = listed.iter().map(|c| c.email.as_str()).collect();
        assert_eq!(order, ["first@example.com", "second@example.com"]);
    }
}
