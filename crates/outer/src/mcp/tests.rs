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
    assert_eq!(anon.len(), 25);
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
    assert_eq!(authed.len(), 23);
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
            "app_logs"
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
    sqlx::query(
        "INSERT INTO usage_day (account_id, day, code_changes) VALUES (?1, date('now'), 49)",
    )
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

    // GET on /mcp is 405; the platform router still serves llms.txt.
    let app = crate::http::router(app_state(&state));
    use tower::ServiceExt as _;
    let get = app
        .clone()
        .oneshot(Request::get("/mcp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::METHOD_NOT_ALLOWED);
    let llms = app
        .oneshot(Request::get("/llms.txt").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(llms.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(llms.into_body(), usize::MAX)
        .await
        .unwrap();
    let page = String::from_utf8(bytes.to_vec()).unwrap();
    for tool in registry() {
        assert!(
            page.contains(&format!("`{}`", tool.name)),
            "llms.txt misses {}",
            tool.name
        );
    }
    assert!(page.contains("claude mcp add"));
    assert!(page.contains("codex mcp add"));
    assert!(page.contains("outer.localhost"));
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
        sqlx::query_scalar("SELECT code_changes FROM usage_day WHERE account_id = ?1")
            .bind(account_id)
            .fetch_one(&state.pool)
            .await
            .unwrap()
    }
    sqlx::query(
        "INSERT INTO usage_day (account_id, day, code_changes) VALUES (?1, date('now'), 10)",
    )
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
    sqlx::query("UPDATE usage_day SET code_changes = 49 WHERE account_id = ?1")
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
