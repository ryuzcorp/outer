//! End-to-end MCP tests: JSON-RPC envelope, lazy auth, every tool group and
//! the OTP sign-in round trip. Deterministic: memory store, temp dirs, fake
//! deploys, captured email codes — no celld, no network.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use serde_json::{json, Value};

use super::registry::registry;
use super::{mcp_handler, FakeOutcome, McpState, INSTRUCTIONS, LATEST_PROTOCOL};
use crate::auth::test_support as auth_ts;
use crate::fleet::manager::{FleetManager, ManagerSettings};
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

/// A tiny stand-in for the vendored templates: three files in each
/// template folder, one commit, bundled straight into the memory store.
async fn vendor_template(store: &Store) {
    crate::templates::vendor_test_templates(
        store,
        &[
            ("package.json", "{}\n"),
            ("pages/index.tsx", "<h1>Groceries</h1>\n"),
            (
                "lib/moderate.ts",
                "export function moderate(s: string) { return s; }\n",
            ),
        ],
    )
    .await;
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
    let mut structured = result(&body)["structuredContent"].clone();
    // Under a loaded parallel suite the spawned fake deploy can outlast the
    // short test `tool_wait`, so the call answers `building`: follow the
    // deploy to its terminal row instead of asserting on the first answer.
    // (Tests that want the in-progress shape call the tool directly.)
    if structured["deploy"]["status"] == "building" {
        for _ in 0..100 {
            let (_, status) = call(
                state,
                Some(token),
                "deploy_status",
                json!({"app": slug, "wait": true}),
            )
            .await;
            structured["deploy"] = result(&status)["structuredContent"].clone();
            if structured["deploy"]["status"] != "building" {
                break;
            }
        }
    }
    assert_eq!(structured["deploy"]["status"], "success");
    structured
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
    assert_eq!(anon.len(), 61);
    assert!(anon.contains(&"sign_in".to_string()));
    assert!(anon.contains(&"create_app".to_string()));
    assert!(anon.contains(&"add_example".to_string()));

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
    assert_eq!(authed.len(), 59);
    assert!(!authed.contains(&"sign_in".to_string()));
    assert!(!authed.contains(&"verify_code".to_string()));
    assert!(authed.contains(&"upgrade".to_string()));
    assert!(authed.contains(&"manage_billing".to_string()));

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
            "read_files",
            "search_code",
            "diff_draft",
            "show_commit",
            "deploy_log",
            "app_logs",
            "app_errors",
            "app_metrics",
            "db_query",
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
        "read_files",
        json!({"app": "groceries", "files": [{"path": "pages/index.tsx"}]}),
    )
    .await;
    let read = result(&read);
    let files = read["structuredContent"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "pages/index.tsx");
    // Numbered lines, exact at the boundaries: the file's single line.
    assert!(files[0]["text"].as_str().unwrap().starts_with("1│ "));
    assert!(files[0]["text"].as_str().unwrap().contains("Groceries"));
    assert_eq!(files[0]["total_lines"], 1);
    assert_eq!(files[0]["start_line"], 1);
    assert_eq!(files[0]["end_line"], 1);
    assert_eq!(files[0]["blob"].as_str().unwrap().len(), 40);
    assert!(read["structuredContent"].get("truncated").is_some());
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
    let sfiles = search["structuredContent"]["files"].as_array().unwrap();
    assert_eq!(sfiles.len(), 1);
    assert_eq!(sfiles[0]["path"], "pages/index.tsx");
    assert_eq!(sfiles[0]["matches"].as_array().unwrap().len(), 1);
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
        ("open_app", json!({"app": "groceries"})),
        (
            "read_files",
            json!({"app": "groceries", "files": [{"path": "pages/index.tsx"}]}),
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
async fn open_app_unknown_sha_and_changed_since_are_not_found() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    // Well-formed but unknown: 40 hex chars naming nothing on main.
    let missing = "0".repeat(40);
    let (_, bad_sha) = call(
        &state,
        Some(&token),
        "open_app",
        json!({"app": "groceries", "sha": missing}),
    )
    .await;
    let err = is_error(&bad_sha);
    assert_eq!(err["error"], "not_found");
    assert!(err["message"].as_str().unwrap().contains("unknown commit"));
    let (_, bad_since) = call(
        &state,
        Some(&token),
        "open_app",
        json!({"app": "groceries", "changed_since": missing}),
    )
    .await;
    let err = is_error(&bad_since);
    assert_eq!(err["error"], "not_found");
    assert!(err["message"].as_str().unwrap().contains("unknown commit"));
}

#[tokio::test]
async fn free_plan_caps_apps_at_two() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "one").await;
    create_app(&state, &token, "two").await;
    let (_, third) = call(&state, Some(&token), "create_app", json!({"slug": "three"})).await;
    let err = is_error(&third);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "apps_owned");
    assert_eq!(err["limit"], 2);
    assert!(err["user_message"]
        .as_str()
        .unwrap()
        .contains("all 2 app slots on your Outer free plan"));

    let (_, who) = call(&state, Some(&token), "whoami", json!({})).await;
    let who = &result(&who)["structuredContent"];
    assert_eq!(who["plan"], "free");
    assert_eq!(who["usage"]["apps_owned"], 2);
    assert_eq!(who["limits"]["apps_owned_max"], 2);
    assert_eq!(who["limits"]["code_changes_per_day"], 40);
    assert_eq!(who["limits"]["build_minutes_per_day"], 30);
    assert_eq!(who["limits"]["requests_per_month"], 100_000);
    assert_eq!(who["limits"]["collaborators_per_app"], 1);
    assert!(who["resets_at"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn create_app_template_selects_the_starter() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    // An unknown id fails naming both options, before any row exists.
    let (_, unknown) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "nope", "template": "blog"}),
    )
    .await;
    let err = is_error(&unknown);
    assert_eq!(err["error"], "invalid_argument");
    let message = err["message"].as_str().unwrap();
    assert!(
        message.contains("\"app\"") && message.contains("\"website\""),
        "names both templates: {message}"
    );
    // The website starter seeds and deploys like the app one.
    let (_, site) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "site", "template": "website"}),
    )
    .await;
    assert_eq!(
        result(&site)["structuredContent"]["deploy"]["status"],
        "success"
    );
    assert_eq!(result(&site)["structuredContent"]["app"]["slug"], "site");
}

/// Read `path` at `sha` from the app's seeded repo.
async fn show_seed_file(state: &McpState, slug: &str, sha: &str, path: &str) -> String {
    let bare = crate::git::manifest::ensure_synced(&state.store, &state.git, &state.sync, slug)
        .await
        .unwrap();
    let git_dir = format!("--git-dir={}", bare.display());
    crate::git::manifest::run_git(
        &[&git_dir, "show", &format!("{sha}:{path}")],
        &[],
        std::time::Duration::from_secs(15),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn create_app_seed_commit_carries_brand_json() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let (_, body) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "pizza", "name": "Pizza Calculator",
                "description": "Split pizza orders fairly between friends."}),
    )
    .await;
    assert_eq!(
        result(&body)["structuredContent"]["deploy"]["status"],
        "success",
        "create failed: {body}"
    );
    let sha = result(&body)["structuredContent"]["deploy"]["sha"]
        .as_str()
        .unwrap()
        .to_string();
    // The first commit carries the given brand, overwriting the template's.
    let shown = show_seed_file(&state, "pizza", &sha, "brand.json").await;
    let brand: serde_json::Value = serde_json::from_str(shown.trim()).unwrap();
    assert_eq!(brand["name"], "Pizza Calculator");
    assert_eq!(brand["shortName"], "Pizza");
    assert_eq!(
        brand["description"],
        "Split pizza orders fairly between friends."
    );
    assert_eq!(brand["accent"], crate::brand::accent_for_slug("pizza"));
    // The app row keeps the same display name.
    let row: Option<String> = sqlx::query_scalar("SELECT name FROM app WHERE slug = 'pizza'")
        .fetch_optional(&state.pool)
        .await
        .unwrap();
    assert_eq!(row.as_deref(), Some("Pizza Calculator"));
}

#[tokio::test]
async fn create_app_brand_defaults_from_the_slug() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let (_, body) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "kalkulator-pizzy"}),
    )
    .await;
    assert_eq!(
        result(&body)["structuredContent"]["deploy"]["status"],
        "success",
        "create failed: {body}"
    );
    let sha = result(&body)["structuredContent"]["deploy"]["sha"]
        .as_str()
        .unwrap()
        .to_string();
    let shown = show_seed_file(&state, "kalkulator-pizzy", &sha, "brand.json").await;
    let brand: serde_json::Value = serde_json::from_str(shown.trim()).unwrap();
    assert_eq!(brand["name"], "Kalkulator Pizzy");
    assert_eq!(brand["shortName"], "Kalkulator");
    assert_eq!(brand["description"], "Kalkulator Pizzy");
    assert_eq!(
        brand["accent"],
        crate::brand::accent_for_slug("kalkulator-pizzy")
    );
    let row: Option<String> =
        sqlx::query_scalar("SELECT name FROM app WHERE slug = 'kalkulator-pizzy'")
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert_eq!(row.as_deref(), Some("Kalkulator Pizzy"));
}

#[tokio::test]
async fn create_app_refuses_overlong_name_and_description() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    // A 161-char description refuses naming the 160 cap, before any row.
    let (_, body) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "pizza", "description": "d".repeat(161)}),
    )
    .await;
    let err = is_error(&body);
    assert_eq!(err["error"], "invalid_argument");
    assert!(
        err["message"].as_str().unwrap().contains("160"),
        "refusal names the cap: {body}"
    );
    // A 61-char name refuses the same way.
    let (_, body) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "pizza", "name": "n".repeat(61)}),
    )
    .await;
    let err = is_error(&body);
    assert_eq!(err["error"], "invalid_argument");
    assert!(
        err["message"].as_str().unwrap().contains("60"),
        "refusal names the cap: {body}"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app WHERE slug = 'pizza'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "refused creates leave no row");
}

#[tokio::test]
async fn free_plan_caps_changes_per_day() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let account_id = crate::apps::ensure_account(&state.pool, "ana@example.com")
        .await
        .unwrap();
    sqlx::query("INSERT INTO usage_day (userId, day, code_changes) VALUES (?1, date('now'), 39)")
        .bind(&account_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (_, fortieth) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "forty",
               "writes": [{"path": "n.txt", "content": "n"}]}),
    )
    .await;
    assert!(result(&fortieth).get("isError").is_none());

    let (_, over) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "forty-one",
               "writes": [{"path": "m.txt", "content": "m"}]}),
    )
    .await;
    let err = is_error(&over);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "code_changes_per_day");
    assert_eq!(err["limit"], 40);
    assert_eq!(err["used"], 40);
    assert!(err["resets_at"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn build_minutes_cap_refuses_a_new_build() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let account_id = crate::apps::ensure_account(&state.pool, "ana@example.com")
        .await
        .unwrap();
    // The full 30 Free minutes are burned: a new build refuses before any
    // git work, spending no code change.
    sqlx::query("INSERT INTO usage_day (userId, day, build_ms) VALUES (?1, date('now'), 1800000)")
        .bind(&account_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let (_, over) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "too late",
               "writes": [{"path": "late.txt", "content": "late"}]}),
    )
    .await;
    let err = is_error(&over);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "build_minutes_per_day");
    assert_eq!(err["limit"], 30);
    assert_eq!(err["used"], 30);
    assert!(err["resets_at"].as_str().unwrap().ends_with('Z'));
    assert!(err["user_message"]
        .as_str()
        .unwrap()
        .contains("upgrade to Pro"));
    let changes: i64 = sqlx::query_scalar("SELECT code_changes FROM usage_day WHERE userId = ?1")
        .bind(&account_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(changes, 0, "a refused build spends no code change");
}

#[tokio::test]
async fn frozen_storage_refuses_commit_until_back_under_cap() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let app_id: String = sqlx::query_scalar("SELECT id FROM app WHERE slug = 'groceries'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE app SET storage_frozen_at = 1, storage_frozen_detail = 'D1 120 MB over the 100 MB cap' WHERE id = ?1",
    )
    .bind(&app_id)
    .execute(&state.pool)
    .await
    .unwrap();
    let (_, over) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "frozen",
               "writes": [{"path": "ice.txt", "content": "ice"}]}),
    )
    .await;
    let err = is_error(&over);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "storage_bytes");
    assert!(
        err["message"].as_str().unwrap().contains("read-only"),
        "{err}"
    );
    assert!(err["user_message"]
        .as_str()
        .unwrap()
        .contains("nothing is deleted"));
}

#[tokio::test]
async fn downgrade_keeps_oldest_collaborator_only() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    invite(&state, &owner, "groceries", "bruno@example.com").await;
    invite(&state, &owner, "groceries", "cara@example.com").await;
    let bruno = api_key_for(&state, "bruno@example.com").await;
    let cara = api_key_for(&state, "cara@example.com").await;
    for key in [&bruno, &cara] {
        let (_, apps) = call(&state, Some(key), "list_apps", json!({})).await;
        assert_eq!(
            result(&apps)["structuredContent"]["apps"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "both collaborators see the app while the owner is Pro"
        );
    }
    // Downgrade: the oldest collaborator keeps access, the second pauses
    // (no rows deleted).
    set_plan(&state, "ana@example.com", "free").await;
    let (_, apps) = call(&state, Some(&bruno), "list_apps", json!({})).await;
    assert_eq!(
        result(&apps)["structuredContent"]["apps"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (_, apps) = call(&state, Some(&cara), "list_apps", json!({})).await;
    assert!(
        result(&apps)["structuredContent"]["apps"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the second collaborator loses access on Free"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app_collaborator")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(rows, 2, "downgrade pauses access, never deletes rows");
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
    // Free allows 2: one app already owned, so both calls pass the early
    // quota check at 1 owned app; the in-lock recheck in `apps::create`
    // lets exactly one through.
    create_app(&state, &token, "pre").await;
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
    assert_eq!(result(&who)["structuredContent"]["usage"]["apps_owned"], 2);
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
    // Racing at the boundary: exactly one claim fits under 40.
    sqlx::query("UPDATE usage_day SET code_changes = 39 WHERE userId = ?1")
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
    assert_eq!(used(&state, &account_id).await, 40);
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
    // The first rollback proceeds: past the cap it reports `building` once
    // the short test `tool_wait` lapses. It is fully awaited, so no
    // scheduler luck is involved.
    let (_, first) = call(
        &state,
        Some(&token),
        "rollback",
        json!({"app": "groceries", "sha": sha1}),
    )
    .await;
    assert!(
        result(&first).get("isError").is_none(),
        "first rollback should proceed, got {first}"
    );
    // Refusal by construction, not by scheduler luck: the test itself holds
    // the app's rollback lock — exactly what a running rollback holds — so
    // the second call must take the fail-fast refusal. Waiting until the
    // first call holds the lock and then racing the second call's preamble
    // against the first's bounded `tool_wait` remainder cannot guarantee
    // overlap: under load the second's lock attempt routinely lands after
    // the first returned `building` and released.
    let app_id = crate::apps::get_by_slug(&state.pool, "groceries")
        .await
        .unwrap()
        .unwrap()
        .id;
    let _held = state
        .rollback_locks
        .try_lock(&app_id)
        .expect("no rollback is running");
    let (_, refused) = call(
        &state,
        Some(&token),
        "rollback",
        json!({"app": "groceries", "sha": sha1}),
    )
    .await;
    assert_eq!(
        is_error(&refused)["error"],
        "rate_limited",
        "second rollback is refused while the first runs"
    );
    assert_eq!(
        is_error(&refused)["retry_after_s"],
        30,
        "the in-flight refusal names its backoff"
    );
    // The coalescing call below takes the same lock: release before it.
    drop(_held);
    // A deploy row stuck non-terminal coalesces: no new commit, no budget
    // spent, just the running deploy reported. (The slow fake only writes
    // its row at completion, so the stuck row is inserted directly.)
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
            "read_files",
            json!({"app": "groceries", "files": [{"path": "pages/index.tsx"}], "sha": "../../etc"}),
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
async fn delete_app_removes_everything_and_frees_the_slug() {
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
    // A Pro export the delete must wipe: the zip object and its row.
    state
        .store
        .put("outer/exports/groceries/abc1234-test.zip", b"zip".to_vec())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO app_export (key, app_id, sha, bytes, created_at) VALUES ('outer/exports/groceries/abc1234-test.zip', ?1, 'abc1234', 3, 0)",
    )
    .bind(&groceries_id)
    .execute(&state.pool)
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
    assert!(
        out.get("quarantined_until").is_none(),
        "delete responses no longer carry a reservation timestamp"
    );
    let user_message = out["user_message"].as_str().unwrap();
    assert_eq!(
        user_message,
        "groceries.outer.localhost is deleted. The name is free to claim again."
    );
    assert_eq!(
        result(&body)["content"][0]["text"].as_str().unwrap(),
        user_message
    );

    // Bucket: both prefixes empty, the sibling app untouched.
    assert!(state.store.list("git/groceries/").await.unwrap().is_empty());
    assert!(state
        .store
        .list("fleets/groceries/")
        .await
        .unwrap()
        .is_empty());
    assert!(state
        .store
        .list("outer/exports/groceries/")
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
    // Rows: the app and every child table go; a plain delete leaves no
    // redirect behind.
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
        "app_export",
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
    let redirect: Option<String> =
        sqlx::query_scalar("SELECT renamed_to FROM slug_redirect WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert_eq!(redirect, None);

    // No reservation: a different user claims the freed slug at once, then
    // hands it back so the original owner reclaims it the same way.
    let cara = api_key_for(&state, "cara@example.com").await;
    create_app(&state, &cara, "groceries").await;
    let (_, gone) = call(
        &state,
        Some(&cara),
        "delete_app",
        json!({"app": "groceries", "confirm": "groceries"}),
    )
    .await;
    assert_eq!(result(&gone)["structuredContent"]["deleted"], true);
    create_app(&state, &ana, "groceries").await;
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

    // A bad file fails inline without failing the call, and the stored
    // summary is still bounded even though the path was never length-checked.
    let huge = "p".repeat(100_000);
    let (_, long) = call(
        &state,
        Some(&ana),
        "read_files",
        json!({"app": "groceries", "files": [{"path": huge}]}),
    )
    .await;
    let long = result(&long);
    assert_eq!(
        long["structuredContent"]["files"][0]["code"],
        "invalid_argument"
    );
    let rows = action_rows(&state, "groceries").await;
    assert_eq!(rows[0].tool, "read_files");
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
async fn collaborator_invite_free_gets_one() {
    let (state, codes) = setup().await;
    let owner = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &owner, "groceries").await;

    // A Free owner may invite one collaborator.
    let added = result(&invite(&state, &owner, "groceries", "Bruno@Example.com").await)
        ["structuredContent"]
        .clone();
    assert_eq!(added["collaborator"]["email"], "bruno@example.com");
    assert_eq!(added["collaborator"]["status"], "pending");

    // The second invite is refused: Free allows 1 per app.
    let refused = invite(&state, &owner, "groceries", "cara@example.com").await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "collaborators");
    assert_eq!(err["limit"], 1);
    assert_eq!(err["used"], 1);
    assert!(err["user_message"]
        .as_str()
        .unwrap()
        .contains("Upgrade to Pro"));

    set_plan(&state, "ana@example.com", "pro").await;
    // Pro raises the cap: the previously refused invite now lands.
    let added = result(&invite(&state, &owner, "groceries", "cara@example.com").await)
        ["structuredContent"]
        .clone();
    assert_eq!(added["collaborator"]["email"], "cara@example.com");
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
            "read_files",
            json!({"app": "groceries", "files": [{"path": "pages/index.tsx"}]}),
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
        ("set_always_on", json!({"app": "groceries", "on": true})),
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
    invite(&state, &owner, "groceries", "cara@example.com").await;
    let bruno = api_key_for(&state, "bruno@example.com").await;
    let cara = api_key_for(&state, "cara@example.com").await;
    let (_, seen) = call(&state, Some(&bruno), "get_app", json!({"app": "groceries"})).await;
    assert!(result(&seen).get("isError").is_none());

    // Downgrade: the rows stay, the oldest collaborator keeps access, the
    // second reads the app as missing (Free allows 1 per app).
    set_plan(&state, "ana@example.com", "free").await;
    let (_, seen) = call(&state, Some(&bruno), "get_app", json!({"app": "groceries"})).await;
    assert!(result(&seen).get("isError").is_none(), "{seen}");
    let (_, hidden) = call(&state, Some(&cara), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(is_error(&hidden)["error"], "not_found");
    let (_, apps) = call(&state, Some(&cara), "list_apps", json!({})).await;
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
        2
    );

    // Re-upgrade: access is back with no re-invite.
    set_plan(&state, "ana@example.com", "pro").await;
    let (_, back) = call(&state, Some(&cara), "get_app", json!({"app": "groceries"})).await;
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

/// `on` is a Pro feature, gated in the shared domain function: a Free
/// owner is refused with `payment_required`, `off` is always allowed, and
/// Pro flips the row. A collaborator is refused with `forbidden`.
#[tokio::test]
async fn set_always_on_is_pro_gated() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &ana, "groceries").await;

    let (_, refused) = call(
        &state,
        Some(&ana),
        "set_always_on",
        json!({"app": "groceries", "on": true}),
    )
    .await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "payment_required");
    assert!(
        err["user_message"].as_str().unwrap().contains("Pro"),
        "{err}"
    );

    let (_, off) = call(
        &state,
        Some(&ana),
        "set_always_on",
        json!({"app": "groceries", "on": false}),
    )
    .await;
    assert_eq!(result(&off)["structuredContent"]["always_on"], false);

    set_plan(&state, "ana@example.com", "pro").await;
    let (_, on) = call(
        &state,
        Some(&ana),
        "set_always_on",
        json!({"app": "groceries", "on": true}),
    )
    .await;
    assert_eq!(result(&on)["structuredContent"]["always_on"], true);
    let flag: i64 = sqlx::query_scalar("SELECT always_on FROM app WHERE slug = 'groceries'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(flag, 1);
}

/// New Pro apps default to keep-warm; Free apps start cold.
#[tokio::test]
async fn new_pro_apps_default_to_keep_warm() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    let (_, warm) = call(&state, Some(&owner), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(result(&warm)["structuredContent"]["always_on"], true);

    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &bob, "bobshop").await;
    let (_, cold) = call(&state, Some(&bob), "get_app", json!({"app": "bobshop"})).await;
    assert_eq!(result(&cold)["structuredContent"]["always_on"], false);
}

/// A failing keep-warm flag write must not orphan the app: creation still
/// succeeds with the flag off (set it later), instead of erroring after
/// the row exists.
#[tokio::test]
async fn create_app_survives_flag_write_failure() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    set_plan(&state, "ana@example.com", "pro").await;
    sqlx::query(
        "CREATE TRIGGER flag_fail BEFORE UPDATE OF always_on ON app \
         BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    create_app(&state, &token, "groceries").await;
    let (_, body) = call(&state, Some(&token), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(result(&body)["structuredContent"]["always_on"], false);
    let flag: i64 = sqlx::query_scalar("SELECT always_on FROM app WHERE slug = 'groceries'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(flag, 0, "row and response agree: the flag stays off");
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
async fn rename_app_moves_everything_and_redirects_without_blocking() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;
    create_app(&state, &bob, "bobapp").await;
    let id_before = app_id(&state, "groceries").await;

    // Fake deploys write no bucket objects, so seed both prefixes by hand:
    // a deploy pointer plus a D1-shaped object that must survive the move.
    // Exports are 24 h-ephemeral links: the rename drops them instead of
    // moving them, so seed one object plus its row.
    for (key, body) in [
        ("fleets/groceries/deploy/current.json", "{}"),
        ("fleets/groceries/cells/d1.sqlite", "D1DATA"),
        ("fleets/bobapp/deploy/current.json", "{}"),
        ("outer/exports/groceries/abc1234-test.zip", "zip"),
    ] {
        state
            .store
            .put(key, body.as_bytes().to_vec())
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO app_export (key, app_id, sha, bytes, created_at) VALUES ('outer/exports/groceries/abc1234-test.zip', ?1, 'abc1234', 3, 0)",
    )
    .bind(&id_before)
    .execute(&state.pool)
    .await
    .unwrap();
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
    assert_eq!(structured["old_slug"], "groceries");
    assert!(
        structured.get("quarantined_until").is_none(),
        "rename responses no longer carry a reservation timestamp"
    );
    // The app ran (fresh create leaves it awake with a successful deploy),
    // so it redeployed under the new name through the fake.
    assert_eq!(structured["deploy"]["status"], "success");
    assert!(
        result(&body)["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("redirects here until claimed"),
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
    // Dropped, not moved: no objects under either slug, and no rows left.
    assert!(state
        .store
        .list("outer/exports/groceries/")
        .await
        .unwrap()
        .is_empty());
    assert!(state
        .store
        .list("outer/exports/pantry/")
        .await
        .unwrap()
        .is_empty());
    let exports: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app_export WHERE app_id = ?1")
        .bind(&id_before)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(exports, 0);
    assert!(!state.store.list("git/pantry/").await.unwrap().is_empty());
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
    // The redirect names the new slug but never blocks it.
    let row: Option<String> =
        sqlx::query_scalar("SELECT renamed_to FROM slug_redirect WHERE slug = 'groceries'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(row.as_deref(), Some("pantry"));
    // The old slug answers not_found on the tools (no live app there), and
    // a fresh account claims it at once — dropping the redirect — while
    // app_id-keyed history followed the rename. The claim is the pool's
    // third live app, so Bob's sibling goes first to free its ports.
    let (_, missing) = call(&state, Some(&ana), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(is_error(&missing)["error"], "not_found");
    let (_, bye) = call(
        &state,
        Some(&bob),
        "delete_app",
        json!({"app": "bobapp", "confirm": "bobapp"}),
    )
    .await;
    assert_eq!(result(&bye)["structuredContent"]["deleted"], true);
    let carol = api_key_for(&state, "carol@example.com").await;
    create_app(&state, &carol, "groceries").await;
    let dropped: Option<String> =
        sqlx::query_scalar("SELECT renamed_to FROM slug_redirect WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert_eq!(dropped, None, "claiming the old slug drops its redirect");
    let (_, claimed) = call(&state, Some(&carol), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(result(&claimed)["structuredContent"]["slug"], "groceries");
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
}

#[tokio::test]
async fn rename_app_refusals_keep_the_app_and_refund_quota() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;
    create_app(&state, &bob, "bobapp").await;
    // Commit one rename so an old-name redirect exists.
    let (_, first) = call(
        &state,
        Some(&ana),
        "rename_app",
        json!({"app": "groceries", "slug": "pantry"}),
    )
    .await;
    assert_eq!(result(&first)["structuredContent"]["slug"], "pantry");

    // Taken (a live app), reserved, misshapen and same-slug targets are all
    // refused with the create_app messages; unknown and foreign slugs are
    // not_found. The old name is NOT refused: its redirect never blocks, so
    // renaming back onto it succeeds (pinned by the test below).
    for (app, slug, code, message) in [
        ("pantry", "bobapp", "invalid_argument", "taken"),
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

    // The row never moved and the redirect still points at pantry.
    let id: String = app_id(&state, "pantry").await;
    let target: Option<String> =
        sqlx::query_scalar("SELECT renamed_to FROM slug_redirect WHERE slug = 'groceries'")
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
async fn rename_app_reclaims_its_old_name() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &ana, "groceries").await;
    let (_, first) = call(
        &state,
        Some(&ana),
        "rename_app",
        json!({"app": "groceries", "slug": "pantry"}),
    )
    .await;
    assert_eq!(result(&first)["structuredContent"]["slug"], "pantry");
    // Renaming back onto the redirected old name succeeds: the redirect
    // never blocks, and the flip consumes it.
    let (_, second) = call(
        &state,
        Some(&ana),
        "rename_app",
        json!({"app": "pantry", "slug": "groceries"}),
    )
    .await;
    let structured = &result(&second)["structuredContent"];
    assert_eq!(structured["slug"], "groceries");
    // The redirect traded places: groceries → pantry is gone, and the new
    // old name redirects back.
    let gone: Option<String> =
        sqlx::query_scalar("SELECT renamed_to FROM slug_redirect WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert_eq!(gone, None);
    let back: Option<String> =
        sqlx::query_scalar("SELECT renamed_to FROM slug_redirect WHERE slug = 'pantry'")
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert_eq!(back.as_deref(), Some("groceries"));
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
    assert_eq!(jobs, 3, "the target rows block the slug until drained");
    let (_, blocked) = call(&state, Some(&ana), "get_app", json!({"app": "pantry"})).await;
    assert_eq!(is_error(&blocked)["error"], "not_found");
    // The maintenance pass drains the abandoned copy; retry converges.
    let drained =
        crate::apps::rename::drain_prefix_cleanup(&state.pool, &state.store, Some(&state.fleets))
            .await
            .unwrap();
    assert_eq!(drained, 3);
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
    let redirect: Option<String> =
        sqlx::query_scalar("SELECT slug FROM slug_redirect WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert!(redirect.is_none());
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
async fn export_code_is_free_owner_only() {
    let (state, _) = setup().await;
    let owner = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &owner, "groceries").await;

    // Export is available on Free too (owner-only stays): no Pro gate.
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
            // Ground truth is the blob itself: `read_files` numbers lines
            // for display, so it cannot serve as the oracle here.
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

async fn make_pro(state: &McpState, email: &str) {
    sqlx::query("UPDATE user SET plan = 'pro' WHERE email = ?1")
        .bind(email)
        .execute(&state.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn preview_commit_needs_pro_and_burns_nothing() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    let (_, refused) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "trial",
               "writes": [{"path": "t.txt", "content": "t"}],
               "preview": true}),
    )
    .await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "payment_required");
    assert!(err["message"].as_str().unwrap().contains("preview deploys"));
    assert!(
        err["user_message"]
            .as_str()
            .unwrap()
            .contains("Preview deploys are a Pro feature"),
        "relay word for word: {}",
        err["user_message"]
    );
    // Refused before the claim: no quota burned, no preview state.
    let used: i64 = sqlx::query_scalar("SELECT code_changes FROM usage_day")
        .fetch_optional(&state.pool)
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(used, 0, "the refused preview burned no quota");
    let preview: Option<String> =
        sqlx::query_scalar("SELECT preview_sha FROM app WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            .flatten();
    assert!(preview.is_none());
}

#[tokio::test]
async fn preview_commit_promote_discard_round_trip() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    make_pro(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    // Trial the change: same gate, fresh namespace, live untouched.
    let (_, trialed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "trial",
               "writes": [{"path": "t.txt", "content": "t"}],
               "preview": true}),
    )
    .await;
    let structured = &result(&trialed)["structuredContent"];
    assert!(result(&trialed).get("isError").is_none());
    let sha = structured["commit"]["sha"].as_str().unwrap().to_string();
    let preview_url = structured["preview_url"].as_str().unwrap();
    assert_eq!(preview_url, "http://groceries--preview.outer.localhost");
    assert_eq!(structured["deploy"]["url"], preview_url);
    assert_eq!(structured["deploy"]["status"], "success");
    assert_eq!(structured["quota"]["used"], 1);
    // The preview row names the fleet; the live last-deploy is the seed.
    let row: (Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT preview_sha, preview_port, preview_internal_port FROM app WHERE slug = 'groceries'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(row.0.as_deref(), Some(sha.as_str()));
    assert!(row.1.is_some() && row.2.is_some() && row.1 != row.2);
    let live: Option<String> =
        sqlx::query_scalar("SELECT last_deploy_sha FROM app WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            .flatten();
    assert!(live.is_some() && live.as_deref() != Some(sha.as_str()));
    let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM deploy ORDER BY created_at")
        .fetch_all(&state.pool)
        .await
        .unwrap();
    assert!(kinds.contains(&"preview".to_string()), "kinds: {kinds:?}");

    // Live history never sees the preview run.
    let (_, status) = call(
        &state,
        Some(&token),
        "deploy_status",
        json!({"app": "groceries"}),
    )
    .await;
    assert!(result(&status)["structuredContent"]["url"]
        .as_str()
        .unwrap()
        .ends_with("groceries.outer.localhost"));
    let (_, preview_status) = call(
        &state,
        Some(&token),
        "deploy_status",
        json!({"app": "groceries", "preview": true}),
    )
    .await;
    assert_eq!(
        result(&preview_status)["structuredContent"]["url"],
        json!(preview_url)
    );
    let (_, log) = call(
        &state,
        Some(&token),
        "deploy_log",
        json!({"app": "groceries"}),
    )
    .await;
    assert!(result(&log).get("isError").is_none());
    let (_, get) = call(&state, Some(&token), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(
        result(&get)["structuredContent"]["preview"]["url"],
        json!(preview_url)
    );

    // Promote: main fast-forwards, preview clears, live deploys normally.
    let (_, promoted) = call(
        &state,
        Some(&token),
        "promote_preview",
        json!({"app": "groceries"}),
    )
    .await;
    let pstructured = &result(&promoted)["structuredContent"];
    assert!(result(&promoted).get("isError").is_none());
    assert_eq!(pstructured["sha"], json!(sha));
    assert_eq!(pstructured["deploy"]["status"], "success");
    assert!(pstructured["deploy"]["url"]
        .as_str()
        .unwrap()
        .ends_with("groceries.outer.localhost"));
    let live_after: Option<String> =
        sqlx::query_scalar("SELECT last_deploy_sha FROM app WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            .flatten();
    assert_eq!(live_after.as_deref(), Some(sha.as_str()));
    let cleared: Option<String> =
        sqlx::query_scalar("SELECT preview_sha FROM app WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            .flatten();
    assert!(cleared.is_none());
    // A stale mirror ref cannot resurrect the preview.
    let second = call(
        &state,
        Some(&token),
        "promote_preview",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(is_error(&second.1)["error"], "not_found");

    // Discard without a preview is a clean not_found.
    let (_, missing) = call(
        &state,
        Some(&token),
        "discard_preview",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(is_error(&missing)["error"], "not_found");
}

#[tokio::test]
async fn build_capped_create_leaves_the_app_and_names_the_reset() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    // Burn the whole Free build day before the seed deploy runs.
    let owner: String = sqlx::query_scalar("SELECT id FROM user WHERE email = 'ana@example.com'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO usage_day (userId, day, build_ms) VALUES (?1, ?2, ?3)")
        .bind(&owner)
        .bind(crate::mcp::tools::today_utc())
        .bind(crate::limits::build_ms_limit("free") as i64)
        .execute(&state.pool)
        .await
        .unwrap();
    let (_, body) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "groceries"}),
    )
    .await;
    let err = is_error(&body);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "build_minutes_per_day");
    assert!(
        err.get("resets_at").is_some(),
        "the agent is told to retry after the reset: {err}"
    );
    // The seed commit's app row survives the refused deploy.
    let kept: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM app WHERE slug = 'groceries'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!(kept, "a refused seed deploy leaves the created app behind");
    let deploys: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deploy WHERE app_id = (SELECT id FROM app WHERE slug = 'groceries')",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(deploys, 0, "a refused deploy starts nothing");
}

#[tokio::test]
async fn frozen_promote_refuses_before_the_fast_forward() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    make_pro(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, trialed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "trial",
               "writes": [{"path": "t.txt", "content": "t"}],
               "preview": true}),
    )
    .await;
    assert!(result(&trialed).get("isError").is_none());
    let live_before: Option<String> =
        sqlx::query_scalar("SELECT last_deploy_sha FROM app WHERE slug = 'groceries'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    sqlx::query(
        "UPDATE app SET storage_frozen_at = 1, storage_frozen_detail = 'R2 over cap' \
         WHERE slug = 'groceries'",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let (_, promoted) = call(
        &state,
        Some(&token),
        "promote_preview",
        json!({"app": "groceries"}),
    )
    .await;
    let err = is_error(&promoted);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "storage_bytes");
    // Nothing moved: live still names the seed, the preview is intact.
    let live_after: Option<String> =
        sqlx::query_scalar("SELECT last_deploy_sha FROM app WHERE slug = 'groceries'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(live_after, live_before);
    let preview: Option<String> =
        sqlx::query_scalar("SELECT preview_sha FROM app WHERE slug = 'groceries'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(preview.is_some(), "the refused promote keeps the preview");
}

#[tokio::test]
async fn frozen_rename_refuses_before_the_move() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    sqlx::query(
        "UPDATE app SET storage_frozen_at = 1, storage_frozen_detail = 'D1 over cap' \
         WHERE slug = 'groceries'",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let (_, body) = call(
        &state,
        Some(&token),
        "rename_app",
        json!({"app": "groceries", "slug": "corner"}),
    )
    .await;
    let err = is_error(&body);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "storage_bytes");
    let kept: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM app WHERE slug = 'groceries'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!(kept, "the refused rename leaves the slug in place");
}

#[tokio::test]
async fn build_capped_rename_refunds_the_claimed_change() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let owner: String = sqlx::query_scalar("SELECT id FROM user WHERE email = 'ana@example.com'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO usage_day (userId, day, build_ms) VALUES (?1, ?2, ?3)")
        .bind(&owner)
        .bind(crate::mcp::tools::today_utc())
        .bind(crate::limits::build_ms_limit("free") as i64)
        .execute(&state.pool)
        .await
        .unwrap();
    let (_, body) = call(
        &state,
        Some(&token),
        "rename_app",
        json!({"app": "groceries", "slug": "corner"}),
    )
    .await;
    let err = is_error(&body);
    assert_eq!(err["error"], "quota_exceeded");
    assert_eq!(err["quota"], "build_minutes_per_day");
    let changes: Option<i64> =
        sqlx::query_scalar("SELECT code_changes FROM usage_day WHERE userId = ?1 AND day = ?2")
            .bind(&owner)
            .bind(crate::mcp::tools::today_utc())
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            .flatten();
    assert_eq!(
        changes.unwrap_or(0),
        0,
        "the rename that never happened gives its change back"
    );
}

#[tokio::test]
async fn preview_promote_resumes_a_cancelled_run() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    make_pro(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    // Trial, then promote: main moves and the preview clears.
    let (_, trialed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "trial",
               "writes": [{"path": "t.txt", "content": "t"}],
               "preview": true}),
    )
    .await;
    let sha = result(&trialed)["structuredContent"]["commit"]["sha"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, promoted) = call(
        &state,
        Some(&token),
        "promote_preview",
        json!({"app": "groceries"}),
    )
    .await;
    assert!(result(&promoted).get("isError").is_none());

    // A cancelled promote strands the manifest ahead of the row: the fast
    // forward landed, but the row cleanup and live deploy never ran.
    // Rehearse it by restoring the preview row on the promoted tip.
    sqlx::query(
        "UPDATE app SET preview_sha = ?1, preview_updated_at = ?2, \
         preview_port = 18001, preview_internal_port = 18002 WHERE slug = 'groceries'",
    )
    .bind(&sha)
    .bind(crate::apps::now_ms())
    .execute(&state.pool)
    .await
    .unwrap();
    // No successful preview deploy at this sha anymore (it was promoted),
    // so plant one: the resumed run still requires the success check.
    sqlx::query(
        "INSERT INTO deploy (id, app_id, sha, kind, status, created_at, finished_at) \
         SELECT 'preview-resume', id, ?1, 'preview', 'success', 2, 3 FROM app WHERE slug = 'groceries'",
    )
    .bind(&sha)
    .execute(&state.pool)
    .await
    .unwrap();
    let (_, resumed) = call(
        &state,
        Some(&token),
        "promote_preview",
        json!({"app": "groceries"}),
    )
    .await;
    assert!(result(&resumed).get("isError").is_none());
    let cleared: Option<String> =
        sqlx::query_scalar("SELECT preview_sha FROM app WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            .flatten();
    assert!(cleared.is_none());
}

#[tokio::test]
async fn preview_promote_needs_success_and_discard_cleans_up() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    make_pro(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    // A blocked preview has no successful deploy: promotion refuses.
    state.deploys.set_fake(FakeOutcome::blocked());
    let (_, trialed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "trial",
               "writes": [{"path": "t.txt", "content": "t"}],
               "preview": true}),
    )
    .await;
    assert_eq!(
        result(&trialed)["structuredContent"]["deploy"]["status"],
        "blocked"
    );
    let (_, refused) = call(
        &state,
        Some(&token),
        "promote_preview",
        json!({"app": "groceries"}),
    )
    .await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "invalid_argument");
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("no successful deploy"));

    // Back to success: discard clears the preview without touching live.
    state.deploys.set_fake(FakeOutcome::success());
    let (_, discarded) = call(
        &state,
        Some(&token),
        "discard_preview",
        json!({"app": "groceries"}),
    )
    .await;
    assert!(result(&discarded).get("isError").is_none());
    let live: Option<String> =
        sqlx::query_scalar("SELECT last_deploy_sha FROM app WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            .flatten();
    assert!(live.is_some(), "discard never touches the live version");
    let cleared: Option<String> =
        sqlx::query_scalar("SELECT preview_sha FROM app WHERE slug = 'groceries'")
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            .flatten();
    assert!(cleared.is_none());
}

#[tokio::test]
async fn preview_capacity_refuses_with_retry() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    make_pro(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    // Fill the host cap with preview rows (bypasses per-owner app limits).
    sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('owner-cap', 'cap@example.com', 0, 0, 0)")
        .execute(&state.pool)
        .await
        .unwrap();
    for i in 0..20 {
        let id = format!("cap{i}");
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at, \
             preview_sha, preview_updated_at, preview_port, preview_internal_port) \
             VALUES (?1, ?2, 'owner-cap', ?3, ?4, ?5, 0, 0, 'abc', 1000, ?4, ?5)",
        )
        .bind(&id)
        .bind(format!("cap-{i}"))
        .bind(300000 + i)
        .bind(19000 + 2 * i)
        .bind(19001 + 2 * i)
        .execute(&state.pool)
        .await
        .unwrap();
    }
    let (_, refused) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "trial",
               "writes": [{"path": "t.txt", "content": "t"}],
               "preview": true}),
    )
    .await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "rate_limited");
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("preview capacity is full (20/20 active previews)"));
    assert!(err["retry_after_s"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn preview_guards_rename_and_slug_shape() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    make_pro(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    // No app may claim a preview-shaped slug: it would collide with a URL.
    let (_, taken) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "groceries--preview"}),
    )
    .await;
    assert_eq!(is_error(&taken)["error"], "invalid_argument");

    // Renaming with an active preview refuses instead of orphaning the fleet.
    let (_, trialed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "trial",
               "writes": [{"path": "t.txt", "content": "t"}],
               "preview": true}),
    )
    .await;
    assert!(result(&trialed).get("isError").is_none());
    let (_, renamed) = call(
        &state,
        Some(&token),
        "rename_app",
        json!({"app": "groceries", "slug": "pantry"}),
    )
    .await;
    let err = is_error(&renamed);
    assert_eq!(err["error"], "invalid_argument");
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("discard the preview"));
}
/// `read_files` batch reads with ranges: line numbers are exact at the
/// boundaries, and each file carries its blob sha and true total.
#[tokio::test]
async fn read_files_batch_ranges_and_numbering() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, committed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "Five lines",
               "writes": [{"path": "a.txt", "content": "l1\nl2\nl3\nl4\nl5\n"},
                          {"path": "b.txt", "content": "only\n"}]}),
    )
    .await;
    assert!(result(&committed).get("isError").is_none());

    let (_, read) = call(
        &state,
        Some(&token),
        "read_files",
        json!({"app": "groceries", "files": [
            {"path": "a.txt", "start_line": 2, "end_line": 3},
            {"path": "b.txt"},
            {"path": "a.txt", "start_line": 4, "end_line": 99},
        ]}),
    )
    .await;
    let read = result(&read);
    assert!(read.get("isError").is_none(), "{read}");
    let files = read["structuredContent"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);
    // Exact at the boundaries: middle slice.
    assert_eq!(files[0]["text"], "2│ l2\n3│ l3");
    assert_eq!(files[0]["total_lines"], 5);
    assert_eq!(files[0]["start_line"], 2);
    assert_eq!(files[0]["end_line"], 3);
    // Whole file defaults to 1..total.
    assert_eq!(files[1]["text"], "1│ only");
    assert_eq!(files[1]["total_lines"], 1);
    assert_eq!(files[1]["start_line"], 1);
    assert_eq!(files[1]["end_line"], 1);
    // A range past EOF is clamped to the true end.
    assert_eq!(files[2]["text"], "4│ l4\n5│ l5");
    assert_eq!(files[2]["start_line"], 4);
    assert_eq!(files[2]["end_line"], 5);
    // Blob shas are per-file handles, distinct per content.
    let blob_a = files[0]["blob"].as_str().unwrap();
    let blob_b = files[1]["blob"].as_str().unwrap();
    assert_eq!(blob_a.len(), 40);
    assert_ne!(blob_a, blob_b);
    assert_eq!(read["structuredContent"]["truncated"], false);
}

/// One bad file never fails the call: missing, binary and bad ranges report
/// inline while the good file reads.
#[tokio::test]
async fn read_files_error_isolation() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, committed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "Good and binary",
               "writes": [{"path": "ok.txt", "content": "fine\n"},
                          {"path": "img.bin", "base64": "iVBORwAB"}]}),
    )
    .await;
    assert!(result(&committed).get("isError").is_none());

    let (_, read) = call(
        &state,
        Some(&token),
        "read_files",
        json!({"app": "groceries", "files": [
            {"path": "ok.txt"},
            {"path": "missing.txt"},
            {"path": "img.bin"},
            {"path": "ok.txt", "start_line": 3, "end_line": 2},
            {"path": "ok.txt", "start_line": 1, "symbol": "Nope"},
        ]}),
    )
    .await;
    let read = result(&read);
    assert!(read.get("isError").is_none(), "{read}");
    let files = read["structuredContent"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 5);
    assert_eq!(files[0]["text"], "1│ fine");
    assert_eq!(files[1]["code"], "not_found");
    assert!(files[1]["error"].as_str().unwrap().contains("not found"));
    assert!(files[2]["error"].as_str().unwrap().contains("binary"));
    assert_eq!(files[3]["code"], "invalid_argument");
    assert!(files[4]["error"]
        .as_str()
        .unwrap()
        .contains("cannot be combined"));
}

/// The ~200 KB output budget: the file crossing it is cut with `truncated`,
/// later files fail inline, and the total stays within budget.
#[tokio::test]
async fn read_files_budget_truncation() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let big = "x".repeat(80 * 1024);
    let (_, committed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "Big files",
               "writes": [{"path": "f1.txt", "content": big},
                          {"path": "f2.txt", "content": big},
                          {"path": "f3.txt", "content": big},
                          {"path": "small.txt", "content": "s\n"}]}),
    )
    .await;
    assert!(result(&committed).get("isError").is_none());

    let (_, read) = call(
        &state,
        Some(&token),
        "read_files",
        json!({"app": "groceries", "files": [
            {"path": "f1.txt"}, {"path": "f2.txt"},
            {"path": "f3.txt"}, {"path": "small.txt"},
        ]}),
    )
    .await;
    let read = result(&read);
    assert!(read.get("isError").is_none(), "{read}");
    assert_eq!(read["structuredContent"]["truncated"], true);
    let files = read["structuredContent"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 4);
    assert!(files[0].get("truncated").is_none());
    assert!(files[1].get("truncated").is_none());
    assert_eq!(files[2]["truncated"], true);
    assert!(files[3]["error"]
        .as_str()
        .unwrap()
        .contains("budget is exhausted"));
    let total: usize = files
        .iter()
        .filter_map(|f| f.get("text").and_then(Value::as_str))
        .map(str::len)
        .sum();
    assert!(total <= 200 * 1024, "{total}");
}

/// `symbol` returns just the declaration's numbered lines on template-shaped
/// files, and refuses ambiguous, missing and unsupported lookups inline.
#[tokio::test]
async fn read_files_symbol_extraction() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let card = "import { Island } from \"@outer/ilha\";\n\
                import { count } from \"./store\";\n\
                \n\
                const PAGE_SIZE = 25;\n\
                \n\
                export function Card({ title }: { title: string }) {\n\
                \x20 return <Island><h1>{title}</h1></Island>;\n\
                }\n\
                \n\
                export const formatTitle = (t: string) => t.trim();\n\
                \n\
                export default Card;\n";
    let astro = "---\n\
                 import Layout from \"../layouts/Base.astro\";\n\
                 const title = \"Shop\";\n\
                 ---\n\
                 <Layout><h1>{title}</h1></Layout>\n";
    let (_, committed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "Components",
               "writes": [{"path": "src/Card.tsx", "content": card},
                          {"path": "pages/index.astro", "content": astro},
                          {"path": "src/dup.ts", "content": "export const Item = 1;\nconst Item = 2;\n"},
                          {"path": "README.md", "content": "# hi\n"}]}),
    )
    .await;
    assert!(result(&committed).get("isError").is_none());

    let (_, read) = call(
        &state,
        Some(&token),
        "read_files",
        json!({"app": "groceries", "files": [
            {"path": "src/Card.tsx", "symbol": "Card"},
            {"path": "src/Card.tsx", "symbol": "formatTitle"},
            {"path": "pages/index.astro", "symbol": "title"},
            {"path": "src/dup.ts", "symbol": "Item"},
            {"path": "src/Card.tsx", "symbol": "Missing"},
            {"path": "README.md", "symbol": "hi"},
        ]}),
    )
    .await;
    let read = result(&read);
    assert!(read.get("isError").is_none(), "{read}");
    let files = read["structuredContent"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 6);
    assert_eq!(
        files[0]["text"].as_str().unwrap().lines().count(),
        3,
        "{}",
        files[0]["text"]
    );
    assert!(files[0]["text"].as_str().unwrap().starts_with("6│ "));
    assert_eq!(files[0]["start_line"], 6);
    assert_eq!(files[0]["end_line"], 8);
    assert_eq!(files[0]["symbol"], "Card");
    assert_eq!(files[1]["start_line"], 10);
    assert_eq!(files[1]["end_line"], 10);
    assert_eq!(files[2]["text"], "3│ const title = \"Shop\";");
    assert_eq!(files[2]["start_line"], 3);
    // Two top-level `Item`s: refused, not guessed.
    assert!(files[3]["error"].as_str().unwrap().contains("2 top-level"));
    // No such symbol: inline, not a failed call.
    assert!(files[4]["error"].as_str().unwrap().contains("no top-level"));
    // Markdown has no symbol table.
    assert!(files[5]["error"]
        .as_str()
        .unwrap()
        .contains("symbol extraction"));
}

/// `search_code` v2: globs, context, case/word and regex, grouped by file.
#[tokio::test]
async fn search_code_v2_filters_and_grouping() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, committed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "Words",
               "writes": [{"path": "src/app.ts", "content": "const greeting = \"Hello\";\nconsole.log(greeting);\n"},
                          {"path": "src/shout.ts", "content": "HELLO world\n"},
                          {"path": "notes.txt", "content": "hello there\n"}]}),
    )
    .await;
    assert!(result(&committed).get("isError").is_none());

    // Case-sensitive default: only the lowercase file hits.
    let (_, found) = call(
        &state,
        Some(&token),
        "search_code",
        json!({"app": "groceries", "query": "hello"}),
    )
    .await;
    let found = result(&found);
    let files = found["structuredContent"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "notes.txt");
    assert_eq!(files[0]["matches"][0]["line"], 1);
    assert!(files[0]["matches"][0].get("before").is_none());
    assert_eq!(found["structuredContent"]["truncated"], false);

    // Case-insensitive spans all three files, grouped with context.
    let (_, found) = call(
        &state,
        Some(&token),
        "search_code",
        json!({"app": "groceries", "query": "hello",
               "case_insensitive": true, "context": 1}),
    )
    .await;
    let found = result(&found);
    let files = found["structuredContent"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 3, "{files:?}");
    let app = files.iter().find(|f| f["path"] == "src/app.ts").unwrap();
    assert_eq!(app["matches"][0]["line"], 1);
    assert_eq!(app["matches"][0]["after"][0], "console.log(greeting);");

    // Whole words: `hell` matches nothing inside `hello`.
    let (_, found) = call(
        &state,
        Some(&token),
        "search_code",
        json!({"app": "groceries", "query": "hell",
               "case_insensitive": true, "word": true}),
    )
    .await;
    assert_eq!(
        result(&found)["structuredContent"]["files"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // Globs: `*.ts` matches the basename at any depth; `src/*.ts` stays put.
    let (_, found) = call(
        &state,
        Some(&token),
        "search_code",
        json!({"app": "groceries", "query": "hello",
               "case_insensitive": true, "paths": ["*.ts"]}),
    )
    .await;
    assert_eq!(
        result(&found)["structuredContent"]["files"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let (_, found) = call(
        &state,
        Some(&token),
        "search_code",
        json!({"app": "groceries", "query": "hello",
               "case_insensitive": true, "paths": ["notes.*"]}),
    )
    .await;
    let files = result(&found)["structuredContent"]["files"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "notes.txt");

    // Regex still works.
    let (_, found) = call(
        &state,
        Some(&token),
        "search_code",
        json!({"app": "groceries", "query": "gr..ting", "regex": true}),
    )
    .await;
    assert_eq!(
        result(&found)["structuredContent"]["files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Bad shapes are the caller's bug.
    for args in [
        json!({"app": "groceries", "query": "hello", "context": 9}),
        json!({"app": "groceries", "query": "(unclosed", "regex": true}),
        json!({"app": "groceries", "query": "hello", "paths": ["../x"]}),
    ] {
        let (_, bad) = call(&state, Some(&token), "search_code", args).await;
        assert_eq!(is_error(&bad)["error"], "invalid_argument");
    }
}

/// Generated files stay out of results unless `include_generated` opts back in.
#[tokio::test]
async fn search_code_skips_generated_by_default() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, committed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "Marker everywhere",
               "writes": [{"path": "src/app.ts", "content": "const marker_xyz = 1;\n"},
                          {"path": "dist/bundle.js", "content": "var marker_xyz=1;\n"},
                          {"path": "node_modules/pkg/index.js", "content": "var marker_xyz=1;\n"},
                          {"path": "package-lock.json", "content": "{\"marker_xyz\": 1}\n"}]}),
    )
    .await;
    assert!(result(&committed).get("isError").is_none());

    let (_, found) = call(
        &state,
        Some(&token),
        "search_code",
        json!({"app": "groceries", "query": "marker_xyz"}),
    )
    .await;
    let files = result(&found)["structuredContent"]["files"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "src/app.ts");

    let (_, found) = call(
        &state,
        Some(&token),
        "search_code",
        json!({"app": "groceries", "query": "marker_xyz", "include_generated": true}),
    )
    .await;
    let mut paths: Vec<&str> = result(&found)["structuredContent"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec![
            "dist/bundle.js",
            "node_modules/pkg/index.js",
            "package-lock.json",
            "src/app.ts"
        ]
    );
}

/// `kind: "symbol"`: declaration sites are definitions, every other word
/// match is a usage. Unparseable files contribute usages only.
#[tokio::test]
async fn search_code_symbol_kind() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, committed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "Store",
               "writes": [{"path": "src/store.ts", "content": "export const count = 0;\nexport function reset() {\n  console.log(count);\n}\n"},
                          {"path": "src/app.ts", "content": "import { count } from \"./store\";\ncount;\n"},
                          {"path": "notes.txt", "content": "count the carts\n"}]}),
    )
    .await;
    assert!(result(&committed).get("isError").is_none());

    let (_, found) = call(
        &state,
        Some(&token),
        "search_code",
        json!({"app": "groceries", "query": "count", "kind": "symbol"}),
    )
    .await;
    let found = result(&found);
    assert!(found.get("isError").is_none(), "{found}");
    let files = found["structuredContent"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 3, "{files:?}");
    let store = files.iter().find(|f| f["path"] == "src/store.ts").unwrap();
    assert_eq!(store["definitions"].as_array().unwrap().len(), 1);
    assert_eq!(store["definitions"][0]["line"], 1);
    assert_eq!(store["definitions"][0]["end_line"], 1);
    // The `count` on line 3 is a usage, not the declaration.
    assert_eq!(store["usages"].as_array().unwrap().len(), 1);
    assert_eq!(store["usages"][0]["line"], 3);
    let app = files.iter().find(|f| f["path"] == "src/app.ts").unwrap();
    assert_eq!(app["definitions"].as_array().unwrap().len(), 0);
    assert_eq!(app["usages"].as_array().unwrap().len(), 2);
    // Plain text contributes usages only.
    let notes = files.iter().find(|f| f["path"] == "notes.txt").unwrap();
    assert_eq!(notes["definitions"].as_array().unwrap().len(), 0);
    assert_eq!(notes["usages"].as_array().unwrap().len(), 1);

    // `symbol` needs one identifier, regex has no meaning there, and the
    // kind itself is closed.
    for args in [
        json!({"app": "groceries", "query": "not an identifier", "kind": "symbol"}),
        json!({"app": "groceries", "query": "count", "kind": "symbol", "regex": true}),
        json!({"app": "groceries", "query": "count", "kind": "fuzzy"}),
    ] {
        let (_, bad) = call(&state, Some(&token), "search_code", args).await;
        assert_eq!(is_error(&bad)["error"], "invalid_argument");
    }
}

/// A stranger's slug is `not_found` for both new read paths, never `forbidden`.
#[tokio::test]
async fn read_search_strangers_app_is_not_found() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    for (tool, args) in [
        (
            "read_files",
            json!({"app": "groceries", "files": [{"path": "pages/index.tsx"}]}),
        ),
        (
            "search_code",
            json!({"app": "groceries", "query": "Groceries"}),
        ),
    ] {
        let (_, denied) = call(&state, Some(&bob), tool, args).await;
        assert_eq!(is_error(&denied)["error"], "not_found");
    }
}

// --- `open_app` (Wave 4): one orientation call per app -----------------------

/// One `commit` of full-file writes; returns the `structuredContent`.
async fn commit_writes(
    state: &McpState,
    token: &str,
    app: &str,
    writes: Vec<(&str, &str)>,
) -> Value {
    let writes: Vec<Value> = writes
        .into_iter()
        .map(|(path, content)| json!({"path": path, "content": content}))
        .collect();
    let (_, body) = call(
        state,
        Some(token),
        "commit",
        json!({"app": app, "message": "shape the tree", "writes": writes}),
    )
    .await;
    result(&body)["structuredContent"].clone()
}

/// Every tree entry flattened: `(path, lines, role, status?)`.
fn tree_entries(body: &Value) -> Vec<(String, Option<u64>, String, Option<String>)> {
    let sc = &body["structuredContent"];
    sc["tree"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|dir| dir["files"].as_array().unwrap().iter())
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_string(),
                f["lines"].as_u64(),
                f["role"].as_str().unwrap().to_string(),
                f.get("status").and_then(Value::as_str).map(str::to_string),
            )
        })
        .collect()
}

fn tree_paths(body: &Value) -> Vec<String> {
    tree_entries(body)
        .into_iter()
        .map(|(p, _, _, _)| p)
        .collect()
}

fn role_of(entries: &[(String, Option<u64>, String, Option<String>)], path: &str) -> String {
    entries
        .iter()
        .find(|(p, _, _, _)| p == path)
        .unwrap_or_else(|| panic!("{path} missing from the tree"))
        .2
        .clone()
}

const VOID_CONFIG: &str = "import { defineConfig } from \"void/config\";\nexport default defineConfig({\n  cloudflare: {\n    d1_databases: [{ binding: \"DB\", database_name: \"app-db\", migrations_dir: \"db/migrations\" }],\n  },\n});\n";
const BRAND_JSON: &str = "{\"name\": \"Groceries\", \"shortName\": \"Groceries\", \"description\": \"Keep track.\", \"accent\": \"#555de3\"}\n";
const AGENTS_MD: &str = "# AGENTS.md\n\nYou edit this app through Outer's MCP tools.\n\n## Files\n\n- `pages/index.tsx` is the home page.\n";

#[tokio::test]
async fn open_app_orients_ilha_shaped_repo() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    commit_writes(
        &state,
        &token,
        "groceries",
        vec![
            ("pages/index.tsx", "<h1>Groceries</h1>\n<p>list</p>\n"),
            ("pages/settings.tsx", "export default function Settings() {}\n"),
            ("pages/blog/[slug].tsx", "export default function Post() {}\n"),
            ("pages/index.server.ts", "export function load() {}\n"),
            ("pages/layout.tsx", "export default function Layout() {}\n"),
            ("pages/_helper.ts", "export const x = 1;\n"),
            ("pages/x.test.tsx", "import { test } from \"bun:test\";\n"),
            ("middleware/01.not-found.ts", "export default function notFound() {}\n"),
            (
                "db/migrations/0001_item.sql",
                "-- items table\nCREATE TABLE IF NOT EXISTS item (\n  id INTEGER PRIMARY KEY AUTOINCREMENT, -- pk\n  text TEXT NOT NULL,\n  CONSTRAINT uq_text UNIQUE (text)\n);\nALTER TABLE item ADD COLUMN done INTEGER NOT NULL DEFAULT 0;\n",
            ),
            (
                "db/migrations/0002_note.sql",
                "CREATE TABLE \"note\" (\"id\" INTEGER PRIMARY KEY, body TEXT);\n",
            ),
            ("brand.json", BRAND_JSON),
            ("AGENTS.md", AGENTS_MD),
            ("void.config.ts", VOID_CONFIG),
            ("README.md", "# Groceries\n"),
            ("pages/app.css", ".x { color: red; }\n"),
            ("dist/bundle.js", "console.log(1)\n"),
            ("node_modules/dep/index.js", "module.exports = 1;\n"),
            ("bun.lock", "{\"lockfileVersion\": 1}\n"),
            ("void.lock.json", "{}\n"),
            ("public/favicon.svg", "<svg></svg>\n"),
            ("public/apple-touch-icon-180x180.png", "PNGDATA\n"),
            ("public/manifest.webmanifest", "{}\n"),
        ],
    )
    .await;
    let (_, body) = call(
        &state,
        Some(&token),
        "open_app",
        json!({"app": "groceries"}),
    )
    .await;
    let sc = &result(&body)["structuredContent"];

    let head = sc["head"].as_str().unwrap().to_string();
    assert_eq!(head.len(), 40, "head is the full commit sha");
    assert_eq!(sc["truncated"], false);

    // Roles follow the Ilha conventions.
    let entries = tree_entries(&result(&body).clone());
    for (path, role) in [
        ("pages/index.tsx", "page"),
        ("pages/settings.tsx", "page"),
        ("pages/blog/[slug].tsx", "page"),
        ("pages/index.server.ts", "server"),
        ("pages/layout.tsx", "layout"),
        ("pages/_helper.ts", "other"),
        ("pages/x.test.tsx", "test"),
        ("middleware/01.not-found.ts", "middleware"),
        ("db/migrations/0001_item.sql", "migration"),
        ("void.config.ts", "config"),
        ("brand.json", "config"),
        ("README.md", "doc"),
        ("pages/app.css", "style"),
    ] {
        assert_eq!(role_of(&entries, path), role, "role of {path}");
    }
    // Line counts come along, including the two-line index page.
    let index = entries
        .iter()
        .find(|(p, _, _, _)| p == "pages/index.tsx")
        .unwrap();
    assert_eq!(index.1, Some(2));

    // Build output, dependencies, lockfiles and generated icons stay hidden.
    let paths = tree_paths(&result(&body).clone());
    for hidden in [
        "dist/bundle.js",
        "node_modules/dep/index.js",
        "bun.lock",
        "void.lock.json",
        "public/favicon.svg",
        "public/apple-touch-icon-180x180.png",
        "public/manifest.webmanifest",
    ] {
        assert!(
            !paths.contains(&hidden.to_string()),
            "{hidden} must stay hidden"
        );
    }

    // Routes cover the index, the settings page and the dynamic segment.
    let routes: Vec<(String, String)> = sc["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_string(),
                r["file"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(routes.contains(&("/settings".to_string(), "pages/settings.tsx".to_string())));
    assert!(routes.contains(&(
        "/blog/<slug>".to_string(),
        "pages/blog/[slug].tsx".to_string()
    )));
    assert!(routes.iter().any(|(p, _)| p == "/"));
    // Loaders, layouts and partials are never routes.
    assert!(!routes.iter().any(|(_, f)| f == "pages/index.server.ts"));
    assert!(!routes.iter().any(|(_, f)| f == "pages/layout.tsx"));
    assert!(!routes.iter().any(|(_, f)| f == "pages/_helper.ts"));
    assert!(!routes.iter().any(|(_, f)| f == "pages/x.test.tsx"));

    // D1 tables parsed from the committed migrations (constraints skipped,
    // the ALTER column appended) — the live DB untouched.
    let tables: Vec<(String, Vec<String>)> = sc["database"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["columns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c.as_str().unwrap().to_string())
                    .collect(),
            )
        })
        .collect();
    assert!(tables.contains(&(
        "item".to_string(),
        vec!["id".to_string(), "text".to_string(), "done".to_string()]
    )));
    assert!(tables.contains(&(
        "note".to_string(),
        vec!["id".to_string(), "body".to_string()]
    )));

    // Bindings, brand and rules digest.
    assert_eq!(sc["bindings"]["d1_databases"][0]["binding"], "DB");
    assert_eq!(sc["bindings"]["d1_databases"][0]["database_name"], "app-db");
    assert_eq!(sc["brand"]["name"], "Groceries");
    assert_eq!(sc["rules"]["path"], "AGENTS.md");
    assert!(sc["rules"]["digest"]
        .as_str()
        .unwrap()
        .contains("MCP tools"));

    // Deploy, errors and quota.
    assert_eq!(sc["deploy"]["status"], "success");
    assert_eq!(sc["deploy"]["sha"], head);
    assert!(sc["deploy"]["url"].as_str().unwrap().contains("groceries"));
    assert_eq!(sc["errors_open"], 0);
    assert_eq!(
        sc["quota"]["code_changes_per_day"],
        crate::limits::change_limit("free")
    );
    assert!(sc["quota"]["code_changes_used_today"].as_u64().unwrap() >= 1);

    // `next` points at line-ranged reads, search and commit edits.
    let next = sc["next"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(next.contains("read_files"), "next hints reads: {next}");
    assert!(next.contains("search_code"), "next hints search: {next}");
    assert!(next.contains("commit"), "next hints edits: {next}");

    // The orientation call stays compact (≤ 4k tokens ≈ 16k chars here).
    assert!(
        sc.to_string().len() < 16_000,
        "open_app answers in {} chars",
        sc.to_string().len()
    );
}

#[tokio::test]
async fn open_app_orients_astro_shaped_repo() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "site").await;
    commit_writes(
        &state,
        &token,
        "site",
        vec![
            ("src/pages/index.astro", "---\n---\n<h1>Hi</h1>\n"),
            ("src/pages/about.astro", "<h1>About</h1>\n"),
            ("src/pages/blog/index.astro", "<h1>Blog</h1>\n"),
            ("src/pages/blog/[slug].astro", "<h1>Post</h1>\n"),
            ("src/pages/404.astro", "<h1>Missing</h1>\n"),
            ("src/pages/api/newsletter.ts", "export function POST() {}\n"),
            ("src/pages/rss.xml.ts", "export function GET() {}\n"),
            ("src/pages/sitemap.xml.ts", "export function GET() {}\n"),
            ("src/islands/NewsletterForm.tsx", "export default function Form() {}\n"),
            ("src/layouts/Base.astro", "<html></html>\n"),
            ("src/content/posts/hello.md", "# Hello\n"),
            ("src/lib/site.ts", "export const x = 1;\n"),
            ("src/lib/site.test.ts", "import { test } from \"bun:test\";\n"),
            ("src/styles/global.css", "body {}\n"),
            (
                "db/migrations/0001_newsletter.sql",
                "CREATE TABLE IF NOT EXISTS newsletter (id INTEGER PRIMARY KEY AUTOINCREMENT, email TEXT NOT NULL UNIQUE);\n",
            ),
            ("void.config.ts", VOID_CONFIG),
            ("astro.config.mjs", "export default {};\n"),
            ("brand.json", BRAND_JSON),
            ("AGENTS.md", AGENTS_MD),
            ("public/favicon.svg", "<svg></svg>\n"),
            ("public/og.svg", "<svg></svg>\n"),
        ],
    )
    .await;
    let (_, body) = call(&state, Some(&token), "open_app", json!({"app": "site"})).await;
    let sc = &result(&body)["structuredContent"];

    let entries = tree_entries(&result(&body).clone());
    for (path, role) in [
        ("src/pages/index.astro", "page"),
        ("src/pages/api/newsletter.ts", "page"),
        ("src/islands/NewsletterForm.tsx", "island"),
        ("src/layouts/Base.astro", "layout"),
        ("src/content/posts/hello.md", "content"),
        ("src/lib/site.test.ts", "test"),
        ("db/migrations/0001_newsletter.sql", "migration"),
        ("astro.config.mjs", "config"),
    ] {
        assert_eq!(role_of(&entries, path), role, "role of {path}");
    }
    let paths = tree_paths(&result(&body).clone());
    assert!(!paths.contains(&"public/favicon.svg".to_string()));
    assert!(!paths.contains(&"public/og.svg".to_string()));

    let routes: Vec<(String, String)> = sc["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_string(),
                r["file"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    for (path, file) in [
        ("/about", "src/pages/about.astro"),
        ("/blog", "src/pages/blog/index.astro"),
        ("/blog/<slug>", "src/pages/blog/[slug].astro"),
        ("/api/newsletter", "src/pages/api/newsletter.ts"),
        ("/rss.xml", "src/pages/rss.xml.ts"),
        ("/sitemap.xml", "src/pages/sitemap.xml.ts"),
    ] {
        assert!(
            routes.contains(&(path.to_string(), file.to_string())),
            "route {path} → {file} missing in {routes:?}"
        );
    }
    assert!(!routes.iter().any(|(_, f)| f == "src/pages/404.astro"));

    let tables: Vec<String> = sc["database"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(tables.contains(&"newsletter".to_string()));
    assert!(sc.to_string().len() < 16_000);
}

#[tokio::test]
async fn open_app_truncates_large_trees() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "big").await;
    // 320 single-line pages in four commits (100 writes/edits per commit).
    for chunk in 0..4 {
        let writes: Vec<(String, String)> = (0..80)
            .map(|i| {
                let n = chunk * 80 + i;
                (
                    format!("pages/p{n:03}.tsx"),
                    "export default function P() {}\n".to_string(),
                )
            })
            .collect();
        let refs: Vec<(&str, &str)> = writes
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        commit_writes(&state, &token, "big", refs).await;
    }
    let (_, body) = call(&state, Some(&token), "open_app", json!({"app": "big"})).await;
    let sc = &result(&body)["structuredContent"];
    assert_eq!(sc["truncated"], true);
    let total: usize = sc["tree"]
        .as_array()
        .unwrap()
        .iter()
        .map(|dir| dir["files"].as_array().unwrap().len())
        .sum();
    assert_eq!(total, 300);
    let next = sc["next"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        next.contains("list_files"),
        "truncation hints at list_files: {next}"
    );
}

#[tokio::test]
async fn open_app_changed_since_lists_only_deltas() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "delta").await;
    commit_writes(
        &state,
        &token,
        "delta",
        vec![("a.txt", "a\n"), ("b.txt", "b\n")],
    )
    .await;
    let (_, first) = call(&state, Some(&token), "open_app", json!({"app": "delta"})).await;
    let base = result(&first)["structuredContent"]["head"]
        .as_str()
        .unwrap()
        .to_string();
    // One edit, one delete, one addition.
    let (_, edited) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "delta", "message": "second",
               "edits": [{"path": "a.txt", "old": "a\n", "new": "a2\n"}],
               "deletes": ["b.txt"],
               "writes": [{"path": "c.txt", "content": "c\n"}]}),
    )
    .await;
    assert!(result(&edited).get("isError").is_none());
    let (_, body) = call(
        &state,
        Some(&token),
        "open_app",
        json!({"app": "delta", "changed_since": base}),
    )
    .await;
    let entries = tree_entries(&result(&body).clone());
    let mut statuses: Vec<(String, String)> = entries
        .into_iter()
        .map(|(p, _, _, s)| (p, s.unwrap_or_else(|| "none".to_string())))
        .collect();
    statuses.sort();
    assert_eq!(
        statuses,
        vec![
            ("a.txt".to_string(), "modified".to_string()),
            ("b.txt".to_string(), "deleted".to_string()),
            ("c.txt".to_string(), "added".to_string()),
        ]
    );
}

#[tokio::test]
async fn open_app_stranger_gets_not_found() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;
    let (_, body) = call(&state, Some(&bob), "open_app", json!({"app": "groceries"})).await;
    assert_eq!(is_error(&body)["error"], "not_found");
}

#[tokio::test]
async fn open_app_call_is_recorded_without_args() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, body) = call(
        &state,
        Some(&token),
        "open_app",
        json!({"app": "groceries"}),
    )
    .await;
    assert!(result(&body).get("isError").is_none());
    // The recorder runs spawned off the call: poll for the row.
    let mut tries = 0;
    let row = loop {
        let rows: Vec<(String, Option<String>, String, i64)> = sqlx::query_as(
            "SELECT tool, app_slug, session, response_bytes FROM tool_call WHERE tool = 'open_app'",
        )
        .fetch_all(&state.pool)
        .await
        .unwrap();
        if let Some(row) = rows
            .into_iter()
            .find(|(_, slug, _, _)| slug.as_deref() == Some("groceries"))
        {
            break row;
        }
        tries += 1;
        assert!(tries < 100, "telemetry row never landed");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(row.0, "open_app");
    assert!(row.2.starts_with("key:"), "session keys on the credential");
    assert!(row.3 > 0, "response bytes recorded");
}

/// `whoami`, `list_apps` and `get_app` carry the inactivity deadline
/// (spec §7.8) on Free accounts, and omit it on Pro. Each call moves the
/// clock (any authenticated request counts), so the test reads the stamps
/// back and pins the deadline to them.
#[tokio::test]
async fn tools_show_deletes_at_on_free_and_hide_it_on_pro() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    async fn deadline(state: &McpState) -> i64 {
        let (app_last, owner_last): (i64, i64) = sqlx::query_as(
            "SELECT a.last_activity_at, u.lastActiveAt FROM app a              JOIN user u ON u.id = a.owner_id WHERE a.slug = 'groceries'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        app_last.max(owner_last) + 90 * 86_400_000
    }
    let (_, who) = call(&state, Some(&token), "whoami", json!({})).await;
    assert_eq!(
        result(&who)["structuredContent"]["deletes_at"],
        deadline(&state).await
    );
    let (_, list) = call(&state, Some(&token), "list_apps", json!({})).await;
    let apps = result(&list)["structuredContent"]["apps"].clone();
    assert_eq!(apps.as_array().unwrap().len(), 1);
    assert_eq!(apps[0]["deletes_at"], deadline(&state).await);
    assert_eq!(apps[0]["status"], "running");
    let (_, get) = call(&state, Some(&token), "get_app", json!({"app": "groceries"})).await;
    assert_eq!(
        result(&get)["structuredContent"]["deletes_at"],
        deadline(&state).await
    );
    // Pro is exempt: the key vanishes everywhere, `whoami` nulls it.
    sqlx::query("UPDATE user SET plan = 'pro' WHERE email = 'ana@example.com'")
        .execute(&state.pool)
        .await
        .unwrap();
    let (_, who) = call(&state, Some(&token), "whoami", json!({})).await;
    assert_eq!(
        result(&who)["structuredContent"]["deletes_at"],
        serde_json::Value::Null
    );
    let (_, list) = call(&state, Some(&token), "list_apps", json!({})).await;
    let apps = result(&list)["structuredContent"]["apps"].clone();
    assert!(apps[0].get("deletes_at").is_none(), "{apps}");
    let (_, get) = call(&state, Some(&token), "get_app", json!({"app": "groceries"})).await;
    assert!(
        result(&get)["structuredContent"]
            .get("deletes_at")
            .is_none(),
        "{}",
        result(&get)["structuredContent"]
    );
}

// --- app_metrics / db_query (M3 data tools) --------------------------------
//
// app_metrics reads the same metric_minute/path_hour/visitor_day rows as the
// Console metrics tab; db_query runs through the same celld path as the
// Console D1 panel. The fake celld below answers `d1 execute` with canned
// JSON per `--command` marker, and a stub health server makes `ensure_awake`
// count the fleet awake so no real fleet ever spawns.

/// A fake `celld` for db_query tests: answers `d1 execute` with canned JSON
/// rows, recording argv under the state dir derived from
/// `--bucket s3://<dir>/fleets/<slug>`.
fn install_fake_query_celld(state: &std::path::Path) -> PathBuf {
    let script = state.join("fake-celld");
    let tmp = script.with_extension("tmp");
    std::fs::write(
        &tmp,
        r#"#!/bin/sh
command_of() {
  prev=""
  for a in "$@"; do
    if [ "$prev" = "--command" ]; then printf '%s' "$a"; return 0; fi
    prev="$a"
  done
}
bucket_of() {
  prev=""
  for a in "$@"; do
    if [ "$prev" = "--bucket" ]; then printf '%s' "$a"; return 0; fi
    prev="$a"
  done
}
sql=$(command_of "$@")
bucket=$(bucket_of "$@")
statedir=$(printf '%s' "$bucket" | sed -e 's|^s3://||' -e 's|/fleets/.*||')
mkdir -p "$statedir"
printf '%s\n' "$*" >> "$statedir/calls.log"
case "$sql" in
  *FAILME*) echo "no such table: missing" >&2; exit 1;;
  *BIGONE*)
    printf '{"big":"'
    head -c 300000 /dev/zero | tr '\0' 'x'
    printf '"}\n'
    ;;
  *MANYROWS*)
    i=1
    while [ $i -le 250 ]; do printf '{"id":%d,"name":"n%d"}\n' "$i" "$i"; i=$((i+1)); done
    ;;
  *HUGEROWS*)
    pad=$(head -c 2000 /dev/zero | tr '\0' 'y')
    i=1
    while [ $i -le 200 ]; do printf '{"id":%d,"pad":"%s"}\n' "$i" "$pad"; i=$((i+1)); done
    ;;
  *) printf '%s\n' '{"id":1,"name":"ada"}';;
esac
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::rename(&tmp, &script).unwrap();
    script
}

/// The live manifest celld published for the app: D1 bindings only, in the
/// layout `deployed_manifest` reads.
async fn seed_query_manifest(store: &Store, slug: &str, bindings: &[(&str, &str)]) {
    let prefix = "deploy/outer-app/abcd1234";
    store
        .put(
            &format!("fleets/{slug}/deploy/current.json"),
            format!(
                "{{\"script_name\":\"outer-app\",\"version\":\"abcd1234\",\"prefix\":\"{prefix}\"}}"
            )
            .into_bytes(),
        )
        .await
        .unwrap();
    let dbs: Vec<Value> = bindings
        .iter()
        .map(|(binding, name)| json!({"type": "d1", "name": binding, "database_name": name}))
        .collect();
    let manifest = json!({
        "schema_version": 2,
        "script_name": "outer-app",
        "main_module": "index.js",
        "raw_metadata": {
            "bindings": dbs,
            "compatibility_date": "2026-09-01",
            "main_module": "index.js",
        },
    });
    store
        .put(
            &format!("fleets/{slug}/{prefix}/manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .await
        .unwrap();
}

/// A stub fleet health gate: answers 200 on every connection so
/// `ensure_awake`'s pre-spawn probe counts the fleet awake and no `celld`
/// spawn ever runs. Loopback-only; the task dies with the test.
async fn stub_query_health() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });
    (port, handle)
}

/// db_query test app: real app row plus seeded manifest, the fleet pointed
/// at the fake celld and the stub health server. Guards keep the temp dirs
/// and the stub alive; `McpState` only holds path copies.
struct QueryApp {
    state: McpState,
    token: String,
    statedir: tempfile::TempDir,
    _stub: tokio::task::JoinHandle<()>,
}

async fn query_setup(slug: &str, bindings: &[(&str, &str)]) -> QueryApp {
    let (mut state, _) = setup().await;
    set_plan(&state, "ana@example.com", "pro").await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, slug).await;
    seed_query_manifest(&state.store, slug, bindings).await;
    // Point the app at the stub so the D1 wake probe succeeds locally.
    let (port, stub) = stub_query_health().await;
    sqlx::query("UPDATE app SET port = ?1 WHERE slug = ?2")
        .bind(port as i64)
        .bind(slug)
        .execute(&state.pool)
        .await
        .unwrap();
    let statedir = tempfile::tempdir().unwrap();
    let script = install_fake_query_celld(statedir.path());
    let mut storage = state.fleets.storage().clone();
    storage.bucket = statedir.path().to_string_lossy().into_owned();
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();
    std::mem::forget(data_dir);
    state.fleets = FleetManager::new(ManagerSettings {
        data_dir: data_path,
        celld_bin: script.to_string_lossy().into_owned(),
        storage,
        cgroups: None,
        memory_max_bytes: 512 * 1024 * 1024,
        cpu_max: "50000 100000".to_string(),
        sleep_after: std::time::Duration::from_secs(1800),
        credential_refresh_after: std::time::Duration::from_secs(6 * 86400),
        wake_timeout: std::time::Duration::from_secs(1),
        wake_concurrency: 4,
        always_on_max: 10,
        preview_max: 20,
    });
    QueryApp {
        state,
        token,
        statedir,
        _stub: stub,
    }
}

fn query_calls(q: &QueryApp) -> String {
    std::fs::read_to_string(q.statedir.path().join("calls.log")).unwrap_or_default()
}

#[tokio::test]
async fn app_metrics_shape_matches_the_console_source() {
    let (state, _) = setup().await;
    let ana = api_key_for(&state, "ana@example.com").await;
    let bob = api_key_for(&state, "bob@example.com").await;
    create_app(&state, &ana, "groceries").await;
    create_app(&state, &ana, "quiet").await;
    let id = app_id(&state, "groceries").await;
    let now = crate::apps::now_ms();
    // One minute row with all 10 requests in the 50 ms histogram bucket.
    let mut hist = vec![0u64; 14];
    hist[5] = 10;
    sqlx::query(
        "INSERT INTO metric_minute (app_id, minute, requests, s4xx, s5xx, latency_hist, bytes_out, visitors) \
         VALUES (?1, ?2, 10, 2, 1, ?3, 0, 0)",
    )
    .bind(&id)
    .bind(now / 60_000 * 60_000)
    .bind(serde_json::to_string(&hist).unwrap())
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO visitor_day (app_id, day, visitors) VALUES (?1, ?2, 7)")
        .bind(&id)
        .bind(now.div_euclid(86_400_000))
        .execute(&state.pool)
        .await
        .unwrap();
    let hour = now / 3_600_000 * 3_600_000;
    for (path, requests, errors) in [("/home", 6, 1), ("/about", 4, 0)] {
        sqlx::query(
            "INSERT INTO path_hour (app_id, hour, path, requests, s5xx) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&id)
        .bind(hour)
        .bind(path)
        .bind(requests)
        .bind(errors)
        .execute(&state.pool)
        .await
        .unwrap();
    }

    // A stranger's slug answers not_found, exactly like a missing one.
    let (_, stranger) = call(
        &state,
        Some(&bob),
        "app_metrics",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(is_error(&stranger)["error"], "not_found");

    // Only the Console's windows pass; anything else is the caller's bug.
    for hours in [1, 48, 721] {
        let (_, bad) = call(
            &state,
            Some(&ana),
            "app_metrics",
            json!({"app": "groceries", "hours": hours}),
        )
        .await;
        let err = is_error(&bad);
        assert_eq!(err["error"], "invalid_argument");
        assert!(err["message"].as_str().unwrap().contains("24, 168 or 720"));
    }

    let (_, out) = call(
        &state,
        Some(&ana),
        "app_metrics",
        json!({"app": "groceries"}),
    )
    .await;
    let body = result(&out);
    let sc = &body["structuredContent"];
    assert_eq!(sc["app"], "groceries");
    assert_eq!(sc["hours"], 24);
    assert_eq!(sc["requests"], 10);
    assert_eq!(sc["visitors"], 7);
    assert_eq!(sc["errors"], 1);
    assert_eq!(sc["s4xx"], 2);
    assert_eq!(sc["s5xx"], 1);
    assert!((sc["error_rate"].as_f64().unwrap() - 0.1).abs() < 1e-9);
    assert_eq!(sc["p50_ms"], 50);
    assert_eq!(sc["p95_ms"], 50);
    assert_eq!(sc["top_paths"][0]["path"], "/home");
    assert_eq!(sc["top_paths"][0]["requests"], 6);
    assert_eq!(sc["top_paths"][0]["errors"], 1);
    assert_eq!(sc["top_paths"].as_array().unwrap().len(), 2);
    // Paths name app routes, which visitor input can shape: they ride
    // behind the §8.4 notice as their own text block.
    let content = body["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    let data = content[1]["text"].as_str().unwrap();
    assert!(data.starts_with("The following was produced by your app"));
    assert!(data.contains("/home"));

    // A wider window passes through; an app with no traffic answers zeros.
    let (_, wide) = call(
        &state,
        Some(&ana),
        "app_metrics",
        json!({"app": "groceries", "hours": 168}),
    )
    .await;
    assert_eq!(result(&wide)["structuredContent"]["hours"], 168);
    let (_, empty) = call(&state, Some(&ana), "app_metrics", json!({"app": "quiet"})).await;
    let sc = &result(&empty)["structuredContent"];
    assert_eq!(sc["requests"], 0);
    assert_eq!(sc["visitors"], 0);
    assert_eq!(sc["errors"], 0);
    assert_eq!(sc["error_rate"], 0.0);
    assert_eq!(sc["top_paths"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn app_metrics_collaborator_may_read() {
    let (state, _) = setup().await;
    let owner = pro_app(&state, "ana@example.com", "groceries").await;
    invite(&state, &owner, "groceries", "bruno@example.com").await;
    let bruno = api_key_for(&state, "bruno@example.com").await;
    let (_, out) = call(
        &state,
        Some(&bruno),
        "app_metrics",
        json!({"app": "groceries"}),
    )
    .await;
    assert!(result(&out).get("isError").is_none(), "{out}");
    assert_eq!(result(&out)["structuredContent"]["app"], "groceries");
}

#[tokio::test]
async fn db_query_reads_rows_through_the_console_path() {
    let q = query_setup("groceries", &[("DB", "app-db")]).await;
    let (_, out) = call(
        &q.state,
        Some(&q.token),
        "db_query",
        json!({"app": "groceries", "sql": "SELECT * FROM users"}),
    )
    .await;
    let body = result(&out);
    let sc = &body["structuredContent"];
    assert_eq!(sc["app"], "groceries");
    assert_eq!(sc["binding"], "DB");
    assert_eq!(sc["row_count"], 1);
    assert_eq!(sc["truncated"], false);
    assert_eq!(sc["rows"][0]["name"], "ada");
    assert_eq!(sc["columns"].as_array().unwrap().len(), 2);
    // One `celld d1 execute` with the scoped bucket, JSON output, and the
    // app's database identity.
    let calls = query_calls(&q);
    assert!(calls.contains("d1 execute app-db"), "{calls}");
    assert!(calls.contains("--command SELECT * FROM users"), "{calls}");
    assert!(calls.contains("--json"), "{calls}");
    // Rows may hold visitor data: they ride behind the §8.4 notice.
    let content = body["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    let data = content[1]["text"].as_str().unwrap();
    assert!(data.starts_with("The following was produced by your app"));
    assert!(data.contains("ada"));
}

#[tokio::test]
async fn db_query_caps_rows_values_and_bytes() {
    let q = query_setup("groceries", &[("DB", "app-db")]).await;
    // 250 rows in, 200 out.
    let (_, out) = call(
        &q.state,
        Some(&q.token),
        "db_query",
        json!({"app": "groceries", "sql": "SELECT * FROM MANYROWS"}),
    )
    .await;
    let sc = &result(&out)["structuredContent"];
    assert_eq!(sc["row_count"], 200);
    assert_eq!(sc["truncated"], true);
    assert_eq!(sc["rows"][199]["id"], 200);
    assert!(result(&out)["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("(truncated)"));

    // One 300 KiB value is cut to the per-cell cap.
    let (_, out) = call(
        &q.state,
        Some(&q.token),
        "db_query",
        json!({"app": "groceries", "sql": "SELECT big FROM BIGONE"}),
    )
    .await;
    let sc = &result(&out)["structuredContent"];
    assert_eq!(sc["row_count"], 1);
    assert_eq!(sc["truncated"], true);
    assert_eq!(sc["rows"][0]["big"].as_str().unwrap().chars().count(), 5001);

    // 200 wide rows shed trailing rows past the byte cap.
    let (_, out) = call(
        &q.state,
        Some(&q.token),
        "db_query",
        json!({"app": "groceries", "sql": "SELECT * FROM HUGEROWS"}),
    )
    .await;
    let sc = &result(&out)["structuredContent"];
    let n = sc["row_count"].as_u64().unwrap();
    assert!(n < 200 && n > 50, "{n}");
    assert_eq!(sc["truncated"], true);
    let bytes = serde_json::to_string(&sc["rows"]).unwrap().len();
    assert!(bytes <= 250_000, "{bytes}");
}

#[tokio::test]
async fn db_query_refuses_writes_before_touching_celld() {
    let q = query_setup("groceries", &[("DB", "app-db")]).await;
    for sql in [
        "DELETE FROM users",
        "INSERT INTO users VALUES (1)",
        "UPDATE users SET name = 'x'",
        "ATTACH DATABASE 'x' AS y",
        "PRAGMA writable_schema = ON",
        "SELECT 1; DROP TABLE users",
        "WITH x AS (SELECT 1) UPDATE t SET a = 2",
        "SELECT load_extension('evil')",
    ] {
        let (_, out) = call(
            &q.state,
            Some(&q.token),
            "db_query",
            json!({"app": "groceries", "sql": sql}),
        )
        .await;
        let err = is_error(&out);
        assert_eq!(err["error"], "invalid_argument", "{sql}");
        // The refusal names where writes live: migrations and restore.
        let message = err["message"].as_str().unwrap();
        assert!(message.contains("migrations"), "{sql}: {message}");
        assert!(message.contains("restore_backup"), "{sql}: {message}");
    }
    // No refused shape ever reached celld.
    assert!(!q.statedir.path().join("calls.log").exists());
    // An engine refusal (missing table) is still the caller's bug.
    let (_, out) = call(
        &q.state,
        Some(&q.token),
        "db_query",
        json!({"app": "groceries", "sql": "SELECT * FROM FAILME"}),
    )
    .await;
    let err = is_error(&out);
    assert_eq!(err["error"], "invalid_argument");
    assert!(err["message"].as_str().unwrap().contains("no such table"));
}

#[tokio::test]
async fn db_query_binds_params_server_side() {
    let q = query_setup("groceries", &[("DB", "app-db")]).await;
    let (_, out) = call(
        &q.state,
        Some(&q.token),
        "db_query",
        json!({"app": "groceries",
               "sql": "SELECT * FROM users WHERE id = ? AND name <> ?",
               "params": [1, "o'brien"]}),
    )
    .await;
    assert!(result(&out).get("isError").is_none(), "{out}");
    // Numbers stay bare, strings are quoted server-side: no smuggling.
    let calls = query_calls(&q);
    assert!(
        calls.contains("WHERE id = 1 AND name <> 'o''brien'"),
        "{calls}"
    );
    // A hole count mismatch is the caller's bug.
    let (_, out) = call(
        &q.state,
        Some(&q.token),
        "db_query",
        json!({"app": "groceries", "sql": "SELECT ? AND ?", "params": [1]}),
    )
    .await;
    let err = is_error(&out);
    assert_eq!(err["error"], "invalid_argument");
    assert!(err["message"].as_str().unwrap().contains("placeholder"));
}

#[tokio::test]
async fn db_query_access_follows_visibility() {
    let q = query_setup("groceries", &[("DB", "app-db")]).await;
    let bob = api_key_for(&q.state, "bob@example.com").await;
    // A stranger's slug answers not_found, exactly like a missing one —
    // even for a well-formed read.
    let (_, out) = call(
        &q.state,
        Some(&bob),
        "db_query",
        json!({"app": "groceries", "sql": "SELECT 1"}),
    )
    .await;
    assert_eq!(is_error(&out)["error"], "not_found");
}

#[tokio::test]
async fn db_query_collaborator_may_read() {
    let q = query_setup("groceries", &[("DB", "app-db")]).await;
    invite(&q.state, &q.token, "groceries", "bruno@example.com").await;
    let bruno = api_key_for(&q.state, "bruno@example.com").await;
    // A collaborator passes the access check: a write-shaped query is
    // refused as invalid_argument, never not_found — and a real read runs.
    let (_, refused) = call(
        &q.state,
        Some(&bruno),
        "db_query",
        json!({"app": "groceries", "sql": "DELETE FROM users"}),
    )
    .await;
    assert_eq!(is_error(&refused)["error"], "invalid_argument");
    let (_, out) = call(
        &q.state,
        Some(&bruno),
        "db_query",
        json!({"app": "groceries", "sql": "PRAGMA table_info(users)"}),
    )
    .await;
    assert!(result(&out).get("isError").is_none(), "{out}");
    assert_eq!(result(&out)["structuredContent"]["binding"], "DB");
}

#[tokio::test]
async fn db_query_resolves_the_default_binding() {
    let q = query_setup("groceries", &[("DB", "app-db")]).await;
    // An explicit binding works, including PRAGMA table_info.
    let (_, out) = call(
        &q.state,
        Some(&q.token),
        "db_query",
        json!({"app": "groceries", "binding": "DB", "sql": "PRAGMA table_info(users)"}),
    )
    .await;
    assert!(result(&out).get("isError").is_none(), "{out}");
    // An unknown binding names the problem before any celld call.
    let (_, out) = call(
        &q.state,
        Some(&q.token),
        "db_query",
        json!({"app": "groceries", "binding": "NOPE", "sql": "SELECT 1"}),
    )
    .await;
    let err = is_error(&out);
    assert_eq!(err["error"], "invalid_argument");
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("unknown database"));

    // Several bindings need an explicit choice.
    let q2 = query_setup("stock", &[("DB", "app-db"), ("PLAIN", "plain-db")]).await;
    let (_, out) = call(
        &q2.state,
        Some(&q2.token),
        "db_query",
        json!({"app": "stock", "sql": "SELECT 1"}),
    )
    .await;
    let err = is_error(&out);
    assert_eq!(err["error"], "invalid_argument");
    assert!(err["message"].as_str().unwrap().contains("binding"));

    // An app with no deployed D1 binding fails before any celld call.
    create_app(&q2.state, &q2.token, "plain").await;
    let (_, out) = call(
        &q2.state,
        Some(&q2.token),
        "db_query",
        json!({"app": "plain", "sql": "SELECT 1"}),
    )
    .await;
    assert_eq!(is_error(&out)["error"], "invalid_argument");
    assert!(!q2.statedir.path().join("calls.log").exists());
}
// Drafts (spec §1.5/§1.6): stage → diff/check/ship, discard, rebase,
// conflicts, collaborator/stranger access, structured deploy diagnostics.

/// A build log in the shapes the real tools emit: esbuild-style failure
/// plus one tsc error.
const DRAFT_TSC_LOG: &str = "=== install ===\ninstalled\n=== build ===\n✘ [ERROR] Could not resolve \"missing-dep\"\n    src/main.ts:3:7:\nerror: build failed\nsrc/routes/index.tsx(12,34): error TS2322: Type 'number' is not assignable to type 'string'.\n";

async fn account_id(state: &McpState, email: &str) -> String {
    sqlx::query_scalar("SELECT id FROM user WHERE email = ?1")
        .bind(email)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

async fn code_changes(state: &McpState, email: &str) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(SUM(code_changes), 0) FROM usage_day WHERE userId = ?1")
        .bind(account_id(state, email).await)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

async fn build_ms_used(state: &McpState, email: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT build_ms FROM usage_day WHERE userId = ?1")
        .bind(account_id(state, email).await)
        .fetch_optional(&state.pool)
        .await
        .unwrap()
}

async fn history_len(state: &McpState, token: &str, slug: &str) -> usize {
    let (_, history) = call(state, Some(token), "history", json!({"app": slug})).await;
    result(&history)["structuredContent"]["commits"]
        .as_array()
        .unwrap()
        .len()
}

#[tokio::test]
async fn draft_stage_accumulates_and_diffs_without_spending() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;

    let (_, staged) = call(
        &state,
        Some(&token),
        "stage",
        json!({
            "app": "groceries",
            "writes": [{"path": "notes.txt", "content": "hello\n"}],
            "edits": [{"path": "pages/index.tsx", "old": "Groceries", "new": "Berries"}],
        }),
    )
    .await;
    let draft = result(&staged)["structuredContent"]["draft"].clone();
    assert_eq!(draft["staged"]["writes"], 1);
    assert_eq!(draft["staged"]["edits"], 1);
    let base = draft["base"].as_str().unwrap().to_string();

    // A second stage accumulates onto the same base.
    let (_, staged2) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries", "deletes": ["lib/moderate.ts"]}),
    )
    .await;
    let draft2 = result(&staged2)["structuredContent"]["draft"].clone();
    assert_eq!(draft2["base"], base);
    assert_eq!(draft2["staged"]["deletes"], 1);
    assert_eq!(draft2["staged"]["writes"], 1);

    // Staging spends nothing and starts no deploy.
    assert_eq!(code_changes(&state, "ana@example.com").await, 0);
    let deploys: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deploy WHERE app_id = (SELECT id FROM app WHERE slug = 'groceries')",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(deploys, 1, "only the seed deploy exists");

    let (_, diff) = call(
        &state,
        Some(&token),
        "diff_draft",
        json!({"app": "groceries"}),
    )
    .await;
    let shown = result(&diff)["structuredContent"]["draft"].clone();
    assert_eq!(shown["base"], base);
    let paths: Vec<&str> = shown["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"notes.txt"), "{paths:?}");
    assert!(paths.contains(&"pages/index.tsx"), "{paths:?}");
    assert!(paths.contains(&"lib/moderate.ts"), "{paths:?}");
    assert!(shown["patch"].as_str().unwrap().contains("Berries"));
}

#[tokio::test]
async fn draft_check_pass_charges_build_minutes_not_code_changes() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, _) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries", "writes": [{"path": "notes.txt", "content": "hi\n"}]}),
    )
    .await;

    let (_, checked) = call(&state, Some(&token), "check", json!({"app": "groceries"})).await;
    let check = &result(&checked)["structuredContent"]["check"];
    assert_eq!(check["status"], "pass");
    assert_eq!(check["diagnostics"].as_array().unwrap().len(), 0);

    // Build minutes move, code changes do not.
    assert!(
        build_ms_used(&state, "ana@example.com").await.is_some(),
        "the check charges its wall time"
    );
    assert_eq!(code_changes(&state, "ana@example.com").await, 0);
}

#[tokio::test]
async fn draft_check_fail_returns_structured_diagnostics() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, _) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries", "writes": [{"path": "notes.txt", "content": "hi\n"}]}),
    )
    .await;
    state.deploys.set_fake_check(crate::deploy::CheckOutput {
        steps: vec![
            crate::deploy::CheckStep {
                name: "install".to_string(),
                ms: 1,
                skipped: false,
                ok: true,
            },
            crate::deploy::CheckStep {
                name: "build".to_string(),
                ms: 1,
                skipped: false,
                ok: false,
            },
            crate::deploy::CheckStep {
                name: "typecheck".to_string(),
                ms: 1,
                skipped: false,
                ok: false,
            },
            crate::deploy::CheckStep {
                name: "lint".to_string(),
                ms: 1,
                skipped: true,
                ok: true,
            },
            crate::deploy::CheckStep {
                name: "tests".to_string(),
                ms: 1,
                skipped: true,
                ok: true,
            },
        ],
        log: DRAFT_TSC_LOG.to_string(),
    });

    let (_, checked) = call(&state, Some(&token), "check", json!({"app": "groceries"})).await;
    let check = &result(&checked)["structuredContent"]["check"];
    assert_eq!(check["status"], "fail");
    let diagnostics = check["diagnostics"].as_array().unwrap();
    assert!(!diagnostics.is_empty(), "a failed check names its errors");
    assert!(
        diagnostics.iter().any(|d| d["code"] == "TS2322"
            && d["file"] == "src/routes/index.tsx"
            && d["line"] == 12),
        "{diagnostics:?}"
    );
    assert_eq!(code_changes(&state, "ana@example.com").await, 0);
}

#[tokio::test]
async fn draft_ship_is_one_commit_and_clears() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    assert_eq!(history_len(&state, &token, "groceries").await, 1);

    let (_, _) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries", "writes": [{"path": "notes.txt", "content": "hi\n"}]}),
    )
    .await;
    let (_, shipped) = call(
        &state,
        Some(&token),
        "ship",
        json!({"app": "groceries", "message": "add notes"}),
    )
    .await;
    let structured = &result(&shipped)["structuredContent"];
    assert_eq!(structured["deploy"]["status"], "success");
    assert_eq!(
        structured["draft"],
        json!({"cleared": true, "rebased": false})
    );
    assert!(structured["commit"]["sha"].as_str().unwrap().len() == 40);
    assert_eq!(history_len(&state, &token, "groceries").await, 2);
    assert_eq!(code_changes(&state, "ana@example.com").await, 1);

    // The draft is gone: diff and ship both report it.
    let (_, diff) = call(
        &state,
        Some(&token),
        "diff_draft",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(result(&diff)["structuredContent"]["error"], "not_found");
    let (_, reship) = call(
        &state,
        Some(&token),
        "ship",
        json!({"app": "groceries", "message": "again"}),
    )
    .await;
    assert_eq!(result(&reship)["structuredContent"]["error"], "not_found");
}

#[tokio::test]
async fn draft_ship_rebases_onto_a_moved_head() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, _) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries",
               "edits": [{"path": "pages/index.tsx", "old": "Groceries", "new": "Berries"}]}),
    )
    .await;
    // Someone else lands first, on a different file: the head moves but the
    // draft still applies.
    let (_, _) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "other file",
               "writes": [{"path": "lib/other.ts", "content": "export {};\n"}]}),
    )
    .await;
    let (_, shipped) = call(
        &state,
        Some(&token),
        "ship",
        json!({"app": "groceries", "message": "berries"}),
    )
    .await;
    let structured = &result(&shipped)["structuredContent"];
    assert_eq!(structured["deploy"]["status"], "success");
    assert_eq!(
        structured["draft"],
        json!({"cleared": true, "rebased": true})
    );
    assert_eq!(history_len(&state, &token, "groceries").await, 3);
    assert_eq!(code_changes(&state, "ana@example.com").await, 2);
}

#[tokio::test]
async fn draft_ship_conflict_keeps_the_draft() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, _) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries",
               "edits": [{"path": "pages/index.tsx", "old": "Groceries", "new": "Berries"}]}),
    )
    .await;
    // The head moves through the same text: the draft no longer applies.
    let (_, moved) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "apples",
               "edits": [{"path": "pages/index.tsx", "old": "Groceries", "new": "Apples"}]}),
    )
    .await;
    let current = result(&moved)["structuredContent"]["commit"]["sha"].clone();

    let (_, shipped) = call(
        &state,
        Some(&token),
        "ship",
        json!({"app": "groceries", "message": "berries"}),
    )
    .await;
    let err = is_error(&shipped);
    assert_eq!(err["error"], "conflict");
    assert_eq!(err["current_sha"], current);

    // The draft survives the conflict.
    let (_, diff) = call(
        &state,
        Some(&token),
        "diff_draft",
        json!({"app": "groceries"}),
    )
    .await;
    assert!(result(&diff).get("isError").is_none());
    assert_eq!(history_len(&state, &token, "groceries").await, 2);
}

#[tokio::test]
async fn draft_discard_is_idempotent() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, _) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries", "writes": [{"path": "notes.txt", "content": "hi\n"}]}),
    )
    .await;
    let (_, dropped) = call(
        &state,
        Some(&token),
        "discard_draft",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(
        result(&dropped)["structuredContent"]["draft"]["dropped"],
        true
    );
    let (_, dropped2) = call(
        &state,
        Some(&token),
        "discard_draft",
        json!({"app": "groceries"}),
    )
    .await;
    assert_eq!(
        result(&dropped2)["structuredContent"]["draft"]["dropped"],
        false
    );
    assert_eq!(code_changes(&state, "ana@example.com").await, 0);
}

#[tokio::test]
async fn draft_tools_refuse_strangers_but_serve_collaborators() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let stranger = api_key_for(&state, "stranger@example.com").await;

    for tool in ["stage", "diff_draft", "check", "ship", "discard_draft"] {
        let args = if tool == "stage" {
            json!({"app": "groceries", "writes": [{"path": "x.txt", "content": "x\n"}]})
        } else if tool == "ship" {
            json!({"app": "groceries", "message": "x"})
        } else {
            json!({"app": "groceries"})
        };
        let (_, denied) = call(&state, Some(&stranger), tool, args).await;
        assert_eq!(
            result(&denied)["structuredContent"]["error"],
            "not_found",
            "{tool} leaks nothing to strangers"
        );
    }

    // A collaborator stages onto their own draft (Pro owner, spec §7.4).
    sqlx::query("UPDATE user SET plan = 'pro' WHERE email = 'ana@example.com'")
        .execute(&state.pool)
        .await
        .unwrap();
    let (_, _) = call(
        &state,
        Some(&token),
        "add_collaborator",
        json!({"app": "groceries", "email": "cara@example.com"}),
    )
    .await;
    let cara = api_key_for(&state, "cara@example.com").await;
    let (_, staged) = call(
        &state,
        Some(&cara),
        "stage",
        json!({"app": "groceries", "writes": [{"path": "cara.txt", "content": "hi\n"}]}),
    )
    .await;
    assert_eq!(
        result(&staged)["structuredContent"]["draft"]["staged"]["writes"],
        1
    );
}

#[tokio::test]
async fn failed_commit_deploy_returns_structured_diagnostics() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let mut failed = FakeOutcome::failed();
    failed.log_tail = Some(DRAFT_TSC_LOG);
    state.deploys.set_fake(failed);

    let (_, committed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "groceries", "message": "broken",
               "writes": [{"path": "q.txt", "content": "q"}]}),
    )
    .await;
    let deploy = &result(&committed)["structuredContent"]["deploy"];
    assert_eq!(deploy["status"], "failed");
    let diagnostics = deploy["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics
            .iter()
            .any(|d| d["code"] == "TS2322" && d["file"] == "src/routes/index.tsx"),
        "{diagnostics:?}"
    );

    // The follow-up read carries the same diagnostics.
    let (_, status) = call(
        &state,
        Some(&token),
        "deploy_status",
        json!({"app": "groceries"}),
    )
    .await;
    let seen = &result(&status)["structuredContent"]["diagnostics"];
    assert!(
        seen.as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "TS2322"),
        "{seen:?}"
    );
}

#[tokio::test]
async fn draft_stage_line_edits_and_inserts_ship() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    // pages/index.tsx is one line: "<h1>Groceries</h1>".
    let (_, staged) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries",
               "line_edits": [{"path": "pages/index.tsx", "start_line": 1, "end_line": 1,
                               "new": "<h1>Berries</h1>"}],
               "inserts": [{"path": "pages/index.tsx", "after_line": 1,
                             "insert": "<p>fresh</p>\n"}]}),
    )
    .await;
    let draft = &result(&staged)["structuredContent"]["draft"];
    assert_eq!(draft["staged"]["line_edits"], 1);
    assert_eq!(draft["staged"]["inserts"], 1);
    let (_, diff) = call(
        &state,
        Some(&token),
        "diff_draft",
        json!({"app": "groceries"}),
    )
    .await;
    let patch = result(&diff)["structuredContent"]["draft"]["patch"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(patch.contains("Berries"), "{patch}");
    assert!(patch.contains("fresh"), "{patch}");
    let (_, shipped) = call(
        &state,
        Some(&token),
        "ship",
        json!({"app": "groceries", "message": "line edits"}),
    )
    .await;
    assert_eq!(
        result(&shipped)["structuredContent"]["deploy"]["status"],
        "success"
    );
    assert_eq!(history_len(&state, &token, "groceries").await, 2);
}

#[tokio::test]
async fn draft_stage_patch_ships() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let patch = "--- a/pages/index.tsx\n+++ b/pages/index.tsx\n@@ -1 +1 @@\n-<h1>Groceries</h1>\n+<h1>Figs</h1>\n";
    let (_, staged) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries", "patch": patch}),
    )
    .await;
    assert_eq!(
        result(&staged)["structuredContent"]["draft"]["staged"]["patch"],
        true
    );
    let (_, shipped) = call(
        &state,
        Some(&token),
        "ship",
        json!({"app": "groceries", "message": "patched"}),
    )
    .await;
    assert_eq!(
        result(&shipped)["structuredContent"]["deploy"]["status"],
        "success"
    );
    let (_, read) = call(
        &state,
        Some(&token),
        "read_files",
        json!({"app": "groceries", "files": [{"path": "pages/index.tsx"}]}),
    )
    .await;
    assert!(result(&read)["structuredContent"]["files"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Figs"));
}

#[tokio::test]
async fn draft_patch_refuses_mixing_and_second_patch() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let patch = "--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+hi\n";
    let (_, _) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries", "patch": patch}),
    )
    .await;
    // Other ops on a patch draft refuse, and a second patch refuses too.
    for args in [
        json!({"app": "groceries", "writes": [{"path": "x.txt", "content": "x"}]}),
        json!({"app": "groceries", "patch": patch}),
    ] {
        let (_, denied) = call(&state, Some(&token), "stage", args).await;
        assert_eq!(
            result(&denied)["structuredContent"]["error"],
            "invalid_argument",
            "a patch stages alone"
        );
    }
    // The reverse order refuses the same way.
    let (_, _) = call(
        &state,
        Some(&token),
        "discard_draft",
        json!({"app": "groceries"}),
    )
    .await;
    let (_, _) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries", "writes": [{"path": "x.txt", "content": "x"}]}),
    )
    .await;
    let (_, denied) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries", "patch": patch}),
    )
    .await;
    assert_eq!(
        result(&denied)["structuredContent"]["error"],
        "invalid_argument"
    );
    // The first refusal kept nothing partial: the draft still holds the write.
    let (_, diff) = call(
        &state,
        Some(&token),
        "diff_draft",
        json!({"app": "groceries"}),
    )
    .await;
    let paths: Vec<&str> = result(&diff)["structuredContent"]["draft"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["x.txt"], "{paths:?}");
}

#[tokio::test]
async fn draft_stale_expect_blob_conflicts_at_stage() {
    let (state, _) = setup().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "groceries").await;
    let (_, denied) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries",
               "edits": [{"path": "pages/index.tsx", "old": "Groceries", "new": "Berries",
                          "expect_blob": "0000000000000000000000000000000000000000"}]}),
    )
    .await;
    let err = is_error(&denied);
    assert_eq!(err["error"], "conflict");
    assert_eq!(err["path"], "pages/index.tsx");
    // The fresh blob from read_files stages cleanly.
    let (_, read) = call(
        &state,
        Some(&token),
        "read_files",
        json!({"app": "groceries", "files": [{"path": "pages/index.tsx"}]}),
    )
    .await;
    let blob = result(&read)["structuredContent"]["files"][0]["blob"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, staged) = call(
        &state,
        Some(&token),
        "stage",
        json!({"app": "groceries",
               "edits": [{"path": "pages/index.tsx", "old": "Groceries", "new": "Berries",
                          "expect_blob": blob}]}),
    )
    .await;
    assert_eq!(
        result(&staged)["structuredContent"]["draft"]["staged"]["edits"],
        1
    );
}

/// apply-examples behaviour tests: `add_example`, `create_app {examples}`
/// and the slimmer `examples` shape, against a vendored templates repo that
/// carries markers plus `passkeys`/`photos` (app) and `newsletter` (website).
const APPLY_NAV: &str = "import { House, Settings } from \"lucide\"; // outer:nav-icons\nimport type { LucideIcon } from \"lucide\";\n\nexport const NAV: readonly NavItem[] = [\n  { href: \"/\", label: \"Home\", icon: House },\n  // outer:nav-items\n  { href: \"/settings\", label: \"Settings\", icon: Settings },\n];\n";
const APPLY_VOID: &str = "export default {\n  cloudflare: {\n    // outer:bindings\n  },\n};\n";
const APPLY_PACKAGE: &str = "{\n  \"name\": \"seed\",\n  \"private\": true\n}\n";

async fn setup_apply() -> (McpState, Codes) {
    let dir = tempfile::tempdir().unwrap();
    let pool = crate::db::open(&dir.path().join("outer.sqlite"))
        .await
        .unwrap();
    std::mem::forget(dir);
    let (auth_cfg, codes) = auth_ts::test_config();
    let auth_state = crate::auth::AuthState {
        pool: pool.clone(),
        config: auth_cfg,
    };
    let state = McpState::for_tests(pool, auth_state);
    crate::templates::vendor_test_templates_with(
        &state.store,
        &[
            ("package.json", APPLY_PACKAGE),
            ("pages/_nav.ts", APPLY_NAV),
            ("void.config.ts", APPLY_VOID),
            (
                "pages/index.tsx",
                "export default function Home() {\n  return <h1>Hi</h1>;\n}\n",
            ),
        ],
        &[
            ("package.json", APPLY_PACKAGE),
            ("void.config.ts", APPLY_VOID),
            ("src/pages/index.astro", "---\n---\n<h1>Hi</h1>\n"),
        ],
        &[
            (
                "passkeys",
                r#"{"title": "Passkeys", "summary": "Sign in with passkeys.", "template": "app", "requires": [], "nav": [{"href": "/sign-in", "label": "Sign in", "icon": "KeyRound"}, {"href": "/members", "label": "Members", "icon": "Users"}], "bindings": {}, "dependencies": {}}"#,
                &[
                    ("README.md", "# Passkeys\n\nSign in.\n"),
                    (
                        "pages/sign-in.tsx",
                        "export default function SignIn() {\n  return <h1>Sign in</h1>;\n}\n",
                    ),
                    (
                        "pages/_session.ts",
                        "export async function getViewer() {\n  return null;\n}\n",
                    ),
                ],
            ),
            (
                "photos",
                r#"{"title": "Photos", "summary": "A photo feed.", "template": "app", "requires": ["passkeys"], "nav": [{"href": "/photos", "label": "Photos", "icon": "Camera"}], "bindings": {"r2_buckets": [{"binding": "PHOTOS", "bucket_name": "photos"}]}, "dependencies": {"some-pkg": "^1.0.0"}}"#,
                &[
                    ("README.md", "# Photos\n\nPost photos.\n"),
                    (
                        "pages/photos.tsx",
                        "export default function Photos() {\n  return <h1>Photos</h1>;\n}\n",
                    ),
                    (
                        "db/migrations/0001_photos.sql",
                        "CREATE TABLE photos (\n  id TEXT PRIMARY KEY\n);\n",
                    ),
                ],
            ),
            (
                "newsletter",
                r#"{"title": "Newsletter", "summary": "Email signup.", "template": "website", "requires": [], "nav": [], "bindings": {"d1_databases": [{"binding": "DB", "database_name": "newsletter"}]}, "dependencies": {}}"#,
                &[
                    ("README.md", "# Newsletter\n\nSubscribe.\n"),
                    ("src/pages/subscribe.astro", "---\n---\n<h1>Subscribe</h1>\n"),
                    (
                        "db/migrations/0001_sub.sql",
                        "CREATE TABLE subs (\n  id TEXT PRIMARY KEY\n);\n",
                    ),
                ],
            ),
        ],
    )
    .await;
    (state, codes)
}

/// The head tree's file texts, keyed by path.
async fn head_texts(state: &McpState, token: &str, app: &str, paths: &[&str]) -> Value {
    let files: Vec<Value> = paths.iter().map(|path| json!({"path": path})).collect();
    let (_, read) = call(
        state,
        Some(token),
        "read_files",
        json!({"app": app, "files": files}),
    )
    .await;
    result(&read)["structuredContent"]["files"].clone()
}

#[tokio::test]
async fn add_example_applies_requires_nav_bindings_and_deploys() {
    let (state, _) = setup_apply().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "familygram").await;
    let (_, added) = call(
        &state,
        Some(&token),
        "add_example",
        json!({"app": "familygram", "name": "photos"}),
    )
    .await;
    let structured = &result(&added)["structuredContent"];
    // `requires` pulls `passkeys` in, requires first.
    assert_eq!(structured["added"], json!(["passkeys", "photos"]));
    assert_eq!(
        structured["nav"],
        json!(["/sign-in", "/members", "/photos"])
    );
    assert_eq!(structured["deploy"]["status"], "success");
    for path in [
        "pages/sign-in.tsx",
        "pages/_session.ts",
        "pages/photos.tsx",
        "db/migrations/0001_photos.sql",
    ] {
        assert!(
            structured["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file == path),
            "missing {path}: {}",
            structured["files"]
        );
    }
    let texts = head_texts(
        &state,
        &token,
        "familygram",
        &[
            "pages/_nav.ts",
            "void.config.ts",
            "package.json",
            "pages/photos.tsx",
            "db/migrations/0001_photos.sql",
        ],
    )
    .await;
    // `read_files` numbers lines (`{n}\u{2502} {text}`): strip the numbers.
    let by_path = |path: &str| {
        let numbered = texts
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == path)
            .unwrap_or_else(|| panic!("no {path}"))
            .get("text")
            .unwrap()
            .as_str()
            .unwrap();
        let out: Vec<&str> = numbered
            .lines()
            .map(|line| {
                line.split_once('│')
                    .map(|(_, rest)| rest.strip_prefix(' ').unwrap_or(rest))
                    .unwrap_or(line)
            })
            .collect();
        let mut text = out.join("\n");
        text.push('\n');
        text
    };
    let nav = by_path("pages/_nav.ts");
    assert!(
        nav.contains("import { Camera, House, KeyRound, Settings, Users }"),
        "{nav}"
    );
    for href in ["/sign-in", "/members", "/photos"] {
        assert!(nav.contains(&format!("href: \"{href}\"")), "{nav}");
    }
    let config = by_path("void.config.ts");
    assert!(
        config.contains("r2_buckets: [{ binding: \"PHOTOS\", bucket_name: \"photos\" }],"),
        "{config}"
    );
    let package: Value = serde_json::from_str(&by_path("package.json")).unwrap();
    assert_eq!(package["dependencies"]["some-pkg"], "^1.0.0");
    assert_eq!(
        by_path("pages/photos.tsx"),
        "export default function Photos() {\n  return <h1>Photos</h1>;\n}\n"
    );
    assert_eq!(
        by_path("db/migrations/0001_photos.sql"),
        "CREATE TABLE photos (\n  id TEXT PRIMARY KEY\n);\n"
    );
    // Seed plus the one apply commit.
    assert_eq!(history_len(&state, &token, "familygram").await, 2);
}

#[tokio::test]
async fn add_example_reapply_is_a_noop_refusal() {
    let (state, _) = setup_apply().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "familygram").await;
    let (_, first) = call(
        &state,
        Some(&token),
        "add_example",
        json!({"app": "familygram", "name": "passkeys"}),
    )
    .await;
    assert_eq!(
        result(&first)["structuredContent"]["added"],
        json!(["passkeys"])
    );
    let (_, again) = call(
        &state,
        Some(&token),
        "add_example",
        json!({"app": "familygram", "name": "passkeys"}),
    )
    .await;
    let err = is_error(&again);
    assert_eq!(err["error"], "invalid_argument");
    assert!(
        err["message"].as_str().unwrap().contains("already applied"),
        "{}",
        err["message"]
    );
    assert_eq!(history_len(&state, &token, "familygram").await, 2);
}

#[tokio::test]
async fn add_example_conflicting_file_refuses_without_a_commit() {
    let (state, _) = setup_apply().await;
    let token = api_key_for(&state, "ana@example.com").await;
    create_app(&state, &token, "familygram").await;
    let (_, committed) = call(
        &state,
        Some(&token),
        "commit",
        json!({"app": "familygram", "message": "own photos page",
               "writes": [{"path": "pages/photos.tsx", "content": "mine\n"}]}),
    )
    .await;
    assert_eq!(
        result(&committed)["structuredContent"]["deploy"]["status"],
        "success"
    );
    let (_, refused) = call(
        &state,
        Some(&token),
        "add_example",
        json!({"app": "familygram", "name": "photos"}),
    )
    .await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "invalid_argument");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("pages/photos.tsx"),
        "{}",
        err["message"]
    );
    assert_eq!(history_len(&state, &token, "familygram").await, 2);
}

#[tokio::test]
async fn add_example_wrong_template_is_refused() {
    let (state, _) = setup_apply().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let (_, created) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "site", "template": "website"}),
    )
    .await;
    assert_eq!(
        result(&created)["structuredContent"]["deploy"]["status"],
        "success"
    );
    let (_, refused) = call(
        &state,
        Some(&token),
        "add_example",
        json!({"app": "site", "name": "passkeys"}),
    )
    .await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "invalid_argument");
    assert!(
        err["message"].as_str().unwrap().contains("website"),
        "{}",
        err["message"]
    );
    assert_eq!(history_len(&state, &token, "site").await, 1);
}

#[tokio::test]
async fn create_app_with_examples_makes_exactly_one_commit() {
    let (state, _) = setup_apply().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let (_, created) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "familygram", "examples": ["photos"]}),
    )
    .await;
    let body = result(&created);
    let text = body["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("with passkeys, photos"), "{text}");
    assert!(text.contains("/photos"), "{text}");
    let structured = &body["structuredContent"];
    assert_eq!(structured["added"], json!(["passkeys", "photos"]));
    assert_eq!(structured["deploy"]["status"], "success");
    // One commit holds the seed and the examples together.
    assert_eq!(history_len(&state, &token, "familygram").await, 1);
    let texts = head_texts(&state, &token, "familygram", &["pages/photos.tsx"]).await;
    let photos = texts[0]["text"].as_str().unwrap();
    assert!(
        photos.contains("export default function Photos()"),
        "{photos}"
    );
    assert!(photos.contains("return <h1>Photos</h1>;"), "{photos}");
}

#[tokio::test]
async fn create_app_unknown_example_refuses_before_creation() {
    let (state, _) = setup_apply().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let (_, refused) = call(
        &state,
        Some(&token),
        "create_app",
        json!({"slug": "familygram", "examples": ["nope"]}),
    )
    .await;
    let err = is_error(&refused);
    assert_eq!(err["error"], "invalid_argument");
    assert!(
        err["message"].as_str().unwrap().contains("nope"),
        "{}",
        err["message"]
    );
    let (_, missing) = call(
        &state,
        Some(&token),
        "get_app",
        json!({"app": "familygram"}),
    )
    .await;
    assert_eq!(is_error(&missing)["error"], "not_found");
}

#[tokio::test]
async fn examples_without_files_is_small_and_text_only() {
    let (state, _) = setup_apply().await;
    let token = api_key_for(&state, "ana@example.com").await;
    let (_, body) = call(&state, Some(&token), "examples", json!({"name": "photos"})).await;
    let text = result(&body)["content"][0]["text"].as_str().unwrap();
    assert!(text.len() < 6 * 1024, "stays small: {}", text.len());
    assert!(
        !text.contains("return <h1>Photos</h1>"),
        "no contents: {text}"
    );
    let structured = &result(&body)["structuredContent"];
    assert_eq!(structured["requires"], json!(["passkeys"]));
    assert_eq!(structured["nav"][0]["href"], "/photos");
    assert_eq!(structured["bindings"]["r2_buckets"][0]["binding"], "PHOTOS");
    for entry in structured["files"].as_array().unwrap() {
        assert!(entry.get("text").is_none(), "no contents in structured");
    }
}
