//! The public `examples` tool: list the copy-paste patterns vendored from
//! the templates repo, or return one pattern's README plus its files.
//!
//! Our own content, so no untrusted notice: agents copy the files onto a
//! fresh template and adapt them.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{internal, ok, Ctx};
use crate::mcp::error::ToolError;

/// The output budget, same shape as `read_files`: file bytes past this are
/// cut, and the result carries `truncated`.
pub const MAX_EXAMPLE_OUTPUT_BYTES: usize = 200 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExamplesInput {
    /// An example name (`examples` lists them). Omit to list.
    pub name: Option<String>,
    /// Only examples for one template: "app" or "website". Omit for both.
    pub template: Option<String>,
}

fn truncate_utf8(s: &str, cap: usize) -> &str {
    let mut end = cap.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub async fn examples(input: ExamplesInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let template = match input.template.as_deref() {
        None => None,
        Some(id) => Some(crate::templates::Template::from_id(id).ok_or_else(|| {
            ToolError::invalid_argument(format!(
                "unknown template {id:?}: want \"app\" or \"website\""
            ))
        })?),
    };
    match input.name {
        None => list_examples(ctx, template).await,
        Some(name) => get_example(ctx, template, &name).await,
    }
}

async fn list_examples(
    ctx: &Ctx<'_>,
    template: Option<crate::templates::Template>,
) -> Result<Value, ToolError> {
    let examples =
        crate::examples::list(&ctx.state.store, &ctx.state.delete_cfg.data_dir, template)
            .await
            .map_err(internal)?;
    let rows: Vec<Value> = examples
        .iter()
        .map(|example| {
            json!({
                "name": example.name,
                "title": example.title,
                "summary": example.summary,
                "template": example.template,
                "dependencies": example.dependencies,
                "files": example.files.iter().map(|file| {
                    json!({"path": file.path, "lines": file.lines})
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    let text = if rows.is_empty() {
        "(no examples at the current templates pin)".to_string()
    } else {
        examples
            .iter()
            .map(|example| {
                format!(
                    "- {} ({}): {} — {}",
                    example.name, example.template, example.title, example.summary
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(ok(
        text,
        json!({
            "examples": rows,
            "next": "examples {name} returns the README and files to copy",
        }),
    ))
}

async fn get_example(
    ctx: &Ctx<'_>,
    template: Option<crate::templates::Template>,
    name: &str,
) -> Result<Value, ToolError> {
    if !crate::examples::is_valid_name(name) {
        return Err(ToolError::invalid_argument(format!(
            "bad example name {name:?}: names match ^[a-z0-9][a-z0-9-]{{0,38}}[a-z0-9]$"
        )));
    }
    let data_dir = &ctx.state.delete_cfg.data_dir;
    match crate::examples::get(&ctx.state.store, data_dir, name)
        .await
        .map_err(internal)?
    {
        None => {
            let valid = crate::examples::list(&ctx.state.store, data_dir, template)
                .await
                .map_err(internal)?;
            Err(ToolError::invalid_argument(format!(
                "unknown example {name:?}; pick one of: {}",
                valid
                    .iter()
                    .map(|example| example.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
        Some(example) => {
            if template.is_some_and(|template| example.template != template.id()) {
                return Err(ToolError::invalid_argument(format!(
                    "example {name:?} is for template {:?}, not {:?}",
                    example.template,
                    template.map(|template| template.id())
                )));
            }
            Ok(render_example(&example))
        }
    }
}

/// The README, then each file as a fenced block headed by its path, cut at
/// the output budget exactly like `read_files`.
fn render_example(example: &crate::examples::Example) -> Value {
    let mut used = 0usize;
    let mut spent = false;
    let mut truncated = false;
    let mut files = Vec::with_capacity(example.files.len());
    let mut blocks = Vec::with_capacity(example.files.len() + 1);
    blocks.push(example.readme.clone());
    for file in &example.files {
        if spent {
            files.push(json!({
                "path": file.path,
                "code": "invalid_argument",
                "error": "skipped: the 200 KB output budget is exhausted by earlier files",
            }));
            blocks.push(format!(
                "## {}\n(skipped: output budget exhausted)",
                file.path
            ));
            truncated = true;
            continue;
        }
        let block = format!("## {}\n```\n{}\n```", file.path, file.text);
        if used + block.len() > MAX_EXAMPLE_OUTPUT_BYTES {
            let room = MAX_EXAMPLE_OUTPUT_BYTES.saturating_sub(used);
            let cut = truncate_utf8(&block, room);
            // A file cut to nothing is an inline skip, not an empty read.
            if cut.is_empty() {
                files.push(json!({
                    "path": file.path,
                    "code": "invalid_argument",
                    "error": "skipped: the 200 KB output budget is exhausted by earlier files",
                }));
                blocks.push(format!(
                    "## {}\n(skipped: output budget exhausted)",
                    file.path
                ));
            } else {
                files.push(json!({
                    "path": file.path,
                    "text": cut,
                    "truncated": true,
                }));
                blocks.push(format!("{cut}\n(truncated at the 200 KB output budget)"));
            }
            used = MAX_EXAMPLE_OUTPUT_BYTES;
            spent = true;
            truncated = true;
        } else {
            used += block.len();
            files.push(json!({"path": file.path, "text": file.text}));
            blocks.push(block);
        }
    }
    ok(
        blocks.join("\n\n"),
        json!({
            "name": example.name,
            "title": example.title,
            "template": example.template,
            "dependencies": example.dependencies,
            "readme": example.readme,
            "files": files,
            "truncated": truncated,
        }),
    )
}
