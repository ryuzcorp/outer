//! OAuth 2.1 authorization server: discovery, DCR, the sign-in page,
//! token issuance and revocation. Mounted on the platform router only; app
//! hosts never see these routes.

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use serde_json::json;
use sqlx::SqlitePool;
use url::Url;

use super::cimd;
use super::{limits, pages, tokens};
use super::{now_ms, AuthState};
use crate::http::client_ip::client_ip;

/// Pending authorization TTL: 10 minutes to finish the flow.
const PENDING_TTL_MS: i64 = 10 * 60_000;
/// Authorization code TTL: single use, 60 s.
const AUTH_CODE_TTL_MS: i64 = 60_000;
/// Registrations with no token for 30 days are pruned.
const STALE_DCR_MS: i64 = 30 * 86_400_000;
/// DCR per-IP budget: shared provider egress (ChatGPT, Claude) registers on
/// behalf of many users from one address, so the cap bounds one registrant
/// without becoming a per-provider cap.
const DCR_IP_LIMIT: u32 = 120;
/// DCR global budget: a row-growth guard for the 30-day-retained client
/// rows, not a per-client cap.
const DCR_GLOBAL_LIMIT: u32 = 300;
/// One fixed window for both DCR buckets and the authorize page.
const RATE_WINDOW_MS: i64 = 3_600_000;
/// `/oauth/authorize` GET budget per IP: one fetch per sign-in plus retries,
/// generous for CGNAT. The browser calls this from the user's own address.
const AUTHORIZE_LIMIT: u32 = 120;
/// A public client that retries a refresh with the token it just rotated
/// away gets the same pair back inside this window instead of a revocation.
const REFRESH_REUSE_GRACE_MS: i64 = 30_000;
/// How long a losing concurrent refresh waits for the winner's commit
/// before it treats the token as stolen (20 × 50 ms).
const REPLAY_POLL_ATTEMPTS: u8 = 20;
const REPLAY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// `/.well-known/oauth-protected-resource`: the RFC 9728 document.
pub const PROTECTED_RESOURCE_PATH: &str = "/.well-known/oauth-protected-resource";
/// `/.well-known/oauth-protected-resource/mcp`: the protected-resource
/// document at the path clients derive from the MCP resource URL.
pub const PROTECTED_RESOURCE_MCP_PATH: &str = "/.well-known/oauth-protected-resource/mcp";
/// `/.well-known/oauth-authorization-server` (RFC 8414).
pub const AUTHORIZATION_SERVER_PATH: &str = "/.well-known/oauth-authorization-server";

/// The exact `/.well-known/*` paths the auth router answers. The platform
/// fallback uses the same list ([`super::owns_path`]): every other
/// well-known path belongs to the site (its `/.well-known/` files).
pub const WELL_KNOWN_PATHS: &[&str] = &[
    PROTECTED_RESOURCE_PATH,
    PROTECTED_RESOURCE_MCP_PATH,
    AUTHORIZATION_SERVER_PATH,
];

pub fn router(state: AuthState) -> Router {
    Router::new()
        .route(PROTECTED_RESOURCE_PATH, get(protected_resource))
        .route(PROTECTED_RESOURCE_MCP_PATH, get(protected_resource))
        .route(AUTHORIZATION_SERVER_PATH, get(authorization_server))
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize_get).post(authorize_post))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .with_state(state)
}

fn json_nostore(value: serde_json::Value) -> Response {
    json_text_nostore(value.to_string())
}

fn json_text_nostore(text: String) -> Response {
    (
        StatusCode::OK,
        [
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
            ("content-type", "application/json"),
        ],
        text,
    )
        .into_response()
}

/// Redirect targets a self-registered client may use: https anywhere,
/// plain http on loopback (CLI tools on an ephemeral port), and an app's own
/// URI scheme (RFC 8252 §7.1: desktop apps such as Cursor or VS Code). Never
/// script, data or file URLs, and never plain http to a remote host.
fn dcr_redirect_allowed(uri: &Url) -> bool {
    match uri.scheme() {
        "https" => true,
        "http" => cimd::is_loopback_host(uri.host_str()),
        "javascript" | "data" | "file" | "vbscript" | "about" | "blob" | "ftp" | "ws" | "wss" => {
            false
        }
        _ => true,
    }
}

/// The redirect-URI rules shared by DCR and the operator command: https
/// anywhere, plain http on loopback, and an app's own scheme. Returns the
/// URI unchanged or the reason it is refused.
fn validated_redirect(uri: &str) -> Result<String, String> {
    let Ok(parsed) = Url::parse(uri) else {
        return Err(format!("{uri} is not a URL"));
    };
    if !dcr_redirect_allowed(&parsed) {
        return Err(
            "redirect URIs must be https, loopback http, or an app's own scheme (cursor://, vscode://, …)"
                .to_string(),
        );
    }
    Ok(uri.to_string())
}

fn rate_limited_json(retry_after_s: u64) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("retry-after", retry_after_s.to_string().parse().unwrap());
    (
        StatusCode::TOO_MANY_REQUESTS,
        headers,
        Json(json!({ "error": "rate_limited", "error_description": "too many registrations" }))
            .0
            .to_string(),
    )
        .into_response()
}

fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response {
    tracing::info!(
        status = status.as_u16(),
        error = code,
        description,
        "oauth error answered"
    );
    (
        status,
        [
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
            ("content-type", "application/json"),
        ],
        Json(json!({ "error": code, "error_description": description }))
            .0
            .to_string(),
    )
        .into_response()
}

fn html(status: StatusCode, page: maud::Markup, cookie: Option<String>) -> Response {
    html_with_csp(status, page, cookie, pages::CSP)
}

/// The consent page: its Allow/Deny form answers with a redirect to the
/// client, and browsers apply CSP `form-action` to the redirect target of a
/// form submission too. With only `'self'` the browser silently drops the
/// 302 to the app (the user stays on the consent page — found with
/// Perplexity), so this page also allows the request's own redirect origin.
fn consent(page: maud::Markup, redirect_uri: &str, cookie: Option<String>) -> Response {
    let csp = pages::consent_csp(redirect_uri);
    html_with_csp(StatusCode::OK, page, cookie, &csp)
}

fn html_with_csp(
    status: StatusCode,
    page: maud::Markup,
    cookie: Option<String>,
    csp: &str,
) -> Response {
    let mut response = (
        status,
        [
            ("content-type", "text/html; charset=utf-8"),
            ("content-security-policy", csp),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
            ("cache-control", "no-store"),
        ],
        pages::render(page),
    )
        .into_response();
    if let Some(cookie) = cookie {
        response.headers_mut().insert(
            "set-cookie",
            cookie.parse().expect("session cookie is header-safe"),
        );
    }
    response
}

fn request_ip(
    headers: &HeaderMap,
    origin_locked: bool,
    peer: Option<SocketAddr>,
) -> Option<String> {
    client_ip(headers, origin_locked, peer)
}

/// `GET /.well-known/oauth-protected-resource[/mcp]` (spec §6.1).
async fn protected_resource(State(state): State<AuthState>) -> Response {
    json_nostore(json!({
        "resource": state.config.resource,
        "authorization_servers": [state.config.issuer],
        "scopes_supported": ["apps"],
        "bearer_methods_supported": ["header"],
    }))
}

/// `GET /.well-known/oauth-authorization-server` (spec §6.1).
async fn authorization_server(State(state): State<AuthState>) -> Response {
    let issuer = &state.config.issuer;
    json_nostore(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/oauth/authorize"),
        "token_endpoint": format!("{issuer}/oauth/token"),
        "registration_endpoint": format!("{issuer}/oauth/register"),
        "revocation_endpoint": format!("{issuer}/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
        "scopes_supported": ["apps", "offline_access"],
    }))
}

#[derive(Debug)]
struct PendingRequest {
    id: String,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    scope: String,
    account_id: Option<String>,
    csrf: String,
}

/// One pending authorization row: id, client_id, redirect_uri, state,
/// code_challenge, scope, resource, userId, csrf, expires_at.
type PendingRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    i64,
);
/// One issued authorization code row: id, client_id, redirect_uri, state,
/// code_challenge, scope, resource, userId, expires_at.
type CodeRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
);
/// One refresh token row: row id, consent id, userId, clientId, scopes,
/// resources, expiresAt, rotatedAt, the public-client replay payload and its
/// expiry, the user's banned flag and ban expiry.
/// `resources`/`expiresAt` arrive `None` only from rows
/// Outer never writes (it always sets both); `None` expires immediately.
type RefreshRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    i64,
    Option<i64>,
);
async fn load_request(
    pool: &SqlitePool,
    id: &str,
    now: i64,
) -> anyhow::Result<Option<PendingRequest>> {
    let row: Option<PendingRow> =
        sqlx::query_as(
            "SELECT id, client_id, redirect_uri, state, code_challenge, scope, resource, userId, csrf, expires_at \
             FROM oauth_request WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.filter(|row| now <= row.9).map(
        |(id, client_id, redirect_uri, state, _, scope, _, account_id, csrf, _)| PendingRequest {
            id,
            client_id,
            redirect_uri,
            state,
            scope: scope.unwrap_or_else(|| "apps".to_string()),
            account_id,
            csrf,
        },
    ))
}

/// Every scope must come from the fixed set; the stored scope is normalized
/// to the requested subset (default `apps`).
fn normalize_scope(raw: Option<&str>) -> Option<String> {
    let raw = raw.unwrap_or("apps");
    let mut scopes = raw
        .split(' ')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if scopes.is_empty()
        || scopes
            .iter()
            .any(|part| *part != "apps" && *part != "offline_access")
    {
        return None;
    }
    scopes.sort_unstable();
    scopes.dedup();
    Some(scopes.join(" "))
}

/// `GET /oauth/authorize`: validate, stash the pending request, and show the
/// email page (or skip to terms/consent on a returning session).
async fn authorize_get(
    State(state): State<AuthState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let ip = request_ip(&headers, state.config.origin_locked, Some(peer))
        .unwrap_or_else(|| "unknown".to_string());
    let now = now_ms();
    // Limited first: an over-limit request must not insert an
    // `oauth_request` row or trigger a CIMD fetch.
    if let Some(retry_after_s) = limits::check_and_record(
        &state.pool,
        &format!("ip:{ip}"),
        "oauth_authorize",
        AUTHORIZE_LIMIT,
        RATE_WINDOW_MS,
        now,
    )
    .await
    .unwrap_or(None)
    {
        let mut response = html(
            StatusCode::TOO_MANY_REQUESTS,
            pages::error_page("Too many attempts", pages::friendly_error("rate_limited")),
            None,
        );
        response.headers_mut().insert(
            "retry-after",
            retry_after_s
                .to_string()
                .parse()
                .expect("digits are header-safe"),
        );
        return response;
    }
    let error = |message: &str| {
        html(
            StatusCode::BAD_REQUEST,
            pages::error_page("Sign in failed", message),
            None,
        )
    };
    if query.get("response_type").map(String::as_str) != Some("code") {
        return error("response_type must be code");
    }
    let (Some(client_id), Some(redirect_uri), Some(challenge)) = (
        query.get("client_id"),
        query.get("redirect_uri"),
        query.get("code_challenge"),
    ) else {
        return error("client_id, redirect_uri and code_challenge are required");
    };
    if query.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return error("code_challenge_method must be S256");
    }
    let Some(scope) = normalize_scope(query.get("scope").map(String::as_str)) else {
        return error("scope must be a subset of \"apps offline_access\"");
    };
    let resource = query.get("resource").cloned();
    if let Some(resource) = &resource {
        if *resource != state.config.resource {
            return error("resource must be the Outer MCP URL");
        }
    }
    let client =
        match cimd::resolve_client(&state.pool, &state.config, client_id, redirect_uri).await {
            Ok(client) => client,
            Err(err) => {
                // A client the matrix doesn't cover yet shows up here first.
                tracing::info!(client_id, redirect_uri, error = %err, "oauth client refused");
                return error(&clinic_error(&err));
            }
        };
    let id = tokens::new_id();
    let csrf = tokens::new_csrf();
    if sqlx::query(
        "INSERT INTO oauth_request (id, client_id, redirect_uri, state, code_challenge, scope, resource, csrf, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(client_id)
    .bind(redirect_uri)
    .bind(query.get("state"))
    .bind(challenge)
    .bind(&scope)
    .bind(&resource)
    .bind(&csrf)
    .bind(now + PENDING_TTL_MS)
    .execute(&state.pool)
    .await
    .is_err()
    {
        return error("sign-in is unavailable right now");
    }
    // Returning session: the sign-in already happened, skip email and code.
    if let Some(session_id) = super::session_from_headers(&headers) {
        if let Ok(Some(account)) = super::account_from_session(&state.pool, &session_id).await {
            sqlx::query("UPDATE oauth_request SET userId = ? WHERE id = ?")
                .bind(&account.id)
                .bind(&id)
                .execute(&state.pool)
                .await
                .ok();
            if !account.terms_accepted(&state.config) {
                return html(
                    StatusCode::OK,
                    pages::terms_page(&state.config.terms_version, &id, &csrf, None),
                    None,
                );
            }
            return consent(
                pages::consent_page(&client, &scope, &id, &csrf),
                redirect_uri,
                None,
            );
        }
    }
    html(
        StatusCode::OK,
        pages::email_page(&client, &id, &csrf, None, false, "", None),
        None,
    )
}

fn clinic_error(err: &cimd::ClientError) -> String {
    match err {
        cimd::ClientError::RedirectMismatch => {
            "redirect_uri is not registered for this client".to_string()
        }
        cimd::ClientError::UnknownClient => "unknown client".to_string(),
        other => format!("client check failed: {other}"),
    }
}

/// `POST /oauth/authorize`: one step per form (`email` → `code` → `terms` →
/// `decision`). Pending state and CSRF live in `oauth_request`, not cookies.
async fn authorize_post(
    State(state): State<AuthState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let now = now_ms();
    let ip = request_ip(&headers, state.config.origin_locked, Some(peer));
    let error = |message: &str| {
        html(
            StatusCode::BAD_REQUEST,
            pages::error_page("Sign in failed", message),
            None,
        )
    };
    let (Some(request_id), Some(csrf), Some(step)) =
        (form.get("request_id"), form.get("csrf"), form.get("step"))
    else {
        return error("request_id, csrf and step are required");
    };
    let Some(pending) = load_request(&state.pool, request_id, now)
        .await
        .unwrap_or(None)
    else {
        return error("this sign-in expired, start over from the app");
    };
    if !tokens::ct_eq(csrf, &pending.csrf) {
        return html(
            StatusCode::FORBIDDEN,
            pages::error_page(
                "Sign in failed",
                "session mismatch, start over from the app",
            ),
            None,
        );
    }
    let client = match cimd::resolve_client(
        &state.pool,
        &state.config,
        &pending.client_id,
        &pending.redirect_uri,
    )
    .await
    {
        Ok(client) => client,
        Err(err) => return error(&clinic_error(&err)),
    };
    match step.as_str() {
        "email" => {
            let email = form.get("email").cloned().unwrap_or_default();
            let invite_code = form.get("invite_code").cloned().unwrap_or_default();
            let invite_value = invite_code.trim().to_string();
            match super::send_code(
                &state.pool,
                &state.config,
                &email,
                (!invite_value.is_empty()).then_some(invite_value.as_str()),
                ip.as_deref(),
            )
            .await
            {
                Ok(_) => html(
                    StatusCode::OK,
                    pages::code_page(
                        &super::normalize_email(&email),
                        &pending.id,
                        &pending.csrf,
                        None,
                    ),
                    None,
                ),
                Err(err) => {
                    let status = if err.code == "rate_limited" {
                        StatusCode::TOO_MANY_REQUESTS
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    // A closed-beta refusal opens the invite field so the
                    // user can retry with a code in one step.
                    let invite_open = err.code == "closed_beta";
                    html(
                        status,
                        pages::email_page(
                            &client,
                            &pending.id,
                            &pending.csrf,
                            Some(pages::friendly_error(err.code)),
                            invite_open,
                            &invite_value,
                            None,
                        ),
                        None,
                    )
                }
            }
        }
        "code" => {
            let email = form.get("email").cloned().unwrap_or_default();
            let code = form.get("code").cloned().unwrap_or_default();
            match super::verify_code_core(&state.pool, &state.config, &email, &code, ip.as_deref())
                .await
            {
                Ok((account_id, terms_ok)) => {
                    sqlx::query("UPDATE oauth_request SET userId = ? WHERE id = ?")
                        .bind(&account_id)
                        .bind(&pending.id)
                        .execute(&state.pool)
                        .await
                        .ok();
                    let session = super::create_session(&state.pool, &account_id)
                        .await
                        .ok()
                        .map(|id| super::set_session_cookie(&id));
                    if !terms_ok {
                        return html(
                            StatusCode::OK,
                            pages::terms_page(
                                &state.config.terms_version,
                                &pending.id,
                                &pending.csrf,
                                None,
                            ),
                            session,
                        );
                    }
                    consent(
                        pages::consent_page(&client, &pending.scope, &pending.id, &pending.csrf),
                        &pending.redirect_uri,
                        session,
                    )
                }
                Err(err) => html(
                    if err.code == "rate_limited" {
                        StatusCode::TOO_MANY_REQUESTS
                    } else {
                        StatusCode::BAD_REQUEST
                    },
                    pages::code_page(
                        &super::normalize_email(&email),
                        &pending.id,
                        &pending.csrf,
                        Some(pages::friendly_error(err.code)),
                    ),
                    None,
                ),
            }
        }
        "password" => {
            let email = form.get("email").cloned().unwrap_or_default();
            let password = form.get("password").cloned().unwrap_or_default();
            match super::reviewer_sign_in(
                &state.pool,
                &state.config,
                &email,
                &password,
                ip.as_deref(),
            )
            .await
            {
                Ok((account_id, terms_ok)) => {
                    sqlx::query("UPDATE oauth_request SET userId = ? WHERE id = ?")
                        .bind(&account_id)
                        .bind(&pending.id)
                        .execute(&state.pool)
                        .await
                        .ok();
                    let session = super::create_session(&state.pool, &account_id)
                        .await
                        .ok()
                        .map(|id| super::set_session_cookie(&id));
                    if !terms_ok {
                        return html(
                            StatusCode::OK,
                            pages::terms_page(
                                &state.config.terms_version,
                                &pending.id,
                                &pending.csrf,
                                None,
                            ),
                            session,
                        );
                    }
                    consent(
                        pages::consent_page(&client, &pending.scope, &pending.id, &pending.csrf),
                        &pending.redirect_uri,
                        session,
                    )
                }
                Err(err) => {
                    let status = if err.code == "rate_limited" {
                        StatusCode::TOO_MANY_REQUESTS
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    html(
                        status,
                        pages::email_page(
                            &client,
                            &pending.id,
                            &pending.csrf,
                            None,
                            false,
                            "",
                            Some(pages::friendly_error(err.code)),
                        ),
                        None,
                    )
                }
            }
        }
        "terms" => {
            let Some(account_id) = &pending.account_id else {
                return error("sign in with email first");
            };
            if form.get("accept").map(String::as_str) != Some("yes") {
                return html(
                    StatusCode::BAD_REQUEST,
                    pages::terms_page(
                        &state.config.terms_version,
                        &pending.id,
                        &pending.csrf,
                        Some("Accept the Terms and Privacy Policy to continue."),
                    ),
                    None,
                );
            }
            if super::record_terms_acceptance(&state.pool, &state.config, account_id)
                .await
                .is_err()
            {
                return error("sign-in is unavailable right now");
            }
            consent(
                pages::consent_page(&client, &pending.scope, &pending.id, &pending.csrf),
                &pending.redirect_uri,
                None,
            )
        }
        "decision" => {
            if pending.account_id.is_none() {
                return error("sign in with email first");
            }
            if form.get("allow").map(String::as_str) == Some("yes") {
                let (code, code_hash) = tokens::new_auth_code();
                sqlx::query("UPDATE oauth_request SET code_hash = ?, expires_at = ? WHERE id = ?")
                    .bind(&code_hash)
                    .bind(now + AUTH_CODE_TTL_MS)
                    .bind(&pending.id)
                    .execute(&state.pool)
                    .await
                    .ok();
                redirect(
                    &pending.redirect_uri,
                    &[("code", &code)],
                    pending.state.as_deref(),
                    &state.config.issuer,
                )
            } else {
                redirect(
                    &pending.redirect_uri,
                    &[("error", "access_denied")],
                    pending.state.as_deref(),
                    &state.config.issuer,
                )
            }
        }
        _ => error("unknown step"),
    }
}

/// Allow and deny hand the browser back to the client (see
/// `pages::return_page` for why it isn't a 302). The URL carries `iss`
/// (RFC 9207) plus `state` when sent.
fn redirect(base: &str, params: &[(&str, &str)], state: Option<&str>, issuer: &str) -> Response {
    let mut url = match Url::parse(base) {
        Ok(url) => url,
        Err(_) => {
            return html(
                StatusCode::BAD_REQUEST,
                pages::error_page("Sign in failed", "redirect_uri is not a URL"),
                None,
            )
        }
    };
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in params {
            pairs.append_pair(key, value);
        }
        if let Some(state) = state {
            pairs.append_pair("state", state);
        }
        pairs.append_pair("iss", issuer);
    }
    let host = match url.host_str() {
        Some(host) => host.to_string(),
        None => format!("{}:", url.scheme()),
    };
    html(
        StatusCode::OK,
        pages::return_page(url.as_str(), &host),
        None,
    )
}

/// `POST /oauth/register` (RFC 7591): HTTPS or loopback redirects, public or
/// secret-bearing clients, 120/h per IP plus 300/h globally. Prunes
/// registrations with no token for 30 days (operator-created clients are
/// `verified` and outlive the prune).
async fn register(
    State(state): State<AuthState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let now = now_ms();
    let ip = request_ip(&headers, state.config.origin_locked, Some(peer))
        .unwrap_or_else(|| "unknown".to_string());
    // Two buckets before the body is even parsed: the global one is a
    // row-growth guard for the 30-day-retained client rows, the per-IP one
    // bounds a single registrant. Both record on a pass; a partial failure
    // over-counts by one, which is harmless.
    if let Some(retry_after_s) = limits::check_and_record(
        &state.pool,
        "global",
        "dcr_register",
        DCR_GLOBAL_LIMIT,
        RATE_WINDOW_MS,
        now,
    )
    .await
    .unwrap_or(None)
    {
        return rate_limited_json(retry_after_s);
    }
    if let Some(retry_after_s) = limits::check_and_record(
        &state.pool,
        &format!("ip:{ip}"),
        "dcr_register",
        DCR_IP_LIMIT,
        RATE_WINDOW_MS,
        now,
    )
    .await
    .unwrap_or(None)
    {
        return rate_limited_json(retry_after_s);
    }
    // `verified = 0` keeps operator-created clients (`verified = 1`) out of
    // the stale-DCR sweep: they are not anonymous self-registrations.
    sqlx::query(
        "DELETE FROM oauthClient WHERE kind = 'dcr' AND verified = 0 AND lastTokenAt IS NULL AND createdAt < ?",
    )
    .bind(now - STALE_DCR_MS)
    .execute(&state.pool)
    .await
    .ok();
    let uris = body
        .get("redirect_uris")
        .and_then(|uris| uris.as_array())
        .cloned()
        .unwrap_or_default();
    if uris.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect_uris must not be empty",
        );
    }
    let mut redirect_uris = Vec::with_capacity(uris.len());
    for uri in &uris {
        let Some(uri) = uri.as_str() else {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                "redirect_uris must be strings",
            );
        };
        match validated_redirect(uri) {
            Ok(uri) => redirect_uris.push(uri),
            Err(reason) => {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", &reason)
            }
        }
    }
    // `none` (the default) registers a public client; `client_secret_post`
    // and `client_secret_basic` get a secret stored as a hash. Anything
    // else (private_key_jwt, tls_client_auth, …) falls back to `none` and is
    // told so: refusing a client that defaults to an unknown method would
    // lock it out, and PKCE protects the code either way.
    let method = match body
        .get("token_endpoint_auth_method")
        .and_then(|method| method.as_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("client_secret_post") => "client_secret_post",
        Some("client_secret_basic") => "client_secret_basic",
        _ => "none",
    };
    let secret = if method == "none" {
        None
    } else {
        Some(tokens::new_client_secret())
    };
    let name = body
        .get("client_name")
        .and_then(|name| name.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(200).collect::<String>());
    let host = redirect_uris
        .first()
        .and_then(|uri| Url::parse(uri).ok())
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default();
    let client_id = format!("dcr_{}", hex::encode(rand::random::<[u8; 16]>()));
    // A row that was never stored must not hand out a secret: the client
    // could never authenticate with it, and the 201 would say otherwise.
    let stored = sqlx::query(
        "INSERT INTO oauthClient (id, clientId, kind, name, host, redirectUris, verified, clientSecret, tokenEndpointAuthMethod, createdAt, updatedAt) \
         VALUES (?, ?, 'dcr', ?, ?, ?, 0, ?, ?, ?, ?)",
    )
    .bind(tokens::new_id())
    .bind(&client_id)
    .bind(&name)
    .bind(&host)
    .bind(serde_json::to_string(&redirect_uris).unwrap_or_default())
    .bind(secret.as_ref().map(|(_, hash)| hash.clone()))
    .bind(method)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await;
    if stored.is_err() {
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "the client could not be registered",
        );
    }
    let mut response = json!({
        "client_id": client_id,
        "redirect_uris": redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": method,
    });
    if let Some((secret, _)) = secret {
        response["client_secret"] = json!(secret);
        response["client_secret_expires_at"] = json!(0);
    }
    if let Some(name) = name {
        response["client_name"] = json!(name);
    }
    (
        StatusCode::CREATED,
        [
            ("content-type", "application/json"),
            ("cache-control", "no-store"),
        ],
        Json(response).0.to_string(),
    )
        .into_response()
}

/// `POST /oauth/token` (form): authorization_code (single use, 60 s TTL,
/// PKCE S256 required, `client_id`/redirect_uri/`resource` bound to the
/// authorize request) and refresh_token (public clients rotate every use
/// with a replay grace; confidential clients get a non-rotating sliding
/// token; 90-day sliding). Proper OAuth JSON errors + `Cache-Control:
/// no-store`.
async fn token(
    State(state): State<AuthState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let _ = request_ip(&headers, state.config.origin_locked, Some(peer));
    let now = now_ms();
    sqlx::query("DELETE FROM oauthAccessToken WHERE expiresAt < ?")
        .bind(now)
        .execute(&state.pool)
        .await
        .ok();
    sqlx::query("DELETE FROM oauthRefreshToken WHERE expiresAt < ?")
        .bind(now)
        .execute(&state.pool)
        .await
        .ok();
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => exchange_code(&state, &headers, form, now).await,
        Some("refresh_token") => exchange_refresh(&state, &headers, form, now).await,
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "grant_type must be authorization_code or refresh_token",
        ),
    }
}

/// What a token request presents as the client. `via_basic` only decides
/// whether an authentication failure carries `WWW-Authenticate`.
struct ClientCreds {
    client_id: String,
    secret: Option<String>,
    via_basic: bool,
}

/// A decoded `Authorization: Basic` header.
enum Basic {
    Absent,
    Malformed,
    Credentials(String, String),
}

fn parse_basic(headers: &HeaderMap) -> Basic {
    let Some(value) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    else {
        return Basic::Absent;
    };
    let Some((scheme, encoded)) = value.split_once(' ') else {
        return Basic::Malformed;
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return Basic::Absent;
    }
    let Some(decoded) = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
    else {
        return Basic::Malformed;
    };
    // `rsplit_once`: a CIMD `client_id` is an unencoded URL that itself
    // contains colons; the secret never does (base64url).
    let Some((id, secret)) = decoded.rsplit_once(':') else {
        return Basic::Malformed;
    };
    Basic::Credentials(form_decode(id), form_decode(secret))
}

/// RFC 6749 §2.3.1: client credentials are form-urlencoded on the wire.
fn form_decode(raw: &str) -> String {
    let plus = raw.replace('+', " ");
    percent_encoding::percent_decode_str(&plus)
        .decode_utf8_lossy()
        .into_owned()
}

/// Why client authentication failed. Returned instead of a built `Response`
/// so the helper's `Result` stays small; the call site renders it.
struct ClientAuthError {
    status: StatusCode,
    code: &'static str,
    description: &'static str,
    via_basic: bool,
}

impl ClientAuthError {
    /// Fails closed when the client row cannot be read: treating a DB error
    /// as "no secret stored" would downgrade a confidential client to a
    /// public one and skip the secret check.
    fn server() -> Self {
        ClientAuthError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "server_error",
            description: "the client store is unavailable",
            via_basic: false,
        }
    }

    fn response(self) -> Response {
        let mut response = oauth_error(self.status, self.code, self.description);
        if self.via_basic {
            response.headers_mut().insert(
                "www-authenticate",
                "Basic realm=\"outer\"".parse().expect("static header"),
            );
        }
        response
    }
}

/// The client identity a token request presents: form `client_id` first,
/// else Basic. A malformed Basic header is ignored when the form already
/// names the client (public-client compatibility).
fn client_creds(
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Result<ClientCreds, ClientAuthError> {
    let form_id = form.get("client_id").filter(|id| !id.is_empty());
    let form_secret = form
        .get("client_secret")
        .filter(|secret| !secret.is_empty());
    match parse_basic(headers) {
        Basic::Credentials(basic_id, basic_secret) => match form_id {
            Some(form_id) if form_id != &basic_id => Err(ClientAuthError {
                status: StatusCode::UNAUTHORIZED,
                code: "invalid_client",
                description: "client_id in the body does not match the Authorization header",
                via_basic: true,
            }),
            Some(form_id) => Ok(ClientCreds {
                client_id: form_id.clone(),
                secret: Some(basic_secret),
                via_basic: true,
            }),
            None => Ok(ClientCreds {
                client_id: basic_id,
                secret: Some(basic_secret),
                via_basic: true,
            }),
        },
        Basic::Malformed if form_id.is_none() => Err(ClientAuthError {
            status: StatusCode::UNAUTHORIZED,
            code: "invalid_client",
            description: "the Authorization header is not valid Basic credentials",
            via_basic: true,
        }),
        Basic::Absent | Basic::Malformed => match form_id {
            Some(form_id) => Ok(ClientCreds {
                client_id: form_id.clone(),
                secret: form_secret.cloned(),
                via_basic: false,
            }),
            None => Err(ClientAuthError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_client",
                description: "client_id is required for public clients",
                via_basic: false,
            }),
        },
    }
}

/// The resolved client plus whether it is confidential. CIMD rows and rows
/// with no stored secret (including every pre-L-11 DCR row) are public:
/// any secret or Basic header they send is ignored, never verified.
struct AuthenticatedClient {
    client_id: String,
    confidential: bool,
}

async fn authenticate_client(
    state: &AuthState,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Result<AuthenticatedClient, ClientAuthError> {
    let creds = client_creds(headers, form)?;
    let row: Option<(Option<String>, Option<String>)> =
        match sqlx::query_as("SELECT clientSecret, kind FROM oauthClient WHERE clientId = ?")
            .bind(&creds.client_id)
            .fetch_optional(&state.pool)
            .await
        {
            Ok(row) => row,
            Err(_) => return Err(ClientAuthError::server()),
        };
    let stored_hash = row.as_ref().and_then(|(secret, kind)| {
        let cimd = kind.as_deref() == Some("cimd");
        (!cimd).then(|| secret.clone()).flatten()
    });
    let Some(stored_hash) = stored_hash else {
        return Ok(AuthenticatedClient {
            client_id: creds.client_id,
            confidential: false,
        });
    };
    let unauthorized = |description: &'static str| ClientAuthError {
        status: StatusCode::UNAUTHORIZED,
        code: "invalid_client",
        description,
        via_basic: creds.via_basic,
    };
    let Some(secret) = creds.secret.filter(|secret| !secret.is_empty()) else {
        return Err(unauthorized("this client must present its secret"));
    };
    if !tokens::ct_eq(&tokens::sha256_hex(&secret), &stored_hash) {
        return Err(unauthorized("the client secret is wrong"));
    }
    Ok(AuthenticatedClient {
        client_id: creds.client_id,
        confidential: true,
    })
}

async fn exchange_code(
    state: &AuthState,
    headers: &HeaderMap,
    form: HashMap<String, String>,
    now: i64,
) -> Response {
    let bad =
        |code: &str, description: &str| oauth_error(StatusCode::BAD_REQUEST, code, description);
    // Authenticate before the code is loaded or claimed: a wrong secret
    // must not burn a code. A missing id is an authentication failure, not a
    // bad grant.
    let auth = match authenticate_client(state, headers, &form).await {
        Ok(auth) => auth,
        Err(err) => return err.response(),
    };
    let form_client = &auth.client_id;
    let (Some(code), Some(redirect_uri), Some(verifier)) = (
        form.get("code"),
        form.get("redirect_uri"),
        form.get("code_verifier"),
    ) else {
        return bad(
            "invalid_request",
            "code, redirect_uri and code_verifier are required",
        );
    };
    if verifier.is_empty() {
        return bad(
            "invalid_request",
            "code_verifier is required (PKCE S256 only)",
        );
    }
    let hash = tokens::sha256_hex(code);
    let row: Option<CodeRow> =
        sqlx::query_as(
            "SELECT id, client_id, redirect_uri, state, code_challenge, scope, resource, userId, expires_at \
             FROM oauth_request WHERE code_hash = ?",
        )
        .bind(&hash)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
    let Some((
        id,
        client_id,
        stored_redirect,
        _,
        challenge,
        scope,
        stored_resource,
        account_id,
        expires_at,
    )) = row
    else {
        return bad("invalid_grant", "the authorization code is unknown");
    };
    // Single use, atomically: only the redemption that actually deletes the
    // row may continue; a concurrent duplicate sees zero rows affected.
    let claimed = sqlx::query("DELETE FROM oauth_request WHERE id = ? AND code_hash = ?")
        .bind(&id)
        .bind(&hash)
        .execute(&state.pool)
        .await
        .map(|done| done.rows_affected() == 1)
        .unwrap_or(false);
    if !claimed {
        return bad("invalid_grant", "the authorization code was already used");
    }
    if form_client != &client_id {
        return bad(
            "invalid_grant",
            "client_id does not match the authorize request",
        );
    }
    if now > expires_at {
        return bad("invalid_grant", "the authorization code expired");
    }
    if redirect_uri != &stored_redirect {
        return bad(
            "invalid_grant",
            "redirect_uri does not match the authorize request",
        );
    }
    if !tokens::ct_eq(&tokens::pkce_s256(verifier), &challenge) {
        return bad("invalid_grant", "PKCE verification failed");
    }
    // The token request is bound to the authorize request's resource (RFC
    // 8707): omitting it keeps the stored value, sending a different one —
    // including the server default — is a downgrade and fails.
    if let Some(resource) = form.get("resource") {
        let expected = stored_resource.as_ref().unwrap_or(&state.config.resource);
        if resource != expected {
            return bad(
                "invalid_target",
                "resource does not match the authorize request",
            );
        }
    }
    let Some(account_id) = account_id else {
        return bad("invalid_grant", "the authorization was never completed");
    };
    if user_banned(&state.pool, &account_id, now).await {
        return bad("invalid_grant", "the account is no longer active");
    }
    let scope = scope.unwrap_or_else(|| "apps".to_string());
    let resource = stored_resource.unwrap_or_else(|| state.config.resource.clone());
    issue_grant(state, &client_id, &account_id, &scope, &resource, now).await
}

/// Refresh-token reuse: revoke the consent and drop all of its tokens. One
/// transaction, so a concurrent rotation either lands fully before the
/// revoke (and is wiped with everything else) or fails closed after it —
/// never a fresh token minted onto a revoked consent.
async fn revoke_grant_tokens(pool: &sqlx::SqlitePool, grant_id: &str, now: i64) {
    let Ok(mut tx) = pool.begin().await else {
        return;
    };
    let done = sqlx::query(
        "UPDATE oauthConsent SET revokedAt = ?, updatedAt = ? WHERE id = ? AND revokedAt IS NULL",
    )
    .bind(now)
    .bind(now)
    .bind(grant_id)
    .execute(&mut *tx)
    .await
    .is_ok()
        && sqlx::query("DELETE FROM oauthAccessToken WHERE referenceId = ?")
            .bind(grant_id)
            .execute(&mut *tx)
            .await
            .is_ok()
        && sqlx::query("DELETE FROM oauthRefreshToken WHERE referenceId = ?")
            .bind(grant_id)
            .execute(&mut *tx)
            .await
            .is_ok();
    if done {
        tx.commit().await.ok();
    }
}

/// A banned user (or a code/refresh pointing at a deleted user) must stop
/// authenticating. An expired temporary ban no longer counts.
async fn user_banned(pool: &sqlx::SqlitePool, user_id: &str, now: i64) -> bool {
    let row: Option<(i64, Option<i64>)> =
        sqlx::query_as("SELECT banned, banExpires FROM user WHERE id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
    match row {
        None => true,
        Some((banned, expires)) => banned != 0 && expires.is_none_or(|until| now < until),
    }
}

async fn exchange_refresh(
    state: &AuthState,
    headers: &HeaderMap,
    form: HashMap<String, String>,
    now: i64,
) -> Response {
    let bad =
        |code: &str, description: &str| oauth_error(StatusCode::BAD_REQUEST, code, description);
    let unavailable = || {
        oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "the token store is unavailable",
        )
    };
    // Same client binding as the code exchange (RFC 6749 §6): a missing id
    // fails authentication, a wrong one fails the grant. Authentication runs
    // before any state change, so a wrong secret or id never rotates,
    // slides, or revokes someone else's grant.
    let auth = match authenticate_client(state, headers, &form).await {
        Ok(auth) => auth,
        Err(err) => return err.response(),
    };
    let Some(refresh) = form.get("refresh_token") else {
        return bad("invalid_request", "refresh_token is required");
    };
    let hash = tokens::sha256_hex(refresh);
    // The `user` join rejects banned accounts and deleted users (a missing
    // user yields no row: `invalid_grant`, same as an unknown token). The
    // scope ceiling is the refresh row's own scopes: a narrower refresh
    // narrows the token chain, so a later refresh cannot widen it back.
    let row: Option<RefreshRow> = sqlx::query_as(
        "SELECT t.id, t.referenceId, c.userId, c.clientId, t.scopes, c.resources, t.expiresAt, t.rotatedAt, \
                t.rotationReplayResponse, t.rotationReplayExpiresAt, u.banned, u.banExpires \
         FROM oauthRefreshToken t JOIN oauthConsent c ON c.id = t.referenceId \
         JOIN user u ON u.id = c.userId \
         WHERE t.token = ?",
    )
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);
    let Some((
        refresh_row_id,
        grant_id,
        account_id,
        client_id,
        grant_scope,
        grant_resource,
        expires_at,
        rotated_at,
        replay_body,
        replay_expires_at,
        banned,
        ban_expires,
    )) = row
    else {
        return bad("invalid_grant", "the refresh token is unknown");
    };
    if auth.client_id != client_id {
        return bad("invalid_grant", "client_id does not match the grant");
    }
    if banned != 0 && ban_expires.is_none_or(|until| now < until) {
        return bad("invalid_grant", "the account is no longer active");
    }
    // Outer always writes `resources` and `expiresAt`; `None` comes only
    // from a foreign row and expires immediately.
    let grant_resource = grant_resource.unwrap_or_else(|| state.config.resource.clone());
    let expires_at = expires_at.unwrap_or(0);
    // A rotated token is either the loser of a retry race inside the replay
    // grace (public clients: hand back the winner's identical pair) or
    // token theft (revoke the whole grant). Confidential clients never
    // rotate, so a rotated row there is invalid but must not revoke.
    if rotated_at.is_some() {
        if !auth.confidential {
            if let Some(body) = replay_body.as_deref() {
                let in_grace = now <= replay_expires_at.unwrap_or(0);
                if in_grace
                    && body.parse::<serde_json::Value>().is_ok()
                    && !grant_revoked(&state.pool, &grant_id).await
                {
                    return json_text_nostore(body.to_string());
                }
            }
        }
        revoke_grant_tokens(&state.pool, &grant_id, now).await;
        return bad("invalid_grant", "the refresh token was already used");
    }
    if now > expires_at {
        sqlx::query("DELETE FROM oauthRefreshToken WHERE token = ?")
            .bind(&hash)
            .execute(&state.pool)
            .await
            .ok();
        return bad("invalid_grant", "the refresh token expired");
    }
    if grant_revoked(&state.pool, &grant_id).await {
        return bad("invalid_grant", "the grant was revoked");
    }
    let scope = match form.get("scope") {
        Some(requested) => {
            let requested_parts = requested
                .split(' ')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>();
            if requested_parts
                .iter()
                .any(|part| !grant_scope.split(' ').any(|allowed| allowed == *part))
            {
                return bad("invalid_scope", "requested scope exceeds the grant");
            }
            requested.to_string()
        }
        None => grant_scope.clone(),
    };
    let access_ttl_ms = ttl_ms(state.config.access_ttl, 3_600_000);
    let refresh_ttl_ms = ttl_ms(state.config.refresh_ttl, 90 * 86_400_000);
    if auth.confidential {
        // Non-rotating, sliding: the same refresh token stays valid, the
        // row's expiry moves forward and its scopes narrow when the request
        // asked for a subset. No reuse detection (D5: Gemini requires the
        // same token back on every refresh).
        let (access, access_hash) = tokens::new_access_token();
        let Ok(mut tx) = state.pool.begin_with("BEGIN IMMEDIATE").await else {
            return unavailable();
        };
        if !consent_live(&mut tx, &grant_id).await {
            return bad("invalid_grant", "the grant was revoked");
        }
        let slid = sqlx::query(
            "UPDATE oauthRefreshToken SET expiresAt = ?, scopes = ? \
             WHERE token = ? AND rotatedAt IS NULL",
        )
        .bind(now + refresh_ttl_ms)
        .bind(&scope)
        .bind(&hash)
        .execute(&mut *tx)
        .await
        .map(|done| done.rows_affected() == 1)
        .unwrap_or(false);
        let inserted = slid
            && sqlx::query(
                "INSERT INTO oauthAccessToken (id, token, clientId, userId, referenceId, refreshId, scopes, resources, expiresAt, createdAt) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(tokens::new_id())
            .bind(&access_hash)
            .bind(&client_id)
            .bind(&account_id)
            .bind(&grant_id)
            .bind(&refresh_row_id)
            .bind(&scope)
            .bind(&grant_resource)
            .bind(now + access_ttl_ms)
            .bind(now)
            .execute(&mut *tx)
            .await
            .is_ok();
        if !inserted {
            return bad("invalid_grant", "the refresh token is no longer usable");
        }
        if tx.commit().await.is_err() {
            return unavailable();
        }
        touch_client(state, &client_id, now).await;
        return json_nostore(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": access_ttl_ms / 1000,
            "scope": scope,
            "refresh_token": refresh,
        }));
    }
    // Public client: rotate inside one write transaction. The claim UPDATE
    // stores the exact response body it answers with, so a concurrent
    // retry inside the grace window gets that same pair back instead of a
    // revocation. `BEGIN IMMEDIATE` takes the write lock up front: a
    // deferred read-then-write upgrade can fail with `SQLITE_BUSY_SNAPSHOT`
    // before the winner commits.
    let (access, access_hash) = tokens::new_access_token();
    let (refresh, refresh_hash) = tokens::new_refresh_token();
    let response = json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": access_ttl_ms / 1000,
        "scope": scope,
        "refresh_token": refresh,
    })
    .to_string();
    let Ok(mut tx) = state.pool.begin_with("BEGIN IMMEDIATE").await else {
        return unavailable();
    };
    if !consent_live(&mut tx, &grant_id).await {
        return bad("invalid_grant", "the grant was revoked");
    }
    let claimed = sqlx::query(
        "UPDATE oauthRefreshToken SET rotatedAt = ?, rotationReplayResponse = ?, rotationReplayExpiresAt = ? \
         WHERE token = ? AND rotatedAt IS NULL",
    )
    .bind(now)
    .bind(&response)
    .bind(now + REFRESH_REUSE_GRACE_MS)
    .bind(&hash)
    .execute(&mut *tx)
    .await
    .map(|done| done.rows_affected() == 1)
    .unwrap_or(false);
    if !claimed {
        // Release the write lock first: the replay lookup and the revoke
        // use the pool, and would otherwise wait on this transaction.
        tx.rollback().await.ok();
        // The loser of a concurrent refresh must not revoke: the winner
        // committed a replay body for exactly this case.
        if let Some(replay) = replay_after_lost_race(state, &hash, &grant_id, now).await {
            return replay;
        }
        revoke_grant_tokens(&state.pool, &grant_id, now).await;
        return bad("invalid_grant", "the refresh token was already used");
    }
    let refresh_id = tokens::new_id();
    let inserted = sqlx::query(
        "INSERT INTO oauthRefreshToken (id, token, clientId, userId, referenceId, scopes, resources, expiresAt, createdAt) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&refresh_id)
    .bind(&refresh_hash)
    .bind(&client_id)
    .bind(&account_id)
    .bind(&grant_id)
    .bind(&scope)
    .bind(&grant_resource)
    .bind(now + refresh_ttl_ms)
    .bind(now)
    .execute(&mut *tx)
    .await
    .is_ok()
        && sqlx::query(
            "INSERT INTO oauthAccessToken (id, token, clientId, userId, referenceId, refreshId, scopes, resources, expiresAt, createdAt) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(tokens::new_id())
        .bind(&access_hash)
        .bind(&client_id)
        .bind(&account_id)
        .bind(&grant_id)
        .bind(&refresh_id)
        .bind(&scope)
        .bind(&grant_resource)
        .bind(now + access_ttl_ms)
        .bind(now)
        .execute(&mut *tx)
        .await
        .is_ok();
    if !inserted {
        return unavailable();
    }
    if tx.commit().await.is_err() {
        return unavailable();
    }
    touch_client(state, &client_id, now).await;
    json_text_nostore(response)
}

/// Re-checks the consent inside the write transaction: a revoke that landed
/// first must refuse the refresh even if the token row is still there.
async fn consent_live(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, grant_id: &str) -> bool {
    let live: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT revokedAt FROM oauthConsent WHERE id = ?")
            .bind(grant_id)
            .fetch_optional(&mut **tx)
            .await
            .unwrap_or(None);
    live.is_some_and(|(revoked,)| revoked.is_none())
}

async fn touch_client(state: &AuthState, client_id: &str, now: i64) {
    sqlx::query("UPDATE oauthClient SET lastTokenAt = ? WHERE clientId = ?")
        .bind(now)
        .bind(client_id)
        .execute(&state.pool)
        .await
        .ok();
}

/// The winner of a concurrent refresh commits the response body it answered
/// with on the old row. Read it back (polling briefly in case the commit is
/// still landing) and answer with it verbatim. A missing or expired body
/// returns `None`, and the caller treats the token as reused.
async fn replay_after_lost_race(
    state: &AuthState,
    hash: &str,
    grant_id: &str,
    now: i64,
) -> Option<Response> {
    for attempt in 0..REPLAY_POLL_ATTEMPTS {
        let row: Option<(Option<String>, Option<i64>)> = sqlx::query_as(
            "SELECT rotationReplayResponse, rotationReplayExpiresAt FROM oauthRefreshToken WHERE token = ?",
        )
        .bind(hash)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
        if let Some((Some(body), Some(expires_at))) = row {
            if now <= expires_at
                && body.parse::<serde_json::Value>().is_ok()
                && !grant_revoked(&state.pool, grant_id).await
            {
                return Some(json_text_nostore(body));
            }
            return None;
        }
        if attempt + 1 < REPLAY_POLL_ATTEMPTS {
            tokio::time::sleep(REPLAY_POLL_INTERVAL).await;
        }
    }
    None
}

async fn grant_revoked(pool: &SqlitePool, grant_id: &str) -> bool {
    sqlx::query_as::<_, (Option<i64>,)>("SELECT revokedAt FROM oauthConsent WHERE id = ?")
        .bind(grant_id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None)
        .is_none_or(|(revoked,)| revoked.is_some())
}

/// Mint a grant plus access token (and a refresh token when the scope asks
/// for `offline_access`).
async fn issue_grant(
    state: &AuthState,
    client_id: &str,
    account_id: &str,
    scope: &str,
    resource: &str,
    now: i64,
) -> Response {
    let grant_id = tokens::new_id();
    sqlx::query(
        "INSERT INTO oauthConsent (id, clientId, userId, scopes, resources, createdAt, updatedAt) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&grant_id)
    .bind(client_id)
    .bind(account_id)
    .bind(scope)
    .bind(resource)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .ok();
    let (access, access_hash) = tokens::new_access_token();
    let access_ttl_ms = ttl_ms(state.config.access_ttl, 3_600_000);
    let mut response = json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": access_ttl_ms / 1000,
        "scope": scope,
    });
    let mut refresh_id: Option<String> = None;
    if scope.split(' ').any(|part| part == "offline_access") {
        let (refresh, refresh_hash) = tokens::new_refresh_token();
        let refresh_ttl_ms = ttl_ms(state.config.refresh_ttl, 90 * 86_400_000);
        let id = tokens::new_id();
        sqlx::query(
            "INSERT INTO oauthRefreshToken (id, token, clientId, userId, referenceId, scopes, resources, expiresAt, createdAt) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&refresh_hash)
        .bind(client_id)
        .bind(account_id)
        .bind(&grant_id)
        .bind(scope)
        .bind(resource)
        .bind(now + refresh_ttl_ms)
        .bind(now)
        .execute(&state.pool)
        .await
        .ok();
        response["refresh_token"] = json!(refresh);
        refresh_id = Some(id);
    }
    sqlx::query(
        "INSERT INTO oauthAccessToken (id, token, clientId, userId, referenceId, refreshId, scopes, resources, expiresAt, createdAt) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(tokens::new_id())
    .bind(&access_hash)
    .bind(client_id)
    .bind(account_id)
    .bind(&grant_id)
    .bind(&refresh_id)
    .bind(scope)
    .bind(resource)
    .bind(now + access_ttl_ms)
    .bind(now)
    .execute(&state.pool)
    .await
    .ok();
    sqlx::query("UPDATE oauthClient SET lastTokenAt = ? WHERE clientId = ?")
        .bind(now)
        .bind(client_id)
        .execute(&state.pool)
        .await
        .ok();
    json_nostore(response)
}

fn ttl_ms(ttl: std::time::Duration, fallback_ms: i64) -> i64 {
    let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
    if ms > 0 {
        ms
    } else {
        fallback_ms
    }
}

/// `POST /oauth/revoke` (RFC 7009): always 200, even for unknown tokens.
async fn revoke(
    State(state): State<AuthState>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(token) = form.get("token") {
        let hash = tokens::sha256_hex(token.trim());
        let mut grant: Option<(String,)> =
            sqlx::query_as("SELECT referenceId FROM oauthAccessToken WHERE token = ?")
                .bind(&hash)
                .fetch_optional(&state.pool)
                .await
                .unwrap_or(None);
        if grant.is_none() {
            grant = sqlx::query_as("SELECT referenceId FROM oauthRefreshToken WHERE token = ?")
                .bind(&hash)
                .fetch_optional(&state.pool)
                .await
                .unwrap_or(None);
        }
        if let Some((grant_id,)) = grant {
            revoke_grant_tokens(&state.pool, &grant_id, now_ms()).await;
        } else {
            sqlx::query(
                "UPDATE apikey SET enabled = 0, updatedAt = ? WHERE key = ? AND enabled != 0",
            )
            .bind(now_ms())
            .bind(&hash)
            .execute(&state.pool)
            .await
            .ok();
        }
    }
    (
        StatusCode::OK,
        [("cache-control", "no-store"), ("pragma", "no-cache")],
        "",
    )
        .into_response()
}

/// Delete expired authorization leftovers (the daily prune): unclaimed
/// `oauth_request` rows past `expires_at` (claimed ones are deleted by the
/// exchange), expired token rows in `oauthAccessToken`/`oauthRefreshToken`
/// (also deleted opportunistically on every token call), and unverified DCR
/// clients with no token issued in 30 days (also deleted opportunistically
/// on register). Operator-created clients are `verified = 1`, so they never
/// age out. Only rows no reader can still accept go (`expiresAt < now`,
/// matching the `now > expiresAt` checks).
/// Returns `(requests, tokens, stale_clients)` removed.
pub async fn prune_expired(pool: &sqlx::SqlitePool, now: i64) -> anyhow::Result<(u64, u64, u64)> {
    let requests = sqlx::query("DELETE FROM oauth_request WHERE expires_at < ?")
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();
    let access = sqlx::query("DELETE FROM oauthAccessToken WHERE expiresAt < ?")
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();
    let refresh = sqlx::query("DELETE FROM oauthRefreshToken WHERE expiresAt < ?")
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();
    let stale_clients = sqlx::query(
        "DELETE FROM oauthClient WHERE kind = 'dcr' AND verified = 0 AND lastTokenAt IS NULL AND createdAt < ?",
    )
    .bind(now - STALE_DCR_MS)
    .execute(pool)
    .await?
    .rows_affected();
    Ok((requests, access + refresh, stale_clients))
}

/// The `outer oauth-client add` command: register a confidential client for
/// a directory that takes a client ID and secret out of band (Gemini's
/// console fields, for example) instead of using DCR. Same redirect rules as
/// DCR. The row is a `dcr`-kind confidential client marked `verified = 1`:
/// the operator vouched for its name, and the stale-DCR prune skips it.
/// Returns `(client_id, secret)`; only the secret's hash is stored.
pub async fn add_operator_client(
    pool: &SqlitePool,
    name: &str,
    redirect_uris: &[String],
    auth_method: &str,
) -> anyhow::Result<(String, String)> {
    if redirect_uris.is_empty() {
        anyhow::bail!("at least one --redirect-uri is required");
    }
    anyhow::ensure!(
        matches!(auth_method, "client_secret_basic" | "client_secret_post"),
        "unknown auth method {auth_method:?}: want client_secret_basic or client_secret_post"
    );
    let mut validated = Vec::with_capacity(redirect_uris.len());
    for uri in redirect_uris {
        validated.push(validated_redirect(uri).map_err(anyhow::Error::msg)?);
    }
    let now = now_ms();
    let name = name.trim().chars().take(200).collect::<String>();
    anyhow::ensure!(!name.is_empty(), "--name must not be empty");
    let host = validated
        .first()
        .and_then(|uri| Url::parse(uri).ok())
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default();
    let client_id = format!("dcr_{}", hex::encode(rand::random::<[u8; 16]>()));
    let (secret, secret_hash) = tokens::new_client_secret();
    sqlx::query(
        "INSERT INTO oauthClient (id, clientId, kind, name, host, redirectUris, verified, clientSecret, tokenEndpointAuthMethod, createdAt, updatedAt) \
         VALUES (?, ?, 'dcr', ?, ?, ?, 1, ?, ?, ?, ?)",
    )
    .bind(tokens::new_id())
    .bind(&client_id)
    .bind(&name)
    .bind(&host)
    .bind(serde_json::to_string(&validated).unwrap_or_default())
    .bind(&secret_hash)
    .bind(auth_method)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok((client_id, secret))
}
