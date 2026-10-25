//! The `look_at_app` tool: render the live app page in Cloudflare's
//! browser and return a screenshot plus the navigation status.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{authed, visible_app, Ctx};
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LookAtAppInput {
    /// The app slug.
    pub app: String,
    /// Page path below the app root (default `"/"`).
    pub path: Option<String>,
    /// Viewport width: `390` (phone) or `1280` (desktop, default).
    pub width: Option<u32>,
    /// Capture the whole page, not just the viewport (default false).
    pub full_page: Option<bool>,
}

/// The two widths Cloudflare renders at: phone and desktop. Anything
/// else is the caller's bug (`invalid_argument`), never a render.
const WIDTHS: [u32; 2] = [390, 1280];

/// Longest path accepted: a URL path, not a body.
const MAX_PATH_LEN: usize = 2048;

/// The untrusted-data notice for a screenshot: the pixels are a
/// rendering of app content, so the same §8.4 rule applies as to text.
fn image_notice(summary: &str) -> String {
    format!("{summary}\n\n{}", super::UNTRUSTED_NOTICE)
}

/// Check the caller-supplied path before it joins the app host. Only a
/// same-host path is ever built: it must start with `/`, and carry no
/// backslash or control characters (no scheme, host or CRLF to smuggle).
fn check_path(path: &str) -> Result<(), ToolError> {
    if path.len() > MAX_PATH_LEN
        || !path.starts_with('/')
        || path.bytes().any(|b| b == b'\\' || b < 0x20 || b == 0x7F)
    {
        return Err(ToolError::invalid_argument(format!(
            "bad path {path:?}: expected a same-app path starting with '/'"
        )));
    }
    Ok(())
}

/// The host the browser fetches: the app's active custom domain when it
/// has one, else its `<slug>.<apps_domain>` host. Both come from our own
/// rows, never from caller input, so the browser can only ever reach the
/// caller's own visible apps (unlisted apps work: Cloudflare fetches the
/// public URL from outside).
async fn render_host(ctx: &Ctx<'_>, app: &crate::apps::App) -> Result<String, ToolError> {
    let stored = crate::apps::domains::stored_for_app(&ctx.state.pool, &app.id)
        .await
        .map_err(|err| {
            tracing::warn!("look host lookup failed: {err:#}");
            ToolError::internal("looking at the app failed on our side; try again in a minute")
        })?;
    if stored.as_ref().is_some_and(|row| row.status == "active") {
        return Ok(stored.map(|row| row.hostname).unwrap_or_default());
    }
    Ok(format!("{}.{}", app.slug, ctx.state.apps_domain))
}

fn render_error(err: crate::browser::RenderError, url: &str) -> ToolError {
    use crate::browser::RenderError as E;
    tracing::warn!(url = %url, "look_at_app render failed: {err:#}");
    match err {
        E::TooLarge(bytes, cap) => ToolError::internal(format!(
            "the render came back {bytes} bytes, over the {cap} byte cap; retry at width 390"
        )),
        E::Refused(_) => {
            ToolError::internal("Cloudflare refused to render the page; try again in a minute")
        }
        E::BadImage | E::Transport(_) => {
            ToolError::internal("rendering the page failed on our side; try again in a minute")
        }
    }
}

/// Render the app page and return the screenshot as an MCP image block
/// plus the navigation status/title. Untrusted like `read_file`: the
/// pixels render app or visitor content. The REST endpoint cannot report
/// console errors or failed sub-requests (no log stream), so the answer
/// says so and points at `app_logs` / `app_errors` instead.
pub async fn look_at_app(input: LookAtAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let caller = authed(ctx)?;
    let (app, _) = visible_app(ctx, &input.app).await?;
    let path = input.path.as_deref().unwrap_or("/");
    check_path(path)?;
    let width = input.width.unwrap_or(1280);
    if !WIDTHS.contains(&width) {
        return Err(ToolError::invalid_argument(format!(
            "bad width {width}: expected 390 or 1280"
        )));
    }
    let full_page = input.full_page.unwrap_or(false);
    let Some(render) = ctx.state.browser.clone() else {
        return Err(ToolError::internal(
            "look_at_app is not configured on this host (no Cloudflare browser credentials)",
        ));
    };
    let used = crate::browser::claim_look(&ctx.state.pool, &caller.account_id)
        .await
        .map_err(|err| match err {
            crate::browser::QuotaError::Exhausted {
                used,
                retry_after_s,
            } => ToolError::rate_limited(
                format!(
                    "look quota: {} per hour, {used} used; retry in {retry_after_s}s",
                    crate::browser::LOOKS_PER_HOUR
                ),
                retry_after_s,
            ),
            crate::browser::QuotaError::Db(db) => {
                tracing::warn!("look quota claim failed: {db:#}");
                ToolError::internal("looking at the app failed on our side; try again in a minute")
            }
        })?;
    let host = render_host(ctx, &app).await?;
    let url = format!(
        "{}://{}{}{}",
        ctx.state.app_scheme, host, ctx.state.app_port_suffix, path
    );
    let shot = crate::browser::snapshot_page(&render, &url, width, full_page)
        .await
        .map_err(|err| render_error(err, &url))?;
    let status = shot.http_status.unwrap_or(-1);
    let title = shot.title.clone().unwrap_or_default();
    let shot_kind = if full_page { "full page" } else { "viewport" };
    let summary = format!(
        "{} {url}: HTTP {status}{}, {width}px {shot_kind}, look {used} of {} this hour.",
        app.slug,
        if title.is_empty() {
            String::new()
        } else {
            format!(" ({title})")
        },
        crate::browser::LOOKS_PER_HOUR,
    );
    let note = "Cloudflare's render returns the screenshot plus the page's HTTP status and title only: it cannot report console errors or failed requests. Use app_logs and app_errors for server-side failures.";
    Ok(look_result(
        &summary,
        note,
        &shot.image,
        json!({
            "url": url,
            "http_status": shot.http_status,
            "title": shot.title,
            "width": width,
            "full_page": full_page,
            "note": note,
        }),
    ))
}

#[derive(Debug, Serialize)]
struct ImageBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    data: String,
    #[serde(rename = "mimeType")]
    mime_type: &'static str,
}

/// The MCP result: one text block (summary + the §8.4 notice) plus the
/// screenshot as an image block, with the full JSON mirror as
/// `structuredContent`.
fn look_result(summary: &str, note: &str, image: &[u8], structured: Value) -> Value {
    use base64::Engine as _;
    let text = image_notice(&format!("{summary}\n{note}"));
    json!({
        "content": [
            {"type": "text", "text": text},
            serde_json::to_value(ImageBlock {
                kind: "image",
                data: base64::engine::general_purpose::STANDARD.encode(image),
                mime_type: "image/jpeg",
            }).unwrap_or(Value::Null),
        ],
        "structuredContent": structured,
    })
}
