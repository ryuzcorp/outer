//! The draft tools: `stage`, `diff_draft`, `check`, `ship`, `discard_draft`
//! (spec §1.5).
//!
//! A draft is per account per app: staged ops in the `commit` shapes plus
//! the main head they were staged against, stored in the `draft` table so
//! they survive restarts. `stage` validates and accumulates without
//! deploying and without spending a code change; `ship` commits the draft
//! as ONE commit through the normal commit path (gate, deploy, one code
//! change) and clears it. A head that moved under a draft makes `ship`
//! rebase when the ops still apply cleanly, else answer `conflict`.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use super::code::{CommitInput, EditInput, FileWriteInput, MoveInput};
use super::diagnostics::parse_diagnostics;
use super::{authed, check_build_quota, internal, ok, visible_app, Ctx};
use crate::git::{DraftEdit, DraftMove, DraftOps, DraftWrite};
use crate::mcp::error::ToolError;
use crate::ops::noreply_author;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StageInput {
    /// The app slug.
    pub app: String,
    /// Full-file writes (new files or replacements).
    pub writes: Option<Vec<FileWriteInput>>,
    /// Exact-match replacements in existing files.
    pub edits: Option<Vec<EditInput>>,
    /// Paths to delete.
    pub deletes: Option<Vec<String>>,
    /// File renames.
    pub moves: Option<Vec<MoveInput>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DraftAppInput {
    /// The app slug.
    pub app: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckInput {
    /// The app slug.
    pub app: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShipInput {
    /// The app slug.
    pub app: String,
    /// Commit message, 1-500 chars.
    pub message: String,
    /// Wait for the deploy (default true). False returns immediately;
    /// poll deploy_status.
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiscardDraftInput {
    /// The app slug.
    pub app: String,
}

/// The stored draft: base head plus staged ops.
struct StoredDraft {
    base_sha: String,
    ops: DraftOps,
}

async fn load_draft(
    pool: &SqlitePool,
    app_id: &str,
    account_id: &str,
) -> Result<Option<StoredDraft>, ToolError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT base_sha, ops FROM draft WHERE app_id = ?1 AND account_id = ?2")
            .bind(app_id)
            .bind(account_id)
            .fetch_optional(pool)
            .await
            .map_err(internal)?;
    row.map(|(base_sha, ops)| {
        let ops: DraftOps = serde_json::from_str(&ops).map_err(internal)?;
        Ok(StoredDraft { base_sha, ops })
    })
    .transpose()
}

async fn save_draft(
    pool: &SqlitePool,
    app_id: &str,
    account_id: &str,
    base_sha: &str,
    ops: &DraftOps,
) -> Result<(), ToolError> {
    let ops_json = serde_json::to_string(ops).map_err(internal)?;
    sqlx::query(
        "INSERT INTO draft (app_id, account_id, base_sha, ops, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(app_id, account_id) DO UPDATE \
         SET ops = excluded.ops, updated_at = excluded.updated_at",
    )
    .bind(app_id)
    .bind(account_id)
    .bind(base_sha)
    .bind(ops_json)
    .bind(crate::apps::now_ms())
    .execute(pool)
    .await
    .map_err(internal)?;
    Ok(())
}

async fn clear_draft(pool: &SqlitePool, app_id: &str, account_id: &str) -> Result<bool, ToolError> {
    let rows = sqlx::query("DELETE FROM draft WHERE app_id = ?1 AND account_id = ?2")
        .bind(app_id)
        .bind(account_id)
        .execute(pool)
        .await
        .map_err(internal)?
        .rows_affected();
    Ok(rows > 0)
}

/// The mirror head, or `not_found` when the app has no commits yet.
async fn draft_head(bare: &std::path::Path) -> Result<String, ToolError> {
    crate::git::manifest::head_of(bare)
        .await
        .map_err(internal)?
        .ok_or_else(|| ToolError::not_found("app has no commits yet"))
}

fn ops_summary(ops: &DraftOps) -> Value {
    let mut files: Vec<String> = ops.writes.iter().map(|w| w.path.clone()).collect();
    files.extend(ops.edits.iter().map(|e| e.path.clone()));
    files.extend(ops.deletes.iter().cloned());
    files.extend(ops.moves.iter().map(|m| format!("{} -> {}", m.from, m.to)));
    files.sort();
    files.dedup();
    json!({
        "writes": ops.writes.len(),
        "edits": ops.edits.len(),
        "deletes": ops.deletes.len(),
        "moves": ops.moves.len(),
        "files": files,
    })
}

/// Collect one `stage` call's ops into draft shapes. Takes the input by
/// value: the tool input structs share `commit`'s shapes, which are not
/// `Clone`, so the fields move instead of copying.
fn stage_ops(input: StageInput) -> DraftOps {
    DraftOps {
        writes: input
            .writes
            .unwrap_or_default()
            .into_iter()
            .map(|w| DraftWrite {
                path: w.path,
                content: w.content,
                base64: w.base64,
            })
            .collect(),
        edits: input
            .edits
            .unwrap_or_default()
            .into_iter()
            .map(|e| DraftEdit {
                path: e.path,
                old: e.old,
                new: e.new,
                replace_all: e.replace_all.unwrap_or(false),
            })
            .collect(),
        deletes: input.deletes.unwrap_or_default(),
        moves: input
            .moves
            .unwrap_or_default()
            .into_iter()
            .map(|m| DraftMove {
                from: m.from,
                to: m.to,
            })
            .collect(),
    }
}

/// Map staged ops back onto a commit input for `ship`.
fn ship_input(app: &str, message: String, base: String, ops: &DraftOps, wait: Option<bool>) -> CommitInput {
    CommitInput {
        app: app.to_string(),
        message,
        base: Some(base),
        writes: if ops.writes.is_empty() {
            None
        } else {
            Some(
                ops.writes
                    .iter()
                    .map(|w| FileWriteInput {
                        path: w.path.clone(),
                        content: w.content.clone(),
                        base64: w.base64.clone(),
                    })
                    .collect(),
            )
        },
        edits: if ops.edits.is_empty() {
            None
        } else {
            Some(
                ops.edits
                    .iter()
                    .map(|e| EditInput {
                        path: e.path.clone(),
                        old: e.old.clone(),
                        new: e.new.clone(),
                        replace_all: Some(e.replace_all),
                    })
                    .collect(),
            )
        },
        deletes: if ops.deletes.is_empty() {
            None
        } else {
            Some(ops.deletes.clone())
        },
        moves: if ops.moves.is_empty() {
            None
        } else {
            Some(
                ops.moves
                    .iter()
                    .map(|m| MoveInput {
                        from: m.from.clone(),
                        to: m.to.clone(),
                    })
                    .collect(),
            )
        },
        wait,
        preview: None,
    }
}

/// Stage changes into the caller's draft: no deploy, no code change. The
/// merged ops validate exactly like a commit, so a `ship` later cannot fail
/// on shapes.
pub async fn stage(input: StageInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    let staged = stage_ops(input);
    if staged.is_empty() {
        return Err(ToolError::invalid_argument(
            "stage needs at least one write, edit, move or delete",
        ));
    }
    let bare = crate::git::manifest::ensure_synced(
        &ctx.state.store,
        &ctx.state.git,
        &ctx.state.sync,
        &app.slug,
    )
    .await
    .map_err(internal)?;
    let head = draft_head(&bare).await?;
    let mut draft = load_draft(&ctx.state.pool, &app.id, &caller.account_id)
        .await?
        .map(|d| d.ops)
        .unwrap_or_default();
    draft.append(staged.writes, staged.edits, staged.deletes, staged.moves);
    crate::git::drafts::validate_ops(&draft).map_err(ToolError::from_commit)?;
    save_draft(&ctx.state.pool, &app.id, &caller.account_id, &head, &draft).await?;
    // The base pins at the first stage: later stages accumulate onto it and
    // `ship` rebases when the head moved underneath.
    let base = load_draft(&ctx.state.pool, &app.id, &caller.account_id)
        .await?
        .map(|d| d.base_sha)
        .unwrap_or(head);
    let summary = format!(
        "Staged {} file(s) in the draft for {}.",
        ops_summary(&draft)["files"].as_array().map(Vec::len).unwrap_or(0),
        app.slug,
    );
    Ok(ok(
        summary,
        json!({
            "draft": {"app": app.slug, "base": base, "staged": ops_summary(&draft)},
            "next": "diff_draft {app} shows the staged diff; check {app} verifies it; ship {app, message} deploys it as one commit",
        }),
    ))
}

/// Show the staged unified diff, base head to draft tree.
pub async fn diff_draft(input: DraftAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    let draft = load_draft(&ctx.state.pool, &app.id, &caller.account_id)
        .await?
        .ok_or_else(|| ToolError::not_found(format!("no draft for {:?}", app.slug)))?;
    let bare = crate::git::manifest::ensure_synced(
        &ctx.state.store,
        &ctx.state.git,
        &ctx.state.sync,
        &app.slug,
    )
    .await
    .map_err(internal)?;
    let author = noreply_author(&caller.account_id, &caller.email, &ctx.state.domain);
    let draft_sha = crate::git::drafts::build_draft_commit(&bare, &draft.base_sha, &draft.ops, &author)
        .await
        .map_err(ToolError::from_commit)?;
    let (from, to, stats, patch, truncated) =
        crate::git::local::diff_range(&bare, &draft.base_sha, &draft_sha)
            .await
            .map_err(internal)?;
    let files: Vec<Value> = stats
        .iter()
        .map(|s| {
            json!({
                "path": s.path,
                "status": s.status.to_string(),
                "added": s.added,
                "removed": s.removed,
            })
        })
        .collect();
    Ok(ok(
        format!("Draft for {}: {} file(s) changed.", app.slug, files.len()),
        json!({
            "draft": {
                "app": app.slug,
                "base": from,
                "tree": to,
                "files": files,
                "patch": patch,
                "truncated": truncated,
            },
            "next": "check {app} verifies the draft; ship {app, message} deploys it as one commit",
        }),
    ))
}

/// Verify the draft in the build sandbox: install, build, typecheck, lint
/// and tests. Never deploys, never runs the content gate, never spends a
/// code change — the wall time lands on the owner's daily build minutes.
pub async fn check(input: CheckInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    let draft = load_draft(&ctx.state.pool, &app.id, &caller.account_id)
        .await?
        .ok_or_else(|| ToolError::not_found(format!("no draft for {:?}", app.slug)))?;
    // Admission runs before the work, next to the commit path's own checks:
    // a refused build spends nothing.
    check_build_quota(&ctx.state.pool, &ctx.state.billing, &app.owner_id).await?;
    let bare = crate::git::manifest::ensure_synced(
        &ctx.state.store,
        &ctx.state.git,
        &ctx.state.sync,
        &app.slug,
    )
    .await
    .map_err(internal)?;
    let author = noreply_author(&caller.account_id, &caller.email, &ctx.state.domain);
    let draft_sha = crate::git::drafts::build_draft_commit(&bare, &draft.base_sha, &draft.ops, &author)
        .await
        .map_err(ToolError::from_commit)?;
    let started = std::time::Instant::now();
    let output = ctx
        .state
        .deploys
        .check(&ctx.state.pool, &ctx.state.git, &app, &draft_sha)
        .await
        .map_err(internal)?;
    let charged_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    // Accounting never fails the check: a lost charge only warns, exactly
    // like the pipeline's own build-minute charge.
    if let Err(err) = crate::limits::add_build_ms(&ctx.state.pool, &app.owner_id, charged_ms).await
    {
        tracing::warn!("check build-minute charge failed: {err:#}");
    }
    let diagnostics = parse_diagnostics(&output.log);
    let status = if output.steps.iter().any(|s| !s.skipped && !s.ok) {
        "fail"
    } else {
        "pass"
    };
    let summary = if status == "pass" {
        format!("Check passed for {} ({} diagnostic(s)).", app.slug, diagnostics.len())
    } else {
        format!(
            "Check failed for {} ({} diagnostic(s)): fix them, stage again, re-check.",
            app.slug,
            diagnostics.len()
        )
    };
    Ok(ok(
        summary,
        json!({
            "check": {
                "app": app.slug,
                "status": status,
                "steps": output.steps,
                "diagnostics": diagnostics.iter().map(|d| d.json()).collect::<Vec<_>>(),
                "log_tail": crate::mcp::tools::tail_8k(&output.log),
                "build_ms_charged": charged_ms,
            },
            "next": if status == "pass" {
                "ship {app, message} deploys the draft as one commit".to_string()
            } else {
                "read the diagnostics, stage corrections, check again; discard_draft {app} drops it".to_string()
            },
        }),
    ))
}

/// Ship the draft as ONE commit through the normal commit path (gate,
/// deploy, one code change), then clear it. A head that moved under the
/// draft rebases when the ops still apply cleanly, else answers `conflict`
/// with `current_sha` and keeps the draft.
pub async fn ship(input: ShipInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    let draft = load_draft(&ctx.state.pool, &app.id, &caller.account_id)
        .await?
        .ok_or_else(|| ToolError::not_found(format!("no draft for {:?}", app.slug)))?;
    let bare = crate::git::manifest::ensure_synced(
        &ctx.state.store,
        &ctx.state.git,
        &ctx.state.sync,
        &app.slug,
    )
    .await
    .map_err(internal)?;
    let head = draft_head(&bare).await?;
    // The rebase check: the ops must still apply cleanly on the current
    // head. A moved head with stale ops is a conflict; the draft stays so
    // the agent can diff, fix and ship again.
    let author = noreply_author(&caller.account_id, &caller.email, &ctx.state.domain);
    if let Err(e) = crate::git::drafts::build_draft_commit(&bare, &head, &draft.ops, &author).await
    {
        if head == draft.base_sha {
            return Err(ToolError::from_commit(e));
        }
        return Err(ToolError::conflict(
            format!(
                "the draft no longer applies cleanly: main moved to {head}; diff the draft, stage a fix and ship again"
            ),
            &head,
        )
        .with_user(format!(
            "Someone changed the code since this draft was staged. Read the latest files, stage a fix and ship again with base {head}."
        )));
    }
    let rebased = head != draft.base_sha;
    let commit_input = ship_input(&app.slug, input.message, head, &draft.ops, input.wait);
    let mut result = super::code::commit(commit_input, ctx).await?;
    // The commit exists now (the deploy may still have failed): clear the
    // draft exactly once, so a retry ships nothing twice.
    clear_draft(&ctx.state.pool, &app.id, &caller.account_id).await?;
    if let Some(structured) = result.get_mut("structuredContent") {
        structured["draft"] = json!({"cleared": true, "rebased": rebased});
    }
    Ok(result)
}

/// Drop the caller's draft. Always allowed; idempotent.
pub async fn discard_draft(input: DiscardDraftInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    let dropped = clear_draft(&ctx.state.pool, &app.id, &caller.account_id).await?;
    Ok(ok(
        if dropped {
            format!("Discarded the draft for {}.", app.slug)
        } else {
            format!("No draft for {}.", app.slug)
        },
        json!({
            "draft": {"app": app.slug, "dropped": dropped},
            "next": "stage {app, ...} starts a new draft",
        }),
    ))
}
