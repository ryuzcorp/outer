//! Console code export (spec §7.5, §13.1): the Settings → Export code
//! card downloads through here, calling the SAME domain function as the
//! MCP `export_code` tool (same Pro gate, same caps, same messages).
//!
//! - `POST /api/console/apps/{slug}/export {sha?}`: zips the tree at `sha`
//!   (default: `main`), uploads it, and answers `{url, expires_at, sha,
//!   bytes}`. The UI navigates the browser to `url`. Audited as
//!   `export_code`.
//!
//! Owner only: a collaborator reads 403 with the MCP `forbidden` code and
//! user message (spec §7.4), and the card itself only renders for owners.
//! Errors carry the `{error, message, user_message?}` MCP shape via
//! [`super::console_actions::tool_response`].

use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

pub fn routes() -> Router<super::AppState> {
    Router::new().route("/api/console/apps/{slug}/export", post(api_export))
}

/// The session plus the visible app, plus the explicit owner check: exports
/// are owner-only (spec §7.5), so a collaborator reads 403 with the same
/// code and user message the MCP tool answers — never 404 (spec §8.2
/// reserves 404 for slugs the caller has no role on). The admin override
/// reports the owner role, so admins pass this check. Export audits itself,
/// so no read label.
async fn owner_app(
    mcp: &crate::mcp::McpState,
    headers: &HeaderMap,
    slug: &str,
) -> Result<(crate::auth::AccountRow, crate::apps::App, bool), Box<Response>> {
    use crate::mcp::error::ToolError;
    let account = match super::console::guard(&mcp.auth, headers).await {
        Ok(account) => account,
        Err(res) => return Err(res),
    };
    match super::console::authorized_app(mcp, &account, headers, slug, None).await {
        Ok(authz) if authz.role == crate::collaborators::Role::Owner => {
            Ok((authz.account, authz.app, authz.admin))
        }
        Ok(_) => Err(super::console_actions::tool_response(
            &ToolError::forbidden(format!("only the app owner may export {slug:?} this way"))
                .with_user("Only the app owner can do that. Collaborators can read and deploy, but not change these settings."),
        )
        .into()),
        Err(res) => Err(res),
    }
}

#[derive(Debug, serde::Deserialize)]
struct ExportBody {
    sha: Option<String>,
}

/// Bounded: a sha overlong is refused by validation anyway, but the cap
/// keeps one log row small whatever the caller sent.
const MAX_SHA_BYTES: usize = 256;

async fn api_export(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    use crate::mcp::error::ToolError;
    let started = Instant::now();
    let (account, app, admin) = match owner_app(&state.mcp, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let raw: ExportBody = match serde_json::from_value(body) {
        Ok(raw) => raw,
        Err(_) => {
            return super::console_actions::tool_response(&ToolError::invalid_argument(
                "body must be {sha?}",
            ));
        }
    };
    if raw
        .sha
        .as_ref()
        .is_some_and(|sha| sha.len() > MAX_SHA_BYTES)
    {
        return super::console_actions::tool_response(&ToolError::invalid_argument("sha too long"));
    }
    let outcome = crate::mcp::tools::export::export_for_owner(
        &state.mcp.pool,
        &state.mcp.store,
        &state.mcp.git,
        &state.mcp.sync,
        &app.slug,
        &app.id,
        raw.sha.as_deref(),
        chrono::Utc::now(),
    )
    .await;
    // Audited like the MCP tool (client "Console": both ids NULL). A
    // refused Pro-gated export is signal, not noise.
    let (outcome_str, error_code) = match &outcome {
        Ok(_) => ("ok", None),
        Err(err) => ("error", Some(err.code)),
    };
    crate::mcp::actions::record_console(
        &state.mcp.pool,
        &app.id,
        &account.id,
        "export_code",
        &super::console::audit_summary(&account, &format!("export_code {}", app.slug), admin),
        outcome_str,
        error_code,
        started.elapsed().as_millis().min(u128::from(u64::MAX)) as i64,
    )
    .await;
    match outcome {
        Ok(outcome) => super::console::ok_json(json!({
            "url": outcome.url,
            "expires_at": outcome.expires_at,
            "sha": outcome.sha,
            "bytes": outcome.bytes,
        })),
        Err(err) => super::console_actions::tool_response(&err),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use tower::ServiceExt;

    use crate::auth::test_support::{test_config, test_pool};
    use crate::auth::{create_session, AuthState};
    use crate::git::commit::{self, Author, CommitRequest, FileWrite, WriteBody};
    use crate::mcp::McpState;
    use crate::store::snapshot::StateSync;

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

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String) {
        let res = app.clone().oneshot(peer(req)).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    fn post(path: &str, cookie: &str, body: serde_json::Value) -> Request<Body> {
        same_origin(
            Request::post(path)
                .header("content-type", "application/json")
                .header("cookie", cookie),
        )
        .body(Body::from(body.to_string()))
        .unwrap()
    }

    fn author() -> Author {
        Author {
            name: "ana".to_string(),
            email: "ana@example.com".to_string(),
        }
    }

    /// One owner, one app, one commit on a real mirror: the endpoint has a
    /// tree to zip. Returns the session cookie and the commit sha.
    async fn seed(pool: &sqlx::SqlitePool, mcp: &McpState, plan: &str) -> (String, String) {
        let now = crate::auth::now_ms();
        sqlx::query(
            "INSERT INTO user (id, email, plan, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("acct-ana@example.com")
        .bind("ana@example.com")
        .bind(plan)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) \
             VALUES ('app-groceries', 'groceries', 'acct-ana@example.com', 300001, 18001, 18002, ?, ?)",
        )
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        crate::git::manifest::init_repo(&mcp.store, &mcp.git, &mcp.sync, "groceries")
            .await
            .unwrap();
        let sha = commit::commit(
            &mcp.store,
            &mcp.git,
            &mcp.sync,
            "groceries",
            CommitRequest {
                message: "groceries".to_string(),
                base: None,
                writes: vec![FileWrite {
                    path: "pages/index.tsx".to_string(),
                    body: WriteBody::Text("<h1>Groceries</h1>\n".to_string()),
                }],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let session = create_session(pool, "acct-ana@example.com").await.unwrap();
        (format!("__Host-outer_sid={session}"), sha)
    }

    async fn harness(plan: &str) -> (Router, sqlx::SqlitePool, String, String) {
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
        let (cookie, sha) = seed(&pool, &mcp, plan).await;
        (app, pool, cookie, sha)
    }

    #[tokio::test]
    async fn export_is_free_and_returns_a_download() {
        let (app, pool, cookie, sha) = harness("free").await;
        let path = "/api/console/apps/groceries/export";

        // Free owner: export works (owner-only stays, no Pro gate).
        let (status, body) = send(&app, post(path, &cookie, serde_json::json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["sha"], serde_json::json!(sha));
        assert!(body["bytes"].as_u64().unwrap() > 0, "{body}");
        let url = body["url"].as_str().unwrap();
        assert!(url.contains("response-content-disposition="), "{url}");
        assert!(url.contains("attachment"), "{url}");
        assert!(body["expires_at"].as_str().is_some(), "{body}");

        // The version picker passes a sha through; an unknown one 400s.
        let (status, picked) =
            send(&app, post(path, &cookie, serde_json::json!({"sha": sha}))).await;
        assert_eq!(status, StatusCode::OK, "{picked}");
        assert_eq!(
            picked.parse::<serde_json::Value>().unwrap()["sha"],
            json_sha(&sha)
        );
        let (status, bad) = send(
            &app,
            post(path, &cookie, serde_json::json!({"sha": "deadbeef"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");

        // Every attempt audited under the MCP tool name, client "Console".
        let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT tool, outcome, error_code FROM agent_action ORDER BY created_at",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(rows.len() >= 3, "{rows:?}");
        assert!(
            rows.iter().all(|(tool, _, _)| tool == "export_code"),
            "{rows:?}"
        );
        assert!(rows.iter().any(|(_, outcome, _)| outcome == "ok"));
        assert!(rows.iter().any(|(_, outcome, code)| outcome == "error"
            && *code == Some("invalid_argument".to_string())));
    }

    fn json_sha(sha: &str) -> serde_json::Value {
        serde_json::Value::String(sha.to_string())
    }

    #[tokio::test]
    async fn export_is_owner_only() {
        let (app, pool, cookie, _) = harness("pro").await;
        let now = crate::auth::now_ms();
        for email in ["bruno@example.com", "stranger@example.com"] {
            sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, ?, ?, ?)")
                .bind(format!("acct-{email}"))
                .bind(email)
                .bind(now)
                .bind(now)
                .bind(now)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO app_collaborator (app_id, email, userId, added_at) VALUES ('app-groceries', 'bruno@example.com', 'acct-bruno@example.com', ?1)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let bruno = create_session(&pool, "acct-bruno@example.com")
            .await
            .unwrap();
        let stranger = create_session(&pool, "acct-stranger@example.com")
            .await
            .unwrap();
        let path = "/api/console/apps/groceries/export";

        // A collaborator reads forbidden with the owner-only user message.
        let (status, body) = send(
            &app,
            post(
                path,
                &format!("__Host-outer_sid={bruno}"),
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let body: serde_json::Value = body.parse().unwrap();
        assert_eq!(body["error"], "forbidden");
        // A slug the caller has no role on reads exactly like a missing one.
        let (status, _) = send(
            &app,
            post(
                path,
                &format!("__Host-outer_sid={stranger}"),
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // The owner still downloads.
        let (status, body) = send(&app, post(path, &cookie, serde_json::json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}
