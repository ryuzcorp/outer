//! The code tools: `list_files`, `read_file`, `search_code`, `commit`,
//! `history`, `show_commit`, `rollback`.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    authed, check_rev, claim_change, internal, ok, ok_deploy, ok_untrusted, owned_app,
    refund_change, run_deploy, Ctx,
};
use crate::apps::App;
use crate::git::{self, CommitError};
use crate::mcp::error::ToolError;
use crate::mcp::McpState;
use crate::ops::noreply_author;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesInput {
    /// The app slug.
    pub app: String,
    /// A commit sha (default: the tip of main).
    pub sha: Option<String>,
    /// Only files under this directory.
    pub path_prefix: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileInput {
    /// The app slug.
    pub app: String,
    /// Repo-relative path.
    pub path: String,
    /// A commit sha (default: the tip of main).
    pub sha: Option<String>,
    /// First line to return (1-based, default 1).
    pub start_line: Option<u64>,
    /// Last line to return, inclusive (default: end of file).
    pub end_line: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchCodeInput {
    /// The app slug.
    pub app: String,
    /// Text to find (fixed string unless regex is true).
    pub query: String,
    /// Treat the query as an extended regular expression.
    pub regex: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileWriteInput {
    /// Repo-relative path.
    pub path: String,
    /// UTF-8 content (exactly one of content, base64).
    pub content: Option<String>,
    /// Raw bytes, base64-encoded (exactly one of content, base64).
    pub base64: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditInput {
    /// Repo-relative path.
    pub path: String,
    /// Exact text to replace (must occur exactly once, or set replace_all).
    pub old: String,
    /// Replacement text.
    pub new: String,
    /// Replace every occurrence instead of requiring exactly one.
    pub replace_all: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveInput {
    /// Existing path.
    pub from: String,
    /// New path.
    pub to: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CommitInput {
    /// The app slug.
    pub app: String,
    /// Commit message, 1-500 chars.
    pub message: String,
    /// Expected head (CAS): a moved head answers conflict with current_sha.
    pub base: Option<String>,
    /// Full-file writes (new files or replacements).
    pub writes: Option<Vec<FileWriteInput>>,
    /// Exact-match replacements in existing files.
    pub edits: Option<Vec<EditInput>>,
    /// Paths to delete.
    pub deletes: Option<Vec<String>>,
    /// File renames.
    pub moves: Option<Vec<MoveInput>>,
    /// Wait for the deploy (default true). False returns immediately;
    /// poll deploy_status.
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryInput {
    /// The app slug.
    pub app: String,
    /// Only commits touching this path.
    pub path: Option<String>,
    /// Commits to skip (the next_skip cursor).
    pub skip: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShowCommitInput {
    /// The app slug.
    pub app: String,
    /// The commit sha to show.
    pub sha: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RollbackInput {
    /// The app slug.
    pub app: String,
    /// An earlier successfully deployed sha: the new commit copies its tree.
    pub sha: String,
    /// Wait for the deploy (default true).
    pub wait: Option<bool>,
}

/// The synced bare mirror for `slug`, or `not_found` when the app has no
/// repo yet (a brand-new app whose seed is still running).
async fn mirror(ctx: &Ctx<'_>, slug: &str) -> Result<std::path::PathBuf, ToolError> {
    crate::git::manifest::ensure_synced(&ctx.state.store, &ctx.state.git, &ctx.state.sync, slug)
        .await
        .map_err(internal)
}

pub async fn list_files(input: ListFilesInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    // Validate before git runs: bad shape is `invalid_argument` with a
    // message built only from agent input, so no server path can leak.
    // Anything the mirror itself reports after that is `internal`.
    if let Some(prefix) = input.path_prefix.as_deref() {
        crate::git::local::validate_read_path(prefix)
            .map_err(|e| ToolError::invalid_argument(e.to_string()))?;
    }
    if let Some(rev) = input.sha.as_deref() {
        check_rev("sha", rev)?;
    }
    let bare = mirror(ctx, &app.slug).await?;
    let (sha, files, truncated) = crate::git::local::list_files(
        &bare,
        input.sha.as_deref(),
        input.path_prefix.as_deref(),
        5000,
    )
    .await
    .map_err(internal)?;
    let files: Vec<Value> = files
        .iter()
        .map(|f| json!({"path": f.path, "size": f.size}))
        .collect();
    Ok(ok(
        format!("{} file(s) at {}.", files.len(), &sha[..12.min(sha.len())]),
        json!({"sha": sha, "files": files, "truncated": truncated}),
    ))
}

pub async fn read_file(input: ReadFileInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    if let (Some(start), Some(end)) = (input.start_line, input.end_line) {
        if start < 1 || end < start {
            return Err(ToolError::invalid_argument(
                "start_line is 1-based and end_line must not precede it",
            ));
        }
    }
    let app = owned_app(ctx, &input.app).await?;
    if let Some(rev) = input.sha.as_deref() {
        check_rev("sha", rev)?;
    }
    let bare = mirror(ctx, &app.slug).await?;
    let (sha, bytes) = crate::git::local::read_blob(&bare, input.sha.as_deref(), &input.path)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("no commits") {
                ToolError::not_found(msg)
            } else if msg.contains("over the") || msg.contains("must") || msg.contains("path") {
                ToolError::invalid_argument(msg)
            } else {
                internal(e)
            }
        })?;
    let total_lines;
    let (text, base64) = if bytes.contains(&0) {
        total_lines = None;
        (
            None,
            Some(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &bytes,
            )),
        )
    } else {
        let text = String::from_utf8(bytes).map_err(internal)?;
        total_lines = Some(text.lines().count() as u64);
        let start = input.start_line.unwrap_or(1).max(1) as usize;
        let end = input.end_line.map(|e| e as usize).unwrap_or(usize::MAX);
        let picked: Vec<&str> = text
            .lines()
            .skip(start - 1)
            .take(end.saturating_sub(start - 1))
            .collect();
        let picked = picked.join("\n");
        (Some(picked), None)
    };
    let (body, truncated) = match &text {
        Some(t) if t.len() > 64 * 1024 => {
            let mut end = 64 * 1024;
            while !t.is_char_boundary(end) {
                end -= 1;
            }
            (t[..end].to_string(), true)
        }
        Some(t) => (t.clone(), false),
        None => (String::new(), false),
    };
    let mut structured = json!({
        "path": input.path,
        "sha": sha,
        "truncated": truncated,
    });
    if let Some(text) = text {
        structured["text"] = json!(body);
        let _ = text;
    } else if let Some(encoded) = base64 {
        structured["binary"] = json!(encoded);
    }
    if let Some(total) = total_lines {
        structured["total_lines"] = json!(total);
    }
    if input.start_line.is_some() {
        structured["start_line"] = json!(input.start_line);
    }
    if input.end_line.is_some() {
        structured["end_line"] = json!(input.end_line);
    }
    let summary = format!("{} at {}.", input.path, &sha[..12.min(sha.len())]);
    let data = match structured.get("text").and_then(Value::as_str) {
        Some(t) => t.to_string(),
        None => format!(
            "<binary, {} bytes base64>",
            structured["binary"].as_str().map_or(0, str::len)
        ),
    };
    Ok(ok_untrusted(summary, data, structured))
}

pub async fn search_code(input: SearchCodeInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    let bare = mirror(ctx, &app.slug).await?;
    let (sha, hits, truncated) = crate::git::local::grep(
        &bare,
        None,
        &input.query,
        input.regex.unwrap_or(false),
        None,
    )
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("query") || msg.contains("regex") {
            ToolError::invalid_argument(msg)
        } else {
            internal(e)
        }
    })?;
    let matches: Vec<Value> = hits
        .iter()
        .map(|h| json!({"path": h.path, "line": h.line, "text": h.text}))
        .collect();
    let data = matches
        .iter()
        .map(|m| {
            format!(
                "{}:{}:{}",
                m["path"].as_str().unwrap_or(""),
                m["line"].as_u64().unwrap_or(0),
                m["text"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ok_untrusted(
        format!("{} match(es).", matches.len()),
        if data.is_empty() {
            "(no matches)".to_string()
        } else {
            data
        },
        json!({"sha": sha, "matches": matches, "truncated": truncated}),
    ))
}

fn to_commit_request(
    input: &CommitInput,
    author: git::Author,
) -> Result<git::CommitRequest, ToolError> {
    let mut writes = Vec::new();
    for w in input.writes.as_deref().unwrap_or(&[]) {
        let body = match (&w.content, &w.base64) {
            (Some(text), None) => git::WriteBody::Text(text.clone()),
            (None, Some(encoded)) => git::WriteBody::Base64(encoded.clone()),
            _ => {
                return Err(ToolError::invalid_argument(format!(
                    "write to {:?} needs exactly one of content, base64",
                    w.path
                )));
            }
        };
        writes.push(git::FileWrite {
            path: w.path.clone(),
            body,
        });
    }
    let mut edits = Vec::new();
    for e in input.edits.as_deref().unwrap_or(&[]) {
        edits.push(git::Edit {
            path: e.path.clone(),
            old: e.old.clone(),
            new: e.new.clone(),
            replace_all: e.replace_all.unwrap_or(false),
        });
    }
    Ok(git::CommitRequest {
        message: input.message.clone(),
        base: input.base.clone(),
        writes,
        edits,
        deletes: input.deletes.clone().unwrap_or_default(),
        moves: input
            .moves
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|m| git::Move {
                from: m.from.clone(),
                to: m.to.clone(),
            })
            .collect(),
        author,
    })
}

pub async fn commit(input: CommitInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    let author = noreply_author(&caller.account_id, &caller.email, &ctx.state.domain);
    let request = to_commit_request(&input, author)?;
    if let Some(base) = input.base.as_deref() {
        check_rev("base", base)?;
    }
    // Claimed before the git work: a commit that never happens (bad base,
    // moved head) gives the change back; a created commit counts even when
    // its deploy fails or is blocked.
    let claim = claim_change(&ctx.state.pool, &caller.account_id).await?;
    let outcome = match git::commit::commit_with_outcome(
        &ctx.state.store,
        &ctx.state.git,
        &ctx.state.sync,
        &app.slug,
        request,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            if let Err(refund) = refund_change(&ctx.state.pool, &caller.account_id).await {
                tracing::warn!("commit refund failed: {refund:#}");
            }
            return Err(ToolError::from_commit(e));
        }
    };
    let sha = outcome.sha;
    let deploy = run_deploy(ctx.state, &app, &sha, input.wait.unwrap_or(true)).await?;
    let quota =
        json!({"limit": claim.limit, "used": claim.used, "resets_at": super::resets_at_utc()});
    let mut summary = format!(
        "Committed {} ({}).",
        &sha[..12.min(sha.len())],
        deploy["status"].as_str().unwrap_or("")
    );
    if !outcome.formatted.is_empty() {
        summary.push_str(&format!(" Formatted {} file(s).", outcome.formatted.len()));
    }
    let mut commit = json!({"sha": sha, "formatted": outcome.formatted});
    if !outcome.format_skipped.is_empty() {
        commit["format_skipped"] = json!(outcome
            .format_skipped
            .iter()
            .map(|s| json!({"path": s.path, "reason": s.reason}))
            .collect::<Vec<_>>());
    }
    let structured = json!({"commit": commit, "deploy": deploy, "quota": quota});
    Ok(ok_deploy(
        summary,
        &structured["deploy"].clone(),
        structured,
    ))
}

pub async fn history(input: HistoryInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    if let Some(path) = input.path.as_deref() {
        crate::git::local::validate_read_path(path)
            .map_err(|e| ToolError::invalid_argument(e.to_string()))?;
    }
    let bare = mirror(ctx, &app.slug).await?;
    let skip = input.skip.unwrap_or(0) as usize;
    let (head, entries, next_skip) = crate::git::local::log(&bare, input.path.as_deref(), skip, 50)
        .await
        .map_err(internal)?;
    let commits: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "sha": e.sha,
                "author": e.author_name,
                "email": e.author_email,
                "at": e.at,
                "message": e.message,
            })
        })
        .collect();
    Ok(ok(
        format!("{} commit(s).", commits.len()),
        json!({"head": head, "commits": commits, "next_skip": next_skip}),
    ))
}

pub async fn show_commit(input: ShowCommitInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    check_rev("sha", &input.sha)?;
    let bare = mirror(ctx, &app.slug).await?;
    let shown = crate::git::local::show(&bare, &input.sha)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("unknown commit") {
                ToolError::not_found(msg)
            } else {
                internal(e)
            }
        })?;
    let structured = json!({
        "commit": {
            "sha": shown.commit.sha,
            "author": shown.commit.author_name,
            "email": shown.commit.author_email,
            "at": shown.commit.at,
            "message": shown.commit.message,
        },
        "files": shown.files,
        "patch": shown.patch,
        "truncated": shown.truncated,
    });
    Ok(ok_untrusted(
        format!(
            "{} ({} file(s)).",
            &shown.commit.sha[..12.min(shown.commit.sha.len())],
            shown.files.len()
        ),
        shown.patch.clone(),
        structured,
    ))
}

pub async fn rollback(input: RollbackInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owned_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    rollback_to(
        ctx.state,
        &app,
        &caller.account_id,
        &caller.email,
        &input.sha,
        input.wait.unwrap_or(true),
    )
    .await
}

/// The shared rollback path: the MCP tool and the console endpoint both run
/// here, so the target checks, the daily quota and the
/// new-commit-then-deploy shape are identical. `app` must already be
/// ownership-checked by the caller (MCP's `owned_app` refuses foreign slugs
/// as `not_found`; the console answers 404 the same way).
pub async fn rollback_to(
    state: &McpState,
    app: &App,
    account_id: &str,
    email: &str,
    sha: &str,
    wait: bool,
) -> Result<Value, ToolError> {
    check_rev("sha", sha)?;
    // At most one rollback runs per app: a second one fails fast instead of
    // queueing another full build behind the first.
    let _guard = state.rollback_locks.try_lock(&app.id).ok_or_else(|| {
        ToolError::rate_limited(
            "another rollback is already running for this app; wait for it to finish",
            30,
        )
    })?;
    // Coalesce: a deploy already queued or running for this app wins over a
    // new rollback commit. Report it (awaited or not) instead of building.
    if let Some(row) = super::latest_deploy(&state.pool, &app.id).await? {
        if !super::is_terminal(&row.status) {
            let deploy = super::await_row(state, app, Some(&row.deploy_id), wait).await?;
            let structured = json!({"commit": {"sha": row.sha}, "deploy": deploy});
            let summary = format!(
                "A deploy is already running; reporting it instead of a new rollback ({}).",
                structured["deploy"]["status"].as_str().unwrap_or("")
            );
            return Ok(ok_deploy(
                summary,
                &structured["deploy"].clone(),
                structured,
            ));
        }
    }
    // The target must be an earlier successful deploy: its tree is known
    // good, so review is a cache hit and the fleet can adopt it.
    // Agents quote short shas from `history`: a prefix names the target when
    // it matches exactly one successfully deployed commit. `check_rev` above
    // already limited the input to hex, so it carries no LIKE wildcards.
    let matches = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT sha FROM deploy \
         WHERE app_id = ?1 AND status = 'success' AND sha LIKE ?2 || '%' LIMIT 2",
    )
    .bind(&app.id)
    .bind(sha.to_ascii_lowercase())
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;
    let target = match matches.as_slice() {
        [one] => one.clone(),
        [] => {
            return Err(ToolError::invalid_argument(format!(
                "{sha:?} is not a successfully deployed commit of {:?}; pick a sha from history whose deploy succeeded",
                app.slug
            )))
        }
        _ => {
            return Err(ToolError::invalid_argument(format!(
                "{sha:?} matches more than one deployed commit of {:?}; give more of the sha",
                app.slug
            )))
        }
    };
    // The daily budget is claimed after the cheap checks and before the git
    // work: a refused or redundant rollback costs nothing, and a rollback
    // whose commit never happens gives the claim back.
    super::claim_rollback(&state.pool, &app.id).await?;
    let author = noreply_author(account_id, email, &state.domain);
    let sha = match git::commit::rollback(
        &state.store,
        &state.git,
        &state.sync,
        &app.slug,
        &target,
        &author,
    )
    .await
    {
        Ok(sha) => sha,
        Err(e) => {
            if let Err(refund) = super::refund_rollback(&state.pool, &app.id).await {
                tracing::warn!("rollback refund failed: {refund:#}");
            }
            return Err(match e {
                CommitError::BadRollback { target, reason } => {
                    ToolError::invalid_argument(format!("cannot roll back to {target}: {reason}"))
                }
                other => ToolError::from_commit(other),
            });
        }
    };
    // Rollbacks don't count as code changes (spec §14.1).
    let deploy = run_deploy(state, app, &sha, wait).await?;
    let summary = format!(
        "Rolled back to {} ({}).",
        &target[..12.min(target.len())],
        deploy["status"].as_str().unwrap_or("")
    );
    let structured = json!({"commit": {"sha": sha}, "deploy": deploy});
    Ok(ok_deploy(
        summary,
        &structured["deploy"].clone(),
        structured,
    ))
}
