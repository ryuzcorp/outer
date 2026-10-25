//! Daily D1 backups (wave 3, feature #4).
//!
//! Every app has at most one D1 binding (enforced elsewhere). Once a day the
//! backup sweep dumps each active app's databases to SQL and stores the dump
//! in R2 under `outer/backups/{slug}/` — deliberately NOT under
//! `fleets/{slug}/`, so app code (whose scoped credentials cover only its
//! own fleet prefix) can never read or touch the backups. Free keeps 7 days
//! of dumps, Pro 30.
//!
//! celld 0.6.2 has no `d1 export` command: only `d1 execute` and
//! `d1 migrations apply` run SQL from the CLI
//! (<https://celld.dev/docs/services/d1>), so the dump is produced the
//! portable way — the schema out of `sqlite_master`, then paged
//! `SELECT * … LIMIT … OFFSET …` rows through `celld d1 execute --json`,
//! rendered to `CREATE` plus multi-row `INSERT` statements. Every generated
//! statement stays under the 100,000-byte D1 statement limit
//! (<https://celld.dev/docs/services/d1>) with headroom, and the dump holds
//! no `BEGIN`/`COMMIT` (application SQL may not use them): each restore
//! batch runs inside celld's own transaction instead.
//!
//! Restore replays a dump into the same database (drop the user tables,
//! replay in batches) after taking a fresh backup first, so a bad restore
//! is itself restorable. `d1_migrations` — celld's migration bookkeeping,
//! written by `migrations apply` — is neither dumped nor dropped, so a
//! restore never confuses a later apply.

use std::collections::HashMap;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::apps::App;
use crate::fleet::manager::FleetManager;
use crate::http::console_data::d1::{self, D1Ctx, D1Error};
use crate::store::Store;

/// Bucket scope for every backup object. Not under `fleets/{slug}/` on
/// purpose: app code must never reach its backups (see the module docs).
pub const BACKUP_ROOT: &str = "outer/backups";

/// The bucket prefix one slug's backups live under. Shared by the backup
/// engine, the rename copy, and the delete path.
pub fn backup_prefix(slug: &str) -> String {
    format!("{BACKUP_ROOT}/{slug}/")
}

/// One dump's key: the creation time sorts the prefix, and the sanitized
/// binding keeps the key unique per database.
pub fn backup_key(slug: &str, created_at: i64, binding: &str) -> String {
    format!(
        "{}{created_at}-{}.sql",
        backup_prefix(slug),
        sanitize_binding(binding)
    )
}

/// A binding is agent-influenced (declared in wrangler config), so the key
/// keeps only filename-safe characters.
fn sanitize_binding(binding: &str) -> String {
    let mut out = String::with_capacity(binding.len());
    for c in binding.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("db");
    }
    out
}

/// Free keeps 7 days of dumps, Pro 30 (spec §3 plans via `user.plan`).
pub const FREE_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub const PRO_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub fn retention_ms(plan: &str) -> i64 {
    if plan == "pro" {
        PRO_RETENTION_MS
    } else {
        FREE_RETENTION_MS
    }
}

/// A backup is due when the newest dump for the binding is at least this old.
pub const BACKUP_EVERY_MS: i64 = 24 * 60 * 60 * 1000;

/// How often the sweep looks for due apps.
pub const TICK_EVERY: Duration = Duration::from_secs(3600);

/// Rows per dump page: small enough that no page nears the 32 MiB binding
/// result cap, large enough that a daily dump is a handful of execs.
pub const ROW_PAGE: i64 = 500;

/// Generated statements stay under the 100,000-byte D1 statement limit with
/// headroom for celld's own wrapping.
pub const MAX_STATEMENT_BYTES: usize = 90_000;

/// A dump bigger than this fails instead of filling R2: D1 apps are small
/// config-plus-content databases, and an unbounded dump would wedge the
/// daily sweep on one app.
pub const MAX_DUMP_BYTES: usize = 32 * 1024 * 1024;

/// Restore applies the dump in batches of at most this many bytes. celld
/// brackets each multi-statement exec in its own transaction, so a batch is
/// all-or-nothing while the whole restore stays resumable statement-wise.
pub const RESTORE_BATCH_BYTES: usize = 80_000;

/// One stored dump, as the list endpoints return it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BackupRow {
    pub id: String,
    pub binding: String,
    pub store_key: String,
    pub created_at: i64,
    pub size_bytes: i64,
}

/// What one restore did, for the console/MCP answers and the audit summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// The fresh backup taken before the restore ran.
    pub fresh_backup_id: String,
    pub tables_dropped: usize,
    pub statements_applied: usize,
    pub bytes_applied: usize,
}

fn unavailable(err: impl Into<anyhow::Error>) -> D1Error {
    D1Error::Unavailable(err.into())
}

/// The owner's plan (`user.plan`), defaulting to free.
async fn owner_plan(pool: &SqlitePool, owner_id: &str) -> anyhow::Result<String> {
    let plan: Option<String> = sqlx::query_scalar("SELECT plan FROM user WHERE id = ?1")
        .bind(owner_id)
        .fetch_optional(pool)
        .await?;
    Ok(plan.unwrap_or_else(|| "free".to_string()))
}

// ---------------------------------------------------------------------------
// Table I/O
// ---------------------------------------------------------------------------

/// Every dump for one app, newest first.
pub async fn backups_for(pool: &SqlitePool, app_id: &str) -> anyhow::Result<Vec<BackupRow>> {
    Ok(sqlx::query_as::<_, BackupRow>(
        "SELECT id, binding, store_key, created_at, size_bytes \
         FROM d1_backup WHERE app_id = ?1 ORDER BY created_at DESC, id DESC",
    )
    .bind(app_id)
    .fetch_all(pool)
    .await?)
}

/// One dump by id, scoped to its app.
pub async fn backup_by_id(
    pool: &SqlitePool,
    app_id: &str,
    id: &str,
) -> anyhow::Result<Option<BackupRow>> {
    Ok(sqlx::query_as::<_, BackupRow>(
        "SELECT id, binding, store_key, created_at, size_bytes \
         FROM d1_backup WHERE app_id = ?1 AND id = ?2",
    )
    .bind(app_id)
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// The newest dump for one binding, if any.
async fn newest_for_binding(
    pool: &SqlitePool,
    app_id: &str,
    binding: &str,
) -> anyhow::Result<Option<BackupRow>> {
    Ok(sqlx::query_as::<_, BackupRow>(
        "SELECT id, binding, store_key, created_at, size_bytes \
         FROM d1_backup WHERE app_id = ?1 AND binding = ?2 \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(app_id)
    .bind(binding)
    .fetch_optional(pool)
    .await?)
}

/// True when the binding has no dump, or its newest dump is a day old.
pub async fn backup_due(
    pool: &SqlitePool,
    app_id: &str,
    binding: &str,
    now_ms: i64,
) -> anyhow::Result<bool> {
    let newest = newest_for_binding(pool, app_id, binding).await?;
    Ok(newest.is_none_or(|row| now_ms.saturating_sub(row.created_at) >= BACKUP_EVERY_MS))
}

/// Drop every dump older than `retention_ms`, bytes and rows both. A missing
/// key is already gone (a crashed delete converges here), never an error.
pub async fn prune_app(
    pool: &SqlitePool,
    store: &Store,
    app_id: &str,
    retention_ms: i64,
    now_ms: i64,
) -> anyhow::Result<u64> {
    let cutoff = now_ms.saturating_sub(retention_ms);
    let stale: Vec<BackupRow> = sqlx::query_as::<_, BackupRow>(
        "SELECT id, binding, store_key, created_at, size_bytes \
         FROM d1_backup WHERE app_id = ?1 AND created_at < ?2 ORDER BY created_at",
    )
    .bind(app_id)
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    let mut pruned = 0u64;
    for row in &stale {
        store.delete(&row.store_key).await?;
        sqlx::query("DELETE FROM d1_backup WHERE id = ?1")
            .bind(&row.id)
            .execute(pool)
            .await?;
        pruned += 1;
    }
    Ok(pruned)
}

// ---------------------------------------------------------------------------
// Dump
// ---------------------------------------------------------------------------

/// One `celld d1 execute --json` against the app's fleet, through the backup
/// entry point. A free function (not a closure) so the borrow checker sees
/// each call's borrows independently.
async fn exec_one(
    ctx: &D1Ctx,
    slug: &str,
    identity: &str,
    project: &std::path::Path,
    sql: &str,
) -> Result<String, D1Error> {
    d1::d1_exec(ctx, slug, identity, sql, true, project).await
}
/// One `sqlite_master` row with a body: tables, indexes, triggers, views.
/// Auto-indexes carry NULL `sql` and never reach here.
struct SchemaObject {
    kind: String,
    name: String,
    sql: String,
}

/// Dump one binding to SQL text. Runs `1 + tables × 2 + pages` small execs
/// (schema, then per table one `PRAGMA foreign_key_list` and one
/// `PRAGMA table_info`, then the row pages), each bounded by
/// [`ROW_PAGE`] — a daily sweep cost, not an interactive one.
pub async fn dump_database(ctx: &D1Ctx, app: &App, binding: &str) -> Result<String, D1Error> {
    let (identity, project) = d1::ready_backup(ctx, app, binding).await?;

    let out = exec_one(
        ctx,
        &app.slug,
        &identity,
        project.path(),
        "SELECT type, name, tbl_name, sql FROM sqlite_master \
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid",
    )
    .await?;
    let mut objects = Vec::new();
    for row in d1::parse_json_rows(&out) {
        let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let table = row.get("tbl_name").and_then(|v| v.as_str()).unwrap_or("");
        // celld's migration bookkeeping stays out of the dump (and out of
        // the restore's drops): a restore must never confuse a later
        // `migrations apply`.
        if name == "d1_migrations" || table == "d1_migrations" {
            continue;
        }
        let (Some(kind), Some(sql)) = (
            row.get("type").and_then(|v| v.as_str()),
            row.get("sql").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        objects.push(SchemaObject {
            kind: kind.to_string(),
            name: name.to_string(),
            sql: sql.to_string(),
        });
    }
    let tables: Vec<&SchemaObject> = objects.iter().filter(|o| o.kind == "table").collect();

    // Foreign-key edges for dependency order, one PRAGMA per table.
    let mut deps: HashMap<&str, Vec<String>> = HashMap::new();
    for table in &tables {
        let sql = format!("PRAGMA foreign_key_list({})", d1::quote_ident(&table.name));
        let mut refs = Vec::new();
        for row in
            d1::parse_json_rows(&exec_one(ctx, &app.slug, &identity, project.path(), &sql).await?)
        {
            if let Some(target) = row.get("table").and_then(|v| v.as_str()) {
                refs.push(target.to_string());
            }
        }
        deps.insert(table.name.as_str(), refs);
    }
    let ordered = topo_order(&tables, &deps);

    // Column types for value rendering, one PRAGMA per table.
    let mut columns: HashMap<&str, Vec<(String, String)>> = HashMap::new();
    for table in &tables {
        let sql = format!("PRAGMA table_info({})", d1::quote_ident(&table.name));
        let mut cols: Vec<(i64, String, String)> = Vec::new();
        for row in
            d1::parse_json_rows(&exec_one(ctx, &app.slug, &identity, project.path(), &sql).await?)
        {
            let cid = row.get("cid").and_then(|v| v.as_i64()).unwrap_or(0);
            let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let decltype = row.get("type").and_then(|v| v.as_str()).unwrap_or("");
            cols.push((cid, name.to_string(), decltype.to_string()));
        }
        cols.sort_by_key(|(cid, _, _)| *cid);
        columns.insert(
            table.name.as_str(),
            cols.into_iter()
                .map(|(_, name, decltype)| (name, decltype))
                .collect(),
        );
    }

    let mut dump = format!(
        "-- Outer D1 backup: database {binding:?}\n\
         -- Restore with Console → D1 → Backups, or the restore_backup MCP tool.\n"
    );
    let push = |dump: &mut String, stmt: &str| {
        dump.push_str(stmt);
        dump.push_str(";\n");
    };
    for table in &ordered {
        push(&mut dump, &table.sql);
    }
    // Indexes, triggers and views after the tables they belong to, in
    // catalog order.
    for obj in objects
        .iter()
        .filter(|o| o.kind == "index" || o.kind == "trigger" || o.kind == "view")
    {
        push(&mut dump, &obj.sql);
    }
    let mut bytes = dump.len();

    for table in &ordered {
        let Some(cols) = columns.get(table.name.as_str()) else {
            continue;
        };
        if cols.is_empty() {
            continue;
        }
        let prefix = format!(
            "INSERT INTO {} ({}) VALUES ",
            d1::quote_ident(&table.name),
            cols.iter()
                .map(|(name, _)| d1::quote_ident(name))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut offset = 0i64;
        loop {
            let sql = format!(
                "SELECT * FROM {} LIMIT {ROW_PAGE} OFFSET {offset}",
                d1::quote_ident(&table.name)
            );
            let rows = d1::parse_json_rows(
                &exec_one(ctx, &app.slug, &identity, project.path(), &sql).await?,
            );
            let page_len = rows.len();
            for stmt in pack_inserts(&prefix, cols, &rows) {
                bytes += stmt.len() + 2;
                if bytes > MAX_DUMP_BYTES {
                    return Err(D1Error::TooLarge(format!(
                        "database {binding:?} exceeds the {} MiB dump cap",
                        MAX_DUMP_BYTES / (1024 * 1024)
                    )));
                }
                push(&mut dump, &stmt);
            }
            if page_len < ROW_PAGE as usize {
                break;
            }
            offset += ROW_PAGE;
        }
    }
    Ok(dump)
}

/// Order tables so referenced tables are created first. Repeatedly emits the
/// first catalog table whose prerequisites (references staying inside the
/// dumped set) are all emitted — O(n²), but D1 apps have tens of tables at
/// most, and the scan order keeps the output deterministic. A reference
/// cycle — or a reference to a table outside the dump — can never stall the
/// scan: when no table is emittable, the first leftover goes next in catalog
/// order, so the dump never loses a table.
fn topo_order<'a>(
    tables: &[&'a SchemaObject],
    deps: &HashMap<&str, Vec<String>>,
) -> Vec<&'a SchemaObject> {
    let in_set: std::collections::HashSet<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    let mut emitted: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(tables.len());
    while out.len() < tables.len() {
        let mut progressed = false;
        for table in tables {
            if emitted.contains(table.name.as_str()) {
                continue;
            }
            let blocked = deps
                .get(table.name.as_str())
                .map(|refs| {
                    refs.iter().any(|r| {
                        r != &table.name
                            && in_set.contains(r.as_str())
                            && !emitted.contains(r.as_str())
                    })
                })
                .unwrap_or(false);
            if !blocked {
                emitted.insert(table.name.as_str());
                out.push(*table);
                progressed = true;
            }
        }
        if !progressed {
            // A cycle: emit the first leftover in catalog order. Its CREATE
            // may name a table created later in the file, which SQLite
            // accepts — the reference resolves by the time data lands.
            let leftover = tables
                .iter()
                .find(|t| !emitted.contains(t.name.as_str()))
                .expect("topo: leftover exists while out is short");
            emitted.insert(leftover.name.as_str());
            out.push(*leftover);
        }
    }
    out
}

/// True for a `x'hex'` blob literal on a BLOB-affinity column: it passes
/// through unquoted, since [`d1::literal`] would quote-escape it into a
/// text value. `cell_value` only ever produces NULL or `x'hex'` for such
/// columns (a text-looking `x'…'` in a TEXT column still quotes normally).
/// An empty decltype also has BLOB affinity in SQLite.
fn is_blob_literal(decltype: &str, rendered: &str) -> bool {
    let blob_affinity = decltype.is_empty() || decltype.to_uppercase().contains("BLOB");
    blob_affinity
        && rendered.len() >= 4
        && rendered.starts_with("x'")
        && rendered.ends_with('\'')
        && rendered[2..rendered.len() - 1]
            .chars()
            .all(|c| c.is_ascii_hexdigit())
}

/// Pack one page's rows into multi-row `INSERT` statements of at most
/// [`MAX_STATEMENT_BYTES`] bytes. One row per statement worst case (a single
/// row bigger than the cap fails loudly at restore time instead of silently
/// truncating here).
fn pack_inserts(
    prefix: &str,
    cols: &[(String, String)],
    rows: &[serde_json::Map<String, serde_json::Value>],
) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut current = String::new();
    for row in rows {
        let values: Vec<String> = cols
            .iter()
            .map(|(name, decltype)| {
                let rendered = row.get(name).map(d1::cell_value).unwrap_or(None);
                match rendered {
                    Some(value) if is_blob_literal(decltype, &value) => value,
                    other => d1::literal(decltype, other.as_deref()),
                }
            })
            .collect();
        let tuple = format!("({})", values.join(", "));
        if !current.is_empty()
            && prefix.len() + current.len() + 2 + tuple.len() > MAX_STATEMENT_BYTES
        {
            stmts.push(format!("{prefix}{current}"));
            current.clear();
        }
        if !current.is_empty() {
            current.push_str(", ");
        }
        current.push_str(&tuple);
    }
    if !current.is_empty() {
        stmts.push(format!("{prefix}{current}"));
    }
    stmts
}

/// Split dump SQL back into statements, respecting single-quoted strings
/// (`''` escapes) and `--` line comments. Returns statements without their
/// trailing `;`.
fn split_sql(dump: &str) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut current = String::new();
    let mut chars = dump.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            current.push(c);
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    current.push(chars.next().expect("split: peeked quote"));
                } else {
                    in_string = false;
                }
            }
            continue;
        }
        if c == '\'' {
            in_string = true;
            current.push(c);
            continue;
        }
        if c == '-' && chars.peek() == Some(&'-') {
            // A `--` comment runs to the end of the line.
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            continue;
        }
        if c == ';' {
            let stmt = current.trim().to_string();
            if !stmt.is_empty() {
                stmts.push(stmt);
            }
            current.clear();
            continue;
        }
        current.push(c);
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() {
        stmts.push(tail);
    }
    stmts
}

/// Restore one binding from SQL text: drop the current user tables and
/// views, then replay the statements in [`RESTORE_BATCH_BYTES`] batches.
/// Each batch is one `celld d1 execute`, which brackets it in its own
/// transaction — all-or-nothing per batch, resumable across batches.
pub async fn restore_database(
    ctx: &D1Ctx,
    app: &App,
    binding: &str,
    sql: &str,
) -> Result<RestoreReport, D1Error> {
    let (identity, project) = d1::ready_backup(ctx, app, binding).await?;
    let statements = split_sql(sql);

    // Views first (they may name tables about to go), then the tables the
    // panel itself shows — which already hides `d1_migrations`, so the
    // bookkeeping survives the restore.
    let (_identity, tables) = d1::list_tables(ctx, app, binding).await?;
    let views_out = exec_one(
        ctx,
        &app.slug,
        &identity,
        project.path(),
        "SELECT name FROM sqlite_master WHERE type = 'view' AND name NOT LIKE 'sqlite_%'",
    )
    .await?;
    let mut drops = Vec::new();
    for row in d1::parse_json_rows(&views_out) {
        if let Some(name) = row.get("name").and_then(|v| v.as_str()) {
            drops.push(format!("DROP VIEW IF EXISTS {}", d1::quote_ident(name)));
        }
    }
    for table in &tables {
        drops.push(format!(
            "DROP TABLE IF EXISTS {}",
            d1::quote_ident(&table.name)
        ));
    }
    if !drops.is_empty() {
        exec_one(
            ctx,
            &app.slug,
            &identity,
            project.path(),
            &drops.join(";\n"),
        )
        .await?;
    }

    let mut batch = String::new();
    let mut statements_applied = 0usize;
    let mut bytes_applied = 0usize;
    for stmt in &statements {
        if !batch.is_empty() && batch.len() + 2 + stmt.len() + 1 > RESTORE_BATCH_BYTES {
            exec_one(ctx, &app.slug, &identity, project.path(), &batch).await?;
            statements_applied += batch.matches(';').count();
            bytes_applied += batch.len();
            batch.clear();
        }
        if !batch.is_empty() {
            batch.push_str(";\n");
        }
        batch.push_str(stmt);
        batch.push(';');
    }
    if !batch.trim().is_empty() {
        exec_one(ctx, &app.slug, &identity, project.path(), &batch).await?;
        statements_applied += batch.matches(';').count();
        bytes_applied += batch.len();
    }
    Ok(RestoreReport {
        fresh_backup_id: String::new(),
        tables_dropped: tables.len(),
        statements_applied,
        bytes_applied,
    })
}

/// Take a dump now and store it: the shared path for the sweep, the console
/// "Back up now" button, and the `backup_database` MCP tool. Prunes past
/// retention on the way out, so one call leaves the binding within budget.
pub async fn backup_binding(
    pool: &SqlitePool,
    store: &Store,
    ctx: &D1Ctx,
    app: &App,
    binding: &str,
    now_ms: i64,
) -> Result<BackupRow, D1Error> {
    let sql = dump_database(ctx, app, binding).await?;
    let bytes = sql.into_bytes();
    let size_bytes = bytes.len() as i64;
    let key = backup_key(&app.slug, now_ms, binding);
    store.put(&key, bytes).await.map_err(unavailable)?;
    let row = BackupRow {
        id: uuid::Uuid::new_v4().to_string(),
        binding: binding.to_string(),
        store_key: key,
        created_at: now_ms,
        size_bytes,
    };
    sqlx::query(
        "INSERT INTO d1_backup (id, app_id, binding, store_key, created_at, size_bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&row.id)
    .bind(&app.id)
    .bind(&row.binding)
    .bind(&row.store_key)
    .bind(row.created_at)
    .bind(row.size_bytes)
    .execute(pool)
    .await
    .map_err(unavailable)?;
    let plan = owner_plan(pool, &app.owner_id).await.map_err(unavailable)?;
    prune_app(pool, store, &app.id, retention_ms(&plan), now_ms)
        .await
        .map_err(unavailable)?;
    Ok(row)
}

/// Restore one stored dump: take a fresh backup first (so a bad restore is
/// itself restorable), then replay the dump into the same database.
pub async fn restore_binding(
    pool: &SqlitePool,
    store: &Store,
    ctx: &D1Ctx,
    app: &App,
    binding: &str,
    backup_id: &str,
    now_ms: i64,
) -> Result<(BackupRow, RestoreReport), D1Error> {
    let Some(row) = backup_by_id(pool, &app.id, backup_id)
        .await
        .map_err(unavailable)?
    else {
        return Err(D1Error::UnknownBackup(backup_id.to_string()));
    };
    if row.binding != binding {
        return Err(D1Error::UnknownBackup(backup_id.to_string()));
    }
    let fresh = backup_binding(pool, store, ctx, app, binding, now_ms).await?;
    let Some(bytes) = store.get(&row.store_key).await.map_err(unavailable)? else {
        return Err(D1Error::Unavailable(anyhow::anyhow!(
            "backup {} has no stored bytes",
            row.id
        )));
    };
    let sql = std::string::String::from_utf8(bytes)
        .map_err(|_| D1Error::Celld(format!("backup {} is not valid UTF-8", row.id)))?;
    let mut report = restore_database(ctx, app, binding, &sql).await?;
    report.fresh_backup_id = fresh.id.clone();
    Ok((fresh, report))
}

// ---------------------------------------------------------------------------
// Sweep
// ---------------------------------------------------------------------------

/// Apps that may need a backup: serving apps (`desired_state` running,
/// moderation ok, neither paused nor expired) with a request or a
/// successful deploy in the last 24 h. Sleeping apps with no recent
/// activity keep sleeping — their D1 cannot have changed since the last
/// backup, and waking them nightly would defeat the sleep they earned.
pub async fn backup_due_apps(pool: &SqlitePool, now_ms: i64) -> anyhow::Result<Vec<App>> {
    let cutoff = now_ms.saturating_sub(BACKUP_EVERY_MS);
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT a.id FROM app AS a \
         WHERE a.desired_state = 'running' \
           AND a.moderation_state = 'ok' \
           AND a.paused_reason IS NULL \
           AND a.expired_at IS NULL \
           AND (a.last_request_at >= ?1 \
                OR EXISTS (SELECT 1 FROM deploy AS d \
                            WHERE d.app_id = a.id \
                              AND d.status = 'success' \
                              AND d.created_at >= ?1))",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(app) = crate::apps::get_by_id(pool, id).await? {
            out.push(app);
        }
    }
    Ok(out)
}

/// One sweep pass: back up every due binding of every due app, then prune
/// each touched app to its plan's retention even when nothing was due (a
/// downgrade shortens retention). One app's failure warns and the sweep
/// continues with the next.
pub async fn tick(pool: &SqlitePool, store: &Store, fleets: &FleetManager) -> anyhow::Result<()> {
    let now = crate::apps::now_ms();
    for app in backup_due_apps(pool, now).await? {
        let bindings = d1::database_bindings(store, &app.slug).await?;
        if bindings.is_empty() {
            continue;
        }
        let ctx = D1Ctx {
            celld_bin: fleets.celld_bin(),
            storage: fleets.storage().clone(),
            pool: pool.clone(),
            store: store.clone(),
            wake: Some(fleets.clone()),
        };
        let plan = owner_plan(pool, &app.owner_id).await?;
        for binding in &bindings {
            match backup_due(pool, &app.id, binding, now).await {
                Ok(false) => {}
                Ok(true) => match backup_binding(pool, store, &ctx, &app, binding, now).await {
                    Ok(row) => tracing::info!(
                        slug = %app.slug,
                        binding = %binding,
                        bytes = row.size_bytes,
                        "d1 backup stored"
                    ),
                    Err(err) => tracing::warn!(
                        slug = %app.slug,
                        binding = %binding,
                        error = %format!("{err:#}"),
                        "d1 backup failed; the next sweep retries"
                    ),
                },
                Err(err) => tracing::warn!(
                    slug = %app.slug,
                    binding = %binding,
                    error = %format!("{err:#}"),
                    "d1 backup due-check failed"
                ),
            }
        }
        if let Err(err) = prune_app(pool, store, &app.id, retention_ms(&plan), now).await {
            tracing::warn!(
                slug = %app.slug,
                error = %format!("{err:#}"),
                "d1 backup prune failed"
            );
        }
    }
    Ok(())
}

/// A tokio interval loop for the daily backups. Failures log and the loop
/// continues on the next tick.
pub fn spawn_backup_task(pool: SqlitePool, store: Store, fleets: FleetManager, every: Duration) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        loop {
            interval.tick().await;
            if let Err(err) = tick(&pool, &store, &fleets).await {
                tracing::warn!(error = %format!("{err:#}"), "d1 backup sweep failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_object(kind: &str, name: &str, sql: &str) -> SchemaObject {
        SchemaObject {
            kind: kind.to_string(),
            name: name.to_string(),
            sql: sql.to_string(),
        }
    }

    #[test]
    fn backup_keys_sort_by_time_under_one_slug() {
        let early = backup_key("acme", 1000, "DB");
        let late = backup_key("acme", 2000, "DB");
        assert!(early < late);
        assert!(early.starts_with("outer/backups/acme/"));
        assert!(late.ends_with(".sql"));
        // Backups never land where app code can reach them.
        assert!(!early.starts_with("fleets/"));
    }

    #[test]
    fn backup_key_sanitizes_hostile_bindings() {
        assert_eq!(
            backup_key("acme", 7, "../../etc"),
            "outer/backups/acme/7-______etc.sql"
        );
        assert_eq!(backup_key("acme", 7, ""), "outer/backups/acme/7-db.sql");
        assert_eq!(
            backup_key("acme", 7, "my-db_2"),
            "outer/backups/acme/7-my-db_2.sql"
        );
    }

    #[test]
    fn retention_is_7_days_free_30_pro() {
        assert_eq!(retention_ms("free"), 7 * 24 * 60 * 60 * 1000);
        assert_eq!(retention_ms("pro"), 30 * 24 * 60 * 60 * 1000);
        assert_eq!(retention_ms("anything-else"), FREE_RETENTION_MS);
    }

    #[test]
    fn topo_order_puts_parents_first() {
        let users = schema_object(
            "table",
            "users",
            "CREATE TABLE users (id INTEGER PRIMARY KEY)",
        );
        let orders = schema_object(
            "table",
            "orders",
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id))",
        );
        let items = schema_object(
            "table",
            "items",
            "CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER REFERENCES orders(id))",
        );
        let tables = vec![&items, &orders, &users];
        let mut deps = HashMap::new();
        deps.insert("orders", vec!["users".to_string()]);
        deps.insert("items", vec!["orders".to_string()]);
        let ordered = topo_order(&tables, &deps);
        let names: Vec<&str> = ordered.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["users", "orders", "items"]);
    }

    #[test]
    fn topo_order_survives_a_reference_cycle() {
        let a = schema_object(
            "table",
            "a",
            "CREATE TABLE a (id INTEGER PRIMARY KEY, b_id INTEGER REFERENCES b(id))",
        );
        let b = schema_object(
            "table",
            "b",
            "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a(id))",
        );
        let tables = vec![&a, &b];
        let mut deps = HashMap::new();
        deps.insert("a", vec!["b".to_string()]);
        deps.insert("b", vec!["a".to_string()]);
        let ordered = topo_order(&tables, &deps);
        assert_eq!(ordered.len(), 2);
        // Catalog order for the cycle: no table lost, no hang.
        assert_eq!(ordered[0].name, "a");
        assert_eq!(ordered[1].name, "b");
    }

    #[test]
    fn topo_order_ignores_references_outside_the_dump() {
        let users = schema_object(
            "table",
            "users",
            "CREATE TABLE users (id INTEGER PRIMARY KEY)",
        );
        let tables = vec![&users];
        let mut deps = HashMap::new();
        deps.insert("users", vec!["d1_migrations".to_string()]);
        let ordered = topo_order(&tables, &deps);
        assert_eq!(ordered.len(), 1);
    }

    #[test]
    fn split_sql_respects_quotes_and_comments() {
        let dump = "-- a header comment\n\
                    CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT);\n\
                    INSERT INTO \"t\" (\"note\") VALUES ('it''s; tricky -- here');\n\
                    INSERT INTO \"t\" (\"note\") VALUES ('plain');\n";
        let stmts = split_sql(dump);
        assert_eq!(stmts.len(), 3);
        assert!(stmts[1].contains("it''s; tricky -- here"));
        assert!(!stmts.iter().any(|s| s.starts_with("--")));
    }

    #[test]
    fn pack_inserts_stays_under_the_statement_cap() {
        let prefix = "INSERT INTO \"t\" (\"note\") VALUES ";
        let cols = vec![("note".to_string(), "TEXT".to_string())];
        let big = "x".repeat(1000);
        let rows: Vec<serde_json::Map<String, serde_json::Value>> = (0..200)
            .map(|i| {
                let mut row = serde_json::Map::new();
                row.insert(
                    "note".to_string(),
                    serde_json::Value::String(format!("{big}-{i}")),
                );
                row
            })
            .collect();
        let stmts = pack_inserts(prefix, &cols, &rows);
        assert!(stmts.len() > 1);
        for stmt in &stmts {
            assert!(stmt.len() <= MAX_STATEMENT_BYTES, "{}", stmt.len());
        }
        let tuples: usize = stmts.iter().map(|s| s.matches("), (").count() + 1).sum();
        assert_eq!(tuples, 200);
    }

    // --- fake celld ------------------------------------------------------

    use crate::auth::test_support::test_pool;
    use crate::fleet::manager::FleetStorage;

    /// A fake `celld` answering the backup engine's exact queries: the
    /// schema read, per-table PRAGMAs, row pages, the restore's table/view
    /// reads, and (by logging + `exit 0`) every drop/replay batch. The state
    /// dir is baked into the script, so parallel tests never share it.
    fn install_fake_celld(state: &std::path::Path) -> std::path::PathBuf {
        let script = state.join("fake-celld");
        let tmp = script.with_extension("tmp");
        let log = state.join("calls.log").to_string_lossy().into_owned();
        std::fs::write(
            &tmp,
            format!(
                r#"#!/bin/sh
command_of() {{
  prev=""
  for a in "$@"; do
    if [ "$prev" = "--command" ]; then printf '%s' "$a"; return 0; fi
    prev="$a"
  done
}}
sql=$(command_of "$@")
printf '%s\n' "$sql" >> "{log}"
case "$sql" in
  "SELECT type, name, tbl_name, sql"*)
    printf '%s\n' '{{"type":"table","name":"users","tbl_name":"users","sql":"CREATE TABLE \"users\" (\"id\" INTEGER PRIMARY KEY, \"name\" TEXT, \"avatar\" BLOB)"}}' '{{"type":"table","name":"orders","tbl_name":"orders","sql":"CREATE TABLE \"orders\" (\"id\" INTEGER PRIMARY KEY, \"user_id\" INTEGER REFERENCES \"users\" (\"id\"))"}}' '{{"type":"table","name":"d1_migrations","tbl_name":"d1_migrations","sql":"CREATE TABLE d1_migrations (id TEXT)"}}' '{{"type":"index","name":"idx_orders_user","tbl_name":"orders","sql":"CREATE INDEX \"idx_orders_user\" ON \"orders\" (\"user_id\")"}}' '{{"type":"view","name":"big","tbl_name":"big","sql":"CREATE VIEW \"big\" AS SELECT * FROM \"users\""}}'
    ;;
  'PRAGMA foreign_key_list("orders")')
    printf '%s\n' '{{"table":"users","from":"user_id","to":"id"}}'
    ;;
  'PRAGMA foreign_key_list("users")')
    ;;
  'PRAGMA table_info("users")')
    printf '%s\n' '{{"cid":0,"name":"id","type":"INTEGER"}}' '{{"cid":1,"name":"name","type":"TEXT"}}' '{{"cid":2,"name":"avatar","type":"BLOB"}}'
    ;;
  'PRAGMA table_info("orders")')
    printf '%s\n' '{{"cid":0,"name":"id","type":"INTEGER"}}' '{{"cid":1,"name":"user_id","type":"INTEGER"}}'
    ;;
  'SELECT * FROM "users"'*)
    printf '%s\n' '{{"id":1,"name":"ada"}}' '{{"id":2,"name":"o'"'"'brien; -- x","avatar":[222,173]}}'
    ;;
  'SELECT * FROM "orders"'*)
    printf '%s\n' '{{"id":7,"user_id":1}}'
    ;;
  "SELECT name FROM sqlite_master WHERE type = 'view'"*)
    printf '%s\n' '{{"name":"big"}}'
    ;;
  "PRAGMA table_list")
    printf '%s\n' '{{"type":"table","name":"users"}}' '{{"type":"table","name":"orders"}}' '{{"type":"table","name":"_hidden"}}'
    ;;
  *"COUNT(*) AS n"*)
    printf '%s\n' '{{"t":"users","n":2}}' '{{"t":"orders","n":1}}'
    ;;
  # Drop and replay batches: logged for order assertions, then accepted.
  'DROP '* | 'CREATE '* | 'INSERT '*)
    ;;
  *) echo "unexpected command: $sql" >&2; exit 1;;
esac
exit 0
"#,
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::rename(&tmp, &script).unwrap();
        script
    }

    fn backup_ctx(
        pool: &SqlitePool,
        state: &tempfile::TempDir,
        script: &std::path::Path,
        store: Store,
    ) -> D1Ctx {
        D1Ctx {
            celld_bin: script.to_string_lossy().into_owned(),
            storage: FleetStorage {
                endpoint: "http://127.0.0.1:1".to_string(),
                region: "test-region".to_string(),
                bucket: state.path().to_string_lossy().into_owned(),
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: "outeraccess".to_string(),
                    secret_access_key: "outersecretoutersecretoutersecret12".to_string(),
                },
            },
            pool: pool.clone(),
            store,
            wake: None,
        }
    }

    /// One deployed app with a single DB binding, as the sweep would find it.
    async fn seed_backup_app(pool: &SqlitePool, store: &Store, now: i64) {
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) \
             VALUES ('acct-ana', 'ana@example.com', ?1, ?1, ?1)",
        )
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_request_at, last_activity_at, created_at) \
             VALUES ('app-acme', 'acme', 'acct-ana', 300001, 18001, 18002, ?1, ?1, ?1)",
        )
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        store
            .put(
                "fleets/acme/deploy/current.json",
                br#"{"script_name":"outer-app","version":"v1","prefix":"deploy/outer-app/v1"}"#
                    .to_vec(),
            )
            .await
            .unwrap();
        store
            .put(
                "fleets/acme/deploy/outer-app/v1/manifest.json",
                serde_json::json!({
                    "schema_version": 2,
                    "script_name": "outer-app",
                    "main_module": "index.js",
                    "raw_metadata": {
                        "bindings": [
                            {"type": "d1", "name": "DB", "database_name": "app-db", "database_id": "abc-123"}
                        ],
                        "compatibility_date": "2026-09-01",
                        "compatibility_flags": ["nodejs_als"],
                        "main_module": "index.js"
                    }
                })
                .to_string()
                .into_bytes(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dump_orders_parents_first_and_renders_values() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        let now = crate::apps::now_ms();
        seed_backup_app(&pool, &store, now).await;
        let ctx = backup_ctx(&pool, &state, &script, store);
        let app = crate::apps::get_by_slug(&pool, "acme")
            .await
            .unwrap()
            .unwrap();

        let dump = dump_database(&ctx, &app, "DB").await.unwrap();

        let users_at = dump.find("CREATE TABLE \"users\"").expect("users DDL");
        let orders_at = dump.find("CREATE TABLE \"orders\"").expect("orders DDL");
        assert!(users_at < orders_at, "parents are created first");
        assert!(dump.contains("CREATE INDEX \"idx_orders_user\""));
        assert!(dump.contains("CREATE VIEW \"big\""));
        assert!(!dump.contains("d1_migrations"), "bookkeeping stays out");
        assert!(
            dump.contains(
                "INSERT INTO \"users\" (\"id\", \"name\", \"avatar\") VALUES (1, 'ada', NULL), (2, 'o''brien; -- x', x'dead')"
            ),
            "{dump}"
        );
        assert!(dump.contains("INSERT INTO \"orders\" (\"id\", \"user_id\") VALUES (7, 1)"));
    }

    #[tokio::test]
    async fn restore_replays_drops_then_dump_after_a_fresh_backup() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        let now = crate::apps::now_ms();
        seed_backup_app(&pool, &store, now).await;
        let ctx = backup_ctx(&pool, &state, &script, store.clone());
        let app = crate::apps::get_by_slug(&pool, "acme")
            .await
            .unwrap()
            .unwrap();

        let first = backup_binding(&pool, &store, &ctx, &app, "DB", now)
            .await
            .unwrap();
        let (fresh, report) =
            restore_binding(&pool, &store, &ctx, &app, "DB", &first.id, now + 1000)
                .await
                .unwrap();

        assert_ne!(fresh.id, first.id);
        assert_eq!(report.tables_dropped, 2);
        assert!(report.statements_applied > 0, "{report:?}");
        assert!(!report.fresh_backup_id.is_empty());
        let calls = std::fs::read_to_string(state.path().join("calls.log")).unwrap();
        let drop_view = calls
            .find("DROP VIEW IF EXISTS \"big\"")
            .expect("drops the view");
        let drop_tables = calls
            .find("DROP TABLE IF EXISTS")
            .expect("drops the tables");
        let replay = calls
            .find("CREATE TABLE \"users\"")
            .expect("replays the dump");
        assert!(drop_view < drop_tables && drop_tables < replay);
        // Fresh backup + restore target coexist: nothing pruned within retention.
        assert_eq!(backups_for(&pool, &app.id).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn prune_keeps_retention_and_drops_bytes() {
        let pool = test_pool().await;
        let store = Store::memory();
        let now = crate::apps::now_ms();
        seed_backup_app(&pool, &store, now).await;
        let app = crate::apps::get_by_slug(&pool, "acme")
            .await
            .unwrap()
            .unwrap();
        for (id, created_at) in [("old", now - FREE_RETENTION_MS - 1), ("fresh", now - 1000)] {
            let key = backup_key(&app.slug, created_at, "DB");
            store.put(&key, b"dump".to_vec()).await.unwrap();
            sqlx::query(
                "INSERT INTO d1_backup (id, app_id, binding, store_key, created_at, size_bytes) \
                 VALUES (?1, ?2, 'DB', ?3, ?4, 4)",
            )
            .bind(id)
            .bind(&app.id)
            .bind(&key)
            .bind(created_at)
            .execute(&pool)
            .await
            .unwrap();
        }

        assert_eq!(
            prune_app(&pool, &store, &app.id, FREE_RETENTION_MS, now)
                .await
                .unwrap(),
            1
        );
        assert!(backup_by_id(&pool, &app.id, "old").await.unwrap().is_none());
        assert!(store
            .get(&backup_key(&app.slug, now - FREE_RETENTION_MS - 1, "DB"))
            .await
            .unwrap()
            .is_none());
        assert!(backup_by_id(&pool, &app.id, "fresh")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn backup_due_apps_needs_recent_activity() {
        let pool = test_pool().await;
        let store = Store::memory();
        let now = crate::apps::now_ms();
        seed_backup_app(&pool, &store, now).await;
        let app = crate::apps::get_by_slug(&pool, "acme")
            .await
            .unwrap()
            .unwrap();

        // A day-old request with no deploy since: not due.
        sqlx::query("UPDATE app SET last_request_at = ?1 WHERE id = ?2")
            .bind(now - BACKUP_EVERY_MS - 1)
            .bind(&app.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(backup_due_apps(&pool, now).await.unwrap().is_empty());

        // A fresh request makes it due again; a stored dump quiets it.
        crate::apps::touch_request(&pool, &app.id, now)
            .await
            .unwrap();
        assert_eq!(backup_due_apps(&pool, now).await.unwrap().len(), 1);
        assert!(backup_due(&pool, &app.id, "DB", now).await.unwrap());
        sqlx::query(
            "INSERT INTO d1_backup (id, app_id, binding, store_key, created_at, size_bytes) \
             VALUES ('b1', ?1, 'DB', 'outer/backups/acme/1-DB.sql', ?2, 4)",
        )
        .bind(&app.id)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        assert!(!backup_due(&pool, &app.id, "DB", now).await.unwrap());
    }
}
