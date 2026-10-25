//! Console account pages (spec §13.1, wave 2): connections + API keys,
//! usage + plan, and account hygiene. New endpoints live here (not in
//! `console.rs`); `console.rs` only gains the `.merge` line.
//!
//! Rules (same as `CONSOLE-ACTIONS-CONTRACT.md`): session cookie only,
//! `Sec-Fetch-Site: same-origin`, every query scoped to the signed-in
//! account, the SAME domain functions the MCP tools call
//! ([`crate::accounts`], [`crate::auth`]), `account_action` audit rows
//! with the MCP tool name and client "Console", `{error, message,
//! user_message}` errors, and daisyUI `modal` confirms in the UI.
//!
//! The console itself never appears in these lists: it signs in with a
//! `__Host-outer_sid` web session (the `session` table), not an OAuth
//! grant and not an API key, so revoking connections or keys can never
//! sign this browser out. The list responses say so explicitly.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::json;

use super::console::{guard, nostore_json, ok_json};
use super::console_actions::tool_response;
use crate::accounts::{self, AccountError};
use crate::auth::{self, now_ms};

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/api/console/account", get(api_account))
        .route("/api/console/alerts/prefs", post(api_update_prefs))
        .route(
            "/console/alerts/unsubscribe",
            get(unsubscribe_confirm).post(unsubscribe_apply),
        )
        .route("/api/console/connections", get(api_connections))
        .route(
            "/api/console/connections/{id}/revoke",
            post(api_revoke_connection),
        )
        .route(
            "/api/console/api-keys",
            get(api_list_keys).post(api_create_key),
        )
        .route("/api/console/api-keys/revoke", post(api_revoke_key))
        .route(
            "/api/console/sessions/sign-out-others",
            post(api_sign_out_others),
        )
        .route("/api/console/account/delete", post(api_delete_account))
}

/// Account errors through the shared console envelope ([`tool_response`]):
/// `{error, message, user_message?}` with the console's `no-store` rules.
fn err_json(err: AccountError) -> Response {
    tool_response(&err.into())
}

/// The audit outcome for a finished mutation: `ok`, or `error:<code>`.
fn outcome_of(
    result: &Result<serde_json::Value, AccountError>,
) -> (&'static str, Option<&'static str>) {
    match result {
        Ok(_) => ("ok", None),
        Err(err) => ("error", Some(err.code)),
    }
}

/// `GET /api/console/account`: email, plan, member since, and usage
/// against the same limits the quota checks enforce (single source of
/// truth: [`accounts::account_summary`]).
async fn api_account(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    match accounts::account_summary(&mcp.pool, &account.id).await {
        Ok(summary) => match crate::alerts::get_prefs(&mcp.pool, &account.id).await {
            Ok(prefs) => ok_json(json!({
                "email": summary.email,
                "plan": summary.plan,
                "plan_status": summary.plan_status,
                "member_since": summary.member_since,
                "usage": {
                    "apps_owned": summary.apps_owned,
                    "apps_limit": summary.apps_limit,
                    "code_changes_today": summary.code_changes_today,
                    "code_changes_limit": summary.code_changes_limit,
                    "build_ms_today": summary.build_ms_today,
                    "rollbacks": summary.rollbacks,
                    "rollback_limit": summary.rollback_limit,
                    "requests_month": summary.requests_month,
                    "storage_peak_bytes": summary.storage_peak_bytes,
                    "egress_bytes_month": summary.egress_bytes_month,
                },
                "alerts": {
                    "deploy_failed": prefs.deploy_failed,
                    "error_spike": prefs.error_spike,
                    "quota": prefs.quota,
                },
                "resets_at": summary.resets_at,
            })),
            Err(err) => err_json(AccountError::internal(format!("{err:#}"))),
        },
        Err(err) => err_json(err),
    }
}

/// `GET /api/console/connections`: the caller's OAuth grants (same rows
/// as MCP `list_connections`). `console_session` names what this browser
/// uses — a web session, never a grant — so the UI can say revoking here
/// never signs this browser out.
async fn api_connections(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    match auth::list_connections(&mcp.pool, &account.id).await {
        Ok(grants) => ok_json(json!({
            "console_session": "web",
            "connections": grants.iter().map(|g| json!({
                "id": g.grant_id,
                "client_id": g.client_id,
                "client_name": g.client_name,
                "host": g.client_host,
                "scope": g.scope,
                "created_at": g.created_at,
                "last_used_at": g.last_used_at,
            })).collect::<Vec<_>>(),
        })),
        Err(err) => err_json(AccountError::internal(format!("{err:#}"))),
    }
}

/// `POST /api/console/connections/{id}/revoke`: same domain function as
/// MCP `revoke_connection`. A foreign or unknown id 404s exactly like a
/// missing one; the console's own web session is not a grant and is
/// unaffected by construction.
async fn api_revoke_connection(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let started = now_ms();
    let id = id.chars().take(200).collect::<String>();
    let result: Result<serde_json::Value, AccountError> = async {
        let revoked = auth::revoke_connection(&mcp.pool, &account.id, &id).await?;
        if !revoked {
            return Err(AccountError::not_found(format!("no connection {id:?}")));
        }
        Ok(json!({"revoked": true}))
    }
    .await;
    let (outcome, error_code) = outcome_of(&result);
    accounts::record_account_action(
        &mcp.pool,
        &account.id,
        "revoke_connection",
        &format!("revoke_connection {id}"),
        outcome,
        error_code,
        started,
    )
    .await;
    match result {
        Ok(body) => ok_json(body),
        Err(err) => err_json(err),
    }
}

/// `GET /api/console/api-keys`: the caller's live keys (same rows as MCP
/// `list_api_keys`; secrets never leave the server).
async fn api_list_keys(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    match auth::list_api_keys(&mcp.pool, &account.id).await {
        Ok(keys) => ok_json(json!({
            "console_session": "web",
            "keys": keys.iter().map(|k| json!({
                "id": k.id,
                "name": k.name,
                "prefix": k.prefix,
                "created_at": k.created_at,
                "last_used_at": k.last_used_at,
                "client": match (&k.client_name, &k.client_version) {
                    (Some(name), Some(version)) => Some(format!("{name}/{version}")),
                    (Some(name), None) => Some(name.clone()),
                    _ => None,
                },
            })).collect::<Vec<_>>(),
        })),
        Err(err) => err_json(AccountError::internal(format!("{err:#}"))),
    }
}

#[derive(Debug, Deserialize)]
struct CreateKey {
    name: Option<String>,
}

/// `POST /api/console/api-keys {name?}`: same domain function as MCP
/// `create_api_key`. The key is returned once; the UI shows it in a modal
/// with copy + "you won't see it again".
async fn api_create_key(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    body: axum::Json<CreateKey>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if name.map(|name| name.chars().count() > 100).unwrap_or(false) {
        return err_json(AccountError::invalid_argument(
            "name is longer than 100 characters".to_string(),
            "Keep the key name under 100 characters.".to_string(),
        ));
    }
    let started = now_ms();
    let result: Result<serde_json::Value, AccountError> = async {
        let key = auth::create_api_key(&mcp.pool, &account.id, name).await?;
        Ok(json!({"id": key.id, "name": name, "prefix": key.prefix, "api_key": key.key}))
    }
    .await;
    let (outcome, error_code) = outcome_of(&result);
    accounts::record_account_action(
        &mcp.pool,
        &account.id,
        "create_api_key",
        &format!("create_api_key {}", name.unwrap_or("(unnamed)")),
        outcome,
        error_code,
        started,
    )
    .await;
    match result {
        Ok(body) => ok_json(body),
        Err(err) => err_json(err),
    }
}

#[derive(Debug, Deserialize)]
struct RevokeKey {
    prefix: Option<String>,
}

/// `POST /api/console/api-keys/revoke {prefix}`: same domain function as
/// MCP `revoke_api_key`. Unknown prefixes 404; the console's web session
/// is not a key and is unaffected by construction.
async fn api_revoke_key(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    body: axum::Json<RevokeKey>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let Some(prefix) = body
        .prefix
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    else {
        return err_json(AccountError::invalid_argument(
            "prefix is required".to_string(),
            "Pick which key to revoke.".to_string(),
        ));
    };
    let prefix: String = prefix.chars().take(100).collect();
    let started = now_ms();
    let result: Result<serde_json::Value, AccountError> = async {
        let revoked = auth::revoke_api_key(&mcp.pool, &account.id, &prefix).await?;
        if !revoked {
            return Err(AccountError::not_found(format!(
                "no API key with prefix {prefix:?}"
            )));
        }
        Ok(json!({"revoked": true}))
    }
    .await;
    let (outcome, error_code) = outcome_of(&result);
    accounts::record_account_action(
        &mcp.pool,
        &account.id,
        "revoke_api_key",
        &format!("revoke_api_key {prefix}"),
        outcome,
        error_code,
        started,
    )
    .await;
    match result {
        Ok(body) => ok_json(body),
        Err(err) => err_json(err),
    }
}

/// `POST /api/console/sessions/sign-out-others`: delete every `session`
/// row for this account except the current one. The current session hash
/// never reaches storage raw — only its sha256 does.
async fn api_sign_out_others(State(state): State<super::AppState>, headers: HeaderMap) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let Some(session) = auth::session_from_headers(&headers) else {
        return nostore_json(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "sign in at /console/sign-in",
        );
    };
    let started = now_ms();
    let result: Result<serde_json::Value, AccountError> = async {
        let signed_out =
            accounts::sign_out_others(&mcp.pool, &account.id, &auth::session_hash(&session))
                .await?;
        Ok(json!({"signed_out": signed_out, "kept": 1}))
    }
    .await;
    let (outcome, error_code) = outcome_of(&result);
    accounts::record_account_action(
        &mcp.pool,
        &account.id,
        "sign_out_others",
        "sign_out_others",
        outcome,
        error_code,
        started,
    )
    .await;
    match result {
        Ok(body) => ok_json(body),
        Err(err) => err_json(err),
    }
}

#[derive(Debug, Deserialize)]
struct DeleteAccount {
    confirm: Option<String>,
}

/// `POST /api/console/account/delete {confirm}`: the `delete_account`
/// domain path — `confirm` must equal the account email exactly. On
/// success the session cookie is cleared (all sessions are gone) and the
/// browser goes back through sign-in.
async fn api_delete_account(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    body: axum::Json<DeleteAccount>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let Some(confirm) = body
        .confirm
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    else {
        return err_json(AccountError::invalid_argument(
            "confirm is required".to_string(),
            "To delete your account, type your email address exactly.".to_string(),
        ));
    };
    let confirm: String = confirm.chars().take(320).collect();
    let deps = accounts::DeleteAccountDeps {
        pool: &mcp.pool,
        store: &mcp.store,
        git: &mcp.git,
        sync: &mcp.sync,
        fleets: &mcp.fleets,
        deploys: &mcp.deploys,
        rollback_locks: &mcp.rollback_locks,
        delete_cfg: &mcp.delete_cfg,
    };
    match accounts::delete_account(&deps, &account.id, &confirm).await {
        Ok(out) => (
            StatusCode::OK,
            [
                ("cache-control", "no-store"),
                ("content-type", "application/json"),
                ("set-cookie", &auth::clear_session_cookie() as &str),
            ],
            json!({
                "deleted": true,
                "email": out.email,
                "deleted_apps": out.deleted_apps,
                "signed_out_sessions": out.signed_out_sessions,
            })
            .to_string(),
        )
            .into_response(),
        Err(err) => err_json(err),
    }
}

#[derive(Debug, Deserialize)]
struct PrefsBody {
    deploy_failed: Option<bool>,
    error_spike: Option<bool>,
    quota: Option<bool>,
}

/// `POST /api/console/alerts/prefs {deploy_failed?, error_spike?,
/// quota?}`: flip the caller's email-alert toggles. At least one toggle
/// must be present; absent toggles stay as they are. Audited as
/// `update_alert_prefs` like every other account mutation.
async fn api_update_prefs(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    body: axum::Json<PrefsBody>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let started = now_ms();
    if body.deploy_failed.is_none() && body.error_spike.is_none() && body.quota.is_none() {
        return err_json(AccountError::invalid_argument(
            "no toggles provided".to_string(),
            "Pass at least one of deploy_failed, error_spike or quota.".to_string(),
        ));
    }
    let patch = crate::alerts::PrefsPatch {
        deploy_failed: body.deploy_failed,
        error_spike: body.error_spike,
        quota: body.quota,
    };
    let result: Result<serde_json::Value, AccountError> = async {
        let prefs = crate::alerts::set_prefs(&mcp.pool, &account.id, &patch, started)
            .await
            .map_err(|err| AccountError::internal(format!("{err:#}")))?;
        Ok(json!({
            "deploy_failed": prefs.deploy_failed,
            "error_spike": prefs.error_spike,
            "quota": prefs.quota,
        }))
    }
    .await;
    let (outcome, error_code) = outcome_of(&result);
    accounts::record_account_action(
        &mcp.pool,
        &account.id,
        "update_alert_prefs",
        &format!(
            "update_alert_prefs deploy_failed={:?} error_spike={:?} quota={:?}",
            body.deploy_failed, body.error_spike, body.quota
        ),
        outcome,
        error_code,
        started,
    )
    .await;
    match result {
        Ok(body) => ok_json(body),
        Err(err) => err_json(err),
    }
}

/// `GET /console/alerts/unsubscribe?token=`: no login, and **never**
/// changes state. Link scanners and email-client prefetchers open every
/// URL in incoming mail, so applying the opt-out here would silently turn
/// alerts off without any user action. A valid token renders a
/// confirmation page whose form POSTs back; anything else explains the
/// link is not valid. Always 200 HTML: there is no session to challenge.
async fn unsubscribe_confirm(
    State(state): State<super::AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let issuer = state.mcp.auth.config.issuer.clone();
    let key = state.mcp.unsub_key.clone();
    let token = query.get("token").cloned().unwrap_or_default();
    let manage = format!("{}/console?t=account", issuer.trim_end_matches('/'));
    let Some((_, scope)) = crate::alerts::verify_unsubscribe(&key, &token) else {
        return unsubscribe_page(
            "Link not valid",
            "This unsubscribe link is not valid. It may be from an older email — \
             manage email alerts in the console under Account.",
            &manage,
        );
    };
    unsubscribe_page(
        "Confirm unsubscribe",
        &format!(
            "Click below to stop getting {} emails from Outer. \
             Change your mind anytime in the console under Account. \
             <form method=\"post\" action=\"/console/alerts/unsubscribe\"> \
             <input type=\"hidden\" name=\"token\" value=\"{token}\"> \
             <button type=\"submit\">Turn off {} emails</button></form>",
            scope_label(&scope),
            scope_label(&scope),
        ),
        &manage,
    )
}

/// `POST /console/alerts/unsubscribe`: applies the opt-out the GET page
/// confirmed. Accepts the token from the form body or the query string so
/// RFC 8058 one-click unsubscribes (`List-Unsubscribe-Post:
/// List-Unsubscribe=One-Click`) succeed without a browser. The body is
/// read as raw bytes and scanned for `token=` rather than via `Form`:
/// one-click POSTs carry no `token` field at all, and our tokens are
/// URL-safe base64url so no percent-decoding is needed. Always 200 HTML,
/// same as the confirmation page.
async fn unsubscribe_apply(
    State(state): State<super::AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    body: bytes::Bytes,
) -> Response {
    let issuer = state.mcp.auth.config.issuer.clone();
    let key = state.mcp.unsub_key.clone();
    let manage = format!("{}/console?t=account", issuer.trim_end_matches('/'));
    let form_token = std::str::from_utf8(&body).ok().and_then(|text| {
        text.split('&').find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name == "token").then(|| value.to_string())
        })
    });
    let token = form_token
        .or_else(|| query.get("token").cloned())
        .unwrap_or_default();
    let Some((user_id, scope)) = crate::alerts::verify_unsubscribe(&key, &token) else {
        return unsubscribe_page(
            "Link not valid",
            "This unsubscribe link is not valid. It may be from an older email — \
             manage email alerts in the console under Account.",
            &manage,
        );
    };
    let started = now_ms();
    match crate::alerts::apply_unsubscribe(&state.mcp.pool, &user_id, &scope, started).await {
        Ok(_) => {
            accounts::record_account_action(
                &state.mcp.pool,
                &user_id,
                "update_alert_prefs",
                &format!("unsubscribe {scope} via email link"),
                "ok",
                None,
                started,
            )
            .await;
            unsubscribe_page(
                "Unsubscribed",
                &format!(
                    "You will no longer get {} emails from Outer. \
                     Change your mind anytime in the console under Account.",
                    scope_label(&scope),
                ),
                &manage,
            )
        }
        Err(_) => unsubscribe_page(
            "Something went wrong",
            "Outer could not update your email settings. Try again from the console under Account.",
            &manage,
        ),
    }
}

/// The shared 200-HTML shell for the unsubscribe pages (`message` may
/// carry the confirmation form; it is server-rendered from a verified
/// token, never from request text).
fn unsubscribe_page(title: &str, message: &str, manage: &str) -> Response {
    (
        StatusCode::OK,
        [
            ("cache-control", "no-store"),
            ("content-type", "text/html; charset=utf-8"),
        ],
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <title>{title} — Outer</title></head>\
             <body style=\"font-family: system-ui, sans-serif; max-width: 36rem; margin: 4rem auto; padding: 0 1rem;\">\
             <h1>{title}</h1><p>{message}</p>\
             <p><a href=\"{manage}\">Manage email alerts in the console</a></p>\
             </body></html>"
        ),
    )
        .into_response()
}

/// Human label for an unsubscribe scope, for the confirmation page and
/// (through [`crate::alerts`]) the email footers.
fn scope_label(scope: &str) -> &'static str {
    match scope {
        crate::alerts::KIND_DEPLOY_FAILED => "deploy failure",
        crate::alerts::KIND_ERROR_SPIKE => "error spike",
        crate::alerts::SCOPE_ALL => "alert",
        _ => "quota",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::SqlitePool;
    use tower::ServiceExt as _;

    type HttpApp = axum::Router;
    async fn setup() -> (HttpApp, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        std::mem::forget(dir);
        let (auth_cfg, _) = crate::auth::test_support::test_config();
        let auth = crate::auth::AuthState {
            pool: pool.clone(),
            config: auth_cfg,
        };
        let mcp = crate::mcp::McpState::for_tests(pool.clone(), auth.clone());
        vendor_template(&mcp.store).await;
        let app = super::super::router(crate::http::AppState {
            pool: pool.clone(),
            sync: crate::store::snapshot::StateSync::new("test"),
            auth,
            mcp,
            site: crate::http::site::SiteDir::new(std::path::Path::new("/nonexistent")),
        });
        (app, pool)
    }

    /// A tiny stand-in for the vendored Ilha template (mirrors
    /// `crate::mcp::tests::vendor_template`, which is private to that
    /// module): three files, one commit, bundled into the memory store.
    async fn vendor_template(store: &crate::store::Store) {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("pages")).unwrap();
        std::fs::write(src.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(src.path().join("pages/index.tsx"), "<h1>Test</h1>\n").unwrap();
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
        crate::templates::put_test_template(
            store,
            crate::templates::Template::App,
            sha.trim(),
            &bundle,
        )
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

    async fn account(pool: &SqlitePool, email: &str) -> String {
        crate::apps::ensure_account(pool, email).await.unwrap()
    }

    async fn session_cookie(pool: &SqlitePool, account_id: &str) -> String {
        let id = crate::auth::create_session(pool, account_id).await.unwrap();
        format!("{}={id}", crate::auth::SESSION_COOKIE)
    }

    async fn api_key(pool: &SqlitePool, account_id: &str) -> String {
        crate::auth::create_api_key(pool, account_id, None)
            .await
            .unwrap()
            .key
    }

    fn get(path: &str, cookie: Option<&str>, same_origin: bool) -> Request<Body> {
        let mut req = Request::get(path);
        if same_origin {
            req = req.header("sec-fetch-site", "same-origin");
        }
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        req.body(Body::empty()).unwrap()
    }

    fn post(
        path: &str,
        cookie: Option<&str>,
        same_origin: bool,
        body: serde_json::Value,
    ) -> Request<Body> {
        let mut req = Request::post(path).header("content-type", "application/json");
        if same_origin {
            req = req.header("sec-fetch-site", "same-origin");
        }
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        req.body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    async fn send(app: &HttpApp, req: Request<Body>) -> (StatusCode, serde_json::Value, HeaderMap) {
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        // Handler-level failures (401) carry a plain-text body, not JSON.
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body, headers)
    }

    /// One OAuth grant row: the client row must exist for the FK.
    async fn grant(pool: &SqlitePool, account_id: &str, grant_id: &str) {
        sqlx::query(
            "INSERT OR IGNORE INTO oauthClient (id, clientId, redirectUris, kind, createdAt, updatedAt) \
             VALUES ('c1', 'https://claude.ai/oauth/mcp-oauth-client-metadata', '[\"https://x\"]', 'cimd', 0, 0)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO oauthConsent (id, clientId, userId, scopes, createdAt, updatedAt) \
             VALUES (?1, 'https://claude.ai/oauth/mcp-oauth-client-metadata', ?2, 'apps', 0, 0)",
        )
        .bind(grant_id)
        .bind(account_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn audit_rows(pool: &SqlitePool, user_id: &str, tool: &str) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT summary, outcome FROM account_action WHERE user_id = ?1 AND tool = ?2",
        )
        .bind(user_id)
        .bind(tool)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn account_endpoints_need_same_origin_session() {
        let (app, pool) = setup().await;
        let id = account(&pool, "ana@example.com").await;
        let cookie = session_cookie(&pool, &id).await;

        let (status, _, _) = send(&app, get("/api/console/account", None, true)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = send(&app, get("/api/console/account", Some(&cookie), false)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body, _) = send(&app, get("/api/console/account", Some(&cookie), true)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["email"], "ana@example.com");
        assert_eq!(body["plan"], "free");
    }

    #[tokio::test]
    async fn revoke_only_own_connections_and_audit() {
        let (app, pool) = setup().await;
        let ana = account(&pool, "ana@example.com").await;
        let bob = account(&pool, "bob@example.com").await;
        grant(&pool, &ana, "grant-ana").await;
        grant(&pool, &bob, "grant-bob").await;
        let cookie = session_cookie(&pool, &ana).await;

        let (status, body, _) =
            send(&app, get("/api/console/connections", Some(&cookie), true)).await;
        assert_eq!(status, StatusCode::OK);
        // The list says what the console itself uses: a web session, so
        // revoking grants can never sign this browser out.
        assert_eq!(body["console_session"], "web");
        assert_eq!(body["connections"].as_array().unwrap().len(), 1);
        assert_eq!(body["connections"][0]["id"], "grant-ana");

        // A foreign grant 404s exactly like a missing one, and stays live.
        let (status, body, _) = send(
            &app,
            post(
                "/api/console/connections/grant-bob/revoke",
                Some(&cookie),
                true,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["user_message"].is_string());
        let live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM oauthConsent WHERE id = 'grant-bob' AND revokedAt IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(live, 1);

        let (status, body, _) = send(
            &app,
            post(
                "/api/console/connections/grant-ana/revoke",
                Some(&cookie),
                true,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({"revoked": true}));
        // Tokens die with the grant (the domain path deletes them).
        let tokens: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM oauthAccessToken WHERE referenceId = 'grant-ana'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tokens, 0);
        let rows = audit_rows(&pool, &ana, "revoke_connection").await;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|(_, outcome)| outcome == "error"));
        assert!(rows.iter().any(|(_, outcome)| outcome == "ok"));
    }

    #[tokio::test]
    async fn created_key_works_then_revoked_key_fails() {
        let (app, pool) = setup().await;
        let id = account(&pool, "ana@example.com").await;
        let cookie = session_cookie(&pool, &id).await;
        let (auth_cfg, _) = crate::auth::test_support::test_config();

        let (status, body, _) = send(
            &app,
            post(
                "/api/console/api-keys",
                Some(&cookie),
                true,
                serde_json::json!({"name": "laptop"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let key = body["api_key"].as_str().unwrap().to_string();
        let prefix = body["prefix"].as_str().unwrap().to_string();
        assert!(body["api_key"].is_string());
        // The key authenticates (same bearer path MCP uses) …
        let before = crate::auth::resolve_bearer(&pool, &auth_cfg, &format!("Bearer {key}"))
            .await
            .unwrap();
        assert!(before.is_some());

        let (status, body, _) = send(
            &app,
            post(
                "/api/console/api-keys/revoke",
                Some(&cookie),
                true,
                serde_json::json!({"prefix": prefix}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({"revoked": true}));
        // … and fails once revoked.
        let after = crate::auth::resolve_bearer(&pool, &auth_cfg, &format!("Bearer {key}"))
            .await
            .unwrap();
        assert!(after.is_none());
        // Revoking again (or a foreign prefix) 404s; the audit row stands.
        let (status, _, _) = send(
            &app,
            post(
                "/api/console/api-keys/revoke",
                Some(&cookie),
                true,
                serde_json::json!({"prefix": prefix}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(audit_rows(&pool, &id, "create_api_key").await.len(), 1);
        assert_eq!(audit_rows(&pool, &id, "revoke_api_key").await.len(), 2);
    }

    #[tokio::test]
    async fn usage_numbers_equal_what_the_quota_checks_compute() {
        let (app, pool) = setup().await;
        let id = account(&pool, "ana@example.com").await;
        let cookie = session_cookie(&pool, &id).await;

        // Two claimed changes plus one refunded: the summary must read 1,
        // exactly what `changes_used_today` computes for the quota check.
        crate::mcp::tools::claim_change(&pool, &id).await.unwrap();
        crate::mcp::tools::claim_change(&pool, &id).await.unwrap();
        crate::mcp::tools::refund_change(&pool, &id).await.unwrap();
        sqlx::query("INSERT INTO usage_month (userId, month, requests, storage_peak_bytes, egress_bytes) VALUES (?1, ?2, 7, 9, 11)")
            .bind(&id)
            .bind(crate::accounts::month_utc())
            .execute(&pool)
            .await
            .unwrap();

        let (status, body, _) = send(&app, get("/api/console/account", Some(&cookie), true)).await;
        assert_eq!(status, StatusCode::OK);
        let used = crate::mcp::tools::changes_used_today(&pool, &id)
            .await
            .unwrap();
        assert_eq!(body["usage"]["code_changes_today"].as_u64().unwrap(), used);
        assert_eq!(used, 1);
        let plan = crate::mcp::tools::plan_of(&pool, &id).await.unwrap();
        assert_eq!(
            body["usage"]["code_changes_limit"].as_u64().unwrap(),
            crate::mcp::tools::change_limit(&plan)
        );
        assert_eq!(
            body["usage"]["apps_limit"].as_u64().unwrap(),
            crate::mcp::tools::app_limit(&plan)
        );
        assert_eq!(body["usage"]["requests_month"].as_u64().unwrap(), 7);
        assert_eq!(body["usage"]["storage_peak_bytes"].as_u64().unwrap(), 9);
        assert_eq!(body["usage"]["egress_bytes_month"].as_u64().unwrap(), 11);
        assert_eq!(
            body["usage"]["rollback_limit"].as_u64().unwrap(),
            crate::mcp::tools::ROLLBACKS_PER_DAY
        );
        assert!(body["resets_at"].is_string());
    }

    #[tokio::test]
    async fn sign_out_others_keeps_the_current_session() {
        let (app, pool) = setup().await;
        let id = account(&pool, "ana@example.com").await;
        let current = session_cookie(&pool, &id).await;
        let other = session_cookie(&pool, &id).await;
        let third = session_cookie(&pool, &id).await;

        let (status, body, _) = send(
            &app,
            post(
                "/api/console/sessions/sign-out-others",
                Some(&current),
                true,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({"signed_out": 2, "kept": 1}));
        let hash = |cookie: &str| crate::auth::session_hash(cookie.split_once('=').unwrap().1);
        let current_ok: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session WHERE token = ?1")
            .bind(hash(&current))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(current_ok, 1);
        for gone in [&other, &third] {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session WHERE token = ?1")
                .bind(hash(gone))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(count, 0);
        }
        assert_eq!(audit_rows(&pool, &id, "sign_out_others").await.len(), 1);
    }

    #[tokio::test]
    async fn delete_account_requires_the_matching_email() {
        let (app, pool) = setup().await;
        let id = account(&pool, "ana@example.com").await;
        let cookie = session_cookie(&pool, &id).await;

        let (status, body, _) = send(
            &app,
            post(
                "/api/console/account/delete",
                Some(&cookie),
                true,
                serde_json::json!({"confirm": "someone-else@example.com"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_argument");
        assert!(body["user_message"].as_str().unwrap().contains("email"));
        let still_there: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user WHERE id = ?1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(still_there, 1);
    }

    /// Create a real app through MCP (fake deploys, memory store), so the
    /// delete path exercises the same deletion the `delete_app` tool runs.
    async fn create_app_via_mcp(app: &HttpApp, key: &str, slug: &str) {
        let req = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": "create_app", "arguments": {"slug": slug}},
                }))
                .unwrap(),
            ))
            .unwrap();
        let (status, body, _) = send(app, req).await;
        assert_eq!(status, StatusCode::OK, "create_app {slug}: {body}");
        assert_eq!(
            body["result"]["structuredContent"]["deploy"]["status"],
            "success"
        );
    }

    #[tokio::test]
    async fn delete_account_removes_owned_apps_but_leaves_others_apps() {
        let (app, pool) = setup().await;
        let ana = account(&pool, "ana@example.com").await;
        let bob = account(&pool, "bob@example.com").await;
        let ana_key = api_key(&pool, &ana).await;
        let bob_key = api_key(&pool, &bob).await;
        create_app_via_mcp(&app, &ana_key, "anaapp").await;
        create_app_via_mcp(&app, &bob_key, "bobapp").await;
        // Ana collaborates on Bob's app (email-keyed): her deletion must
        // leave Bob's app and the collaboration row alone.
        sqlx::query(
            "INSERT INTO app_collaborator (app_id, email, userId, added_at) \
             SELECT id, 'ana@example.com', ?1, 0 FROM app WHERE slug = 'bobapp'",
        )
        .bind(&ana)
        .execute(&pool)
        .await
        .unwrap();
        let cookie = session_cookie(&pool, &ana).await;

        let (status, body, headers) = send(
            &app,
            post(
                "/api/console/account/delete",
                Some(&cookie),
                true,
                serde_json::json!({"confirm": "ana@example.com"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], true);
        assert_eq!(body["deleted_apps"], serde_json::json!(["anaapp"]));
        // The session cookie is cleared: every session is gone.
        let cleared = headers
            .get_all("set-cookie")
            .iter()
            .any(|v| v.to_str().unwrap().starts_with("__Host-outer_sid=;"));
        assert!(cleared);

        let ana_apps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app WHERE slug = 'anaapp'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(ana_apps, 0);
        let quarantine: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM slug_quarantine WHERE slug = 'anaapp'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(quarantine, 1);
        let ana_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user WHERE id = ?1")
            .bind(&ana)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(ana_rows, 0);
        let ana_sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM session WHERE userId = ?1")
                .bind(&ana)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ana_sessions, 0);
        // Bob is untouched: his account, his app, and Ana's (now
        // ownerless) collaboration on it.
        let bob_apps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app WHERE slug = 'bobapp'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(bob_apps, 1);
        let collab: Option<String> = sqlx::query_scalar(
            "SELECT userId FROM app_collaborator WHERE app_id = (SELECT id FROM app WHERE slug = 'bobapp')",
        )
        .fetch_optional(&pool)
        .await
        .unwrap()
        .flatten();
        assert_eq!(collab, None);
        // The deletion itself is audited, and the row survives the account.
        let audit = audit_rows(&pool, &ana, "delete_account").await;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].1, "ok");
    }

    #[tokio::test]
    async fn mcp_delete_account_uses_the_same_domain_path() {
        let (app, pool) = setup().await;
        let ana = account(&pool, "ana@example.com").await;
        let key = api_key(&pool, &ana).await;
        create_app_via_mcp(&app, &key, "anaapp").await;

        let call = |args: serde_json::Value| {
            Request::post("/mcp")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": "delete_account", "arguments": args},
                    }))
                    .unwrap(),
                ))
                .unwrap()
        };
        let (status, body, _) = send(
            &app,
            call(serde_json::json!({"confirm": "nope@example.com"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], true);
        assert_eq!(
            body["result"]["structuredContent"]["error"],
            "invalid_argument"
        );
        let apps_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app WHERE slug = 'anaapp'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(apps_left, 1);

        let (status, body, _) = send(
            &app,
            call(serde_json::json!({"confirm": "ana@example.com"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["result"]["structuredContent"]["deleted"], true);
        // The key died with the account: the next call is unauthenticated.
        let (status, _, _) = send(
            &app,
            call(serde_json::json!({"confirm": "ana@example.com"})),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn alert_prefs_default_on_and_partial_update() {
        let (app, pool) = setup().await;
        let id = account(&pool, "ana@example.com").await;
        let cookie = session_cookie(&pool, &id).await;

        let (status, body, _) = send(&app, get("/api/console/account", Some(&cookie), true)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["alerts"],
            serde_json::json!({"deploy_failed": true, "error_spike": true, "quota": true}),
            "no row means all on"
        );

        // An empty patch is a no-op refusal, not a silent success.
        let (status, body, _) = send(
            &app,
            post(
                "/api/console/alerts/prefs",
                Some(&cookie),
                true,
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["user_message"].is_string());

        // Partial update: only the provided toggle moves.
        let (status, body, _) = send(
            &app,
            post(
                "/api/console/alerts/prefs",
                Some(&cookie),
                true,
                serde_json::json!({"quota": false}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({"deploy_failed": true, "error_spike": true, "quota": false})
        );
        let (status, body, _) = send(&app, get("/api/console/account", Some(&cookie), true)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["alerts"]["quota"], false);
        assert_eq!(body["alerts"]["deploy_failed"], true);

        // The mutation is audited under the MCP tool name.
        assert_eq!(audit_rows(&pool, &id, "update_alert_prefs").await.len(), 1);

        // Same-origin session still required.
        let (status, _, _) = send(
            &app,
            post(
                "/api/console/alerts/prefs",
                Some(&cookie),
                false,
                serde_json::json!({"quota": true}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    async fn unsubscribe_body(app: &HttpApp, token: &str) -> (StatusCode, String) {
        let res = app
            .clone()
            .oneshot(
                Request::get(format!("/console/alerts/unsubscribe?token={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn unsubscribe_post(app: &HttpApp, token: &str, form: &str) -> (StatusCode, String) {
        let res = app
            .clone()
            .oneshot(
                Request::post(format!("/console/alerts/unsubscribe?token={token}"))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn email_unsubscribe_get_confirms_post_applies() {
        let (app, pool) = setup().await;
        let id = account(&pool, "ana@example.com").await;
        let key = b"outer-test-unsub-key".to_vec();

        // No session cookie, no same-origin header. GET renders the
        // confirmation form and changes nothing (link scanners prefetch).
        let token = crate::alerts::sign_unsubscribe(&key, &id, crate::alerts::KIND_ERROR_SPIKE);
        let (status, html) = unsubscribe_body(&app, &token).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Confirm unsubscribe"), "{html}");
        assert!(html.contains("<form"), "{html}");
        assert!(html.contains(&token), "form carries the token: {html}");
        let prefs = crate::alerts::get_prefs(&pool, &id).await.unwrap();
        assert!(
            prefs.deploy_failed && prefs.error_spike && prefs.quota,
            "GET changes nothing"
        );
        assert_eq!(audit_rows(&pool, &id, "update_alert_prefs").await.len(), 0);

        // POSTing the form applies the opt-out.
        let (status, html) = unsubscribe_post(&app, &token, &format!("token={token}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("You will no longer get error spike emails"),
            "{html}"
        );
        let prefs = crate::alerts::get_prefs(&pool, &id).await.unwrap();
        assert!(!prefs.error_spike && prefs.deploy_failed && prefs.quota);
        assert_eq!(audit_rows(&pool, &id, "update_alert_prefs").await.len(), 1);

        // Forged tokens explain themselves on both methods, changing nothing.
        let (status, html) = unsubscribe_body(&app, "v1.eA.evil.eA").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("not valid"), "{html}");
        let (status, html) = unsubscribe_post(&app, "v1.eA.evil.eA", "token=v1.eA.evil.eA").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("not valid"), "{html}");
        let prefs = crate::alerts::get_prefs(&pool, &id).await.unwrap();
        assert!(!prefs.error_spike && prefs.deploy_failed && prefs.quota);
    }

    #[tokio::test]
    async fn email_unsubscribe_one_click_post_applies() {
        let (app, pool) = setup().await;
        let id = account(&pool, "bob@example.com").await;
        let key = b"outer-test-unsub-key".to_vec();

        // RFC 8058: the mail client POSTs to the List-Unsubscribe URL with
        // `List-Unsubscribe=One-Click` and no form fields.
        let token = crate::alerts::sign_unsubscribe(&key, &id, crate::alerts::KIND_DEPLOY_FAILED);
        let (status, html) = unsubscribe_post(&app, &token, "List-Unsubscribe=One-Click").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("You will no longer get deploy failure emails"),
            "{html}"
        );
        let prefs = crate::alerts::get_prefs(&pool, &id).await.unwrap();
        assert!(!prefs.deploy_failed && prefs.error_spike && prefs.quota);
    }
}
