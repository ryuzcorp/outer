//! D1 backup console API (wave 3, feature #4): list dumps, back up now,
//! download one dump as `.sql`, and restore one dump into the same database.
//!
//! Same guard as the other data panels: session + same-origin + the
//! owner-scoped 404, owner-only in v1. Mutations audit through
//! [`crate::mcp::actions::record_console`] with the summaries carrying only
//! the binding and the byte counts — never row values.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::http::console::guard;
use crate::http::console_data::d1::{d1_response, D1Ctx, D1Error};
use crate::http::console_data::owner_app;

/// POST `…/backups/{id}/restore` body: the backup id again, proving the
/// click-through was deliberate (the console modal makes the user confirm).
#[derive(Debug, serde::Deserialize)]
pub struct RestoreBody {
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

/// The `{error, message, user_message}` outcome pair for the audit row,
/// mirroring the D1 panel's convention.
fn audit_outcome(
    outcome: &Result<serde_json::Value, D1Error>,
) -> (&'static str, Option<&'static str>) {
    match outcome {
        Ok(_) => ("success", None),
        Err(
            D1Error::BadQuery(_)
            | D1Error::UnknownTable(_)
            | D1Error::UnknownDatabase(_)
            | D1Error::UnknownBackup(_),
        ) => ("error", Some("invalid_argument")),
        Err(_) => ("error", Some("d1_unavailable")),
    }
}

pub fn routes() -> Router<crate::http::AppState> {
    Router::new()
        .route(
            "/api/console/apps/{slug}/data/d1/{binding}/backups",
            get(api_list).post(api_backup_now),
        )
        .route(
            "/api/console/apps/{slug}/data/d1/{binding}/backups/{id}/download",
            get(api_download),
        )
        .route(
            "/api/console/apps/{slug}/data/d1/{binding}/backups/{id}/restore",
            post(api_restore),
        )
}

async fn api_list(
    State(state): State<crate::http::AppState>,
    headers: HeaderMap,
    Path((slug, binding)): Path<(String, String)>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app, _) = match owner_app(
        mcp,
        &headers,
        &account,
        &slug,
        "this database",
        Some("data_backups"),
    )
    .await
    {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    if binding.len() > 128 {
        return d1_response(Err(D1Error::UnknownDatabase(binding)));
    }
    let result = async {
        let rows = crate::d1_backup::backups_for(&state.mcp.pool, &app.id)
            .await
            .map_err(D1Error::Unavailable)?;
        Ok::<_, D1Error>(json!({
            "binding": binding,
            "backups": rows
                .iter()
                .filter(|row| row.binding == binding)
                .map(backup_json)
                .collect::<Vec<_>>(),
        }))
    }
    .await;
    d1_response(result)
}

async fn api_backup_now(
    State(state): State<crate::http::AppState>,
    headers: HeaderMap,
    Path((slug, binding)): Path<(String, String)>,
) -> Response {
    // Deliberately NOT storage-gated: a backup never changes live data — it
    // is the escape hatch that lets an owner read a frozen database out.
    // Its bytes feed the next sweep sizing, so a frozen app stays frozen
    // until the owner frees space elsewhere.
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app, admin) = match owner_app(mcp, &headers, &account, &slug, "this database", None).await
    {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    if binding.len() > 128 {
        return d1_response(Err(D1Error::UnknownDatabase(binding)));
    }
    let started = std::time::Instant::now();
    let ctx = D1Ctx::from_state(&state);
    let outcome = crate::d1_backup::backup_binding(
        &ctx.pool,
        &ctx.store,
        &ctx,
        &app,
        &binding,
        crate::apps::now_ms(),
    )
    .await;
    let (summary, result) = match outcome {
        Ok(row) => (
            format!("d1 backup now {binding} ({} bytes)", row.size_bytes),
            Ok(json!({"backup": backup_json(&row)})),
        ),
        Err(D1Error::UnknownDatabase(_)) => (
            format!("refused d1 backup on unknown database {binding:?}"),
            Err(D1Error::UnknownDatabase(binding.clone())),
        ),
        Err(D1Error::NoConfig) => (
            format!("refused d1 backup on {binding}: never deployed"),
            Err(D1Error::NoConfig),
        ),
        Err(err) => (format!("failed d1 backup on {binding}"), Err(err)),
    };
    let (o, code) = audit_outcome(&result);
    crate::mcp::actions::record_console(
        &ctx.pool,
        &app.id,
        &account.id,
        "d1_backup",
        &crate::http::console::audit_summary(&account, &summary, admin),
        o,
        code,
        started.elapsed().as_millis().min(i64::MAX as u128) as i64,
    )
    .await;
    d1_response(result)
}

async fn api_download(
    State(state): State<crate::http::AppState>,
    headers: HeaderMap,
    Path((slug, binding, id)): Path<(String, String, String)>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app, _) = match owner_app(
        mcp,
        &headers,
        &account,
        &slug,
        "this database",
        Some("data_backups"),
    )
    .await
    {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    let outcome = async {
        let row = crate::d1_backup::backup_by_id(&state.mcp.pool, &app.id, &id)
            .await
            .map_err(D1Error::Unavailable)?
            .filter(|row| row.binding == binding)
            .ok_or_else(|| D1Error::UnknownBackup(id.clone()))?;
        state
            .mcp
            .store
            .get(&row.store_key)
            .await
            .map_err(D1Error::Unavailable)?
            .ok_or_else(|| D1Error::Unavailable(anyhow::anyhow!("backup bytes are gone")))
            .map(|bytes| (row, bytes))
    }
    .await;
    let (row, bytes) = match outcome {
        Ok(pair) => pair,
        Err(err) => return d1_response(Err(err)),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/sql")
        .header(
            "content-disposition",
            format!(
                "attachment; filename=\"outer-{}-{}-{}.sql\"",
                slug.replace('"', "_"),
                row.binding.replace('"', "_"),
                row.created_at,
            ),
        )
        .header("x-content-type-options", "nosniff")
        .header("cache-control", "no-store")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| {
            d1_response(Err(D1Error::Unavailable(anyhow::anyhow!(
                "download failed"
            ))))
        })
}

async fn api_restore(
    State(state): State<crate::http::AppState>,
    headers: HeaderMap,
    Path((slug, binding, id)): Path<(String, String, String)>,
    Json(body): Json<RestoreBody>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let (app, admin) = match owner_app(mcp, &headers, &account, &slug, "this database", None).await
    {
        Ok(pair) => pair,
        Err(res) => return *res,
    };
    // A restore rewrites live tables, so a frozen app refuses it like any
    // other write (deletes stay the escape hatch back under the cap).
    if let Some(res) = super::storage_frozen_refusal(mcp, &app).await {
        return res;
    }
    if binding.len() > 128 || body.confirm != id {
        return d1_response(Err(D1Error::BadQuery(
            "confirm must equal the backup id".to_string(),
        )));
    }
    let started = std::time::Instant::now();
    let ctx = D1Ctx::from_state(&state);
    let outcome = crate::d1_backup::restore_binding(
        &ctx.pool,
        &ctx.store,
        &ctx,
        &app,
        &binding,
        &id,
        crate::apps::now_ms(),
    )
    .await;
    let (summary, result) = match outcome {
        Ok((fresh, report)) => (
            format!(
                "d1 restore {binding} from backup {} (fresh backup {}, {} tables, {} statements)",
                id, fresh.id, report.tables_dropped, report.statements_applied,
            ),
            Ok(json!({
                "backup": backup_json(&fresh),
                "restore": {
                    "tables_dropped": report.tables_dropped,
                    "statements_applied": report.statements_applied,
                    "bytes_applied": report.bytes_applied,
                },
            })),
        ),
        Err(D1Error::UnknownBackup(_)) => (
            format!("refused d1 restore on {binding}: unknown backup"),
            Err(D1Error::UnknownBackup(id.clone())),
        ),
        Err(err) => (format!("failed d1 restore on {binding}"), Err(err)),
    };
    let (o, code) = audit_outcome(&result);
    crate::mcp::actions::record_console(
        &ctx.pool,
        &app.id,
        &account.id,
        "d1_restore",
        &crate::http::console::audit_summary(&account, &summary, admin),
        o,
        code,
        started.elapsed().as_millis().min(i64::MAX as u128) as i64,
    )
    .await;
    d1_response(result)
}
