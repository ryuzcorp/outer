//! The `export_code` tool (spec §7.5): a zip download of the whole app tree
//! at a commit. Available on every plan; owner only: a collaborator's call
//! answers `forbidden` (via [`owner_app`]), and a slug with no role answers
//! `not_found`, exactly like a missing one.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{check_rev, ok, owner_app, Ctx};
use crate::git::export;
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportCodeInput {
    /// The app slug.
    pub app: String,
    /// A commit sha (full or short). Defaults to `main`.
    pub sha: Option<String>,
}

/// An [`export::ExportError`] as the MCP [`ToolError`] the tool and the
/// console endpoint report (same codes, same user message).
pub(crate) fn tool_error(app: &str, err: export::ExportError) -> ToolError {
    use export::ExportError as E;
    match err {
        E::NoRepo => {
            ToolError::invalid_argument(format!("{app} has no commits yet; nothing to export"))
        }
        E::UnknownRev(rev) => {
            ToolError::invalid_argument(format!("{rev:?} is not a commit on {app}"))
        }
        E::TooManyFiles(count) => ToolError::invalid_argument(format!(
            "{app} has {count} files at that commit, over the export cap"
        )),
        E::FileTooLarge { path, size } => {
            ToolError::invalid_argument(format!("{path} is {size} bytes, over the export file cap"))
        }
        E::TooLarge { bytes } => ToolError::invalid_argument(format!(
            "that commit's tree is {bytes} bytes, over the export cap"
        )),
        E::Internal(err) => {
            tracing::warn!("export_code internal: {err:#}");
            ToolError::internal("the export failed on our side; try again in a minute")
        }
    }
}

/// The zip build, shared by the `export_code` tool and the console
/// `POST /api/console/apps/:slug/export` endpoint (same caps, same
/// messages). Export is available on every plan (owner-only stays): the
/// caller checks the role first; this builds the zip. A plan-lookup failure
/// propagates as `internal`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn export_for_owner(
    pool: &sqlx::SqlitePool,
    store: &crate::store::Store,
    git: &crate::git::manifest::GitConfig,
    sync: &crate::git::manifest::GitSync,
    slug: &str,
    app_id: &str,
    rev: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<export::ExportOutcome, ToolError> {
    if let Some(rev) = rev {
        check_rev("sha", rev)?;
    }
    export::export_app(
        &export::ExportDeps {
            pool,
            store,
            git,
            sync,
        },
        export::ExportParams {
            slug,
            app_id,
            rev,
            caps: &export::ExportCaps::default(),
            now,
        },
    )
    .await
    .map_err(|err| tool_error(slug, err))
}

pub async fn export_code(input: ExportCodeInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    let outcome = export_for_owner(
        &ctx.state.pool,
        &ctx.state.store,
        &ctx.state.git,
        &ctx.state.sync,
        &app.slug,
        &app.id,
        input.sha.as_deref(),
        chrono::Utc::now(),
    )
    .await?;
    Ok(ok(
        format!(
            "Exported {} at {} ({} bytes, link good for one hour).",
            app.slug,
            &outcome.sha[..7.min(outcome.sha.len())],
            outcome.bytes,
        ),
        json!({
            "url": outcome.url,
            "expires_at": outcome.expires_at,
            "sha": outcome.sha,
            "bytes": outcome.bytes,
        }),
    ))
}
