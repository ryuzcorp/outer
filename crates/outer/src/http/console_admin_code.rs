//! Console admin code inspection (spec §15.3): a read-only window into
//! any app's repo for admins, so the moderation queue's Code view works
//! even when the admin has no owner or collaborator role on the app.
//!
//! Routes mirror [`super::console_code`] one for one, under
//! `/api/console/admin/apps/:slug/code/…`, and call the same `*_for`
//! domain functions — no second implementation, same bounds and errors.
//! Every read is audited with [`crate::mcp::actions::record_console`]
//! under the matching `code_*` console action name. Signed-in non-admins
//! get the same owner-scoped 404 as unknown slugs, so the mirror routes do
//! not oracle the admin surface; slugs the caller has no role on but is
//! not admin for are unreachable here.
//!
//! There are no writes on these routes: no data panels, no secrets, no
//! deploys.

use std::collections::HashMap;

use super::console::admin_guard;
use super::console_code::{compare_for, diff_for, file_for, history_for, tree_for};
use super::AppState;
use crate::auth::AccountRow;
use crate::mcp::McpState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

/// The admin code-inspection routes, merged by [`super::console::router`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/console/admin/apps/{slug}/code/tree", get(api_tree))
        .route("/api/console/admin/apps/{slug}/code/file", get(api_file))
        .route(
            "/api/console/admin/apps/{slug}/code/history",
            get(api_history),
        )
        .route("/api/console/admin/apps/{slug}/code/diff", get(api_diff))
        .route(
            "/api/console/admin/apps/{slug}/code/compare",
            get(api_compare),
        )
}

/// The session plus the admin check, plus the app through the single
/// Console authorization point ([`super::console::authorized_app`]): the
/// admin needs a recent session (step-up) for apps they have no role on,
/// audited per read below. Unknown slugs answer 404 like the role-scoped
/// routes, and so do signed-in non-admins (via `admin_guard`), so the
/// mirror routes do not oracle the admin surface. Reads audit themselves,
/// so no read label.
async fn admin_app(
    mcp: &McpState,
    headers: &HeaderMap,
    slug: &str,
) -> Result<(AccountRow, crate::apps::App), Box<Response>> {
    let account = admin_guard(&mcp.auth, headers).await?;
    match super::console::authorized_app(mcp, &account, headers, slug, None).await {
        Ok(authz) => Ok((authz.account, authz.app)),
        Err(res) => Err(res),
    }
}

/// Audit one admin code read: the tool name matches the console action
/// the Code view runs, so the action log shows what was inspected. Takes
/// the status by value — `Response` is `Send` but not `Sync`, so holding
/// a borrow across the audit write would break the handler.
async fn audit(
    pool: &sqlx::SqlitePool,
    app: &crate::apps::App,
    account: &AccountRow,
    tool: &str,
    status: StatusCode,
    started: i64,
) {
    let outcome = if status.is_success() { "ok" } else { "error" };
    let code = if status.is_success() {
        None
    } else {
        Some("request_failed")
    };
    crate::mcp::actions::record_console(
        pool,
        &app.id,
        &account.id,
        tool,
        &super::console::audit_summary(account, &format!("admin_code {tool} {}", app.slug), true),
        outcome,
        code,
        crate::auth::now_ms() - started,
    )
    .await;
}

async fn api_tree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (account, app) = match admin_app(&state.mcp, &headers, &slug).await {
        Ok(found) => found,
        Err(res) => return *res,
    };
    let started = crate::auth::now_ms();
    let res = tree_for(
        &state.mcp,
        &app,
        query.get("sha").cloned(),
        query.get("prefix").cloned(),
    )
    .await;
    let status = res.status();
    audit(
        &state.mcp.pool,
        &app,
        &account,
        "code_list_files",
        status,
        started,
    )
    .await;
    res
}

async fn api_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (account, app) = match admin_app(&state.mcp, &headers, &slug).await {
        Ok(found) => found,
        Err(res) => return *res,
    };
    let started = crate::auth::now_ms();
    let res = file_for(
        &state.mcp,
        &app,
        query.get("path").cloned(),
        query.get("sha").cloned(),
    )
    .await;
    let status = res.status();
    audit(
        &state.mcp.pool,
        &app,
        &account,
        "code_read_file",
        status,
        started,
    )
    .await;
    res
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    path: Option<String>,
    skip: Option<String>,
}

async fn api_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let (account, app) = match admin_app(&state.mcp, &headers, &slug).await {
        Ok(found) => found,
        Err(res) => return *res,
    };
    let started = crate::auth::now_ms();
    let res = history_for(&state.mcp, &app, query.path, query.skip).await;
    let status = res.status();
    audit(
        &state.mcp.pool,
        &app,
        &account,
        "code_history",
        status,
        started,
    )
    .await;
    res
}

async fn api_diff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (account, app) = match admin_app(&state.mcp, &headers, &slug).await {
        Ok(found) => found,
        Err(res) => return *res,
    };
    let started = crate::auth::now_ms();
    let res = diff_for(&state.mcp, &app, query.get("sha").cloned()).await;
    let status = res.status();
    audit(
        &state.mcp.pool,
        &app,
        &account,
        "code_diff",
        status,
        started,
    )
    .await;
    res
}

async fn api_compare(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (account, app) = match admin_app(&state.mcp, &headers, &slug).await {
        Ok(found) => found,
        Err(res) => return *res,
    };
    let started = crate::auth::now_ms();
    let res = compare_for(
        &state.mcp,
        &app,
        query.get("from").cloned(),
        query.get("to").cloned(),
    )
    .await;
    let status = res.status();
    audit(
        &state.mcp.pool,
        &app,
        &account,
        "code_compare",
        status,
        started,
    )
    .await;
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::create_session;
    use crate::auth::test_support::{test_config, test_pool};
    use crate::git::commit::{self, CommitRequest, FileWrite, WriteBody};
    use crate::mcp::McpState;
    use crate::store::snapshot::StateSync;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::SqlitePool;
    use tower::ServiceExt;

    const PEER: &str = "127.0.0.1:43123";

    fn peer(mut req: Request<Body>) -> Request<Body> {
        req.extensions_mut().insert(axum::extract::ConnectInfo(
            PEER.parse::<std::net::SocketAddr>()
                .expect("test peer parses"),
        ));
        req
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

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String) {
        let res = app.clone().oneshot(peer(req)).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// One owner account with an app and one commit, plus an admin and a
    /// stranger account. The admin has no role on the app.
    async fn harness() -> (Router, SqlitePool, String, String, String) {
        let (config, _codes) = test_config();
        let pool = test_pool().await;
        let auth = crate::auth::AuthState {
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
        for (id, email) in [
            ("acct-ana@example.com", "ana@example.com"),
            ("acct-owner@example.com", "owner@example.com"),
            ("acct-bruno@example.com", "bruno@example.com"),
        ] {
            sqlx::query(
                "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(email)
            .bind(now)
            .bind(now)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) \
             VALUES ('app-groceries', 'groceries', 'acct-ana@example.com', 300001, 18001, 18002, ?, ?)",
        )
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        crate::git::manifest::init_repo(&mcp.store, &mcp.git, &mcp.sync, "groceries")
            .await
            .unwrap();
        let c1 = commit::commit(
            &mcp.store,
            &mcp.git,
            &mcp.sync,
            "groceries",
            CommitRequest {
                message: "first groceries".to_string(),
                base: None,
                writes: vec![FileWrite {
                    path: "pages/app.tsx".to_string(),
                    body: WriteBody::Text("export default function App() {}\n".to_string()),
                }],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: commit::Author {
                    name: "ana".to_string(),
                    email: "ana@example.com".to_string(),
                },
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO deploy (id, app_id, sha, status, created_at, finished_at) VALUES ('dep-one', 'app-groceries', ?, 'success', ?, ?)",
        )
        .bind(&c1)
        .bind(now - 60_000)
        .bind(now - 50_000)
        .execute(&pool)
        .await
        .unwrap();
        let admin = create_session(&pool, "acct-owner@example.com")
            .await
            .unwrap();
        let stranger = create_session(&pool, "acct-bruno@example.com")
            .await
            .unwrap();
        (
            app,
            pool,
            format!("__Host-outer_sid={admin}"),
            format!("__Host-outer_sid={stranger}"),
            c1,
        )
    }

    #[tokio::test]
    async fn admin_reads_foreign_app_code_and_audits() {
        let (app, pool, admin, stranger, c1) = harness().await;

        // The role-scoped route still 404s for the stranger.
        let (status, _) = send(
            &app,
            authed("/api/console/apps/groceries/code/tree", &stranger),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // The admin inspection route serves the same listing…
        let (status, body) = send(
            &app,
            authed("/api/console/admin/apps/groceries/code/tree", &admin),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "tree failed: {body}");
        let tree: serde_json::Value = body.parse().unwrap();
        assert_eq!(tree["sha"], serde_json::Value::String(c1.clone()));
        assert_eq!(tree["files"][0]["path"], "pages/app.tsx");

        // …and the same file, history and diff, all audited as admin reads.
        for (path, tool) in [
            (
                "/api/console/admin/apps/groceries/code/file?path=pages/app.tsx".to_string(),
                "code_read_file",
            ),
            (
                "/api/console/admin/apps/groceries/code/history".to_string(),
                "code_history",
            ),
            (
                format!("/api/console/admin/apps/groceries/code/diff?sha={c1}"),
                "code_diff",
            ),
            (
                format!("/api/console/admin/apps/groceries/code/compare?from={c1}&to={c1}"),
                "code_compare",
            ),
        ] {
            let (status, _) = send(&app, authed(&path, &admin)).await;
            assert_eq!(status, StatusCode::OK, "{tool} failed");
        }
        let tools: Vec<(String,)> = sqlx::query_as(
            "SELECT tool FROM agent_action WHERE app_id = 'app-groceries' ORDER BY created_at, rowid",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            tools,
            [
                ("code_list_files".to_string(),),
                ("code_read_file".to_string(),),
                ("code_history".to_string(),),
                ("code_diff".to_string(),),
                ("code_compare".to_string(),),
            ]
        );

        // Unknown slugs stay 404, even for admins.
        let (status, _) = send(
            &app,
            authed("/api/console/admin/apps/missing/code/tree", &admin),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn admin_code_stays_admin_only() {
        let (app, _, admin, stranger, _) = harness().await;

        // A non-admin on the admin routes reads the same 404 as an unknown
        // slug, so the mirror routes do not oracle the admin surface.
        let (status, body) = send(
            &app,
            authed("/api/console/admin/apps/groceries/code/tree", &stranger),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["error"],
            "not_found"
        );

        // No session at all is 401, not 404: the guard runs before lookup.
        let (status, _) = send(
            &app,
            same_origin(Request::get("/api/console/admin/apps/groceries/code/tree"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let _ = admin;
    }
}
