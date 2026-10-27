//! Console admin app list (spec §15.3): every app on Outer, for admins.
//!
//! `GET /api/console/admin/apps` is the Admin page's directory: slug, name,
//! owner email, plan, visibility, moderation state, the latest deploy and
//! its status, and `created_at`, newest first, with search (`q` over slug
//! and owner email), filters (`state`, `plan`) and keyset pagination
//! (`limit`, `before`). Server-side and admin-only: non-admins get the same
//! 404 as an unknown slug, so the endpoint does not reveal that it exists.
//!
//! From here the admin opens any app in the normal Console app views (see
//! [`super::console::authorized_app`]): the same handlers and code paths,
//! owner-equivalent through the single authorization point. The directory
//! read itself writes one throttled `account_action` row per admin
//! (`admin_list_apps`, at most one per minute — the list is the
//! bulk-enumeration surface, so every page is traceable without flooding
//! the log on fast paging); opening an app audits the access, and every
//! mutation audits itself.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use super::console::{action_cursor, guard, nostore_json, not_found, ok_json, parse_action_cursor};
use super::AppState;
use crate::auth::AccountRow;

/// The admin app-directory routes, merged by [`super::console::router`].
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/console/admin/apps", get(api_list))
}

/// Cap on list calls per admin per minute: the directory is enumerable, so
/// it stays cheap to read but expensive to scrape.
const LIST_LIMIT: u32 = 60;
const LIST_WINDOW_MS: i64 = 60_000;

/// `GET /api/console/admin/apps?q=&state=&plan=&limit=&before=`: every app
/// on Outer, newest first. `q` matches slug or owner email (substring, at
/// most 128 chars); `state` is one of `ok|held|suspended`; `plan` is
/// `free|pro`; `limit` defaults to 50 and clamps to 1–100; `before` is the
/// opaque `<created_at>:<id>` cursor of the last row on the previous page.
#[derive(Debug, Deserialize)]
struct ListQuery {
    q: Option<String>,
    state: Option<String>,
    plan: Option<String>,
    limit: Option<u64>,
    before: Option<String>,
}

async fn api_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let mcp = &state.mcp;
    let account: AccountRow = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    // 404, not 403: the directory must not reveal that it exists.
    if !crate::auth::is_admin(&mcp.auth.config, &account.email) {
        return not_found();
    }
    if let Err(res) = check_list_rate_limit(&mcp.pool, &account.email).await {
        return *res;
    }
    let q = query.q.unwrap_or_default();
    if q.chars().count() > 128 {
        return nostore_json(
            StatusCode::BAD_REQUEST,
            "bad_query",
            "q is at most 128 characters",
        );
    }
    if let Some(state) = query.state.as_deref() {
        if !matches!(state, "ok" | "held" | "suspended") {
            return nostore_json(
                StatusCode::BAD_REQUEST,
                "bad_state",
                "state must be one of ok, held, suspended",
            );
        }
    }
    if let Some(plan) = query.plan.as_deref() {
        if !matches!(plan, "free" | "pro") {
            return nostore_json(
                StatusCode::BAD_REQUEST,
                "bad_plan",
                "plan must be one of free, pro",
            );
        }
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let before = match query.before.as_deref() {
        None => None,
        Some(raw) => match parse_action_cursor(raw) {
            Some(cursor) => Some(cursor),
            None => {
                return nostore_json(
                    StatusCode::BAD_REQUEST,
                    "bad_cursor",
                    "before must be the next cursor from the previous page",
                );
            }
        },
    };
    let rows = match list_apps(
        &mcp.pool,
        &q,
        query.state.as_deref(),
        query.plan.as_deref(),
        before,
        limit,
    )
    .await
    {
        Ok(rows) => rows,
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the apps right now",
            );
        }
    };
    // One extra row tells a full page from the last one.
    let (page, next) = match rows.len() as u64 > limit {
        true => (&rows[..limit as usize], {
            let last = &rows[limit as usize - 1];
            Some(action_cursor(last.created_at, &last.app_id))
        }),
        false => (&rows[..], None),
    };
    audit_list(
        &mcp.pool,
        &account,
        &q,
        query.state.as_deref(),
        query.plan.as_deref(),
    )
    .await;
    ok_json(serde_json::json!({
        "apps": page.iter().map(|row| {
            let mut v = serde_json::json!({
                "slug": row.slug,
                "name": row.name,
                "owner_email": row.owner_email,
                "plan": row.plan,
                "visibility": row.visibility,
                "moderation_state": row.moderation_state,
                "created_at": row.created_at,
            });
            v["last_deploy"] = match (&row.deploy_id, &row.deploy_sha, &row.deploy_status, row.deploy_created_at) {
                (Some(id), Some(sha), Some(status), Some(created_at)) => serde_json::json!({
                    "id": id,
                    "sha": sha,
                    "status": status,
                    "created_at": created_at,
                }),
                _ => serde_json::Value::Null,
            };
            v
        }).collect::<Vec<_>>(),
        "next": next,
    }))
}

/// One throttled `account_action` row per admin directory read (tool
/// `admin_list_apps`, under the admin's own account): the list is the
/// bulk-enumeration surface, so every page is traceable. At most one row
/// per minute per admin — fast paging must not flood the log, and the
/// per-minute list rate limit bounds the read itself. Best-effort like any
/// read audit: a lost row warns, the directory still answers.
async fn audit_list(
    pool: &sqlx::SqlitePool,
    account: &AccountRow,
    q: &str,
    state: Option<&str>,
    plan: Option<&str>,
) {
    let now = crate::auth::now_ms();
    let recent: Option<(i64,)> = sqlx::query_as(
        "SELECT created_at FROM account_action \
         WHERE user_id = ?1 AND tool = 'admin_list_apps' AND created_at > ?2 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&account.id)
    .bind(now - LIST_WINDOW_MS)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    if recent.is_some() {
        return;
    }
    crate::accounts::record_account_action(
        pool,
        &account.id,
        "admin_list_apps",
        &format!(
            "admin_list_apps q={q:?} state={} plan={}",
            state.unwrap_or("-"),
            plan.unwrap_or("-"),
        ),
        "ok",
        None,
        now,
    )
    .await;
}

/// One directory row: the app, its owner's email and plan, and the latest
/// `main` deploy, if the app ever deployed.
struct ListRow {
    app_id: String,
    slug: String,
    name: Option<String>,
    owner_email: String,
    plan: String,
    visibility: String,
    moderation_state: String,
    created_at: i64,
    deploy_id: Option<String>,
    deploy_sha: Option<String>,
    deploy_status: Option<String>,
    deploy_created_at: Option<i64>,
}

#[allow(clippy::too_many_arguments)]
async fn list_apps(
    pool: &sqlx::SqlitePool,
    q: &str,
    state: Option<&str>,
    plan: Option<&str>,
    before: Option<(i64, String)>,
    limit: u64,
) -> anyhow::Result<Vec<ListRow>> {
    type Row = (
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    );
    // One static query: empty filters match everything, so the shape never
    // changes with the query string. The latest deploy per app is the row no
    // newer row supersedes (same pattern as the moderation queue).
    let like = format!("%{}%", escape_like(q.trim()));
    let state_filter = state.unwrap_or("").to_string();
    let plan_filter = plan.unwrap_or("").to_string();
    let (before_at, before_id) = before.unwrap_or((i64::MAX, "\u{10FFFF}".to_string()));
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT a.id, a.slug, a.name, u.email, COALESCE(u.plan, 'free'), \
                a.visibility, a.moderation_state, a.created_at, \
                d.id, d.sha, d.status, d.created_at \
         FROM app a JOIN user u ON u.id = a.owner_id \
         LEFT JOIN deploy d ON d.app_id = a.id AND d.kind = 'main' \
           AND NOT EXISTS (SELECT 1 FROM deploy newer \
                           WHERE newer.app_id = d.app_id \
                             AND (newer.created_at, newer.rowid) > (d.created_at, d.rowid)) \
         WHERE (a.slug LIKE ?1 ESCAPE '\\' OR u.email LIKE ?1 ESCAPE '\\') \
           AND (?2 = '' OR a.moderation_state = ?2) \
           AND (?3 = '' OR COALESCE(u.plan, 'free') = ?3) \
           AND (a.created_at, a.id) < (?4, ?5) \
         ORDER BY a.created_at DESC, a.id DESC LIMIT ?6",
    )
    .bind(&like)
    .bind(&state_filter)
    .bind(&plan_filter)
    .bind(before_at)
    .bind(&before_id)
    .bind((limit + 1) as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                app_id,
                slug,
                name,
                owner_email,
                plan,
                visibility,
                moderation_state,
                created_at,
                deploy_id,
                deploy_sha,
                deploy_status,
                deploy_created_at,
            )| {
                ListRow {
                    app_id,
                    slug,
                    name,
                    owner_email,
                    plan: plan.unwrap_or_else(|| "free".to_string()),
                    visibility,
                    moderation_state,
                    created_at,
                    deploy_id,
                    deploy_sha,
                    deploy_status,
                    deploy_created_at,
                }
            },
        )
        .collect())
}

/// Escape the `LIKE` wildcards in a caller-supplied substring.
fn escape_like(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn check_list_rate_limit(
    pool: &sqlx::SqlitePool,
    admin_email: &str,
) -> Result<(), Box<Response>> {
    let now = crate::auth::now_ms();
    match crate::auth::limits::check_and_record(
        pool,
        &format!("admin-list:{admin_email}"),
        "console_admin_list",
        LIST_LIMIT,
        LIST_WINDOW_MS,
        now,
    )
    .await
    {
        Ok(None) => Ok(()),
        Ok(Some(retry_after_s)) => Err(nostore_json(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            &format!("slow down and try again in {retry_after_s}s"),
        )
        .into()),
        Err(_) => Err(nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "could not check the rate limit right now",
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::{test_config, test_pool};
    use crate::auth::{create_session, session_hash, AuthState};
    use crate::mcp::McpState;
    use crate::store::snapshot::StateSync;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use tower::ServiceExt;

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String) {
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    fn same_origin(req: axum::http::request::Builder) -> axum::http::request::Builder {
        req.header("sec-fetch-site", "same-origin")
    }

    fn authed(path: &str, cookie: &str) -> Request<Body> {
        same_origin(Request::get(path))
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    }

    /// Two owners (one Pro, one Free with a held app), one admin, one
    /// stranger. Sessions are fresh, so the admin passes step-up.
    async fn harness() -> (Router, sqlx::SqlitePool, String, String, String) {
        let (config, _codes) = test_config();
        let pool = test_pool().await;
        let auth = AuthState {
            pool: pool.clone(),
            config: config.clone(),
        };
        let mcp = McpState::for_tests(pool.clone(), auth.clone());
        let missing = std::path::Path::new("/nonexistent-site");
        let app = super::super::router(super::super::AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
            auth,
            mcp: mcp.clone(),
            site: crate::http::site::SiteDir::new(missing),
        });
        let now = crate::auth::now_ms();
        for (id, email, plan) in [
            ("acct-ana@example.com", "ana@example.com", "free"),
            ("acct-pro@example.com", "pro@example.com", "pro"),
            ("acct-owner@example.com", "owner@example.com", "free"),
            ("acct-bruno@example.com", "bruno@example.com", "free"),
        ] {
            sqlx::query(
                "INSERT INTO user (id, email, plan, lastActiveAt, createdAt, updatedAt) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(email)
            .bind(plan)
            .bind(now)
            .bind(now)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        }
        for (i, (id, slug, owner, created, moderation)) in [
            (
                "app-groceries",
                "groceries",
                "acct-ana@example.com",
                now - 3000,
                "ok",
            ),
            (
                "app-shop",
                "shop",
                "acct-pro@example.com",
                now - 2000,
                "held",
            ),
            (
                "app-diary",
                "diary",
                "acct-ana@example.com",
                now - 1000,
                "ok",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let i = i as i64;
            sqlx::query(
                "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
                                 moderation_state, last_activity_at, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(slug)
            .bind(owner)
            .bind(300001 + i)
            .bind(18001 + (i * 2))
            .bind(18002 + (i * 2))
            .bind(moderation)
            .bind(created)
            .bind(created)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO deploy (id, app_id, sha, status, kind, created_at, finished_at) \
                 VALUES (?, ?, 'abc123', 'success', 'main', ?, ?)",
            )
            .bind(format!("dep-{slug}"))
            .bind(id)
            .bind(created + 10)
            .bind(created + 20)
            .execute(&pool)
            .await
            .unwrap();
        }
        let admin = create_session(&pool, "acct-owner@example.com")
            .await
            .unwrap();
        let stranger = create_session(&pool, "acct-bruno@example.com")
            .await
            .unwrap();
        let ana = create_session(&pool, "acct-ana@example.com").await.unwrap();
        (
            app,
            pool,
            format!("__Host-outer_sid={admin}"),
            format!("__Host-outer_sid={stranger}"),
            format!("__Host-outer_sid={ana}"),
        )
    }

    fn apps_of(body: &str) -> Vec<serde_json::Value> {
        body.parse::<serde_json::Value>().unwrap()["apps"]
            .as_array()
            .unwrap()
            .clone()
    }

    #[tokio::test]
    async fn admin_list_lists_every_app_newest_first() {
        let (app, _, admin, _, _) = harness().await;
        let (status, body) = send(&app, authed("/api/console/admin/apps", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        let apps = apps_of(&body);
        assert_eq!(apps.len(), 3, "every app, whatever the owner");
        assert_eq!(apps[0]["slug"], "diary");
        assert_eq!(apps[0]["owner_email"], "ana@example.com");
        assert_eq!(apps[0]["plan"], "free");
        assert_eq!(apps[0]["visibility"], "unlisted");
        assert_eq!(apps[0]["moderation_state"], "ok");
        assert_eq!(apps[0]["last_deploy"]["status"], "success");
        assert_eq!(apps[0]["last_deploy"]["sha"], "abc123");
        assert_eq!(apps[1]["slug"], "shop");
        assert_eq!(apps[1]["moderation_state"], "held");
        assert_eq!(apps[1]["plan"], "pro");
    }

    #[tokio::test]
    async fn admin_list_search_filter_and_pagination() {
        let (app, _, admin, _, _) = harness().await;
        let get = async |path: &str| {
            let (status, body) = send(
                &app,
                authed(&format!("/api/console/admin/apps{path}"), &admin),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{path}");
            body
        };
        let slugs = |body: &str| {
            apps_of(body)
                .iter()
                .map(|a| a["slug"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(slugs(&get("?q=gro").await), vec!["groceries"]);
        assert_eq!(
            slugs(&get("?q=pro@example.com").await),
            vec!["shop"],
            "search matches owner email"
        );
        assert_eq!(slugs(&get("?state=held").await), vec!["shop"]);
        assert_eq!(slugs(&get("?plan=pro").await), vec!["shop"]);
        assert_eq!(
            slugs(&get("?plan=free&state=ok").await)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["diary", "groceries"]
        );
        // Pagination: two pages of two, then the cursor ends.
        let first = get("?limit=2").await;
        assert_eq!(slugs(&first), vec!["diary", "shop"]);
        let next: String = first.parse::<serde_json::Value>().unwrap()["next"]
            .as_str()
            .unwrap()
            .to_string();
        let second = get(&format!("?limit=2&before={next}")).await;
        assert_eq!(slugs(&second), vec!["groceries"]);
        assert!(
            second.parse::<serde_json::Value>().unwrap()["next"].is_null(),
            "last page has no cursor"
        );
        // Unknown filter values refuse loudly.
        for path in ["?state=banned", "?plan=team", "?before=nope"] {
            let (status, _) = send(
                &app,
                authed(&format!("/api/console/admin/apps{path}"), &admin),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        }
    }

    #[tokio::test]
    async fn admin_list_stays_hidden_from_non_admins() {
        let (app, _, _admin, stranger, ana) = harness().await;
        // Strangers and plain owners read 404, not 403: the endpoint must
        // not reveal that it exists.
        for cookie in [&stranger, &ana] {
            let (status, body) = send(&app, authed("/api/console/admin/apps", cookie)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{cookie}");
            assert_eq!(
                body.parse::<serde_json::Value>().unwrap()["error"],
                "not_found"
            );
        }
        // No session at all is 401.
        let (status, _) = send(
            &app,
            same_origin(Request::get("/api/console/admin/apps"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_list_is_rate_limited() {
        let (app, _, admin, _, _) = harness().await;
        for _ in 0..LIST_LIMIT {
            let (status, _) = send(&app, authed("/api/console/admin/apps", &admin)).await;
            assert_eq!(status, StatusCode::OK);
        }
        let (status, body) = send(&app, authed("/api/console/admin/apps", &admin)).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["error"],
            "rate_limited"
        );
    }

    #[tokio::test]
    async fn stale_admin_session_cannot_act_on_foreign_apps() {
        let (app, pool, admin, _, _) = harness().await;
        // Fresh sessions act: the admin opens a foreign app.
        let (status, _) = send(&app, authed("/api/console/apps/groceries", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        // Backdate the session past the step-up window.
        let raw = admin.trim_start_matches("__Host-outer_sid=");
        sqlx::query("UPDATE session SET createdAt = ? WHERE token = ?")
            .bind(crate::auth::now_ms() - crate::auth::ADMIN_STEP_UP_MS - 1)
            .bind(session_hash(raw))
            .execute(&pool)
            .await
            .unwrap();
        for path in [
            "/api/console/apps/groceries",
            "/api/console/apps/groceries/secrets",
        ] {
            let (status, body) = send(&app, authed(path, &admin)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
            assert_eq!(
                body.parse::<serde_json::Value>().unwrap()["error"],
                "reverify"
            );
        }
        // Writes refuse too, through the same point.
        let (status, _) = send(
            &app,
            same_origin(Request::post("/api/console/apps/groceries/visibility"))
                .header("cookie", &admin)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"visibility":"unlisted"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_admin_cannot_reach_foreign_apps() {
        let (app, _, _, stranger, _) = harness().await;
        // Reads 404, including crafted slugs the stranger could never own.
        for path in [
            "/api/console/apps/groceries",
            "/api/console/apps/groceries/secrets",
            "/api/console/apps/groceries/actions",
            "/api/console/apps/groceries/code/tree",
            "/api/console/apps/no-such-app",
            "/api/console/apps/%2e%2e",
        ] {
            let (status, _) = send(&app, authed(path, &stranger)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
        // Owner-only writes 404 too (never 403: the slug is not theirs).
        let (status, _) = send(
            &app,
            same_origin(Request::post("/api/console/apps/groceries/visibility"))
                .header("cookie", &stranger)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"visibility":"unlisted"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn admin_reads_and_writes_audit_as_admin() {
        let (app, pool, admin, _, ana) = harness().await;
        // Read: the overview.
        let (status, _) = send(&app, authed("/api/console/apps/groceries", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        // Write: an owner-only mutation through the same handler.
        let (status, _) = send(
            &app,
            same_origin(Request::post("/api/console/apps/groceries/visibility"))
                .header("cookie", &admin)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"visibility":"unlisted"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Both rows attribute the admin, never the owner.
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT user_id, tool, summary FROM agent_action WHERE app_id = 'app-groceries' ORDER BY created_at",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(rows.len() >= 2, "read + write audited, got {rows:?}");
        for (user_id, tool, summary) in &rows {
            assert_eq!(
                user_id, "acct-owner@example.com",
                "{tool} attributed to the admin"
            );
            assert!(
                summary.starts_with("admin owner@example.com: "),
                "{tool} marked as an admin act: {summary}"
            );
        }
        assert!(rows.iter().any(|(_, tool, _)| tool == "app"));
        assert!(rows.iter().any(|(_, tool, _)| tool == "set_visibility"));
        // The owner sees the audit in the app's activity log.
        let (status, body) = send(&app, authed("/api/console/apps/groceries/actions", &ana)).await;
        assert_eq!(status, StatusCode::OK);
        let actions = body.parse::<serde_json::Value>().unwrap()["actions"]
            .as_array()
            .unwrap()
            .clone();
        assert!(
            actions.iter().any(|a| a["tool"] == "set_visibility"
                && a["summary"]
                    .as_str()
                    .unwrap()
                    .starts_with("admin owner@example.com: ")),
            "owner sees the admin write: {body}"
        );
    }

    #[tokio::test]
    async fn plan_gates_follow_the_app_owner() {
        let (app, pool, admin, _, _) = harness().await;
        // `groceries` is Free: the admin cannot make it public either.
        let (status, body) = send(
            &app,
            same_origin(Request::post("/api/console/apps/groceries/visibility"))
                .header("cookie", &admin)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"visibility":"public"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{body}");
        // Promote the owner: the same call goes through on the owner's plan.
        sqlx::query("UPDATE user SET plan = 'pro' WHERE id = 'acct-ana@example.com'")
            .execute(&pool)
            .await
            .unwrap();
        let (status, _) = send(
            &app,
            same_origin(Request::post("/api/console/apps/groceries/visibility"))
                .header("cookie", &admin)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"visibility":"public"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_secrets_stay_write_only() {
        let (app, pool, admin, _, _) = harness().await;
        sqlx::query(
            "INSERT INTO app_secret (app_id, name, value_enc, updated_at) VALUES ('app-groceries', 'API_KEY', X'00', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let (status, body) =
            send(&app, authed("/api/console/apps/groceries/secrets", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        let parsed = body.parse::<serde_json::Value>().unwrap();
        let secrets = parsed["secrets"].as_array().unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0]["name"], "API_KEY");
        assert!(
            !body.contains("00") || body.contains("updated_at"),
            "no secret value leaves the server: {body}"
        );
        assert!(!body.contains("sk-"), "no secret value leaves the server");
    }

    #[tokio::test]
    async fn admin_list_writes_one_throttled_audit_row() {
        let (app, pool, admin, _, _) = harness().await;
        // Two rapid page reads ...
        for _ in 0..2 {
            let (status, _) = send(&app, authed("/api/console/admin/apps", &admin)).await;
            assert_eq!(status, StatusCode::OK);
        }
        // ... land a single throttled row under the admin's own account.
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT user_id, tool, summary FROM account_action WHERE tool = 'admin_list_apps'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "one throttled row, got: {rows:?}");
        assert_eq!(
            rows[0].0, "acct-owner@example.com",
            "attributed to the admin"
        );
        // And the admin audit view surfaces it.
        let (status, body) = send(&app, authed("/api/console/admin/audit", &admin)).await;
        assert_eq!(status, StatusCode::OK);
        let actions = body.parse::<serde_json::Value>().unwrap()["actions"]
            .as_array()
            .unwrap()
            .clone();
        assert!(
            actions.iter().any(|a| a["tool"] == "admin_list_apps"),
            "audit view carries the list read: {body}"
        );
    }
}
