//! The deploy and runtime tools: `deploy_status`, `deploy_log`, `app_logs`.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{await_row, deploy_by, ok_untrusted, owned_app, Ctx};
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployStatusInput {
    /// The app slug.
    pub app: String,
    /// A deploy id (default: the latest deploy).
    pub deploy_id: Option<String>,
    /// Wait for a terminal state (default false).
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployLogInput {
    /// The app slug.
    pub app: String,
    /// A deploy id (default: the latest deploy).
    pub deploy_id: Option<String>,
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
    let app = owned_app(ctx, &input.app).await?;
    if let Some(deploy_id) = input.deploy_id.as_deref() {
        if deploy_by(&ctx.state.pool, &app.id, Some(deploy_id))
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
    )
    .await?;
    let status = deploy["status"].as_str().unwrap_or("");
    Ok(super::ok_deploy(
        format!("Deploy is {status}."),
        &deploy.clone(),
        deploy,
    ))
}

pub async fn deploy_log(input: DeployLogInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    let row = deploy_by(&ctx.state.pool, &app.id, input.deploy_id.as_deref()).await?;
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

/// Lines from a fleet's in-memory log ring: the newest `limit` tail, or —
/// when `since` is set — the OLDEST `limit` after that sequence number, so
/// the next poll from the last returned seq continues without a gap.
/// Shared by the MCP `app_logs` tool and the console logs endpoint.
pub async fn log_lines(
    fleets: &crate::fleet::manager::FleetManager,
    slug: &str,
    since: Option<u64>,
    limit: usize,
) -> Vec<Value> {
    let Some(ring) = fleets.logs(slug) else {
        return Vec::new();
    };
    let ring = ring.lock().await;
    let entries = match since {
        Some(seq) => ring.since(seq),
        None => ring.tail(limit),
    };
    // tail() returns the newest N in order; since() is unbounded and feeds
    // a cursor: keep the OLDEST `limit` after `since`.
    let entries = if since.is_some() {
        cursor_page(entries, limit)
    } else {
        entries
    };
    entries
        .iter()
        .map(|e| {
            json!({
                "seq": e.seq,
                "ts": e.ts,
                "stream": format!("{:?}", e.stream).to_ascii_lowercase(),
                "line": e.line,
            })
        })
        .collect()
}

pub async fn app_logs(input: AppLogsInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    let limit = input.limit.unwrap_or(100).clamp(1, 500) as usize;
    let lines: Vec<Value> = log_lines(&ctx.state.fleets, &app.slug, input.since, limit).await;
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

/// One page of a `since` cursor: the OLDEST `limit` entries, so the next
/// poll from the last returned seq continues without a gap.
fn cursor_page<T>(mut entries: Vec<T>, limit: usize) -> Vec<T> {
    entries.truncate(limit);
    entries
}

#[cfg(test)]
mod cursor_tests {
    use super::cursor_page;

    #[test]
    fn polling_pages_through_every_line() {
        let ring: Vec<u64> = (1..=7).collect();
        let mut seen = Vec::new();
        let mut since = 0;
        loop {
            let page = cursor_page(ring.iter().copied().filter(|seq| *seq > since).collect(), 3);
            let Some(last) = page.last().copied() else {
                break;
            };
            seen.extend(page);
            since = last;
        }
        assert_eq!(seen, ring, "no line falls behind the cursor");
    }
}
