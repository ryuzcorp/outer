//! End-to-end MCP tests: JSON-RPC envelope, lazy auth, every tool group and
//! the OTP sign-in round trip. Deterministic: memory store, temp dirs, fake
//! deploys, captured email codes — no celld, no network.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use serde_json::{json, Value};

use super::registry::registry;
use super::{mcp_handler, FakeOutcome, McpState, INSTRUCTIONS, LATEST_PROTOCOL};
use crate::auth::test_support as auth_ts;
use crate::store::Store;

type Codes = Arc<Mutex<HashMap<String, String>>>;

async fn setup() -> (McpState, Codes) {
    let dir = tempfile::tempdir().unwrap();
    let pool = crate::db::open(&dir.path().join("outer.sqlite"))
        .await
        .unwrap();
    // The pool holds the path open; the dir must outlive the test.
    std::mem::forget(dir);
    let (auth_cfg, codes) = auth_ts::test_config();
    let auth_state = crate::auth::AuthState {
        pool: pool.clone(),
        config: auth_cfg,
    };
    let state = McpState::for_tests(pool, auth_state);
    vendor_template(&state.store).await;
    (state, codes)
}

/// A tiny stand-in for the vendored Ilha template: three files, one commit,
/// bundled straight into the memory store. No network.
async fn vendor_template(store: &Store) {
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("pages")).unwrap();
    std::fs::create_dir_all(src.path().join("lib")).unwrap();
    std::fs::write(src.path().join("package.json"), "{}\n").unwrap();
    std::fs::write(src.path().join("pages/index.tsx"), "<h1>Groceries</h1>\n").unwrap();
    std::fs::write(
        src.path().join("lib/moderate.ts"),
        "export function moderate(s: string) { return s; }\n",
    )
    .unwrap();
    git(&src, &["init", "-b", "main"]);
    git(&src, &["add", "-A"]);
    git(
        &src,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "seed",
        ],
    );
    let sha = git_out(&src, &["rev-parse", "HEAD"]);
    let bundle = src.path().join("template.bundle");
    git(
        &src,
        &["bundle", "create", bundle.to_str().unwrap(), "main"],
    );
    crate::templates::put_test_template(store, sha.trim(), &bundle)
        .await
        .unwrap();
}

fn git(dir: &tempfile::TempDir, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &tempfile::TempDir, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn app_state(state: &McpState) -> crate::http::AppState {
    crate::http::AppState {
        pool: state.pool.clone(),
        sync: crate::store::snapshot::StateSync::new("test"),
        auth: state.auth.clone(),
        mcp: state.clone(),
        site: crate::http::site::SiteDir::new(std::path::Path::new("/nonexistent")),
    }
}

/// One `POST /mcp` through the real handler: status, headers, parsed body
/// (`Null` when the handler answers an empty 202).
async fn post(
    state: &McpState,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = mcp_handler(State(app_state(state)), req).await;
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        // Handler-level failures (401) carry a plain-text body, not JSON.
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (parts.status, parts.headers, parsed)
}

/// Raw bytes through the handler (for the parse-error path).
async fn post_raw(state: &McpState, raw: &[u8]) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .body(Body::from(raw.to_vec()))
        .unwrap();
    let resp = mcp_handler(State(app_state(state)), req).await;
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap();
    (parts.status, parsed)
}

async fn call(
    state: &McpState,
    token: Option<&str>,
    name: &str,
    args: Value,
) -> (StatusCode, Value) {
    let (_, _, body) = post(
        state,
        token,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": name, "arguments": args}}),
    )
    .await;
    // Handler-level failures (401) come back without a JSON-RPC envelope.
    (StatusCode::OK, body)
}

fn result(body: &Value) -> &Value {
    body.get("result")
        .unwrap_or_else(|| panic!("expected result, got {body}"))
}

fn is_error(body: &Value) -> &Value {
    let result = result(body);
    assert_eq!(
        result.get("isError"),
        Some(&json!(true)),
        "not an isError: {result}"
    );
    &result["structuredContent"]
}

/// An account plus a usable API key, bypassing email.
async fn api_key_for(state: &McpState, email: &str) -> String {
    let account_id = crate::apps::ensure_account(&state.pool, email)
        .await
        .unwrap();
    crate::auth::create_api_key(&state.pool, &account_id, None)
        .await
        .unwrap()
        .key
}

async fn create_app(state: &McpState, token: &str, slug: &str) -> Value {
    let (_, body) = call(state, Some(token), "create_app", json!({"slug": slug})).await;
    let structured = &result(&body)["structuredContent"];
    assert_eq!(structured["deploy"]["status"], "success");
    structured.clone()
}

#[tokio::test]
async fn envelope_and_error_codes() {
    let (state, _) = setup().await;

    let (_, body) = post_raw(&state, b"not json").await;
    assert_eq!(body["error"]["code"], -32700);

    let (_, _, missing_method) = post(&state, None, json!({"jsonrpc": "2.0", "id": 1})).await;
    assert_eq!(missing_method["error"]["code"], -32600);

    let (_, _, bad_version) = post(
        &state,
        None,
        json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
    )
    .await;
    assert_eq!(bad_version["error"]["code"], -32600);

    let (_, _, unknown) = post(
        &state,
        None,
        json!({"jsonrpc": "2.0", "id": 2, "method": "nope"}),
    )
    .await;
    assert_eq!(unknown["error"]["code"], -32601);

    let (_, _, no_version) = post(
        &state,
        None,
        json!({"jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {}}),
    )
    .await;
    assert_eq!(no_version["error"]["code"], -32602);

    let (_, _, unknown_tool) = post(
        &state,
        None,
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
               "params": {"name": "nope"}}),
    )
    .await;
    assert_eq!(unknown_tool["error"]["code"], -32602);

    // The lazy-auth gate answers 401 before argument validation, so the
    // bad-arguments case needs a token to reach the -32602.
    let token = api_key_for(&state, "ana@example.com").await;
    let (_, _, bad_args) = post(
        &state,
        Some(&token),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
               "params": {"name": "commit", "arguments": "x"}}),
    )
    .await;
    assert_eq!(bad_args["error"]["code"], -32602);

    // Notifications answer 202 with an empty body.
    let (status, _, body) = post(
        &state,
        None,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body, Value::Null);
}

#[tokio::test]
async fn initialize_negotiates_and_pins_instructions() {
    let (state, _) = setup().await;
    for version in ["2025-06-18", "2025-11-25"] {
        let (_, _, body) = post(
            &state,
            None,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": version}}),
        )
        .await;
        assert_eq!(result(&body)["protocolVersion"], version);
    }
    let (_, _, body) = post(
        &state,
        None,
        json!({"jsonrpc": "2.0", "id": 2, "method": "initialize",
               "params": {"protocolVersion": "1999-01-01"}}),
    )
    .await;
    assert_eq!(result(&body)["protocolVersion"], LATEST_PROTOCOL);
    assert_eq!(result(&body)["instructions"], json!(INSTRUCTIONS));
    assert!(
        INSTRUCTIONS.len() <= 512,
        "instructions are {} chars",
        INSTRUCTIONS.len()
    );
    assert_eq!(result(&body)["serverInfo"]["name"], "outer");

    // An authenticated initialize records clientInfo on the credential.
    let token = api_key_for(&state, "ana@example.com").await;
    let (_, _, authed) = post(
        &state,
        Some(&token),
        json!({"jsonrpc": "2.0", "id": 3, "method": "initialize",
               "params": {"protocolVersion": "2025-11-25",
                          "clientInfo": {"name": "claude-code", "version": "1.2.3"}}}),
    )
    .await;
    assert!(result(&authed).get("error").is_none());
    let (_, keys) = call(&state, Some(&token), "list_api_keys", json!({})).await;
    assert!(result(&keys)["structuredContent"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .any(|k| k["client"] == "claude-code/1.2.3"));
}

#[tokio::test]
async fn tools_list_filters_per_caller() {
    let (state, _) = setup().await;
    let (_, _, anon) = post(
        &state,
        None,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
    )
    .await;
    let anon: Vec<String> = result(&anon)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(anon.len(), 43);
    assert!(anon.contains(&"sign_in".to_string()));
    assert!(anon.contains(&"create_app".to_string()));

    let token = api_key_for(&state, "ana@example.com").await;
    let (_, _, authed) = post(
        &state,
        Some(&token),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
    )
    .await;
    let authed: Vec<String> = result(&authed)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(authed.len(), 41);
    assert!(!authed.contains(&"sign_in".to_string()));
    assert!(!authed.contains(&"verify_code".to_string()));

    // The test admin sees the same rows: M1 has no admin tools.
    let admin = api_key_for(&state, "owner@example.com").await;
    let (_, _, admin_list) = post(
        &state,
        Some(&admin),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
    )
    .await;
    let admin: Vec<String> = result(&admin_list)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(admin, authed);
}

#[tokio::test]
async fn lazy_auth_gates_account_tools() {
    let (state, _) = setup().await;
    // Public tools run anonymously.
    let (_, about) = call(&state, None, "about", json!({})).await;
    assert!(result(&about).get("isError").is_none());
    let (_, docs) = call(&state, None, "docs", json!({"topic": "ilha"})).await;
    assert!(result(&docs)["structuredContent"]["markdown"]
        .as_str()
        .unwrap()
        .contains("atom.lazy"));

    // An account tool without a token answers 401 with the exact challenge,
    // before the tool runs (no isError body: the gate is HTTP-level).
    let (status, headers, body) = post(
        &state,
        None,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "whoami", "arguments": {}}}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, Value::Null);
    let expected = crate::auth::www_authenticate(&state.auth.config)
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(headers["www-authenticate"], expected.as_str());

    // A bad token gets the same 401.
    let (status, _, _) = post(
        &state,
        Some("outer_deadbeef"),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "whoami", "arguments": {}}}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn registry_schemas_are_valid_json_schema() {
    for tool in registry() {
        assert!(
            !tool.name.is_empty() && !tool.description.is_empty(),
            "nameless tool"
        );
        assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
    }
    let names: Vec<_> = registry().iter().map(|t| t.name).collect();
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(names.len(), sorted.len(), "duplicate tool names");
    let untrusted: Vec<_> = registry()
        .iter()
        .filter(|t| t.untrusted)
        .map(|t| t.name)
        .collect();
    assert_eq!(
        untrusted,
        vec![
            "read_file",
            "search_code",
            "show_commit",
            "deploy_log",
            "app_logs",
            "app_errors",
            "look_at_app"
        ]
    );
}

#[tokio::test]
async fn create_commit_read_history_roundtrip() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;

    let created = create_app(&state, &token, "groceries").await;
    assert_eq!(created["app"]["url"], "http://groceries.outer.localhost");
    assert_eq!(created["app"]["visibility"], "unlisted");

    let (_, list) = call(
        &state,
        Some(&token),
        "list_files",
        json!({"app": "groceries"}),
    )
    .await;
    let files = &result(&list)["structuredContent"]["files"];
    assert!(files
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["path"] == "pages/index.tsx"));

    let (_, read) = call(
        &state,
        Some(&token),
        "read_file",
        json!({"app": "groceries", "path": "pages/index.tsx"}),
    )
    .await;
    let read = result(&read);
    assert!(read["structuredContent"]["text"]
        .as_str()
        .unwrap()
        .contains("Groceries"));
    // Untrusted content rides behind the §8.4 notice as its own block.
    let blocks = read["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert!(blocks[1]["text"]
        .as_str()
        .unwrap()
        .starts_with("The following was produced by your app and its visitors."));

    let (_, commit) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "Vegetables",
               "edits": [{"path": "pages/index.tsx", "old": "Groceries", "new": "Vegetables"}]}),
    )
    .await;
    let commit = &result(&commit)["structuredContent"];
    assert_eq!(commit["deploy"]["status"], "success");
    assert!(commit["deploy"]["url"]
        .as_str()
        .unwrap()
        .contains("groceries"));
    assert_eq!(commit["quota"]["used"], 1);
    // No formatter in tests: `formatted` is present but empty, and
    // `format_skipped` is omitted when nothing was skipped.
    assert_eq!(commit["commit"]["formatted"].as_array().unwrap().len(), 0);
    assert!(commit["commit"].get("format_skipped").is_none());
    let sha2 = commit["commit"]["sha"].as_str().unwrap().to_string();

    let (_, search) = call(
        &state,
        Some(&token),
        "search_code",
        json!({"app": "groceries", "query": "Vegetables"}),
    )
    .await;
    let search = result(&search);
    assert_eq!(
        search["structuredContent"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(search["content"].as_array().unwrap()[1]["text"]
        .as_str()
        .unwrap()
        .starts_with("The following was produced by your app and its visitors."));

    let (_, history) = call(&state, Some(&token), "history", json!({"app": "groceries"})).await;
    assert_eq!(
        result(&history)["structuredContent"]["commits"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let (_, shown) = call(
        &state,
        Some(&token),
        "show_commit",
        json!({"app": "groceries", "sha": sha2}),
    )
    .await;
    let shown = result(&shown);
    assert!(shown["structuredContent"]["patch"]
        .as_str()
        .unwrap()
        .contains("Vegetables"));

    let (_, get) = call(&state, Some(&token), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(result(&get)["structuredContent"]["status"], "running");

    // `public` is a Pro feature: upgrade before flipping visibility.
    sqlx::query("UPDATE user SET plan = 'pro' WHERE email = 'ana@example.com'")
        .execute(&state.pool)
        .await
        .unwrap();

    let (_, vis) = call(
        &state,
        Some(&token),
        "set_visibility",
        json!({"app": "groceries", "visibility": "public"}),
    )
    .await;
    assert_eq!(result(&vis)["structuredContent"]["visibility"], "public");

    let (_, share) = call(
        &state,
        Some(&token),
        "share_app",
        json!({"app": "groceries"}),
    )
    .await;
    let share = &result(&share)["structuredContent"];
    assert!(share["text"]
        .as_str()
        .unwrap()
        .contains("http://groceries.outer.localhost"));
    assert!(share["qr"].as_str().unwrap().len() > 20);
}

#[tokio::test]
async fn edit_match_semantics_name_path_and_count() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    // Zero matches name the path and the count.
    let (_, zero) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "nope",
               "edits": [{"path": "pages/index.tsx", "old": "Potatoes", "new": "x"}]}),
    )
    .await;
    let err = is_error(&zero);
    assert_eq!(err["error"], "invalid_argument");
    assert!(err["message"].as_str().unwrap().contains("pages/index.tsx"));
    assert!(err["message"].as_str().unwrap().contains('0'));

    // Two matches without replace_all: invalid_argument naming both.
    let (_, write) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "dup",
               "writes": [{"path": "dup.txt", "content": "x\nx\n"}]}),
    )
    .await;
    assert!(result(&write).get("isError").is_none());
    let (_, two) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "edit",
               "edits": [{"path": "dup.txt", "old": "x\n", "new": "y\n"}]}),
    )
    .await;
    let err = is_error(&two);
    assert_eq!(err["error"], "invalid_argument");
    let message = err["message"].as_str().unwrap();
    assert!(message.contains("dup.txt"), "{message}");
    assert!(message.contains('2'), "{message}");

    // replace_all clears both.
    let (_, all) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "edit-all",
               "edits": [{"path": "dup.txt", "old": "x\n", "new": "y\n", "replace_all": true}]}),
    )
    .await;
    assert!(result(&all).get("isError").is_none());
}

#[tokio::test]
async fn cas_conflict_returns_current_sha() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let created = create_app(&state, &token, "groceries").await;
    let sha1 = created["deploy"]["sha"].as_str().unwrap().to_string();

    let (_, first) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "one",
               "writes": [{"path": "a.txt", "content": "a\n"}]}),
    )
    .await;
    let sha2 = result(&first)["structuredContent"]["commit"]["sha"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(sha1, sha2);

    let (_, conflict) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "stale", "base": sha1,
               "writes": [{"path": "b.txt", "content": "b\n"}]}),
    )
    .await;
    let err = is_error(&conflict);
    assert_eq!(err["error"], "conflict");
    assert_eq!(err["current_sha"], sha2);
}

#[tokio::test]
async fn strangers_app_is_not_found_never_forbidden() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;

    for (tool, args) in [
        ("get_app", json!({"app": "groceries"})),
        (
            "read_file",
            json!({"app": "groceries", "path": "pages/index.tsx"}),
        ),
        ("list_files", json!({"app": "groceries"})),
        ("history", json!({"app": "groceries"})),
        ("deploy_status", json!({"app": "groceries"})),
        ("app_logs", json!({"app": "groceries"})),
        (
            "commit",
            json!({"app": "groceries", "message": "hijack",
                   "writes": [{"path": "x.txt", "content": "x"}]}),
        ),
        ("share_app", json!({"app": "groceries"})),
    ] {
        let (_, body) = call(&state, Some(&bob), tool, args).await;
        assert_eq!(is_error(&body)["error"], "not_found", "tool {tool}");
    }
    // Bob's own list is empty: the app is invisible, not forbidden.
    let (_, list) = call(&state, Some(&bob), "list_apps", json!({})).await;
    assert_eq!(
        result(&list)["structuredContent"]["apps"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn free_plan_caps_apps_at_one() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "one").await;
    let (_, second) = call(&state, Some(&token), "create_app", json!({"slug": "two"})).await;
    let err = is_error(&second);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "apps_owned");
    assert_eq!(err["limit"], 1);
    assert!(err["user_message"]
        .as_str()
        .unwrap()
        .contains("all 1 app slot on your Outer free plan"));

    let (_, who) = call(&state, Some(&token), "whoami", json!({})).await;
    let who = &result(&who)["structuredContent"];
    assert_eq!(who["plan"], "free");
    assert_eq!(who["usage"]["apps_owned"], 1);
    assert!(who["resets_at"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn free_plan_caps_changes_per_day() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let account_id = crate::apps::ensure_account(&state.pool, "ana@example.com")
        .await
        .unwrap();
    sqlx::query("INSERT INTO usage_day (userId, day, code_changes) VALUES (?1, date('now'), 49)")
        .bind(&account_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (_, fiftieth) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "fifty",
               "writes": [{"path": "n.txt", "content": "n"}]}),
    )
    .await;
    assert!(result(&fiftieth).get("isError").is_none());

    let (_, over) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "fifty-one",
               "writes": [{"path": "m.txt", "content": "m"}]}),
    )
    .await;
    let err = is_error(&over);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "code_changes_per_day");
    assert_eq!(err["limit"], 50);
    assert_eq!(err["used"], 50);
    assert!(err["resets_at"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn blocked_and_failed_deploys_are_results_not_errors() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    state.deploys.set_fake(FakeOutcome::blocked());
    let (_, blocked) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "profane",
               "writes": [{"path": "p.txt", "content": "p"}]}),
    )
    .await;
    let deploy = &result(&blocked)["structuredContent"]["deploy"];
    assert!(result(&blocked).get("isError").is_none());
    assert_eq!(deploy["status"], "blocked");
    assert_eq!(deploy["reason"], "profanity");
    assert_eq!(deploy["findings"].as_array().unwrap().len(), 1);
    assert!(deploy["user_message"]
        .as_str()
        .unwrap()
        .contains("strong profanity"));

    let (_, status) = call(
        &state,
        Some(&token),
        "deploy_status",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(result(&status)["structuredContent"]["status"], "blocked");

    let (_, log) = call(
        &state,
        Some(&token),
        "deploy_log",
        json!({"app": "groceries"}),
    )
    .await;
    assert!(result(&log)["content"].as_array().unwrap()[1]["text"]
        .as_str()
        .unwrap()
        .starts_with("The following was produced by your app and its visitors."));

    state.deploys.set_fake(FakeOutcome::failed());
    let (_, failed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "broken",
               "writes": [{"path": "q.txt", "content": "q"}]}),
    )
    .await;
    let deploy = &result(&failed)["structuredContent"]["deploy"];
    assert_eq!(deploy["status"], "failed");
    assert_eq!(deploy["failed_step"], "build");
    assert!(deploy["log_tail"]
        .as_str()
        .unwrap()
        .contains("fake deploy ok"));
    // The failed log tail rides behind the notice as its own block.
    let blocks = result(&failed)["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert!(blocks[1]["text"]
        .as_str()
        .unwrap()
        .contains("fake deploy ok"));
}

#[tokio::test]
async fn rollback_redeploys_an_earlier_success() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let created = create_app(&state, &token, "groceries").await;
    let sha1 = created["deploy"]["sha"].as_str().unwrap().to_string();
    let (_, second) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "two",
               "writes": [{"path": "a.txt", "content": "a"}]}),
    )
    .await;
    let sha2 = result(&second)["structuredContent"]["commit"]["sha"]
        .as_str()
        .unwrap()
        .to_string();

    // A short sha, as agents quote it from history, names the target.
    let (_, rolled) = call(
        &state,
        Some(&token),
        "rollback",
        json!({"app": "groceries", "sha": &sha1[..8]}),
    )
    .await;
    let rolled = &result(&rolled)["structuredContent"];
    assert_eq!(rolled["deploy"]["status"], "success");
    assert_ne!(rolled["commit"]["sha"], sha2);

    let (_, history) = call(&state, Some(&token), "history", json!({"app": "groceries"})).await;
    assert_eq!(
        result(&history)["structuredContent"]["commits"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // Unknown shas, and shas that never deployed cleanly, are refused.
    let (_, unknown) = call(
        &state,
        Some(&token),
        "rollback",
        json!({"app": "groceries", "sha": "0123456789abcdef0123456789abcdef01234567"}),
    )
    .await;
    assert_eq!(is_error(&unknown)["error"], "invalid_argument");

    state.deploys.set_fake(FakeOutcome::blocked());
    let (_, bad) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "blocked",
               "writes": [{"path": "b.txt", "content": "b"}]}),
    )
    .await;
    let blocked_sha = result(&bad)["structuredContent"]["deploy"]["sha"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, refused) = call(
        &state,
        Some(&token),
        "rollback",
        json!({"app": "groceries", "sha": blocked_sha}),
    )
    .await;
    assert_eq!(is_error(&refused)["error"], "invalid_argument");
}

#[tokio::test]
async fn slow_deploys_return_in_progress() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    state.deploys.set_fake(FakeOutcome::slow());

    // An awaited call past the tool_wait cap returns the in-progress shape.
    let (_, creating) = call(&state, Some(&token), "create_app", json!({"slug": "slow"})).await;
    let deploy = &result(&creating)["structuredContent"]["deploy"];
    assert_eq!(deploy["status"], "building");
    assert_eq!(deploy["next"], "call deploy_status with wait: true");

    // The row doesn't exist yet, so polling also reports in-progress.
    let (_, status) = call(
        &state,
        Some(&token),
        "deploy_status",
        json!({"app": "slow", "wait": true}),
    )
    .await;
    assert_eq!(result(&status)["structuredContent"]["status"], "building");
}

#[tokio::test]
async fn unawaited_calls_return_and_poll_resolves() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let (_, creating) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "bg", "wait": false}),
    )
    .await;
    assert_eq!(
        result(&creating)["structuredContent"]["deploy"]["status"],
        "building"
    );
    // The background task finishes the deploy; polling sees success.
    let mut last = Value::Null;
    for _ in 0..50 {
        let (_, status) = call(&state, Some(&token), "deploy_status", json!({"app": "bg"})).await;
        last = result(&status)["structuredContent"].clone();
        if last["status"] == "success" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(last["status"], "success");
    assert!(last["url"].as_str().unwrap().contains("bg"));
}

#[tokio::test]
async fn otp_sign_in_round_trip_then_account_tools() {
    let (state, codes) = setup().await;
    // No token yet: the full OTP fallback through the tools.
    let (_, sent) = call(&state, None, "sign_in", json!({"email": "New@Example.com"})).await;
    assert_eq!(result(&sent)["structuredContent"]["sent"], true);
    let code = auth_ts::captured(&codes, "new@example.com");

    // First use without accept_terms is refused — and the spent code is
    // gone, so sign in again for a fresh one.
    let (_, denied) = call(
        &state,
        None,
        "verify_code",
        json!({"email": "new@example.com", "code": code, "accept_terms": false}),
    )
    .await;
    assert_eq!(is_error(&denied)["error"], "invalid_argument");

    let (_, resent) = call(&state, None, "sign_in", json!({"email": "new@example.com"})).await;
    assert_eq!(result(&resent)["structuredContent"]["sent"], true);
    let code = auth_ts::captured(&codes, "new@example.com");

    let (_, verified) = call(
        &state,
        None,
        "verify_code",
        json!({"email": "new@example.com", "code": code, "accept_terms": true,
               "key_name": "laptop"}),
    )
    .await;
    let verified = &result(&verified)["structuredContent"];
    let key = verified["api_key"].as_str().unwrap().to_string();
    assert!(key.starts_with("outer_"));
    assert_eq!(verified["account"]["email"], "new@example.com");
    assert!(verified["mcp_url"].as_str().unwrap().ends_with("/mcp"));

    let (_, who) = call(&state, Some(&key), "whoami", json!({})).await;
    assert_eq!(
        result(&who)["structuredContent"]["email"],
        "new@example.com"
    );

    let (_, minted) = call(
        &state,
        Some(&key),
        "create_api_key",
        json!({"name": "phone"}),
    )
    .await;
    let prefix = result(&minted)["structuredContent"]["prefix"]
        .as_str()
        .unwrap()
        .to_string();

    let (_, listed) = call(&state, Some(&key), "list_api_keys", json!({})).await;
    let keys = result(&listed)["structuredContent"]["keys"]
        .as_array()
        .unwrap();
    assert!(keys.iter().any(|k| k["prefix"] == prefix));

    let (_, revoked) = call(
        &state,
        Some(&key),
        "revoke_api_key",
        json!({"prefix": prefix}),
    )
    .await;
    assert_eq!(result(&revoked)["structuredContent"]["revoked"], true);

    let (_, gone) = call(
        &state,
        Some(&key),
        "revoke_api_key",
        json!({"prefix": prefix}),
    )
    .await;
    assert_eq!(is_error(&gone)["error"], "not_found");

    let (_, conns) = call(&state, Some(&key), "list_connections", json!({})).await;
    assert_eq!(
        result(&conns)["structuredContent"]["connections"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let (_, no_conn) = call(&state, Some(&key), "revoke_connection", json!({"id": "x"})).await;
    assert_eq!(is_error(&no_conn)["error"], "not_found");
}

#[tokio::test]
async fn app_logs_empty_ring_and_method_not_allowed() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    // No fleet ever ran, so the ring is empty — still a 200 with the shape.
    let (_, logs) = call(
        &state,
        Some(&token),
        "app_logs",
        json!({"app": "groceries"}),
    )
    .await;
    let logs = result(&logs);
    assert_eq!(
        logs["structuredContent"]["lines"].as_array().unwrap().len(),
        0
    );

    // GET on /mcp is 405. (Blume's own /llms.txt replaces the
    // registry-rendered one; the site build checks it instead.)
    let app = crate::http::router(app_state(&state));
    use tower::ServiceExt as _;
    let get = app
        .oneshot(Request::get("/mcp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::METHOD_NOT_ALLOWED);
}
#[tokio::test]
async fn app_errors_groups_scope_and_shape() {
    use crate::fleet::supervisor::{LogEntry, LogStream};
    use crate::observe::logs as obs;

    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;
    let app_id = app_id(&state, "groceries").await;

    // Seed the log stream the way a fleet would write it, then flush the
    // way the sweep does.
    let lines = [
        "serving on :8080",
        "Error: kaboom user-1",
        "    at handler (file:///app/index.js:10:5)",
        "TypeError: bad id 42",
    ];
    let entries: Vec<LogEntry> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| LogEntry {
            seq: i as u64,
            ts: 1_700_000_000 + i as i64,
            stream: LogStream::Stderr,
            line: line.to_string(),
        })
        .collect();
    obs::flush_fleet(&state.pool, &app_id, &entries)
        .await
        .unwrap();

    // One proxy 5xx in the trailing hour.
    let now = crate::apps::now_ms();
    sqlx::query(
        "INSERT INTO metric_minute (app_id, minute, requests, s4xx, s5xx, latency_hist, bytes_out, visitors) \
         VALUES (?1, ?2, 10, 0, 1, '', 0, 0)",
    )
    .bind(&app_id)
    .bind(now / 60_000 * 60_000)
    .execute(&state.pool)
    .await
    .unwrap();

    // A stranger's slug answers not_found, exactly like a missing one.
    let (_, bob_body) = call(
        &state,
        Some(&bob),
        "app_errors",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(is_error(&bob_body)["error"], "not_found");
    // An unknown status is the caller's bug.
    let (_, bad) = call(
        &state,
        Some(&ana),
        "app_errors",
        json!({"app": "groceries", "status": "bogus"}),
    )
    .await;
    assert_eq!(is_error(&bad)["error"], "invalid_argument");

    let (_, out) = call(
        &state,
        Some(&ana),
        "app_errors",
        json!({"app": "groceries"}),
    )
    .await;
    let out = result(&out);
    let structured = &out["structuredContent"];
    assert_eq!(structured["s5xx_last_hour"], 1);
    let found = structured["issues"].as_array().unwrap();
    assert_eq!(found.len(), 2);
    // Newest first.
    assert!(found[0]["last_seen"].as_i64().unwrap() >= found[1]["last_seen"].as_i64().unwrap());
    let kaboom = found
        .iter()
        .find(|item| item["message"].as_str().unwrap().contains("kaboom"))
        .unwrap();
    assert_eq!(kaboom["count"], 1);
    assert_eq!(kaboom["status"], "open");
    assert_eq!(kaboom["fingerprint"].as_str().unwrap().len(), 64);
    assert!(kaboom["frame"].as_str().unwrap().contains("at handler"));
    assert!(kaboom.get("last_deploy_sha").is_some());
    // Untrusted: the payload rides behind the §8.4 notice as its own block.
    let blocks = out["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert!(blocks[1]["text"]
        .as_str()
        .unwrap()
        .starts_with("The following was produced by your app and its visitors."));

    // Status filter.
    let (_, open) = call(
        &state,
        Some(&ana),
        "app_errors",
        json!({"app": "groceries", "status": "open"}),
    )
    .await;
    assert_eq!(
        result(&open)["structuredContent"]["issues"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let (_, resolved) = call(
        &state,
        Some(&ana),
        "app_errors",
        json!({"app": "groceries", "status": "resolved"}),
    )
    .await;
    assert!(result(&resolved)["structuredContent"]["issues"]
        .as_array()
        .unwrap()
        .is_empty());

    // The call lands in the console action log.
    let actions = crate::mcp::actions::list(&state.pool, &app_id, None, 10)
        .await
        .unwrap();
    assert!(actions
        .iter()
        .any(|action| action.tool == "app_errors" && action.summary == "app_errors"));
}

#[tokio::test]
async fn concurrent_creates_race_app_quota() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    // Both calls pass the early quota check at 0 owned apps; the in-lock
    // recheck in `apps::create` lets exactly one through.
    let (a, b) = tokio::join!(
        call(&state, Some(&token), "create_app", json!({"slug": "aa"})),
        call(&state, Some(&token), "create_app", json!({"slug": "bb"})),
    );
    let bodies = [&a.1, &b.1];
    let wins = bodies
        .iter()
        .filter(|body| body["result"].get("isError").is_none())
        .count();
    assert_eq!(wins, 1, "exactly one concurrent create wins the quota race");
    let loser = bodies
        .iter()
        .find(|body| body["result"].get("isError").is_some())
        .unwrap();
    assert_eq!(
        loser["result"]["structuredContent"]["error"],
        "quota_exceeded"
    );
    assert_eq!(loser["result"]["structuredContent"]["quota"], "apps_owned");
    let (_, who) = call(&state, Some(&token), "whoami", json!({})).await;
    assert_eq!(result(&who)["structuredContent"]["usage"]["apps_owned"], 1);
}

#[tokio::test]
async fn commit_quota_claim_is_atomic_and_refunded() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let account_id = crate::apps::ensure_account(&state.pool, "ana@example.com")
        .await
        .unwrap();
    async fn used(state: &McpState, account_id: &str) -> i64 {
        sqlx::query_scalar("SELECT code_changes FROM usage_day WHERE userId = ?1")
            .bind(account_id)
            .fetch_one(&state.pool)
            .await
            .unwrap()
    }
    sqlx::query("INSERT INTO usage_day (userId, day, code_changes) VALUES (?1, date('now'), 10)")
        .bind(&account_id)
        .execute(&state.pool)
        .await
        .unwrap();
    // A malformed base never claims: validation runs before the UPSERT.
    let (_, bad_shape) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "x", "base": "zzzz",
               "writes": [{"path": "z.txt", "content": "z"}]}),
    )
    .await;
    assert_eq!(is_error(&bad_shape)["error"], "invalid_argument");
    assert_eq!(used(&state, &account_id).await, 10);
    // A well-formed but moved base claims, fails in git, and refunds.
    let (_, unknown) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "x", "base": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
               "writes": [{"path": "z.txt", "content": "z"}]}),
    )
    .await;
    assert_eq!(is_error(&unknown)["error"], "conflict");
    assert_eq!(used(&state, &account_id).await, 10);
    // Racing at the boundary: exactly one claim fits under 50.
    sqlx::query("UPDATE usage_day SET code_changes = 49 WHERE userId = ?1")
        .bind(&account_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        call(
            &state,
            Some(&token),
            "commit",
            json!({"app": "groceries", "message": "r1", "writes": [{"path": "r1.txt", "content": "1"}]})
        ),
        call(
            &state,
            Some(&token),
            "commit",
            json!({"app": "groceries", "message": "r2", "writes": [{"path": "r2.txt", "content": "2"}]})
        ),
    );
    let bodies = [&a.1, &b.1];
    assert_eq!(
        bodies
            .iter()
            .filter(|b| b["result"].get("isError").is_none())
            .count(),
        1
    );
    let loser = bodies
        .iter()
        .find(|b| b["result"].get("isError").is_some())
        .unwrap();
    assert_eq!(
        loser["result"]["structuredContent"]["error"],
        "quota_exceeded"
    );
    assert_eq!(used(&state, &account_id).await, 50);
}

#[tokio::test]
async fn rollback_in_flight_rejects_and_coalesces() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let created = create_app(&state, &token, "groceries").await;
    let sha1 = created["deploy"]["sha"].as_str().unwrap().to_string();
    // A second commit, so the seed is a legal (earlier) rollback target.
    let (_, second) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "two",
               "writes": [{"path": "a.txt", "content": "a"}]}),
    )
    .await;
    assert!(result(&second).get("isError").is_none());
    state.deploys.set_fake(FakeOutcome::slow());
    // One rollback proceeds (past the cap it reports building); the second
    // fails fast instead of queueing another build.
    let (a, b) = tokio::join!(
        call(
            &state,
            Some(&token),
            "rollback",
            json!({"app": "groceries", "sha": sha1})
        ),
        call(
            &state,
            Some(&token),
            "rollback",
            json!({"app": "groceries", "sha": sha1})
        ),
    );
    let bodies = [&a.1, &b.1];
    let limited: Vec<_> = bodies
        .iter()
        .filter(|b| b["result"]["structuredContent"]["error"] == "rate_limited")
        .collect();
    assert_eq!(limited.len(), 1, "exactly one rollback is rate-limited");
    assert_eq!(
        limited[0]["result"]["structuredContent"]["retry_after_s"], 30,
        "the in-flight refusal names its backoff"
    );
    // A deploy row stuck non-terminal coalesces: no new commit, no budget
    // spent, just the running deploy reported. (The slow fake only writes
    // its row at completion, so the stuck row is inserted directly.)
    let app_id = crate::apps::get_by_slug(&state.pool, "groceries")
        .await
        .unwrap()
        .unwrap()
        .id;
    sqlx::query(
        "INSERT INTO deploy (id, app_id, sha, status, created_at) VALUES (?1, ?2, ?3, 'building', ?4)",
    )
    .bind("stuck-deploy")
    .bind(&app_id)
    .bind(&sha1)
    .bind(crate::apps::now_ms())
    .execute(&state.pool)
    .await
    .unwrap();
    let (_, history_before) =
        call(&state, Some(&token), "history", json!({"app": "groceries"})).await;
    let n_before = result(&history_before)["structuredContent"]["commits"]
        .as_array()
        .unwrap()
        .len();
    let (_, coalesced) = call(
        &state,
        Some(&token),
        "rollback",
        json!({"app": "groceries", "sha": sha1, "wait": false}),
    )
    .await;
    let structured = &result(&coalesced)["structuredContent"];
    assert_eq!(structured["deploy"]["status"], "building");
    assert!(result(&coalesced)["content"].as_array().unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .contains("already running"));
    let (_, history_after) =
        call(&state, Some(&token), "history", json!({"app": "groceries"})).await;
    assert_eq!(
        result(&history_after)["structuredContent"]["commits"]
            .as_array()
            .unwrap()
            .len(),
        n_before,
        "coalescing must not create a commit"
    );
    let app_id = crate::apps::get_by_slug(&state.pool, "groceries")
        .await
        .unwrap()
        .unwrap()
        .id;
    let rollbacks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rollback_day WHERE app_id = ?1")
        .bind(&app_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(rollbacks, 1, "only the real rollback spends budget");
}

#[tokio::test]
async fn rollback_budget_exhausts_with_retry_after() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let created = create_app(&state, &token, "groceries").await;
    let sha1 = created["deploy"]["sha"].as_str().unwrap().to_string();
    let (_, second) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "two",
               "writes": [{"path": "a.txt", "content": "a"}]}),
    )
    .await;
    assert!(result(&second).get("isError").is_none());
    let app_id = crate::apps::get_by_slug(&state.pool, "groceries")
        .await
        .unwrap()
        .unwrap()
        .id;
    sqlx::query("INSERT INTO rollback_day (app_id, day, count) VALUES (?1, date('now'), 20)")
        .bind(&app_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let (_, refused) = call(
        &state,
        Some(&token),
        "rollback",
        json!({"app": "groceries", "sha": sha1}),
    )
    .await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "rate_limited");
    assert!(
        err["retry_after_s"].as_u64().unwrap_or(0) > 0,
        "budget exhaustion names its backoff"
    );
    let (_, history) = call(&state, Some(&token), "history", json!({"app": "groceries"})).await;
    assert_eq!(
        result(&history)["structuredContent"]["commits"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "a refused rollback creates nothing"
    );
}
#[tokio::test]
async fn git_tools_reject_bad_revs_without_server_paths() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    for (tool, args) in [
        (
            "list_files",
            json!({"app": "groceries", "sha": "not-a-sha!!"}),
        ),
        (
            "read_file",
            json!({"app": "groceries", "path": "pages/index.tsx", "sha": "../../etc"}),
        ),
        ("show_commit", json!({"app": "groceries", "sha": "-5"})),
        ("history", json!({"app": "groceries", "path": "../x"})),
        (
            "commit",
            json!({"app": "groceries", "message": "x", "base": "--help",
                   "writes": [{"path": "z.txt", "content": "z"}]}),
        ),
        ("rollback", json!({"app": "groceries", "sha": "0xzz"})),
    ] {
        let (_, body) = call(&state, Some(&token), tool, args).await;
        let err = is_error(&body);
        assert_eq!(err["error"], "invalid_argument", "tool {tool}");
        let message = err["message"].as_str().unwrap_or("");
        assert!(
            !message.contains("--git-dir") && !message.contains("/tmp"),
            "tool {tool} leaked a server path: {message}"
        );
    }
}

#[tokio::test]
async fn untrusted_notice_covers_findings_and_scores_round() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let created = create_app(&state, &token, "groceries").await;
    let sha1 = created["deploy"]["sha"].as_str().unwrap().to_string();
    state.deploys.set_fake(FakeOutcome::blocked());
    let (_, blocked) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "profane",
               "writes": [{"path": "p.txt", "content": "p"}]}),
    )
    .await;
    let structured = &result(&blocked)["structuredContent"];
    assert_eq!(structured["deploy"]["status"], "blocked");
    assert_eq!(structured["deploy"]["findings"][0]["score"], 0.93);
    let blocks = result(&blocked)["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert!(blocks[1]["text"]
        .as_str()
        .unwrap()
        .starts_with("The following was produced by your app and its visitors."));
    assert!(blocks[1]["text"]
        .as_str()
        .unwrap()
        .contains("pages/index.tsx"));
    // `deploy_status` carries the same payload, so it carries the notice too.
    let (_, status) = call(
        &state,
        Some(&token),
        "deploy_status",
        json!({"app": "groceries"}),
    )
    .await;
    let status_blocks = result(&status)["content"].as_array().unwrap();
    assert_eq!(status_blocks.len(), 2);
    assert!(status_blocks[1]["text"]
        .as_str()
        .unwrap()
        .starts_with("The following was produced by your app and its visitors."));
    // A blocked rollback reports its findings behind the notice as well.
    let (_, rolled) = call(
        &state,
        Some(&token),
        "rollback",
        json!({"app": "groceries", "sha": sha1}),
    )
    .await;
    let rolled_blocks = result(&rolled)["content"].as_array().unwrap();
    assert_eq!(rolled_blocks.len(), 2);
    assert!(rolled_blocks[1]["text"]
        .as_str()
        .unwrap()
        .contains("pages/index.tsx"));
}

async fn app_id(state: &McpState, slug: &str) -> String {
    sqlx::query_scalar::<_, String>("SELECT id FROM app WHERE slug = ?1")
        .bind(slug)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn delete_app_refuses_confirm_mismatch_and_deletes_nothing() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    let (_, body) = call(
        &state,
        Some(&token),
        "delete_app",
        json!({"app": "groceries", "confirm": "groceries-x"}),
    )
    .await;
    let err = is_error(&body);
    assert_eq!(err["error"], "invalid_argument");
    assert!(err["user_message"]
        .as_str()
        .unwrap()
        .contains("confirm \"groceries\""));

    // Nothing deleted: the app still lists, its bucket prefix still holds
    // the manifest and bundle, and the row still exists.
    let (_, list) = call(&state, Some(&token), "list_apps", json!({})).await;
    assert_eq!(
        result(&list)["structuredContent"]["apps"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(!state.store.list("git/groceries/").await.unwrap().is_empty());
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app WHERE slug = 'groceries'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn delete_app_unknown_and_foreign_slugs_are_not_found() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;

    // Missing entirely, even with a matching confirm.
    let (_, missing) = call(
        &state,
        Some(&ana),
        "delete_app",
        json!({"app": "nope", "confirm": "nope"}),
    )
    .await;
    assert_eq!(is_error(&missing)["error"], "not_found");
    // Foreign: Bob's correct confirm still answers exactly like missing.
    let (_, foreign) = call(
        &state,
        Some(&bob),
        "delete_app",
        json!({"app": "groceries", "confirm": "groceries"}),
    )
    .await;
    assert_eq!(is_error(&foreign)["error"], "not_found");
}

#[tokio::test]
async fn delete_app_removes_everything_and_quarantines_the_slug() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;
    create_app(&state, &bob, "bobapp").await;
    let groceries_id = app_id(&state, "groceries").await;

    // Fake deploys write no bucket objects, so seed both prefixes by hand.
    state
        .store
        .put("fleets/groceries/deploy/current.json", b"{}".to_vec())
        .await
        .unwrap();
    state
        .store
        .put("fleets/bobapp/deploy/current.json", b"{}".to_vec())
        .await
        .unwrap();
    // Every app-scoped table with no cascade gets a row (deploys, secrets
    // and cron rows already exist from create/fake-deploy; add the rest).
    for sql in [
        "INSERT INTO metric_minute (app_id, minute, requests, s4xx, s5xx, latency_hist, bytes_out, visitors) VALUES (?1, 0, 1, 0, 0, '', 0, 0)",
        "INSERT INTO path_hour (app_id, hour, path, requests, s5xx) VALUES (?1, 0, '/', 1, 0)",
        "INSERT INTO visitor_day (app_id, day, visitors) VALUES (?1, 0, 1)",
        "INSERT INTO app_log (app_id, seq, ts, stream, line) VALUES (?1, 0, 0, 'stdout', 'x')",
        "INSERT INTO error_issue (app_id, fingerprint, message, count, first_seen, last_seen) VALUES (?1, 'f', 'm', 1, 0, 0)",
        "INSERT INTO moderation_text (app_id, text_hash, verdict, checked_at) VALUES (?1, 'h', 'clear', 0)",
        "INSERT INTO moderation_check (id, app_id, stage, created_at) VALUES ('m1', ?1, 'gate', 0)",
        "INSERT INTO rollback_day (app_id, day, count) VALUES (?1, '2026-10-23', 1)",
    ] {
        sqlx::query(sql)
            .bind(&groceries_id)
            .execute(&state.pool)
            .await
            .unwrap();
    }
    // Local state the pipeline would have left behind.
    let data = &state.delete_cfg.data_dir;
    tokio::fs::create_dir_all(data.join("work/groceries"))
        .await
        .unwrap();
    tokio::fs::write(data.join("work/groceries.index"), b"i")
        .await
        .unwrap();
    tokio::fs::write(data.join("work/groceries.install-hash"), b"h")
        .await
        .unwrap();
    tokio::fs::create_dir_all(data.join("cache/groceries"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(data.join("fleets/groceries"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(data.join("secret-sidecars"))
        .await
        .unwrap();
    tokio::fs::write(data.join("secret-sidecars/groceries.orig"), b"o")
        .await
        .unwrap();
    tokio::fs::write(data.join("secret-sidecars/groceries.target"), b"0\ncfg")
        .await
        .unwrap();

    let (_, body) = call(
        &state,
        Some(&ana),
        "delete_app",
        json!({"app": "groceries", "confirm": "groceries"}),
    )
    .await;
    let out = &result(&body)["structuredContent"];
    assert_eq!(out["deleted"], true);
    assert_eq!(out["slug"], "groceries");
    let user_message = out["user_message"].as_str().unwrap();
    assert_eq!(
        user_message,
        "groceries.outer.localhost is deleted. The name stays reserved for 30 days."
    );
    assert_eq!(
        result(&body)["content"][0]["text"].as_str().unwrap(),
        user_message
    );
    let quarantined_until = out["quarantined_until"].as_i64().unwrap();
    assert!(quarantined_until > crate::apps::now_ms());

    // Bucket: both prefixes empty, the sibling app untouched.
    assert!(state.store.list("git/groceries/").await.unwrap().is_empty());
    assert!(state
        .store
        .list("fleets/groceries/")
        .await
        .unwrap()
        .is_empty());
    assert!(!state.store.list("fleets/bobapp/").await.unwrap().is_empty());
    assert!(!state.store.list("git/bobapp/").await.unwrap().is_empty());
    // Local: worktree, sidecars, caches, fleet state and mirror gone.
    assert!(!data.join("work/groceries").exists());
    assert!(!data.join("work/groceries.index").exists());
    assert!(!data.join("work/groceries.install-hash").exists());
    assert!(!data.join("cache/groceries").exists());
    assert!(!data.join("fleets/groceries").exists());
    assert!(!data.join("secret-sidecars/groceries.orig").exists());
    assert!(!data.join("secret-sidecars/groceries.target").exists());
    assert!(!state.git.repos_dir.join("groceries.git").exists());
    // Rows: the app, every child table, and the quarantine row to prove it.
    for table in [
        "app",
        "deploy",
        "app_secret",
        "app_domain",
        "app_collaborator",
        "moderation_text",
        "moderation_check",
        "app_cron",
        "rollback_day",
        "path_hour",
        "visitor_day",
        "app_log",
        "error_issue",
    ] {
        // `table` comes from the literal list above, never agent input.
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE {} = ?1",
            if table == "app" { "id" } else { "app_id" },
        )))
        .bind(&groceries_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "table {table}");
    }
    let until: i64 =
        sqlx::query_scalar("SELECT until FROM slug_quarantine WHERE slug = 'groceries'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(until, quarantined_until);

    // The slug is quarantined: recreating it is refused.
    let (_, again) = call(
        &state,
        Some(&ana),
        "create_app",
        json!({"slug": "groceries"}),
    )
    .await;
    let err = is_error(&again);
    assert_eq!(err["error"], "invalid_argument");
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("taken or quarantined"));
    // And Bob's app still serves its summary.
    let (_, kept) = call(&state, Some(&bob), "get_app", json!({"app": "bobapp"})).await;
    assert_eq!(result(&kept)["structuredContent"]["slug"], "bobapp");
}
#[tokio::test]
async fn create_app_wipes_abandoned_local_state_for_an_unclaimed_slug() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    // A crashed delete's orphans under a slug nobody owns: stale files
    // under every local path the slug will own.
    let data = &state.delete_cfg.data_dir;
    tokio::fs::create_dir_all(data.join("work/fig"))
        .await
        .unwrap();
    tokio::fs::write(data.join("work/fig/leftover"), b"stale")
        .await
        .unwrap();
    tokio::fs::write(data.join("work/fig.index"), b"stale")
        .await
        .unwrap();
    tokio::fs::write(data.join("work/fig.install-hash"), b"stale")
        .await
        .unwrap();
    tokio::fs::create_dir_all(data.join("cache/fig"))
        .await
        .unwrap();
    tokio::fs::write(data.join("cache/fig/leftover"), b"stale")
        .await
        .unwrap();
    tokio::fs::create_dir_all(data.join("fleets/fig"))
        .await
        .unwrap();
    tokio::fs::write(data.join("fleets/fig/leftover"), b"stale")
        .await
        .unwrap();
    tokio::fs::create_dir_all(data.join("secret-sidecars"))
        .await
        .unwrap();
    tokio::fs::write(data.join("secret-sidecars/fig.orig"), b"stale")
        .await
        .unwrap();
    tokio::fs::write(data.join("secret-sidecars/fig.target"), b"stale")
        .await
        .unwrap();
    tokio::fs::create_dir_all(state.git.repos_dir.join("fig.git"))
        .await
        .unwrap();
    tokio::fs::write(state.git.repos_dir.join("fig.git/leftover"), b"stale")
        .await
        .unwrap();
    state.sync.set_applied_seq("fig", 99).await;

    let (_, body) = call(&state, Some(&ana), "create_app", json!({"slug": "fig"})).await;
    let structured = &result(&body)["structuredContent"];
    assert_eq!(structured["app"]["slug"], "fig");
    // Every stale marker is gone; the mirror is the fresh seed, not the
    // orphan, and the stale seq went with the orphans.
    for stale in [
        "work/fig/leftover",
        "work/fig.index",
        "work/fig.install-hash",
        "cache/fig/leftover",
        "fleets/fig/leftover",
        "secret-sidecars/fig.orig",
        "secret-sidecars/fig.target",
    ] {
        assert!(!data.join(stale).exists(), "{stale} survived the claim");
    }
    assert!(!state.git.repos_dir.join("fig.git/leftover").exists());
    assert!(state.git.repos_dir.join("fig.git").exists());
    assert_eq!(state.sync.applied_seq("fig").await, Some(1));
}

#[tokio::test]
async fn delete_app_retry_completes_after_a_partial_first_attempt() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    state
        .store
        .put("fleets/groceries/deploy/current.json", b"{}".to_vec())
        .await
        .unwrap();

    // A first attempt that crashed after the bucket and local steps but
    // before the row delete: redo those steps by hand, then the retry must
    // finish the job (every step tolerates already-gone state).
    state
        .store
        .delete_prefix("fleets/groceries/")
        .await
        .unwrap();
    state.store.delete_prefix("git/groceries/").await.unwrap();
    let data = &state.delete_cfg.data_dir;
    let _ = tokio::fs::remove_dir_all(data.join("work/groceries")).await;

    let (_, body) = call(
        &state,
        Some(&token),
        "delete_app",
        json!({"app": "groceries", "confirm": "groceries"}),
    )
    .await;
    let out = &result(&body)["structuredContent"];
    assert_eq!(out["deleted"], true);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app WHERE slug = 'groceries'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn delete_app_shows_as_destructive_in_tools_list() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let (_, _, authed) = post(
        &state,
        Some(&token),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
    )
    .await;
    let tools = result(&authed)["tools"].as_array().unwrap();
    let delete = tools
        .iter()
        .find(|t| t["name"] == "delete_app")
        .expect("delete_app is listed");
    assert_eq!(delete["annotations"]["destructiveHint"], true);
    assert_eq!(delete["annotations"]["readOnlyHint"], false);
}

async fn action_rows(state: &McpState, slug: &str) -> Vec<crate::mcp::actions::ActionRow> {
    let id = app_id(state, slug).await;
    crate::mcp::actions::list(&state.pool, &id, None, 50)
        .await
        .unwrap()
}

#[tokio::test]
async fn agent_action_commit_logged_with_redacted_summary() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    let marker = "UNIQUE_BODY_MARKER_9Z8X7Y";
    let body = format!("<h1>{marker}</h1>");
    let (_, commit) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "Add a header line",
               "writes": [{"path": "pages/about.tsx", "content": body}]}),
    )
    .await;
    assert_eq!(
        result(&commit)["structuredContent"]["deploy"]["status"],
        "success"
    );

    let rows = action_rows(&state, "groceries").await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    // Newest first: the commit, then the create_app.
    assert_eq!(rows[0].tool, "commit");
    assert_eq!(rows[0].outcome, "success");
    assert_eq!(rows[0].error_code, None);
    assert!(
        rows[0].summary.contains("pages/about.tsx"),
        "{}",
        rows[0].summary
    );
    assert!(
        rows[0].summary.contains("Add a header line"),
        "{}",
        rows[0].summary
    );
    assert!(
        rows[0].summary.contains("1 writes ("),
        "{}",
        rows[0].summary
    );
    assert!(
        !rows[0].summary.contains(marker),
        "file bodies never land in the log: {}",
        rows[0].summary
    );
    assert_eq!(rows[1].tool, "create_app");
    assert_eq!(rows[1].outcome, "success");
    // API-key callers label with the key name (or its prefix when unnamed).
    assert!(
        rows[0].client_label.starts_with("API key"),
        "{}",
        rows[0].client_label
    );
    let key_ids: Vec<Option<String>> =
        sqlx::query_scalar("SELECT apikey_id FROM agent_action ORDER BY created_at")
            .fetch_all(&state.pool)
            .await
            .unwrap();
    assert_eq!(key_ids.len(), 2);
    assert!(key_ids.iter().all(|id| id.is_some()), "{key_ids:?}");
    let grants: Vec<Option<String>> =
        sqlx::query_scalar("SELECT consent_id FROM agent_action ORDER BY created_at")
            .fetch_all(&state.pool)
            .await
            .unwrap();
    assert!(grants.iter().all(|id| id.is_none()), "{grants:?}");
}

#[tokio::test]
async fn agent_action_foreign_app_writes_nothing_and_failures_log_error() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;

    // A stranger's slug answers not_found and writes nothing.
    let (_, foreign) = call(&state, Some(&bob), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(is_error(&foreign)["error"], "not_found");
    assert_eq!(action_rows(&state, "groceries").await.len(), 1);

    // A failing call on the caller's own app logs outcome=error with the code.
    let (_, bad) = call(
        &state,
        Some(&ana),
        "set_visibility",
        json!({"app": "groceries", "visibility": "everyone"}),
    )
    .await;
    assert_eq!(is_error(&bad)["error"], "invalid_argument");
    let rows = action_rows(&state, "groceries").await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].tool, "set_visibility");
    assert_eq!(rows[0].outcome, "error");
    assert_eq!(rows[0].error_code.as_deref(), Some("invalid_argument"));
    assert!(rows[0].summary.contains("everyone"), "{}", rows[0].summary);

    // Arg-shape failures (JSON-RPC -32602) never reach a handler: no row.
    let (_, shape) = call(&state, Some(&ana), "commit", json!({})).await;
    assert_eq!(shape["error"]["code"], -32602);
    assert_eq!(action_rows(&state, "groceries").await.len(), 2);

    // A refused call's arguments were never length-checked: the stored
    // summary is still bounded.
    let huge = "p".repeat(100_000);
    let (_, long) = call(
        &state,
        Some(&ana),
        "read_file",
        json!({"app": "groceries", "path": huge}),
    )
    .await;
    assert_eq!(is_error(&long)["error"], "invalid_argument");
    let rows = action_rows(&state, "groceries").await;
    assert_eq!(rows[0].tool, "read_file");
    assert!(
        rows[0].summary.chars().count() <= 2003,
        "{}",
        rows[0].summary.len()
    );

    // Multi-byte input where the summary keeps a short prefix (sha, deploy
    // id) must not panic the call: the caller still gets its result.
    let (_, odd) = call(
        &state,
        Some(&ana),
        "list_files",
        json!({"app": "groceries", "sha": "aéééééééé"}),
    )
    .await;
    assert!(odd.get("result").is_some(), "{odd}");
    let (_, odd) = call(
        &state,
        Some(&ana),
        "deploy_log",
        json!({"app": "groceries", "deploy_id": "dééééé"}),
    )
    .await;
    assert!(odd.get("result").is_some(), "{odd}");
}

#[tokio::test]
async fn agent_action_oauth_caller_resolves_client_label() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let account_id: String =
        sqlx::query_scalar("SELECT id FROM user WHERE email = 'ana@example.com'")
            .fetch_one(&state.pool)
            .await
            .unwrap();

    let (access, hash) = crate::auth::tokens::new_access_token();
    let now = crate::auth::now_ms();
    sqlx::query(
        "INSERT INTO oauthClient (id, clientId, kind, name, host, redirectUris, verified, createdAt, updatedAt) \
         VALUES ('ocid-1', 'cid-claude', 'dcr', 'Claude', 'claude.ai', '[\"https://claude.ai/cb\"]', 0, ?1, ?1)",
    )
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO oauthConsent (id, clientId, userId, scopes, resources, clientName, createdAt, updatedAt) \
         VALUES ('gid-1', 'cid-claude', ?1, 'apps', 'http://outer.localhost/mcp', 'Claude', ?2, ?2)",
    )
    .bind(&account_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO oauthAccessToken (id, token, clientId, userId, referenceId, scopes, expiresAt, createdAt) \
         VALUES ('oat-1', ?1, 'cid-claude', ?2, 'gid-1', 'apps', ?3, ?4)",
    )
    .bind(&hash)
    .bind(&account_id)
    .bind(now + 3_600_000)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();

    let (_, got) = call(
        &state,
        Some(&access),
        "get_app",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(result(&got)["structuredContent"]["slug"], "groceries");

    let rows = action_rows(&state, "groceries").await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].tool, "get_app");
    assert_eq!(rows[0].client_label, "Claude (claude.ai)");
    let grant_id: Option<String> =
        sqlx::query_scalar("SELECT consent_id FROM agent_action WHERE tool = 'get_app'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(grant_id.as_deref(), Some("gid-1"));
}

#[tokio::test]
async fn agent_action_pagination_prune_and_delete_cascade() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let id = app_id(&state, "groceries").await;
    let account_id: String =
        sqlx::query_scalar("SELECT id FROM user WHERE email = 'ana@example.com'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    sqlx::query("DELETE FROM agent_action")
        .execute(&state.pool)
        .await
        .unwrap();
    for (row_id, tool, ts) in [
        ("a", "get_app", 100),
        ("b", "history", 200),
        ("c", "commit", 300),
    ] {
        sqlx::query(
            "INSERT INTO agent_action (id, app_id, user_id, tool, summary, outcome, duration_ms, created_at) \
             VALUES (?1, ?2, ?3, ?4, 's', 'ok', 1, ?5)",
        )
        .bind(row_id)
        .bind(&id)
        .bind(&account_id)
        .bind(tool)
        .bind(ts)
        .execute(&state.pool)
        .await
        .unwrap();
    }

    // Newest first, cursor pages through the rest.
    let page: Vec<crate::mcp::actions::ActionRow> =
        crate::mcp::actions::list(&state.pool, &id, None, 2)
            .await
            .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].tool, "commit");
    assert_eq!(page[1].tool, "history");
    let rest: Vec<crate::mcp::actions::ActionRow> = crate::mcp::actions::list(
        &state.pool,
        &id,
        Some((page[1].created_at, page[1].id.clone())),
        2,
    )
    .await
    .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].tool, "get_app");

    // Prune keeps rows inside the Free 7-day retention and drops older ones.
    let now = 8 * 86_400_000i64;
    sqlx::query("UPDATE agent_action SET created_at = ?1 WHERE id = 'a'")
        .bind(now - 8 * 86_400_000)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE agent_action SET created_at = ?1 WHERE id IN ('b', 'c')")
        .bind(now - 86_400_000)
        .execute(&state.pool)
        .await
        .unwrap();
    let removed = crate::mcp::actions::prune(&state.pool, now).await.unwrap();
    assert_eq!(removed, 1);
    let kept: Vec<String> = sqlx::query_scalar("SELECT id FROM agent_action ORDER BY id")
        .fetch_all(&state.pool)
        .await
        .unwrap();
    assert_eq!(kept, vec!["b".to_string(), "c".to_string()]);

    // Pro accounts keep 30 days: an 8-day-old row survives, a 31-day one goes.
    sqlx::query("UPDATE user SET plan = 'pro' WHERE id = ?1")
        .bind(&account_id)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE agent_action SET created_at = ?1 WHERE id = 'b'")
        .bind(now - 8 * 86_400_000)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE agent_action SET created_at = ?1 WHERE id = 'c'")
        .bind(now - 31 * 86_400_000)
        .execute(&state.pool)
        .await
        .unwrap();
    let removed = crate::mcp::actions::prune(&state.pool, now).await.unwrap();
    assert_eq!(removed, 1);
    let kept: Vec<String> = sqlx::query_scalar("SELECT id FROM agent_action ORDER BY id")
        .fetch_all(&state.pool)
        .await
        .unwrap();
    assert_eq!(kept, vec!["b".to_string()]);

    // Deleting the app removes its log rows with everything else.
    let (_, deleted) = call(
        &state,
        Some(&token),
        "delete_app",
        json!({"app": "groceries", "confirm": "groceries"}),
    )
    .await;
    assert_eq!(result(&deleted)["structuredContent"]["deleted"], true);
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_action")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// `X-Robots-Tag` and `/robots.txt` change with visibility and the edge
/// caches them: flipping visibility purges the app's host.
#[tokio::test]
async fn set_visibility_purges_the_app_host() {
    let (mut state, _) = setup().await;
    let fake = crate::cloudflare::fake::FakePurge::start().await;
    state.delete_cfg.cache_purge = Some(
        crate::cloudflare::CachePurge::new("zone-1".to_string(), "token-1".to_string())
            .with_api_base(fake.base_url.clone()),
    );
    let ana = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &ana, "groceries").await;
    sqlx::query("UPDATE user SET plan = 'pro' WHERE email = 'ana@example.com'")
        .execute(&state.pool)
        .await
        .unwrap();
    let before = fake.calls();
    let (_, out) = call(
        &state,
        Some(&ana),
        "set_visibility",
        json!({"app": "groceries", "visibility": "public"}),
    )
    .await;
    assert!(out.get("result").is_some(), "{out}");
    assert_eq!(fake.calls(), before + 1);
    let requests = fake.requests.lock().unwrap();
    let host = format!("groceries.{}", state.delete_cfg.apps_domain);
    assert_eq!(requests.last().unwrap().body["hosts"][0], host);
}

#[tokio::test]
async fn secrets_set_list_unset_roundtrip_without_values() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let marker = "SECRET_VALUE_MARKER_9Z8Y7X";
    let (_, set) = call(
        &state,
        Some(&token),
        "set_secret",
        json!({"app": "groceries", "key": "MAIL_KEY", "value": marker}),
    )
    .await;
    assert_eq!(
        result(&set)["structuredContent"],
        json!({"ok": true, "applies": "next deploy"})
    );
    let body = serde_json::to_string(&set).unwrap();
    assert!(!body.contains(marker), "the value never echoes: {body}");
    let (_, list) = call(
        &state,
        Some(&token),
        "list_secrets",
        json!({"app": "groceries"}),
    )
    .await;
    let secrets = result(&list)["structuredContent"]["secrets"].clone();
    assert_eq!(secrets.as_array().unwrap().len(), 1);
    assert_eq!(secrets[0]["name"], "MAIL_KEY");
    assert!(
        secrets[0]
            .get("updated_at")
            .and_then(Value::as_i64)
            .unwrap()
            > 0
    );
    assert!(
        secrets[0].get("value").is_none(),
        "list carries no value: {secrets}"
    );
    assert!(
        !serde_json::to_string(&list).unwrap().contains(marker),
        "list never carries the value"
    );
    let (_, set2) = call(
        &state,
        Some(&token),
        "set_secret",
        json!({"app": "groceries", "key": "MAIL_KEY", "value": "second"}),
    )
    .await;
    assert_eq!(result(&set2)["structuredContent"]["ok"], true);
    let (_, list2) = call(
        &state,
        Some(&token),
        "list_secrets",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(
        result(&list2)["structuredContent"]["secrets"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (_, unset) = call(
        &state,
        Some(&token),
        "unset_secret",
        json!({"app": "groceries", "key": "MAIL_KEY"}),
    )
    .await;
    assert_eq!(
        result(&unset)["structuredContent"],
        json!({"ok": true, "applies": "next deploy"})
    );
    let (_, empty) = call(
        &state,
        Some(&token),
        "list_secrets",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(result(&empty)["structuredContent"]["secrets"], json!([]));
    let (_, missing) = call(
        &state,
        Some(&token),
        "unset_secret",
        json!({"app": "groceries", "key": "MAIL_KEY"}),
    )
    .await;
    assert_eq!(is_error(&missing)["error"], "not_found");
}
#[tokio::test]
async fn secrets_refuse_bad_names_reserved_values_and_strangers() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;
    for key in ["my_key", "9LIVES", "HAS-DASH", "HAS SPACE", ""] {
        let (_, bad) = call(
            &state,
            Some(&ana),
            "set_secret",
            json!({"app": "groceries", "key": key, "value": "v"}),
        )
        .await;
        let err = is_error(&bad);
        assert_eq!(err["error"], "invalid_argument", "{key}");
        assert!(
            err["message"].as_str().unwrap().contains("secret name"),
            "{key}: {}",
            err["message"]
        );
    }
    for key in [
        "PORT",
        "AWS_SECRET_ACCESS_KEY",
        "OUTER_SECRET",
        "LD_PRELOAD",
    ] {
        let (_, bad) = call(
            &state,
            Some(&ana),
            "set_secret",
            json!({"app": "groceries", "key": key, "value": "v"}),
        )
        .await;
        let err = is_error(&bad);
        assert_eq!(err["error"], "invalid_argument", "{key}");
        assert!(
            err["message"].as_str().unwrap().contains("reserved"),
            "{key}: {}",
            err["message"]
        );
    }
    let (_, big) = call(
        &state,
        Some(&ana),
        "set_secret",
        json!({"app": "groceries", "key": "K", "value": "x".repeat(4097)}),
    )
    .await;
    assert_eq!(is_error(&big)["error"], "invalid_argument");
    let marker = "STRANGER_SECRET_MARKER_1A2B3C";
    let (_, foreign) = call(
        &state,
        Some(&bob),
        "set_secret",
        json!({"app": "groceries", "key": "K", "value": marker}),
    )
    .await;
    assert_eq!(is_error(&foreign)["error"], "not_found");
    let (_, foreign_list) = call(
        &state,
        Some(&bob),
        "list_secrets",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(is_error(&foreign_list)["error"], "not_found");
    let rows: Vec<String> = sqlx::query_scalar("SELECT name FROM app_secret")
        .fetch_all(&state.pool)
        .await
        .unwrap();
    assert!(rows.is_empty(), "{rows:?}");
}
#[tokio::test]
async fn secrets_cap_counts_distinct_names_and_values_stay_out_of_logs() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let marker = "LOG_SECRET_MARKER_4D5E6F";
    for n in 0..50 {
        let (_, set) = call(
            &state,
            Some(&token),
            "set_secret",
            json!({"app": "groceries", "key": format!("K{n:02}"), "value": marker}),
        )
        .await;
        assert_eq!(
            result(&set)["structuredContent"]["ok"],
            true,
            "K{n:02} fits"
        );
    }
    let (_, capped) = call(
        &state,
        Some(&token),
        "set_secret",
        json!({"app": "groceries", "key": "ONE_TOO_MANY", "value": "v"}),
    )
    .await;
    let err = is_error(&capped);
    assert_eq!(err["error"], "invalid_argument");
    assert!(
        err["message"].as_str().unwrap().contains("50"),
        "{}",
        err["message"]
    );
    for row in action_rows(&state, "groceries").await {
        assert!(
            !row.summary.contains(marker),
            "value leaked into log: {}",
            row.summary
        );
    }
    let summaries: Vec<String> = sqlx::query_scalar("SELECT summary FROM agent_action")
        .fetch_all(&state.pool)
        .await
        .unwrap();
    assert!(
        !summaries.iter().any(|s| s.contains(marker)),
        "{summaries:?}"
    );
    assert!(
        summaries.iter().any(|s| s.contains("K00")),
        "name-only summaries: {summaries:?}"
    );
    let tools: Vec<String> = sqlx::query_scalar("SELECT DISTINCT tool FROM agent_action")
        .fetch_all(&state.pool)
        .await
        .unwrap();
    assert!(tools.contains(&"set_secret".to_string()), "{tools:?}");
}

/// Flip an account's plan by email.
async fn set_plan(state: &McpState, email: &str, plan: &str) {
    sqlx::query("UPDATE user SET plan = ?1 WHERE email = ?2")
        .bind(plan)
        .bind(email)
        .execute(&state.pool)
        .await
        .unwrap();
}

/// A Pro owner's app, returning the owner's token.
async fn pro_app(state: &McpState, owner_email: &str, slug: &str) -> String {
    let token = api_key_for(state, owner_email).await;
    set_plan(state, owner_email, "pro").await;
    create_app(state, &token, slug).await;
    token
}

async fn invite(state: &McpState, token: &str, slug: &str, email: &str) -> Value {
    let (_, body) = call(
        state,
        Some(token),
        "add_collaborator",
        json!({"app": slug, "email": email}),
    )
    .await;
    body
}

#[tokio::test]
async fn collaborator_invite_needs_a_pro_owner() {
    let (state, codes) = setup().await;
    let owner = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &owner, "groceries").await;

    // A Free owner is refused with payment_required, pointing at Pro.
    let refused = invite(&state, &owner, "groceries", "bruno@example.com").await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "payment_required");
    assert!(err["user_message"].as_str().unwrap().contains("Pro"));

    set_plan(&state, "ana@example.com", "pro").await;
    let added = result(&invite(&state, &owner, "groceries", "Bruno@Example.com").await)
        ["structuredContent"]
        .clone();
    assert_eq!(added["collaborator"]["email"], "bruno@example.com");
    assert_eq!(added["collaborator"]["status"], "pending");

    // The invite allowlists the email and emails the spec wording.
    let allowlisted: Option<String> =
        sqlx::query_scalar("SELECT email FROM beta_allowlist WHERE email = 'bruno@example.com'")
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert_eq!(allowlisted.as_deref(), Some("bruno@example.com"));
    let mail = codes
        .lock()
        .unwrap()
        .get("bruno@example.com")
        .cloned()
        .expect("invite email sent");
    assert!(mail.contains("groceries"), "{mail}");
    assert!(mail.contains("/mcp"), "{mail}");

    // Inviting twice, inviting the owner, and inviting junk all refuse.
    let dup_body = invite(&state, &owner, "groceries", "bruno@example.com").await;
    assert_eq!(is_error(&dup_body)["error"], "invalid_argument");
    let self_body = invite(&state, &owner, "groceries", "ana@example.com").await;
    assert_eq!(is_error(&self_body)["error"], "invalid_argument");
    let bad_body = invite(&state, &owner, "groceries", "not-an-email").await;
    assert_eq!(is_error(&bad_body)["error"], "invalid_argument");
}

#[tokio::test]
async fn collaborator_cap_is_ten_per_app() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    for n in 0..10 {
        let body = invite(
            &state,
            &owner,
            "groceries",
            &format!("friend{n}@example.com"),
        )
        .await;
        assert!(result(&body).get("isError").is_none(), "{body}");
    }
    let capped = invite(&state, &owner, "groceries", "extra@example.com").await;
    let err = is_error(&capped);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "collaborators");
    assert_eq!(err["limit"], 10);
    assert_eq!(err["used"], 10);

    let (_, list) = call(
        &state,
        Some(&owner),
        "list_collaborators",
        json!({"app": "groceries"}),
    )
    .await;
    let collaborators = result(&list)["structuredContent"]["collaborators"]
        .as_array()
        .unwrap();
    assert_eq!(collaborators.len(), 10);
    assert!(collaborators.iter().all(|c| c["status"] == "pending"));
}

#[tokio::test]
async fn collaborator_joins_through_the_closed_beta_without_invite_codes() {
    let (mut state, codes) = setup().await;
    state.auth.config.signup_open = false;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    invite(&state, &owner, "groceries", "bruno@example.com").await;

    // A non-allowlisted stranger is still refused a code (the auth layer's
    // `closed_beta` maps to `forbidden` on the wire).
    let (_, refused) = call(
        &state,
        None,
        "sign_in",
        json!({"email": "stranger@example.com"}),
    )
    .await;
    assert_eq!(is_error(&refused)["error"], "forbidden");

    // The invited email signs straight in: no invite code involved.
    let (_, sent) = call(
        &state,
        None,
        "sign_in",
        json!({"email": "bruno@example.com"}),
    )
    .await;
    assert_eq!(result(&sent)["structuredContent"]["sent"], true);
    let code = auth_ts::captured(&codes, "bruno@example.com");
    let (_, verified) = call(
        &state,
        None,
        "verify_code",
        json!({"email": "bruno@example.com", "code": code, "accept_terms": true}),
    )
    .await;
    let key = result(&verified)["structuredContent"]["api_key"]
        .as_str()
        .unwrap()
        .to_string();

    // The row links to the new account: joined, visible in list_apps.
    let linked: Option<String> =
        sqlx::query_scalar("SELECT userId FROM app_collaborator WHERE email = 'bruno@example.com'")
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            .flatten();
    assert!(linked.is_some());
    let (_, listed) = call(
        &state,
        Some(&owner),
        "list_collaborators",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(
        result(&listed)["structuredContent"]["collaborators"][0]["status"],
        "joined"
    );
    let (_, apps) = call(&state, Some(&key), "list_apps", json!({})).await;
    let apps = result(&apps)["structuredContent"]["apps"].clone();
    assert_eq!(apps.as_array().unwrap().len(), 1);
    assert_eq!(apps[0]["slug"], "groceries");
    assert_eq!(apps[0]["role"], "collaborator");
}

#[tokio::test]
async fn collaborator_calls_allowed_tools_and_is_refused_owner_only_ones() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    invite(&state, &owner, "groceries", "bruno@example.com").await;
    let bruno = api_key_for(&state, "bruno@example.com").await;

    // Reads, changes and deploys all answer: none of these is owner-only.
    for (name, args) in [
        ("get_app", json!({"app": "groceries"})),
        ("share_app", json!({"app": "groceries"})),
        ("list_files", json!({"app": "groceries"})),
        (
            "read_file",
            json!({"app": "groceries", "path": "pages/index.tsx"}),
        ),
        (
            "search_code",
            json!({"app": "groceries", "query": "Groceries"}),
        ),
        ("history", json!({"app": "groceries"})),
        ("deploy_status", json!({"app": "groceries"})),
        ("app_logs", json!({"app": "groceries"})),
        ("app_errors", json!({"app": "groceries"})),
        ("list_secrets", json!({"app": "groceries"})),
    ] {
        let (_, body) = call(&state, Some(&bruno), name, args).await;
        assert!(result(&body).get("isError").is_none(), "{name}: {body}");
    }
    let (_, commit) = call(
        &state,
        Some(&bruno),
        "commit",
        json!({"app": "groceries", "message": "bruno was here",
               "writes": [{"path": "note.txt", "content": "hi\n"}]}),
    )
    .await;
    assert!(result(&commit).get("isError").is_none(), "{commit}");
    let first = result(&commit)["structuredContent"]["commit"]["sha"].clone();
    // A second commit moves the head, so the rollback has a target.
    let (_, second) = call(
        &state,
        Some(&bruno),
        "commit",
        json!({"app": "groceries", "message": "bruno again",
               "writes": [{"path": "note.txt", "content": "hi again\n"}]}),
    )
    .await;
    assert!(result(&second).get("isError").is_none(), "{second}");
    let (_, rollback) = call(
        &state,
        Some(&bruno),
        "rollback",
        json!({"app": "groceries", "sha": first}),
    )
    .await;
    assert!(result(&rollback).get("isError").is_none(), "{rollback}");

    // Owner-only tools refuse a collaborator with forbidden.
    for (name, args) in [
        (
            "set_visibility",
            json!({"app": "groceries", "visibility": "public"}),
        ),
        (
            "rename_app",
            json!({"app": "groceries", "slug": "brand-new"}),
        ),
        (
            "set_secret",
            json!({"app": "groceries", "key": "K", "value": "v"}),
        ),
        ("unset_secret", json!({"app": "groceries", "key": "K"})),
        (
            "delete_app",
            json!({"app": "groceries", "confirm": "groceries"}),
        ),
        (
            "add_collaborator",
            json!({"app": "groceries", "email": "x@example.com"}),
        ),
        (
            "remove_collaborator",
            json!({"app": "groceries", "email": "bruno@example.com"}),
        ),
        ("list_collaborators", json!({"app": "groceries"})),
    ] {
        let (_, body) = call(&state, Some(&bruno), name, args).await;
        assert_eq!(is_error(&body)["error"], "forbidden", "{name}: {body}");
    }

    // A slug with no role reads exactly like a missing one.
    let (_, missing) = call(&state, Some(&bruno), "get_app", json!({"app": "nope"})).await;
    assert_eq!(is_error(&missing)["error"], "not_found");
}

#[tokio::test]
async fn collaborator_may_triage_errors() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    invite(&state, &owner, "groceries", "bruno@example.com").await;
    let bruno = api_key_for(&state, "bruno@example.com").await;
    let stranger = api_key_for(&state, "stranger@example.com").await;
    let app_id = app_id(&state, "groceries").await;
    let now = crate::apps::now_ms();
    sqlx::query(
        "INSERT INTO error_issue (app_id, fingerprint, message, frame, count, first_seen, last_seen, status) \
         VALUES (?1, 'fp1', 'Error: kaboom', 'at handler (x:1:2)', 1, ?2, ?3, 'open')",
    )
    .bind(&app_id)
    .bind(now - 1_000)
    .bind(now - 500)
    .execute(&state.pool)
    .await
    .unwrap();
    // A collaborator triages like the owner: resolve, ignore, reopen.
    let (_, resolved) = call(
        &state,
        Some(&bruno),
        "resolve_error",
        json!({"app": "groceries", "fingerprint": "fp1"}),
    )
    .await;
    assert_eq!(
        result(&resolved)["structuredContent"],
        json!({"fingerprint": "fp1", "status": "resolved"})
    );
    let (_, ignored) = call(
        &state,
        Some(&bruno),
        "ignore_error",
        json!({"app": "groceries", "fingerprint": "fp1"}),
    )
    .await;
    assert_eq!(result(&ignored)["structuredContent"]["status"], "ignored");
    let (_, reopened) = call(
        &state,
        Some(&bruno),
        "reopen_error",
        json!({"app": "groceries", "fingerprint": "fp1"}),
    )
    .await;
    assert_eq!(result(&reopened)["structuredContent"]["status"], "open");
    // A stranger's slug still answers not_found, exactly like a missing one.
    let (_, refused) = call(
        &state,
        Some(&stranger),
        "resolve_error",
        json!({"app": "groceries", "fingerprint": "fp1"}),
    )
    .await;
    assert_eq!(is_error(&refused)["error"], "not_found");
}

#[tokio::test]
async fn collaborator_removal_takes_effect_on_the_next_call() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    invite(&state, &owner, "groceries", "bruno@example.com").await;
    let bruno = api_key_for(&state, "bruno@example.com").await;
    let (_, before) = call(&state, Some(&bruno), "get_app", json!({"app": "groceries"})).await;
    assert!(result(&before).get("isError").is_none());

    let (_, removed) = call(
        &state,
        Some(&owner),
        "remove_collaborator",
        json!({"app": "groceries", "email": "bruno@example.com"}),
    )
    .await;
    assert_eq!(result(&removed)["structuredContent"]["removed"], true);
    let (_, after) = call(&state, Some(&bruno), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(is_error(&after)["error"], "not_found");
    let (_, apps) = call(&state, Some(&bruno), "list_apps", json!({})).await;
    assert_eq!(
        result(&apps)["structuredContent"]["apps"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // Removing someone who isn't there is the caller's bug.
    let (_, again) = call(
        &state,
        Some(&owner),
        "remove_collaborator",
        json!({"app": "groceries", "email": "bruno@example.com"}),
    )
    .await;
    assert_eq!(is_error(&again)["error"], "invalid_argument");
}

#[tokio::test]
async fn collaborator_commit_counts_against_the_owner_quota() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    invite(&state, &owner, "groceries", "bruno@example.com").await;
    let bruno = api_key_for(&state, "bruno@example.com").await;
    let owner_id: String =
        sqlx::query_scalar("SELECT id FROM user WHERE email = 'ana@example.com'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    let bruno_id: String =
        sqlx::query_scalar("SELECT id FROM user WHERE email = 'bruno@example.com'")
            .fetch_one(&state.pool)
            .await
            .unwrap();

    let (_, commit) = call(
        &state,
        Some(&bruno),
        "commit",
        json!({"app": "groceries", "message": "bruno's change",
               "writes": [{"path": "note.txt", "content": "hi\n"}]}),
    )
    .await;
    assert!(result(&commit).get("isError").is_none(), "{commit}");
    // create_app seeds git directly (no claim); the one commit lands on the
    // owner, and nothing lands on the collaborator.
    let owner_used: Option<i64> =
        sqlx::query_scalar("SELECT code_changes FROM usage_day WHERE userId = ?1")
            .bind(&owner_id)
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert_eq!(owner_used, Some(1));
    let bruno_used: Option<i64> =
        sqlx::query_scalar("SELECT code_changes FROM usage_day WHERE userId = ?1")
            .bind(&bruno_id)
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert_eq!(bruno_used, None);
}

#[tokio::test]
async fn collaborator_access_follows_the_owner_plan() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    invite(&state, &owner, "groceries", "bruno@example.com").await;
    let bruno = api_key_for(&state, "bruno@example.com").await;
    let (_, seen) = call(&state, Some(&bruno), "get_app", json!({"app": "groceries"})).await;
    assert!(result(&seen).get("isError").is_none());

    // Downgrade: the rows stay, but the app reads as missing.
    set_plan(&state, "ana@example.com", "free").await;
    let (_, hidden) = call(&state, Some(&bruno), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(is_error(&hidden)["error"], "not_found");
    let (_, apps) = call(&state, Some(&bruno), "list_apps", json!({})).await;
    assert_eq!(
        result(&apps)["structuredContent"]["apps"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // The invite list itself stays put through the downgrade.
    let (_, listed) = call(
        &state,
        Some(&owner),
        "list_collaborators",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(
        result(&listed)["structuredContent"]["collaborators"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Re-upgrade: access is back with no re-invite.
    set_plan(&state, "ana@example.com", "pro").await;
    let (_, back) = call(&state, Some(&bruno), "get_app", json!({"app": "groceries"})).await;
    assert!(result(&back).get("isError").is_none());
}
/// `public` is a Pro feature, gated in the shared domain function: a Free
/// owner is refused with `payment_required` (and a user message pointing to
/// Pro), `unlisted` is always allowed, and Pro flips the row.
#[tokio::test]
async fn set_visibility_public_is_pro_gated() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &ana, "groceries").await;

    let (_, refused) = call(
        &state,
        Some(&ana),
        "set_visibility",
        json!({"app": "groceries", "visibility": "public"}),
    )
    .await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "payment_required");
    assert!(
        err["user_message"].as_str().unwrap().contains("Pro"),
        "{err}"
    );

    let (_, unlisted) = call(
        &state,
        Some(&ana),
        "set_visibility",
        json!({"app": "groceries", "visibility": "unlisted"}),
    )
    .await;
    assert_eq!(
        result(&unlisted)["structuredContent"]["visibility"],
        "unlisted"
    );

    sqlx::query("UPDATE user SET plan = 'pro' WHERE email = 'ana@example.com'")
        .execute(&state.pool)
        .await
        .unwrap();
    let (_, public) = call(
        &state,
        Some(&ana),
        "set_visibility",
        json!({"app": "groceries", "visibility": "public"}),
    )
    .await;
    assert_eq!(result(&public)["structuredContent"]["visibility"], "public");
}

#[tokio::test]
async fn error_triage_tools_move_issues() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;
    let app_id = app_id(&state, "groceries").await;
    let now = crate::apps::now_ms();
    sqlx::query(
        "INSERT INTO error_issue (app_id, fingerprint, message, frame, count, first_seen, last_seen, status) \
         VALUES (?1, 'fp1', 'Error: kaboom', 'at handler (x:1:2)', 1, ?2, ?3, 'open')",
    )
    .bind(&app_id)
    .bind(now - 1_000)
    .bind(now - 500)
    .execute(&state.pool)
    .await
    .unwrap();
    // Resolve moves the issue out of the open list.
    let (_, out) = call(
        &state,
        Some(&ana),
        "resolve_error",
        json!({"app": "groceries", "fingerprint": "fp1"}),
    )
    .await;
    assert_eq!(
        result(&out)["structuredContent"],
        json!({"fingerprint": "fp1", "status": "resolved"})
    );
    let (_, resolved) = call(
        &state,
        Some(&ana),
        "app_errors",
        json!({"app": "groceries", "status": "resolved"}),
    )
    .await;
    assert_eq!(
        result(&resolved)["structuredContent"]["issues"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // A stranger's slug answers not_found, exactly like a missing one.
    let (_, bob_body) = call(
        &state,
        Some(&bob),
        "resolve_error",
        json!({"app": "groceries", "fingerprint": "fp1"}),
    )
    .await;
    assert_eq!(is_error(&bob_body)["error"], "not_found");
    // Unknown fingerprints are not_found; empty ones are the caller's bug.
    let (_, missing) = call(
        &state,
        Some(&ana),
        "ignore_error",
        json!({"app": "groceries", "fingerprint": "nobody"}),
    )
    .await;
    assert_eq!(is_error(&missing)["error"], "not_found");
    let (_, empty) = call(
        &state,
        Some(&ana),
        "ignore_error",
        json!({"app": "groceries", "fingerprint": ""}),
    )
    .await;
    assert_eq!(is_error(&empty)["error"], "invalid_argument");
    // Ignore then reopen round-trips back to open.
    let (_, ignored) = call(
        &state,
        Some(&ana),
        "ignore_error",
        json!({"app": "groceries", "fingerprint": "fp1"}),
    )
    .await;
    assert_eq!(result(&ignored)["structuredContent"]["status"], "ignored");
    let (_, reopened) = call(
        &state,
        Some(&ana),
        "reopen_error",
        json!({"app": "groceries", "fingerprint": "fp1"}),
    )
    .await;
    assert_eq!(result(&reopened)["structuredContent"]["status"], "open");
    // Every triage call lands in the action log under its own name.
    let actions = crate::mcp::actions::list(&state.pool, &app_id, None, 10)
        .await
        .unwrap();
    for tool in ["resolve_error", "ignore_error", "reopen_error"] {
        assert!(
            actions
                .iter()
                .any(|action| action.tool == tool && action.summary.starts_with(tool)),
            "{tool} audited: {:?}",
            actions.iter().map(|a| &a.tool).collect::<Vec<_>>(),
        );
    }
}

#[tokio::test]
async fn rename_app_moves_everything_and_quarantines_with_redirect() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;
    create_app(&state, &bob, "bobapp").await;
    let id_before = app_id(&state, "groceries").await;

    // Fake deploys write no bucket objects, so seed both prefixes by hand:
    // a deploy pointer plus a D1-shaped object that must survive the move.
    for (key, body) in [
        ("fleets/groceries/deploy/current.json", "{}"),
        ("fleets/groceries/cells/d1.sqlite", "D1DATA"),
        ("fleets/bobapp/deploy/current.json", "{}"),
    ] {
        state
            .store
            .put(key, body.as_bytes().to_vec())
            .await
            .unwrap();
    }
    // Local state the pipeline would have left behind.
    let data = &state.delete_cfg.data_dir;
    tokio::fs::create_dir_all(data.join("work/groceries"))
        .await
        .unwrap();
    tokio::fs::write(data.join("work/groceries.index"), b"i")
        .await
        .unwrap();
    tokio::fs::write(data.join("work/groceries.install-hash"), b"h")
        .await
        .unwrap();
    tokio::fs::create_dir_all(data.join("cache/groceries"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(data.join("fleets/groceries"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(data.join("secret-sidecars"))
        .await
        .unwrap();
    tokio::fs::write(data.join("secret-sidecars/groceries.orig"), b"o")
        .await
        .unwrap();
    tokio::fs::write(data.join("secret-sidecars/groceries.target"), b"0\ncfg")
        .await
        .unwrap();
    assert!(state.git.repos_dir.join("groceries.git").exists());

    let (_, body) = call(
        &state,
        Some(&ana),
        "rename_app",
        json!({"app": "groceries", "slug": "pantry"}),
    )
    .await;
    let structured = &result(&body)["structuredContent"];
    assert_eq!(structured["slug"], "pantry");
    assert_eq!(structured["url"], "http://pantry.outer.localhost");
    // The app ran (fresh create leaves it awake with a successful deploy),
    // so it redeployed under the new name through the fake.
    assert_eq!(structured["deploy"]["status"], "success");
    let quarantined_until = structured["quarantined_until"].as_i64().unwrap();
    assert!(quarantined_until > crate::apps::now_ms());
    assert!(
        result(&body)["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("stays reserved for 30 days"),
        "{body}"
    );

    // The row flipped, same id; the old slug is gone.
    assert_eq!(app_id(&state, "pantry").await, id_before);
    let gone: Option<String> = sqlx::query_scalar("SELECT id FROM app WHERE slug = 'groceries'")
        .fetch_optional(&state.pool)
        .await
        .unwrap();
    assert!(gone.is_none());
    // Bucket prefixes moved (deploy pointer + D1 data intact), old ones
    // empty, the sibling app untouched.
    assert_eq!(
        state
            .store
            .get("fleets/pantry/deploy/current.json")
            .await
            .unwrap(),
        Some(b"{}".to_vec())
    );
    assert_eq!(
        state
            .store
            .get("fleets/pantry/cells/d1.sqlite")
            .await
            .unwrap(),
        Some(b"D1DATA".to_vec())
    );
    assert!(state
        .store
        .list("fleets/groceries/")
        .await
        .unwrap()
        .is_empty());
    assert!(state.store.list("git/groceries/").await.unwrap().is_empty());
    assert!(!state.store.list("git/pantry/").await.unwrap().is_empty());
    assert!(!state.store.list("fleets/bobapp/").await.unwrap().is_empty());
    // Local state moved.
    assert!(data.join("work/pantry").exists());
    assert!(!data.join("work/groceries").exists());
    assert!(data.join("work/pantry.index").exists());
    assert!(!data.join("work/groceries.index").exists());
    assert!(data.join("work/pantry.install-hash").exists());
    assert!(!data.join("work/groceries.install-hash").exists());
    assert!(data.join("cache/pantry").exists());
    assert!(!data.join("cache/groceries").exists());
    assert!(data.join("fleets/pantry").exists());
    assert!(!data.join("fleets/groceries").exists());
    assert!(state.git.repos_dir.join("pantry.git").exists());
    assert!(!state.git.repos_dir.join("groceries.git").exists());
    assert!(data.join("secret-sidecars/pantry.orig").exists());
    assert!(!data.join("secret-sidecars/groceries.orig").exists());
    // Quarantine names the redirect target.
    let row: (i64, Option<String>) =
        sqlx::query_as("SELECT until, renamed_to FROM slug_quarantine WHERE slug = 'groceries'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(row.0, quarantined_until);
    assert_eq!(row.1.as_deref(), Some("pantry"));
    // The old slug answers not_found on the tools, and a fresh account's
    // create is refused as quarantined (Ana is at her Free app slot, so her
    // own create would fail quota first); app_id-keyed history followed.
    let (_, missing) = call(&state, Some(&ana), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(is_error(&missing)["error"], "not_found");
    let carol = api_key_for(&state, "carol@example.com").await;
    let (_, again) = call(
        &state,
        Some(&carol),
        "create_app",
        json!({"slug": "groceries"}),
    )
    .await;
    let err = is_error(&again);
    assert_eq!(err["error"], "invalid_argument");
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("taken or quarantined"));
    let deploys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deploy WHERE app_id = ?1")
        .bind(&id_before)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!(deploys >= 2, "create + rename redeploy: {deploys}");
    // The committed rename counted one change.
    let account = crate::apps::ensure_account(&state.pool, "ana@example.com")
        .await
        .unwrap();
    let used: i64 = sqlx::query_scalar("SELECT code_changes FROM usage_day WHERE userId = ?1")
        .bind(&account)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(used, 1);
    // The MCP audit row logged old → new.
    let audit: (String, String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT tool, summary, consent_id, apikey_id FROM agent_action WHERE app_id = ?1 AND tool = 'rename_app'",
    )
    .bind(&id_before)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(audit.0, "rename_app");
    assert_eq!(audit.1, "rename_app groceries to pantry");
    // And Bob's app still serves its summary.
    let (_, kept) = call(&state, Some(&bob), "get_app", json!({"app": "bobapp"})).await;
    assert_eq!(result(&kept)["structuredContent"]["slug"], "bobapp");
}

#[tokio::test]
async fn rename_app_refusals_keep_the_app_and_refund_quota() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;
    create_app(&state, &bob, "bobapp").await;
    // Commit one rename so a quarantined target exists.
    let (_, first) = call(
        &state,
        Some(&ana),
        "rename_app",
        json!({"app": "groceries", "slug": "pantry"}),
    )
    .await;
    assert_eq!(result(&first)["structuredContent"]["slug"], "pantry");

    // Quarantined (someone's old name), taken (a live app), reserved,
    // misshapen and same-slug targets are all refused with the create_app
    // messages; unknown and foreign slugs are not_found.
    for (app, slug, code, message) in [
        (
            "pantry",
            "groceries",
            "invalid_argument",
            "taken or quarantined",
        ),
        (
            "pantry",
            "bobapp",
            "invalid_argument",
            "taken or quarantined",
        ),
        ("pantry", "api", "invalid_argument", "reserved"),
        ("pantry", "Bad_Shape", "invalid_argument", "must match"),
        ("pantry", "pantry", "invalid_argument", "already named"),
        ("nope", "pantry", "not_found", "no app"),
    ] {
        let (_, body) = call(
            &state,
            Some(&ana),
            "rename_app",
            json!({"app": app, "slug": slug}),
        )
        .await;
        let err = is_error(&body);
        assert_eq!(err["error"], code, "{app} → {slug}: {body}");
        assert!(
            err["message"].as_str().unwrap().contains(message),
            "{app} → {slug}: {body}"
        );
    }
    // Foreign: Bob's correct-looking call answers exactly like missing.
    let (_, foreign) = call(
        &state,
        Some(&bob),
        "rename_app",
        json!({"app": "pantry", "slug": "bobbins"}),
    )
    .await;
    assert_eq!(is_error(&foreign)["error"], "not_found");

    // The row never moved and the quarantine still points at pantry.
    let id: String = app_id(&state, "pantry").await;
    let target: Option<String> =
        sqlx::query_scalar("SELECT renamed_to FROM slug_quarantine WHERE slug = 'groceries'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(target.as_deref(), Some("pantry"));
    // Only the committed rename counted: every refusal refunded its claim.
    let account = crate::apps::ensure_account(&state.pool, "ana@example.com")
        .await
        .unwrap();
    let used: i64 = sqlx::query_scalar("SELECT code_changes FROM usage_day WHERE userId = ?1")
        .bind(&account)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(used, 1, "refusals must refund");
    let _ = id;
}

#[tokio::test]
async fn rename_app_store_fault_refunds_quota_and_keeps_the_old_identity() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &ana, "groceries").await;
    // Fail the rename's first store op (the pre-copy listing): a crash
    // before the commit, while the quota claim is held.
    state.store.fail_next(1, "injected store outage");
    let (_, body) = call(
        &state,
        Some(&ana),
        "rename_app",
        json!({"app": "groceries", "slug": "pantry"}),
    )
    .await;
    assert_eq!(is_error(&body)["error"], "internal");
    // Nothing committed, so nothing counted: the claimed change is refunded.
    let account = crate::apps::ensure_account(&state.pool, "ana@example.com")
        .await
        .unwrap();
    let used: i64 = sqlx::query_scalar("SELECT code_changes FROM usage_day WHERE userId = ?1")
        .bind(&account)
        .fetch_optional(&state.pool)
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(used, 0, "a pre-commit failure must refund the claim");
    // The old identity is untouched, but the target rows were already
    // recorded (by design: the target is unclaimable from that moment on).
    let (_, kept) = call(&state, Some(&ana), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(result(&kept)["structuredContent"]["slug"], "groceries");
    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM prefix_cleanup")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(jobs, 2, "the target rows block the slug until drained");
    let (_, blocked) = call(&state, Some(&ana), "get_app", json!({"app": "pantry"})).await;
    assert_eq!(is_error(&blocked)["error"], "not_found");
    // The maintenance pass drains the abandoned copy; retry converges.
    let drained =
        crate::apps::rename::drain_prefix_cleanup(&state.pool, &state.store, Some(&state.fleets))
            .await
            .unwrap();
    assert_eq!(drained, 2);
    let (_, again) = call(
        &state,
        Some(&ana),
        "rename_app",
        json!({"app": "groceries", "slug": "pantry"}),
    )
    .await;
    assert_eq!(result(&again)["structuredContent"]["slug"], "pantry");
}
#[tokio::test]
async fn rename_app_reports_a_failed_redeploy_as_rename_success() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &ana, "groceries").await;
    // The pipeline itself blows up on the redeploy: the rename already
    // committed, so it still answers success with the failure embedded.
    state.deploys.set_fake(FakeOutcome::transport_error());
    let (_, body) = call(
        &state,
        Some(&ana),
        "rename_app",
        json!({"app": "groceries", "slug": "pantry"}),
    )
    .await;
    assert!(
        result(&body).get("isError").is_none(),
        "a failed redeploy must not fail the rename: {body}"
    );
    let structured = &result(&body)["structuredContent"];
    assert_eq!(structured["slug"], "pantry");
    assert_eq!(structured["deploy"]["status"], "failed");
    // The pipeline's detail stays server-side (see `internal`): the agent
    // gets the generic message, exactly like any other tool failure.
    assert!(
        structured["deploy"]["error"]
            .as_str()
            .unwrap()
            .contains("something failed on our side"),
        "{body}"
    );
    // The committed rename counts: the quota claim is retained, not refunded.
    let account = crate::apps::ensure_account(&state.pool, "ana@example.com")
        .await
        .unwrap();
    let used: i64 = sqlx::query_scalar("SELECT code_changes FROM usage_day WHERE userId = ?1")
        .bind(&account)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        used, 1,
        "a committed rename counts even when its redeploy fails"
    );
}

#[tokio::test]
async fn rename_app_waits_for_the_deploy_lock() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &ana, "groceries").await;
    let id = app_id(&state, "groceries").await;

    // A deploy holds the app lock: the rename serializes behind it. The
    // pre-release assertions hold in every schedule — the rename cannot pass
    // lock acquisition while this guard lives, whatever runs first.
    let guard = state.deploys.app_lock(&id).lock_owned().await;
    let moved = state.clone();
    let token = ana.clone();
    let handle = tokio::spawn(async move {
        let (_, body) = call(
            &moved,
            Some(&token),
            "rename_app",
            json!({"app": "groceries", "slug": "pantry"}),
        )
        .await;
        body
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let still: Option<String> = sqlx::query_scalar("SELECT slug FROM app WHERE id = ?1")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await
        .unwrap();
    assert_eq!(still.as_deref(), Some("groceries"));
    let quarantine: Option<String> =
        sqlx::query_scalar("SELECT slug FROM slug_quarantine WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert!(quarantine.is_none());
    drop(guard);
    let body = handle.await.unwrap();
    assert_eq!(result(&body)["structuredContent"]["slug"], "pantry");
}

/// Point `state`'s zone client at the SaaS fake, so custom-hostname calls
/// never touch the network.
fn point_zone_at_fake(state: &mut McpState, fake: &crate::cloudflare::fake_saas::FakeSaaS) {
    state.delete_cfg.cache_purge = Some(
        crate::cloudflare::CachePurge::new("zone-1".to_string(), "token-1".to_string())
            .with_api_base(fake.base_url.clone()),
    );
}

/// The `add_domain` tool against the scripted Cloudflare for SaaS fake:
/// the Pro gate, hostname validation, and the pending → active → removed
/// flow with the DNS setup in the summary.
#[tokio::test]
async fn add_list_remove_domain_round_trip() {
    let (mut state, _) = setup().await;
    let fake = crate::cloudflare::fake_saas::FakeSaaS::start().await;
    point_zone_at_fake(&mut state, &fake);
    let token = pro_app(&state, "ana@example.com", "groceries").await;
    // Free callers never reach Cloudflare.
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &bob, "deli").await;
    let (_, body) = call(
        &state,
        Some(&bob),
        "add_domain",
        json!({"app": "deli", "hostname": "shop.example.com"}),
    )
    .await;
    let err = is_error(&body);
    assert_eq!(err["error"], "payment_required");
    assert!(err["user_message"].as_str().unwrap().contains("Outer Pro"));
    assert!(fake.calls("POST").is_empty(), "Pro gate fires first");
    // Bad hostnames are the caller's bug, before any app checks.
    let (_, body) = call(
        &state,
        Some(&token),
        "add_domain",
        json!({"app": "groceries", "hostname": "not a host!!"}),
    )
    .await;
    assert_eq!(is_error(&body)["error"], "invalid_argument");
    let (_, body) = call(
        &state,
        Some(&token),
        "add_domain",
        json!({"app": "groceries", "hostname": "groceries.outer.localhost"}),
    )
    .await;
    assert_eq!(is_error(&body)["error"], "invalid_argument");
    // Pro add: pending + the DNS setup relayed to the agent.
    let (_, body) = call(
        &state,
        Some(&token),
        "add_domain",
        json!({"app": "groceries", "hostname": "shop.example.com"}),
    )
    .await;
    let added = &result(&body)["structuredContent"];
    assert_eq!(added["domains"][0]["hostname"], "shop.example.com");
    assert_eq!(added["domains"][0]["status"], "pending");
    assert_eq!(
        added["domains"][0]["cname_target"],
        "domains.outer.localhost"
    );
    assert!(
        result(&body)["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("CNAME"),
        "summary carries the DNS setup: {}",
        result(&body)["content"][0]["text"]
    );
    // One per app: the second add is refused without a new POST.
    let (_, body) = call(
        &state,
        Some(&token),
        "add_domain",
        json!({"app": "groceries", "hostname": "other.example.com"}),
    )
    .await;
    assert_eq!(is_error(&body)["error"], "invalid_argument");
    assert_eq!(fake.calls("POST").len(), 1);
    // list_domains refreshes live: still pending until Cloudflare finishes.
    let (_, body) = call(
        &state,
        Some(&token),
        "list_domains",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(
        result(&body)["structuredContent"]["domains"][0]["status"],
        "pending"
    );
    fake.state.lock().unwrap().status.insert(
        "cf-shop.example.com".to_string(),
        ("active".to_string(), "active".to_string()),
    );
    let (_, body) = call(
        &state,
        Some(&token),
        "list_domains",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(
        result(&body)["structuredContent"]["domains"][0]["status"],
        "active"
    );
    // Removal drops the row and deletes the Cloudflare hostname. A wrong
    // hostname is refused first, without touching anything.
    let (_, body) = call(
        &state,
        Some(&token),
        "remove_domain",
        json!({"app": "groceries", "hostname": "other.example.com"}),
    )
    .await;
    assert_eq!(is_error(&body)["error"], "invalid_argument");
    assert!(fake.calls("DELETE").is_empty());
    let (_, body) = call(
        &state,
        Some(&token),
        "remove_domain",
        json!({"app": "groceries", "hostname": "shop.example.com"}),
    )
    .await;
    assert_eq!(
        result(&body)["structuredContent"]["removed"],
        "shop.example.com"
    );
    assert_eq!(fake.calls("DELETE").len(), 1);
    let (_, body) = call(
        &state,
        Some(&token),
        "list_domains",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(result(&body)["structuredContent"]["domains"], json!([]));
    let (_, body) = call(
        &state,
        Some(&token),
        "remove_domain",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(is_error(&body)["error"], "not_found");
    // Foreign slugs answer exactly like missing ones.
    let (_, body) = call(
        &state,
        Some(&bob),
        "list_domains",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(is_error(&body)["error"], "not_found");
}

/// Deleting an app removes its domain at Cloudflare too (spec §7.7): the
/// row goes and the DELETE reaches the fake.
#[tokio::test]
async fn delete_app_removes_the_custom_hostname() {
    let (mut state, _) = setup().await;
    let fake = crate::cloudflare::fake_saas::FakeSaaS::start().await;
    point_zone_at_fake(&mut state, &fake);
    let token = pro_app(&state, "ana@example.com", "groceries").await;
    let (_, body) = call(
        &state,
        Some(&token),
        "add_domain",
        json!({"app": "groceries", "hostname": "shop.example.com"}),
    )
    .await;
    assert_eq!(
        result(&body)["structuredContent"]["domains"][0]["status"],
        "pending"
    );
    let (_, body) = call(
        &state,
        Some(&token),
        "delete_app",
        json!({"app": "groceries", "confirm": "groceries"}),
    )
    .await;
    assert_eq!(result(&body)["structuredContent"]["deleted"], true);
    assert_eq!(
        fake.calls("DELETE").len(),
        1,
        "the custom hostname goes with the app"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app_domain")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

/// `delete_account` deletes owned apps through the `delete_app` path, so
/// their custom hostnames go at Cloudflare too — not just the local rows.
#[tokio::test]
async fn delete_account_removes_custom_hostnames() {
    let (mut state, _) = setup().await;
    let fake = crate::cloudflare::fake_saas::FakeSaaS::start().await;
    point_zone_at_fake(&mut state, &fake);
    let token = pro_app(&state, "ana@example.com", "groceries").await;
    let (_, body) = call(
        &state,
        Some(&token),
        "add_domain",
        json!({"app": "groceries", "hostname": "shop.example.com"}),
    )
    .await;
    assert_eq!(
        result(&body)["structuredContent"]["domains"][0]["hostname"],
        "shop.example.com"
    );
    let (_, body) = call(
        &state,
        Some(&token),
        "delete_account",
        json!({"confirm": "ana@example.com"}),
    )
    .await;
    assert_eq!(result(&body)["structuredContent"]["deleted"], true);
    assert_eq!(
        result(&body)["structuredContent"]["deleted_apps"],
        json!(["groceries"])
    );
    assert_eq!(
        fake.calls("DELETE").len(),
        1,
        "the custom hostname goes with the account's app"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app_domain")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

/// Jev stand-ins for the slug/name check: flag any text holding `flagme`
/// (slug hyphens already became spaces), clear everything else.
mod slug_check_tests {
    use super::*;
    use crate::moderation::jev::fake::{envelope, FakeJev, Responder};
    use crate::moderation::{JevClient, JevConfig};

    fn slug_test_client(base_url: &str) -> JevClient {
        let mut config = JevConfig::new(
            "test-account".to_string(),
            "test-token".to_string(),
            "outer".to_string(),
        );
        config.api_base = base_url.to_string();
        config.timeout = std::time::Duration::from_millis(500);
        JevClient::new(config).unwrap()
    }

    fn flagging_responder() -> Responder {
        std::sync::Arc::new(|body| {
            let state = body.pointer("/input/state").unwrap().as_object().unwrap();
            let questions = body
                .pointer("/input/questions")
                .unwrap()
                .as_object()
                .unwrap();
            let mut answers = serde_json::Map::new();
            for (qid, question) in questions {
                let slot = qid.split('_').next().unwrap_or("");
                let text = state
                    .get(slot)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let flagged = text.contains("flagme");
                let value = if question.get("type").and_then(|t| t.as_str()) == Some("score") {
                    serde_json::json!({"type": "score", "score": 0.0})
                } else {
                    serde_json::json!({"type": "noul", "noul": if flagged { 0.95 } else { 0.01 }})
                };
                answers.insert(qid.clone(), value);
            }
            (200, envelope(serde_json::json!({"answers": answers})))
        })
    }

    #[tokio::test]
    async fn create_and_rename_refuse_brand_slugs_with_the_rule() {
        let (state, _) = setup().await;
        let token = api_key_for(&state, "ana@example.com").await;
        // Create refuses the brand slug with the plain-language rule.
        let (_, body) = call(
            &state,
            Some(&token),
            "create_app",
            json!({"slug": "paypal-login"}),
        )
        .await;
        let err = is_error(&body);
        assert_eq!(err["error"], "invalid_argument");
        let message = err["message"].as_str().unwrap();
        assert!(
            message.contains("paypal") && message.contains("aren't allowed"),
            "create refusal names the brand and the rule: {body}"
        );
        // Rename refuses the same way, before claiming any quota.
        create_app(&state, &token, "groceries").await;
        let (_, body) = call(
            &state,
            Some(&token),
            "rename_app",
            json!({"app": "groceries", "slug": "secure-bank-verify"}),
        )
        .await;
        let err = is_error(&body);
        assert_eq!(err["error"], "invalid_argument");
        let message = err["message"].as_str().unwrap();
        assert!(
            message.contains("bank") && message.contains("aren't allowed"),
            "rename refusal names the brand and the rule: {body}"
        );
    }

    #[tokio::test]
    async fn jev_flagged_slug_and_name_are_refused_without_a_claim() {
        let (mut state, _) = setup().await;
        let fake = FakeJev::start(flagging_responder()).await;
        state.jev = Some(slug_test_client(&fake.base_url));
        let token = api_key_for(&state, "ana@example.com").await;
        // A flagged slug refuses.
        let (_, body) = call(
            &state,
            Some(&token),
            "create_app",
            json!({"slug": "flagme-shop"}),
        )
        .await;
        let err = is_error(&body);
        assert_eq!(err["error"], "invalid_argument");
        assert!(
            err["message"].as_str().unwrap().contains("flagged"),
            "flagged slug refused: {body}"
        );
        // A clean slug with a flagged display name refuses too, naming it.
        let (_, body) = call(
            &state,
            Some(&token),
            "create_app",
            json!({"slug": "clean-shop", "name": "Flagme Store"}),
        )
        .await;
        let err = is_error(&body);
        assert_eq!(err["error"], "invalid_argument");
        assert!(
            err["message"].as_str().unwrap().contains("Flagme Store"),
            "flagged name refused: {body}"
        );
        // Brand slugs never reach Jev: the cheap checks run first.
        let (_, body) = call(
            &state,
            Some(&token),
            "create_app",
            json!({"slug": "paypal-login"}),
        )
        .await;
        assert_eq!(is_error(&body)["error"], "invalid_argument");
        assert_eq!(fake.calls(), 2, "only the two Jev-checked names classify");
        // Rename applies the same check to its target slug.
        create_app(&state, &token, "groceries").await;
        let (_, body) = call(
            &state,
            Some(&token),
            "rename_app",
            json!({"app": "groceries", "slug": "flagme-shop"}),
        )
        .await;
        let err = is_error(&body);
        assert_eq!(err["error"], "invalid_argument");
        assert!(
            err["message"].as_str().unwrap().contains("flagged"),
            "flagged rename target refused: {body}"
        );
    }

    #[tokio::test]
    async fn jev_outage_fails_the_name_check_closed_with_a_retry() {
        let (mut state, _) = setup().await;
        let fake = FakeJev::start(std::sync::Arc::new(|_| {
            (500, serde_json::json!({"error": "gateway down"}))
        }))
        .await;
        state.jev = Some(slug_test_client(&fake.base_url));
        let token = api_key_for(&state, "ana@example.com").await;
        // Create fails closed with the retryable line.
        let (_, body) = call(
            &state,
            Some(&token),
            "create_app",
            json!({"slug": "clean-shop"}),
        )
        .await;
        let err = is_error(&body);
        assert_eq!(err["error"], "internal");
        assert!(
            err["message"]
                .as_str()
                .unwrap()
                .contains("try again in a minute"),
            "create outage is retryable: {body}"
        );
        // Rename answers not_found for a missing app before ever reaching
        // Jev; with the app in place the outage fails closed the same way.
        let (_, body) = call(
            &state,
            Some(&token),
            "rename_app",
            json!({"app": "groceries", "slug": "clean-shop"}),
        )
        .await;
        assert_eq!(is_error(&body)["error"], "not_found");
        // Seed with the check disabled: the seed itself must not fail closed.
        state.jev = None;
        create_app(&state, &token, "groceries").await;
        state.jev = Some(slug_test_client(&fake.base_url));
        let (_, body) = call(
            &state,
            Some(&token),
            "rename_app",
            json!({"app": "groceries", "slug": "clean-shop"}),
        )
        .await;
        let err = is_error(&body);
        assert_eq!(err["error"], "internal");
        assert!(
            err["message"]
                .as_str()
                .unwrap()
                .contains("try again in a minute"),
            "rename outage is retryable: {body}"
        );
    }
}

/// Point one test state's renderer at the fake Cloudflare endpoint.
fn with_fake_browser(state: &mut McpState, fake: &crate::browser::fake::FakeRender) {
    state.browser = Some(fake.render());
}

fn snapshot_body(screenshot_b64: &str, status: i64, title: &str) -> Value {
    json!({
        "success": true,
        "result": {"screenshot": screenshot_b64, "content": "<html></html>"},
        "meta": {"status": status, "title": title},
    })
}

fn jpeg_b64() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(crate::browser::test_jpeg_bytes())
}

#[tokio::test]
async fn look_at_app_renders_screenshot_and_status() {
    let (mut state, _) = setup().await;
    let fake = crate::browser::fake::FakeRender::start().await;
    with_fake_browser(&mut state, &fake);
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    fake.program(200, snapshot_body(&jpeg_b64(), 200, "Groceries"));

    let (_, body) = call(
        &state,
        Some(&token),
        "look_at_app",
        json!({"app": "groceries"}),
    )
    .await;
    let out = result(&body);
    assert!(out.get("isError").is_none(), "render succeeds: {body}");
    let content = out["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert!(content[0]["text"].as_str().unwrap().contains("HTTP 200"));
    assert!(content[0]["text"]
        .as_str()
        .unwrap()
        .contains(super::tools::UNTRUSTED_NOTICE));
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/jpeg");
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD
        .decode(content[1]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(image, crate::browser::test_jpeg_bytes());
    let structured = &out["structuredContent"];
    assert_eq!(structured["url"], "http://groceries.outer.localhost/");
    assert_eq!(structured["http_status"], 200);
    assert_eq!(structured["title"], "Groceries");
    assert_eq!(structured["width"], 1280);
    // The request carries the render knobs: JPEG, viewport width, goto budget.
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["url"], "http://groceries.outer.localhost/");
    assert_eq!(calls[0]["screenshotOptions"]["type"], "jpeg");
}

#[tokio::test]
async fn look_at_app_validates_before_touching_the_network() {
    let (mut state, _) = setup().await;
    let fake = crate::browser::fake::FakeRender::start().await;
    with_fake_browser(&mut state, &fake);
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    fake.program(200, snapshot_body(&jpeg_b64(), 200, "Groceries"));

    for args in [
        json!({"app": "groceries", "width": 800}),
        json!({"app": "groceries", "path": "https://evil.example/"}),
        json!({"app": "groceries", "path": "/a\nb"}),
        json!({"app": "no-such-app"}),
    ] {
        let (_, body) = call(&state, Some(&token), "look_at_app", args).await;
        let err = is_error(&body);
        assert!(
            err["error"] == "invalid_argument" || err["error"] == "not_found",
            "bad input refuses: {body}"
        );
    }
    assert!(fake.calls().is_empty(), "no render runs before validation");
}

#[tokio::test]
async fn look_at_app_uses_the_active_custom_domain() {
    let (mut state, _) = setup().await;
    let fake = crate::browser::fake::FakeRender::start().await;
    with_fake_browser(&mut state, &fake);
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let app = crate::apps::get_by_slug(&state.pool, "groceries")
        .await
        .unwrap()
        .unwrap();
    sqlx::query(
        "INSERT INTO app_domain (hostname, app_id, cf_custom_hostname_id, status, created_at) \
         VALUES (?1, ?2, ?3, 'active', ?4)",
    )
    .bind("shop.example.com")
    .bind(&app.id)
    .bind("cf-host-1")
    .bind(crate::auth::now_ms())
    .execute(&state.pool)
    .await
    .unwrap();
    fake.program(200, snapshot_body(&jpeg_b64(), 200, "Shop"));

    let (_, body) = call(
        &state,
        Some(&token),
        "look_at_app",
        json!({"app": "groceries", "path": "/about", "width": 390, "full_page": true}),
    )
    .await;
    let out = result(&body);
    assert_eq!(
        out["structuredContent"]["url"],
        "http://shop.example.com/about"
    );
    assert_eq!(fake.calls()[0]["url"], "http://shop.example.com/about");
    assert_eq!(fake.calls()[0]["viewport"]["width"], 390);
    assert_eq!(fake.calls()[0]["screenshotOptions"]["fullPage"], true);
}

#[tokio::test]
async fn look_at_app_counts_hourly_quota_then_rate_limits() {
    let (mut state, _) = setup().await;
    let fake = crate::browser::fake::FakeRender::start().await;
    with_fake_browser(&mut state, &fake);
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    fake.program(200, snapshot_body(&jpeg_b64(), 200, "Groceries"));

    for _ in 0..crate::browser::LOOKS_PER_HOUR {
        let (_, body) = call(
            &state,
            Some(&token),
            "look_at_app",
            json!({"app": "groceries"}),
        )
        .await;
        assert!(result(&body).get("isError").is_none(), "looks pass: {body}");
    }
    let (_, body) = call(
        &state,
        Some(&token),
        "look_at_app",
        json!({"app": "groceries"}),
    )
    .await;
    let err = is_error(&body);
    assert_eq!(err["error"], "rate_limited");
    assert!(err["retry_after_s"].as_u64().unwrap() >= 1, "{err}");
    assert_eq!(fake.calls().len(), crate::browser::LOOKS_PER_HOUR as usize);
}

#[tokio::test]
async fn look_at_app_refuses_without_browser_and_on_render_failure() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    // No Cloudflare pair (localhost dev): the tool refuses, no panic.
    let (_, body) = call(
        &state,
        Some(&token),
        "look_at_app",
        json!({"app": "groceries"}),
    )
    .await;
    let err = is_error(&body);
    assert_eq!(err["error"], "internal");
    assert!(
        err["message"].as_str().unwrap().contains("not configured"),
        "{err}"
    );

    // Cloudflare refuses the render: internal, and the quota claim stands
    // (the browser session still ran).
    let (mut state, _) = setup().await;
    let fake = crate::browser::fake::FakeRender::start().await;
    state.browser = Some(fake.render());
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    fake.program(400, json!({"success": false, "errors": ["bad page"]}));
    let (_, body) = call(
        &state,
        Some(&token),
        "look_at_app",
        json!({"app": "groceries"}),
    )
    .await;
    let err = is_error(&body);
    assert_eq!(err["error"], "internal");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("try again in a minute"),
        "{err}"
    );
    let used: i64 =
        sqlx::query_scalar("SELECT looks FROM look_hour WHERE userId = (SELECT id FROM user WHERE email = 'ana@example.com')")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(used, 1);
}

fn look_page_with_capture() -> String {
    "<html><head>\
        <script>recorder</script>\
        <script type=\"application/json\" id=\"__outer_look\">{\"console\":\
        [{\"level\":\"error\",\"message\":\"boom\",\"source\":\"app.js:3\"}],\
        \"failed\":[{\"url\":\"/api/x\",\"status\":500}]}\
        </script></head><body></body></html>"
        .to_string()
}

#[tokio::test]
async fn look_at_app_returns_console_and_failed_requests() {
    let (mut state, _) = setup().await;
    let fake = crate::browser::fake::FakeRender::start().await;
    with_fake_browser(&mut state, &fake);
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    fake.program(
        200,
        json!({
            "success": true,
            "result": {"screenshot": jpeg_b64(), "content": look_page_with_capture()},
            "meta": {"status": 200, "title": "Groceries"},
        }),
    );

    let (_, body) = call(
        &state,
        Some(&token),
        "look_at_app",
        json!({"app": "groceries"}),
    )
    .await;
    let out = result(&body);
    assert!(out.get("isError").is_none(), "render succeeds: {body}");
    let structured = &out["structuredContent"];
    assert_eq!(structured["capture"], "ok");
    assert_eq!(structured["console"][0]["level"], "error");
    assert_eq!(structured["console"][0]["message"], "boom");
    assert_eq!(structured["console"][0]["source"], "app.js:3");
    assert_eq!(structured["failed_requests"][0]["url"], "/api/x");
    assert_eq!(structured["failed_requests"][0]["status"], 500);
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("Console: 1 entry, failed requests: 1."),
        "{text}"
    );
    assert!(text.contains("[error] boom"), "{text}");
    assert!(text.contains("/api/x -> 500"), "{text}");
    assert!(
        !text.contains("cannot report console errors"),
        "no stale gap note when capture worked: {text}"
    );
    // The render asked Cloudflare to send back a capture token.
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    let token = calls[0]["setExtraHTTPHeaders"]["x-outer-look"]
        .as_str()
        .expect("capture token sent");
    assert_eq!(token.len(), 32, "128-bit hex token");
}

#[tokio::test]
async fn look_at_app_reports_precise_capture_notes() {
    let (mut state, _) = setup().await;
    let fake = crate::browser::fake::FakeRender::start().await;
    with_fake_browser(&mut state, &fake);
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    for (content, capture, note) in [
        (
            "<!--__outer_look:csp--><html><body>x</body></html>",
            "csp-blocked",
            "blocked by the app's CSP",
        ),
        (
            "<!--__outer_look:oversize--><html><body>x</body></html>",
            "oversize",
            "exceeded",
        ),
        ("{\"json\": true}", "not-html", "not an HTML document"),
        (
            "<html><body>no recorder</body></html>",
            "missing",
            "did not report",
        ),
    ] {
        fake.program(
            200,
            json!({
                "success": true,
                "result": {"screenshot": jpeg_b64(), "content": content},
                "meta": {"status": 200, "title": "Shop"},
            }),
        );
        let (_, body) = call(
            &state,
            Some(&token),
            "look_at_app",
            json!({"app": "groceries"}),
        )
        .await;
        let out = result(&body);
        assert_eq!(out["structuredContent"]["capture"], capture);
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(note), "{capture}: {text}");
        assert!(
            out["structuredContent"]["console"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{capture}"
        );
    }
}

async fn export_call(state: &McpState, token: &str, slug: &str, sha: Option<&str>) -> Value {
    let mut args = json!({"app": slug});
    if let Some(sha) = sha {
        args["sha"] = json!(sha);
    }
    let (_, body) = call(state, Some(token), "export_code", args).await;
    body
}

/// Read a stored zip into `name -> bytes`, for export assertions.
fn zip_map(bytes: Vec<u8>) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut out = std::collections::BTreeMap::new();
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut buf).unwrap();
        out.insert(file.name().to_string(), buf);
    }
    out
}

/// Raw blob bytes straight from the mirror: the exact-bytes oracle for
/// export assertions.
fn blob_bytes(state: &McpState, sha: &str, path: &str) -> Vec<u8> {
    let bare = crate::git::manifest::mirror_path(&state.git, "groceries");
    let out = std::process::Command::new("git")
        .arg(format!("--git-dir={}", bare.display()))
        .args(["cat-file", "blob", &format!("{sha}:{path}")])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "cat-file {sha}:{path}");
    out.stdout
}

#[tokio::test]
async fn export_code_needs_a_pro_owner() {
    let (state, _) = setup().await;
    let owner = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &owner, "groceries").await;

    // A Free owner is refused with payment_required, pointing at Pro.
    let body = export_call(&state, &owner, "groceries", None).await;
    let err = is_error(&body);
    assert_eq!(err["error"], "payment_required");
    assert!(
        err["user_message"].as_str().unwrap().contains("Pro"),
        "{err}"
    );

    set_plan(&state, "ana@example.com", "pro").await;
    let body = export_call(&state, &owner, "groceries", None).await;
    let content = result(&body)["structuredContent"].clone();
    let url = content["url"].as_str().expect("download url");
    assert!(url.contains("response-content-disposition="), "{url}");
    assert!(url.contains("attachment"), "{url}");
    let sha = content["sha"].as_str().expect("sha");
    assert_eq!(sha.len(), 40, "{content}");
    assert!(content["bytes"].as_u64().unwrap() > 0, "{content}");
    let expires =
        chrono::DateTime::parse_from_rfc3339(content["expires_at"].as_str().unwrap()).unwrap();
    let delta = expires
        .signed_duration_since(chrono::Utc::now())
        .num_seconds();
    assert!((3500..=3700).contains(&delta), "{delta}");

    // The object and its row exist under the spec key shape.
    let keys = state.store.list("outer/exports/groceries/").await.unwrap();
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(
        keys[0].starts_with(&format!("outer/exports/groceries/{}-", &sha[..7])),
        "{keys:?}"
    );
    assert!(keys[0].ends_with(".zip"), "{keys:?}");
    let row: Option<String> = sqlx::query_scalar("SELECT sha FROM app_export WHERE key = ?1")
        .bind(&keys[0])
        .fetch_optional(&state.pool)
        .await
        .unwrap();
    assert_eq!(row.as_deref(), Some(sha));
}

#[tokio::test]
async fn export_code_zip_matches_the_tree_at_sha() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    let (_, commit) = call(
        &state,
        Some(&owner),
        "commit",
        json!({"app": "groceries", "message": "add note",
               "writes": [{"path": "note.txt", "content": "hi\n"}]}),
    )
    .await;
    assert!(result(&commit).get("isError").is_none(), "{commit}");
    let (_, hist) = call(&state, Some(&owner), "history", json!({"app": "groceries"})).await;
    let commits = result(&hist)["structuredContent"]["commits"]
        .as_array()
        .unwrap()
        .clone();
    assert!(commits.len() >= 2, "{commits:?}");
    let head = commits[0]["sha"].as_str().unwrap().to_string();
    let first = commits[1]["sha"].as_str().unwrap().to_string();

    // The oracle is the same git read paths the Code view uses: every blob
    // `list_files` names at the sha lands in the zip under the top folder
    // with identical bytes, and nothing else does.
    for sha in [&head, &first] {
        let body = export_call(&state, &owner, "groceries", Some(sha)).await;
        let content = result(&body)["structuredContent"].clone();
        assert_eq!(content["sha"], json!(sha), "{body}");
        let (_, listed) = call(
            &state,
            Some(&owner),
            "list_files",
            json!({"app": "groceries", "sha": sha}),
        )
        .await;
        let files = result(&listed)["structuredContent"]["files"]
            .as_array()
            .unwrap()
            .clone();
        let short = &sha[..7];
        let mut expected = std::collections::BTreeMap::new();
        for file in &files {
            let path = file["path"].as_str().unwrap();
            // Ground truth is the blob itself: `read_file` trims a trailing
            // newline for display, so it cannot serve as the oracle here.
            expected.insert(
                format!("groceries-{short}/{path}"),
                blob_bytes(&state, sha, path),
            );
        }
        let key: String = sqlx::query_scalar("SELECT key FROM app_export WHERE sha = ?1")
            .bind(sha)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let bytes = state.store.get(&key).await.unwrap().expect("zip stored");
        assert_eq!(zip_map(bytes), expected, "zip of {sha}");
    }
    // The earlier commit predates the note: its zip lacks the file.
    let old_key: String = sqlx::query_scalar("SELECT key FROM app_export WHERE sha = ?1")
        .bind(&first)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let old_bytes = state.store.get(&old_key).await.unwrap().unwrap();
    assert!(
        !zip_map(old_bytes)
            .keys()
            .any(|name| name.ends_with("note.txt")),
        "old tree carries the new file"
    );
}

#[tokio::test]
async fn export_code_is_owner_only() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    invite(&state, &owner, "groceries", "bruno@example.com").await;
    let bruno = api_key_for(&state, "bruno@example.com").await;
    let stranger = api_key_for(&state, "stranger@example.com").await;

    // A collaborator reads forbidden; a slug with no role reads not_found.
    let body = export_call(&state, &bruno, "groceries", None).await;
    assert_eq!(is_error(&body)["error"], "forbidden");
    let body = export_call(&state, &stranger, "groceries", None).await;
    assert_eq!(is_error(&body)["error"], "not_found");

    // Bad shapes and unknown commits are the caller's bug.
    let body = export_call(&state, &owner, "groceries", Some("!!!")).await;
    assert_eq!(is_error(&body)["error"], "invalid_argument");
    let body = export_call(&state, &owner, "groceries", Some("deadbeef")).await;
    assert_eq!(is_error(&body)["error"], "invalid_argument");
}

#[tokio::test]
async fn export_caps_refuse_oversized_trees() {
    use crate::git::export::{export_app, ExportCaps, ExportDeps, ExportError, ExportParams};
    let (state, _) = setup().await;
    pro_app(&state, "ana@example.com", "groceries").await;
    let app_id = app_id(&state, "groceries").await;
    let now = chrono::Utc::now();
    let deps = ExportDeps {
        pool: &state.pool,
        store: &state.store,
        git: &state.git,
        sync: &state.sync,
    };
    let tiny = ExportCaps {
        max_total_bytes: 10,
        ..ExportCaps::default()
    };
    let err = export_app(
        &deps,
        ExportParams {
            slug: "groceries",
            app_id: &app_id,
            rev: None,
            caps: &tiny,
            now,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ExportError::TooLarge { .. }), "{err:?}");
    let few = ExportCaps {
        max_total_bytes: u64::MAX,
        max_file_bytes: u64::MAX,
        max_files: 0,
    };
    let err = export_app(
        &deps,
        ExportParams {
            slug: "groceries",
            app_id: &app_id,
            rev: None,
            caps: &few,
            now,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ExportError::TooManyFiles(_)), "{err:?}");
    let small = ExportCaps {
        max_total_bytes: u64::MAX,
        max_file_bytes: 1,
        max_files: 100_000,
    };
    let err = export_app(
        &deps,
        ExportParams {
            slug: "groceries",
            app_id: &app_id,
            rev: None,
            caps: &small,
            now,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ExportError::FileTooLarge { .. }), "{err:?}");
    let full = ExportCaps::default();
    let ok = export_app(
        &deps,
        ExportParams {
            slug: "groceries",
            app_id: &app_id,
            rev: None,
            caps: &full,
            now,
        },
    )
    .await;
    assert!(ok.is_ok(), "default caps fit the test tree");
}

#[tokio::test]
async fn export_prune_deletes_only_exports_older_than_a_day() {
    use crate::git::export::prune_exports;
    let (state, _) = setup().await;
    pro_app(&state, "ana@example.com", "groceries").await;
    let app_id = app_id(&state, "groceries").await;
    let now = crate::apps::now_ms();
    for (key, age_ms) in [
        ("outer/exports/groceries/abc1234-aaaa.zip", 25 * 3_600_000),
        ("outer/exports/groceries/abc1234-bbbb.zip", 3_600_000),
    ] {
        state.store.put(key, b"zip".to_vec()).await.unwrap();
        sqlx::query(
            "INSERT INTO app_export (key, app_id, sha, bytes, created_at) VALUES (?1, ?2, 'abc', 3, ?3)",
        )
        .bind(key)
        .bind(&app_id)
        .bind(now - age_ms)
        .execute(&state.pool)
        .await
        .unwrap();
    }
    let pruned = prune_exports(&state.pool, &state.store, now).await.unwrap();
    assert_eq!(pruned, 1);
    assert!(
        state
            .store
            .get("outer/exports/groceries/abc1234-aaaa.zip")
            .await
            .unwrap()
            .is_none(),
        "old export survives the sweep"
    );
    assert!(
        state
            .store
            .get("outer/exports/groceries/abc1234-bbbb.zip")
            .await
            .unwrap()
            .is_some(),
        "fresh export pruned too early"
    );
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM app_export ORDER BY key")
        .fetch_all(&state.pool)
        .await
        .unwrap();
    assert_eq!(keys, vec!["outer/exports/groceries/abc1234-bbbb.zip"]);
}
