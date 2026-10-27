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
//! - `set_always_on`: `set_always_on <on|off>`
//! - `share_app`: `share_app`
//! - `get_app`: `get_app`
//! - `list_files`: `list_files [prefix=<p>] [sha=<short>]`
//! - `read_files`: `read_files <n> file(s) [paths...]`
//! - `search_code`: `search_code "<query, 200 chars>" [regex]`
//! - `history`: `history [path=<p>] [skip=<n>]`
//! - `show_commit`: `show_commit <sha>`
//! - `deploy_status` / `deploy_log`: `<tool> [deploy <id-short>]`
//! - `app_logs`: `app_logs [limit=<n>] [since=<seq>]`
//! - `app_errors`: `app_errors [status=<s>]`
//! - `look_at_app`: `look_at_app [path=<p>] [width=<w>] [full]`
//! - `resolve_error` / `ignore_error` / `reopen_error`: `<tool> <fp-short>`
//! - `list_secrets`: `list_secrets`
//! - `set_secret` / `unset_secret`: `<tool> <name>` (never the value)

use serde_json::Value;
use sqlx::SqlitePool;

use crate::auth::{now_ms, Credential, Principal};
use crate::mcp::tools::CallError;

/// Tools that write an `agent_action` row. Everything else (account tools,
/// `list_apps`, `delete_app`) writes nothing.
const LOGGED_TOOLS: &[&str] = &[
    "create_app",
    "commit",
    "rollback",
    "rename_app",
    "set_visibility",
    "set_always_on",
    "share_app",
    "get_app",
    "list_files",
    "read_files",
    "search_code",
    "history",
    "show_commit",
    "deploy_status",
    "deploy_log",
    "app_logs",
    "app_errors",
    "resolve_error",
    "ignore_error",
    "reopen_error",
    "list_secrets",
    "set_secret",
    "unset_secret",
    "add_collaborator",
    "remove_collaborator",
    "list_collaborators",
    "add_domain",
    "remove_domain",
    "list_domains",
    "look_at_app",
    "backup_database",
    "restore_backup",
];

/// Ceiling on one stored summary, in chars.
const MAX_SUMMARY_CHARS: usize = 2000;

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push_str("...");
    out
}

/// The first `n` chars: caller-supplied values may be any UTF-8, so never
/// slice them by byte offset.
fn prefix_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
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
        "set_always_on" => format!(
            "set_always_on {}",
            args.get("on")
                .and_then(serde_json::Value::as_bool)
                .map(|b| if b { "on" } else { "off" })
                .unwrap_or("?")
        ),
        "rename_app" => format!(
            "rename_app {} to {}",
            arg_str(args, "app").unwrap_or("?"),
            arg_str(args, "slug").unwrap_or("?")
        ),
        "share_app" | "get_app" => tool.to_string(),
        "list_secrets" | "list_collaborators" | "list_domains" | "remove_domain" => {
            tool.to_string()
        }
        "backup_database" | "restore_backup" => {
            let mut s = format!("{} {}", tool, arg_str(args, "app").unwrap_or("?"));
            // Two restores of one app must be distinguishable in the log.
            if tool == "restore_backup" {
                s.push_str(&format!(
                    " backup {}",
                    arg_str(args, "backup_id").unwrap_or("?")
                ));
            }
            s
        }
        "add_domain" => format!("add_domain {}", arg_str(args, "hostname").unwrap_or("?")),
        "add_collaborator" | "remove_collaborator" => {
            format!("{} {}", tool, arg_str(args, "email").unwrap_or("?"))
        }
        "set_secret" | "unset_secret" => {
            format!("{} {}", tool, arg_str(args, "key").unwrap_or("?"),)
        }
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
                s.push_str(&format!(" sha={}", prefix_chars(sha, 12)));
            }
            s
        }
        "read_files" => {
            let files = args.get("files").and_then(Value::as_array);
            let n = files.map_or(0, Vec::len);
            let mut s = format!("read_files {n} file(s)");
            if let Some(fs) = files {
                for f in fs.iter().take(2) {
                    if let Some(p) = f.get("path").and_then(Value::as_str) {
                        s.push_str(&format!(" {p}"));
                    }
                }
                if n > 2 {
                    s.push_str(&format!(" +{}", n - 2));
                }
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
                s.push_str(&format!(" deploy {}", prefix_chars(id, 8)));
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
        "app_errors" => {
            let mut s = "app_errors".to_string();
            if let Some(status) = arg_str(args, "status") {
                s.push_str(&format!(" status={status}"));
            }
            s
        }
        "look_at_app" => {
            let mut s = "look_at_app".to_string();
            if let Some(path) = arg_str(args, "path") {
                s.push_str(&format!(" path={}", truncate(path, 120)));
            }
            if let Some(width) = arg_u64(args, "width") {
                s.push_str(&format!(" width={width}"));
            }
            if args.get("full_page").and_then(Value::as_bool) == Some(true) {
                s.push_str(" full");
            }
            s
        }
        "resolve_error" | "ignore_error" | "reopen_error" => {
            format!(
                "{} {}",
                tool,
                prefix_chars(arg_str(args, "fingerprint").unwrap_or("?"), 12)
            )
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
    // `create_app` names the slug `slug`; every other app tool uses `app`;
    // `rename_app` carries both, and the row already moved, so the old slug
    // misses — try each present key until one resolves.
    let mut found: Option<(String, String)> = None;
    for key in ["app", "slug"] {
        let Some(slug) = args.get(key).and_then(Value::as_str) else {
            continue;
        };
        let app: Option<(String, String)> =
            sqlx::query_as("SELECT id, owner_id FROM app WHERE slug = ?1")
                .bind(slug)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
        if app.is_some() {
            found = app;
            break;
        }
    }
    let Some((app_id, owner_id)) = found else {
        return;
    };
    // Owners and collaborators both leave rows here (the console shows the
    // log to both); a slug with no role writes nothing, so `not_found`
    // probes leave no trace.
    let role =
        crate::collaborators::role(pool, &app_id, &owner_id, &caller.account_id, &caller.email)
            .await
            .unwrap_or(None);
    if role.is_none() {
        return;
    }
    // Recompute: `summary_for` returned Some above, so this is Some too.
    // Field values are bounded by the tools on success, but a refused call
    // is logged too and its arguments were never length-checked: one final
    // cap keeps a single row small whatever the caller sent.
    let Some(summary) = summary_for(tool, args).map(|s| truncate(&s, MAX_SUMMARY_CHARS)) else {
        return;
    };
    let (outcome, error_code) = outcome_of(result);
    let (grant_id, api_key_id) = match &caller.credential {
        Credential::OAuth { grant_id } => (Some(grant_id.as_str()), None),
        Credential::ApiKey { key_id } => (None, Some(key_id.as_str())),
    };
    if let Err(err) = sqlx::query(
        "INSERT INTO agent_action (id, app_id, user_id, consent_id, apikey_id, \
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
        tracing::warn!(app = %app_id, tool, error = %format!("{err:#}"), "agent_action insert failed");
    }
}

/// Insert one `agent_action` row for a console mutation, returning the
/// insert result so admin-override paths can fail closed (a lost audit row
/// on an admin act must surface as a 500, never a silent success).
pub async fn try_record_console(
    pool: &SqlitePool,
    app_id: &str,
    user_id: &str,
    tool: &str,
    summary: &str,
    outcome: &str,
    error_code: Option<&str>,
    duration_ms: i64,
) -> Result<(), sqlx::Error> {
    let summary = truncate(summary, MAX_SUMMARY_CHARS);
    sqlx::query(
        "INSERT INTO agent_action (id, app_id, user_id, consent_id, apikey_id, \
         tool, summary, outcome, error_code, duration_ms, created_at) \
         VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(app_id)
    .bind(user_id)
    .bind(tool)
    .bind(&summary)
    .bind(outcome)
    .bind(error_code)
    .bind(duration_ms)
    .bind(now_ms())
    .execute(pool)
    .await
    .map(|_| ())
}

/// Write one `agent_action` row for a console mutation, under the equivalent
/// MCP tool name (`set_visibility`, `set_secret`, `unset_secret`, `rollback`,
/// …). Both `consent_id` and `apikey_id` stay NULL — the console signs its
/// own session cookie — so [`list`] labels the row's client "Console".
/// `delete_app` has no row: its app row cascades, taking the entry with it
/// (the MCP tool is unlogged for the same reason).
#[allow(clippy::too_many_arguments)]
pub async fn record_console(
    pool: &SqlitePool,
    app_id: &str,
    user_id: &str,
    tool: &str,
    summary: &str,
    outcome: &str,
    error_code: Option<&str>,
    duration_ms: i64,
) {
    if let Err(err) = try_record_console(
        pool,
        app_id,
        user_id,
        tool,
        summary,
        outcome,
        error_code,
        duration_ms,
    )
    .await
    {
        tracing::warn!(tool, error = %format!("{err:#}"), "console action insert failed");
    }
}

/// Delete rows older than the owning account's plan retention (spec §3,
/// owner-set 2026-10-26: 24 h Free, 30 d Pro; numbers in [`crate::limits`]).
/// Returns the rows removed.
pub async fn prune(pool: &SqlitePool, now: i64) -> anyhow::Result<u64> {
    let free_cutoff = now - crate::limits::FREE_LOG_RETENTION_MS;
    let pro_cutoff = now - crate::limits::PRO_LOG_RETENTION_MS;
    // Set-based, on the `created_at` index: Pro apps keep 30 d, the rest 24 h.
    let pro = sqlx::query(
        "DELETE FROM agent_action WHERE created_at < ?1 AND app_id IN \
           (SELECT app.id FROM app JOIN user ON user.id = app.owner_id \
            WHERE user.plan = 'pro')",
    )
    .bind(pro_cutoff)
    .execute(pool)
    .await?;
    let rest = sqlx::query(
        "DELETE FROM agent_action WHERE created_at < ?1 AND app_id NOT IN \
           (SELECT app.id FROM app JOIN user ON user.id = app.owner_id \
            WHERE user.plan = 'pro')",
    )
    .bind(free_cutoff)
    .execute(pool)
    .await?;
    Ok(pro.rows_affected() + rest.rows_affected())
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

/// Who made the call: the OAuth client name + domain, or the API key name.
/// Both ids NULL means the row came from the console UI, which signs its
/// own session cookie rather than an OAuth grant or an API key: label that
/// client "Console".
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
        // No grant, client or key: a console write (the console-actions
        // contract records those with NULL consent and API key IDs).
        (None, None) => "Console".to_string(),
    }
}

/// The app's action log, newest first. The caller must have checked that the
/// signed-in account may see `app_id` (e.g. the console's `visible_app`): this
/// filters by app only. `before` is the `(created_at, id)`
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
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = match before {
        Some((before_ts, before_id)) => sqlx::query_as(
            "SELECT a.id, a.tool, a.summary, a.outcome, a.error_code, a.duration_ms, a.created_at, \
             g.clientName, c.name, c.host, k.name, k.prefix, a.consent_id, a.apikey_id \
             FROM agent_action AS a \
             LEFT JOIN oauthConsent AS g ON g.id = a.consent_id \
             LEFT JOIN oauthClient AS c ON c.clientId = g.clientId \
             LEFT JOIN apikey AS k ON k.id = a.apikey_id \
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
             g.clientName, c.name, c.host, k.name, k.prefix, a.consent_id, a.apikey_id \
             FROM agent_action AS a \
             LEFT JOIN oauthConsent AS g ON g.id = a.consent_id \
             LEFT JOIN oauthClient AS c ON c.clientId = g.clientId \
             LEFT JOIN apikey AS k ON k.id = a.apikey_id \
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
                consent_id,
                apikey_id,
            )| {
                // Console mutations record NULL consent + NULL key: the
                // label join shows those as "Console" (contract).
                let client_label = match (consent_id, apikey_id) {
                    (None, None) => "Console".to_string(),
                    _ => client_label(grant_name, client_name, client_host, key_name, key_prefix),
                };
                ActionRow {
                    id,
                    tool,
                    summary,
                    outcome,
                    error_code,
                    duration_ms,
                    created_at,
                    client_label,
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

    #[test]
    fn console_rows_label_without_grant_or_key() {
        assert_eq!(client_label(None, None, None, None, None), "Console");
        // Named keys and OAuth clients still win over the fallback.
        assert_eq!(
            client_label(None, None, None, Some("deploy".to_string()), None),
            "deploy"
        );
    }
}
