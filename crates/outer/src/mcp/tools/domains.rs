//! The custom-domain tools: `add_domain`, `remove_domain`, `list_domains`
//! (spec §7.7, §8.2; Pro, one per app, owner only in M1).
//!
//! Every handler runs [`owner_app`] first (a foreign slug answers
//! `not_found`, exactly like a missing one), then the shared functions in
//! [`crate::apps::domains`] — the same functions the console endpoints call.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{ok, owner_app, plan_of, Ctx};
use crate::apps::domains;
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddDomainInput {
    /// The app slug.
    pub app: String,
    /// The full domain name, e.g. `groceries.silva.family`.
    pub hostname: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveDomainInput {
    /// The app slug.
    pub app: String,
    /// The hostname to remove. Optional: the app holds at most one, so it
    /// defaults to that; when given it must match or the call is refused.
    pub hostname: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListDomainsInput {
    /// The app slug.
    pub app: String,
}

/// A [`domains::DomainError`] as the MCP [`ToolError`] the tool and the
/// console endpoint report (same codes, same user message).
/// `billing` + `owner_id` supply the `upgrade_url` the Pro refusal carries;
/// `billing` also supplies the live promo state the $4 mention gates on.
pub(crate) async fn tool_error(
    err: domains::DomainError,
    billing: &crate::billing::BillingState,
    owner_id: &str,
) -> ToolError {
    let code = err.code();
    let message = err.to_string();
    // The promo lookup hits Stripe: only pay for it on the path that names
    // the launch price. Every other refusal keeps today's wording.
    let user = if code == "payment_required" {
        err.user_message(billing.launch_discount_active().await)
    } else {
        err.user_message(false)
    };
    match code {
        "not_found" => ToolError::not_found(message),
        "payment_required" => super::with_upgrade(
            ToolError::new(code, message).with_user(user),
            billing,
            owner_id,
        ),
        "internal" => ToolError::internal(message).with_user(user),
        _ => ToolError::invalid_argument(message).with_user(user),
    }
}

fn cf(ctx: &Ctx<'_>) -> Option<crate::cloudflare::CustomHostnames> {
    crate::cloudflare::CustomHostnames::from_purge(ctx.state.delete_cfg.cache_purge.as_ref())
}

fn domain_json(view: &domains::DomainView) -> Value {
    json!({
        "hostname": view.hostname,
        "status": view.status,
        "ssl_status": view.ssl_status,
        "cname_target": view.cname_target,
        "txt_records": view.txt_records.iter().map(|r| json!({"name": r.name, "value": r.value})).collect::<Vec<_>>(),
        "reason": view.reason,
    })
}

fn dns_help(view: &domains::DomainView) -> String {
    let mut out = format!(
        "Point a CNAME from {} to {}.",
        view.hostname, view.cname_target
    );
    for txt in &view.txt_records {
        out.push_str(&format!(" Also add TXT {} with {:?}.", txt.name, txt.value));
    }
    out
}

pub async fn add_domain(input: AddDomainInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    let plan = plan_of(&ctx.state.pool, &app.owner_id).await?;
    let view = match domains::add_domain(
        &ctx.state.pool,
        cf(ctx).as_ref(),
        &ctx.state.domain,
        &ctx.state.apps_domain,
        &plan,
        &app,
        &input.hostname,
    )
    .await
    {
        Ok(view) => view,
        Err(err) => return Err(tool_error(err, &ctx.state.billing, &app.owner_id).await),
    };
    // The POST may omit validation records, so one GET fills them in: the
    // agent relays the full DNS setup word for word.
    let summary = format!(
        "{} now points at {} ({}). {}",
        view.hostname,
        app.slug,
        view.status,
        dns_help(&view)
    );
    Ok(ok(
        summary.clone(),
        json!({"domains": [domain_json(&view)], "user_message": summary}),
    ))
}

pub async fn remove_domain(input: RemoveDomainInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    let hostname = match domains::remove_domain(
        &ctx.state.pool,
        cf(ctx).as_ref(),
        ctx.state.delete_cfg.cache_purge.as_ref(),
        &app,
        input.hostname.as_deref(),
    )
    .await
    {
        Ok(hostname) => hostname,
        Err(err) => return Err(tool_error(err, &ctx.state.billing, &app.owner_id).await),
    };
    let message = format!("{hostname} no longer serves {}.", app.slug);
    Ok(ok(
        message.clone(),
        json!({"removed": hostname, "user_message": message}),
    ))
}

pub async fn list_domains(input: ListDomainsInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    // Live refresh: the GET endpoint is the source of truth, so the agent
    // sees `active` as soon as Cloudflare finishes (or the failure reason).
    // A Cloudflare outage falls back to the stored status.
    let view =
        match domains::refresh_domain(&ctx.state.pool, cf(ctx).as_ref(), &ctx.state.domain, &app)
            .await
        {
            Ok(view) => Some(view),
            Err(domains::DomainError::NotFound(_, _)) => None,
            Err(domains::DomainError::Unavailable(_)) => {
                match domains::list_domains(&ctx.state.pool, &ctx.state.domain, &app).await {
                    Ok(rows) => rows.into_iter().next(),
                    Err(err) => {
                        return Err(tool_error(err, &ctx.state.billing, &app.owner_id).await)
                    }
                }
            }
            Err(err) => return Err(tool_error(err, &ctx.state.billing, &app.owner_id).await),
        };
    let out: Vec<Value> = view.iter().map(domain_json).collect();
    let summary = match view.as_ref() {
        None => format!("{} has no custom domain.", app.slug),
        Some(v) => format!(
            "{}: {} ({}). {}",
            app.slug,
            v.hostname,
            v.status,
            dns_help(v)
        ),
    };
    Ok(ok(summary, json!({"domains": out})))
}
