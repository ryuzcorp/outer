//! Preview deploys (Pro): try a change at a temporary URL before it goes live.
//!
//! A preview is one extra git ref (`refs/heads/preview`) plus a short-lived
//! fleet served at `<slug>--preview.<apps_domain>`. The `--preview` suffix
//! is a single DNS label, so the existing wildcard cert covers it; a
//! sub-label (`preview.<slug>...`) would need a second cert level and is
//! refused by design. Previews share no production data: the preview fleet's
//! bucket prefix (`fleets/{slug}--preview/`) is fresh and empty, D1
//! migrations run into it from scratch, and the same pre-live moderation
//! gate that guards main deploys guards preview deploys.
//!
//! State is four columns on the app row (set or cleared together): the
//! preview commit, when it was last touched, and the preview fleet's two
//! loopback ports. One preview per app; a host-wide cap ([`MAX_PREVIEWS`]
//! default, `OUTER_PREVIEW_MAX` in config) bounds total preview fleets, and
//! previews idle longer than [`PREVIEW_IDLE_TTL`] are discarded by the
//! sleep sweep (same tick as the fleet sleep, so no new task is needed).

use sqlx::SqlitePool;

use crate::apps::{self, App};

/// The host suffix marking a preview: `<slug>--preview.<apps_domain>`.
/// One DNS label, so `*.outer.now` covers `<slug>--preview.outer.now`.
/// (`preview.<slug>.outer.now` would be two labels and is NOT covered.)
pub const PREVIEW_HOST_SUFFIX: &str = "--preview";

/// A preview with no request for this long is discarded by the sweep.
pub const PREVIEW_IDLE_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Default cap on apps with preview state at once (host memory budget).
pub const DEFAULT_PREVIEW_MAX: usize = 20;

/// One app's active preview, resolved from its row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub sha: String,
    pub updated_at_ms: i64,
    pub port: u16,
    pub internal_port: u16,
}

/// The app's preview, if one is active. All four columns are set or cleared
/// together; a partially-set row (only possible after a crashed write)
/// reads as no preview rather than a broken one.
pub fn preview_of(app: &App) -> Option<Preview> {
    Some(Preview {
        sha: app.preview_sha.clone()?,
        updated_at_ms: app.preview_updated_at?,
        port: app.preview_port?,
        internal_port: app.preview_internal_port?,
    })
}

/// The fleet/bucket/workdir identity for `slug`'s preview. Distinct from the
/// live slug in every namespaced surface (bucket prefix, state dir, cgroup
/// leaf, fleet map key) while sharing the app uid (same trust domain).
pub fn preview_key(slug: &str) -> String {
    format!("{slug}{PREVIEW_HOST_SUFFIX}")
}

/// Strip the preview suffix off a fleet key or host label: `Some("grocery")`
/// for `grocery--preview`, `None` otherwise. Slugs cannot end in `-`
/// ([`crate::git::manifest::validate_slug`]), so a `--preview` tail is
/// unambiguous — except a slug ending in `-preview` (`x-preview--preview`
/// strips to `x-preview`, correctly).
pub fn strip_preview_key(key: &str) -> Option<&str> {
    key.strip_suffix(PREVIEW_HOST_SUFFIX)
}

/// The public URL of `slug`'s preview.
pub fn preview_url(scheme: &str, slug: &str, apps_domain: &str, port_suffix: &str) -> String {
    format!("{scheme}://{slug}{PREVIEW_HOST_SUFFIX}.{apps_domain}{port_suffix}")
}

/// The synthetic app identity a preview fleet, deploy and proxy upstream
/// run under: the base app's id (deploy rows, secrets, moderation and D1
/// pin checks stay app-scoped) with the preview key, ports and visibility.
pub fn preview_app(app: &App) -> Option<App> {
    let preview = preview_of(app)?;
    Some(App {
        id: app.id.clone(),
        slug: preview_key(&app.slug),
        owner_id: app.owner_id.clone(),
        name: app.name.clone(),
        uid: app.uid,
        port: preview.port,
        internal_port: preview.internal_port,
        visibility: app.visibility,
        asleep_since: None,
        last_request_at: Some(preview.updated_at_ms),
        last_deploy_sha: None,
        created_at: app.created_at,
        always_on: false,
        preview_sha: None,
        preview_updated_at: None,
        preview_port: None,
        preview_internal_port: None,
    })
}

/// Record a new (or refreshed) preview on the app row: the commit plus
/// freshly claimed fleet ports, with the touch timestamp. The caller holds
/// the app's deploy lock, so two racing previews serialize here.
pub async fn set_preview(
    pool: &SqlitePool,
    app_id: &str,
    sha: &str,
    now_ms: i64,
    port: u16,
    internal_port: u16,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE app SET preview_sha = ?1, preview_updated_at = ?2, \
         preview_port = ?3, preview_internal_port = ?4 WHERE id = ?5",
    )
    .bind(sha)
    .bind(now_ms)
    .bind(port as i64)
    .bind(internal_port as i64)
    .bind(app_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A preview request counts as activity: the idle TTL measures from the
/// last request, not the last commit.
pub async fn touch_preview(pool: &SqlitePool, app_id: &str, now_ms: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE app SET preview_updated_at = ?1 WHERE id = ?2 AND preview_sha IS NOT NULL")
        .bind(now_ms)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Clear the preview columns. The fleet, bundles and manifest entry are the
/// caller's to clean (see [`discard_state`]).
pub async fn clear_preview_row(pool: &SqlitePool, app_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE app SET preview_sha = NULL, preview_updated_at = NULL, \
         preview_port = NULL, preview_internal_port = NULL WHERE id = ?1",
    )
    .bind(app_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// How many apps hold preview state right now (the host-cap numerator).
pub async fn count_previews(pool: &SqlitePool) -> anyhow::Result<u64> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app WHERE preview_sha IS NOT NULL")
        .fetch_optional(pool)
        .await?
        .unwrap_or(0);
    Ok(count.max(0) as u64)
}

/// The oldest preview touch on the host, for the host-cap refusal's
/// `retry_after_s`: the first moment a slot may free up (oldest expiry).
pub async fn oldest_preview_touch(pool: &SqlitePool) -> anyhow::Result<Option<i64>> {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(preview_updated_at) FROM app WHERE preview_sha IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)
}

/// Apps whose preview outlived the idle TTL: `(app_id, slug)` in no
/// particular order. The sweep discards each (fleet stop, row clear,
/// bundle + manifest cleanup) and moves on; one app's failure warns.
pub async fn expired_previews(
    pool: &SqlitePool,
    now_ms: i64,
) -> anyhow::Result<Vec<(String, String)>> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT id, slug FROM app WHERE preview_sha IS NOT NULL AND preview_updated_at < ?1",
    )
    .bind(now_ms - PREVIEW_IDLE_TTL_MS)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)
}

/// Preview D1 migrations check their edited-after-apply pins against the
/// live pins, but are never recorded (see `Deployer::migrate`'s `record`
/// flag): the preview database is disposable (fresh prefix per preview
/// lineage), celld's own `d1_migrations` journal in the preview database
/// makes repeat applies idempotent, and the live gate still refuses an
/// edited migration on the next live deploy.
///
/// Whether `host` (a bare host, port already stripped, lowercased) is a
/// preview host, and if so the base slug: `grocery--preview` → `grocery`.
pub fn preview_slug_for(host: &str, apps_domain: &str) -> Option<String> {
    let domain = apps_domain.trim().trim_matches('.').to_lowercase();
    let host = host.trim().trim_matches('.').to_lowercase();
    let label = host.strip_suffix(&domain)?.strip_suffix('.')?;
    if label.is_empty() || label.contains('.') {
        return None;
    }
    strip_preview_key(label).map(str::to_string)
}

/// Drop a preview completely: stop the fleet (no-op when asleep), clear the
/// row, remove the preview ref from the manifest, and delete the preview
/// bundles. Stops before clearing so the fleet key still resolves while it
/// is being stopped. Idempotent: discarding twice is a quiet `false`.
/// Shared by the discard tool and the expiry sweep.
pub async fn discard_state(
    pool: &SqlitePool,
    store: &crate::store::Store,
    git: &crate::git::GitConfig,
    sync: &crate::git::GitSync,
    fleets: &crate::fleet::manager::FleetManager,
    app_id: &str,
    slug: &str,
) -> anyhow::Result<bool> {
    let app = crate::apps::get_by_id(pool, app_id).await?;
    let Some(app) = app else {
        return Ok(false);
    };
    if app.slug != slug || preview_of(&app).is_none() {
        return Ok(false);
    }
    fleets.stop_preview(slug).await;
    clear_preview_row(pool, app_id).await?;
    crate::git::manifest::clear_preview(store, git, sync, slug).await?;
    crate::git::manifest::delete_preview_bundles(store, slug).await;
    Ok(true)
}

/// Discard every preview idle past [`PREVIEW_IDLE_TTL_MS`]: the fleet stops,
/// the row clears, and the mirror ref plus bundles go away, so the next
/// preview starts from a clean slate. Returns the discarded slugs. One
/// app's failure warns and the rest still expire (the sweep retries the
/// failed one next tick).
pub async fn expire_due(
    pool: &SqlitePool,
    store: &crate::store::Store,
    git: &crate::git::GitConfig,
    sync: &crate::git::GitSync,
    fleets: &crate::fleet::manager::FleetManager,
) -> anyhow::Result<Vec<String>> {
    let due = expired_previews(pool, apps::now_ms()).await?;
    let mut expired = Vec::new();
    for (app_id, slug) in due {
        match discard_state(pool, store, git, sync, fleets, &app_id, &slug).await {
            Ok(true) => expired.push(slug),
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(slug = %slug, error = %format!("{err:#}"), "preview expiry failed");
            }
        }
    }
    Ok(expired)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_preview() -> App {
        App {
            id: "app1".to_string(),
            slug: "grocery".to_string(),
            owner_id: "owner1".to_string(),
            name: None,
            uid: 10001,
            port: 18001,
            internal_port: 18002,
            visibility: apps::Visibility::Unlisted,
            asleep_since: None,
            last_request_at: Some(1000),
            last_deploy_sha: Some("a".repeat(40)),
            created_at: 500,
            always_on: false,
            preview_sha: Some("b".repeat(40)),
            preview_updated_at: Some(2000),
            preview_port: Some(18003),
            preview_internal_port: Some(18004),
        }
    }

    #[test]
    fn host_shapes_are_single_label() {
        assert_eq!(preview_key("grocery"), "grocery--preview");
        assert_eq!(strip_preview_key("grocery--preview"), Some("grocery"));
        assert_eq!(strip_preview_key("x-preview--preview"), Some("x-preview"));
        assert_eq!(strip_preview_key("grocery"), None);
        assert_eq!(strip_preview_key("grocery--other"), None);
        assert_eq!(
            preview_url("https", "grocery", "outer.now", ""),
            "https://grocery--preview.outer.now"
        );
        assert_eq!(
            preview_slug_for("grocery--preview.outer.now", "outer.now"),
            Some("grocery".to_string())
        );
        assert_eq!(preview_slug_for("grocery.outer.now", "outer.now"), None);
        assert_eq!(
            preview_slug_for("preview.grocery.outer.now", "outer.now"),
            None,
            "a sub-label is not a preview host"
        );
    }

    #[test]
    fn synthetic_app_shares_id_but_not_ports() {
        let app = app_with_preview();
        let fleet = preview_app(&app).unwrap();
        assert_eq!(fleet.slug, "grocery--preview");
        assert_eq!(fleet.id, app.id);
        assert_eq!(fleet.uid, app.uid);
        assert_eq!(fleet.port, 18003);
        assert_eq!(fleet.internal_port, 18004);
        assert_eq!(fleet.last_request_at, Some(2000));
        let mut partial = app.clone();
        partial.preview_port = None;
        assert!(preview_of(&partial).is_none());
        let mut cleared = app.clone();
        cleared.preview_sha = None;
        cleared.preview_updated_at = None;
        cleared.preview_port = None;
        cleared.preview_internal_port = None;
        assert!(preview_of(&cleared).is_none());
        assert!(preview_app(&cleared).is_none());
    }

    async fn pool() -> SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("test.sqlite"))
            .await
            .unwrap();
        std::mem::forget(dir);
        pool
    }

    async fn seed_app(pool: &SqlitePool) -> String {
        let owner = apps::ensure_account(pool, "ana@example.com").await.unwrap();
        let alloc = apps::Allocation {
            uid_range: (210000, 210003),
            port_range: (18000, 18009),
        };
        let app = apps::create(pool, &owner, "grocery", None, &alloc, 10)
            .await
            .unwrap();
        app.id
    }

    #[tokio::test]
    async fn preview_row_lifecycle() {
        let pool = pool().await;
        let app_id = seed_app(&pool).await;
        assert_eq!(count_previews(&pool).await.unwrap(), 0);
        assert!(oldest_preview_touch(&pool).await.unwrap().is_none());

        set_preview(&pool, &app_id, &"b".repeat(40), 1000, 18005, 18006)
            .await
            .unwrap();
        assert_eq!(count_previews(&pool).await.unwrap(), 1);
        assert_eq!(oldest_preview_touch(&pool).await.unwrap(), Some(1000));
        let app = apps::get_by_id(&pool, &app_id).await.unwrap().unwrap();
        let preview = preview_of(&app).unwrap();
        assert_eq!(preview.port, 18005);

        touch_preview(&pool, &app_id, 2000).await.unwrap();
        let app = apps::get_by_id(&pool, &app_id).await.unwrap().unwrap();
        assert_eq!(preview_of(&app).unwrap().updated_at_ms, 2000);
        assert!(expired_previews(&pool, 2000 + PREVIEW_IDLE_TTL_MS - 1)
            .await
            .unwrap()
            .is_empty());
        let due = expired_previews(&pool, 2000 + PREVIEW_IDLE_TTL_MS + 1)
            .await
            .unwrap();
        assert_eq!(due, vec![(app_id.clone(), "grocery".to_string())]);

        clear_preview_row(&pool, &app_id).await.unwrap();
        assert_eq!(count_previews(&pool).await.unwrap(), 0);
        let app = apps::get_by_id(&pool, &app_id).await.unwrap().unwrap();
        assert!(preview_of(&app).is_none());
    }

    #[tokio::test]
    async fn discard_and_expiry_clear_state() {
        let pool = pool().await;
        let app_id = seed_app(&pool).await;
        let dir = tempfile::tempdir().unwrap();
        let git = crate::git::GitConfig::new(dir.path().join("git"), None);
        let sync = crate::git::GitSync::default();
        let store = crate::store::Store::memory();
        let fleets =
            crate::fleet::manager::FleetManager::new(crate::fleet::manager::ManagerSettings {
                data_dir: dir.path().join("data"),
                celld_bin: "celld".to_string(),
                storage: crate::fleet::manager::FleetStorage {
                    endpoint: "http://127.0.0.1:1".to_string(),
                    region: "test".to_string(),
                    bucket: "b".to_string(),
                    credentials: crate::store::credentials::CredentialSource::Static {
                        access_key_id: "k".to_string(),
                        secret_access_key: "s".to_string(),
                    },
                },
                cgroups: None,
                memory_max_bytes: 0,
                cpu_max: "1".to_string(),
                sleep_after: std::time::Duration::from_secs(60),
                credential_refresh_after: std::time::Duration::from_secs(60),
                wake_timeout: std::time::Duration::from_secs(1),
                wake_concurrency: 1,
                always_on_max: 10,
                preview_max: 20,
            });
        // No manifest and no fleet: discard still clears the row and
        // answers true once, then false.
        set_preview(&pool, &app_id, &"b".repeat(40), 1000, 18005, 18006)
            .await
            .unwrap();
        assert!(
            discard_state(&pool, &store, &git, &sync, &fleets, &app_id, "grocery")
                .await
                .unwrap()
        );
        assert!(preview_of(&apps::get_by_id(&pool, &app_id).await.unwrap().unwrap()).is_none());
        assert!(
            !discard_state(&pool, &store, &git, &sync, &fleets, &app_id, "grocery")
                .await
                .unwrap()
        );

        // A stale preview expires at once; a fresh one survives the sweep.
        set_preview(&pool, &app_id, &"b".repeat(40), 1000, 18005, 18006)
            .await
            .unwrap();
        assert_eq!(
            expire_due(&pool, &store, &git, &sync, &fleets)
                .await
                .unwrap(),
            vec!["grocery".to_string()]
        );
        assert!(preview_of(&apps::get_by_id(&pool, &app_id).await.unwrap().unwrap()).is_none());
        set_preview(
            &pool,
            &app_id,
            &"b".repeat(40),
            apps::now_ms(),
            18005,
            18006,
        )
        .await
        .unwrap();
        assert!(expire_due(&pool, &store, &git, &sync, &fleets)
            .await
            .unwrap()
            .is_empty());
        sqlx::query("UPDATE app SET preview_updated_at = ?1 WHERE id = ?2")
            .bind(1000)
            .bind(&app_id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            expire_due(&pool, &store, &git, &sync, &fleets)
                .await
                .unwrap(),
            vec!["grocery".to_string()]
        );
        assert!(preview_of(&apps::get_by_id(&pool, &app_id).await.unwrap().unwrap()).is_none());
    }
}
