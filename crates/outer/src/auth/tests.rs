//! End-to-end auth tests: discovery, the full authorize flow, token
//! rotation, DCR, CIMD policy, OTP limits, API keys, sessions and CSRF.
//! Deterministic: codes are captured in memory, expiry is set with SQL, and
//! the only network is loopback to fake CIMD/email servers.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use crate::auth::test_support::{captured, test_config, test_pool};
use crate::auth::{self, tokens, AuthState};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use tower::ServiceExt;

const PEER: &str = "127.0.0.1:43123";
const VERIFIER: &str = "test-verifier-0123456789abcdefghij";
const SCOPE: &str = "apps offline_access";
const RESOURCE: &str = "http://outer.localhost:8080/mcp";
const ISSUER: &str = "http://outer.localhost:8080";

fn peer(mut req: Request<Body>) -> Request<Body> {
    req.extensions_mut()
        .insert(ConnectInfo(PEER.parse::<SocketAddr>().unwrap()));
    req
}

fn form(params: &[(&str, &str)]) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in params {
        out.append_pair(key, value);
    }
    out.finish()
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(peer(req)).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

fn get_authed(path: &str, session: &str) -> Request<Body> {
    Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap()
}

fn post_form(path: &str, body: &str) -> Request<Body> {
    Request::post(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn post_json(path: &str, body: &serde_json::Value) -> Request<Body> {
    Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// `name="X" value="..."` out of a maud form.
fn hidden(body: &str, name: &str) -> String {
    let needle = format!(r#"name="{name}" value=""#);
    let start = body
        .find(&needle)
        .unwrap_or_else(|| panic!("{name} input missing"));
    let rest = &body[start + needle.len()..];
    rest[..rest.find('"').unwrap()].to_string()
}

fn set_session(headers: &HeaderMap) -> String {
    let value = headers
        .get_all("set-cookie")
        .iter()
        .find_map(|value| {
            let value = value.to_str().ok()?;
            value
                .starts_with("__Host-outer_sid=")
                .then(|| value.to_string())
        })
        .expect("session cookie set");
    value.split(';').next().unwrap().to_string()
}

fn raw_set_session(headers: &HeaderMap) -> String {
    headers
        .get_all("set-cookie")
        .iter()
        .find_map(|value| {
            let value = value.to_str().ok()?;
            value
                .starts_with("__Host-outer_sid=")
                .then(|| value.to_string())
        })
        .expect("session cookie set")
}

/// The client URL the Allow/Deny page navigates to (its `<meta refresh>`).
fn return_query(body: &str) -> HashMap<String, String> {
    let start = body.find("0;url=").expect("refresh target") + "0;url=".len();
    let end = start + body[start..].find('"').expect("closing quote");
    let target = body[start..end].replace("&amp;", "&");
    url::Url::parse(&target)
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn app_with(state: &AuthState) -> Router {
    auth::router(state.clone())
}

#[tokio::test]
async fn discovery_documents_match_the_spec() {
    let pool = test_pool().await;
    let (config, _) = test_config();
    let state = AuthState { pool, config };
    let app = app_with(&state);

    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let (status, _, body) = send(&app, get(path)).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let doc: serde_json::Value = body.parse().unwrap();
        assert_eq!(
            doc,
            serde_json::json!({
                "resource": format!("{ISSUER}/mcp"),
                "authorization_servers": [ISSUER],
                "scopes_supported": ["apps"],
                "bearer_methods_supported": ["header"],
            })
        );
    }

    let (status, headers, body) = send(&app, get("/.well-known/oauth-authorization-server")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    let doc: serde_json::Value = body.parse().unwrap();
    assert_eq!(
        doc,
        serde_json::json!({
            "issuer": ISSUER,
            "authorization_endpoint": format!("{ISSUER}/oauth/authorize"),
            "token_endpoint": format!("{ISSUER}/oauth/token"),
            "registration_endpoint": format!("{ISSUER}/oauth/register"),
            "revocation_endpoint": format!("{ISSUER}/oauth/revoke"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "client_id_metadata_document_supported": true,
            "authorization_response_iss_parameter_supported": true,
            "scopes_supported": ["apps", "offline_access"],
        })
    );
}

#[tokio::test]
async fn www_authenticate_has_the_exact_lazy_auth_shape() {
    let (config, _) = test_config();
    let value = auth::www_authenticate(&config);
    assert_eq!(
        value.to_str().unwrap(),
        format!(
            "Bearer error=\"invalid_token\", error_description=\"Sign in to Outer to use this tool\", \
             resource_metadata=\"{ISSUER}/.well-known/oauth-protected-resource/mcp\", scope=\"apps\""
        )
    );
}

struct Flow {
    app: Router,
    state: AuthState,
    codes: Arc<Mutex<HashMap<String, String>>>,
}

async fn flow() -> Flow {
    let pool = test_pool().await;
    let (config, codes) = test_config();
    let state = AuthState { pool, config };
    let app = app_with(&state);
    Flow { app, state, codes }
}

/// Register a DCR client for flow tests.
async fn dcr_client(app: &Router, redirect: &str) -> String {
    let (status, _, body) = send(
        app,
        post_json(
            "/oauth/register",
            &serde_json::json!({ "redirect_uris": [redirect], "client_name": "Flow App" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body.parse::<serde_json::Value>().unwrap()["client_id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Authorize URL for a client; PKCE challenge included, state fixed.
fn authorize_url(client_id: &str, redirect_uri: &str) -> String {
    let query = form(&[
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_challenge", &tokens::pkce_s256(VERIFIER)),
        ("code_challenge_method", "S256"),
        ("scope", SCOPE),
        ("resource", RESOURCE),
        ("state", "xyz-state"),
    ]);
    format!("/oauth/authorize?{query}")
}

/// Run the email → code → terms steps; return request id, csrf and session.
async fn sign_in(
    flow: &Flow,
    client_id: &str,
    redirect_uri: &str,
    email: &str,
) -> (String, String, String) {
    let (status, _, body) = send(&flow.app, get(&authorize_url(client_id, redirect_uri))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Send code"));
    let request_id = hidden(&body, "request_id");
    let csrf = hidden(&body, "csrf");

    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/authorize",
            &form(&[
                ("request_id", &request_id),
                ("csrf", &csrf),
                ("step", "email"),
                ("email", email),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("6-digit code"));
    let code = captured(&flow.codes, &email.to_lowercase());

    let (status, headers, body) = send(
        &flow.app,
        post_form(
            "/oauth/authorize",
            &form(&[
                ("request_id", &request_id),
                ("csrf", &csrf),
                ("step", "code"),
                ("email", email),
                ("code", &code),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Terms"), "first sign-in owes terms: {body}");
    let session = set_session(&headers);

    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/authorize",
            &form(&[
                ("request_id", &request_id),
                ("csrf", &csrf),
                ("step", "terms"),
                ("accept", "yes"),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (request_id, csrf, session)
}

/// Consent Allow → the redirect carries code, state and iss.
async fn allow(
    flow: &Flow,
    request_id: &str,
    csrf: &str,
    session: &str,
) -> HashMap<String, String> {
    let mut req = post_form(
        "/oauth/authorize",
        &form(&[
            ("request_id", request_id),
            ("csrf", csrf),
            ("step", "decision"),
            ("allow", "yes"),
        ]),
    );
    req.headers_mut().insert("cookie", session.parse().unwrap());
    let (status, _, body) = send(&flow.app, req).await;
    assert_eq!(status, StatusCode::OK);
    let params = return_query(&body);
    assert_eq!(params.get("state").map(String::as_str), Some("xyz-state"));
    assert_eq!(params.get("iss").map(String::as_str), Some(ISSUER));
    assert!(params.contains_key("code"));
    params
}

async fn exchange(
    flow: &Flow,
    client_id: &str,
    params: &HashMap<String, String>,
    redirect_uri: &str,
) -> serde_json::Value {
    let (status, headers, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "authorization_code"),
                ("client_id", client_id),
                ("code", &params["code"]),
                ("redirect_uri", redirect_uri),
                ("code_verifier", VERIFIER),
                ("resource", RESOURCE),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    body.parse().unwrap()
}

#[tokio::test]
async fn full_authorize_flow_mints_a_working_token() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "ana@example.com").await;

    // DCR clients show the unverified label plus no loopback warning here.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/authorize",
            &form(&[
                ("request_id", "x"),
                ("csrf", "y"),
                ("step", "decision"),
                ("allow", "yes"),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("expired") || body.contains("Start over"));

    let params = allow(&flow, &request_id, &csrf, &session).await;
    let tokens: serde_json::Value = exchange(&flow, &client_id, &params, redirect_uri).await;
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["scope"], SCOPE);
    assert!(tokens["refresh_token"]
        .as_str()
        .is_some_and(|token| !token.is_empty()));

    let principal = auth::resolve_bearer(
        &flow.state.pool,
        &flow.state.config,
        tokens["access_token"].as_str().unwrap(),
    )
    .await
    .unwrap()
    .expect("access token resolves");
    assert_eq!(principal.email, "ana@example.com");
    assert!(!principal.is_admin);
    assert!(matches!(
        principal.credential,
        auth::Credential::OAuth { .. }
    ));

    // Refresh rotates: a new pair, same grant family.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rotated: serde_json::Value = body.parse().unwrap();
    assert_ne!(rotated["access_token"], tokens["access_token"]);

    // The rotated-in refresh is dead: reusing it revokes the whole grant.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"));
    for token in [
        tokens["access_token"].as_str().unwrap(),
        rotated["access_token"].as_str().unwrap(),
    ] {
        let resolved = auth::resolve_bearer(&flow.state.pool, &flow.state.config, token)
            .await
            .unwrap();
        assert!(resolved.is_none(), "revoked grant stops resolving");
    }

    // Deny redirects with error=access_denied, state and iss.
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "bob@example.com").await;
    let mut req = post_form(
        "/oauth/authorize",
        &form(&[
            ("request_id", &request_id),
            ("csrf", &csrf),
            ("step", "decision"),
            ("allow", "no"),
        ]),
    );
    req.headers_mut().insert("cookie", session.parse().unwrap());
    let (status, _, body) = send(&flow.app, req).await;
    assert_eq!(status, StatusCode::OK);
    let params = return_query(&body);
    assert_eq!(
        params.get("error").map(String::as_str),
        Some("access_denied")
    );
    assert_eq!(params.get("state").map(String::as_str), Some("xyz-state"));
    assert_eq!(params.get("iss").map(String::as_str), Some(ISSUER));
}

/// Concurrent redemption of one code mints at most one grant, and two
/// concurrent uses of one refresh token never both succeed: the loser is
/// reuse and revokes the grant (theft detection survives the race).
#[tokio::test]
async fn concurrent_code_and_refresh_reuse_cannot_double_spend() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "ana@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;
    let redeem = || {
        send(
            &flow.app,
            post_form(
                "/oauth/token",
                &form(&[
                    ("grant_type", "authorization_code"),
                    ("client_id", &client_id),
                    ("code", &params["code"]),
                    ("redirect_uri", redirect_uri),
                    ("code_verifier", VERIFIER),
                ]),
            ),
        )
    };
    let (a, b) = tokio::join!(redeem(), redeem());
    let ok: Vec<_> = [&a, &b]
        .into_iter()
        .filter(|r| r.0 == StatusCode::OK)
        .collect();
    assert_eq!(ok.len(), 1, "exactly one redemption wins: {a:?} {b:?}");
    let tokens: serde_json::Value = ok[0].2.parse().unwrap();

    let refresh = tokens["refresh_token"].as_str().unwrap().to_string();
    let use_refresh = || {
        send(
            &flow.app,
            post_form(
                "/oauth/token",
                &form(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", &client_id),
                    ("refresh_token", &refresh),
                ]),
            ),
        )
    };
    let (a, b) = tokio::join!(use_refresh(), use_refresh());
    let wins = [&a, &b]
        .into_iter()
        .filter(|r| r.0 == StatusCode::OK)
        .count();
    assert!(wins <= 1, "a refresh token rotates once: {a:?} {b:?}");
    if wins == 1 {
        // The loser was reuse: the whole grant is gone, the winner's
        // fresh access token included.
        let winner = if a.0 == StatusCode::OK { &a } else { &b };
        let rotated: serde_json::Value = winner.2.parse().unwrap();
        let resolved = auth::resolve_bearer(
            &flow.state.pool,
            &flow.state.config,
            rotated["access_token"].as_str().unwrap(),
        )
        .await
        .unwrap();
        assert!(resolved.is_none(), "reuse revoked the grant");
    }
}

#[tokio::test]
async fn authorize_rejects_bad_params_without_redirecting() {
    let flow = flow().await;
    let client_id = dcr_client(&flow.app, "https://app.example/cb").await;

    // Wrong response_type.
    let (status, _, _) = send(&flow.app, get("/oauth/authorize?response_type=token")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Non-S256 method.
    let (status, _, body) = send(
        &flow.app,
        get(&format!(
            "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri=https://app.example/cb&code_challenge=x&code_challenge_method=plain"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("S256"));
    // Unknown client.
    let (status, _, _) = send(
        &flow.app,
        get("/oauth/authorize?response_type=code&client_id=https%3A%2F%2Funknown.example%2Fdoc&redirect_uri=https://app.example/cb&code_challenge=x&code_challenge_method=S256"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Redirect URI the client never registered.
    let query = form(&[
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", "https://evil.example/cb"),
        ("code_challenge", "x"),
        ("code_challenge_method", "S256"),
    ]);
    let (status, _, body) = send(&flow.app, get(&format!("/oauth/authorize?{query}"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("redirect_uri"));
    // Scope outside the fixed set.
    let query = form(&[
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", "https://app.example/cb"),
        ("code_challenge", "x"),
        ("code_challenge_method", "S256"),
        ("scope", "apps admin"),
    ]);
    let (status, _, _) = send(&flow.app, get(&format!("/oauth/authorize?{query}"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn codes_are_single_use_expiring_and_pkce_bound() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "ana@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;

    // Wrong PKCE verifier.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", &params["code"]),
                ("redirect_uri", redirect_uri),
                ("code_verifier", "wrong-verifier"),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"));

    // Wrong redirect_uri.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", &params["code"]),
                ("redirect_uri", "https://evil.example/cb"),
                ("code_verifier", VERIFIER),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The code above is still unused (failures don't consume... but PKCE and
    // redirect failures DO consume: single use deletes the row first). The
    // first exchange attempt deleted it, so a fresh flow is needed for the
    // success path.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", &params["code"]),
                ("redirect_uri", redirect_uri),
                ("code_verifier", VERIFIER),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"), "single use: {body}");

    // Expired codes die too.
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "bob@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;
    sqlx::query("UPDATE oauth_request SET expires_at = 0 WHERE code_hash = ?")
        .bind(auth::tokens::sha256_hex(&params["code"]))
        .execute(&flow.state.pool)
        .await
        .unwrap();
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", &params["code"]),
                ("redirect_uri", redirect_uri),
                ("code_verifier", VERIFIER),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"));
}

#[tokio::test]
async fn token_endpoint_binds_client_id() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_a = dcr_client(&flow.app, redirect_uri).await;
    let client_b = dcr_client(&flow.app, redirect_uri).await;
    let (request_id, csrf, session) =
        sign_in(&flow, &client_a, redirect_uri, "ana@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;
    let code = params["code"].clone();

    // Missing client_id fails closed as invalid_client and does NOT consume
    // the code: the id check runs before the row is even loaded.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", VERIFIER),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_client"), "{body}");

    // Another client's id fails as invalid_grant and consumes the code:
    // single use survives the theft attempt.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_b),
                ("code", &code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", VERIFIER),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"), "{body}");

    // The right client now finds the code already spent.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_a),
                ("code", &code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", VERIFIER),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"), "{body}");

    // Refresh binding: a live grant for the mismatch probes.
    let (request_id, csrf, session) =
        sign_in(&flow, &client_a, redirect_uri, "bob@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;
    let tokens: serde_json::Value = exchange(&flow, &client_a, &params, redirect_uri).await;
    let refresh = tokens["refresh_token"].as_str().unwrap().to_string();

    // Missing client_id: invalid_client, grant untouched.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[("grant_type", "refresh_token"), ("refresh_token", &refresh)]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_client"), "{body}");

    // Wrong client_id: invalid_grant, and the grant is NOT revoked — a
    // mismatched id never counts as reuse.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_b),
                ("refresh_token", &refresh),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"), "{body}");

    // The grant survives both probes: the real client rotates fine.
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_a),
                ("refresh_token", &refresh),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Two concurrent refreshes of one token: at most one succeeds, and once
/// the consent is revoked nothing from the family authenticates — not even
/// a token planted directly on the revoked row (`resolve_bearer` joins
/// `revokedAt IS NULL`, the second barrier behind the rotation
/// transaction).
#[tokio::test]
async fn refresh_race_and_revocation_leave_no_survivors() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "ana@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;
    let tokens: serde_json::Value = exchange(&flow, &client_id, &params, redirect_uri).await;
    let first_access = tokens["access_token"].as_str().unwrap().to_string();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_string();
    let principal = auth::resolve_bearer(&flow.state.pool, &flow.state.config, &first_access)
        .await
        .unwrap()
        .expect("fresh access resolves");
    let grant_id = match &principal.credential {
        auth::Credential::OAuth { grant_id } => grant_id.clone(),
        _ => panic!("expected an OAuth grant"),
    };

    // Race the same refresh twice: at most one rotation commits.
    let use_refresh = || {
        send(
            &flow.app,
            post_form(
                "/oauth/token",
                &form(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", &client_id),
                    ("refresh_token", &refresh),
                ]),
            ),
        )
    };
    let (a, b) = tokio::join!(use_refresh(), use_refresh());
    let winners: Vec<serde_json::Value> = [&a, &b]
        .into_iter()
        .filter(|r| r.0 == StatusCode::OK)
        .map(|r| r.2.parse().unwrap())
        .collect();
    assert!(
        winners.len() <= 1,
        "a refresh token rotates once: {a:?} {b:?}"
    );
    let mut family = vec![first_access.clone()];
    for won in &winners {
        family.push(won["access_token"].as_str().unwrap().to_string());
    }

    // Explicit revocation wipes the family: no survivor authenticates, the
    // refresh is dead, and the token rows are gone. (When the race had a
    // winner, the loser already revoked the grant — `revoke_connection`
    // then reports false, which is fine: the end state is identical.)
    let _ = auth::revoke_connection(&flow.state.pool, &principal.account_id, &grant_id).await;
    for token in &family {
        assert!(
            auth::resolve_bearer(&flow.state.pool, &flow.state.config, token)
                .await
                .unwrap()
                .is_none(),
            "revoked consent stops resolving"
        );
    }
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("refresh_token", &refresh),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"), "{body}");
    let (left,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM oauthAccessToken WHERE referenceId = ?")
            .bind(&grant_id)
            .fetch_one(&flow.state.pool)
            .await
            .unwrap();
    assert_eq!(left, 0, "revocation deletes the family's tokens");

    // Even a token minted behind the revocation's back (the rotation-race
    // shape: committed onto a revoked consent) cannot authenticate: the
    // `revokedAt IS NULL` join blocks it.
    let (planted, planted_hash) = auth::tokens::new_access_token();
    sqlx::query(
        "INSERT INTO oauthAccessToken (id, token, clientId, userId, referenceId, scopes, expiresAt, createdAt) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(auth::tokens::new_id())
    .bind(&planted_hash)
    .bind(&client_id)
    .bind(&principal.account_id)
    .bind(&grant_id)
    .bind("apps offline_access")
    .bind(auth::now_ms() + 3_600_000)
    .bind(auth::now_ms())
    .execute(&flow.state.pool)
    .await
    .unwrap();
    assert!(
        auth::resolve_bearer(&flow.state.pool, &flow.state.config, &planted)
            .await
            .unwrap()
            .is_none(),
        "token on a revoked consent does not resolve"
    );
}
/// Banned users fail closed at both grant types; an expired temporary ban
/// does not. The access token stops resolving while banned, and deleting
/// the user kills the grant (the join finds no row: `invalid_grant`).
#[tokio::test]
async fn banned_users_cannot_redeem_or_refresh() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    // A live grant for the refresh probes, minted before the ban.
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "bob@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;
    let tokens: serde_json::Value = exchange(&flow, &client_id, &params, redirect_uri).await;
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let mut refresh = tokens["refresh_token"].as_str().unwrap().to_string();
    fn refresh_post(client_id: &str, refresh: &str) -> Request<Body> {
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("refresh_token", refresh),
            ]),
        )
    }

    sqlx::query("UPDATE user SET banned = 1 WHERE email = 'bob@example.com'")
        .execute(&flow.state.pool)
        .await
        .unwrap();
    let (status, _, body) = send(&flow.app, refresh_post(&client_id, &refresh)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"), "{body}");
    assert!(
        auth::resolve_bearer(&flow.state.pool, &flow.state.config, &access)
            .await
            .unwrap()
            .is_none(),
        "banned user's access token stops resolving"
    );

    // An expired temporary ban no longer counts: rotation works again.
    sqlx::query("UPDATE user SET banExpires = 0 WHERE email = 'bob@example.com'")
        .execute(&flow.state.pool)
        .await
        .unwrap();
    let (status, _, body) = send(&flow.app, refresh_post(&client_id, &refresh)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    refresh = body.parse::<serde_json::Value>().unwrap()["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // An active temporary ban fails again.
    sqlx::query("UPDATE user SET banExpires = 9999999999999 WHERE email = 'bob@example.com'")
        .execute(&flow.state.pool)
        .await
        .unwrap();
    let (status, _, body) = send(&flow.app, refresh_post(&client_id, &refresh)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"), "{body}");

    // A deleted user is an unknown grant: the join finds no row.
    sqlx::query("DELETE FROM user WHERE email = 'bob@example.com'")
        .execute(&flow.state.pool)
        .await
        .unwrap();
    let (status, _, body) = send(&flow.app, refresh_post(&client_id, &refresh)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"), "{body}");

    // Code redemption fails closed for banned users too.
    // Fresh code for ana, banned before redeeming:
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "ana@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;
    sqlx::query("UPDATE user SET banned = 1, banExpires = NULL WHERE email = 'ana@example.com'")
        .execute(&flow.state.pool)
        .await
        .unwrap();
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/token",
            &form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", &params["code"]),
                ("redirect_uri", redirect_uri),
                ("code_verifier", VERIFIER),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_grant"), "{body}");
}

/// `apikey.expiresAt` is enforced and the token `revoked` flag is honored:
/// expired keys and revoked-flagged access tokens do not resolve, live ones
/// do.
#[tokio::test]
async fn bearer_rejects_expired_keys_and_revoked_tokens() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "ana@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;
    let tokens: serde_json::Value = exchange(&flow, &client_id, &params, redirect_uri).await;
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let principal = auth::resolve_bearer(&flow.state.pool, &flow.state.config, &access)
        .await
        .unwrap()
        .expect("fresh access resolves");
    let access_hash = auth::tokens::sha256_hex(&access);

    // A set `revoked` flag blocks the access token; clearing it restores it.
    sqlx::query("UPDATE oauthAccessToken SET revoked = 1 WHERE token = ?")
        .bind(&access_hash)
        .execute(&flow.state.pool)
        .await
        .unwrap();
    assert!(
        auth::resolve_bearer(&flow.state.pool, &flow.state.config, &access)
            .await
            .unwrap()
            .is_none(),
        "revoked-flagged access token does not resolve"
    );
    sqlx::query("UPDATE oauthAccessToken SET revoked = 0 WHERE token = ?")
        .bind(&access_hash)
        .execute(&flow.state.pool)
        .await
        .unwrap();
    assert!(
        auth::resolve_bearer(&flow.state.pool, &flow.state.config, &access)
            .await
            .unwrap()
            .is_some(),
        "unflagged access token resolves again"
    );

    // API keys: no expiry resolves, a past expiry does not, a future one
    // does.
    let created = auth::create_api_key(&flow.state.pool, &principal.account_id, None)
        .await
        .unwrap();
    let key_hash = auth::tokens::sha256_hex(&created.key);
    assert!(
        auth::resolve_bearer(&flow.state.pool, &flow.state.config, &created.key)
            .await
            .unwrap()
            .is_some(),
        "unexpiring key resolves"
    );
    sqlx::query("UPDATE apikey SET expiresAt = 0 WHERE key = ?")
        .bind(&key_hash)
        .execute(&flow.state.pool)
        .await
        .unwrap();
    assert!(
        auth::resolve_bearer(&flow.state.pool, &flow.state.config, &created.key)
            .await
            .unwrap()
            .is_none(),
        "expired key does not resolve"
    );
    sqlx::query("UPDATE apikey SET expiresAt = 9999999999999 WHERE key = ?")
        .bind(&key_hash)
        .execute(&flow.state.pool)
        .await
        .unwrap();
    assert!(
        auth::resolve_bearer(&flow.state.pool, &flow.state.config, &created.key)
            .await
            .unwrap()
            .is_some(),
        "unexpired key resolves"
    );
}

/// The token request cannot change the authorize request's resource (RFC
/// 8707): omitting it binds the stored value, sending anything else —
/// including the server default over a non-default stored value — fails
/// with `invalid_target`.
#[tokio::test]
async fn token_resource_must_match_authorize_request() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    // Four distinct users up front: account creation is limited to 3/day
    // per IP, and the OTP flow enforces it.
    for n in 1..=4u32 {
        sqlx::query(
            "INSERT INTO user (id, email, emailVerified, lastActiveAt, createdAt, updatedAt) \
             VALUES (?, ?, 1, 0, 0, 0)",
        )
        .bind(format!("user-{n}"))
        .bind(format!("ana{n}@example.com"))
        .execute(&flow.state.pool)
        .await
        .unwrap();
    }
    async fn mint_code(flow: &Flow, client_id: &str, redirect_uri: &str, n: u32) -> String {
        let email = format!("ana{n}@example.com");
        let (request_id, csrf, session) = sign_in(flow, client_id, redirect_uri, &email).await;
        allow(flow, &request_id, &csrf, &session).await["code"].clone()
    }
    let redeem = |code: &str, resource: Option<&str>| {
        let mut pairs = vec![
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", VERIFIER),
        ];
        if let Some(resource) = resource {
            pairs.push(("resource", resource));
        }
        send(&flow.app, post_form("/oauth/token", &form(&pairs)))
    };

    // A resource the authorize request never saw is rejected.
    let code = mint_code(&flow, &client_id, redirect_uri, 1).await;
    let (status, _, body) = redeem(&code, Some("https://other.example/mcp")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_target"), "{body}");

    // Omitting it binds the stored value and succeeds.
    let code = mint_code(&flow, &client_id, redirect_uri, 2).await;
    let (status, _, body) = redeem(&code, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Stored non-default + server default sent: a downgrade, rejected. (The
    // authorize page only issues the default, so plant the foreign shape
    // directly.)
    const FOREIGN: &str = "https://foreign.example/mcp";
    let code = mint_code(&flow, &client_id, redirect_uri, 3).await;
    sqlx::query("UPDATE oauth_request SET resource = ? WHERE code_hash = ?")
        .bind(FOREIGN)
        .bind(auth::tokens::sha256_hex(&code))
        .execute(&flow.state.pool)
        .await
        .unwrap();
    let (status, _, body) = redeem(&code, Some(RESOURCE)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invalid_target"), "{body}");

    // The matching stored value succeeds and the grant carries it.
    let code = mint_code(&flow, &client_id, redirect_uri, 4).await;
    sqlx::query("UPDATE oauth_request SET resource = ? WHERE code_hash = ?")
        .bind(FOREIGN)
        .bind(auth::tokens::sha256_hex(&code))
        .execute(&flow.state.pool)
        .await
        .unwrap();
    let (status, _, body) = redeem(&code, Some(FOREIGN)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let issued: serde_json::Value = body.parse().unwrap();
    let principal = auth::resolve_bearer(
        &flow.state.pool,
        &flow.state.config,
        issued["access_token"].as_str().unwrap(),
    )
    .await
    .unwrap()
    .expect("bound token resolves");
    let grant_id = match &principal.credential {
        auth::Credential::OAuth { grant_id } => grant_id.clone(),
        _ => panic!("expected an OAuth grant"),
    };
    let (resources,): (Option<String>,) =
        sqlx::query_as("SELECT resources FROM oauthConsent WHERE id = ?")
            .bind(&grant_id)
            .fetch_one(&flow.state.pool)
            .await
            .unwrap();
    assert_eq!(resources.as_deref(), Some(FOREIGN));
}

#[tokio::test]
async fn dcr_validates_and_rate_limits() {
    let flow = flow().await;
    // Plain http to a remote host, script URLs and garbage are refused.
    for redirect in ["http://app.example/cb", "javascript:alert(1)", "not-a-url"] {
        let (status, _, body) = send(
            &flow.app,
            post_json(
                "/oauth/register",
                &serde_json::json!({ "redirect_uris": [redirect] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{redirect}: {body}");
    }
    // Desktop apps register their own scheme (RFC 8252), e.g. Cursor.
    let (status, _, body) = send(
        &flow.app,
        post_json(
            "/oauth/register",
            &serde_json::json!({ "redirect_uris": ["cursor://anysphere.cursor-retrieval/oauth/user-outer/callback"] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // A client defaulting to a secret method is registered public, not
    // refused: the answer tells it so.
    let (status, _, body) = send(
        &flow.app,
        post_json(
            "/oauth/register",
            &serde_json::json!({ "redirect_uris": ["https://app.example/cb"], "token_endpoint_auth_method": "client_secret_basic" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let registered: serde_json::Value = body.parse().unwrap();
    assert_eq!(registered["token_endpoint_auth_method"], "none");

    // Loopback http is fine.
    let (status, _, _) = send(
        &flow.app,
        post_json(
            "/oauth/register",
            &serde_json::json!({ "redirect_uris": ["http://localhost:43123/cb"] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // 20 registrations per hour per IP: the failed validations above count
    // too (6 events so far), so 13 more succeed, the 20th succeeds, the 21st
    // fails.
    for _ in 0..13 {
        let (status, _, _) = send(
            &flow.app,
            post_json(
                "/oauth/register",
                &serde_json::json!({ "redirect_uris": ["https://app.example/cb"] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, _, _) = send(
        &flow.app,
        post_json(
            "/oauth/register",
            &serde_json::json!({ "redirect_uris": ["https://app.example/cb"] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, headers, body) = send(
        &flow.app,
        post_json(
            "/oauth/register",
            &serde_json::json!({ "redirect_uris": ["https://app.example/cb"] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(headers.contains_key("retry-after"));
}
#[tokio::test]
async fn otp_attempts_expiry_and_limits() {
    let pool = test_pool().await;
    let (config, codes) = test_config();
    let state = AuthState {
        pool: pool.clone(),
        config,
    };

    // Five wrong codes burn the row.
    auth::otp_sign_in(&pool, &state.config, "ana@example.com", None)
        .await
        .unwrap();
    for _ in 0..5 {
        let err = auth::otp_verify(
            &pool,
            &state.config,
            "ana@example.com",
            "000000",
            false,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.code == "invalid_code" || err.code == "too_many_attempts",
            "{}",
            err.code
        );
    }
    let err = auth::otp_verify(
        &pool,
        &state.config,
        "ana@example.com",
        "000000",
        false,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "invalid_code", "burned row reads as unknown");

    // Expired codes are rejected and cleared.
    auth::otp_sign_in(&pool, &state.config, "ana@example.com", None)
        .await
        .unwrap();
    let code = captured(&codes, "ana@example.com");
    sqlx::query("UPDATE verification SET expiresAt = 0 WHERE identifier = 'email-verification:ana@example.com'")
        .execute(&pool)
        .await
        .unwrap();
    let err = auth::otp_verify(
        &pool,
        &state.config,
        "ana@example.com",
        &code,
        true,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "code_expired");

    // Five sends per hour per email.
    for _ in 0..4 {
        auth::otp_sign_in(&pool, &state.config, "limited@example.com", None)
            .await
            .unwrap();
    }
    auth::otp_sign_in(&pool, &state.config, "limited@example.com", None)
        .await
        .unwrap();
    let err = auth::otp_sign_in(&pool, &state.config, "limited@example.com", None)
        .await
        .unwrap_err();
    assert_eq!(err.code, "rate_limited");
    assert!(err.retry_after_s.is_some_and(|retry| retry > 0));

    // Twenty sends per hour per IP.
    for index in 0..20 {
        auth::otp_sign_in(
            &pool,
            &state.config,
            &format!("user{index}@example.com"),
            Some("10.0.0.1"),
        )
        .await
        .unwrap();
    }
    let err = auth::otp_sign_in(
        &pool,
        &state.config,
        "one-more@example.com",
        Some("10.0.0.1"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "rate_limited");
}

#[tokio::test]
async fn closed_beta_waitlists_by_default() {
    let pool = test_pool().await;
    let (mut config, _) = test_config();
    config.signup_open = false;
    let state = AuthState {
        pool: pool.clone(),
        config,
    };

    let err = auth::otp_sign_in(&pool, &state.config, "new@example.com", None)
        .await
        .unwrap_err();
    assert_eq!(err.code, "closed_beta");
    assert_eq!(
        err.user_message.as_deref(),
        Some("Outer is in closed beta. You're on the waitlist.")
    );
    let (attempts,): (i64,) =
        sqlx::query_as("SELECT attempts FROM waitlist WHERE email = 'new@example.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
    let _ = auth::otp_sign_in(&pool, &state.config, "new@example.com", None)
        .await
        .unwrap_err();
    let (attempts,): (i64,) =
        sqlx::query_as("SELECT attempts FROM waitlist WHERE email = 'new@example.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 2, "repeat sends count the waitlist");

    // Allowlisted emails pass; admin emails always pass.
    sqlx::query("INSERT INTO beta_allowlist (email, added_at) VALUES ('friend@example.com', 0)")
        .execute(&pool)
        .await
        .unwrap();
    auth::otp_sign_in(&pool, &state.config, "friend@example.com", None)
        .await
        .unwrap();
    auth::otp_sign_in(&pool, &state.config, "owner@example.com", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn api_keys_create_list_revoke_and_resolve() {
    let pool = test_pool().await;
    let (config, codes) = test_config();
    let state = AuthState {
        pool: pool.clone(),
        config,
    };

    auth::otp_sign_in(&pool, &state.config, "ana@example.com", None)
        .await
        .unwrap();
    let code = captured(&codes, "ana@example.com");
    let verified = auth::otp_verify(
        &pool,
        &state.config,
        "ana@example.com",
        &code,
        true,
        Some("laptop"),
        None,
    )
    .await
    .unwrap();
    assert!(verified.api_key.starts_with("outer_"));

    // Terms are stored on first verify.
    let (terms,): (Option<String>,) = sqlx::query_as("SELECT termsVersion FROM user WHERE id = ?")
        .bind(&verified.account_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(terms.as_deref(), Some("2026-10-23"));

    let principal = auth::resolve_bearer(&pool, &state.config, &verified.api_key)
        .await
        .unwrap()
        .expect("fresh key resolves");
    assert_eq!(principal.account_id, verified.account_id);
    assert!(matches!(
        principal.credential,
        auth::Credential::ApiKey { .. }
    ));

    let keys = auth::list_api_keys(&pool, &verified.account_id)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].name.as_deref(), Some("laptop"));
    assert_eq!(keys[0].prefix.len(), 10);
    assert!(verified.api_key.contains(&keys[0].prefix));

    // Unknown and malformed tokens resolve to nothing.
    assert!(auth::resolve_bearer(&pool, &state.config, "outer_deadbeef")
        .await
        .unwrap()
        .is_none());
    assert!(
        auth::resolve_bearer(&pool, &state.config, "Bearer outer_deadbeef")
            .await
            .unwrap()
            .is_none()
    );
    assert!(auth::resolve_bearer(&pool, &state.config, "")
        .await
        .unwrap()
        .is_none());

    assert!(
        auth::revoke_api_key(&pool, &verified.account_id, &keys[0].prefix)
            .await
            .unwrap()
    );
    assert!(
        !auth::revoke_api_key(&pool, &verified.account_id, &keys[0].prefix)
            .await
            .unwrap()
    );
    assert!(
        auth::resolve_bearer(&pool, &state.config, &verified.api_key)
            .await
            .unwrap()
            .is_none(),
        "revoked keys stop resolving"
    );
}

#[tokio::test]
async fn returning_session_skips_email_and_code() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    let (_, _, session) = sign_in(&flow, &client_id, redirect_uri, "ana@example.com").await;

    let (status, _, body) = send(
        &flow.app,
        get_authed(&authorize_url(&client_id, redirect_uri), &session),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Allow"), "session skips to consent: {body}");
    assert!(!body.contains("Send code"));
}

#[tokio::test]
async fn csrf_mismatch_is_forbidden() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    let (request_id, _, _) = sign_in(&flow, &client_id, redirect_uri, "ana@example.com").await;
    let (status, _, body) = send(
        &flow.app,
        post_form(
            "/oauth/authorize",
            &form(&[
                ("request_id", &request_id),
                ("csrf", "wrong"),
                ("step", "decision"),
                ("allow", "yes"),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn session_cookie_uses_the_host_prefix() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    let (status, _, body) = send(&flow.app, get(&authorize_url(&client_id, redirect_uri))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _headers, _) = send(
        &flow.app,
        post_form(
            "/oauth/authorize",
            &form(&[
                ("request_id", &hidden(&body, "request_id")),
                ("csrf", &hidden(&body, "csrf")),
                ("step", "email"),
                ("email", "ana@example.com"),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let code = captured(&flow.codes, "ana@example.com");
    let (status, headers, _) = send(
        &flow.app,
        post_form(
            "/oauth/authorize",
            &form(&[
                ("request_id", &hidden(&body, "request_id")),
                ("csrf", &hidden(&body, "csrf")),
                ("step", "code"),
                ("email", "ana@example.com"),
                ("code", &code),
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let raw = raw_set_session(&headers);
    for part in [
        "__Host-outer_sid=",
        "Path=/",
        "Max-Age=2592000",
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
    ] {
        assert!(raw.contains(part), "{part} missing in {raw}");
    }
    assert!(
        !raw.contains("Domain="),
        "Host-prefixed cookies set no Domain: {raw}"
    );
}

#[tokio::test]
async fn revoke_endpoint_kills_tokens() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "ana@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;
    let tokens: serde_json::Value = exchange(&flow, &client_id, &params, redirect_uri).await;
    let access = tokens["access_token"].as_str().unwrap().to_string();

    let (status, _, _) = send(
        &flow.app,
        post_form("/oauth/revoke", &form(&[("token", &access)])),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        auth::resolve_bearer(&flow.state.pool, &flow.state.config, &access)
            .await
            .unwrap()
            .is_none()
    );
    // Unknown tokens still answer 200 (RFC 7009).
    let (status, _, _) = send(
        &flow.app,
        post_form("/oauth/revoke", &form(&[("token", "outer_at_dead")])),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn connections_list_record_and_revoke() {
    let flow = flow().await;
    let redirect_uri = "https://app.example/cb";
    let client_id = dcr_client(&flow.app, redirect_uri).await;
    let (request_id, csrf, session) =
        sign_in(&flow, &client_id, redirect_uri, "ana@example.com").await;
    let params = allow(&flow, &request_id, &csrf, &session).await;
    let tokens: serde_json::Value = exchange(&flow, &client_id, &params, redirect_uri).await;
    let access = tokens["access_token"].as_str().unwrap();

    let principal = auth::resolve_bearer(&flow.state.pool, &flow.state.config, access)
        .await
        .unwrap()
        .unwrap();
    let grant_id = match &principal.credential {
        auth::Credential::OAuth { grant_id } => grant_id.clone(),
        _ => panic!("expected an OAuth grant"),
    };
    auth::record_client_info(&flow.state.pool, &principal.credential, "Flow App", "1.0")
        .await
        .unwrap();
    let connections = auth::list_connections(&flow.state.pool, &principal.account_id)
        .await
        .unwrap();
    assert_eq!(connections.len(), 1);
    assert_eq!(connections[0].grant_id, grant_id);
    assert_eq!(connections[0].client_name.as_deref(), Some("Flow App"));

    assert!(
        auth::revoke_connection(&flow.state.pool, &principal.account_id, &grant_id)
            .await
            .unwrap()
    );
    assert!(
        !auth::revoke_connection(&flow.state.pool, &principal.account_id, &grant_id)
            .await
            .unwrap()
    );
    assert!(
        auth::resolve_bearer(&flow.state.pool, &flow.state.config, access)
            .await
            .unwrap()
            .is_none()
    );
}

/// A fake CIMD host on loopback: good docs, mismatched docs and oversize docs.
async fn fake_cimd() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let good = base.clone();
    let app = Router::new()
        .route(
            "/good",
            axum::routing::get(move || {
                let base = good.clone();
                async move {
                    Json(serde_json::json!({
                        "client_id": format!("{base}/good"),
                        "redirect_uris": ["http://127.0.0.1:9/cb"],
                        "client_name": "Loopback App",
                    }))
                }
            }),
        )
        .route(
            "/mismatch",
            axum::routing::get(|| async {
                Json(serde_json::json!({
                    "client_id": "https://other.example/doc",
                    "redirect_uris": ["https://other.example/cb"],
                }))
            }),
        )
        .route(
            "/big",
            axum::routing::get(|| async {
                let big = "x".repeat(20 * 1024);
                Json(serde_json::json!({ "client_id": "x", "redirect_uris": ["https://x.example/cb"], "pad": big }))
            }),
        );
    let handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap();
    });
    (base, handle)
}

#[tokio::test]
async fn cimd_policy_enforced_on_loopback() {
    use crate::auth::cimd;
    let pool = test_pool().await;
    let (config, _) = test_config();
    let (base, _server) = fake_cimd().await;

    // Non-https client_ids never fetch.
    let err = cimd::resolve_client(
        &pool,
        &config,
        "http://example.com/doc",
        "http://example.com/cb",
    )
    .await
    .unwrap_err();
    assert_eq!(err, cimd::ClientError::NonHttps);

    // Mismatched client_id in the doc.
    let err = cimd::resolve_client(
        &pool,
        &config,
        &format!("{base}/mismatch"),
        "https://other.example/cb",
    )
    .await
    .unwrap_err();
    assert_eq!(err, cimd::ClientError::ClientIdMismatch);

    // Oversize docs are refused before parsing.
    let err = cimd::resolve_client(
        &pool,
        &config,
        &format!("{base}/big"),
        "https://x.example/cb",
    )
    .await
    .unwrap_err();
    assert_eq!(err, cimd::ClientError::Oversize);

    // Unknown hosts are refused without network.
    // Unresolvable hosts are refused without network.
    let err = cimd::resolve_client(
        &pool,
        &config,
        "https://unknown.invalid/doc",
        "https://unknown.invalid/cb",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            cimd::ClientError::Fetch(_) | cimd::ClientError::PrivateIp
        ),
        "{err}"
    );
}

#[tokio::test]
async fn cimd_loopback_redirect_ignores_port() {
    use crate::auth::cimd;
    let pool = test_pool().await;
    let (config, _) = test_config();
    let (base, _server) = fake_cimd().await;
    // The /good doc is real: loopback redirects match with the port ignored.
    let doc_url = format!("{base}/good");
    let client = cimd::resolve_client(&pool, &config, &doc_url, "http://127.0.0.1:5555/cb")
        .await
        .unwrap();
    assert_eq!(client.name, "Loopback App");
    assert!(
        client.verified,
        "a CIMD client is bound to its https domain"
    );
    assert!(client.loopback_only);
    // The second resolve hits the oauth_client cache (the doc is fetched once).
    let again = cimd::resolve_client(&pool, &config, &doc_url, "http://127.0.0.1:7777/cb")
        .await
        .unwrap();
    assert_eq!(again.redirect_uris, client.redirect_uris);
}

/// A fake Cloudflare Email Service: records the request, behaves by address.
struct FakeEmail {
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
    bearer: Arc<Mutex<Vec<String>>>,
}

async fn fake_email() -> (String, FakeEmail, tokio::task::JoinHandle<()>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let bearer = Arc::new(Mutex::new(Vec::new()));
    let seen_route = seen.clone();
    let bearer_route = bearer.clone();
    let app = Router::new().route(
        "/client/v4/accounts/{account_id}/email/sending/send",
        post(|headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
            seen_route.lock().unwrap().push(body.clone());
            bearer_route.lock().unwrap().push(
                headers.get("authorization").unwrap().to_str().unwrap().to_string(),
            );
            let to = body["to"].as_str().unwrap_or("").to_string();
            if to.contains("bounce") {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true, "errors": [],
                        "result": { "delivered": [], "permanent_bounces": [to], "queued": [] },
                    })),
                );
            }
            if to.contains("ratelimit") {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(serde_json::json!({
                        "success": false, "errors": [{ "code": 10004, "message": "rate limited" }],
                    })),
                );
            }
            if to.contains("refused") {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "success": false, "errors": [{ "code": 10001, "message": "email.sending.error.invalid_request_schema" }],
                    })),
                );
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true, "errors": [],
                    "result": { "delivered": [to], "permanent_bounces": [], "queued": [] },
                })),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap();
    });
    (
        format!("http://127.0.0.1:{port}"),
        FakeEmail { seen, bearer },
        handle,
    )
}

#[tokio::test]
async fn cloudflare_sender_shape_bounce_and_retry() {
    use crate::auth::email::{EmailError, EmailFrom, EmailSender};
    let (base, fake, _server) = fake_email().await;
    let sender = EmailSender::Cloudflare {
        client: reqwest::Client::new(),
        api_base: base,
        account_id: "acct-1".to_string(),
        api_token: "cf-secret-token".to_string(),
        from: EmailFrom::parse("Outer <hello@outer.now>"),
    };

    sender.send_code("ana@example.com", "123456").await.unwrap();
    let seen = fake.seen.lock().unwrap().clone();
    let bearer = fake.bearer.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["to"], "ana@example.com");
    assert_eq!(
        seen[0]["from"],
        serde_json::json!({ "address": "hello@outer.now", "name": "Outer" })
    );
    assert!(seen[0]["subject"].as_str().unwrap().contains("123456"));
    assert!(seen[0]["text"].as_str().unwrap().contains("123456"));
    assert_eq!(bearer[0], "Bearer cf-secret-token");

    let err = sender
        .send_code("bounce@example.com", "123456")
        .await
        .unwrap_err();
    assert!(matches!(err, EmailError::Permanent(_)), "{err}");
    assert!(
        !err.to_string().contains("cf-secret-token"),
        "token never leaks"
    );

    let err = sender
        .send_code("ratelimit@example.com", "123456")
        .await
        .unwrap_err();
    assert!(matches!(err, EmailError::Retryable(_)), "{err}");
    assert!(
        !err.to_string().contains("cf-secret-token"),
        "token never leaks"
    );

    let err = sender
        .send_code("refused@example.com", "123456")
        .await
        .unwrap_err();
    assert!(matches!(err, EmailError::Permanent(_)), "{err}");
    assert!(err.to_string().contains("10001"));
}

/// Every app that identifies itself by an https document (Claude, ChatGPT,
/// Perplexity, any other CIMD client) gets a plain consent page; only a
/// self-registered app carries a neutral note. No page warns or refuses.
#[test]
fn consent_page_never_warns_and_notes_only_self_registered_apps() {
    use crate::auth::cimd::ClientDoc;
    use crate::auth::pages;
    let doc = |verified: bool| ClientDoc {
        client_id: "https://www.perplexity.ai/.well-known/mcp-client.json".to_string(),
        name: "Perplexity".to_string(),
        host: "www.perplexity.ai".to_string(),
        redirect_uris: vec!["https://www.perplexity.ai/rest/connections/oauth_callback".to_string()],
        verified,
        loopback_only: false,
    };
    let cimd = pages::render(pages::consent_page(&doc(true), "apps", "rid", "csrf"));
    assert!(cimd.contains("Perplexity"), "{cimd}");
    assert!(cimd.contains("www.perplexity.ai"), "{cimd}");
    assert!(!cimd.contains("registered itself"), "{cimd}");
    let dcr = pages::render(pages::consent_page(&doc(false), "apps", "rid", "csrf"));
    assert!(dcr.contains("registered itself"), "{dcr}");
    for page in [&cimd, &dcr] {
        assert!(!page.to_lowercase().contains("unverified"), "{page}");
        assert!(!page.contains("Only continue if you trust"), "{page}");
    }
}

/// The daily prune deletes only codes and sessions every reader agrees are
/// dead: `expires_at == now` is still acceptable on the verify paths, so it
/// stays for the next tick.
#[tokio::test]
async fn prune_expired_keeps_live_and_boundary_rows() {
    let pool = test_pool().await;
    sqlx::query(
        "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('a1', 'a@example.com', 0, 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (email, expires_at) in [
        ("old@example.com", 99),
        ("live@example.com", 200),
        ("edge@example.com", 100),
    ] {
        sqlx::query(
            "INSERT INTO verification (id, identifier, value, expiresAt, createdAt, updatedAt) VALUES (?, ?, 'h', ?, 0, 0)",
        )
        .bind(email)
        .bind(format!("email-verification:{email}"))
        .bind(expires_at)
        .execute(&pool)
        .await
        .unwrap();
    }
    for (hash, expires_at) in [("dead", 99), ("live", 200), ("edge", 100)] {
        sqlx::query(
            "INSERT INTO session (id, token, userId, expiresAt, createdAt, updatedAt) VALUES (?, ?, 'a1', ?, 0, 0)",
        )
        .bind(hash)
        .bind(hash)
        .bind(expires_at)
        .execute(&pool)
        .await
        .unwrap();
    }
    let (codes, sessions) = auth::prune_expired(&pool, 100).await.unwrap();
    assert_eq!((codes, sessions), (1, 1));
    let kept_codes: Vec<String> =
        sqlx::query_scalar("SELECT identifier FROM verification ORDER BY identifier")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        kept_codes,
        vec![
            "email-verification:edge@example.com",
            "email-verification:live@example.com"
        ]
    );
    let kept_sessions: Vec<String> = sqlx::query_scalar("SELECT token FROM session ORDER BY token")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(kept_sessions, vec!["edge", "live"]);
}

/// The daily prune deletes expired unclaimed requests, expired tokens and
/// stale DCR clients, keeping live rows, boundary rows (`expires_at == now`
/// is still acceptable) and non-DCR clients of any age.
#[tokio::test]
async fn oauth_prune_expired_keeps_live_and_other_kinds() {
    let pool = test_pool().await;
    // Realistic clock: the stale-DCR cutoff is 30 days, so toy timestamps
    // would make every client "fresh".
    let now: i64 = 3_000_000_000;
    sqlx::query(
        "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('a1', 'a@example.com', 0, 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (id, expires_at) in [
        ("r-old", now - 1),
        ("r-live", now + 86_400_000),
        ("r-edge", now),
    ] {
        sqlx::query(
            "INSERT INTO oauth_request (id, client_id, redirect_uri, code_challenge, csrf, expires_at) \
             VALUES (?, 'c', 'https://x.example/cb', 'ch', 'csrf', ?)",
        )
        .bind(id)
        .bind(expires_at)
        .execute(&pool)
        .await
        .unwrap();
    }
    for (client_id, kind, created_at) in [
        ("c", "cimd", 100),
        ("dcr-old", "dcr", 100),
        ("dcr-fresh", "dcr", now - 1_000),
        ("cimd-old", "cimd", 100),
    ] {
        sqlx::query(
            "INSERT INTO oauthClient (id, clientId, kind, redirectUris, verified, createdAt, updatedAt) VALUES (?, ?, ?, '[]', 0, ?, ?)",
        )
        .bind(format!("oc-{client_id}"))
        .bind(client_id)
        .bind(kind)
        .bind(created_at)
        .bind(created_at)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO oauthConsent (id, clientId, userId, scopes, resources, createdAt, updatedAt) \
         VALUES ('g1', 'c', 'a1', 'apps', 'r', 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (hash, expires_at) in [
        ("t-old", now - 1),
        ("t-live", now + 86_400_000),
        ("t-edge", now),
    ] {
        sqlx::query(
            "INSERT INTO oauthAccessToken (id, token, clientId, userId, referenceId, scopes, expiresAt, createdAt) VALUES (?, ?, 'c', 'a1', 'g1', 'apps', ?, 0)",
        )
        .bind(hash)
        .bind(hash)
        .bind(expires_at)
        .execute(&pool)
        .await
        .unwrap();
    }
    let (requests, tokens, clients) = auth::oauth::prune_expired(&pool, now).await.unwrap();
    assert_eq!((requests, tokens, clients), (1, 1, 1));
    let kept_requests: Vec<String> = sqlx::query_scalar("SELECT id FROM oauth_request ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(kept_requests, vec!["r-edge", "r-live"]);
    let kept_tokens: Vec<String> =
        sqlx::query_scalar("SELECT token FROM oauthAccessToken ORDER BY token")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(kept_tokens, vec!["t-edge", "t-live"]);
    let kept_clients: Vec<String> =
        sqlx::query_scalar("SELECT clientId FROM oauthClient ORDER BY clientId")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(kept_clients, vec!["c", "cimd-old", "dcr-fresh"]);
}
