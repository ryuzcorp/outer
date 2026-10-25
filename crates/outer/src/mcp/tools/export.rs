//! The `export_code` tool (spec §7.5, Pro): a zip download of the whole app
//! tree at a commit. Owner only: a collaborator's call answers `forbidden`
//! (via [`owner_app`]), and a slug with no role answers `not_found`,
//! exactly like a missing one.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{check_rev, ok, owner_app, plan_of, Ctx};
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

pub async fn export_code(input: ExportCodeInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    if let Some(sha) = input.sha.as_deref() {
        check_rev("sha", sha)?;
    }
    let plan = plan_of(&ctx.state.pool, &app.owner_id).await?;
    if plan != "pro" {
        return Err(ToolError::payment_required(
            format!(
                "{} is on a Free account; code export is a Pro feature",
                app.slug
            ),
            "Code export is a Pro feature. Upgrade to Pro to download your app as a zip.",
        ));
    }
    let outcome = export::export_app(
        &export::ExportDeps {
            pool: &ctx.state.pool,
            store: &ctx.state.store,
            git: &ctx.state.git,
            sync: &ctx.state.sync,
        },
        export::ExportParams {
            slug: &app.slug,
            app_id: &app.id,
            rev: input.sha.as_deref(),
            caps: &export::ExportCaps::default(),
            now: chrono::Utc::now(),
        },
    )
    .await
    .map_err(|err| tool_error(&app.slug, err))?;
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
