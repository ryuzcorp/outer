//! The collaborator tools (Pro, spec §7.4): `add_collaborator`,
//! `remove_collaborator`, `list_collaborators`. Owner only: a collaborator's
//! call answers `forbidden`, and a slug with no role answers `not_found`,
//! exactly like a missing one.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{internal, ok, owner_app, Ctx};
use crate::collaborators::{self, AddError};
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddCollaboratorInput {
    /// The app slug.
    pub app: String,
    /// The collaborator's email: allowlisted for the closed beta and
    /// emailed the invite.
    pub email: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveCollaboratorInput {
    /// The app slug.
    pub app: String,
    /// The collaborator's email to remove.
    pub email: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCollaboratorsInput {
    /// The app slug.
    pub app: String,
}

fn collaborator_json(collaborator: &collaborators::Collaborator) -> Value {
    json!({
        "email": collaborator.email,
        "status": if collaborator.joined { "joined" } else { "pending" },
        "added_at": collaborator.added_at,
    })
}

pub async fn add_collaborator(
    input: AddCollaboratorInput,
    ctx: &Ctx<'_>,
) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    let caller = super::authed(ctx)?;
    match collaborators::add(
        &ctx.state.pool,
        &ctx.state.auth.config.email,
        &ctx.state.mcp_url(),
        &app,
        &caller.email,
        &input.email,
    )
    .await
    {
        Ok(collaborator) => Ok(ok(
            format!("{} can now change {}.", collaborator.email, app.slug),
            json!({
                "app": app.slug,
                "collaborator": collaborator_json(&collaborator),
            }),
        )),
        Err(AddError::NotPro) => Err(super::with_upgrade(
            ToolError::payment_required(
                format!("collaborators on {} need a Pro owner", app.slug),
                "Collaborators are a Pro feature. Upgrade to Pro to invite people to your apps.",
            ),
            &ctx.state.billing,
            &app.owner_id,
        )),
        Err(AddError::LimitReached) => Err(super::with_upgrade(
            ToolError::quota_exceeded(
                format!(
                    "app quota: {} collaborators allowed, {} on {}",
                    collaborators::MAX_COLLABORATORS,
                    collaborators::MAX_COLLABORATORS,
                    app.slug
                ),
                format!(
                    "{} already has the maximum {} collaborators.",
                    app.slug,
                    collaborators::MAX_COLLABORATORS
                ),
                "collaborators",
                collaborators::MAX_COLLABORATORS,
                collaborators::MAX_COLLABORATORS,
                None,
            ),
            &ctx.state.billing,
            &app.owner_id,
        )),
        Err(AddError::InvalidEmail(_)) => Err(ToolError::invalid_argument(format!(
            "{:?} is not an address that can be invited to {}",
            input.email, app.slug
        ))),
        Err(AddError::SelfInvite) => Err(ToolError::invalid_argument(format!(
            "you already own {}; invite someone else's address",
            app.slug
        ))),
        Err(AddError::Duplicate) => Err(ToolError::invalid_argument(format!(
            "{:?} is already a collaborator on {}",
            crate::auth::normalize_email(&input.email),
            app.slug
        ))),
        Err(AddError::Internal(err)) => Err(internal(err)),
    }
}

pub async fn remove_collaborator(
    input: RemoveCollaboratorInput,
    ctx: &Ctx<'_>,
) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    let removed = collaborators::remove(&ctx.state.pool, &app.id, &input.email)
        .await
        .map_err(internal)?;
    if !removed {
        return Err(ToolError::invalid_argument(format!(
            "{:?} is not a collaborator on {}",
            crate::auth::normalize_email(&input.email),
            app.slug
        )));
    }
    Ok(ok(
        format!(
            "{} can no longer change {}.",
            crate::auth::normalize_email(&input.email),
            app.slug
        ),
        json!({
            "app": app.slug,
            "email": crate::auth::normalize_email(&input.email),
            "removed": true,
        }),
    ))
}

pub async fn list_collaborators(
    input: ListCollaboratorsInput,
    ctx: &Ctx<'_>,
) -> Result<Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    let collaborators = collaborators::list(&ctx.state.pool, &app.id)
        .await
        .map_err(internal)?;
    Ok(ok(
        format!("{} collaborator(s) on {}.", collaborators.len(), app.slug),
        json!({
            "app": app.slug,
            "collaborators": collaborators.iter().map(collaborator_json).collect::<Vec<_>>(),
        }),
    ))
}
