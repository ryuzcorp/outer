//! Apps: the `app` table, account lookup, creation with uid/port allocation,
//! and slug quarantine on delete.
//!
//! The proxy slice reads through [`get_by_slug`] and [`list`]; the deploy
//! slice records [`set_last_deploy`]; the fleet manager drives [`set_asleep`]
//! and [`touch_request`].

use anyhow::Context as _;
use sqlx::SqlitePool;

pub mod delete;
pub(crate) mod local;
pub mod rename;
mod slug;
pub use slug::{validate_slug, SlugError};

/// Bucket prefixes a slug owns. Shared by the claim check, the rename guard
/// and the maintenance drain: a slug is exactly its two prefixes.
pub(crate) fn cleanup_prefixes(slug: &str) -> [String; 2] {
    [format!("fleets/{slug}/"), format!("git/{slug}/")]
}

/// How long a deleted slug stays quarantined before anyone may reuse it.
pub(crate) const QUARANTINE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// The new slug is claimable: well-formed, not reserved, naming no live app,
/// and not inside its 30-day quarantine. An expired quarantine row is
/// deleted, so a stale prune never blocks a legitimate reuse. Shared by
/// [`create`] and [`rename::rename_app`]: both refuse exactly the same
/// slugs with exactly the same errors.
pub(crate) async fn check_slug_claimable(pool: &SqlitePool, slug: &str) -> Result<(), CreateError> {
    let mut conn = pool.acquire().await.map_err(internal)?;
    check_slug_claimable_conn(&mut conn, slug).await
}

/// [`check_slug_claimable`] on an open transaction, for the rename commit's
/// authoritative re-check (a racing create converges on `Taken` there).
pub(crate) async fn check_slug_claimable_conn(
    conn: &mut sqlx::SqliteConnection,
    slug: &str,
) -> Result<(), CreateError> {
    validate_slug(slug)?;
    let now = now_ms();
    // A pending cleanup row blocks the slug even past its quarantine: the
    // prefix still holds someone's data, so no app may inherit it. Checked
    // first, so an expired quarantine row survives below and the old
    // hostname keeps redirecting while the drain is still due.
    let [fleets_prefix, git_prefix] = cleanup_prefixes(slug);
    let blocked: Option<String> =
        sqlx::query_scalar("SELECT prefix FROM prefix_cleanup WHERE prefix IN (?1, ?2) LIMIT 1")
            .bind(&fleets_prefix)
            .bind(&git_prefix)
            .fetch_optional(&mut *conn)
            .await
            .map_err(internal)?;
    if blocked.is_some() {
        return Err(CreateError::Taken);
    }
    let taken: Option<String> = sqlx::query_scalar("SELECT id FROM app WHERE slug = ?1")
        .bind(slug)
        .fetch_optional(&mut *conn)
        .await
        .map_err(internal)?;
    if taken.is_some() {
        return Err(CreateError::Taken);
    }
    let until: Option<i64> =
        sqlx::query_scalar("SELECT until FROM slug_quarantine WHERE slug = ?1")
            .bind(slug)
            .fetch_optional(&mut *conn)
            .await
            .map_err(internal)?;
    if let Some(until) = until {
        if until > now {
            return Err(CreateError::Taken);
        }
        sqlx::query("DELETE FROM slug_quarantine WHERE slug = ?1")
            .bind(slug)
            .execute(&mut *conn)
            .await
            .map_err(internal)?;
    }
    Ok(())
}

/// One `slug_quarantine` row: when the slug is claimable again, and where a
/// renamed app went (`None` for plain deletes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineRow {
    pub until: i64,
    pub renamed_to: Option<String>,
}

/// The quarantine row for `slug`, if any. The proxy uses it to redirect an
/// old hostname to its renamed app.
pub async fn quarantine_lookup(
    pool: &SqlitePool,
    slug: &str,
) -> anyhow::Result<Option<QuarantineRow>> {
    let row: Option<(i64, Option<String>)> =
        sqlx::query_as("SELECT until, renamed_to FROM slug_quarantine WHERE slug = ?1")
            .bind(slug)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(until, renamed_to)| QuarantineRow { until, renamed_to }))
}

/// Fast pre-check for renames: well-formed, naming no live app and no live
/// quarantine. Pending cleanup rows are ignored on purpose: they may be our
/// own abandoned attempt's (adopted by the pre-copy guard, whose copy wipes
/// the target first) or another in-flight rename's target (serialized on the
/// new slug's wake lock, which the rename holds). The flip transaction
/// re-checks authoritatively with [`check_slug_claimable_conn`], cleanup
/// rows included — so this early refusal can only be conservative, never
/// wrong in the other direction.
pub(crate) async fn check_slug_preclaim(pool: &SqlitePool, slug: &str) -> Result<(), CreateError> {
    validate_slug(slug)?;
    let taken: Option<String> = sqlx::query_scalar("SELECT id FROM app WHERE slug = ?1")
        .bind(slug)
        .fetch_optional(pool)
        .await
        .map_err(internal)?;
    if taken.is_some() {
        return Err(CreateError::Taken);
    }
    let until: Option<i64> =
        sqlx::query_scalar("SELECT until FROM slug_quarantine WHERE slug = ?1")
            .bind(slug)
            .fetch_optional(pool)
            .await
            .map_err(internal)?;
    if until.is_some_and(|until| until > now_ms()) {
        return Err(CreateError::Taken);
    }
    Ok(())
}

/// One process-wide lock serializing [`create`]: the check-then-insert over
/// slug, quarantine, uids and ports must not interleave with itself.
static CREATE_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// The rename's pre-copy guard runs under the same lock: recording the
/// target's cleanup rows and checking the target unclaimed must not
/// interleave with a create's check-then-insert for the same slug. Held
/// only for two indexed statements, and never while holding app locks in
/// the other direction (create and delete take no per-app locks under it),
/// so no lock cycle.
pub(crate) async fn lock_for_rename_target() -> tokio::sync::MutexGuard<'static, ()> {
    CREATE_LOCK.lock().await
}

/// Who may open the app. Unlisted hides it from search engines; the pages
/// themselves stay reachable to anyone with the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Unlisted,
    Public,
}

impl Visibility {
    /// The `app.visibility` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Unlisted => "unlisted",
            Visibility::Public => "public",
        }
    }
}

/// One row of the `app` table.
#[derive(Debug, Clone)]
pub struct App {
    pub id: String,
    pub slug: String,
    pub owner_id: String,
    pub name: Option<String>,
    pub uid: u32,
    pub port: u16,
    pub internal_port: u16,
    pub visibility: Visibility,
    pub asleep_since: Option<i64>,
    pub last_request_at: Option<i64>,
    pub last_deploy_sha: Option<String>,
    pub created_at: i64,
}

/// The free uid/port ranges one app takes its [`App::uid`], [`App::port`]
/// and [`App::internal_port`] from. Bounds are inclusive.
#[derive(Debug, Clone, Copy)]
pub struct Allocation {
    pub uid_range: (u32, u32),
    pub port_range: (u16, u16),
}

/// App slots per plan (spec §3): Free owns 1 app, Pro 10. Enforced in
/// [`create`]; the MCP layer's early check uses the same numbers.
pub fn app_limit_for_plan(plan: &str) -> u64 {
    if plan == "pro" {
        10
    } else {
        1
    }
}

/// Why [`create`] refused the app. Each message tells the agent what to change.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// The slug breaks the shape rule or is reserved.
    #[error(transparent)]
    InvalidSlug(#[from] SlugError),
    /// The slug names a live app, or its quarantine has not expired yet.
    #[error("slug is taken or quarantined; choose another slug")]
    Taken,
    /// The range has no room left: `"uids"` or `"ports"`.
    #[error("no free {0} left in the configured range; ask the operator to widen it")]
    Exhausted(&'static str),
    /// The owner's plan allows no more apps (`limit` slots, `used` taken).
    #[error("plan allows {limit} app(s), {used} owned")]
    Quota { limit: u64, used: u64 },
    /// Anything else: the database or the pool failed.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

fn internal(e: sqlx::Error) -> CreateError {
    CreateError::Internal(e.into())
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.message().contains("UNIQUE constraint failed"))
}

const APP_COLS: &str = "id, slug, owner_id, name, uid, port, internal_port, visibility, \
    asleep_since, last_request_at, last_deploy_sha, created_at";

#[derive(Debug, sqlx::FromRow)]
struct AppRow {
    id: String,
    slug: String,
    owner_id: String,
    name: Option<String>,
    uid: i64,
    port: Option<i64>,
    internal_port: Option<i64>,
    visibility: String,
    asleep_since: Option<i64>,
    last_request_at: Option<i64>,
    last_deploy_sha: Option<String>,
    created_at: i64,
}

impl AppRow {
    fn into_app(self) -> anyhow::Result<App> {
        let port = self.port.context("app row has no port")?;
        let internal_port = self.internal_port.context("app row has no internal_port")?;
        let visibility = match self.visibility.as_str() {
            "public" => Visibility::Public,
            "unlisted" => Visibility::Unlisted,
            other => anyhow::bail!("unknown visibility {other:?}"),
        };
        Ok(App {
            id: self.id,
            slug: self.slug,
            owner_id: self.owner_id,
            name: self.name,
            uid: u32::try_from(self.uid).context("uid out of range")?,
            port: u16::try_from(port).context("port out of range")?,
            internal_port: u16::try_from(internal_port).context("internal_port out of range")?,
            visibility,
            asleep_since: self.asleep_since,
            last_request_at: self.last_request_at,
            last_deploy_sha: self.last_deploy_sha,
            created_at: self.created_at,
        })
    }
}

/// Unix milliseconds, the unit of every timestamp in the schema.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The account id for `email` (lowercased), creating the row when missing and
/// refreshing `last_active_at` either way.
pub async fn ensure_account(pool: &SqlitePool, email: &str) -> anyhow::Result<String> {
    let email = email.trim().to_lowercase();
    anyhow::ensure!(!email.is_empty(), "email is empty");
    let now = now_ms();
    if let Some(id) = sqlx::query_scalar::<_, String>("SELECT id FROM user WHERE email = ?1")
        .bind(&email)
        .fetch_optional(pool)
        .await?
    {
        sqlx::query("UPDATE user SET lastActiveAt = ?1, updatedAt = ?1 WHERE id = ?2")
            .bind(now)
            .bind(&id)
            .execute(pool)
            .await?;
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let insert = sqlx::query(
        "INSERT INTO user (id, email, emailVerified, lastActiveAt, createdAt, updatedAt) \
         VALUES (?1, ?2, 1, ?3, ?3, ?3)",
    )
    .bind(&id)
    .bind(&email)
    .bind(now)
    .execute(pool)
    .await;
    match insert {
        Ok(_) => Ok(id),
        Err(e) if is_unique_violation(&e) => {
            // A concurrent caller inserted the same email first.
            let id = sqlx::query_scalar::<_, String>("SELECT id FROM user WHERE email = ?1")
                .bind(&email)
                .fetch_one(pool)
                .await?;
            sqlx::query("UPDATE user SET lastActiveAt = ?1, updatedAt = ?1 WHERE id = ?2")
                .bind(now)
                .bind(&id)
                .execute(pool)
                .await?;
            Ok(id)
        }
        Err(e) => Err(e.into()),
    }
}

/// The lowest `n` integers in `[lo, hi]` absent from `used` (sorted). `None`
/// when the range cannot fit `n` more.
fn lowest_free(used: &[i64], lo: i64, hi: i64, n: usize) -> Option<Vec<i64>> {
    let mut out = Vec::with_capacity(n);
    let mut cand = lo;
    while cand <= hi && out.len() < n {
        if used.binary_search(&cand).is_err() {
            out.push(cand);
        }
        cand += 1;
    }
    (out.len() == n).then_some(out)
}

/// Create an app: validate the slug, refuse taken and quarantined slugs,
/// re-check the owner's plan quota under the lock (a pre-check races), then
/// allocate the lowest free uid and the two lowest free ports no app row uses,
/// and insert with `last_activity_at` = now and `desired_state` 'running'.
///
/// One process-wide mutex serializes creates; a `UNIQUE` race against another
/// process still maps to [`CreateError::Taken`].
pub async fn create(
    pool: &SqlitePool,
    owner_id: &str,
    slug: &str,
    name: Option<&str>,
    alloc: &Allocation,
    app_limit: u64,
) -> Result<App, CreateError> {
    let _guard = CREATE_LOCK.lock().await;
    check_slug_claimable(pool, slug).await?;
    let now = now_ms();
    // The plan quota re-checked under the lock: an earlier check races with
    // concurrent creates, so the count here is the one that counts.
    let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app WHERE owner_id = ?1")
        .bind(owner_id)
        .fetch_optional(pool)
        .await
        .map_err(internal)?
        .unwrap_or(0);
    let used = owned.max(0) as u64;
    if used >= app_limit {
        return Err(CreateError::Quota {
            limit: app_limit,
            used,
        });
    }
    let mut uids: Vec<i64> = sqlx::query_scalar("SELECT uid FROM app")
        .fetch_all(pool)
        .await
        .map_err(internal)?;
    uids.sort_unstable();
    let uid = lowest_free(&uids, alloc.uid_range.0 as i64, alloc.uid_range.1 as i64, 1)
        .ok_or(CreateError::Exhausted("uids"))?[0];
    let mut ports: Vec<i64> = sqlx::query_scalar(
        "SELECT port FROM app WHERE port IS NOT NULL \
         UNION SELECT internal_port FROM app WHERE internal_port IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(internal)?;
    ports.sort_unstable();
    let free = lowest_free(
        &ports,
        alloc.port_range.0 as i64,
        alloc.port_range.1 as i64,
        2,
    )
    .ok_or(CreateError::Exhausted("ports"))?;
    let (port, internal_port) = (free[0], free[1]);
    let id = uuid::Uuid::new_v4().to_string();
    let insert = sqlx::query(
        "INSERT INTO app (id, slug, owner_id, name, uid, port, internal_port, \
         desired_state, visibility, last_activity_at, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'running', 'unlisted', ?8, ?8)",
    )
    .bind(&id)
    .bind(slug)
    .bind(owner_id)
    .bind(name)
    .bind(uid)
    .bind(port)
    .bind(internal_port)
    .bind(now)
    .execute(pool)
    .await;
    if let Err(e) = insert {
        if is_unique_violation(&e) {
            return Err(CreateError::Taken);
        }
        return Err(internal(e));
    }
    Ok(App {
        id,
        slug: slug.to_string(),
        owner_id: owner_id.to_string(),
        name: name.map(str::to_string),
        uid: uid as u32,
        port: port as u16,
        internal_port: internal_port as u16,
        visibility: Visibility::Unlisted,
        asleep_since: None,
        last_request_at: None,
        last_deploy_sha: None,
        created_at: now,
    })
}

/// Delete the app row and quarantine its slug for 30 days. Missing rows are a
/// quiet no-op (there is no slug to quarantine).
///
/// One transaction: a failed quarantine insert rolls the delete back, so the
/// slug never becomes claimable without its quarantine. A plain `INSERT`
/// (not `OR REPLACE`) makes a conflicting quarantine row fail the delete
/// instead of silently refreshing another app's quarantine.
pub async fn delete_row(pool: &SqlitePool, app_id: &str) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    let slug: Option<String> = sqlx::query_scalar("SELECT slug FROM app WHERE id = ?1")
        .bind(app_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(slug) = slug else { return Ok(()) };
    sqlx::query("DELETE FROM app WHERE id = ?1")
        .bind(app_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO slug_quarantine (slug, until) VALUES (?1, ?2)")
        .bind(&slug)
        .bind(now_ms() + QUARANTINE_MS)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Delete quarantine rows whose slug is claimable again (the daily prune).
/// `create` treats `until > now` as taken and deletes the row otherwise, so
/// `until <= now` is exactly the dead set — except a slug with a pending
/// `prefix_cleanup` row, which stays blocked (and keeps redirecting, for
/// renames) until the drain deletes its leftover prefix. Returns the rows
/// removed.
pub async fn prune_quarantine(pool: &SqlitePool, now: i64) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM slug_quarantine WHERE until <= ?1 AND NOT EXISTS ( \
             SELECT 1 FROM prefix_cleanup \
             WHERE prefix IN ('fleets/' || slug || '/', 'git/' || slug || '/'))",
    )
    .bind(now)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// The app with `slug`, if any.
pub async fn get_by_slug(pool: &SqlitePool, slug: &str) -> anyhow::Result<Option<App>> {
    let row: Option<AppRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {APP_COLS} FROM app WHERE slug = ?1"
    )))
    .bind(slug)
    .fetch_optional(pool)
    .await?;
    row.map(AppRow::into_app).transpose()
}

/// The app with `id`, if any.
pub async fn get_by_id(pool: &SqlitePool, app_id: &str) -> anyhow::Result<Option<App>> {
    let row: Option<AppRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {APP_COLS} FROM app WHERE id = ?1"
    )))
    .bind(app_id)
    .fetch_optional(pool)
    .await?;
    row.map(AppRow::into_app).transpose()
}

/// Every app, in slug order.
pub async fn list(pool: &SqlitePool) -> anyhow::Result<Vec<App>> {
    let rows: Vec<AppRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {APP_COLS} FROM app ORDER BY slug"
    )))
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(AppRow::into_app).collect()
}

/// Mark the app asleep at `since_ms`, or wake it when `None` (clears the marker).
pub async fn set_asleep(
    pool: &SqlitePool,
    app_id: &str,
    since_ms: Option<i64>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE app SET asleep_since = ?1 WHERE id = ?2")
        .bind(since_ms)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record a served request at `at_ms`. A request means the app is awake, so
/// this also clears `asleep_since`.
pub async fn touch_request(pool: &SqlitePool, app_id: &str, at_ms: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE app SET last_request_at = ?1, asleep_since = NULL WHERE id = ?2")
        .bind(at_ms)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record the sha of the app's latest successful deploy.
pub async fn set_last_deploy(pool: &SqlitePool, app_id: &str, sha: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE app SET last_deploy_sha = ?1 WHERE id = ?2")
        .bind(sha)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Why [`set_visibility`] refused the change. `ProOnly` is decided here, in
/// the shared domain function, so the MCP tool and the console endpoint gate
/// identically: making an app `public` is a Pro feature, switching back to
/// `unlisted` is always allowed.
#[derive(Debug, thiserror::Error)]
pub enum VisibilityError {
    #[error("public apps are a Pro feature; upgrade to Pro to make the app public (unlisted is always free)")]
    ProOnly,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Change who may discover the app: `unlisted` (default, kept out of search
/// engines) or `public`. Owner-only; the MCP layer enforces the role.
/// `plan` is the owner's plan (`"pro"` unlocks `public`).
pub async fn set_visibility(
    pool: &SqlitePool,
    app_id: &str,
    visibility: Visibility,
    plan: &str,
) -> Result<(), VisibilityError> {
    if visibility == Visibility::Public && plan != "pro" {
        return Err(VisibilityError::ProOnly);
    }
    sqlx::query("UPDATE app SET visibility = ?1 WHERE id = ?2")
        .bind(visibility.as_str())
        .bind(app_id)
        .execute(pool)
        .await
        .map_err(|err| VisibilityError::Internal(err.into()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc() -> Allocation {
        Allocation {
            uid_range: (210000, 210003),
            port_range: (18000, 18003),
        }
    }

    async fn setup() -> (tempfile::TempDir, SqlitePool, String) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let owner = ensure_account(&pool, "ana@example.com").await.unwrap();
        (dir, pool, owner)
    }

    #[tokio::test]
    async fn ensure_account_lowercases_and_refreshes_activity() {
        let (_dir, pool, first) = setup().await;
        let second = ensure_account(&pool, "  ANA@example.com ").await.unwrap();
        assert_eq!(first, second, "same email lowercases to the same account");
        let (email, last_active_at): (String, i64) =
            sqlx::query_as("SELECT email, lastActiveAt FROM user WHERE id = ?1")
                .bind(&first)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(email, "ana@example.com");
        assert!(last_active_at > 0, "last_active_at is set to now");
    }

    #[tokio::test]
    async fn create_round_trips_through_get_and_list() {
        let (_dir, pool, owner) = setup().await;
        let app = create(&pool, &owner, "grocery", Some("Groceries"), &alloc(), 10)
            .await
            .unwrap();
        assert_eq!(app.slug, "grocery");
        assert_eq!(app.owner_id, owner);
        assert_eq!(app.name.as_deref(), Some("Groceries"));
        assert_eq!(app.visibility, Visibility::Unlisted);
        assert_eq!(
            (app.uid, app.port, app.internal_port),
            (210000, 18000, 18001)
        );
        let got = get_by_slug(&pool, "grocery").await.unwrap().unwrap();
        assert_eq!((got.id, got.created_at), (app.id, app.created_at));
        assert!(get_by_slug(&pool, "missing").await.unwrap().is_none());
        let apps = list(&pool).await.unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].slug, "grocery");
    }

    #[tokio::test]
    async fn create_refuses_bad_slugs_taken_and_quarantine() {
        let (_dir, pool, owner) = setup().await;
        assert!(matches!(
            create(&pool, &owner, "Bad Slug!", None, &alloc(), 10).await,
            Err(CreateError::InvalidSlug(_))
        ));
        assert!(matches!(
            create(&pool, &owner, "api", None, &alloc(), 10).await,
            Err(CreateError::InvalidSlug(SlugError::Reserved(_)))
        ));
        let app = create(&pool, &owner, "gone", None, &alloc(), 10)
            .await
            .unwrap();
        assert!(matches!(
            create(&pool, &owner, "gone", None, &alloc(), 10).await,
            Err(CreateError::Taken)
        ));
        delete_row(&pool, &app.id).await.unwrap();
        assert!(get_by_slug(&pool, "gone").await.unwrap().is_none());
        assert!(
            matches!(
                create(&pool, &owner, "gone", None, &alloc(), 10).await,
                Err(CreateError::Taken)
            ),
            "a deleted slug stays quarantined"
        );
    }

    #[tokio::test]
    async fn allocation_reuses_the_lowest_free_uid_and_ports() {
        let (_dir, pool, owner) = setup().await;
        let a = create(&pool, &owner, "aaa", None, &alloc(), 10)
            .await
            .unwrap();
        let b = create(&pool, &owner, "bbb", None, &alloc(), 10)
            .await
            .unwrap();
        assert_eq!((b.uid, b.port, b.internal_port), (210001, 18002, 18003));
        delete_row(&pool, &a.id).await.unwrap();
        let c = create(&pool, &owner, "ccc", None, &alloc(), 10)
            .await
            .unwrap();
        assert_eq!(
            (c.uid, c.port, c.internal_port),
            (210000, 18000, 18001),
            "delete frees the lowest uid and both ports"
        );
    }

    #[tokio::test]
    async fn allocation_exhaustion_names_the_resource() {
        let (_dir, pool, owner) = setup().await;
        let tiny_uid = Allocation {
            uid_range: (500000, 500000),
            port_range: (18000, 18100),
        };
        create(&pool, &owner, "first", None, &tiny_uid, 10)
            .await
            .unwrap();
        assert!(matches!(
            create(&pool, &owner, "second", None, &tiny_uid, 10).await,
            Err(CreateError::Exhausted("uids"))
        ));
        let tiny_ports = Allocation {
            uid_range: (500000, 500100),
            port_range: (19000, 19001),
        };
        create(&pool, &owner, "third", None, &tiny_ports, 10)
            .await
            .unwrap();
        assert!(
            matches!(
                create(&pool, &owner, "fourth", None, &tiny_ports, 10).await,
                Err(CreateError::Exhausted("ports"))
            ),
            "one app takes both ports, so a two-port range fits exactly one"
        );
    }

    #[tokio::test]
    async fn concurrent_creates_of_one_slug_have_exactly_one_winner() {
        let (_dir, pool, owner) = setup().await;
        let mut tasks = Vec::new();
        for i in 0..8 {
            let (pool, owner, alloc) = (pool.clone(), owner.clone(), alloc());
            let name = format!("racer-{i}");
            tasks.push(tokio::spawn(async move {
                create(&pool, &owner, "race", Some(&name), &alloc, 10).await
            }));
        }
        let mut wins = 0;
        for task in tasks {
            if task.await.unwrap().is_ok() {
                wins += 1;
            }
        }
        assert_eq!(wins, 1, "exactly one concurrent create wins the slug");
    }

    #[tokio::test]
    async fn create_enforces_the_plan_quota_under_its_lock() {
        let (_dir, pool, owner) = setup().await;
        create(&pool, &owner, "only", None, &alloc(), 1)
            .await
            .unwrap();
        assert!(
            matches!(
                create(&pool, &owner, "second", None, &alloc(), 1).await,
                Err(CreateError::Quota { limit: 1, used: 1 })
            ),
            "the second app is refused even though nothing else changed"
        );
    }
    #[tokio::test]
    async fn touch_asleep_and_last_deploy_round_trip() {
        let (_dir, pool, owner) = setup().await;
        let app = create(&pool, &owner, "sleepy", None, &alloc(), 10)
            .await
            .unwrap();
        touch_request(&pool, &app.id, 123).await.unwrap();
        let got = get_by_slug(&pool, "sleepy").await.unwrap().unwrap();
        assert_eq!(got.last_request_at, Some(123));
        assert_eq!(got.asleep_since, None);
        set_asleep(&pool, &app.id, Some(456)).await.unwrap();
        let got = get_by_slug(&pool, "sleepy").await.unwrap().unwrap();
        assert_eq!(got.asleep_since, Some(456));
        touch_request(&pool, &app.id, 789).await.unwrap();
        let got = get_by_slug(&pool, "sleepy").await.unwrap().unwrap();
        assert_eq!((got.last_request_at, got.asleep_since), (Some(789), None));
        set_asleep(&pool, &app.id, None).await.unwrap();
        set_last_deploy(&pool, &app.id, "abc123").await.unwrap();
        let got = get_by_slug(&pool, "sleepy").await.unwrap().unwrap();
        assert_eq!(got.asleep_since, None);
        assert_eq!(got.last_deploy_sha.as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn delete_row_rolls_back_when_the_quarantine_insert_fails() {
        let (_dir, pool, owner) = setup().await;
        let app = create(&pool, &owner, "doomed", None, &alloc(), 10)
            .await
            .unwrap();
        // A conflicting quarantine row makes the quarantine INSERT fail.
        sqlx::query("INSERT INTO slug_quarantine (slug, until) VALUES (?1, ?2)")
            .bind("doomed")
            .bind(now_ms() + QUARANTINE_MS)
            .execute(&pool)
            .await
            .unwrap();
        assert!(delete_row(&pool, &app.id).await.is_err());
        assert!(
            get_by_slug(&pool, "doomed").await.unwrap().is_some(),
            "a failed quarantine must leave the app row (and the slug) claimable by nobody"
        );
    }

    #[tokio::test]
    async fn prune_quarantine_deletes_only_releasable_slugs() {
        let (_dir, pool, _owner) = setup().await;
        for (slug, until) in [("old-quar", 99), ("edge-quar", 100), ("live-quar", 200)] {
            sqlx::query("INSERT INTO slug_quarantine (slug, until) VALUES (?1, ?2)")
                .bind(slug)
                .bind(until)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(prune_quarantine(&pool, 100).await.unwrap(), 2);
        let kept: Vec<String> = sqlx::query_scalar("SELECT slug FROM slug_quarantine")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(kept, vec!["live-quar"]);
    }
}
