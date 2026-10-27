//! The code tools: `list_files`, `read_files`, `search_code`, `commit`,
//! `history`, `show_commit`, `rollback`.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    authed, check_build_quota, check_rev, check_storage_writable, claim_change, internal, ok,
    ok_deploy, ok_untrusted, refund_change, run_deploy, visible_app, Ctx,
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

/// One file inside a `read_files` call.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileEntry {
    /// Repo-relative path.
    pub path: String,
    /// First line to return (1-based, default 1). Not with `symbol`.
    pub start_line: Option<u64>,
    /// Last line to return, inclusive (default: end of file). Not with `symbol`.
    pub end_line: Option<u64>,
    /// A top-level function, class, const or type to return instead of a
    /// line range (`.ts/.tsx/.js/.jsx/.mjs/.cjs/.mts/.cts`, or `.astro`
    /// frontmatter). Ambiguous names are refused: use lines instead.
    pub symbol: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFilesInput {
    /// The app slug.
    pub app: String,
    /// A commit sha (default: the tip of main).
    pub sha: Option<String>,
    /// 1-20 files to read. A bad file fails inline without failing the call.
    pub files: Vec<ReadFileEntry>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchCodeInput {
    /// The app slug.
    pub app: String,
    /// Text to find (fixed string unless regex is true).
    pub query: String,
    /// Treat the query as an extended regular expression (text only).
    pub regex: Option<bool>,
    /// Only files matching these globs (`*`, `?`, `**`; no `/` matches the
    /// basename at any depth). At most 20.
    pub paths: Option<Vec<String>>,
    /// Surrounding lines per match, 0-5 (default 0).
    pub context: Option<u8>,
    /// Match `Query`, `QUERY` and the rest too.
    pub case_insensitive: Option<bool>,
    /// Match whole words only.
    pub word: Option<bool>,
    /// `text` (default) or `symbol`: one identifier's definitions and usages.
    pub kind: Option<String>,
    /// Search generated files too (`node_modules/`, `dist/`, lockfiles are
    /// skipped by default).
    pub include_generated: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileWriteInput {
    /// Repo-relative path.
    pub path: String,
    /// UTF-8 content (exactly one of content, base64).
    pub content: Option<String>,
    /// Raw bytes, base64-encoded (exactly one of content, base64).
    pub base64: Option<String>,
    /// Per-file CAS: the blob id read_files reported for this path. A stale
    /// blob answers conflict with the current blob; unrelated files never
    /// conflict.
    pub expect_blob: Option<String>,
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
    /// Per-file CAS: the blob id read_files reported. See writes.
    pub expect_blob: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LineEditInput {
    /// Repo-relative path.
    pub path: String,
    /// First line to replace (1-based, matches read_files numbering).
    pub start_line: u64,
    /// Last line to replace, inclusive.
    pub end_line: u64,
    /// Replacement text (empty deletes the lines). The file's trailing
    /// newline is preserved unless the range touches the last line.
    pub new: String,
    /// Per-file CAS: the blob id read_files reported. See writes.
    pub expect_blob: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InsertInput {
    /// Repo-relative path.
    pub path: String,
    /// Line to insert after (0 is the top of the file, the line count is
    /// the end). Several inserts at one line run in input order.
    pub after_line: u64,
    /// Text to insert.
    pub insert: String,
    /// Per-file CAS: the blob id read_files reported. See writes.
    pub expect_blob: Option<String>,
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
    /// Line-range replacements (inclusive, 1-based read_files numbering).
    /// Ranges in one file must not overlap.
    pub line_edits: Option<Vec<LineEditInput>>,
    /// Insertions after a line (0 is the top). Must not land inside a
    /// line_edits range of the same file.
    pub inserts: Option<Vec<InsertInput>>,
    /// One unified diff, or a Codex `*** Begin Patch` block. A commit
    /// carrying patch carries no other change kind. Hunks apply with exact
    /// context; any reject fails the whole commit.
    pub patch: Option<String>,
    /// Paths to delete.
    pub deletes: Option<Vec<String>>,
    /// File renames.
    pub moves: Option<Vec<MoveInput>>,
    /// Wait for the deploy (default true). False returns immediately;
    /// poll deploy_status (with preview: true for a preview commit).
    pub wait: Option<bool>,
    /// Preview the change instead of shipping it (Pro): commits to the
    /// preview ref, deploys to the `<slug>--preview` URL with a fresh,
    /// empty data namespace, and leaves live untouched. Counts a code
    /// change like a live commit.
    pub preview: Option<bool>,
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
    let (app, _) = visible_app(ctx, &input.app).await?;
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
    .map_err(super::rev_err)?;
    let files: Vec<Value> = files
        .iter()
        .map(|f| json!({"path": f.path, "size": f.size}))
        .collect();
    Ok(ok(
        format!("{} file(s) at {}.", files.len(), &sha[..12.min(sha.len())]),
        json!({"sha": sha, "files": files, "truncated": truncated}),
    ))
}

/// Most files one `read_files` call takes.
pub const MAX_READ_FILES: usize = 20;
/// Total numbered-text bytes one `read_files` call returns; the rest is cut
/// with `truncated`, and files past the budget fail inline.
pub const MAX_READ_OUTPUT_BYTES: usize = 200 * 1024;

/// Cut `s` at `cap` bytes on a UTF-8 boundary.
fn truncate_utf8(s: &str, cap: usize) -> &str {
    let mut end = cap.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub async fn read_files(input: ReadFilesInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    if input.files.is_empty() || input.files.len() > MAX_READ_FILES {
        return Err(ToolError::invalid_argument(format!(
            "files holds 1-{MAX_READ_FILES} entries"
        )));
    }
    let (app, _) = visible_app(ctx, &input.app).await?;
    if let Some(rev) = input.sha.as_deref() {
        check_rev("sha", rev)?;
    }
    let bare = mirror(ctx, &app.slug).await?;
    let head = crate::git::local::resolve_rev(&bare, input.sha.as_deref())
        .await
        .map_err(super::rev_err)?
        .ok_or_else(|| ToolError::not_found("app has no commits yet"))?;
    let mut used = 0usize;
    let mut budget_spent = false;
    let mut truncated = false;
    let mut ok_count = 0usize;
    let mut files = Vec::with_capacity(input.files.len());
    let mut data = Vec::with_capacity(input.files.len());
    for entry in &input.files {
        if budget_spent {
            files.push(json!({
                "path": entry.path,
                "code": "invalid_argument",
                "error": "skipped: the 200 KB output budget is exhausted by earlier files",
            }));
            data.push(format!(
                "--- {}: skipped (output budget exhausted)",
                entry.path
            ));
            truncated = true;
            continue;
        }
        match read_one(&bare, &head, entry).await {
            Ok((mut file, numbered)) => {
                if used + numbered.len() > MAX_READ_OUTPUT_BYTES {
                    let room = MAX_READ_OUTPUT_BYTES.saturating_sub(used);
                    let cut = truncate_utf8(&numbered, room);
                    // A file cut to nothing is an inline skip, not an empty read.
                    if cut.is_empty() {
                        files.push(json!({
                            "path": entry.path,
                            "code": "invalid_argument",
                            "error": "skipped: the 200 KB output budget is exhausted by earlier files",
                        }));
                        data.push(format!(
                            "--- {}: skipped (output budget exhausted)",
                            entry.path
                        ));
                    } else {
                        file["text"] = json!(cut);
                        file["truncated"] = json!(true);
                        files.push(file);
                        data.push(format!(
                            "--- {}: truncated at the 200 KB output budget\n{cut}",
                            entry.path
                        ));
                        ok_count += 1;
                    }
                    used = MAX_READ_OUTPUT_BYTES;
                    budget_spent = true;
                    truncated = true;
                } else {
                    used += numbered.len();
                    file["text"] = json!(numbered);
                    files.push(file);
                    data.push(format!("--- {}\n{numbered}", entry.path));
                    ok_count += 1;
                }
            }
            Err((code, message)) => {
                files.push(json!({"path": entry.path, "code": code, "error": message}));
                data.push(format!("--- {}: {message}", entry.path));
            }
        }
    }
    let short = &head[..12.min(head.len())];
    let summary = if truncated {
        format!(
            "{ok_count} of {} file(s) at {short} (truncated).",
            input.files.len()
        )
    } else {
        format!("{ok_count} of {} file(s) at {short}.", input.files.len())
    };
    Ok(ok_untrusted(
        summary,
        data.join("\n"),
        json!({"sha": head, "files": files, "truncated": truncated}),
    ))
}

/// Read one entry: `Ok` carries the structured file (without `text`) plus
/// the numbered text; `Err` carries the inline `(code, message)`.
async fn read_one(
    bare: &std::path::Path,
    head: &str,
    entry: &ReadFileEntry,
) -> Result<(Value, String), (&'static str, String)> {
    if let (Some(start), Some(end)) = (entry.start_line, entry.end_line) {
        if start < 1 || end < start {
            return Err((
                "invalid_argument",
                "start_line is 1-based and end_line must not precede it".to_string(),
            ));
        }
    }
    if entry.symbol.is_some() && (entry.start_line.is_some() || entry.end_line.is_some()) {
        return Err((
            "invalid_argument",
            "symbol cannot be combined with start_line/end_line".to_string(),
        ));
    }
    if let Err(e) = crate::git::local::validate_read_path(&entry.path) {
        return Err(("invalid_argument", e.to_string()));
    }
    let (_, bytes) = crate::git::local::read_blob(bare, Some(head), &entry.path)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("not found") {
                ("not_found", msg)
            } else if msg.contains("over the") || msg.contains("must") || msg.contains("path") {
                ("invalid_argument", msg)
            } else {
                (
                    "invalid_argument",
                    format!("cannot read {}: {msg}", entry.path),
                )
            }
        })?;
    if bytes.contains(&0) {
        return Err((
            "invalid_argument",
            format!("{} is binary ({} bytes)", entry.path, bytes.len()),
        ));
    }
    let text = String::from_utf8(bytes).map_err(|e| {
        (
            "invalid_argument",
            format!("{} is not UTF-8: {e}", entry.path),
        )
    })?;
    let total = text.lines().count() as u64;
    let blob = crate::git::local::blob_sha(bare, Some(head), &entry.path)
        .await
        .map(|(_, blob)| blob)
        .unwrap_or_default();
    if let Some(symbol) = entry.symbol.as_deref() {
        let decl = crate::git::symbol::extract_symbol(&entry.path, &text, symbol)
            .map_err(|e| ("invalid_argument", e.to_string()))?;
        let numbered = number_lines(&text, decl.start_line, decl.end_line);
        let file = json!({
            "path": entry.path,
            "blob": blob,
            "total_lines": total,
            "start_line": decl.start_line,
            "end_line": decl.end_line,
            "symbol": symbol,
        });
        return Ok((file, numbered));
    }
    let start = entry.start_line.unwrap_or(1).max(1);
    let end = entry.end_line.unwrap_or(u64::MAX).min(total);
    let numbered = number_lines(&text, start, end);
    let file = json!({
        "path": entry.path,
        "blob": blob,
        "total_lines": total,
        "start_line": start,
        "end_line": end.min(total),
    });
    Ok((file, numbered))
}

/// Lines `start..=end` (1-based, already clamped) numbered as `N│ line`.
fn number_lines(text: &str, start: u64, end: u64) -> String {
    if end < start {
        return String::new();
    }
    text.lines()
        .enumerate()
        .skip(start.saturating_sub(1) as usize)
        .take(end.saturating_sub(start.saturating_sub(1)) as usize)
        .map(|(i, line)| format!("{}│ {line}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A `word` identifier for `kind: "symbol"`: ASCII letters, digits, `_`
/// and `$`, not starting with a digit.
fn is_symbol_query(query: &str) -> bool {
    let mut chars = query.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    query
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

pub async fn search_code(input: SearchCodeInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let symbol = match input.kind.as_deref() {
        None | Some("text") => false,
        Some("symbol") => true,
        Some(other) => {
            return Err(ToolError::invalid_argument(format!(
                "kind is \"text\" or \"symbol\", not {other:?}"
            )));
        }
    };
    let context = input.context.unwrap_or(0);
    if context > 5 {
        return Err(ToolError::invalid_argument("context is 0-5"));
    }
    if input.paths.as_ref().is_some_and(|p| p.len() > 20) {
        return Err(ToolError::invalid_argument("at most 20 path globs"));
    }
    if symbol {
        if input.regex.unwrap_or(false) {
            return Err(ToolError::invalid_argument(
                "regex has no effect with kind: \"symbol\"",
            ));
        }
        if !is_symbol_query(&input.query) {
            return Err(ToolError::invalid_argument(
                "kind: \"symbol\" needs one identifier (letters, digits, _ and $, not starting with a digit)",
            ));
        }
    }
    let (app, _) = visible_app(ctx, &input.app).await?;
    let bare = mirror(ctx, &app.slug).await?;
    let opts = crate::git::local::GrepOptions {
        regex: input.regex.unwrap_or(false) && !symbol,
        case_insensitive: input.case_insensitive.unwrap_or(false),
        word: input.word.unwrap_or(false) || symbol,
        paths: input.paths.clone().unwrap_or_default(),
        include_generated: input.include_generated.unwrap_or(false),
        context,
    };
    let (sha, files, truncated) = crate::git::local::grep_search(&bare, None, &input.query, &opts)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("query")
                || msg.contains("regex")
                || msg.contains("context")
                || msg.contains("glob")
            {
                ToolError::invalid_argument(msg)
            } else {
                internal(e)
            }
        })?;
    if symbol {
        return search_symbol(
            &bare,
            &sha,
            files,
            truncated,
            &input.query,
            opts.case_insensitive,
        )
        .await;
    }
    let mut structured_files = Vec::with_capacity(files.len());
    let mut data = Vec::new();
    let mut total = 0usize;
    for file in &files {
        let mut matches = Vec::with_capacity(file.matches.len());
        for hit in &file.matches {
            total += 1;
            data.push(format!("{}:{}:{}", file.path, hit.line, hit.text));
            let mut m = json!({"line": hit.line, "text": hit.text});
            if !hit.before.is_empty() {
                m["before"] = json!(hit.before);
            }
            if !hit.after.is_empty() {
                m["after"] = json!(hit.after);
            }
            matches.push(m);
        }
        structured_files.push(json!({"path": file.path, "matches": matches}));
    }
    Ok(ok_untrusted(
        format!("{total} match(es) in {} file(s).", files.len()),
        if data.is_empty() {
            "(no matches)".to_string()
        } else {
            data.join("\n")
        },
        json!({"sha": sha, "files": structured_files, "truncated": truncated}),
    ))
}

/// Split word matches into definitions (declaration lines for `query`) and
/// usages (every other match). Files the parser refuses contribute usages
/// only — an unparseable file never hides a hit.
async fn search_symbol(
    bare: &std::path::Path,
    sha: &str,
    files: Vec<crate::git::local::FileHits>,
    truncated: bool,
    query: &str,
    case_insensitive: bool,
) -> Result<Value, ToolError> {
    let same = |name: &str| {
        if case_insensitive {
            name.eq_ignore_ascii_case(query)
        } else {
            name == query
        }
    };
    let mut structured_files = Vec::with_capacity(files.len());
    let mut data = Vec::new();
    let mut defs = 0usize;
    let mut uses = 0usize;
    for file in &files {
        let mut decl_lines: Vec<(u64, u64)> = Vec::new();
        if let Ok((_, bytes)) = crate::git::local::read_blob(bare, Some(sha), &file.path).await {
            if !bytes.contains(&0) {
                if let Ok(text) = String::from_utf8(bytes) {
                    if let Ok(decls) = crate::git::symbol::top_level_decls(&file.path, &text) {
                        decl_lines = decls
                            .iter()
                            .filter(|d| same(&d.name))
                            .map(|d| (d.start_line, d.end_line))
                            .collect();
                    }
                }
            }
        }
        let mut definitions = Vec::new();
        let mut usages = Vec::new();
        for hit in &file.matches {
            match decl_lines.iter().find(|(start, _)| *start == hit.line) {
                Some((start, end)) => {
                    defs += 1;
                    data.push(format!("{}:{start}:def {query}", file.path));
                    definitions.push(json!({"line": start, "end_line": end}));
                }
                None => {
                    uses += 1;
                    data.push(format!("{}:{}:{}", file.path, hit.line, hit.text));
                    let mut m = json!({"line": hit.line, "text": hit.text});
                    if !hit.before.is_empty() {
                        m["before"] = json!(hit.before);
                    }
                    if !hit.after.is_empty() {
                        m["after"] = json!(hit.after);
                    }
                    usages.push(m);
                }
            }
        }
        structured_files
            .push(json!({"path": file.path, "definitions": definitions, "usages": usages}));
    }
    Ok(ok_untrusted(
        format!(
            "{defs} definition(s), {uses} usage(s) in {} file(s).",
            files.len()
        ),
        if data.is_empty() {
            "(no matches)".to_string()
        } else {
            data.join("\n")
        },
        json!({"sha": sha, "files": structured_files, "truncated": truncated}),
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
            expect_blob: w.expect_blob.clone(),
        });
    }
    let mut edits = Vec::new();
    for e in input.edits.as_deref().unwrap_or(&[]) {
        edits.push(git::Edit {
            path: e.path.clone(),
            old: e.old.clone(),
            new: e.new.clone(),
            replace_all: e.replace_all.unwrap_or(false),
            expect_blob: e.expect_blob.clone(),
        });
    }
    let mut line_edits = Vec::new();
    for e in input.line_edits.as_deref().unwrap_or(&[]) {
        line_edits.push(git::LineEdit {
            path: e.path.clone(),
            start_line: e.start_line,
            end_line: e.end_line,
            new: e.new.clone(),
            expect_blob: e.expect_blob.clone(),
        });
    }
    let mut inserts = Vec::new();
    for i in input.inserts.as_deref().unwrap_or(&[]) {
        inserts.push(git::Insert {
            path: i.path.clone(),
            after_line: i.after_line,
            text: i.insert.clone(),
            expect_blob: i.expect_blob.clone(),
        });
    }
    Ok(git::CommitRequest {
        message: input.message.clone(),
        base: input.base.clone(),
        changes: git::CommitChanges {
            writes,
            edits,
            line_edits,
            inserts,
            patch: input.patch.clone(),
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
        },
        author,
    })
}

pub async fn commit(input: CommitInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    if input.preview.unwrap_or(false) {
        return commit_to_preview(input, ctx).await;
    }
    let (app, _) = visible_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    let author = noreply_author(&caller.account_id, &caller.email, &ctx.state.domain);
    let request = to_commit_request(&input, author)?;
    if let Some(base) = input.base.as_deref() {
        check_rev("base", base)?;
    }
    // Claimed before the git work: a commit that never happens (bad base,
    // moved head) gives the change back; a created commit counts even when
    // its deploy fails or is blocked.
    // A refused build spends nothing: the build-minutes check runs before
    // the change claim, so a capped account keeps its code changes.
    // Charged to the app owner whoever calls (spec §3): a collaborator's
    // commit counts against the owner's daily changes.
    check_build_quota(&ctx.state.pool, &ctx.state.billing, &app.owner_id).await?;
    check_storage_writable(&ctx.state.pool, &ctx.state.billing, &app).await?;
    let claim = claim_change(&ctx.state.pool, &ctx.state.billing, &app.owner_id).await?;
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
            if let Err(refund) = refund_change(&ctx.state.pool, &app.owner_id).await {
                tracing::warn!("commit refund failed: {refund:#}");
            }
            return Err(ToolError::from_commit(e));
        }
    };
    let sha = outcome.sha;
    let deploy = run_deploy(ctx.state, &app, &sha, input.wait.unwrap_or(true)).await?;
    // Quota threshold emails (alerts #2), off the critical path.
    super::spawn_quota_email(ctx.state, &app.owner_id, claim.used, claim.limit);
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

/// The `commit {preview: true}` path: the Pro gate, a host-cap fast check
/// and the change claim all run BEFORE the git work (a refused preview burns
/// no quota and writes nothing). The commit lands on the preview ref, the
/// row names the fleet (reusing ports across refreshes of one preview
/// lifetime; the cap is enforced again with the port reservation, rolling
/// the git work back when a rival filled the last slot), and the deploy runs
/// under the preview identity with a fresh data namespace. A failed deploy
/// still counts the change, exactly like a live commit.
async fn commit_to_preview(input: CommitInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    super::preview::require_pro(
        ctx,
        &app.owner_id,
        &app.slug,
        "preview deploys",
        "are",
        "try changes at a temporary URL before they go live",
    )
    .await?;
    let caller = authed(ctx)?;
    let author = noreply_author(&caller.account_id, &caller.email, &ctx.state.domain);
    let request = to_commit_request(&input, author)?;
    if let Some(base) = input.base.as_deref() {
        check_rev("base", base)?;
    }
    // One preview per app; the host cap bounds total preview fleets. A
    // refresh of the existing preview needs no new slot.
    if crate::preview::preview_of(&app).is_none() {
        let max = ctx.state.fleets.preview_max();
        let used = crate::preview::count_previews(&ctx.state.pool)
            .await
            .map_err(internal)?;
        if used >= max as u64 {
            let retry_after_s = crate::preview::oldest_preview_touch(&ctx.state.pool)
                .await
                .map_err(internal)?
                .map(|oldest| {
                    (oldest + crate::preview::PREVIEW_IDLE_TTL_MS - crate::apps::now_ms()).max(1)
                        as u64
                })
                .unwrap_or(60);
            return Err(ToolError::rate_limited(
                format!("preview capacity is full ({used}/{max} active previews)"),
                retry_after_s,
            ));
        }
    }
    check_build_quota(&ctx.state.pool, &ctx.state.billing, &app.owner_id).await?;
    check_storage_writable(&ctx.state.pool, &ctx.state.billing, &app).await?;
    let claim = claim_change(&ctx.state.pool, &ctx.state.billing, &app.owner_id).await?;
    let outcome = match git::commit::commit_preview(
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
            if let Err(refund) = refund_change(&ctx.state.pool, &app.owner_id).await {
                tracing::warn!("commit refund failed: {refund:#}");
            }
            return Err(ToolError::from_commit(e));
        }
    };
    let sha = outcome.sha;
    // Ports are claimed once per preview lifetime and reused across
    // refreshes, so a running preview fleet's ports never churn under it.
    // The reservation runs after the commit (the sha is an input) under one
    // lock: the second of two racing first-time commits already sees the
    // first's ports, and a cap filled in between rolls the git work back.
    let (port, internal_port) = match crate::preview::preview_of(&app) {
        Some(existing) => (existing.port, existing.internal_port),
        None => {
            let max = ctx.state.fleets.preview_max();
            match crate::apps::claim_preview_ports(&ctx.state.pool, &ctx.state.alloc, &app.id, max)
                .await
            {
                Ok(ports) => ports,
                Err(e) if e.is::<crate::apps::PreviewCapFull>() => {
                    // A rival preview took the last slot after the
                    // pre-commit check: drop the just-published ref (it
                    // names no row) and refuse without burning quota.
                    let _ = crate::git::manifest::clear_preview(
                        &ctx.state.store,
                        &ctx.state.git,
                        &ctx.state.sync,
                        &app.slug,
                    )
                    .await;
                    if let Err(refund) = refund_change(&ctx.state.pool, &app.owner_id).await {
                        tracing::warn!("commit refund failed: {refund:#}");
                    }
                    let retry_after_s = crate::preview::oldest_preview_touch(&ctx.state.pool)
                        .await
                        .map_err(internal)?
                        .map(|oldest| {
                            (oldest + crate::preview::PREVIEW_IDLE_TTL_MS - crate::apps::now_ms())
                                .max(1) as u64
                        })
                        .unwrap_or(60);
                    return Err(ToolError::rate_limited(e.to_string(), retry_after_s));
                }
                Err(e) => return Err(internal(e)),
            }
        }
    };
    let now = crate::apps::now_ms();
    crate::preview::set_preview(&ctx.state.pool, &app.id, &sha, now, port, internal_port)
        .await
        .map_err(internal)?;
    // Re-read: the deploy runs under the preview identity, which lives in
    // the row this just wrote — the `app` above predates it.
    let app = crate::apps::get_by_id(&ctx.state.pool, &app.id)
        .await
        .map_err(internal)?
        .ok_or_else(|| internal(anyhow::anyhow!("app {} vanished mid-commit", app.slug)))?;
    let deploy =
        super::run_preview_deploy(ctx.state, &app, &sha, input.wait.unwrap_or(true)).await?;
    super::spawn_quota_email(ctx.state, &app.owner_id, claim.used, claim.limit);
    let quota =
        json!({"limit": claim.limit, "used": claim.used, "resets_at": super::resets_at_utc()});
    let preview_url = crate::preview::preview_url(
        &ctx.state.app_scheme,
        &app.slug,
        &ctx.state.apps_domain,
        &ctx.state.app_port_suffix,
    );
    let mut summary = format!(
        "Committed {} to the preview ({}). Live is untouched; try it at {}.",
        &sha[..12.min(sha.len())],
        deploy["status"].as_str().unwrap_or(""),
        preview_url,
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
    let structured = json!({
        "commit": commit,
        "deploy": deploy,
        "quota": quota,
        "preview_url": preview_url,
    });
    Ok(ok_deploy(
        summary,
        &structured["deploy"].clone(),
        structured,
    ))
}

pub async fn history(input: HistoryInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
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
    let (app, _) = visible_app(ctx, &input.app).await?;
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
    let (app, _) = visible_app(ctx, &input.app).await?;
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
/// role-checked by the caller (MCP's `visible_app` and the console's
/// `mutation_app` refuse role-less slugs as `not_found`; owner-only callers
/// answer 403 before reaching here).
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
            let deploy = super::await_row(
                state,
                app,
                Some(&row.deploy_id),
                wait,
                crate::deploy::DeployKind::Main,
            )
            .await?;
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
    // A refused build spends nothing: the rollback budget is refunded, so a
    // capped account can still roll back after midnight.
    if let Err(err) = super::check_build_quota(&state.pool, &state.billing, &app.owner_id).await {
        if let Err(refund) = super::refund_rollback(&state.pool, &app.id).await {
            tracing::warn!("rollback refund failed: {refund:#}");
        }
        return Err(err);
    }
    // A frozen app refuses the write like a capped build does: same refund.
    if let Err(err) = super::check_storage_writable(&state.pool, &state.billing, app).await {
        if let Err(refund) = super::refund_rollback(&state.pool, &app.id).await {
            tracing::warn!("rollback refund failed: {refund:#}");
        }
        return Err(err);
    }
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
