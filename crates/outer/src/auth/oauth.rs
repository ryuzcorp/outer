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

pub fn router(state: AuthState) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize_get).post(authorize_post))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .with_state(state)
}

fn json_nostore(value: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        [
            ("cache-control", "no-store"),
            ("pragma", "no-cache"),
            ("content-type", "application/json"),
        ],
        Json(value).0.to_string(),
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
        "token_endpoint_auth_methods_supported": ["none"],
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
/// One refresh token row: consent id, userId, clientId, scopes, resources,
/// expiresAt, rotatedAt, the user's banned flag and ban expiry.
/// `resources`/`expiresAt` arrive `None` only from rows
/// Outer never writes (it always sets both); `None` expires immediately.
type RefreshRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<i64>,
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
    let _ = request_ip(&headers, state.config.origin_locked, Some(peer));
    let now = now_ms();
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
        pages::email_page(&client, &id, &csrf, None),
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
            match super::send_code(&state.pool, &state.config, &email, ip.as_deref()).await {
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
                    html(
                        status,
                        pages::email_page(
                            &client,
                            &pending.id,
                            &pending.csrf,
                            Some(pages::friendly_error(err.code)),
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
                    StatusCode::BAD_REQUEST,
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

/// `POST /oauth/register` (RFC 7591): public clients only, HTTPS or loopback
/// redirects, 20/h per IP. Prunes registrations with no token for 30 days.
async fn register(
    State(state): State<AuthState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let now = now_ms();
    let ip = request_ip(&headers, state.config.origin_locked, Some(peer))
        .unwrap_or_else(|| "unknown".to_string());
    if let Some(retry_after_s) = limits::check_and_record(
        &state.pool,
        &format!("ip:{ip}"),
        "dcr_register",
        20,
        3_600_000,
        now,
    )
    .await
    .unwrap_or(None)
    {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("retry-after", retry_after_s.to_string().parse().unwrap());
        return (
            StatusCode::TOO_MANY_REQUESTS,
            headers,
            Json(json!({ "error": "rate_limited", "error_description": "too many registrations" }))
                .0
                .to_string(),
        )
            .into_response();
    }
    sqlx::query(
        "DELETE FROM oauthClient WHERE kind = 'dcr' AND lastTokenAt IS NULL AND createdAt < ?",
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
        let Ok(parsed) = Url::parse(uri) else {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                format!("{uri} is not a URL").as_str(),
            );
        };
        if !dcr_redirect_allowed(&parsed) {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                "redirect URIs must be https, loopback http, or an app's own scheme (cursor://, vscode://, …)",
            );
        }
        redirect_uris.push(uri.to_string());
    }
    // Every client is registered public (`none`) whatever it asked for:
    // RFC 7591 lets the server answer with the metadata it actually
    // registered, PKCE protects the code, and the token endpoint ignores a
    // client secret or Basic header a client sends anyway. Refusing a
    // client that defaults to `client_secret_basic` would lock it out.
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
    sqlx::query(
        "INSERT INTO oauthClient (id, clientId, kind, name, host, redirectUris, verified, createdAt, updatedAt) \
         VALUES (?, ?, 'dcr', ?, ?, ?, 0, ?, ?)",
    )
    .bind(tokens::new_id())
    .bind(&client_id)
    .bind(&name)
    .bind(&host)
    .bind(serde_json::to_string(&redirect_uris).unwrap_or_default())
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .ok();
    let mut response = json!({
        "client_id": client_id,
        "redirect_uris": redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if let Some(name) = name {
        response["client_name"] = json!(name);
    }
    (
        StatusCode::CREATED,
        [("content-type", "application/json")],
        Json(response).0.to_string(),
    )
        .into_response()
}

/// `POST /oauth/token` (form): authorization_code (single use, 60 s TTL,
/// PKCE S256 required, `client_id`/redirect_uri/`resource` bound to the
/// authorize request) and refresh_token (rotated every use in one
/// transaction, 90-day sliding; reusing a rotated token revokes the whole
/// grant). Proper OAuth JSON errors + `Cache-Control: no-store`.
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
        Some("authorization_code") => exchange_code(&state, form, now).await,
        Some("refresh_token") => exchange_refresh(&state, form, now).await,
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "grant_type must be authorization_code or refresh_token",
        ),
    }
}

async fn exchange_code(state: &AuthState, form: HashMap<String, String>, now: i64) -> Response {
    let bad =
        |code: &str, description: &str| oauth_error(StatusCode::BAD_REQUEST, code, description);
    // Public clients authenticate by `client_id` alone (RFC 6749 §4.1.3,
    // RFC 8414: `token_endpoint_auth_methods_supported: ["none"]`), so a
    // missing id is an authentication failure, not a bad grant.
    let Some(form_client) = form.get("client_id").filter(|id| !id.is_empty()) else {
        return bad("invalid_client", "client_id is required for public clients");
    };
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

async fn exchange_refresh(state: &AuthState, form: HashMap<String, String>, now: i64) -> Response {
    let bad =
        |code: &str, description: &str| oauth_error(StatusCode::BAD_REQUEST, code, description);
    let unavailable = || {
        oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "the token store is unavailable",
        )
    };
    // Same public-client binding as the code exchange (RFC 6749 §6): a
    // missing id fails authentication, a wrong one fails the grant. Both
    // checks run before any state change, so a mismatched id never revokes
    // someone else's grant.
    let Some(form_client) = form.get("client_id").filter(|id| !id.is_empty()) else {
        return bad("invalid_client", "client_id is required for public clients");
    };
    let Some(refresh) = form.get("refresh_token") else {
        return bad("invalid_request", "refresh_token is required");
    };
    let hash = tokens::sha256_hex(refresh);
    // The `user` join rejects banned accounts and deleted users (a missing
    // user yields no row: `invalid_grant`, same as an unknown token).
    let row: Option<RefreshRow> = sqlx::query_as(
        "SELECT t.referenceId, c.userId, c.clientId, c.scopes, c.resources, t.expiresAt, t.rotatedAt, \
                u.banned, u.banExpires \
         FROM oauthRefreshToken t JOIN oauthConsent c ON c.id = t.referenceId \
         JOIN user u ON u.id = c.userId \
         WHERE t.token = ?",
    )
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);
    let Some((
        grant_id,
        account_id,
        client_id,
        grant_scope,
        grant_resource,
        expires_at,
        rotated_at,
        banned,
        ban_expires,
    )) = row
    else {
        return bad("invalid_grant", "the refresh token is unknown");
    };
    if form_client != &client_id {
        return bad("invalid_grant", "client_id does not match the grant");
    }
    if banned != 0 && ban_expires.is_none_or(|until| now < until) {
        return bad("invalid_grant", "the account is no longer active");
    }
    // Outer always writes `resources` and `expiresAt`; `None` comes only
    // from a foreign row and expires immediately.
    let grant_resource = grant_resource.unwrap_or_else(|| state.config.resource.clone());
    let expires_at = expires_at.unwrap_or(0);
    // Reusing a rotated refresh token revokes the whole grant.
    if rotated_at.is_some() {
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
    // Rotate inside one transaction: re-check the consent is live, claim the
    // old token, then mint the new pair. The claim UPDATE takes the write
    // lock, so a concurrent reuse either lands fully before this commit (and
    // the reuse path revokes everything, this pair included) or blocks until
    // after it and sees `rotatedAt` set. Either way no fresh token survives
    // on a revoked consent — and `resolve_bearer` requires
    // `revokedAt IS NULL` as a second barrier.
    let (access, access_hash) = tokens::new_access_token();
    let (refresh, refresh_hash) = tokens::new_refresh_token();
    let access_ttl_ms = ttl_ms(state.config.access_ttl, 3_600_000);
    let refresh_ttl_ms = ttl_ms(state.config.refresh_ttl, 90 * 86_400_000);
    let Ok(mut tx) = state.pool.begin().await else {
        return unavailable();
    };
    let live: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT revokedAt FROM oauthConsent WHERE id = ?")
            .bind(&grant_id)
            .fetch_optional(&mut *tx)
            .await
            .unwrap_or(None);
    if live.is_none_or(|(revoked,)| revoked.is_some()) {
        return bad("invalid_grant", "the grant was revoked");
    }
    let claimed = sqlx::query(
        "UPDATE oauthRefreshToken SET rotatedAt = ? WHERE token = ? AND rotatedAt IS NULL",
    )
    .bind(now)
    .bind(&hash)
    .execute(&mut *tx)
    .await
    .map(|done| done.rows_affected() == 1)
    .unwrap_or(false);
    if !claimed {
        tx.rollback().await.ok();
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
    if !inserted || tx.commit().await.is_err() {
        return unavailable();
    }
    sqlx::query("UPDATE oauthClient SET lastTokenAt = ? WHERE clientId = ?")
        .bind(now)
        .bind(&client_id)
        .execute(&state.pool)
        .await
        .ok();
    json_nostore(json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": access_ttl_ms / 1000,
        "scope": scope,
        "refresh_token": refresh,
    }))
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
/// (also deleted opportunistically on every token call), and DCR clients
/// with no token issued in 30 days (also deleted opportunistically on
/// register). Only rows no reader can still accept go (`expiresAt < now`,
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
        "DELETE FROM oauthClient WHERE kind = 'dcr' AND lastTokenAt IS NULL AND createdAt < ?",
    )
    .bind(now - STALE_DCR_MS)
    .execute(pool)
    .await?
    .rows_affected();
    Ok((requests, access + refresh, stale_clients))
}
