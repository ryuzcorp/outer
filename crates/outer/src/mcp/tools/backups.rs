//! D1 backup tools (wave 3, feature #4): `list_backups`, `backup_database`
//! and `restore_backup`. All three are owner-only: backups hold the app's
//! full database contents.

use serde_json::json;

use super::{owner_app, Ctx};
use crate::http::console_data::d1::{D1Ctx, D1Error};
use crate::mcp::error::ToolError;

/// `list_backups` arguments: the app, and optionally its D1 binding.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListBackupsInput {
    /// The app slug.
    pub app: String,
    /// The D1 binding. Omit to list the app's dumps (one database per app).
    #[serde(default)]
    pub binding: Option<String>,
}

/// `backup_database` arguments: the app, and optionally its D1 binding
/// (defaults to the app's only declared binding).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BackupDatabaseInput {
    /// The app slug.
    pub app: String,
    /// The D1 binding. Omit to use the app's only declared binding.
    #[serde(default)]
    pub binding: Option<String>,
}

/// `restore_backup` arguments: the app, the backup, and a confirmation.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreBackupInput {
    /// The app slug.
    pub app: String,
    /// The D1 binding. Omit to use the app's only declared binding.
    #[serde(default)]
    pub binding: Option<String>,
    /// The backup id to restore, from `list_backups`.
    pub backup_id: String,
    /// Must equal `backup_id`: restoring overwrites the live database (a
    /// fresh backup is taken first, so a bad restore is itself restorable).
    pub confirm: String,
}

fn backup_json(row: &crate::d1_backup::BackupRow) -> serde_json::Value {
    json!({
        "id": row.id,
        "binding": row.binding,
        "created_at": row.created_at,
        "size_bytes": row.size_bytes,
    })
}

fn tool_error(err: D1Error) -> ToolError {
    match err {
        D1Error::UnknownDatabase(binding) => {
            ToolError::not_found(format!("unknown database {binding:?}"))
        }
        D1Error::NoConfig => ToolError::not_found(
            "this app has no deployed database config yet; add a d1_databases binding and deploy first",
        ),
        D1Error::UnknownBackup(id) => ToolError::not_found(format!("unknown backup {id:?}")),
        D1Error::UnknownTable(table) => ToolError::not_found(format!("unknown table {table:?}")),
        D1Error::BadQuery(message) => ToolError::invalid_argument(message),
        D1Error::AppStarting => {
            ToolError::internal("the app is still starting; retry in a few seconds")
        }
        D1Error::Celld(detail) => ToolError::internal(format!("celld d1 failed: {detail}")),
        D1Error::Timeout => ToolError::internal("celld d1 timed out"),
        D1Error::TooLarge(detail) => ToolError::internal(format!("backup too large: {detail}")),
        D1Error::Unavailable(_) => {
            ToolError::internal("couldn't reach the database right now; try again in a minute")
        }
    }
}

fn backup_ctx(ctx: &Ctx<'_>) -> D1Ctx {
    D1Ctx {
        celld_bin: ctx.state.fleets.celld_bin(),
        storage: ctx.state.fleets.storage().clone(),
        pool: ctx.state.pool.clone(),
        store: ctx.state.store.clone(),
        wake: Some(ctx.state.fleets.clone()),
    }
}

/// The one binding `input` names, or the app's only declared binding when
/// omitted (deploys refuse a second database, so there is never a choice
/// to list). Zero bindings is `not_found` (never deployed with one).
async fn resolve_binding(
    ctx: &Ctx<'_>,
    slug: &str,
    input: Option<&str>,
) -> Result<String, ToolError> {
    if let Some(binding) = input {
        if binding.len() > 128 {
            return Err(ToolError::not_found(format!(
                "unknown database {binding:?}"
            )));
        }
        return Ok(binding.to_string());
    }
    let bindings = crate::http::console_data::d1::database_bindings(&ctx.state.store, slug)
        .await
        .map_err(super::internal)?;
    bindings.into_iter().next().ok_or_else(|| {
        ToolError::not_found(
            "this app has no deployed database config yet; add a d1_databases binding and deploy first",
        )
    })
}

pub async fn list_backups(
    input: ListBackupsInput,
    ctx: &Ctx<'_>,
) -> Result<serde_json::Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    let mut rows = crate::d1_backup::backups_for(&ctx.state.pool, &app.id)
        .await
        .map_err(super::internal)?;
    if let Some(binding) = &input.binding {
        rows.retain(|row| &row.binding == binding);
    }
    Ok(super::ok(
        format!("{} D1 backup(s) for {:?}", rows.len(), app.slug),
        json!({"backups": rows.iter().map(backup_json).collect::<Vec<_>>()}),
    ))
}

pub async fn backup_database(
    input: BackupDatabaseInput,
    ctx: &Ctx<'_>,
) -> Result<serde_json::Value, ToolError> {
    let app = owner_app(ctx, &input.app).await?;
    let binding = resolve_binding(ctx, &app.slug, input.binding.as_deref()).await?;
    let d1 = backup_ctx(ctx);
    let row = crate::d1_backup::backup_binding(
        &ctx.state.pool,
        &ctx.state.store,
        &d1,
        &app,
        &binding,
        crate::apps::now_ms(),
    )
    .await
    .map_err(tool_error)?;
    Ok(super::ok(
        format!("backed up {:?} ({} bytes)", app.slug, row.size_bytes,),
        json!({"backup": backup_json(&row)}),
    ))
}

pub async fn restore_backup(
    input: RestoreBackupInput,
    ctx: &Ctx<'_>,
) -> Result<serde_json::Value, ToolError> {
    if input.confirm != input.backup_id {
        return Err(ToolError::invalid_argument(
            "confirm must equal backup_id: restoring overwrites the live database",
        ));
    }
    let app = owner_app(ctx, &input.app).await?;
    let binding = resolve_binding(ctx, &app.slug, input.binding.as_deref()).await?;
    let d1 = backup_ctx(ctx);
    let (fresh, report) = crate::d1_backup::restore_binding(
        &ctx.state.pool,
        &ctx.state.store,
        &d1,
        &app,
        &binding,
        &input.backup_id,
        crate::apps::now_ms(),
    )
    .await
    .map_err(tool_error)?;
    Ok(super::ok(
        format!(
            "restored {:?} from backup {} (fresh backup {} taken first, {} tables dropped, {} statements applied)",
            app.slug,
            input.backup_id,
            fresh.id,
            report.tables_dropped,
            report.statements_applied,
        ),
        json!({
            "backup": backup_json(&fresh),
            "restore": {
                "tables_dropped": report.tables_dropped,
                "statements_applied": report.statements_applied,
                "bytes_applied": report.bytes_applied,
            },
        }),
    ))
}
