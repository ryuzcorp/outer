//! Console code view (wave 2 (e)): a read-only window into the app's repo.
//!
//! Routes (platform host only, merged next to [`super::console`]):
//!
//! - `GET /api/console/apps/:slug/code/tree?sha=&prefix=`: the file listing
//!   at a version (default: the live sha, the latest successful deploy).
//! - `GET /api/console/apps/:slug/code/file?path=&sha=`: one file's content
//!   with line counts, or its kind and size when binary or over the cap.
//! - `GET /api/console/apps/:slug/code/history?path=&skip=`: commits with
//!   messages and times (the version picker).
//! - `GET /api/console/apps/:slug/code/diff?sha=`: one commit's changed
//!   files (status plus +/- counts) and unified patch vs its parent.
//! - `GET /api/console/apps/:slug/code/compare?from=&to=`: the same changed
//!   files and patch between two versions (`to` is the live sha for the
//!   "compare with live" button).
//!
//! Read-only. Owner-scoped exactly like the console app endpoints (and the
//! MCP read tools, which are owner-only until collaborators land): a foreign
//! or unknown slug answers 404 like a missing one. Every read goes through
//! the same [`crate::git::local`] functions the MCP `list_files`/`read_file`
//! tools use — no second implementation, same bounds (256 KiB blob cap,
//! 64 KiB patch/text caps) and the same binary detection.

use std::collections::HashMap;
use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;

use super::console::{guard, nostore_json, not_found, ok_json, owned_app};
use crate::git::local;
use crate::mcp::McpState;

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/api/console/apps/{slug}/code/tree", get(api_tree))
        .route("/api/console/apps/{slug}/code/file", get(api_file))
        .route("/api/console/apps/{slug}/code/history", get(api_history))
        .route("/api/console/apps/{slug}/code/diff", get(api_diff))
        .route("/api/console/apps/{slug}/code/compare", get(api_compare))
}

/// Text longer than this is cut with `truncated: true`, the same ceiling the
/// MCP `read_file` tool reports.
const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Most files one tree listing carries before `truncated` is set.
const MAX_TREE_FILES: usize = 5000;

/// The caller's app, or the response to answer when it isn't theirs.
async fn app_ctx(
    mcp: &McpState,
    headers: &HeaderMap,
    slug: &str,
) -> Result<crate::apps::App, Box<Response>> {
    let account = guard(&mcp.auth, headers).await?;
    match owned_app(mcp, &account, slug).await {
        Ok(Some(app)) => Ok(app),
        Ok(None) => Err(not_found().into()),
        Err(_) => Err(nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "could not load the app right now",
        )
        .into()),
    }
}

/// The latest successfully deployed sha, or `None` when nothing ever
/// succeeded. The tree and file views fall back to it; the UI labels it
/// "live" and compares against it.
async fn live_sha(pool: &SqlitePool, app_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT sha FROM deploy WHERE app_id = ?1 AND status = 'success' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(app_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
}

/// The synced bare mirror, or a 500: the repo should always sync (MCP reads
/// go through the same call).
async fn mirror(mcp: &McpState, slug: &str) -> Result<PathBuf, Box<Response>> {
    crate::git::manifest::ensure_synced(&mcp.store, &mcp.git, &mcp.sync, slug)
        .await
        .map_err(|_| {
            nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the repo right now",
            )
            .into()
        })
}

/// `sha` is hex before it reaches git; anything else is a 400 naming only
/// the caller-supplied value, never a server path.
fn check_sha(raw: &str) -> Result<(), Box<Response>> {
    local::validate_sha(raw).map_err(|_| {
        nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            &format!("sha is not a commit sha: {raw:?}"),
        )
        .into()
    })
}

fn stat_json(s: &local::FileStat) -> serde_json::Value {
    json!({
        "path": s.path,
        "status": s.status.to_string(),
        "added": s.added,
        "removed": s.removed,
        "binary": s.binary,
    })
}

async fn api_tree(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let app = match app_ctx(&state.mcp, &headers, &slug).await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    if let Some(raw) = query.get("sha") {
        if let Err(res) = check_sha(raw) {
            return *res;
        }
    }
    let prefix = query
        .get("prefix")
        .map(|p| p.trim_end_matches('/').to_string())
        .filter(|p| !p.is_empty());
    if let Some(prefix) = prefix.as_deref() {
        if let Err(e) = local::validate_read_path(prefix) {
            return nostore_json(StatusCode::BAD_REQUEST, "invalid_argument", &e.to_string());
        }
    }
    let live = live_sha(&state.mcp.pool, &app.id).await;
    let bare = match mirror(&state.mcp, &app.slug).await {
        Ok(bare) => bare,
        Err(res) => return *res,
    };
    let rev = query.get("sha").cloned().or(live.clone());
    // Resolve first so a well-formed but unknown sha is a 404 ("no such
    // version"), not a 500: `resolve_rev` only fails on bad revs, and the
    // sha shape was checked above.
    let resolved = match local::resolve_rev(&bare, rev.as_deref()).await {
        Ok(Some(sha)) => sha,
        Ok(None) => return nostore_json(StatusCode::NOT_FOUND, "not_found", "no commits yet"),
        Err(_) => return nostore_json(StatusCode::NOT_FOUND, "not_found", "no such version"),
    };
    match local::list_files(&bare, Some(&resolved), prefix.as_deref(), MAX_TREE_FILES).await {
        Ok((sha, files, truncated)) => ok_json(json!({
            "sha": sha,
            "live_sha": live,
            "files": files.iter().map(|f| json!({"path": f.path, "size": f.size})).collect::<Vec<_>>(),
            "truncated": truncated,
        })),
        Err(_) => nostore_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            "could not list files right now",
        ),
    }
}

async fn api_file(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let app = match app_ctx(&state.mcp, &headers, &slug).await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    let Some(path) = query.get("path").cloned() else {
        return nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "path is required",
        );
    };
    if let Err(e) = local::validate_read_path(&path) {
        return nostore_json(StatusCode::BAD_REQUEST, "invalid_argument", &e.to_string());
    }
    if let Some(raw) = query.get("sha") {
        if let Err(res) = check_sha(raw) {
            return *res;
        }
    }
    let live = live_sha(&state.mcp.pool, &app.id).await;
    let bare = match mirror(&state.mcp, &app.slug).await {
        Ok(bare) => bare,
        Err(res) => return *res,
    };
    let rev = query.get("sha").cloned().or(live.clone());
    match local::read_blob(&bare, rev.as_deref(), &path).await {
        Ok((sha, bytes)) => {
            let size = bytes.len() as u64;
            if bytes.contains(&0) {
                // Binary: type and size only, never the bytes.
                return ok_json(json!({
                    "path": path, "sha": sha, "kind": "binary", "size": size,
                    "live_sha": live,
                }));
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let total_lines = text.lines().count() as u64;
            let (text, truncated) = cut_text(&text);
            ok_json(json!({
                "path": path,
                "sha": sha,
                "kind": "text",
                "text": text,
                "truncated": truncated,
                "total_lines": total_lines,
                "size": size,
                "live_sha": live,
            }))
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") {
                return nostore_json(StatusCode::NOT_FOUND, "not_found", "no such file");
            }
            if msg.contains("no commits yet") {
                return nostore_json(StatusCode::NOT_FOUND, "not_found", "no commits yet");
            }
            if msg.contains("over the") {
                // Over the 256 KiB blob cap: name the size so the viewer can
                // say what it refused, without loading the blob.
                let size = local::blob_size(&bare, rev.as_deref(), &path)
                    .await
                    .map(|(_, size)| size)
                    .unwrap_or(0);
                return ok_json(json!({
                    "path": path,
                    "sha": rev.unwrap_or_default(),
                    "kind": "too_large",
                    "size": size,
                    "live_sha": live,
                }));
            }
            if msg.contains("must") || msg.contains("path") {
                return nostore_json(StatusCode::BAD_REQUEST, "invalid_argument", &msg);
            }
            nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not read the file right now",
            )
        }
    }
}

/// Cut `text` at [`MAX_TEXT_BYTES`] on a UTF-8 boundary.
fn cut_text(text: &str) -> (String, bool) {
    if text.len() <= MAX_TEXT_BYTES {
        return (text.to_string(), false);
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    path: Option<String>,
    skip: Option<String>,
}

async fn api_history(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let app = match app_ctx(&state.mcp, &headers, &slug).await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    if let Some(path) = query.path.as_deref() {
        if let Err(e) = local::validate_read_path(path) {
            return nostore_json(StatusCode::BAD_REQUEST, "invalid_argument", &e.to_string());
        }
    }
    let skip: usize = match query.skip.as_deref() {
        None => 0,
        Some(raw) => match raw.parse() {
            Ok(skip) => skip,
            Err(_) => {
                return nostore_json(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "skip must be a non-negative integer",
                );
            }
        },
    };
    let bare = match mirror(&state.mcp, &app.slug).await {
        Ok(bare) => bare,
        Err(res) => return *res,
    };
    match local::log(&bare, query.path.as_deref(), skip, 50).await {
        Ok((head, entries, next_skip)) => {
            let live = live_sha(&state.mcp.pool, &app.id).await;
            ok_json(json!({
                "head": head,
                "live_sha": live,
                "commits": entries.iter().map(|e| json!({
                    "sha": e.sha,
                    "author": e.author_name,
                    "email": e.author_email,
                    "at": e.at,
                    "message": e.message,
                })).collect::<Vec<_>>(),
                "next_skip": next_skip,
            }))
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("must") || msg.contains("path") {
                return nostore_json(StatusCode::BAD_REQUEST, "invalid_argument", &msg);
            }
            nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load history right now",
            )
        }
    }
}

async fn api_diff(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let app = match app_ctx(&state.mcp, &headers, &slug).await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    let Some(raw) = query.get("sha").cloned() else {
        return nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "sha is required",
        );
    };
    if let Err(res) = check_sha(&raw) {
        return *res;
    }
    let bare = match mirror(&state.mcp, &app.slug).await {
        Ok(bare) => bare,
        Err(res) => return *res,
    };
    let shown = match local::show(&bare, &raw).await {
        Ok(shown) => shown,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("unknown commit") {
                return nostore_json(StatusCode::NOT_FOUND, "not_found", "no such version");
            }
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the diff right now",
            );
        }
    };
    let stats = match local::show_stats(&bare, &shown.commit.sha).await {
        Ok(stats) => stats,
        Err(_) => {
            return nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not load the diff right now",
            );
        }
    };
    let parent = local::parent_of(&bare, &shown.commit.sha)
        .await
        .unwrap_or(None);
    ok_json(json!({
        "commit": {
            "sha": shown.commit.sha,
            "author": shown.commit.author_name,
            "email": shown.commit.author_email,
            "at": shown.commit.at,
            "message": shown.commit.message,
        },
        "parent": parent,
        "files": stats.iter().map(stat_json).collect::<Vec<_>>(),
        "patch": shown.patch,
        "truncated": shown.truncated,
    }))
}

async fn api_compare(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let app = match app_ctx(&state.mcp, &headers, &slug).await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    let (Some(from), Some(to)) = (query.get("from").cloned(), query.get("to").cloned()) else {
        return nostore_json(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "from and to are required",
        );
    };
    if let Err(res) = check_sha(&from).and(check_sha(&to)) {
        return *res;
    }
    let bare = match mirror(&state.mcp, &app.slug).await {
        Ok(bare) => bare,
        Err(res) => return *res,
    };
    match local::diff_range(&bare, &from, &to).await {
        Ok((from, to, stats, patch, truncated)) => ok_json(json!({
            "from": from,
            "to": to,
            "files": stats.iter().map(stat_json).collect::<Vec<_>>(),
            "patch": patch,
            "truncated": truncated,
        })),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("unknown commit") {
                return nostore_json(StatusCode::NOT_FOUND, "not_found", "no such version");
            }
            if msg.contains("not a commit sha") {
                return nostore_json(StatusCode::BAD_REQUEST, "invalid_argument", &msg);
            }
            nostore_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unavailable",
                "could not compare right now",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::{test_config, test_pool};
    use crate::auth::{create_session, AuthConfig, AuthState};
    use crate::git::commit::{self, Author, CommitRequest, Edit, FileWrite, WriteBody};
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

    fn author() -> Author {
        Author {
            name: "ana".to_string(),
            email: "ana@example.com".to_string(),
        }
    }

    fn text(path: &str, content: &str) -> FileWrite {
        FileWrite {
            path: path.to_string(),
            body: WriteBody::Text(content.to_string()),
        }
    }

    /// One account, one app, two commits (add/modify/delete), a small binary
    /// and an over-cap blob, and two successful deploys so the second commit
    /// is live. Returns the session cookie and the two commit shas.
    async fn seed(
        pool: &SqlitePool,
        mcp: &McpState,
        config: &AuthConfig,
    ) -> (String, String, String) {
        let _ = config;
        let now = crate::auth::now_ms();
        sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, ?, ?, ?)")
            .bind("acct-ana@example.com")
            .bind("ana@example.com")
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
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::STANDARD;
        let first = CommitRequest {
            message: "first groceries".to_string(),
            base: None,
            writes: vec![
                text(
                    "pages/app.tsx",
                    "export default function App() {\n  return <h1>Hi</h1>;\n}\n",
                ),
                text("README.md", "# groceries\n"),
                text("notes/todo.txt", "milk\neggs\n"),
                FileWrite {
                    path: "img.bin".to_string(),
                    body: WriteBody::Base64(engine.encode([0u8, 1, 2, 3, 255])),
                },
                FileWrite {
                    path: "big.bin".to_string(),
                    body: WriteBody::Base64(engine.encode(vec![b'x'; 300 * 1024])),
                },
            ],
            edits: vec![],
            deletes: vec![],
            moves: vec![],
            author: author(),
        };
        let c1 = commit::commit(&mcp.store, &mcp.git, &mcp.sync, "groceries", first)
            .await
            .unwrap();
        let second = CommitRequest {
            message: "add about, drop notes".to_string(),
            base: None,
            writes: vec![text(
                "pages/about.tsx",
                "export default function About() {\n  return <p>us</p>;\n}\n",
            )],
            edits: vec![Edit {
                path: "notes/todo.txt".to_string(),
                old: "eggs\n".to_string(),
                new: "eggs\nbread\n".to_string(),
                replace_all: false,
            }],
            deletes: vec!["README.md".to_string()],
            moves: vec![],
            author: author(),
        };
        let c2 = commit::commit(&mcp.store, &mcp.git, &mcp.sync, "groceries", second)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO deploy (id, app_id, sha, status, created_at, finished_at) VALUES (?, 'app-groceries', ?, 'success', ?, ?)",
        )
        .bind("deploy-one")
        .bind(&c1)
        .bind(now - 120_000)
        .bind(now - 110_000)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO deploy (id, app_id, sha, status, created_at, finished_at) VALUES (?, 'app-groceries', ?, 'success', ?, ?)",
        )
        .bind("deploy-two")
        .bind(&c2)
        .bind(now - 60_000)
        .bind(now - 50_000)
        .execute(pool)
        .await
        .unwrap();
        let session = create_session(pool, "acct-ana@example.com").await.unwrap();
        (format!("__Host-outer_sid={session}"), c1, c2)
    }

    async fn harness() -> (Router, SqlitePool, String, String, String) {
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
        let (cookie, c1, c2) = seed(&pool, &mcp, &config).await;
        (app, pool, cookie, c1, c2)
    }

    #[tokio::test]
    async fn tree_defaults_to_live_and_lists_sizes() {
        let (app, _, cookie, _, c2) = harness().await;
        let (status, body) = send(
            &app,
            authed("/api/console/apps/groceries/code/tree", &cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "tree failed: {body}");
        let tree: serde_json::Value = body.parse().unwrap();
        assert_eq!(tree["sha"], json!(c2), "default version is the live sha");
        assert_eq!(tree["live_sha"], json!(c2));
        let files = tree["files"].as_array().unwrap();
        assert_eq!(files.len(), 5, "second commit adds about.tsx: {files:?}");
        assert!(files.iter().all(|f| f["size"].as_u64().unwrap_or(0) > 0));
        assert_eq!(tree["truncated"], json!(false));
    }

    #[tokio::test]
    async fn tree_version_picker_and_prefix() {
        let (app, _, cookie, c1, _) = harness().await;
        let (status, body) = send(
            &app,
            authed(
                &format!("/api/console/apps/groceries/code/tree?sha={c1}"),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "old tree failed: {body}");
        let tree: serde_json::Value = body.parse().unwrap();
        assert_eq!(tree["sha"], json!(c1));
        let paths: Vec<&str> = tree["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"notes/todo.txt") && !paths.contains(&"pages/about.tsx"));
        let (status, body) = send(
            &app,
            authed(
                "/api/console/apps/groceries/code/tree?prefix=pages",
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "prefix tree failed: {body}");
        let tree: serde_json::Value = body.parse().unwrap();
        let paths: Vec<&str> = tree["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert!(paths.iter().all(|p| p.starts_with("pages/")) && paths.len() == 2);
        // Traversal and unknown versions are refused without leaking paths.
        let (status, _) = send(
            &app,
            authed("/api/console/apps/groceries/code/tree?prefix=../x", &cookie),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(
            &app,
            authed("/api/console/apps/groceries/code/tree?sha=0123456789abcdef0123456789abcdef01234567", &cookie),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn file_text_binary_oversized_and_missing() {
        let (app, _, cookie, c1, c2) = harness().await;
        let (status, body) = send(
            &app,
            authed(
                "/api/console/apps/groceries/code/file?path=notes/todo.txt",
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "file failed: {body}");
        let file: serde_json::Value = body.parse().unwrap();
        assert_eq!(file["kind"], json!("text"));
        assert_eq!(file["text"], json!("milk\neggs\nbread\n"));
        assert_eq!(file["total_lines"], json!(3));
        assert_eq!(file["sha"], json!(c2));
        // Pinned old versions still read.
        let (status, body) = send(
            &app,
            authed(
                &format!("/api/console/apps/groceries/code/file?path=README.md&sha={c1}"),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "old file failed: {body}");
        assert_eq!(
            body.parse::<serde_json::Value>().unwrap()["text"],
            json!("# groceries\n")
        );
        // Binary files report type and size only, never bytes.
        let (status, body) = send(
            &app,
            authed(
                "/api/console/apps/groceries/code/file?path=img.bin",
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "binary failed: {body}");
        let file: serde_json::Value = body.parse().unwrap();
        assert_eq!(file["kind"], json!("binary"));
        assert_eq!(file["size"], json!(5));
        assert!(file.get("text").is_none());
        // Over-cap blobs name their size instead of loading.
        let (status, body) = send(
            &app,
            authed(
                "/api/console/apps/groceries/code/file?path=big.bin",
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "oversized failed: {body}");
        let file: serde_json::Value = body.parse().unwrap();
        assert_eq!(file["kind"], json!("too_large"));
        assert_eq!(file["size"], json!(300 * 1024));
        // Missing files, missing params and escapes are 404/400.
        let (status, _) = send(
            &app,
            authed(
                "/api/console/apps/groceries/code/file?path=nope.txt",
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            &app,
            authed("/api/console/apps/groceries/code/file", &cookie),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(
            &app,
            authed(
                "/api/console/apps/groceries/code/file?path=../secret",
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn history_lists_commits_with_messages() {
        let (app, _, cookie, c1, c2) = harness().await;
        let (status, body) = send(
            &app,
            authed("/api/console/apps/groceries/code/history", &cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "history failed: {body}");
        let history: serde_json::Value = body.parse().unwrap();
        assert_eq!(history["live_sha"], json!(c2));
        let commits = history["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["sha"], json!(c2));
        assert_eq!(commits[0]["message"], json!("add about, drop notes"));
        assert!(commits[0]["at"].as_i64().unwrap_or(0) > 0);
        assert_eq!(commits[1]["sha"], json!(c1));
        assert_eq!(history["next_skip"], json!(2));
        let (status, body) = send(
            &app,
            authed("/api/console/apps/groceries/code/history?skip=1", &cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "paged history failed: {body}");
        let history: serde_json::Value = body.parse().unwrap();
        assert_eq!(history["commits"].as_array().unwrap().len(), 1);
        let (status, _) = send(
            &app,
            authed(
                "/api/console/apps/groceries/code/history?skip=nope",
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn diff_reports_stats_and_patch() {
        let (app, _, cookie, c1, c2) = harness().await;
        let (status, body) = send(
            &app,
            authed(
                &format!("/api/console/apps/groceries/code/diff?sha={c2}"),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "diff failed: {body}");
        let diff: serde_json::Value = body.parse().unwrap();
        assert_eq!(diff["commit"]["message"], json!("add about, drop notes"));
        assert_eq!(diff["parent"], json!(c1));
        let files = diff["files"].as_array().unwrap();
        assert_eq!(files.len(), 3, "add/modify/delete: {files:?}");
        let by_path = |p: &str| files.iter().find(|f| f["path"] == p).unwrap().clone();
        assert_eq!(by_path("pages/about.tsx")["status"], json!("A"));
        assert_eq!(by_path("README.md")["status"], json!("D"));
        let modified = by_path("notes/todo.txt");
        assert_eq!(modified["status"], json!("M"));
        assert_eq!(
            (modified["added"].clone(), modified["removed"].clone()),
            (json!(1), json!(0))
        );
        assert!(diff["patch"].as_str().unwrap().contains("bread"));
        // The first commit diffs against the empty tree and has no parent.
        let (status, body) = send(
            &app,
            authed(
                &format!("/api/console/apps/groceries/code/diff?sha={c1}"),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "root diff failed: {body}");
        let diff: serde_json::Value = body.parse().unwrap();
        assert_eq!(diff["parent"], serde_json::Value::Null);
        assert_eq!(diff["files"].as_array().unwrap().len(), 5);
        let (status, _) = send(
            &app,
            authed("/api/console/apps/groceries/code/diff", &cookie),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(
            &app,
            authed("/api/console/apps/groceries/code/diff?sha=0123456789abcdef0123456789abcdef01234567", &cookie),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn compare_matches_diff_and_empty_matches_itself() {
        let (app, _, cookie, c1, c2) = harness().await;
        let (status, body) = send(
            &app,
            authed(
                &format!("/api/console/apps/groceries/code/compare?from={c1}&to={c2}"),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "compare failed: {body}");
        let compared: serde_json::Value = body.parse().unwrap();
        assert_eq!(compared["files"].as_array().unwrap().len(), 3);
        assert!(compared["patch"].as_str().unwrap().contains("bread"));
        let (status, body) = send(
            &app,
            authed(
                &format!("/api/console/apps/groceries/code/compare?from={c2}&to={c2}"),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "self compare failed: {body}");
        let compared: serde_json::Value = body.parse().unwrap();
        assert!(compared["files"].as_array().unwrap().is_empty());
        assert!(compared["patch"].as_str().unwrap().is_empty());
        let (status, _) = send(
            &app,
            authed(
                &format!("/api/console/apps/groceries/code/compare?from={c1}"),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn code_scoping_matches_the_app_endpoints() {
        let (app, pool, cookie, _, _) = harness().await;
        // A stranger's slug answers 404 exactly like a missing one.
        let now = crate::auth::now_ms();
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('acct-bob', 'bob@example.com', ?, ?, ?)",
        )
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let bob = create_session(&pool, "acct-bob").await.unwrap();
        let bob = format!("__Host-outer_sid={bob}");
        let (status, _) = send(&app, authed("/api/console/apps/groceries/code/tree", &bob)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // No cookie is 401, a cross-site fetch is 403 — same as every
        // console endpoint.
        let (status, _) = send(
            &app,
            same_origin(Request::get("/api/console/apps/groceries/code/tree"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(
            &app,
            Request::get("/api/console/apps/groceries/code/tree")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
