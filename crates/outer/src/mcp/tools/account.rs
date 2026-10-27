//! Public tools (`about`, `docs`, `sign_in`, `verify_code`) and the account
//! tools (`whoami`, API keys, OAuth connections).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    app_limit, apps_owned, authed, change_limit, changes_used_today, internal, ok, plan_of,
    resets_at_utc, Ctx, NoArgs,
};
use crate::auth;
use crate::mcp::docs;
use crate::mcp::error::ToolError;
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DocsInput {
    /// Which agent guide to read: quickstart, templates, ilha, void, website, celld, d1, storage, backups, kv, auth, cron, queues, websockets, sse, email, sandboxes, limits, moderation, custom-domains, export, previews or look.
    pub topic: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SignInInput {
    /// The email address to sign in as.
    pub email: String,
    /// Optional invite code for the closed beta (`OUTER-XXXX-XXXX`; dashes
    /// and the prefix are optional). Only needed when the email is new and
    /// not allowlisted: existing, allowlisted and admin emails ignore it.
    pub invite_code: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifyCodeInput {
    /// The email address the code was sent to.
    pub email: String,
    /// The 6-digit code from the inbox.
    pub code: String,
    /// Required on first sign-in: accept the Terms and Privacy Policy.
    pub accept_terms: bool,
    /// A label for the minted API key (shown in `list_api_keys`).
    pub key_name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateApiKeyInput {
    /// A label for the new key (shown in `list_api_keys`).
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RevokeApiKeyInput {
    /// The key prefix shown by `list_api_keys` (the first characters).
    pub prefix: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RevokeConnectionInput {
    /// The grant id shown by `list_connections`.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteAccountInput {
    /// Type your account email again to confirm: must equal it exactly.
    pub confirm: String,
}

pub async fn about(_input: NoArgs, _ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let structured = json!({
        "what": "Outer is your agent's own app cloud, built on Ilha. Your agent talks to Outer, Outer builds the app and serves it at https://<slug>.outer.now.",
        "connect": "Claude, Codex and ChatGPT sign in through OAuth automatically. Without OAuth: sign_in {email}, read the emailed code, verify_code {email, code, accept_terms: true}, then send Authorization: Bearer <api_key>.",
        "rules": [
            "Call docs {topic: \"ilha\"} before writing code.",
            "create_app, then commit (prefer edits). Every code tool waits for the deploy.",
            "New apps are empty shells: build the request, copying proven patterns from `examples`.",
            "Relay user_message to your human verbatim.",
        ],
        "plans": "Free: 2 apps, 40 code changes and 30 build minutes/day. Pro ($8/month): 20 apps, 1,000 changes and 300 build minutes/day. See docs {topic: \"limits\"}.",
    });
    Ok(ok(
        "Outer hosts Ilha apps at https://<slug>.outer.now. Sign in with OAuth or sign_in/verify_code, create_app, then commit. Relay any user_message verbatim.",
        structured,
    ))
}

pub async fn docs(input: DocsInput, _ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let topic = input.topic.trim().to_ascii_lowercase();
    match docs::get(&topic) {
        Some(markdown) => Ok(ok(
            format!("Outer guide: {topic}"),
            json!({"topic": topic, "markdown": markdown}),
        )),
        None => Err(ToolError::invalid_argument(format!(
            "unknown docs topic {:?}; pick one of: {}",
            input.topic,
            docs::topic_list().join(", ")
        ))),
    }
}

pub async fn sign_in(input: SignInInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let sent = auth::otp_sign_in(
        &ctx.state.pool,
        &ctx.state.auth.config,
        &input.email,
        input.invite_code.as_deref(),
        ctx.client_ip.as_deref(),
    )
    .await
    .map_err(ToolError::from_auth)?;
    Ok(ok(
        "Code sent. Ask your human for the 6-digit code, then call verify_code.".to_string(),
        json!({"sent": true, "expires_in_s": sent.expires_in_s}),
    ))
}

pub async fn verify_code(input: VerifyCodeInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let verified = auth::otp_verify(
        &ctx.state.pool,
        &ctx.state.auth.config,
        &input.email,
        &input.code,
        input.accept_terms,
        input.key_name.as_deref(),
        ctx.client_ip.as_deref(),
    )
    .await
    .map_err(ToolError::from_auth)?;
    let mcp_url = ctx.state.mcp_url();
    Ok(ok(
        "Signed in. Save the API key in your MCP config and send it as Authorization: Bearer <api_key>.".to_string(),
        json!({
            "api_key": verified.api_key,
            "account": {"id": verified.account_id, "email": verified.email},
            "mcp_url": mcp_url,
        }),
    ))
}

#[derive(Debug, Serialize)]
struct Usage {
    apps_owned: u64,
    code_changes_today: u64,
    build_minutes_today: u64,
    requests_month: u64,
}

#[derive(Debug, Serialize)]
struct Limits {
    apps_owned_max: u64,
    code_changes_per_day: u64,
    build_minutes_per_day: u64,
    requests_per_month: u64,
    collaborators_per_app: u64,
    d1_bytes_max: u64,
    r2_bytes_max: u64,
}

pub async fn whoami(_input: NoArgs, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let plan = plan_of(&ctx.state.pool, &caller.account_id).await?;
    let (_, owner_last) = crate::inactivity::owner_clock(&ctx.state.pool, &caller.account_id).await;
    let build_ms = crate::limits::build_ms_used_today(&ctx.state.pool, &caller.account_id)
        .await
        .map_err(internal)?;
    let month: Option<i64> =
        sqlx::query_scalar("SELECT requests FROM usage_month WHERE userId = ?1 AND month = ?2")
            .bind(&caller.account_id)
            .bind(crate::accounts::month_utc())
            .fetch_optional(&ctx.state.pool)
            .await
            .map_err(internal)?;
    let usage = Usage {
        apps_owned: apps_owned(&ctx.state.pool, &caller.account_id).await?,
        code_changes_today: changes_used_today(&ctx.state.pool, &caller.account_id).await?,
        build_minutes_today: build_ms / 60_000,
        requests_month: month.unwrap_or(0).max(0) as u64,
    };
    let limits = Limits {
        apps_owned_max: app_limit(&plan),
        code_changes_per_day: change_limit(&plan),
        build_minutes_per_day: crate::limits::build_minutes_limit(&plan),
        requests_per_month: crate::limits::requests_cap(&plan),
        collaborators_per_app: crate::limits::collaborator_limit(&plan),
        d1_bytes_max: crate::limits::d1_cap_bytes(&plan),
        r2_bytes_max: crate::limits::r2_cap_bytes(&plan),
    };
    // Soonest inactivity deadline across owned apps (spec §7.8); null on
    // Pro (exempt) or with no owned apps.
    let deletes_at = crate::inactivity::account_deletes_at(
        &ctx.state.pool,
        &caller.account_id,
        &plan,
        owner_last,
        ctx.state.inactivity,
    )
    .await;
    Ok(ok(
        format!("{} on the {} plan.", caller.email, plan),
        json!({
            "email": caller.email,
            "plan": plan,
            "usage": usage,
            "limits": limits,
            "resets_at": resets_at_utc(),
            "stopped_apps": [],
            "deletes_at": deletes_at,
        }),
    ))
}

pub async fn create_api_key(input: CreateApiKeyInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let key = auth::create_api_key(&ctx.state.pool, &caller.account_id, input.name.as_deref())
        .await
        .map_err(ToolError::from_auth)?;
    Ok(ok(
        "API key minted. It is shown once — store it now.".to_string(),
        json!({"id": key.id, "name": input.name, "prefix": key.prefix, "api_key": key.key}),
    ))
}

pub async fn list_api_keys(_input: NoArgs, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let keys = auth::list_api_keys(&ctx.state.pool, &caller.account_id)
        .await
        .map_err(internal)?;
    let keys: Vec<Value> = keys
        .iter()
        .map(|k| {
            let client = match (&k.client_name, &k.client_version) {
                (Some(name), Some(version)) => Some(format!("{name}/{version}")),
                (Some(name), None) => Some(name.clone()),
                _ => None,
            };
            json!({
                "id": k.id,
                "name": k.name,
                "prefix": k.prefix,
                "created_at": k.created_at,
                "last_used_at": k.last_used_at,
                "client": client,
            })
        })
        .collect();
    Ok(ok(
        format!("{} API key(s).", keys.len()),
        json!({"keys": keys}),
    ))
}

pub async fn revoke_api_key(input: RevokeApiKeyInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let revoked = auth::revoke_api_key(&ctx.state.pool, &caller.account_id, &input.prefix)
        .await
        .map_err(ToolError::from_auth)?;
    if !revoked {
        return Err(ToolError::not_found(format!(
            "no API key with prefix {:?}",
            input.prefix
        )));
    }
    Ok(ok("API key revoked.".to_string(), json!({"revoked": true})))
}

pub async fn list_connections(_input: NoArgs, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let grants = auth::list_connections(&ctx.state.pool, &caller.account_id)
        .await
        .map_err(internal)?;
    let grants: Vec<Value> = grants
        .iter()
        .map(|g| {
            json!({
                "id": g.grant_id,
                "client_id": g.client_id,
                "client_name": g.client_name,
                "host": g.client_host,
                "scope": g.scope,
                "created_at": g.created_at,
                "last_used_at": g.last_used_at,
            })
        })
        .collect();
    Ok(ok(
        format!("{} OAuth connection(s).", grants.len()),
        json!({"connections": grants}),
    ))
}

pub async fn revoke_connection(
    input: RevokeConnectionInput,
    ctx: &Ctx<'_>,
) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let revoked = auth::revoke_connection(&ctx.state.pool, &caller.account_id, &input.id)
        .await
        .map_err(ToolError::from_auth)?;
    if !revoked {
        return Err(ToolError::not_found(format!(
            "no connection {:?}",
            input.id
        )));
    }
    Ok(ok(
        "OAuth connection revoked.".to_string(),
        json!({"revoked": true}),
    ))
}

/// Delete the caller's account (spec §8.2): the same
/// [`crate::accounts::delete_account`] domain path the console uses.
/// `confirm` must equal the account email exactly. Deletes owned apps
/// through the `delete_app` path, leaves apps the caller only
/// collaborates on, and signs every session out.
pub async fn delete_account(input: DeleteAccountInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let deps = crate::accounts::DeleteAccountDeps {
        pool: &ctx.state.pool,
        store: &ctx.state.store,
        git: &ctx.state.git,
        sync: &ctx.state.sync,
        fleets: &ctx.state.fleets,
        deploys: &ctx.state.deploys,
        rollback_locks: &ctx.state.rollback_locks,
        delete_cfg: &ctx.state.delete_cfg,
    };
    let out = crate::accounts::delete_account(&deps, &caller.account_id, &input.confirm)
        .await
        .map_err(|err| ToolError::new(err.code, err.message).with_user(err.user_message))?;
    // Owned apps (and their uids) are gone: reinstall at once (best-effort;
    // the sweep tick is the backstop).
    ctx.state.egress.refresh(&ctx.state.pool).await;
    Ok(ok(
        format!("Account {} deleted.", out.email),
        json!({"deleted": true, "email": out.email, "deleted_apps": out.deleted_apps}),
    ))
}
