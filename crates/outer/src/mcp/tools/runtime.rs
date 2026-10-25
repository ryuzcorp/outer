//! The deploy and runtime tools: `deploy_status`, `deploy_log`, `app_logs`,
//! `app_errors`.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{await_row, deploy_by, latest_deploy, ok, ok_untrusted, visible_app, Ctx};
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployStatusInput {
    /// The app slug.
    pub app: String,
    /// A deploy id (default: the latest deploy).
    pub deploy_id: Option<String>,
    /// Wait for a terminal state (default false).
    pub wait: Option<bool>,
    /// Look at preview runs instead of live ones (default false).
    pub preview: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployLogInput {
    /// The app slug.
    pub app: String,
    /// A deploy id (default: the latest deploy).
    pub deploy_id: Option<String>,
    /// Look at preview runs instead of live ones (default false).
    pub preview: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppLogsInput {
    /// The app slug.
    pub app: String,
    /// Most recent lines to return (default 100, max 500).
    pub limit: Option<u64>,
    /// Only lines after this sequence number (exclusive); pass back next_since to poll.
    pub since: Option<u64>,
}

pub async fn deploy_status(input: DeployStatusInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let kind = kind_for(input.preview);
    if let Some(deploy_id) = input.deploy_id.as_deref() {
        if deploy_by(&ctx.state.pool, &app.id, Some(deploy_id), kind)
            .await?
            .is_none()
        {
            return Err(ToolError::not_found(format!("no deploy {deploy_id:?}")));
        }
    }
    let deploy = await_row(
        ctx.state,
        &app,
        input.deploy_id.as_deref(),
        input.wait.unwrap_or(false),
        kind,
    )
    .await?;
    let status = deploy["status"].as_str().unwrap_or("");
    Ok(super::ok_deploy(
        format!("Deploy is {status}."),
        &deploy.clone(),
        deploy,
    ))
}

/// Live or preview runs, from the tool input flag.
fn kind_for(preview: Option<bool>) -> crate::deploy::DeployKind {
    if preview.unwrap_or(false) {
        crate::deploy::DeployKind::Preview
    } else {
        crate::deploy::DeployKind::Main
    }
}

pub async fn deploy_log(input: DeployLogInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let row = deploy_by(
        &ctx.state.pool,
        &app.id,
        input.deploy_id.as_deref(),
        kind_for(input.preview),
    )
    .await?;
    let Some(row) = row else {
        return Err(ToolError::not_found("no deploys for this app yet"));
    };
    let log = row.log_tail.unwrap_or_default();
    Ok(ok_untrusted(
        format!(
            "Build log for deploy {} ({}, {}).",
            row.deploy_id, row.sha, row.status
        ),
        if log.is_empty() {
            "(empty log)".to_string()
        } else {
            log.clone()
        },
        json!({"deploy_id": row.deploy_id, "sha": row.sha, "status": row.status, "log": log}),
    ))
}
/// Lines from the persisted `app_log` merged with the fleet's live ring on
/// one `seq` cursor: the newest `limit` tail, or — when `since` is set —
/// the OLDEST `limit` after that sequence number, so the next poll from the
/// last returned seq continues without a gap. Persisted lines survive sleep
/// and restart; the ring covers what the sweep has not flushed yet.
/// Shared by the MCP `app_logs` tool and the console logs endpoint.
pub async fn log_lines(
    pool: &sqlx::SqlitePool,
    fleets: &crate::fleet::manager::FleetManager,
    app_id: &str,
    slug: &str,
    since: Option<u64>,
    limit: usize,
) -> Vec<Value> {
    crate::observe::logs::read_merged(pool, fleets, app_id, slug, since, limit).await
}

pub async fn app_logs(input: AppLogsInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let limit = input.limit.unwrap_or(100).clamp(1, 500) as usize;
    let lines: Vec<Value> = log_lines(
        &ctx.state.pool,
        &ctx.state.fleets,
        &app.id,
        &app.slug,
        input.since,
        limit,
    )
    .await;
    let next_since = lines.last().and_then(|l| l["seq"].as_u64());
    let data = lines
        .iter()
        .map(|l| l["line"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ok_untrusted(
        format!("{} log line(s).", lines.len()),
        if data.is_empty() {
            "(no lines)".to_string()
        } else {
            data
        },
        json!({"lines": lines, "next_since": next_since}),
    ))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppErrorsInput {
    /// The app slug.
    pub app: String,
    /// Only issues with this status (`open`, `resolved` or `ignored`);
    /// unset returns every status.
    pub status: Option<String>,
}

/// The app's grouped runtime errors, newest first: `ERROR` lines and
/// uncaught-exception stacks from the fleet log stream, grouped by
/// fingerprint, plus the proxy 5xx count over the trailing hour (reused
/// from `metric_minute`, never counted twice). Untrusted like `app_logs`:
/// messages and frames can hold app or visitor text.
pub async fn app_errors(input: AppErrorsInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    if let Some(status) = input.status.as_deref() {
        if !matches!(status, "open" | "resolved" | "ignored") {
            return Err(ToolError::invalid_argument(format!(
                "unknown status {status:?}: expected open, resolved or ignored"
            )));
        }
    }
    let found = crate::observe::logs::issues(&ctx.state.pool, &app.id, input.status.as_deref())
        .await
        .map_err(super::internal)?;
    let s5xx =
        crate::observe::logs::s5xx_last_hour(&ctx.state.pool, &app.id, crate::apps::now_ms())
            .await
            .map_err(super::internal)?;
    // The app's latest deploy, for "did my last deploy fix it"
    // correlation. One value for the whole answer.
    let last_deploy_sha = latest_deploy(&ctx.state.pool, &app.id)
        .await
        .map_err(super::internal)?
        .map(|row| row.sha);
    let items: Vec<Value> = found
        .iter()
        .map(|issue| {
            json!({
                "fingerprint": issue.fingerprint,
                "message": issue.message,
                "frame": issue.frame,
                "count": issue.count,
                "first_seen": issue.first_seen,
                "last_seen": issue.last_seen,
                "status": issue.status,
                "last_deploy_sha": last_deploy_sha,
            })
        })
        .collect();
    let data = items
        .iter()
        .map(|item| {
            format!(
                "{} (x{}, {})",
                item["message"].as_str().unwrap_or(""),
                item["count"].as_i64().unwrap_or(0),
                item["status"].as_str().unwrap_or(""),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ok_untrusted(
        format!(
            "{} error issue(s), {} proxy 5xx in the last hour.",
            items.len(),
            s5xx
        ),
        if data.is_empty() {
            "(no issues)".to_string()
        } else {
            data
        },
        json!({"issues": items, "s5xx_last_hour": s5xx}),
    ))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResolveErrorInput {
    /// The app slug.
    pub app: String,
    /// The issue fingerprint from `app_errors`.
    pub fingerprint: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IgnoreErrorInput {
    /// The app slug.
    pub app: String,
    /// The issue fingerprint from `app_errors`.
    pub fingerprint: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReopenErrorInput {
    /// The app slug.
    pub app: String,
    /// The issue fingerprint from `app_errors`.
    pub fingerprint: String,
}

/// Move one grouped error issue to a new status. Shared with the console
/// triage buttons: the same [`crate::observe::logs::set_status`] domain
/// function, so MCP and console agree on the rules (resolve/ignore stamp
/// `resolved_at`; a resolved issue that recurs reopens automatically;
/// reopen clears the stamp). Collaborators may triage: spec §7.4 keeps
/// owner-only for membership, visibility, secrets and destructive actions,
/// while collaborators already make operational writes (`commit`,
/// `rollback`) — triage is in that second class. The answer carries no
/// message or frame text, so the tool stays trusted (`Hints::WRITE`):
/// read the issue through `app_errors` instead.
async fn set_issue_status(
    ctx: &Ctx<'_>,
    app_slug: &str,
    fingerprint: &str,
    status: &'static str,
) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, app_slug).await?;
    if fingerprint.is_empty() || fingerprint.len() > 64 {
        return Err(ToolError::invalid_argument(format!(
            "bad fingerprint {fingerprint:?}: expected the app_errors fingerprint"
        )));
    }
    let updated = crate::observe::logs::set_status(
        &ctx.state.pool,
        &app.id,
        &[fingerprint],
        status,
        crate::apps::now_ms(),
    )
    .await
    .map_err(super::internal)?;
    if updated == 0 {
        return Err(ToolError::not_found(format!(
            "no such error {fingerprint:?}"
        )));
    }
    Ok(ok(
        format!("error {status}: {fingerprint}"),
        json!({"fingerprint": fingerprint, "status": status}),
    ))
}

/// Mark one grouped runtime error resolved: it leaves the open list, and
/// a new event for the same fingerprint reopens it automatically as a
/// regression.
pub async fn resolve_error(input: ResolveErrorInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    set_issue_status(ctx, &input.app, &input.fingerprint, "resolved").await
}

/// Ignore one grouped runtime error: it keeps counting but stays out of
/// the open list. Ignored issues never reopen automatically.
pub async fn ignore_error(input: IgnoreErrorInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    set_issue_status(ctx, &input.app, &input.fingerprint, "ignored").await
}

/// Reopen one resolved or ignored error issue: it returns to the open
/// list as a fresh triage, not a regression.
pub async fn reopen_error(input: ReopenErrorInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    set_issue_status(ctx, &input.app, &input.fingerprint, "open").await
}
