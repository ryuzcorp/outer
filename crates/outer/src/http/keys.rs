//! `/keys`: the one-page web flow for minting an API key (owner decision D3).
//!
//! The MCP surface collects no OTPs and mints no keys: ChatGPT's directory
//! rejects tools that do. Scripts and clients without OAuth sign in at
//! `/console/sign-in?next=/keys`, then create a key here and send it as
//! `Authorization: Bearer <key>`.
//!
//! Rules: platform host only (an explicit Axum route, so it wins over the
//! site fallback); session cookie only; `Sec-Fetch-Site: same-origin` on the
//! POST; no caching, no referrer, no indexing; the key renders once.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use maud::{html, Markup};

use super::console::{fetch_ok, layout, SIGNIN_CSP};
use crate::accounts;
use crate::auth;

pub fn routes() -> Router<super::AppState> {
    Router::new().route("/keys", get(keys_get).post(keys_post))
}

/// The one page a browser-less user needs: sign in, then create a key.
async fn keys_get(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let account = current_account(&state, &headers).await;
    match account {
        Some(account) if account.terms_accepted(&state.mcp.auth.config) => {
            page(StatusCode::OK, key_form(None, None))
        }
        // No session, or a session that still owes the terms: the console
        // sign-in page carries `next=/keys` back here when it finishes.
        _ => see_other("/console/sign-in?next=/keys"),
    }
}

/// `POST /keys`: mint the key once. Same name rules as the console panel
/// (trimmed, at most 100 characters), same audit row, same domain function.
async fn keys_post(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if !fetch_ok(&headers) {
        return page(
            StatusCode::FORBIDDEN,
            layout(
                "Keys",
                html! {
                    p { "Open this page on outer.now and try again." }
                },
            ),
        );
    }
    let Some(account) = current_account(&state, &headers).await else {
        return see_other("/console/sign-in?next=/keys");
    };
    if !account.terms_accepted(&state.mcp.auth.config) {
        return see_other("/console/sign-in?next=/keys");
    }
    let trimmed = form.get("name").map(|name| name.trim()).unwrap_or("");
    let name = (!trimmed.is_empty()).then_some(trimmed);
    if name.map(|name| name.chars().count() > 100).unwrap_or(false) {
        return page(
            StatusCode::BAD_REQUEST,
            key_form(Some("Keep the key name under 100 characters."), name),
        );
    }
    let started = auth::now_ms();
    let result = auth::create_api_key(&state.mcp.pool, &account.id, name).await;
    let (outcome, error_code) = match &result {
        Ok(_) => ("ok", None),
        Err(err) => ("error", Some(err.code)),
    };
    accounts::record_account_action(
        &state.mcp.pool,
        &account.id,
        "create_api_key",
        &format!("create_api_key {}", name.unwrap_or("(unnamed)")),
        outcome,
        error_code,
        started,
    )
    .await;
    match result {
        Ok(key) => page(StatusCode::OK, key_page(&key.key)),
        Err(_) => page(
            StatusCode::BAD_REQUEST,
            key_form(
                Some("Outer couldn't create the key. Try again shortly."),
                name,
            ),
        ),
    }
}

/// The signed-in account with current terms, if any. The session cookie is
/// the only credential: a bearer header is ignored here, as in the console.
async fn current_account(state: &super::AppState, headers: &HeaderMap) -> Option<auth::AccountRow> {
    let session = auth::session_from_headers(headers)?;
    auth::account_from_session(&state.mcp.pool, &session)
        .await
        .ok()
        .flatten()
}

fn page(status: StatusCode, body: Markup) -> Response {
    (
        status,
        [
            ("content-type", "text/html; charset=utf-8"),
            ("content-security-policy", SIGNIN_CSP),
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
            ("referrer-policy", "no-referrer"),
            ("x-robots-tag", "noindex"),
        ],
        crate::auth::pages::render(body),
    )
        .into_response()
}

fn see_other(to: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [("location", to), ("cache-control", "no-store")],
        "",
    )
        .into_response()
}

fn key_form(error: Option<&str>, name: Option<&str>) -> Markup {
    layout(
        "Create an API key",
        html! {
            p { "Create an API key for scripts and MCP clients without OAuth. Send it as " code { "Authorization: Bearer <key>" } "." }
            @if let Some(error) = error {
                p style="color:#a00" { (error) }
            }
            form method="post" action="/keys" {
                label {
                    "Key name (optional)" br;
                    input type="text" name="name" value=(name.unwrap_or("")) maxlength="100"
                        autocomplete="off" style="width:100%;padding:.5rem;margin:.25rem 0";
                }
                br;
                button type="submit" style="padding:.5rem 1rem" { "Create API key" }
            }
            p { a href="/console" { "Console" } }
        },
    )
}

fn key_page(key: &str) -> Markup {
    layout(
        "API key created",
        html! {
            p style="color:#a00" { "Copy it now; it is shown once." }
            p {
                code style="display:block;padding:.5rem;background:#eee;word-break:break-all;user-select:all" {
                    (key)
                }
            }
            p { "Send it as " code { "Authorization: Bearer <key>" } "." }
            p { a href="/console" { "Console" } }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::{sign_in_and_accept_terms, test_config, test_pool};
    use crate::store::snapshot::StateSync;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::SqlitePool;
    use tower::ServiceExt;

    async fn app() -> (
        Router,
        SqlitePool,
        crate::auth::AuthConfig,
        crate::auth::test_support::Codes,
    ) {
        let pool = test_pool().await;
        let (config, codes) = test_config();
        let auth = crate::auth::AuthState {
            pool: pool.clone(),
            config: config.clone(),
        };
        let mcp = crate::mcp::McpState::for_tests(pool.clone(), auth.clone());
        let router = crate::http::router(crate::http::AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
            auth,
            mcp,
            site: crate::http::site::SiteDir::new(std::path::Path::new("/nonexistent-site")),
        });
        (router, pool, config, codes)
    }

    async fn send(router: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let res = router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    /// Sign in through the domain path, then mint the browser session the
    /// real sign-in page would set.
    async fn signed_in_cookie(
        pool: &SqlitePool,
        config: &crate::auth::AuthConfig,
        codes: &crate::auth::test_support::Codes,
    ) -> String {
        let (account_id, _) =
            sign_in_and_accept_terms(pool, config, codes, "ana@example.com").await;
        let session = crate::auth::create_session(pool, &account_id)
            .await
            .unwrap();
        format!("__Host-outer_sid={session}")
    }

    fn post_key(name: &str, cookie: Option<&str>, same_origin: bool) -> Request<Body> {
        let mut req =
            Request::post("/keys").header("content-type", "application/x-www-form-urlencoded");
        if same_origin {
            req = req.header("sec-fetch-site", "same-origin");
        }
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        req.body(Body::from(format!("name={name}"))).unwrap()
    }

    #[tokio::test]
    async fn get_without_session_redirects_to_sign_in() {
        let (router, ..) = app().await;
        let (status, headers, _) =
            send(&router, Request::get("/keys").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers["location"], "/console/sign-in?next=/keys");
    }

    #[tokio::test]
    async fn get_with_session_renders_the_create_form() {
        let (router, pool, config, codes) = app().await;
        let cookie = signed_in_cookie(&pool, &config, &codes).await;
        let (status, headers, body) = send(
            &router,
            Request::get("/keys")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["x-robots-tag"], "noindex");
        assert!(body.contains("Create API key"), "form renders, got: {body}");
    }

    #[tokio::test]
    async fn post_mints_a_key_that_authenticates_mcp() {
        let (router, pool, config, codes) = app().await;
        let cookie = signed_in_cookie(&pool, &config, &codes).await;
        let (status, headers, body) = send(&router, post_key("ci", Some(&cookie), true)).await;
        assert_eq!(status, StatusCode::OK, "mint succeeds, got: {body}");
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["pragma"], "no-cache");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert!(body.contains("it is shown once"), "warning renders");
        let start = body.find("outer_").expect("key renders");
        let key = &body[start..start + 58];

        // The minted key authenticates a real `/mcp` call.
        let (status, _, response) = send(
            &router,
            Request::post("/mcp")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": "whoami", "arguments": {}}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let response: serde_json::Value = response.parse().unwrap();
        assert_eq!(
            response["result"]["structuredContent"]["email"],
            "ana@example.com"
        );

        // The mint is in the console audit trail under the same action name.
        let (actions,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM account_action WHERE tool = 'create_api_key'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(actions, 1);
    }

    #[tokio::test]
    async fn post_refuses_cross_origin_and_mints_nothing() {
        let (router, pool, config, codes) = app().await;
        let cookie = signed_in_cookie(&pool, &config, &codes).await;
        let count = || async {
            let (keys,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM apikey")
                .fetch_one(&pool)
                .await
                .unwrap();
            keys
        };
        let before = count().await;
        let (status, _, _) = send(&router, post_key("evil", Some(&cookie), false)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(count().await, before, "a cross-origin POST mints nothing");
    }

    #[tokio::test]
    async fn post_rejects_a_name_over_100_chars() {
        let (router, pool, config, codes) = app().await;
        let cookie = signed_in_cookie(&pool, &config, &codes).await;
        let long = "x".repeat(101);
        let count = || async {
            let (keys,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM apikey")
                .fetch_one(&pool)
                .await
                .unwrap();
            keys
        };
        let before = count().await;
        let (status, _, body) = send(&router, post_key(&long, Some(&cookie), true)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("under 100 characters"), "got: {body}");
        assert_eq!(count().await, before, "a rejected name mints nothing");
    }

    #[tokio::test]
    async fn post_keeps_a_100_char_multibyte_name_whole() {
        let (router, pool, config, codes) = app().await;
        let cookie = signed_in_cookie(&pool, &config, &codes).await;
        // 100 characters, 300 bytes: within the limit, and a byte-index
        // truncation would split a character.
        let name = "あ".repeat(100);
        let (status, _, _) = send(&router, post_key(&name, Some(&cookie), true)).await;
        assert_eq!(status, StatusCode::OK);
        // The sign-in helper mints a key of its own in the same millisecond,
        // so look the row up by name rather than by creation order.
        let (stored,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM apikey WHERE name = ?")
            .bind(&name)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 1, "the whole 100-character name is stored");
    }
}
