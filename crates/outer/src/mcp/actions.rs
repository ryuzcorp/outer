//! The console agent action log (spec §13.1): one row per MCP tool call
//! that targets an app — when, which connection or API key, the tool, a
//! redacted summary of the arguments, the outcome and the duration.
//!
//! Recording happens in [`crate::mcp::mcp_handler`]'s `handle_call`, the
//! single place every `tools/call` passes through: the call is timed, then
//! [`record`] runs after the result is computed. A failed insert warns and
//! the tool result is returned anyway, so logging never slows or fails a
//! tool call (one plain awaited SQLite insert).
//!
//! `delete_app` is deliberately NOT logged: its rows cascade with the app
//! row, so a delete entry would vanish together with the app it describes.
//!
//! Per-tool summary format (never file bodies, edit strings or secrets):
//! - `create_app`: `create_app <slug>`
//! - `commit`: `commit "<message, 200 chars>": <W> writes (<B> bytes),
//!   <E> edits (<B> bytes), <M> moves, <D> deletes [<first ≤10 paths>]`
//!   (write bytes are UTF-8/base64-decoded body lengths, edit bytes the
//!   replacement lengths; paths only, never contents)
//! - `rollback`: `rollback to <sha>`
//! - `set_visibility`: `set_visibility <value>`
//! - `share_app`: `share_app`
//! - `get_app`: `get_app`
//! - `list_files`: `list_files [prefix=<p>] [sha=<short>]`
//! - `read_file`: `read_file <path> [lines <start>-<end>]`
//! - `search_code`: `search_code "<query, 200 chars>" [regex]`
//! - `history`: `history [path=<p>] [skip=<n>]`
//! - `show_commit`: `show_commit <sha>`
//! - `deploy_status` / `deploy_log`: `<tool> [deploy <id-short>]`
//! - `app_logs`: `app_logs [limit=<n>] [since=<seq>]`

use serde_json::Value;
use sqlx::SqlitePool;

use crate::auth::{now_ms, Credential, Principal};
use crate::mcp::tools::CallError;

/// Log retention by plan (spec §3): 7 d Free, 30 d Pro.
pub const FREE_RETENTION_MS: i64 = 7 * 86_400_000;
pub const PRO_RETENTION_MS: i64 = 30 * 86_400_000;

/// Tools that write an `agent_action` row. Everything else (account tools,
/// `list_apps`, `delete_app`) writes nothing.
const LOGGED_TOOLS: &[&str] = &[
    "create_app",
    "commit",
    "rollback",
    "set_visibility",
    "share_app",
    "get_app",
    "list_files",
    "read_file",
    "search_code",
    "history",
    "show_commit",
    "deploy_status",
    "deploy_log",
    "app_logs",
];

/// Truncate to `n` chars on a char boundary, marking the cut.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push_str("...");
    out
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)?.as_str()
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key)?.as_u64()
}

/// Byte length of one commit write body: UTF-8 text, or the decoded length
/// of base64 (undecodable input counts 0; the handler refuses it anyway).
fn write_bytes(w: &Value) -> usize {
    if let Some(text) = w.get("content").and_then(Value::as_str) {
        return text.len();
    }
    if let Some(encoded) = w.get("base64").and_then(Value::as_str) {
        use base64::Engine as _;
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map(|b| b.len())
            .unwrap_or(0);
    }
    0
}

/// The redacted per-tool summary, or `None` for tools that log nothing.
fn summary_for(tool: &str, args: &Value) -> Option<String> {
    if !LOGGED_TOOLS.contains(&tool) {
        return None;
    }
    let summary = match tool {
        "create_app" => format!("create_app {}", arg_str(args, "slug").unwrap_or("?")),
        "commit" => {
            let message = truncate(arg_str(args, "message").unwrap_or(""), 200);
            let writes = args.get("writes").and_then(Value::as_array);
            let edits = args.get("edits").and_then(Value::as_array);
            let moves = args.get("moves").and_then(Value::as_array);
            let deletes = args.get("deletes").and_then(Value::as_array);
            let write_bytes: usize = writes
                .map(|ws| ws.iter().map(write_bytes).sum())
                .unwrap_or(0);
            let edit_bytes: usize = edits
                .map(|es| {
                    es.iter()
                        .map(|e| e.get("new").and_then(Value::as_str).map_or(0, str::len))
                        .sum()
                })
                .unwrap_or(0);
            let mut paths: Vec<&str> = Vec::new();
            for list in [writes, edits, moves].into_iter().flatten() {
                for entry in list {
                    let path = entry
                        .get("path")
                        .or_else(|| entry.get("from"))
                        .and_then(Value::as_str);
                    if let Some(path) = path {
                        if paths.len() < 10 {
                            paths.push(path);
                        }
                    }
                }
            }
            if let Some(deletes) = deletes {
                for path in deletes
                    .iter()
                    .filter_map(Value::as_str)
                    .take(10 - paths.len().min(10))
                {
                    paths.push(path);
                }
            }
            format!(
                "commit {message:?}: {} writes ({write_bytes} bytes), {} edits ({edit_bytes} bytes), {} moves, {} deletes [{}]",
                writes.map_or(0, Vec::len),
                edits.map_or(0, Vec::len),
                moves.map_or(0, Vec::len),
                deletes.map_or(0, Vec::len),
                paths.join(", "),
            )
        }
        "rollback" => format!("rollback to {}", arg_str(args, "sha").unwrap_or("?")),
        "set_visibility" => format!(
            "set_visibility {}",
            arg_str(args, "visibility").unwrap_or("?")
        ),
        "share_app" | "get_app" => tool.to_string(),
        "search_code" => {
            let mut s = format!(
                "search_code {:?}",
                truncate(arg_str(args, "query").unwrap_or(""), 200)
            );
            if args.get("regex").and_then(Value::as_bool) == Some(true) {
                s.push_str(" regex");
            }
            s
        }
        "list_files" => {
            let mut s = "list_files".to_string();
            if let Some(prefix) = arg_str(args, "path_prefix") {
                s.push_str(&format!(" prefix={prefix}"));
            }
            if let Some(sha) = arg_str(args, "sha") {
                s.push_str(&format!(" sha={}", &sha[..12.min(sha.len())]));
            }
            s
        }
        "read_file" => {
            let mut s = format!("read_file {}", arg_str(args, "path").unwrap_or("?"));
            match (
                args.get("start_line").and_then(Value::as_u64),
                args.get("end_line").and_then(Value::as_u64),
            ) {
                (Some(start), Some(end)) => s.push_str(&format!(" lines {start}-{end}")),
                (Some(start), None) => s.push_str(&format!(" lines {start}-")),
                (None, Some(end)) => s.push_str(&format!(" lines -{end}")),
                (None, None) => {}
            }
            s
        }
        "history" => {
            let mut s = "history".to_string();
            if let Some(path) = arg_str(args, "path") {
                s.push_str(&format!(" path={path}"));
            }
            if let Some(skip) = arg_u64(args, "skip") {
                if skip > 0 {
                    s.push_str(&format!(" skip={skip}"));
                }
            }
            s
        }
        "show_commit" => format!("show_commit {}", arg_str(args, "sha").unwrap_or("?")),
        "deploy_status" | "deploy_log" => {
            let mut s = tool.to_string();
            if let Some(id) = arg_str(args, "deploy_id") {
                s.push_str(&format!(" deploy {}", &id[..8.min(id.len())]));
            }
            s
        }
        "app_logs" => {
            let mut s = "app_logs".to_string();
            if let Some(limit) = arg_u64(args, "limit") {
                s.push_str(&format!(" limit={limit}"));
            }
            if let Some(since) = arg_u64(args, "since") {
                s.push_str(&format!(" since={since}"));
            }
            s
        }
        _ => return None,
    };
    Some(summary)
}

/// Outcome plus optional error code for one finished dispatch.
fn outcome_of(result: &Result<Value, CallError>) -> (String, Option<&'static str>) {
    match result {
        Ok(value) => {
            // Code tools carry their deploy object in structuredContent: the
            // deploy status (success/failed/blocked/building) is the outcome.
            let status = value
                .pointer("/structuredContent/deploy/status")
                .and_then(Value::as_str);
            match status {
                Some(status) => (status.to_string(), None),
                None => ("ok".to_string(), None),
            }
        }
        Err(CallError::Tool(err)) => ("error".to_string(), Some(err.code)),
        // Arg-shape failures never reach `record` (early return above).
        Err(CallError::Rpc(_, _)) => ("error".to_string(), None),
    }
}

/// Write one `agent_action` row after a tool call finished. Unknown or
/// foreign apps (slug missing, or owned by someone else) write nothing, so
/// `not_found` probes leave no trace. A failed insert warns and the tool
/// result is returned anyway.
pub async fn record(
    pool: &SqlitePool,
    caller: &Principal,
    tool: &str,
    args: Option<&Value>,
    result: &Result<Value, CallError>,
    duration_ms: i64,
) {
    if matches!(result, Err(CallError::Rpc(_, _))) {
        return;
    }
    let Some(args) = args else { return };
    if summary_for(tool, args).is_none() {
        return;
    }
    // `create_app` names the slug `slug`; every other app tool uses `app`.
    let slug = args
        .get("app")
        .or_else(|| args.get("slug"))
        .and_then(Value::as_str);
    let Some(slug) = slug else { return };
    let app: Option<(String, String)> =
        sqlx::query_as("SELECT id, owner_id FROM app WHERE slug = ?1")
            .bind(slug)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
    let Some((app_id, owner_id)) = app else {
        return;
    };
    if owner_id != caller.account_id {
        return;
    }
    // Recompute: `summary_for` returned Some above, so this is Some too.
    let Some(summary) = summary_for(tool, args) else {
        return;
    };
    let (outcome, error_code) = outcome_of(result);
    let (grant_id, api_key_id) = match &caller.credential {
        Credential::OAuth { grant_id } => (Some(grant_id.as_str()), None),
        Credential::ApiKey { key_id } => (None, Some(key_id.as_str())),
    };
    if let Err(err) = sqlx::query(
        "INSERT INTO agent_action (id, app_id, account_id, grant_id, api_key_id, \
         tool, summary, outcome, error_code, duration_ms, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&app_id)
    .bind(&caller.account_id)
    .bind(grant_id)
    .bind(api_key_id)
    .bind(tool)
    .bind(&summary)
    .bind(&outcome)
    .bind(error_code)
    .bind(duration_ms)
    .bind(now_ms())
    .execute(pool)
    .await
    {
        tracing::warn!(app = %slug, tool, error = %format!("{err:#}"), "agent_action insert failed");
    }
}

/// Delete rows older than the owning account's plan retention (spec §3).
/// Returns the rows removed.
pub async fn prune(pool: &SqlitePool, now: i64) -> anyhow::Result<u64> {
    let free_cutoff = now - FREE_RETENTION_MS;
    let pro_cutoff = now - PRO_RETENTION_MS;
    let result = sqlx::query(
        "DELETE FROM agent_action WHERE created_at < ( \
           SELECT CASE WHEN COALESCE( \
             (SELECT plan FROM account WHERE id = \
               (SELECT owner_id FROM app WHERE id = agent_action.app_id)), 'free') = 'pro' \
           THEN ?1 ELSE ?2 END)",
    )
    .bind(pro_cutoff)
    .bind(free_cutoff)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// One console action-log row, newest first, with the caller label resolved:
/// the OAuth client name + domain, or the API key name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRow {
    pub id: String,
    pub tool: String,
    pub summary: String,
    pub outcome: String,
    pub error_code: Option<String>,
    pub duration_ms: i64,
    pub created_at: i64,
    pub client_label: String,
}

fn client_label(
    grant_name: Option<String>,
    client_name: Option<String>,
    client_host: Option<String>,
    key_name: Option<String>,
    key_prefix: Option<String>,
) -> String {
    if grant_name.is_some() || client_name.is_some() || client_host.is_some() {
        let name = grant_name.or(client_name);
        return match (name, client_host) {
            (Some(name), Some(host)) => format!("{name} ({host})"),
            (Some(name), None) => name,
            (None, Some(host)) => host,
            (None, None) => "OAuth client".to_string(),
        };
    }
    match (key_name, key_prefix) {
        (Some(name), _) => name,
        (None, Some(prefix)) => format!("API key {prefix}"),
        (None, None) => "API key".to_string(),
    }
}

/// The app's action log, newest first. `before` is the `(created_at, id)`
/// cursor of the last row on the previous page; `limit` is clamped to
/// 1–100.
pub async fn list(
    pool: &SqlitePool,
    app_id: &str,
    before: Option<(i64, String)>,
    limit: u64,
) -> anyhow::Result<Vec<ActionRow>> {
    let limit = limit.clamp(1, 100) as i64;
    type Row = (
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = match before {
        Some((before_ts, before_id)) => sqlx::query_as(
            "SELECT a.id, a.tool, a.summary, a.outcome, a.error_code, a.duration_ms, a.created_at, \
             g.client_name, c.name, c.host, k.name, k.prefix \
             FROM agent_action AS a \
             LEFT JOIN oauth_grant AS g ON g.id = a.grant_id \
             LEFT JOIN oauth_client AS c ON c.client_id = g.client_id \
             LEFT JOIN api_key AS k ON k.id = a.api_key_id \
             WHERE a.app_id = ?1 AND (a.created_at, a.id) < (?2, ?3) \
             ORDER BY a.created_at DESC, a.id DESC LIMIT ?4",
        )
        .bind(app_id)
        .bind(before_ts)
        .bind(before_id)
        .bind(limit)
        .fetch_all(pool)
        .await?,
        None => sqlx::query_as(
            "SELECT a.id, a.tool, a.summary, a.outcome, a.error_code, a.duration_ms, a.created_at, \
             g.client_name, c.name, c.host, k.name, k.prefix \
             FROM agent_action AS a \
             LEFT JOIN oauth_grant AS g ON g.id = a.grant_id \
             LEFT JOIN oauth_client AS c ON c.client_id = g.client_id \
             LEFT JOIN api_key AS k ON k.id = a.api_key_id \
             WHERE a.app_id = ?1 \
             ORDER BY a.created_at DESC, a.id DESC LIMIT ?2",
        )
        .bind(app_id)
        .bind(limit)
        .fetch_all(pool)
        .await?,
    };
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                tool,
                summary,
                outcome,
                error_code,
                duration_ms,
                created_at,
                grant_name,
                client_name,
                client_host,
                key_name,
                key_prefix,
            )| {
                ActionRow {
                    id,
                    tool,
                    summary,
                    outcome,
                    error_code,
                    duration_ms,
                    created_at,
                    client_label: client_label(
                        grant_name,
                        client_name,
                        client_host,
                        key_name,
                        key_prefix,
                    ),
                }
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlogged_tools_have_no_summary() {
        let args = serde_json::json!({"app": "grocery"});
        assert!(summary_for("list_apps", &args).is_none());
        assert!(summary_for("delete_app", &args).is_none());
        assert!(summary_for("about", &args).is_none());
        assert!(summary_for("nope", &args).is_none());
    }

    #[test]
    fn commit_summary_counts_bytes_and_paths_without_bodies() {
        let secret = "SUPER_SECRET_MARKER_BODY";
        let args = serde_json::json!({
            "app": "grocery",
            "message": "add list",
            "writes": [{"path": "a.tsx", "content": secret}],
            "edits": [{"path": "b.tsx", "old": "x", "new": "yy"}],
            "moves": [{"from": "c.tsx", "to": "d.tsx"}],
            "deletes": ["e.tsx"],
        });
        let summary = summary_for("commit", &args).unwrap();
        assert!(summary.contains("1 writes (24 bytes)"), "{summary}");
        assert!(summary.contains("1 edits (2 bytes)"), "{summary}");
        assert!(summary.contains("1 moves, 1 deletes"), "{summary}");
        assert!(summary.contains("a.tsx"), "{summary}");
        assert!(!summary.contains(secret), "{summary}");
    }

    #[test]
    fn search_summary_truncates_the_query() {
        let args = serde_json::json!({"app": "g", "query": "x".repeat(300)});
        let summary = summary_for("search_code", &args).unwrap();
        assert!(!summary.contains(&"x".repeat(300)), "{summary}");
        assert!(summary.contains("..."), "{summary}");
    }
}
