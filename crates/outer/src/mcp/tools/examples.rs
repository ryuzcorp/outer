//! The public `examples` tool: list the ready patterns vendored from the
//! templates repo, or return one pattern's README plus its file list.
//!
//! Our own content, so no untrusted notice: agents apply patterns with
//! `add_example` and customise afterwards, instead of hand-copying files.
//!
//! The large text is never duplicated: file contents travel in the text
//! block only (like `read_files` shows them), while `structuredContent`
//! carries metadata plus the file list. `examples {name}` without `files`
//! stays small: README plus the file list with line counts.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

use super::{internal, ok, Ctx};
use crate::mcp::error::ToolError;
use crate::store::Store;

/// The output budget, same shape as `read_files`: file bytes past this are
/// cut, and the result carries `truncated`.
pub const MAX_EXAMPLE_OUTPUT_BYTES: usize = 200 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExamplesInput {
    /// An example name (`examples` lists them). Omit to list.
    pub name: Option<String>,
    /// Only examples for one template: "app" or "website". Omit for both.
    pub template: Option<String>,
    /// Include every file's contents in the text block (default false:
    /// README plus the file list with line counts).
    pub files: Option<bool>,
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
        Some(name) => get_example(ctx, template, &name, input.files.unwrap_or(false)).await,
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
            "next": "add_example {app, name} commits an example's working pages, nav and bindings; customise with commit. examples {name} shows its README and file list (files: true adds the contents).",
        }),
    ))
}

async fn get_example(
    ctx: &Ctx<'_>,
    template: Option<crate::templates::Template>,
    name: &str,
    with_files: bool,
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
            Ok(render_example(&example, with_files))
        }
    }
}

/// Load `names` plus their transitive `requires`, in request order. Unknown
/// names refuse with the valid list, before any caller creates anything.
pub(crate) async fn load_closure(
    store: &Store,
    data_dir: &Path,
    names: &[String],
) -> Result<Vec<crate::examples::Example>, ToolError> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue: Vec<String> = names.to_vec();
    while let Some(name) = queue.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if !crate::examples::is_valid_name(&name) {
            return Err(ToolError::invalid_argument(format!(
                "bad example name {name:?}: names match ^[a-z0-9][a-z0-9-]{{0,38}}[a-z0-9]$"
            )));
        }
        match crate::examples::get(store, data_dir, &name)
            .await
            .map_err(internal)?
        {
            None => {
                let valid = crate::examples::list(store, data_dir, None)
                    .await
                    .map_err(internal)?;
                return Err(ToolError::invalid_argument(format!(
                    "unknown example {name:?}; pick one of: {}",
                    valid
                        .iter()
                        .map(|example| example.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            Some(example) => {
                queue.extend(example.requires.iter().cloned());
                out.push(example);
            }
        }
    }
    // Request order first, then newly discovered requires in BFS order.
    let mut ordered: Vec<crate::examples::Example> = Vec::with_capacity(out.len());
    let mut emitted = BTreeSet::new();
    for name in names {
        if let Some(pos) = out.iter().position(|example| &example.name == name) {
            if emitted.insert(name.clone()) {
                ordered.push(out[pos].clone());
            }
        }
    }
    for example in out {
        if emitted.insert(example.name.clone()) {
            ordered.push(example);
        }
    }
    Ok(ordered)
}

/// The README, then the file list. With `files`, each file follows as a
/// fenced block headed by its path, cut at the output budget exactly like
/// `read_files`. Contents live in the text block only; `structuredContent`
/// carries metadata plus the file list with a per-file `truncated` flag.
fn render_example(example: &crate::examples::Example, with_files: bool) -> Value {
    let mut files = Vec::with_capacity(example.files.len());
    let mut list = Vec::with_capacity(example.files.len());
    for file in &example.files {
        let lines = file.text.lines().count();
        files.push(json!({"path": file.path, "lines": lines}));
        list.push(format!("- {} ({lines} lines)", file.path));
    }
    let mut structured = json!({
        "name": example.name,
        "title": example.title,
        "summary": example.summary,
        "template": example.template,
        "requires": example.requires,
        "nav": example.nav.iter().map(|item| {
            json!({"href": item.href, "label": item.label, "icon": item.icon})
        }).collect::<Vec<_>>(),
        "bindings": example.bindings,
        "dependencies": example.dependencies,
        "readme": example.readme,
        "files": files,
        "truncated": false,
    });
    if !with_files {
        let text = format!(
            "{}\n\nFiles ({}):\n{}\n\nCall `examples {{name: {:?}, files: true}}` for the contents, or `add_example {{app, name: {:?}}}` to commit it.",
            example.readme,
            example.files.len(),
            list.join("\n"),
            example.name,
            example.name,
        );
        return ok(text, structured);
    }
    let mut used = 0usize;
    let mut spent = false;
    let mut truncated = false;
    let mut truncated_flags = vec![false; example.files.len()];
    let mut blocks = vec![example.readme.clone()];
    for (i, file) in example.files.iter().enumerate() {
        if spent {
            truncated_flags[i] = true;
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
                truncated_flags[i] = true;
                blocks.push(format!(
                    "## {}\n(skipped: output budget exhausted)",
                    file.path
                ));
            } else {
                truncated_flags[i] = true;
                blocks.push(format!("{cut}\n(truncated at the 200 KB output budget)"));
            }
            used = MAX_EXAMPLE_OUTPUT_BYTES;
            spent = true;
            truncated = true;
        } else {
            used += block.len();
            blocks.push(block);
        }
    }
    for (entry, flag) in structured["files"]
        .as_array_mut()
        .iter_mut()
        .flat_map(|entries| entries.iter_mut())
        .zip(truncated_flags)
    {
        if flag {
            entry["truncated"] = json!(true);
        }
    }
    structured["truncated"] = json!(truncated);
    ok(blocks.join("\n\n"), structured)
}
