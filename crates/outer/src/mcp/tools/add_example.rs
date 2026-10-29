//! The `add_example` write tool: plan an example onto an app's head tree and
//! commit the result through the same commit + gate + awaited deploy path as
//! `commit` (one code change, charged to the app owner).

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::{
    authed, check_build_quota, check_storage_writable, claim_change, internal, ok_deploy,
    refund_change, run_deploy, visible_app, Ctx,
};
use crate::git;
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddExampleInput {
    /// The app slug.
    pub app: String,
    /// The example name (`examples` lists them). Its `requires` apply first.
    pub name: String,
    /// Wait for the deploy (default true). False returns immediately; poll
    /// deploy_status.
    pub wait: Option<bool>,
}

pub async fn add_example(input: AddExampleInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let caller = authed(ctx)?;
    let author = crate::ops::noreply_author(&caller.account_id, &caller.email, &ctx.state.domain);
    let data_dir = &ctx.state.delete_cfg.data_dir;
    // Load the closure before claiming anything: an unknown name refuses
    // before the app is touched.
    let closure = super::examples::load_closure(
        &ctx.state.store,
        data_dir,
        std::slice::from_ref(&input.name),
    )
    .await?;
    let inputs: Vec<crate::examples::apply::ExampleInput> = closure
        .iter()
        .map(|example| example.apply_input())
        .collect();
    // The same gates and quota as `commit`, charged to the app owner whoever
    // calls: a collaborator's apply counts against the owner's daily changes.
    check_build_quota(&ctx.state.pool, &ctx.state.billing, &app.owner_id).await?;
    check_storage_writable(&ctx.state.pool, &ctx.state.billing, &app).await?;
    // A handoff refuses before the claim, so a retry costs no quota.
    super::check_no_drain(ctx.state)?;
    let claim = claim_change(&ctx.state.pool, &ctx.state.billing, &app.owner_id).await?;
    let planned = async {
        let bare = super::code::mirror_for(ctx, &app.slug).await?;
        let (head, tree) = head_tree(&bare, &inputs).await?;
        // The app's template is what the tree shows: the app starter owns
        // `pages/_nav.ts`, the website starter has no nav file.
        let app_template = if tree.contains_key("pages/_nav.ts") {
            "app"
        } else {
            "website"
        };
        crate::examples::apply::plan_apply(&tree, app_template, &inputs, &[input.name.as_str()])
            .map(|plan| (head, plan))
            .map_err(|refusal| ToolError::invalid_argument(refusal.to_string()))
    }
    .await;
    let (head, plan) = match planned {
        Ok(planned) => planned,
        Err(err) => {
            if let Err(refund) = refund_change(&ctx.state.pool, &app.owner_id).await {
                tracing::warn!("add_example refund failed: {refund:#}");
            }
            return Err(err);
        }
    };
    let request = git::CommitRequest {
        message: format!("add_example: {}", plan.added.join(", ")),
        // Full-file writes planned from `head`: a commit that landed since
        // would be silently overwritten, so the CAS refuses with `conflict`
        // (current sha included) and the agent re-runs add_example.
        base: Some(head),
        changes: git::CommitChanges {
            writes: plan
                .writes
                .iter()
                .map(|(path, text)| git::FileWrite {
                    path: path.clone(),
                    body: git::WriteBody::Text(text.clone()),
                    expect_blob: None,
                })
                .collect(),
            ..Default::default()
        },
        author,
    };
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
                tracing::warn!("add_example refund failed: {refund:#}");
            }
            return Err(ToolError::from_commit(e));
        }
    };
    let sha = outcome.sha;
    let url = ctx.state.app_url(&app.slug);
    let deploy = run_deploy(ctx.state, &app, &sha, input.wait.unwrap_or(true)).await?;
    super::spawn_quota_email(ctx.state, &app.owner_id, claim.used, claim.limit);
    let quota =
        json!({"limit": claim.limit, "used": claim.used, "resets_at": super::resets_at_utc()});
    let summary = add_summary(&input.app, &plan, &deploy, &url);
    let structured = json!({
        "commit": {"sha": sha},
        "added": plan.added,
        "files": plan.files,
        "nav": plan.nav,
        "deploy": deploy,
        "quota": quota,
        "next": "Customise the copy with commit; check with look_at_app.",
    });
    Ok(ok_deploy(
        summary,
        &structured["deploy"].clone(),
        structured,
    ))
}

fn add_summary(
    slug: &str,
    plan: &crate::examples::apply::ApplyPlan,
    deploy: &Value,
    url: &str,
) -> String {
    let status = deploy["status"].as_str().unwrap_or("unknown");
    let added = plan.added.join(", ");
    let live = if plan.nav.is_empty() {
        format!("{} file(s) committed", plan.files.len())
    } else {
        format!("{} are live", plan.nav.join(", "))
    };
    match status {
        "success" => format!(
            "Added {added} to {slug}: {live}. Customise the copy with commit; check with look_at_app."
        ),
        "failed" | "blocked" => {
            let step = deploy["failed_step"]
                .as_str()
                .map(|step| format!(" at {step}"))
                .unwrap_or_default();
            let error = deploy["error"]
                .as_str()
                .map(|error| format!(": {error}"))
                .unwrap_or_default();
            format!(
                "Added {added} to {slug}, but its deploy {status}{step}{error}. It is not live yet: fix it with commit (deploy_log {{app}} has the full log)."
            )
        }
        _ => format!(
            "Added {added} to {slug}; its deploy is {status}. Call deploy_status {{app, wait: true}} until it reports success at {url}."
        ),
    }
}

/// The head sha and the tree as the apply engine sees it at that sha: every
/// non-migration file the closure touches (with text), plus the
/// `db/migrations/` names (numbers only) and the two edited files. Every
/// read pins the sha the listing resolved, so the plan and the commit's CAS
/// base are the same commit.
async fn head_tree(
    bare: &std::path::Path,
    inputs: &[crate::examples::apply::ExampleInput],
) -> Result<(String, BTreeMap<String, String>), ToolError> {
    // Migration numbers only: renumbered writes never collide with them.
    let (head, migrations, _) =
        crate::git::local::list_files(bare, None, Some("db/migrations"), usize::MAX)
            .await
            .map_err(|err| {
                if err.to_string().contains("no commits") {
                    ToolError::not_found("app has no commits yet")
                } else {
                    internal(err)
                }
            })?;
    let mut need: BTreeSet<&str> = BTreeSet::new();
    for input in inputs {
        for (path, _) in &input.files {
            need.insert(path.as_str());
        }
    }
    need.insert("pages/_nav.ts");
    need.insert("void.config.ts");
    let mut tree = BTreeMap::new();
    for path in need {
        match crate::git::local::read_blob(bare, Some(&head), path).await {
            Ok((_, bytes)) => match String::from_utf8(bytes) {
                Ok(text) => {
                    tree.insert(path.to_string(), text);
                }
                Err(_) => {
                    return Err(ToolError::invalid_argument(format!(
                        "{path} is not UTF-8: move it aside or convert it by hand, then retry"
                    )));
                }
            },
            Err(err) => {
                if !err.to_string().contains("not found") {
                    return Err(super::rev_err(err));
                }
            }
        }
    }
    for entry in migrations {
        tree.entry(entry.path).or_insert_with(String::new);
    }
    Ok((head, tree))
}
