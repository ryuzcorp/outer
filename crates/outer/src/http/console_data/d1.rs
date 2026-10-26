// Derived from noite apps/runner/src/host/storage/d1.rs @ 58a977a (Apache-2.0).
//
// What changed for Outer: no collaborator roles (owner-only in v1, so no
// `redacted`/`locked`/`caps`/`rowAction` plumbing); the database is addressed
// by its declared `binding` and resolved server-side to the celld identity;
// credentials come from `store::credentials` scoped to `fleets/{slug}/`
// exactly like `deploy::migrate`; the fleet wakes through the fleet manager;
// every write lands in `agent_action` with client "Console".

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path as FsPath;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;

use super::owner_app;
use crate::deploy::migrate::{self, D1Database};
use crate::fleet::manager::{FleetManager, FleetStorage};
use crate::http::console::guard;
use crate::store::Store;

// D1 reads and writes go through the shared owner check in `super`
// (owner-only in v1, spec §7.4); the noun keeps this panel's messages.

/// Filters are AND-ed and bounded so a query can't grow without limit.
pub const MAX_FILTERS: usize = 10;
/// One `delete-rows` call carries at most this many keys.
pub const MAX_DELETE_KEYS: usize = 100;
/// Page size when the caller omits it.
pub const DEFAULT_PAGE_SIZE: i64 = 25;
/// The server clamps every page size into this range.
pub const MIN_PAGE_SIZE: i64 = 1;
pub const MAX_PAGE_SIZE: i64 = 100;
/// `celld d1 execute` budget, matching Noite's 30 s.
pub const D1_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Wire types (snake_case, matching the console's other APIs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct D1Column {
    pub name: String,
    /// Declared type; "" when the DDL has none.
    #[serde(rename = "type")]
    pub col_type: String,
    pub not_null: bool,
    /// DDL default expression text; null when none.
    pub default_value: Option<String>,
    /// 0 = not part of the PK, else 1-based position.
    pub pk: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct D1ForeignKey {
    pub from: String,
    pub table: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct D1Index {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct D1TableSchema {
    pub binding: String,
    pub table: String,
    pub columns: Vec<D1Column>,
    pub foreign_keys: Vec<D1ForeignKey>,
    pub indexes: Vec<D1Index>,
    pub sql: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct D1TableInfo {
    pub name: String,
    pub row_count: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum D1FilterOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    Like,
    IsNull,
    NotNull,
}

#[derive(Debug, Clone, Deserialize)]
pub struct D1Filter {
    pub column: String,
    #[serde(default)]
    pub op: Option<D1FilterOp>,
    /// Ignored for is_null/not_null; `like` takes the user's pattern with
    /// `%` wildcards as typed.
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct D1Sort {
    pub column: String,
    #[serde(default)]
    pub desc: bool,
}

/// POST `…/tables/{table}/rows` body.
#[derive(Debug, Clone, Deserialize)]
pub struct RowsBody {
    /// 0-based.
    #[serde(default)]
    pub page: i64,
    #[serde(default = "default_page_size", alias = "pageSize")]
    pub page_size: i64,
    /// null = primary key order (rowid when the table has no PK).
    #[serde(default)]
    pub sort: Option<D1Sort>,
    #[serde(default)]
    pub filters: Vec<D1Filter>,
    /// "" = none; case-insensitive substring over every column.
    #[serde(default)]
    pub search: String,
}

fn default_page_size() -> i64 {
    DEFAULT_PAGE_SIZE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum D1WriteOp {
    Insert,
    Update,
    Delete,
}

/// POST `…/tables/{table}/write` body.
#[derive(Debug, Clone, Deserialize)]
pub struct WriteBody {
    pub op: D1WriteOp,
    /// update/delete row identity.
    #[serde(default)]
    pub key: BTreeMap<String, Option<String>>,
    /// insert/update; null = SQL NULL, "" = empty string.
    #[serde(default)]
    pub values: BTreeMap<String, Option<String>>,
}

/// POST `…/tables/{table}/delete-rows` body.
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteRowsBody {
    pub keys: Vec<BTreeMap<String, Option<String>>>,
}

// ---------------------------------------------------------------------------
// SQL building (pure — unit-tested)
// ---------------------------------------------------------------------------

/// One table's schema as the builders see it.
#[derive(Debug, Clone)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<D1Column>,
}

impl TableSchema {
    fn column(&self, name: &str) -> Option<&D1Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// PK column names ordered by their 1-based `pk` position.
    fn pk_columns(&self) -> Vec<&str> {
        let mut pk: Vec<(&str, i64)> = self
            .columns
            .iter()
            .filter(|c| c.pk > 0)
            .map(|c| (c.name.as_str(), c.pk))
            .collect();
        pk.sort_by_key(|(_, pos)| *pos);
        pk.into_iter().map(|(name, _)| name).collect()
    }
}

/// True for the real, user-facing tables (internals and the migration
/// bookkeeping table are hidden).
pub fn visible_table(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('_')
        && !name.starts_with("sqlite_")
        && name != "d1_migrations"
}

/// Quote an SQLite identifier — the only identifier splicing in the D1 path.
/// Callers pass allowlisted table/column names only.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A single-quoted SQL string literal.
fn string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Escape the LIKE metacharacters (`\`, `%`, `_`) so a search needle matches
/// literally under `ESCAPE '\'`.
fn escape_like(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len());
    for ch in needle.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Render a UI value for a column: JSON null → NULL, "" → the empty string;
/// numbers for INTEGER/REAL-affinity columns stay bare so REAL precision is
/// preserved, everything else is quote-escaped.
pub(crate) fn literal(decltype: &str, value: Option<&str>) -> String {
    let Some(v) = value else {
        return "NULL".to_string();
    };
    let upper = decltype.to_uppercase();
    let numeric = upper.contains("INT")
        || upper.contains("REAL")
        || upper.contains("FLOA")
        || upper.contains("DOUB");
    if numeric && v.parse::<f64>().is_ok() {
        return v.to_string();
    }
    if v.is_empty() {
        return "''".to_string();
    }
    string_literal(v)
}

/// A column reference validated against the schema's allowlist.
fn check_column<'a>(schema: &'a TableSchema, name: &str) -> anyhow::Result<&'a D1Column> {
    schema
        .column(name)
        .ok_or_else(|| anyhow::anyhow!("unknown column {name}"))
}

/// `WHERE` conditions for the filters and search (AND-ed).
fn where_clause(
    schema: &TableSchema,
    filters: &[D1Filter],
    search: &str,
) -> anyhow::Result<Option<String>> {
    if filters.len() > MAX_FILTERS {
        anyhow::bail!("at most {MAX_FILTERS} filters");
    }
    let mut parts: Vec<String> = Vec::new();
    for filter in filters {
        let op = filter.op.unwrap_or(D1FilterOp::Eq);
        let col = check_column(schema, &filter.column)?;
        let id = quote_ident(&col.name);
        let part = match op {
            D1FilterOp::Eq => format!("{id} = {}", literal(&col.col_type, Some(&filter.value))),
            D1FilterOp::Neq => {
                format!("{id} <> {}", literal(&col.col_type, Some(&filter.value)))
            }
            D1FilterOp::Lt => format!("{id} < {}", literal(&col.col_type, Some(&filter.value))),
            D1FilterOp::Lte => {
                format!("{id} <= {}", literal(&col.col_type, Some(&filter.value)))
            }
            D1FilterOp::Gt => format!("{id} > {}", literal(&col.col_type, Some(&filter.value))),
            D1FilterOp::Gte => {
                format!("{id} >= {}", literal(&col.col_type, Some(&filter.value)))
            }
            // The user's pattern as typed (its own `%`/`_` wildcards stay live).
            D1FilterOp::Like => format!("{id} LIKE {}", string_literal(&filter.value)),
            D1FilterOp::IsNull => format!("{id} IS NULL"),
            D1FilterOp::NotNull => format!("{id} IS NOT NULL"),
        };
        parts.push(part);
    }
    if !search.is_empty() {
        let pattern = format!("%{}%", escape_like(search));
        let needle = string_literal(&pattern);
        let mut ors: Vec<String> = Vec::new();
        for col in &schema.columns {
            ors.push(format!(
                "CAST({} AS TEXT) LIKE {needle} ESCAPE '\\'",
                quote_ident(&col.name)
            ));
        }
        if !ors.is_empty() {
            parts.push(format!("({})", ors.join(" OR ")));
        }
    }
    if parts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(parts.join(" AND ")))
    }
}

/// `ORDER BY`: the requested column, else the primary key, else `rowid`.
fn order_clause(schema: &TableSchema, sort: Option<&D1Sort>) -> anyhow::Result<String> {
    if let Some(sort) = sort {
        let col = check_column(schema, &sort.column)?;
        let dir = if sort.desc { "DESC" } else { "ASC" };
        return Ok(format!("ORDER BY {} {dir}", quote_ident(&col.name)));
    }
    let pk = schema.pk_columns();
    if pk.is_empty() {
        // A WITHOUT ROWID table always has a PK, so no PK means rowid is valid.
        Ok("ORDER BY rowid ASC".to_string())
    } else {
        let parts: Vec<String> = pk
            .iter()
            .map(|name| format!("{} ASC", quote_ident(name)))
            .collect();
        Ok(format!("ORDER BY {}", parts.join(", ")))
    }
}

/// The page query: every column, filters/search, order and pagination.
pub fn build_select(
    schema: &TableSchema,
    filters: &[D1Filter],
    search: &str,
    sort: Option<&D1Sort>,
    page: i64,
    page_size: i64,
) -> anyhow::Result<String> {
    let columns: Vec<String> = schema
        .columns
        .iter()
        .map(|c| quote_ident(&c.name))
        .collect();
    let mut sql = format!(
        "SELECT {} FROM {}",
        columns.join(", "),
        quote_ident(&schema.name)
    );
    if let Some(where_) = where_clause(schema, filters, search)? {
        sql.push_str(&format!(" WHERE {where_}"));
    }
    sql.push_str(&format!(" {}", order_clause(schema, sort)?));
    let offset = page.max(0) * page_size;
    sql.push_str(&format!(" LIMIT {page_size} OFFSET {offset}"));
    Ok(sql)
}

/// COUNT(*) under the same filters/search as the page query.
pub fn build_count(
    schema: &TableSchema,
    filters: &[D1Filter],
    search: &str,
) -> anyhow::Result<String> {
    let mut sql = format!("SELECT COUNT(*) AS n FROM {}", quote_ident(&schema.name));
    if let Some(where_) = where_clause(schema, filters, search)? {
        sql.push_str(&format!(" WHERE {where_}"));
    }
    Ok(sql)
}

/// Validate a write/delete key: every column exists, and the key names exactly
/// the PK (or every column when the table has no PK).
fn check_key(schema: &TableSchema, key: &BTreeMap<String, Option<String>>) -> anyhow::Result<()> {
    if key.is_empty() {
        anyhow::bail!("needs a key");
    }
    for name in key.keys() {
        check_column(schema, name)?;
    }
    let given: BTreeSet<&str> = key.keys().map(String::as_str).collect();
    let pk = schema.pk_columns();
    if pk.is_empty() {
        let all: BTreeSet<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
        if given != all {
            anyhow::bail!("table has no primary key: the key must name every column");
        }
    } else {
        let expected: BTreeSet<&str> = pk.into_iter().collect();
        if given != expected {
            anyhow::bail!("key must match the primary key");
        }
    }
    Ok(())
}

/// `col = value` / `col IS NULL` conjunctions for a key.
fn key_conditions(
    schema: &TableSchema,
    key: &BTreeMap<String, Option<String>>,
) -> anyhow::Result<String> {
    check_key(schema, key)?;
    let mut out: Vec<String> = Vec::new();
    for (name, value) in key {
        let col = check_column(schema, name)?;
        out.push(match value {
            None => format!("{} IS NULL", quote_ident(name)),
            Some(v) => format!(
                "{} = {}",
                quote_ident(name),
                literal(&col.col_type, Some(v))
            ),
        });
    }
    Ok(out.join(" AND "))
}

pub fn build_insert(
    schema: &TableSchema,
    values: &BTreeMap<String, Option<String>>,
) -> anyhow::Result<String> {
    if values.is_empty() {
        anyhow::bail!("no values");
    }
    let mut names: Vec<String> = Vec::new();
    let mut vals: Vec<String> = Vec::new();
    for (name, value) in values {
        let col = check_column(schema, name)?;
        names.push(quote_ident(name));
        vals.push(literal(&col.col_type, value.as_deref()));
    }
    Ok(format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote_ident(&schema.name),
        names.join(", "),
        vals.join(", ")
    ))
}

pub fn build_update(
    schema: &TableSchema,
    values: &BTreeMap<String, Option<String>>,
    key: &BTreeMap<String, Option<String>>,
) -> anyhow::Result<String> {
    if values.is_empty() {
        anyhow::bail!("no values");
    }
    let mut sets: Vec<String> = Vec::new();
    for (name, value) in values {
        let col = check_column(schema, name)?;
        sets.push(format!(
            "{} = {}",
            quote_ident(name),
            literal(&col.col_type, value.as_deref())
        ));
    }
    Ok(format!(
        "UPDATE {} SET {} WHERE {}",
        quote_ident(&schema.name),
        sets.join(", "),
        key_conditions(schema, key)?
    ))
}

pub fn build_delete(
    schema: &TableSchema,
    key: &BTreeMap<String, Option<String>>,
) -> anyhow::Result<String> {
    Ok(format!(
        "DELETE FROM {} WHERE {}",
        quote_ident(&schema.name),
        key_conditions(schema, key)?
    ))
}

/// One atomic batch of DELETEs, each followed by `changes()` so the caller can
/// total the rows actually removed. `celld d1 execute` brackets several
/// statements in one transaction, so a failure rolls the whole batch back.
pub fn build_delete_keys(
    schema: &TableSchema,
    keys: &[BTreeMap<String, Option<String>>],
) -> anyhow::Result<String> {
    if keys.is_empty() {
        anyhow::bail!("no keys");
    }
    if keys.len() > MAX_DELETE_KEYS {
        anyhow::bail!("at most {MAX_DELETE_KEYS} rows per delete");
    }
    let mut sql = String::new();
    for key in keys {
        sql.push_str(&format!(
            "DELETE FROM {} WHERE {}; SELECT changes() AS n; ",
            quote_ident(&schema.name),
            key_conditions(schema, key)?
        ));
    }
    Ok(sql
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
        .to_string())
}

/// One exec for every table's row count:
/// `SELECT 'name' AS t, COUNT(*) AS n FROM "name" UNION ALL …`.
pub fn build_table_counts(tables: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for name in tables {
        parts.push(format!(
            "SELECT {} AS t, COUNT(*) AS n FROM {}",
            string_literal(name),
            quote_ident(name)
        ));
    }
    parts.join(" UNION ALL ")
}

/// Tag column stamping every row of a batched exec so `split_batched` can
/// route the concatenated result sets back apart. `celld d1 execute` runs
/// several `;`-separated statements in one call and prints one JSON object
/// per row, so without a tag the sets would be indistinguishable.
pub const BATCH_TAG: &str = "_q";

/// One exec for the whole Definition schema except per-index columns:
/// table_info + foreign_key_list + index_list + the sqlite_master DDL, each
/// `SELECT` stamping its rows with a tag (see `BATCH_TAG`).
pub fn build_schema_batch(table: &str) -> String {
    let name = string_literal(table);
    format!(
        "SELECT 'ti' AS \"{tag}\", \"cid\", \"name\", \"type\", \"notnull\", \"dflt_value\", \"pk\" \
         FROM pragma_table_info({name}); \
         SELECT 'fk' AS \"{tag}\", \"from\", \"table\", \"to\" \
         FROM pragma_foreign_key_list({name}); \
         SELECT 'il' AS \"{tag}\", \"name\", \"unique\" FROM pragma_index_list({name}); \
         SELECT 'ddl' AS \"{tag}\", \"sql\" FROM sqlite_master \
         WHERE type = 'table' AND name = {name}",
        tag = BATCH_TAG,
    )
}

/// One exec for every index's columns (`None` when there are no indexes).
/// Each `SELECT` stamps its rows `ii` and carries the index name in `_idx`,
/// so `split_batched` callers can group columns back per index.
pub fn build_index_info_batch(indexes: &[String]) -> Option<String> {
    if indexes.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for name in indexes {
        let literal = string_literal(name);
        parts.push(format!(
            "SELECT 'ii' AS \"{tag}\", {literal} AS \"_idx\", \"seqno\", \"name\" AS \"col\" \
             FROM pragma_index_info({literal})",
            tag = BATCH_TAG,
        ));
    }
    Some(parts.join("; "))
}

/// Rows page plus total in one exec: the caller-built SELECTs wrapped as
/// subqueries so their column lists stay untouched, each stamping its rows.
/// Only valid when no user column is itself named `_q` (see
/// `batch_tag_collides`) — the tag would collide with it.
pub fn build_rows_batch(select: &str, count: &str) -> String {
    format!(
        "SELECT 'rows' AS \"{tag}\", \"__r\".* FROM ({select}) AS \"__r\"; \
         SELECT 'count' AS \"{tag}\", \"__c\".* FROM ({count}) AS \"__c\"",
        tag = BATCH_TAG,
    )
}

/// True when the schema contains a column named like the batch tag, in
/// which case `build_rows_batch` cannot stamp rows unambiguously.
pub fn batch_tag_collides(schema: &TableSchema) -> bool {
    schema.columns.iter().any(|c| c.name == BATCH_TAG)
}

// ---------------------------------------------------------------------------
// celld plumbing
// ---------------------------------------------------------------------------

/// What a D1 operation needs beyond the request. Built from `AppState` in the
/// handlers; built by hand in tests with a fake `celld`.
#[derive(Clone)]
pub struct D1Ctx {
    pub celld_bin: String,
    pub storage: FleetStorage,
    pub pool: SqlitePool,
    pub store: Store,
    /// Wakes the fleet before `d1 execute` (D1 goes through a running node).
    /// `None` in tests with a fake `celld`, where no fleet can be woken.
    pub wake: Option<FleetManager>,
}

impl D1Ctx {
    pub fn from_state(state: &crate::http::AppState) -> Self {
        Self {
            celld_bin: state.mcp.fleets.celld_bin(),
            storage: state.mcp.fleets.storage().clone(),
            pool: state.mcp.pool.clone(),
            store: state.mcp.store.clone(),
            wake: Some(state.mcp.fleets.clone()),
        }
    }
}

/// A D1 failure, typed so handlers map it to the right status code and the
/// UI's `user_message` without matching on strings.
#[derive(Debug, thiserror::Error)]
pub enum D1Error {
    #[error("unknown database {0:?}")]
    UnknownDatabase(String),
    #[error("this app has no deployed database config yet")]
    NoConfig,
    #[error("unknown table {0:?}")]
    UnknownTable(String),
    #[error("unknown backup {0:?}")]
    UnknownBackup(String),
    #[error("{0}")]
    BadQuery(String),
    #[error("the app is still starting")]
    AppStarting,
    #[error("celld d1 failed: {0}")]
    Celld(String),
    #[error("celld d1 timed out")]
    Timeout,
    #[error("database is unavailable right now")]
    Unavailable(#[source] anyhow::Error),
    #[error("backup too large: {0}")]
    TooLarge(String),
}

impl D1Error {
    fn bad(query: impl Into<String>) -> Self {
        Self::BadQuery(query.into())
    }
}

fn bad_err(err: anyhow::Error) -> D1Error {
    D1Error::BadQuery(format!("{err:#}"))
}

/// The live manifest's declared D1 databases, from `raw_metadata.bindings`:
/// `type: "d1"` entries map `name` → binding and
/// `database_name`/`database_id` → identity. Other binding types are ignored.
/// `vars` — which may carry injected secret values — is never read here and
/// never lands in the synthesized cwd config, so secrets can't leak through
/// the materialized project dir or the fake-celld-style logs that read it.
fn manifest_dbs(manifest: &serde_json::Value) -> anyhow::Result<Vec<D1Database>> {
    let Some(bindings) = manifest.pointer("/raw_metadata/bindings") else {
        return Ok(Vec::new());
    };
    let bindings = bindings.as_array().ok_or_else(|| {
        anyhow::anyhow!("deployed manifest raw_metadata.bindings must be an array")
    })?;
    let mut dbs = Vec::new();
    for (i, entry) in bindings.iter().enumerate() {
        let obj = entry
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("deployed manifest bindings[{i}] must be an object"))?;
        let kind = obj.get("type").and_then(|v| v.as_str()).ok_or_else(|| {
            anyhow::anyhow!("deployed manifest bindings[{i}] needs a string type")
        })?;
        if kind != "d1" {
            continue;
        }
        let binding = obj.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            anyhow::anyhow!("deployed manifest bindings[{i}] needs a string name")
        })?;
        let database_name = obj
            .get("database_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("deployed manifest bindings[{i}] needs a string database_name")
            })?;
        let database_id = match obj.get("database_id") {
            None => None,
            Some(v) => {
                let id = v.as_str().ok_or_else(|| {
                    anyhow::anyhow!("deployed manifest bindings[{i}] needs a string database_id")
                })?;
                migrate::validate_database_id(id)
                    .map_err(|e| anyhow::anyhow!("deployed manifest bindings[{i}]: {e:#}"))?;
                Some(id.to_string())
            }
        };
        migrate::validate_database_name(database_name)
            .map_err(|e| anyhow::anyhow!("deployed manifest bindings[{i}]: {e:#}"))?;
        dbs.push(D1Database {
            binding: binding.to_string(),
            database_name: database_name.to_string(),
            database_id,
            migrations_dir: None,
        });
    }
    Ok(dbs)
}

/// The minimal wrangler.json synthesized for the `celld d1 execute` cwd: only
/// name (script_name), main, compatibility_date/flags, and `d1_databases`
/// [{binding, database_name, database_id?}]. No vars, nothing else — celld
/// resolves the database identity from the cwd config, so it needs the
/// declarations but must never see secret values.
fn synthesize_config(manifest: &serde_json::Value, dbs: &[D1Database]) -> anyhow::Result<Vec<u8>> {
    let script_name = manifest
        .get("script_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("deployed manifest has no script_name"))?;
    let main = manifest
        .get("main_module")
        .and_then(|v| v.as_str())
        .unwrap_or("index.js");
    let mut config = serde_json::Map::new();
    config.insert(
        "name".to_string(),
        serde_json::Value::String(script_name.to_string()),
    );
    config.insert(
        "main".to_string(),
        serde_json::Value::String(main.to_string()),
    );
    if let Some(date) = manifest.pointer("/raw_metadata/compatibility_date") {
        let date = date.as_str().ok_or_else(|| {
            anyhow::anyhow!("deployed manifest compatibility_date must be a string")
        })?;
        config.insert(
            "compatibility_date".to_string(),
            serde_json::Value::String(date.to_string()),
        );
    }
    if let Some(flags) = manifest.pointer("/raw_metadata/compatibility_flags") {
        let flags = flags.as_array().ok_or_else(|| {
            anyhow::anyhow!("deployed manifest compatibility_flags must be an array")
        })?;
        for flag in flags {
            flag.as_str().ok_or_else(|| {
                anyhow::anyhow!("deployed manifest compatibility_flags must be strings")
            })?;
        }
        config.insert(
            "compatibility_flags".to_string(),
            serde_json::Value::Array(flags.clone()),
        );
    }
    let dbs_json: Vec<serde_json::Value> = dbs
        .iter()
        .map(|db| {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "binding".to_string(),
                serde_json::Value::String(db.binding.clone()),
            );
            entry.insert(
                "database_name".to_string(),
                serde_json::Value::String(db.database_name.clone()),
            );
            if let Some(id) = &db.database_id {
                entry.insert(
                    "database_id".to_string(),
                    serde_json::Value::String(id.clone()),
                );
            }
            serde_json::Value::Object(entry)
        })
        .collect();
    config.insert(
        "d1_databases".to_string(),
        serde_json::Value::Array(dbs_json),
    );
    Ok(serde_json::to_vec(&serde_json::Value::Object(config))?)
}

/// Declared databases plus the synthesized cwd config, read from the live
/// manifest via the scoped store. A missing pointer or manifest means
/// never-deployed (`NoConfig`: the existing "not deployed yet" answer);
/// malformed JSON fails `Unavailable`.
struct DeployedState {
    dbs: Vec<D1Database>,
    config: Vec<u8>,
}

async fn deployed_state(ctx: &D1Ctx, slug: &str) -> Result<DeployedState, D1Error> {
    let manifest = super::deployed_manifest(&ctx.store, slug)
        .await
        .map_err(|e| D1Error::Unavailable(anyhow::anyhow!("reading deployed manifest: {e:#}")))?;
    let Some(manifest) = manifest else {
        return Err(D1Error::NoConfig);
    };
    let dbs = manifest_dbs(&manifest)
        .map_err(|e| D1Error::Unavailable(anyhow::anyhow!("parsing d1 bindings: {e:#}")))?;
    let config = synthesize_config(&manifest, &dbs).map_err(|e| {
        D1Error::Unavailable(anyhow::anyhow!("synthesizing wrangler config: {e:#}"))
    })?;
    Ok(DeployedState { dbs, config })
}

/// The argv name celld gets for `binding`: always the `database_name`, exactly
/// as `deploy::migrate` passes it. celld resolves the real identity
/// (`database_id` when declared) from the synthesized cwd config. Only the
/// name reaches argv because only the name is validated against option
/// injection (`migrate::validate_database_name` refuses a leading `-`); a
/// `database_id` is app-authored and merely length-checked, and celld 0.6.2
/// honors no `--` separator.
fn identity_for(dbs: &[D1Database], binding: &str) -> Result<String, D1Error> {
    dbs.iter()
        .find(|db| db.binding == binding)
        .map(|db| db.database_name.clone())
        .ok_or_else(|| D1Error::UnknownDatabase(binding.to_string()))
}

/// Materialize the synthesized wrangler config into a temp project dir: celld
/// resolves the database identity from the cwd config. The dir holds only
/// the config — no app code — and is removed with the `TempDir` guard.
async fn materialize_project(config: &[u8]) -> anyhow::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    tokio::fs::write(dir.path().join("wrangler.json"), config).await?;
    Ok(dir)
}

/// One `celld d1 execute` against the app's fleet. Callers pass validated
/// SQL only — never raw browser input. The env carries the scoped grant
/// alone (`env_clear` + minimal PATH, so no parent-process AWS keys leak),
/// and the timeout kills the child on expiry.
pub(crate) async fn d1_exec(
    ctx: &D1Ctx,
    slug: &str,
    identity: &str,
    sql: &str,
    json: bool,
    project: &FsPath,
) -> Result<String, D1Error> {
    let prefix = FleetStorage::fleet_prefix(slug);
    let creds = ctx
        .storage
        .credentials
        .for_prefix(&prefix)
        .await
        .map_err(|e| D1Error::Unavailable(anyhow::anyhow!("minting scoped credentials: {e:#}")))?;
    let bucket = ctx.storage.bucket_url(slug);
    let mut cmd = tokio::process::Command::new(&ctx.celld_bin);
    cmd.arg("d1")
        .arg("execute")
        .arg(identity)
        .arg("--command")
        .arg(sql);
    if json {
        cmd.arg("--json");
    }
    cmd.arg("--bucket")
        .arg(&bucket)
        .arg("--endpoint")
        .arg(&ctx.storage.endpoint)
        .arg("--region")
        .arg(&ctx.storage.region)
        .current_dir(project)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("AWS_ACCESS_KEY_ID", creds.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", creds.secret_access_key)
        .env("AWS_REGION", &ctx.storage.region)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("S3_ENDPOINT", &ctx.storage.endpoint)
        // `spawn` inherits stdio by default: pipe both explicitly, or
        // `wait_with_output` returns empty output while the child writes to
        // the server's own stdout.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(token) = &creds.session_token {
        cmd.env("AWS_SESSION_TOKEN", token);
    }
    let child = cmd
        .spawn()
        .map_err(|e| D1Error::Unavailable(anyhow::anyhow!("spawning celld: {e:#}")))?;
    let out = tokio::time::timeout(D1_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| D1Error::Timeout)?
        .map_err(|e| D1Error::Unavailable(anyhow::anyhow!("waiting for celld: {e:#}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(D1Error::Celld(first_lines(&stderr, 5)));
    }
    String::from_utf8(out.stdout)
        .map_err(|e| D1Error::Celld(format!("celld printed non-UTF-8 output: {e}")))
}

/// Last `n` non-empty lines of celld stderr, bounded for the log line.
fn first_lines(stderr: &str, n: usize) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(n)..];
    let joined = tail.join(" ");
    const MAX: usize = 500;
    if joined.len() > MAX {
        format!("{}…", &joined[..MAX])
    } else if joined.is_empty() {
        "celld exited non-zero with no output".to_string()
    } else {
        joined
    }
}

/// Parse `--json` output: one JSON object per line (informational lines go to
/// stderr, but be tolerant and skip anything that isn't an object).
pub(crate) fn parse_json_rows(out: &str) -> Vec<serde_json::Map<String, serde_json::Value>> {
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| match value {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        })
        .collect()
}

/// Split one batched exec's concatenated rows back into per-statement sets,
/// keyed by the `BATCH_TAG` stamp each statement puts on its rows (the tag
/// itself is stripped). Rows without a tag — e.g. a future celld summary
/// object — land under the empty key instead of being silently dropped.
pub fn split_batched(
    rows: Vec<serde_json::Map<String, serde_json::Value>>,
) -> BTreeMap<String, Vec<serde_json::Map<String, serde_json::Value>>> {
    let mut sets: BTreeMap<String, Vec<serde_json::Map<String, serde_json::Value>>> =
        BTreeMap::new();
    for mut row in rows {
        let tag = row
            .remove(BATCH_TAG)
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        sets.entry(tag).or_default().push(row);
    }
    sets
}

fn json_i64(row: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<i64> {
    row.get(key).and_then(|v| v.as_i64())
}

fn json_str<'a>(row: &'a serde_json::Map<String, serde_json::Value>, key: &str) -> Option<&'a str> {
    row.get(key).and_then(|v| v.as_str())
}

/// One cell as the UI sees it: NULL → null, numbers stringified exactly,
/// blobs as `x'hex'`. Shared with the backup dump, which renders the same
/// strings through [`literal`].
pub(crate) fn cell_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
        serde_json::Value::Array(items) => {
            let bytes: Vec<u8> = items
                .iter()
                .filter_map(|item| item.as_u64().map(|n| n as u8))
                .collect();
            if bytes.len() == items.len() {
                Some(format!("x'{}'", hex(&bytes)))
            } else {
                Some(serde_json::Value::Array(items.clone()).to_string())
            }
        }
        other => Some(other.to_string()),
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// Operations (shared by the handlers and, in tests, the fake celld)
// ---------------------------------------------------------------------------

/// Wake the fleet when the server runs one (D1 goes through a live node),
/// then hand back the manifest-derived state for the project dir.
async fn ready_config(ctx: &D1Ctx, app: &crate::apps::App) -> Result<DeployedState, D1Error> {
    if let Some(wake) = &ctx.wake {
        wake.ensure_awake(&ctx.pool, app)
            .await
            .map_err(|_| D1Error::AppStarting)?;
    }
    deployed_state(ctx, &app.slug).await
}

/// The backup engine's entry point ([`crate::d1_backup`]): wake the fleet,
/// resolve `binding` to its celld identity, and materialize the synthesized
/// config dir celld resolves that identity from. The caller runs its own
/// validated SQL through [`d1_exec`] against the returned dir.
pub(crate) async fn ready_backup(
    ctx: &D1Ctx,
    app: &crate::apps::App,
    binding: &str,
) -> Result<(String, tempfile::TempDir), D1Error> {
    let state = ready_config(ctx, app).await?;
    let identity = identity_for(&state.dbs, binding)?;
    let project = materialize_project(&state.config)
        .await
        .map_err(D1Error::Unavailable)?;
    Ok((identity, project))
}

/// Every declared D1 binding for a slug, from the live manifest. A missing
/// pointer or manifest means never-deployed (no bindings, not an error),
/// so the backup sweep skips apps that never deployed.
pub(crate) async fn database_bindings(store: &Store, slug: &str) -> anyhow::Result<Vec<String>> {
    let Some(manifest) = super::deployed_manifest(store, slug).await? else {
        return Ok(Vec::new());
    };
    Ok(manifest_dbs(&manifest)?
        .into_iter()
        .map(|db| db.binding)
        .collect())
}

/// Tables plus row counts, in one `SELECT … UNION ALL …` exec.
pub async fn list_tables(
    ctx: &D1Ctx,
    app: &crate::apps::App,
    binding: &str,
) -> Result<(String, Vec<D1TableInfo>), D1Error> {
    let state = ready_config(ctx, app).await?;
    let identity = identity_for(&state.dbs, binding)?;
    let project = materialize_project(&state.config)
        .await
        .map_err(D1Error::Unavailable)?;
    let out = d1_exec(
        ctx,
        &app.slug,
        &identity,
        "PRAGMA table_list",
        true,
        project.path(),
    )
    .await?;
    let mut names: Vec<String> = Vec::new();
    for row in parse_json_rows(&out) {
        if json_str(&row, "type") != Some("table") {
            continue;
        }
        let Some(name) = json_str(&row, "name") else {
            continue;
        };
        if !visible_table(name) {
            continue;
        }
        names.push(name.to_string());
    }
    names.sort();
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    if !names.is_empty() {
        let out = d1_exec(
            ctx,
            &app.slug,
            &identity,
            &build_table_counts(&names),
            true,
            project.path(),
        )
        .await?;
        for row in parse_json_rows(&out) {
            if let (Some(name), Some(n)) = (json_str(&row, "t"), json_i64(&row, "n")) {
                counts.insert(name.to_string(), n);
            }
        }
    }
    let tables = names
        .into_iter()
        .map(|name| D1TableInfo {
            row_count: counts.get(&name).copied().unwrap_or(0),
            name,
        })
        .collect();
    Ok((identity, tables))
}

/// Columns from `PRAGMA table_info` (empty = unknown table).
async fn load_columns(
    ctx: &D1Ctx,
    slug: &str,
    identity: &str,
    project: &FsPath,
    table: &str,
) -> Result<Vec<D1Column>, D1Error> {
    let out = d1_exec(
        ctx,
        slug,
        identity,
        &format!("PRAGMA table_info({})", quote_ident(table)),
        true,
        project,
    )
    .await?;
    let mut columns = Vec::new();
    for row in parse_json_rows(&out) {
        let Some(name) = json_str(&row, "name") else {
            continue;
        };
        columns.push(D1Column {
            name: name.to_string(),
            col_type: json_str(&row, "type").unwrap_or("").to_string(),
            not_null: json_i64(&row, "notnull").unwrap_or(0) != 0,
            default_value: row.get("dflt_value").and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            }),
            pk: json_i64(&row, "pk").unwrap_or(0),
        });
    }
    Ok(columns)
}

/// `table_info` only — enough to validate and build a read/write.
async fn load_table(
    ctx: &D1Ctx,
    slug: &str,
    identity: &str,
    project: &FsPath,
    table: &str,
) -> Result<TableSchema, D1Error> {
    let columns = load_columns(ctx, slug, identity, project, table).await?;
    if columns.is_empty() {
        return Err(D1Error::UnknownTable(table.to_string()));
    }
    Ok(TableSchema {
        name: table.to_string(),
        columns,
    })
}

/// Full schema for the Definition view: columns, foreign keys, indexes, DDL.
/// Two `celld d1 execute` calls at most: one for table_info +
/// foreign_key_list + index_list + the sqlite_master DDL, and — only when
/// the table has indexes — one for every index's columns.
pub async fn table_schema(
    ctx: &D1Ctx,
    app: &crate::apps::App,
    binding: &str,
    table: &str,
) -> Result<D1TableSchema, D1Error> {
    if !visible_table(table) {
        return Err(D1Error::UnknownTable(table.to_string()));
    }
    let state = ready_config(ctx, app).await?;
    let identity = identity_for(&state.dbs, binding)?;
    let project = materialize_project(&state.config)
        .await
        .map_err(D1Error::Unavailable)?;
    let schema_out = d1_exec(
        ctx,
        &app.slug,
        &identity,
        &build_schema_batch(table),
        true,
        project.path(),
    )
    .await?;
    let sets = split_batched(parse_json_rows(&schema_out));
    let empty = Vec::new();
    let ti = sets.get("ti").unwrap_or(&empty);
    let mut columns = Vec::new();
    for row in ti {
        let Some(name) = json_str(row, "name") else {
            continue;
        };
        columns.push(D1Column {
            name: name.to_string(),
            col_type: json_str(row, "type").unwrap_or("").to_string(),
            not_null: json_i64(row, "notnull").unwrap_or(0) != 0,
            default_value: row.get("dflt_value").and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            }),
            pk: json_i64(row, "pk").unwrap_or(0),
        });
    }
    if columns.is_empty() {
        return Err(D1Error::UnknownTable(table.to_string()));
    }
    let mut foreign_keys = Vec::new();
    for row in sets.get("fk").unwrap_or(&empty) {
        let (Some(from), Some(ftable), Some(to)) = (
            json_str(row, "from"),
            json_str(row, "table"),
            json_str(row, "to"),
        ) else {
            continue;
        };
        foreign_keys.push(D1ForeignKey {
            from: from.to_string(),
            table: ftable.to_string(),
            to: to.to_string(),
        });
    }
    let mut indexes = Vec::new();
    let mut index_names: Vec<String> = Vec::new();
    for row in sets.get("il").unwrap_or(&empty) {
        let Some(name) = json_str(row, "name") else {
            continue;
        };
        index_names.push(name.to_string());
        indexes.push(D1Index {
            name: name.to_string(),
            unique: json_i64(row, "unique").unwrap_or(0) != 0,
            columns: Vec::new(),
        });
    }
    // Index columns need one lookup per index name, so they ride a second
    // batched exec — skipped entirely when the table has no indexes.
    if let Some(batch) = build_index_info_batch(&index_names) {
        let info_out = d1_exec(ctx, &app.slug, &identity, &batch, true, project.path()).await?;
        let info_sets = split_batched(parse_json_rows(&info_out));
        let mut by_index: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
        for row in info_sets.get("ii").unwrap_or(&empty) {
            let (Some(idx), Some(col)) = (json_str(row, "_idx"), json_str(row, "col")) else {
                continue;
            };
            by_index
                .entry(idx.to_string())
                .or_default()
                .push((json_i64(row, "seqno").unwrap_or(0), col.to_string()));
        }
        for index in &mut indexes {
            if let Some(cols) = by_index.remove(&index.name) {
                let mut cols = cols;
                cols.sort_by_key(|(seqno, _)| *seqno);
                index.columns = cols.into_iter().map(|(_, col)| col).collect();
            }
        }
    }
    let sql = sets
        .get("ddl")
        .unwrap_or(&empty)
        .iter()
        .find_map(|row| json_str(row, "sql").map(str::to_string));
    Ok(D1TableSchema {
        binding: binding.to_string(),
        table: table.to_string(),
        columns,
        foreign_keys,
        indexes,
        sql,
    })
}

/// One server-side page of rows plus the total under the same filters/search.
/// Two `celld d1 execute` calls: columns first (validation needs them), then
/// the page and the total in one batched exec.
pub async fn table_rows(
    ctx: &D1Ctx,
    app: &crate::apps::App,
    binding: &str,
    table: &str,
    body: &RowsBody,
) -> Result<RowsResponse, D1Error> {
    if !visible_table(table) {
        return Err(D1Error::UnknownTable(table.to_string()));
    }
    if body.search.len() > 1024 {
        return Err(D1Error::bad("search is too long"));
    }
    for filter in &body.filters {
        if filter.column.len() > 128 || filter.value.len() > 4096 {
            return Err(D1Error::bad("filter is too long"));
        }
    }
    let state = ready_config(ctx, app).await?;
    let identity = identity_for(&state.dbs, binding)?;
    let project = materialize_project(&state.config)
        .await
        .map_err(D1Error::Unavailable)?;
    let schema = load_table(ctx, &app.slug, &identity, project.path(), table).await?;
    let page_size = body.page_size.clamp(MIN_PAGE_SIZE, MAX_PAGE_SIZE);
    let page = body.page.max(0);
    let select = build_select(
        &schema,
        &body.filters,
        &body.search,
        body.sort.as_ref(),
        page,
        page_size,
    )
    .map_err(bad_err)?;
    let count = build_count(&schema, &body.filters, &body.search).map_err(bad_err)?;
    // One exec for page + total, split back apart by tag — except when a
    // user column is itself named `_q`, where the tag would collide.
    let (row_sets, count_sets) = if batch_tag_collides(&schema) {
        let rows_out = d1_exec(ctx, &app.slug, &identity, &select, true, project.path()).await?;
        let count_out = d1_exec(ctx, &app.slug, &identity, &count, true, project.path()).await?;
        (parse_json_rows(&rows_out), parse_json_rows(&count_out))
    } else {
        let batch = build_rows_batch(&select, &count);
        let out = d1_exec(ctx, &app.slug, &identity, &batch, true, project.path()).await?;
        let sets = split_batched(parse_json_rows(&out));
        let empty = Vec::new();
        (
            sets.get("rows").unwrap_or(&empty).clone(),
            sets.get("count").unwrap_or(&empty).clone(),
        )
    };
    let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    for row in &row_sets {
        rows.push(
            columns
                .iter()
                .map(|col| cell_value(row.get(col).unwrap_or(&serde_json::Value::Null)))
                .collect(),
        );
    }
    let total = count_sets
        .iter()
        .find_map(|row| json_i64(row, "n"))
        .unwrap_or(0);
    Ok(RowsResponse {
        table: table.to_string(),
        columns,
        rows,
        total,
        page,
        page_size,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct RowsResponse {
    pub table: String,
    /// Column order of `rows`.
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    /// COUNT(*) under the same filters/search.
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
}

/// Curated single-statement write (insert/update/delete).
pub async fn write_row(
    ctx: &D1Ctx,
    app: &crate::apps::App,
    binding: &str,
    table: &str,
    body: &WriteBody,
) -> Result<(), D1Error> {
    if !visible_table(table) {
        return Err(D1Error::UnknownTable(table.to_string()));
    }
    if body.values.len() > 100 {
        return Err(D1Error::bad("too many values"));
    }
    for (name, value) in body.values.iter().chain(body.key.iter()) {
        if name.len() > 128 || value.as_ref().is_some_and(|v| v.len() > 1_048_576) {
            return Err(D1Error::bad("value is too long"));
        }
    }
    let state = ready_config(ctx, app).await?;
    let identity = identity_for(&state.dbs, binding)?;
    let project = materialize_project(&state.config)
        .await
        .map_err(D1Error::Unavailable)?;
    let schema = load_table(ctx, &app.slug, &identity, project.path(), table).await?;
    let sql = match body.op {
        D1WriteOp::Insert => build_insert(&schema, &body.values).map_err(bad_err)?,
        D1WriteOp::Update => build_update(&schema, &body.values, &body.key).map_err(bad_err)?,
        D1WriteOp::Delete => build_delete(&schema, &body.key).map_err(bad_err)?,
    };
    d1_exec(ctx, &app.slug, &identity, &sql, false, project.path()).await?;
    Ok(())
}

/// Delete 1..100 rows by key. One exec = one transaction: all-or-nothing.
pub async fn delete_rows(
    ctx: &D1Ctx,
    app: &crate::apps::App,
    binding: &str,
    table: &str,
    keys: &[BTreeMap<String, Option<String>>],
) -> Result<u64, D1Error> {
    if !visible_table(table) {
        return Err(D1Error::UnknownTable(table.to_string()));
    }
    if keys.is_empty() || keys.len() > MAX_DELETE_KEYS {
        return Err(D1Error::bad(format!("send 1..={MAX_DELETE_KEYS} keys")));
    }
    let state = ready_config(ctx, app).await?;
    let identity = identity_for(&state.dbs, binding)?;
    let project = materialize_project(&state.config)
        .await
        .map_err(D1Error::Unavailable)?;
    let schema = load_table(ctx, &app.slug, &identity, project.path(), table).await?;
    let sql = build_delete_keys(&schema, keys).map_err(bad_err)?;
    let out = d1_exec(ctx, &app.slug, &identity, &sql, true, project.path()).await?;
    let deleted: i64 = parse_json_rows(&out)
        .iter()
        .filter_map(|row| json_i64(row, "n"))
        .sum();
    Ok(deleted.max(0) as u64)
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

// Console D1 writes audit through `mcp::actions::record_console` (both ids
// NULL, which the action log labels client "Console"): one audit path for
// every console mutation. Summaries never carry values — only the op, table
// and key count.

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub(crate) fn d1_response(result: Result<serde_json::Value, D1Error>) -> Response {
    use crate::http::console_data::{data_error, data_json};
    match result {
        Ok(value) => data_json(StatusCode::OK, value),
        Err(D1Error::UnknownDatabase(binding)) => data_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("unknown database {binding:?}"),
            Some(format!(
                "Unknown database {binding:?}: the app doesn't declare that D1 binding."
            )),
        ),
        Err(D1Error::NoConfig) => data_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "no deployed database config",
            Some(
                "This app has no D1 databases yet: add a d1_databases binding and deploy first. See the D1 guide in Docs.".to_string(),
            ),
        ),
        Err(D1Error::UnknownTable(table)) => data_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("unknown table {table:?}"),
            Some(format!("Unknown table {table:?}.")),
        ),
        Err(D1Error::UnknownBackup(id)) => data_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("unknown backup {id:?}"),
            Some("Unknown backup: it may have been pruned past retention.".to_string()),
        ),
        Err(D1Error::BadQuery(message)) => data_error(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            message.clone(),
            Some(message),
        ),
        Err(D1Error::AppStarting) => data_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "app_starting",
            "the app is still starting",
            Some("Starting the app — retry in a few seconds.".to_string()),
        ),
        Err(D1Error::Celld(detail)) => data_error(
            StatusCode::BAD_GATEWAY,
            "d1_unavailable",
            format!("celld d1 failed: {detail}"),
            Some("Couldn't reach the app's database — the app may still be starting. Retry in a few seconds.".to_string()),
        ),
        Err(D1Error::Timeout) => data_error(
            StatusCode::GATEWAY_TIMEOUT,
            "d1_timeout",
            "celld d1 timed out",
            Some("The database query took too long. Narrow the filters and retry.".to_string()),
        ),
        Err(D1Error::Unavailable(source)) => data_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            format!("{source:#}"),
            Some("Couldn't load the database right now.".to_string()),
        ),
        Err(D1Error::TooLarge(detail)) => data_error(
            StatusCode::INSUFFICIENT_STORAGE,
            "backup_too_large",
            format!("backup too large: {detail}"),
            Some("This database is too large to back up right now.".to_string()),
        ),
    }
}

pub fn routes() -> Router<crate::http::AppState> {
    Router::new()
        .route(
            "/api/console/apps/{slug}/data/d1/databases",
            get(api_databases),
        )
        .route(
            "/api/console/apps/{slug}/data/d1/{binding}/tables",
            get(api_tables),
        )
        .route(
            "/api/console/apps/{slug}/data/d1/{binding}/tables/{table}/schema",
            get(api_schema),
        )
        .route(
            "/api/console/apps/{slug}/data/d1/{binding}/tables/{table}/rows",
            post(api_rows),
        )
        .route(
            "/api/console/apps/{slug}/data/d1/{binding}/tables/{table}/write",
            post(api_write),
        )
        .route(
            "/api/console/apps/{slug}/data/d1/{binding}/tables/{table}/delete-rows",
            post(api_delete_rows),
        )
}

async fn api_databases(
    State(state): State<crate::http::AppState>,
    headers: axum::http::HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let app = match owner_app(mcp, &account, &slug, "this database").await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    let ctx = D1Ctx::from_state(&state);
    let result = async {
        let state = deployed_state(&ctx, &app.slug).await?;
        Ok::<_, D1Error>(json!({
            "databases": state.dbs.iter().map(|db| json!({
                "binding": db.binding,
                "database_name": db.database_name,
            })).collect::<Vec<_>>(),
        }))
    }
    .await;
    d1_response(result)
}

async fn api_tables(
    State(state): State<crate::http::AppState>,
    headers: axum::http::HeaderMap,
    Path((slug, binding)): Path<(String, String)>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let app = match owner_app(mcp, &account, &slug, "this database").await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    if binding.len() > 128 {
        return d1_response(Err(D1Error::UnknownDatabase(binding)));
    }
    let ctx = D1Ctx::from_state(&state);
    let result = async {
        let (identity, tables) = list_tables(&ctx, &app, &binding).await?;
        Ok::<_, D1Error>(json!({
            "binding": binding,
            "database": identity,
            "tables": tables,
        }))
    }
    .await;
    d1_response(result)
}

async fn api_schema(
    State(state): State<crate::http::AppState>,
    headers: axum::http::HeaderMap,
    Path((slug, binding, table)): Path<(String, String, String)>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let app = match owner_app(mcp, &account, &slug, "this database").await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    let ctx = D1Ctx::from_state(&state);
    let result = async {
        let schema = table_schema(&ctx, &app, &binding, &table).await?;
        Ok::<_, D1Error>(json!({
            "binding": schema.binding,
            "table": schema.table,
            "columns": schema.columns,
            "foreign_keys": schema.foreign_keys,
            "indexes": schema.indexes,
            "sql": schema.sql,
        }))
    }
    .await;
    d1_response(result)
}

async fn api_rows(
    State(state): State<crate::http::AppState>,
    headers: axum::http::HeaderMap,
    Path((slug, binding, table)): Path<(String, String, String)>,
    Json(body): Json<RowsBody>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let app = match owner_app(mcp, &account, &slug, "this database").await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    let ctx = D1Ctx::from_state(&state);
    let result = async {
        let rows = table_rows(&ctx, &app, &binding, &table, &body).await?;
        Ok::<_, D1Error>(json!({
            "table": rows.table,
            "columns": rows.columns,
            "rows": rows.rows,
            "total": rows.total,
            "page": rows.page,
            "page_size": rows.page_size,
        }))
    }
    .await;
    d1_response(result)
}

async fn api_write(
    State(state): State<crate::http::AppState>,
    headers: axum::http::HeaderMap,
    Path((slug, binding, table)): Path<(String, String, String)>,
    Json(body): Json<WriteBody>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let app = match owner_app(mcp, &account, &slug, "this database").await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    // Frozen apps stay writable only for deletes (the escape hatch back
    // under the cap): inserts and updates are refused at the storage cap.
    if !matches!(body.op, D1WriteOp::Delete) {
        if let Some(res) = super::storage_frozen_refusal(mcp, &app).await {
            return res;
        }
    }
    let ctx = D1Ctx::from_state(&state);
    let outcome = write_row(&ctx, &app, &binding, &table, &body).await;
    let op = match body.op {
        D1WriteOp::Insert => "insert",
        D1WriteOp::Update => "update",
        D1WriteOp::Delete => "delete",
    };
    let (summary, result) = match &outcome {
        Ok(()) => (
            match body.op {
                D1WriteOp::Insert => format!("inserted a row into {table}"),
                D1WriteOp::Update => format!("updated a row in {table}"),
                D1WriteOp::Delete => format!("deleted a row from {table}"),
            },
            Ok(json!({"ok": true})),
        ),
        Err(D1Error::BadQuery(message)) => (
            format!("refused d1 {op} on {table}"),
            Err(D1Error::BadQuery(message.clone())),
        ),
        Err(D1Error::UnknownTable(_)) => (
            format!("refused d1 {op} on {table}"),
            Err(D1Error::UnknownTable(table.clone())),
        ),
        Err(D1Error::UnknownDatabase(_)) => (
            format!("refused d1 {op} on {table}"),
            Err(D1Error::UnknownDatabase(binding.clone())),
        ),
        Err(err) => (
            format!("failed d1 {op} on {table}"),
            Err(D1Error::Celld(format!("{err:#}"))),
        ),
    };
    // The audit row never carries values: only the op and the table.
    let (o, code) = match &outcome {
        Ok(()) => ("success", None),
        Err(D1Error::BadQuery(_))
        | Err(D1Error::UnknownTable(_))
        | Err(D1Error::UnknownDatabase(_)) => ("error", Some("invalid_argument")),
        Err(_) => ("error", Some("d1_unavailable")),
    };
    crate::mcp::actions::record_console(
        &ctx.pool,
        &app.id,
        &account.id,
        "d1_write",
        &summary,
        o,
        code,
        0,
    )
    .await;
    d1_response(result)
}

async fn api_delete_rows(
    State(state): State<crate::http::AppState>,
    headers: axum::http::HeaderMap,
    Path((slug, binding, table)): Path<(String, String, String)>,
    Json(body): Json<DeleteRowsBody>,
) -> Response {
    let mcp = &state.mcp;
    let account = match guard(&mcp.auth, &headers).await {
        Ok(account) => account,
        Err(res) => return *res,
    };
    let app = match owner_app(mcp, &account, &slug, "this database").await {
        Ok(app) => app,
        Err(res) => return *res,
    };
    let ctx = D1Ctx::from_state(&state);
    let keys = body.keys.len();
    let outcome = delete_rows(&ctx, &app, &binding, &table, &body.keys).await;
    let (summary, result) = match &outcome {
        Ok(deleted) => (
            format!(
                "deleted {deleted} row{} from {table}",
                if *deleted == 1 { "" } else { "s" }
            ),
            Ok(json!({"deleted": deleted})),
        ),
        Err(D1Error::BadQuery(_)) => (
            format!("refused d1 bulk delete on {table} ({keys} keys)"),
            Err(D1Error::bad(format!("send 1..={MAX_DELETE_KEYS} keys"))),
        ),
        Err(D1Error::UnknownTable(_)) => (
            format!("refused d1 bulk delete on {table}"),
            Err(D1Error::UnknownTable(table.clone())),
        ),
        Err(D1Error::UnknownDatabase(_)) => (
            format!("refused d1 bulk delete on {table}"),
            Err(D1Error::UnknownDatabase(binding.clone())),
        ),
        Err(err) => (
            format!("failed d1 bulk delete on {table}"),
            Err(D1Error::Celld(format!("{err:#}"))),
        ),
    };
    let (o, code) = match &outcome {
        Ok(_) => ("success", None),
        Err(D1Error::BadQuery(_))
        | Err(D1Error::UnknownTable(_))
        | Err(D1Error::UnknownDatabase(_)) => ("error", Some("invalid_argument")),
        Err(_) => ("error", Some("d1_unavailable")),
    };
    crate::mcp::actions::record_console(
        &ctx.pool,
        &app.id,
        &account.id,
        "d1_delete_rows",
        &summary,
        o,
        code,
        0,
    )
    .await;
    d1_response(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::{test_config, test_pool};
    use crate::auth::AuthState;
    use crate::mcp::McpState;
    use crate::store::snapshot::StateSync;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use tower::ServiceExt;

    // --- pure builder tests (ported vectors) -------------------------------

    fn col(name: &str, decltype: &str, pk: i64) -> D1Column {
        D1Column {
            name: name.to_string(),
            col_type: decltype.to_string(),
            not_null: false,
            default_value: None,
            pk,
        }
    }

    fn table(columns: Vec<D1Column>) -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            columns,
        }
    }

    fn users() -> TableSchema {
        table(vec![
            col("id", "INTEGER", 1),
            col("name", "TEXT", 0),
            col("age", "REAL", 0),
            col("note", "TEXT", 0),
        ])
    }

    fn filter(column: &str, op: D1FilterOp, value: &str) -> D1Filter {
        D1Filter {
            column: column.to_string(),
            op: Some(op),
            value: value.to_string(),
        }
    }

    #[test]
    fn filter_ops_render() {
        let t = users();
        let cases = [
            (D1FilterOp::Eq, "\"id\" = 7"),
            (D1FilterOp::Neq, "\"name\" <> 'bob'"),
            (D1FilterOp::Lt, "\"id\" < 7"),
            (D1FilterOp::Lte, "\"id\" <= 7"),
            (D1FilterOp::Gt, "\"id\" > 7"),
            (D1FilterOp::Gte, "\"id\" >= 7"),
            (D1FilterOp::Like, "\"name\" LIKE '%o%'"),
            (D1FilterOp::IsNull, "\"note\" IS NULL"),
            (D1FilterOp::NotNull, "\"note\" IS NOT NULL"),
        ];
        for (op, expected) in cases {
            let column = if matches!(op, D1FilterOp::IsNull | D1FilterOp::NotNull) {
                "note"
            } else if matches!(op, D1FilterOp::Like | D1FilterOp::Neq) {
                "name"
            } else {
                "id"
            };
            let value = if matches!(op, D1FilterOp::Like) {
                "%o%".to_string()
            } else if column == "name" {
                "bob".to_string()
            } else {
                "7".to_string()
            };
            let f = filter(column, op, &value);
            let sql = build_count(&t, &[f], "").unwrap();
            assert!(sql.ends_with(&format!("WHERE {expected}")), "{sql}");
        }
    }

    #[test]
    fn like_searches_take_pattern_as_typed() {
        let t = users();
        let f = filter("name", D1FilterOp::Like, "50%_\\x");
        let sql = build_select(&t, &[f], "", None, 0, 25).unwrap();
        // The user's LIKE pattern is taken as typed (its wildcards live).
        assert!(sql.contains("WHERE \"name\" LIKE '50%_\\x'"), "{sql}");
    }

    #[test]
    fn search_escapes_needle_and_quotes() {
        let t = users();
        let sql = build_select(&t, &[], "50%_\\'x", None, 0, 25).unwrap();
        // `%`, `_`, `\` become escaped; the quote doubles inside the literal.
        assert!(
            sql.contains("LIKE '%50\\%\\_\\\\''x%' ESCAPE '\\'"),
            "{sql}"
        );
        // Every column participates in the OR.
        for column in ["id", "name", "age", "note"] {
            assert!(
                sql.contains(&format!("CAST(\"{column}\" AS TEXT) LIKE")),
                "{sql}"
            );
        }
    }

    #[test]
    fn unknown_column_and_table_refused() {
        let t = users();
        let f = filter("nope", D1FilterOp::Eq, "1");
        assert!(build_count(&t, &[f], "").is_err());
        let sort = D1Sort {
            column: "nope".to_string(),
            desc: false,
        };
        assert!(build_select(&t, &[], "", Some(&sort), 0, 25).is_err());
        // The key must name real columns too.
        let mut key = BTreeMap::new();
        key.insert("nope".to_string(), Some("1".to_string()));
        assert!(build_delete(&t, &key).is_err());
    }

    #[test]
    fn hidden_tables_rejected() {
        for hidden in ["", "_migrations", "sqlite_sequence", "d1_migrations"] {
            assert!(!visible_table(hidden), "{hidden}");
        }
        assert!(visible_table("users"));
    }

    #[test]
    fn identifiers_quote_by_doubling() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(string_literal("o'clock"), "'o''clock'");
    }

    #[test]
    fn default_order_is_primary_key() {
        let t = table(vec![
            col("b", "TEXT", 2),
            col("a", "TEXT", 1),
            col("v", "TEXT", 0),
        ]);
        let sql = build_select(&t, &[], "", None, 0, 25).unwrap();
        assert!(sql.contains("ORDER BY \"a\" ASC, \"b\" ASC"), "{sql}");
    }

    #[test]
    fn default_order_is_rowid_without_pk() {
        let t = table(vec![col("a", "TEXT", 0), col("b", "TEXT", 0)]);
        let sql = build_select(&t, &[], "", None, 0, 25).unwrap();
        assert!(sql.contains("ORDER BY rowid ASC"), "{sql}");
    }

    #[test]
    fn explicit_sort_and_pagination() {
        let t = users();
        let sort = D1Sort {
            column: "age".to_string(),
            desc: true,
        };
        let sql = build_select(&t, &[], "", Some(&sort), 3, 50).unwrap();
        assert!(sql.contains("ORDER BY \"age\" DESC"), "{sql}");
        assert!(sql.ends_with("LIMIT 50 OFFSET 150"), "{sql}");
        // Pages before zero clamp.
        let sql = build_select(&t, &[], "", None, -4, 25).unwrap();
        assert!(sql.ends_with("LIMIT 25 OFFSET 0"), "{sql}");
    }

    #[test]
    fn too_many_filters_refused() {
        let t = users();
        let filters: Vec<D1Filter> = (0..=MAX_FILTERS)
            .map(|_| filter("id", D1FilterOp::Eq, "1"))
            .collect();
        assert!(build_count(&t, &filters, "").is_err());
    }

    #[test]
    fn null_and_empty_string_are_distinct() {
        let t = users();
        let mut values: BTreeMap<String, Option<String>> = BTreeMap::new();
        values.insert("name".to_string(), None);
        values.insert("note".to_string(), Some(String::new()));
        let sql = build_insert(&t, &values).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"t\" (\"name\", \"note\") VALUES (NULL, '')"
        );
    }

    #[test]
    fn literal_escaping_vectors() {
        // Quotes double inside string literals.
        assert_eq!(literal("TEXT", Some("o'clock")), "'o''clock'");
        // NULL stays NULL; empty stays the empty string.
        assert_eq!(literal("TEXT", None), "NULL");
        assert_eq!(literal("TEXT", Some("")), "''");
        // Numbers stay bare on numeric affinity, preserving REAL precision.
        assert_eq!(literal("INTEGER", Some("7")), "7");
        assert_eq!(
            literal("REAL", Some("36.600000000000001")),
            "36.600000000000001"
        );
        // Non-numeric text in a numeric column is quoted, not bare.
        assert_eq!(literal("INTEGER", Some("7abc")), "'7abc'");
        assert_eq!(literal("TEXT", Some("7")), "'7'");
        // BLOB affinity has no numeric rule: hex text is quoted.
        assert_eq!(literal("BLOB", Some("x'ff'")), "'x''ff'''");
    }

    #[test]
    fn cell_value_vectors() {
        use serde_json::{json, Value};
        assert_eq!(cell_value(&Value::Null), None);
        assert_eq!(cell_value(&json!("ada")), Some("ada".to_string()));
        // Numbers stringify exactly (REAL precision preserved).
        assert_eq!(cell_value(&json!(36.6)), Some("36.6".to_string()));
        assert_eq!(cell_value(&json!(7)), Some("7".to_string()));
        assert_eq!(cell_value(&json!(true)), Some("1".to_string()));
        assert_eq!(cell_value(&json!(false)), Some("0".to_string()));
        // Blobs arrive as byte arrays and render as x'hex'.
        assert_eq!(cell_value(&json!([222, 173])), Some("x'dead'".to_string()));
    }

    #[test]
    fn key_must_match_pk() {
        let t = users();
        // Right key passes.
        let mut key = BTreeMap::new();
        key.insert("id".to_string(), Some("1".to_string()));
        assert!(check_key(&t, &key).is_ok());
        // Wrong columns fail.
        let mut key = BTreeMap::new();
        key.insert("name".to_string(), Some("ada".to_string()));
        assert!(check_key(&t, &key).is_err());
        assert!(build_update(
            &t,
            &BTreeMap::from([("name".to_string(), Some("b".to_string()))]),
            &key
        )
        .is_err());
        // NULL key values render as IS NULL.
        let mut key = BTreeMap::new();
        key.insert("id".to_string(), None);
        let sql = build_delete(&t, &key).unwrap();
        assert_eq!(sql, "DELETE FROM \"t\" WHERE \"id\" IS NULL");
        // A PK-less table needs every column in the key.
        let flat = table(vec![col("a", "TEXT", 0), col("b", "TEXT", 0)]);
        let mut partial = BTreeMap::new();
        partial.insert("a".to_string(), Some("1".to_string()));
        assert!(check_key(&flat, &partial).is_err());
    }

    #[test]
    fn delete_batch_is_capped_and_atomic() {
        let t = users();
        let one = || BTreeMap::from([("id".to_string(), Some("1".to_string()))]);
        let sql = build_delete_keys(&t, &[one()]).unwrap();
        assert!(
            sql.contains("DELETE FROM \"t\" WHERE \"id\" = 1; SELECT changes() AS n"),
            "{sql}"
        );
        assert!(build_delete_keys(&t, &[]).is_err());
        let many: Vec<_> = (0..=MAX_DELETE_KEYS).map(|_| one()).collect();
        assert!(build_delete_keys(&t, &many).is_err());
        let max: Vec<_> = (0..MAX_DELETE_KEYS).map(|_| one()).collect();
        assert!(build_delete_keys(&t, &max).is_ok());
    }

    #[test]
    fn table_counts_batch_shape() {
        let sql = build_table_counts(&["users".to_string(), "orgs\"x".to_string()]);
        assert_eq!(
            sql,
            "SELECT 'users' AS t, COUNT(*) AS n FROM \"users\" UNION ALL \
             SELECT 'orgs\"x' AS t, COUNT(*) AS n FROM \"orgs\"\"x\""
        );
    }

    // --- database allowlist ----------------------------------------------

    fn dbs() -> Vec<D1Database> {
        vec![
            D1Database {
                binding: "DB".to_string(),
                database_name: "app-db".to_string(),
                database_id: Some("abc-123".to_string()),
                migrations_dir: None,
            },
            D1Database {
                binding: "PLAIN".to_string(),
                database_name: "plain-db".to_string(),
                database_id: None,
                migrations_dir: None,
            },
        ]
    }

    #[test]
    fn argv_identity_is_always_the_validated_name() {
        // `DB` declares a database_id; argv still carries the name, never the id.
        let name = dbs()
            .into_iter()
            .find(|d| d.binding == "DB")
            .unwrap()
            .database_name;
        assert_eq!(identity_for(&dbs(), "DB").unwrap(), name);
        assert_ne!(identity_for(&dbs(), "DB").unwrap(), "abc-123");
        assert_eq!(identity_for(&dbs(), "PLAIN").unwrap(), "plain-db");
    }

    #[test]
    fn unknown_binding_rejected() {
        let err = identity_for(&dbs(), "OTHER").unwrap_err();
        assert!(matches!(err, D1Error::UnknownDatabase(_)));
        // Case differs: bindings are exact.
        assert!(identity_for(&dbs(), "db").is_err());
    }

    // --- manifest parsing ----------------------------------------------------

    fn manifest_with(bindings: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 2,
            "script_name": "outer-app",
            "main_module": "index.js",
            "raw_metadata": {
                "bindings": bindings,
                "compatibility_date": "2026-09-01",
                "compatibility_flags": ["nodejs_als"],
                "main_module": "index.js"
            }
        })
    }

    #[test]
    fn manifest_maps_d1_bindings_with_and_without_id() {
        let dbs = manifest_dbs(&manifest_with(serde_json::json!([
            {"type": "d1", "name": "DB", "database_name": "app-db", "database_id": "abc-123"},
            {"type": "d1", "name": "PLAIN", "database_name": "plain-db"}
        ])))
        .unwrap();
        assert_eq!(dbs.len(), 2);
        assert_eq!(dbs[0].binding, "DB");
        assert_eq!(dbs[0].effective_identity(), "abc-123");
        assert_eq!(dbs[1].binding, "PLAIN");
        assert_eq!(dbs[1].database_id, None);
        assert_eq!(dbs[1].effective_identity(), "plain-db");
    }

    #[test]
    fn manifest_ignores_other_binding_types() {
        let dbs = manifest_dbs(&manifest_with(serde_json::json!([
            {"type": "d1", "name": "DB", "database_name": "app-db"},
            {"type": "kv_namespace", "name": "CACHE", "namespace_id": "kv-1"},
            {"type": "assets", "name": "ASSETS"},
            {"type": "plain_text", "name": "KEY", "text": "v"}
        ])))
        .unwrap();
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].binding, "DB");
    }

    #[test]
    fn manifest_without_bindings_is_empty() {
        let bare = serde_json::json!({"schema_version": 2, "script_name": "x"});
        assert!(manifest_dbs(&bare).unwrap().is_empty());
        let dbs = manifest_dbs(&manifest_with(serde_json::json!([]))).unwrap();
        assert!(dbs.is_empty());
    }

    #[test]
    fn manifest_rejects_malformed_d1_entries() {
        for bindings in [
            serde_json::json!([{"type": "d1", "database_name": "app-db"}]),
            serde_json::json!([{"type": "d1", "name": "DB"}]),
            serde_json::json!([{"type": "d1", "name": "DB", "database_name": "-evil"}]),
            serde_json::json!([{"type": "d1", "name": "DB", "database_name": "app-db", "database_id": 7}]),
            serde_json::json!([{"name": "DB", "database_name": "app-db"}]),
            serde_json::json!("d1_databases"),
        ] {
            assert!(manifest_dbs(&manifest_with(bindings)).is_err());
        }
    }

    #[test]
    fn schema_batch_is_one_tagged_exec() {
        let sql = build_schema_batch("users");
        // Four statements in one exec, each stamping its own tag.
        assert_eq!(sql.matches(';').count(), 3, "{sql}");
        for tag in ["'ti'", "'fk'", "'il'", "'ddl'"] {
            assert!(sql.contains(tag), "{sql}");
        }
        assert!(sql.contains("pragma_table_info('users')"), "{sql}");
        assert!(sql.contains("pragma_foreign_key_list('users')"), "{sql}");
        assert!(sql.contains("pragma_index_list('users')"), "{sql}");
        assert!(sql.contains("sqlite_master"), "{sql}");
        // A quote in the table name stays inside its string literal.
        let evil = build_schema_batch("a'b");
        assert!(evil.contains("pragma_table_info('a''b')"), "{evil}");
        assert!(!evil.contains("pragma_table_info('a'b')"), "{evil}");
    }

    #[test]
    fn index_info_batch_is_none_without_indexes() {
        assert!(build_index_info_batch(&[]).is_none());
        let sql = build_index_info_batch(&["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(sql.matches(';').count(), 1, "{sql}");
        assert!(sql.contains("pragma_index_info('a')"), "{sql}");
        assert!(sql.contains("pragma_index_info('b')"), "{sql}");
        // The index name rides along so rows group back per index.
        assert!(sql.contains("'a' AS \"_idx\""), "{sql}");
        assert!(sql.contains("'b' AS \"_idx\""), "{sql}");
    }

    #[test]
    fn rows_batch_wraps_both_statements() {
        let t = users();
        let select = build_select(&t, &[], "", None, 0, 25).unwrap();
        let count = build_count(&t, &[], "").unwrap();
        let sql = build_rows_batch(&select, &count);
        assert_eq!(sql.matches(';').count(), 1, "{sql}");
        assert!(sql.contains(&select), "{sql}");
        assert!(sql.contains(&count), "{sql}");
        // A user column named like the tag cannot be stamped unambiguously.
        assert!(!batch_tag_collides(&t));
        let mut quirky = t;
        quirky.columns.push(col("_q", "TEXT", 0));
        assert!(batch_tag_collides(&quirky));
    }

    #[test]
    fn split_batched_routes_by_tag() {
        let rows = vec![
            serde_json::json!({"_q": "ti", "name": "id"}),
            serde_json::json!({"_q": "fk", "from": "org_id"}),
            serde_json::json!({"_q": "ti", "name": "name"}),
            serde_json::json!({"no_tag": true}),
        ];
        let sets = split_batched(
            rows.into_iter()
                .map(|v| v.as_object().unwrap().clone())
                .collect(),
        );
        assert_eq!(sets["ti"].len(), 2);
        assert_eq!(sets["fk"].len(), 1);
        // The tag itself is stripped; untagged rows are kept, not dropped.
        assert!(!sets["ti"][0].contains_key("_q"));
        assert_eq!(sets[""].len(), 1);
    }

    #[test]
    fn synthesized_config_is_minimal_and_secret_free() {
        let manifest = manifest_with(serde_json::json!([
            {"type": "d1", "name": "DB", "database_name": "app-db", "database_id": "abc-123"},
            {"type": "d1", "name": "PLAIN", "database_name": "plain-db"}
        ]));
        let dbs = manifest_dbs(&manifest).unwrap();
        let bytes = synthesize_config(&manifest, &dbs).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "name": "outer-app",
                "main": "index.js",
                "compatibility_date": "2026-09-01",
                "compatibility_flags": ["nodejs_als"],
                "d1_databases": [
                    {"binding": "DB", "database_name": "app-db", "database_id": "abc-123"},
                    {"binding": "PLAIN", "database_name": "plain-db"}
                ]
            })
        );
        // Exact key set: no vars, no other metadata.
        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), 5);
        // A manifest carrying injected secret values leaves no trace: vars is
        // never read, so the secret can't appear in the synthesized bytes.
        let mut leaky = manifest.clone();
        leaky["vars"] = serde_json::json!({"SECRET": "super-secret-value"});
        leaky["raw_metadata"]["vars"] = serde_json::json!({"OTHER": "super-secret-value"});
        let bytes = synthesize_config(&leaky, &manifest_dbs(&leaky).unwrap()).unwrap();
        assert!(!bytes.windows(18).any(|w| w == b"super-secret-value"));
    }

    // --- fake celld --------------------------------------------------------

    /// A fake `celld` for D1 tests: answers `d1 execute` with canned JSON
    /// per `--command`, recording argv, env and cwd under the state dir it
    /// reads back out of `--bucket s3://<dir>/fleets/<slug>`.
    fn install_fake_celld(state: &std::path::Path) -> PathBuf {
        let script = state.join("fake-celld");
        // Atomic install (write tmp, chmod, rename): an exec racing a direct
        // write fails ETXTBSY under the parallel suite.
        let tmp = script.with_extension("tmp");
        std::fs::write(
            &tmp,
            r#"#!/bin/sh
state_of_bucket() {
  prev=""
  for a in "$@"; do
    if [ "$prev" = "--bucket" ]; then printf '%s' "$a"; return 0; fi
    prev="$a"
  done
}
command_of() {
  prev=""
  for a in "$@"; do
    if [ "$prev" = "--command" ]; then printf '%s' "$a"; return 0; fi
    prev="$a"
  done
}
sql=$(command_of "$@")
bucket=$(state_of_bucket "$@")
statedir=$(printf '%s' "$bucket" | sed -e 's|^s3://||' -e 's|/fleets/.*||')
mkdir -p "$statedir"
printf '%s\n' "$*" >> "$statedir/calls.log"
env | sort >> "$statedir/env.log"
pwd >> "$statedir/cwd.log"
{ cat wrangler.json 2>/dev/null || echo MISSING; echo; } >> "$statedir/root-config.log"
case "$sql" in
  "PRAGMA table_list")
    printf '%s\n' '{"type":"table","name":"users"}' '{"type":"table","name":"_hidden"}' '{"type":"table","name":"sqlite_sequence"}' '{"type":"table","name":"d1_migrations"}' '{"type":"view","name":"v"}'
    ;;
  'PRAGMA table_info("users")')
    printf '%s\n' '{"name":"id","type":"INTEGER","notnull":1,"dflt_value":null,"pk":1}' '{"name":"name","type":"TEXT","notnull":0,"dflt_value":null,"pk":0}' '{"name":"age","type":"REAL","notnull":0,"dflt_value":null,"pk":0}'
    ;;
  "PRAGMA table_info("*)  # unknown table: no columns, like the real thing
    ;;
  "SELECT 'ti' AS "*)
    # The schema batch: table_info + foreign_key_list + index_list + DDL in
    # one exec, each row stamped with its section tag.
    case "$sql" in
      *"'users'"*)
        printf '%s\n' '{"_q":"ti","cid":0,"name":"id","type":"INTEGER","notnull":1,"dflt_value":null,"pk":1}' '{"_q":"ti","cid":1,"name":"name","type":"TEXT","notnull":0,"dflt_value":null,"pk":0}' '{"_q":"ti","cid":2,"name":"age","type":"REAL","notnull":0,"dflt_value":null,"pk":0}' '{"_q":"fk","from":"org_id","table":"orgs","to":"id"}' '{"_q":"il","name":"idx_users_name","unique":0}' '{"_q":"ddl","sql":"CREATE TABLE \"users\" (\"id\" INTEGER PRIMARY KEY)"}'
        ;;
    esac
    ;;
  "SELECT 'ii' AS "*)
    # The index-columns batch: one SELECT per index, rows tagged per index.
    printf '%s\n' '{"_q":"ii","_idx":"idx_users_name","seqno":0,"col":"name"}'
    ;;
  "SELECT 'rows' AS "*)
    # The rows batch: page plus total in one exec.
    printf '%s\n' '{"_q":"rows","id":1,"name":"ada","age":36.6}' '{"_q":"rows","id":2,"name":null,"age":null}' '{"_q":"count","n":2}'
    ;;
  *"COUNT(*) AS n"*)
    printf '%s\n' '{"t":"users","n":2}'
    ;;
  'SELECT "id", "name", "age" FROM "users"'*)
    printf '%s\n' '{"id":1,"name":"ada","age":36.6}' '{"id":2,"name":null,"age":null}'
    ;;
  'DELETE FROM "users"'*)
    case "$sql" in *"FAILME"*) echo "boom" >&2; exit 1;; esac
    n=$(printf '%s' "$sql" | grep -o 'DELETE FROM' | wc -l)
    i=0
    while [ "$i" -lt "$n" ]; do printf '%s\n' '{"n":1}'; i=$((i+1)); done
    ;;
  'INSERT INTO "users"'*|'UPDATE "users"'*|'DELETE FROM'*)
    case "$sql" in *"FAILME"*) echo "no such table" >&2; exit 1;; esac
    ;;
  *) echo "unexpected command: $sql" >&2; exit 1;;
esac
exit 0
"#,
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

    fn test_storage() -> FleetStorage {
        FleetStorage {
            endpoint: "http://127.0.0.1:1".to_string(),
            region: "test-region".to_string(),
            bucket: "outer-d1-test".to_string(),
            credentials: crate::store::credentials::CredentialSource::Static {
                access_key_id: "outeraccess".to_string(),
                secret_access_key: "outersecretoutersecretoutersecret12".to_string(),
            },
        }
    }

    fn op_ctx(
        pool: &SqlitePool,
        state: &tempfile::TempDir,
        script: &std::path::Path,
        store: Store,
    ) -> D1Ctx {
        let mut storage = test_storage();
        storage.bucket = state.path().to_string_lossy().into_owned();
        D1Ctx {
            celld_bin: script.to_string_lossy().into_owned(),
            storage,
            pool: pool.clone(),
            store,
            wake: None,
        }
    }

    const DEPLOY_PREFIX: &str = "deploy/outer-app/fd031dff832ad90a";

    /// The live manifest celld published for this app: two D1 bindings (one
    /// with an id, one plain), plus non-D1 binding types and a top-level
    /// `vars` map carrying a would-be secret — all of which the panel must
    /// ignore, never echoing the secret anywhere.
    fn seed_manifest() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 2,
            "script_name": "outer-app",
            "main_module": "index.js",
            "raw_metadata": {
                "bindings": [
                    {"type": "d1", "name": "DB", "database_name": "app-db", "database_id": "abc-123"},
                    {"type": "d1", "name": "PLAIN", "database_name": "plain-db"},
                    {"type": "kv_namespace", "name": "CACHE", "namespace_id": "kv-1"},
                    {"type": "assets", "name": "ASSETS"},
                    {"type": "plain_text", "name": "PUBLIC_KEY", "text": "pk-1"}
                ],
                "compatibility_date": "2026-09-01",
                "compatibility_flags": ["nodejs_als"],
                "main_module": "index.js"
            },
            "vars": {"SECRET": "super-secret-value"}
        })
    }

    /// The exact minimal config the panel must synthesize for the cwd.
    fn expected_config() -> serde_json::Value {
        serde_json::json!({
            "name": "outer-app",
            "main": "index.js",
            "compatibility_date": "2026-09-01",
            "compatibility_flags": ["nodejs_als"],
            "d1_databases": [
                {"binding": "DB", "database_name": "app-db", "database_id": "abc-123"},
                {"binding": "PLAIN", "database_name": "plain-db"}
            ]
        })
    }

    /// Pointer plus manifest, as `celld deploy` leaves them in the bucket.
    async fn seed_deployed(store: &Store) {
        store
            .put(
                "fleets/acme/deploy/current.json",
                format!("{{\"script_name\":\"outer-app\",\"version\":\"fd031dff832ad90a\",\"prefix\":\"{DEPLOY_PREFIX}\"}}")
                    .into_bytes(),
            )
            .await
            .unwrap();
        store
            .put(
                &format!("fleets/acme/{DEPLOY_PREFIX}/manifest.json"),
                serde_json::to_vec(&seed_manifest()).unwrap(),
            )
            .await
            .unwrap();
    }

    async fn seed_app(pool: &SqlitePool, port: i64) {
        let now = crate::auth::now_ms();
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("acct-ana@example.com")
        .bind("ana@example.com")
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) \
             VALUES ('app-acme', 'acme', 'acct-ana@example.com', 300001, ?, ?, ?, ?)",
        )
        .bind(port)
        .bind(port + 1)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn test_app(pool: &SqlitePool) -> crate::apps::App {
        // Operation tests never wake (wake: None), so the port is inert.
        seed_app(pool, 18001).await;
        crate::apps::get_by_slug(pool, "acme")
            .await
            .unwrap()
            .unwrap()
    }

    /// A stub fleet health gate: answers 200 on every connection so
    /// `ensure_awake`'s pre-spawn probe counts the fleet awake and no
    /// `celld` spawn ever runs. Loopback-only; the task dies with the test.
    /// Returns the bound port (each router test takes its own).
    async fn stub_health() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        (port, handle)
    }

    fn calls(state: &tempfile::TempDir) -> String {
        std::fs::read_to_string(state.path().join("calls.log")).unwrap_or_default()
    }

    fn last_call(state: &tempfile::TempDir) -> String {
        calls(state).lines().last().unwrap_or_default().to_string()
    }

    fn env_log(state: &tempfile::TempDir) -> String {
        std::fs::read_to_string(state.path().join("env.log")).unwrap_or_default()
    }

    #[tokio::test]
    async fn tables_filter_hidden_and_pin_argv() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        seed_deployed(&store).await;
        let ctx = op_ctx(&pool, &state, &script, store);
        let app = test_app(&pool).await;
        let (identity, tables) = list_tables(&ctx, &app, "DB").await.unwrap();
        assert_eq!(identity, "app-db");
        // Only the visible table survives; internals and views are dropped.
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "users");
        assert_eq!(tables[0].row_count, 2);
        let first = calls(&state).lines().next().unwrap_or_default().to_string();
        // Exact command line: scoped bucket, endpoint, region, JSON.
        assert!(
            first.starts_with(&format!(
                "d1 execute app-db --command PRAGMA table_list --json --bucket s3://{}/fleets/acme --endpoint http://127.0.0.1:1 --region test-region",
                state.path().display()
            )),
            "{first}"
        );
        // Scoped static creds in env; no session token, no parent keys.
        let env = env_log(&state);
        assert!(env.contains("AWS_ACCESS_KEY_ID=outeraccess"), "{env}");
        assert!(
            env.contains("AWS_SECRET_ACCESS_KEY=outersecretoutersecretoutersecret12"),
            "{env}"
        );
        assert!(!env.contains("AWS_SESSION_TOKEN"), "{env}");
        assert!(env.contains("S3_ENDPOINT=http://127.0.0.1:1"), "{env}");
        assert!(env.contains("AWS_REGION=test-region"), "{env}");
        // The command ran from a materialized deploy root holding only the
        // synthesized minimal config — never the secret-bearing manifest.
        let roots = std::fs::read_to_string(state.path().join("root-config.log")).unwrap();
        let lines: Vec<&str> = roots.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(!lines.is_empty(), "{roots}");
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value, expected_config(), "{line}");
            assert!(!line.contains("super-secret-value"), "{line}");
        }
    }

    #[tokio::test]
    async fn unknown_binding_never_reaches_celld() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        seed_deployed(&store).await;
        let ctx = op_ctx(&pool, &state, &script, store);
        let app = test_app(&pool).await;
        let err = list_tables(&ctx, &app, "OTHER").await.unwrap_err();
        assert!(matches!(err, D1Error::UnknownDatabase(_)));
        assert!(calls(&state).is_empty(), "no celld call on bad binding");
    }

    #[tokio::test]
    async fn missing_config_is_no_config() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let ctx = op_ctx(&pool, &state, &script, Store::memory());
        let app = test_app(&pool).await;
        let err = list_tables(&ctx, &app, "DB").await.unwrap_err();
        assert!(matches!(err, D1Error::NoConfig));
    }

    #[tokio::test]
    async fn pointer_without_manifest_is_no_config() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        store
            .put(
                "fleets/acme/deploy/current.json",
                br#"{"script_name":"outer-app","version":"v1","prefix":"deploy/outer-app/v1"}"#
                    .to_vec(),
            )
            .await
            .unwrap();
        let ctx = op_ctx(&pool, &state, &script, store);
        let app = test_app(&pool).await;
        let err = list_tables(&ctx, &app, "DB").await.unwrap_err();
        assert!(matches!(err, D1Error::NoConfig));
        assert!(calls(&state).is_empty(), "no celld call without a manifest");
    }

    #[tokio::test]
    async fn schema_loads_fk_indexes_ddl() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        seed_deployed(&store).await;
        let ctx = op_ctx(&pool, &state, &script, store);
        let app = test_app(&pool).await;
        let schema = table_schema(&ctx, &app, "DB", "users").await.unwrap();
        assert_eq!(schema.columns.len(), 3);
        assert_eq!(schema.columns[0].name, "id");
        assert!(schema.columns[0].not_null);
        assert_eq!(schema.columns[0].pk, 1);
        assert_eq!(schema.foreign_keys.len(), 1);
        assert_eq!(schema.foreign_keys[0].table, "orgs");
        assert_eq!(schema.indexes.len(), 1);
        assert_eq!(schema.indexes[0].columns, vec!["name".to_string()]);
        assert!(
            schema
                .sql
                .as_deref()
                .is_some_and(|s| s.contains("CREATE TABLE")),
            "{schema:?}"
        );
        let log: Vec<String> = calls(&state).lines().map(str::to_string).collect();
        assert_eq!(log.len(), 2, "{log:?}");
        assert!(log[0].contains("SELECT 'ti' AS"), "{log:?}");
        assert!(log[1].contains("SELECT 'ii' AS"), "{log:?}");
        // Hidden tables never reach celld through this path either.
        assert!(matches!(
            table_schema(&ctx, &app, "DB", "d1_migrations")
                .await
                .unwrap_err(),
            D1Error::UnknownTable(_)
        ));
        // An unknown visible table costs exactly the one schema batch.
        assert!(matches!(
            table_schema(&ctx, &app, "DB", "nope").await.unwrap_err(),
            D1Error::UnknownTable(_)
        ));
        assert_eq!(calls(&state).lines().count(), 3, "{}", calls(&state));
    }

    #[tokio::test]
    async fn rows_clamp_page_and_size() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        seed_deployed(&store).await;
        let ctx = op_ctx(&pool, &state, &script, store);
        let app = test_app(&pool).await;
        let body = RowsBody {
            page: -5,
            page_size: 500,
            sort: None,
            filters: vec![],
            search: String::new(),
        };
        let rows = table_rows(&ctx, &app, "DB", "users", &body).await.unwrap();
        assert_eq!(rows.page, 0);
        assert_eq!(rows.page_size, 100);
        assert_eq!(rows.total, 2);
        assert_eq!(
            rows.columns,
            vec!["id".to_string(), "name".to_string(), "age".to_string()]
        );
        // NULLs survive the round trip; numbers keep their text.
        assert_eq!(rows.rows[0][0], Some("1".to_string()));
        assert_eq!(rows.rows[0][1], Some("ada".to_string()));
        assert_eq!(rows.rows[1][1], None);
        let select = calls(&state)
            .lines()
            .find(|l| l.contains("FROM \"users\""))
            .unwrap_or_default()
            .to_string();
        assert!(select.contains("LIMIT 100 OFFSET 0"), "{select}");
        // Two execs: columns for validation, then page + total in one batch.
        let log: Vec<String> = calls(&state).lines().map(str::to_string).collect();
        assert_eq!(log.len(), 2, "{log:?}");
        assert!(log[1].contains("SELECT 'rows' AS"), "{log:?}");
        // Unknown columns and absurd input never reach celld.
        let bad = RowsBody {
            page: 0,
            page_size: 25,
            sort: None,
            filters: vec![filter("nope", D1FilterOp::Eq, "1")],
            search: String::new(),
        };
        assert!(matches!(
            table_rows(&ctx, &app, "DB", "users", &bad)
                .await
                .unwrap_err(),
            D1Error::BadQuery(_)
        ));
    }

    #[tokio::test]
    async fn write_builds_curated_statements() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        seed_deployed(&store).await;
        let ctx = op_ctx(&pool, &state, &script, store);
        let app = test_app(&pool).await;
        // Insert with a quote in the value and an explicit NULL.
        write_row(
            &ctx,
            &app,
            "DB",
            "users",
            &WriteBody {
                op: D1WriteOp::Insert,
                key: BTreeMap::new(),
                values: BTreeMap::from([
                    ("name".to_string(), Some("o'clock".to_string())),
                    ("age".to_string(), None),
                ]),
            },
        )
        .await
        .unwrap();
        let call = last_call(&state);
        assert!(
            call.contains("INSERT INTO \"users\" (\"age\", \"name\") VALUES (NULL, 'o''clock')"),
            "{call}"
        );
        assert!(
            !call.contains("--json"),
            "writes don't parse output: {call}"
        );
        // Update by PK.
        write_row(
            &ctx,
            &app,
            "DB",
            "users",
            &WriteBody {
                op: D1WriteOp::Update,
                key: BTreeMap::from([("id".to_string(), Some("1".to_string()))]),
                values: BTreeMap::from([("age".to_string(), Some("37".to_string()))]),
            },
        )
        .await
        .unwrap();
        let call = last_call(&state);
        assert!(
            call.contains("UPDATE \"users\" SET \"age\" = 37 WHERE \"id\" = 1"),
            "{call}"
        );
        // Unknown value columns are refused while building the statement: the
        // schema load runs (validation needs it), but no INSERT ever does.
        let before = calls(&state);
        let err = write_row(
            &ctx,
            &app,
            "DB",
            "users",
            &WriteBody {
                op: D1WriteOp::Insert,
                key: BTreeMap::new(),
                values: BTreeMap::from([("nope".to_string(), Some("x".to_string()))]),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, D1Error::BadQuery(_)));
        let after = calls(&state);
        let fresh = after.strip_prefix(&before).unwrap_or(&after);
        assert!(
            !fresh.contains("INSERT INTO"),
            "no INSERT for refused write: {fresh}"
        );
    }

    #[tokio::test]
    async fn bulk_delete_sums_changes_and_caps_keys() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        seed_deployed(&store).await;
        let ctx = op_ctx(&pool, &state, &script, store);
        let app = test_app(&pool).await;
        let deleted = delete_rows(
            &ctx,
            &app,
            "DB",
            "users",
            &[
                BTreeMap::from([("id".to_string(), Some("1".to_string()))]),
                BTreeMap::from([("id".to_string(), Some("2".to_string()))]),
            ],
        )
        .await
        .unwrap();
        assert_eq!(deleted, 2);
        let call = last_call(&state);
        assert!(
            call.contains(
                "DELETE FROM \"users\" WHERE \"id\" = 1; SELECT changes() AS n; DELETE FROM"
            ),
            "{call}"
        );
        let many: Vec<_> = (0..=MAX_DELETE_KEYS)
            .map(|i| BTreeMap::from([("id".to_string(), Some(i.to_string()))]))
            .collect();
        assert!(matches!(
            delete_rows(&ctx, &app, "DB", "users", &many)
                .await
                .unwrap_err(),
            D1Error::BadQuery(_)
        ));
    }

    #[tokio::test]
    async fn celld_failure_surfaces() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        seed_deployed(&store).await;
        let ctx = op_ctx(&pool, &state, &script, store);
        let app = test_app(&pool).await;
        let err = write_row(
            &ctx,
            &app,
            "DB",
            "users",
            &WriteBody {
                op: D1WriteOp::Delete,
                key: BTreeMap::from([("id".to_string(), Some("FAILME".to_string()))]),
                values: BTreeMap::new(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, D1Error::Celld(_)), "{err:?}");
    }

    // --- HTTP guard + audit tests ------------------------------------------

    const PEER: &str = "127.0.0.1:43123";

    fn peer(mut req: Request<Body>) -> Request<Body> {
        req.extensions_mut().insert(axum::extract::ConnectInfo(
            PEER.parse::<SocketAddr>().expect("test peer parses"),
        ));
        req
    }

    fn same_origin(req: axum::http::request::Builder) -> axum::http::request::Builder {
        req.header("sec-fetch-site", "same-origin")
    }

    async fn send(
        app: &Router,
        req: Request<Body>,
    ) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
        let res = app.clone().oneshot(peer(req)).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        (
            status,
            headers,
            serde_json::from_str(&text).unwrap_or(json!({"raw": text})),
        )
    }

    async fn authed_get(app: &Router, path: &str, cookie: &str) -> (StatusCode, serde_json::Value) {
        let (status, _, body) = send(
            app,
            same_origin(Request::get(path))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        (status, body)
    }

    async fn router_app(
        pool: &SqlitePool,
        state: &tempfile::TempDir,
    ) -> (Router, tokio::task::JoinHandle<()>) {
        let (config, _codes) = test_config();
        let auth = AuthState {
            pool: pool.clone(),
            config,
        };
        let script = install_fake_celld(state.path());
        let store = Store::memory();
        seed_deployed(&store).await;
        // The wake probe hits this stub and counts the fleet awake, so no
        // celld spawn ever runs in router tests.
        let (port, health) = stub_health().await;
        seed_app(pool, port as i64).await;
        let mut storage = test_storage();
        storage.bucket = state.path().to_string_lossy().into_owned();
        let fleets = FleetManager::new(crate::fleet::manager::ManagerSettings {
            data_dir: state.path().join("fleets"),
            celld_bin: script.to_string_lossy().into_owned(),
            storage,
            cgroups: None,
            memory_max_bytes: 0,
            cpu_max: String::new(),
            sleep_after: Duration::from_secs(1800),
            credential_refresh_after: Duration::from_secs(86400),
            wake_timeout: Duration::from_secs(1),
            wake_concurrency: 1,
            always_on_max: 10,
            preview_max: 20,
        });
        let base = McpState::for_tests(pool.clone(), auth.clone());
        let mcp = McpState {
            fleets,
            store,
            ..base
        };
        let router = crate::http::router(crate::http::AppState {
            pool: pool.clone(),
            sync: StateSync::new("test"),
            auth,
            mcp,
            site: crate::http::site::SiteDir::new(std::path::Path::new("/nonexistent-site")),
        });
        (router, health)
    }

    async fn login(pool: &SqlitePool, email: &str) -> String {
        let id = crate::auth::create_session(pool, &format!("acct-{email}"))
            .await
            .unwrap();
        format!("__Host-outer_sid={id}")
    }

    #[tokio::test]
    async fn guards_owner_and_same_origin() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let (app, _health) = router_app(&pool, &state).await;
        // A second owner with their own app: cross-owner reads are 404.
        let now = crate::auth::now_ms();
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("acct-bob@example.com")
        .bind("bob@example.com")
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) \
             VALUES ('app-bobs', 'bobs', 'acct-bob@example.com', 300002, 18011, 18012, ?, ?)",
        )
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let path = "/api/console/apps/acme/data/d1/DB/tables";
        // No session: 401 with no-store.
        let (status, headers, _) = send(
            &app,
            same_origin(Request::get(path)).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers["cache-control"], "no-store");
        // Session but cross-site: 403.
        let ana = login(&pool, "ana@example.com").await;
        let (status, _, _) = send(
            &app,
            Request::get(path)
                .header("cookie", &ana)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Foreign slug answers 404 exactly like a missing one.
        let (status, body) =
            authed_get(&app, "/api/console/apps/bobs/data/d1/DB/tables", &ana).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");
        // Unknown slug: same 404.
        let (status, _) = authed_get(&app, "/api/console/apps/nope/data/d1/DB/tables", &ana).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn frozen_restore_refuses_before_touching_backups() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let (app, _health) = router_app(&pool, &state).await;
        let ana = login(&pool, "ana@example.com").await;
        sqlx::query(
            "UPDATE app SET storage_frozen_at = 1, storage_frozen_detail = 'D1 over cap' \
             WHERE slug = 'acme'",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Even a well-formed confirm never reaches the restore: the app is
        // read-only until it drops back under the cap.
        let (status, _, body) = send(
            &app,
            same_origin(
                Request::post("/api/console/apps/acme/data/d1/DB/backups/b1/restore")
                    .header("cookie", &ana)
                    .header("content-type", "application/json"),
            )
            .body(Body::from(r#"{"confirm":"b1"}"#))
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["error"], "quota_exceeded");
        assert_eq!(body["quota"], "storage_bytes");
    }

    #[tokio::test]
    async fn d1_stays_owner_only_for_collaborators() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let (app, _health) = router_app(&pool, &state).await;
        // Collaborators count only while the owner is Pro (spec §7.4): the
        // 403 below is the owner check, not a missing role.
        sqlx::query("UPDATE user SET plan = 'pro' WHERE email = 'ana@example.com'")
            .execute(&pool)
            .await
            .unwrap();
        let now = crate::auth::now_ms();
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("acct-bruno@example.com")
        .bind("bruno@example.com")
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("acct-stranger@example.com")
        .bind("stranger@example.com")
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app_collaborator (app_id, email, userId, added_at) \
             VALUES ('app-acme', 'bruno@example.com', 'acct-bruno@example.com', ?1)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let bruno = login(&pool, "bruno@example.com").await;
        let stranger = login(&pool, "stranger@example.com").await;

        // D1 writes and reads are owner-only in v1: a collaborator reads
        // 403 with a user message, before any fleet work runs.
        let (status, body) =
            authed_get(&app, "/api/console/apps/acme/data/d1/databases", &bruno).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "forbidden");
        assert!(
            body["user_message"].as_str().is_some_and(|m| !m.is_empty()),
            "{body}"
        );
        // A stranger's slug still reads 404 exactly like a missing one.
        let (status, _) =
            authed_get(&app, "/api/console/apps/acme/data/d1/databases", &stranger).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn tables_round_trip_through_router() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let (app, _health) = router_app(&pool, &state).await;
        let ana = login(&pool, "ana@example.com").await;
        let (status, body) =
            authed_get(&app, "/api/console/apps/acme/data/d1/databases", &ana).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["databases"][0]["binding"], "DB");
        let (status, body) =
            authed_get(&app, "/api/console/apps/acme/data/d1/DB/tables", &ana).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tables"].as_array().unwrap().len(), 1);
        assert_eq!(body["tables"][0]["row_count"], 2);
        // The error envelope always carries a user_message for the UI.
        let (status, body) =
            authed_get(&app, "/api/console/apps/acme/data/d1/OTHER/tables", &ana).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");
        assert!(
            body["user_message"].as_str().is_some_and(|m| !m.is_empty()),
            "{body}"
        );
    }

    #[tokio::test]
    async fn writes_audit_with_console_client() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let (app, _health) = router_app(&pool, &state).await;
        let ana = login(&pool, "ana@example.com").await;
        let write = |body: serde_json::Value| {
            let app = &app;
            let ana = ana.clone();
            async move {
                send(
                    app,
                    same_origin(Request::post(
                        "/api/console/apps/acme/data/d1/DB/tables/users/write",
                    ))
                    .header("cookie", ana)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
                )
                .await
            }
        };
        let (status, _, body) = write(json!({"op": "insert", "values": {"name": "bea"}})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        // Refused writes still audit (invalid_argument, no values in the log).
        let (status, _, body) = write(json!({"op": "insert", "values": {"nope": "x"}})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_argument");
        #[derive(Debug, sqlx::FromRow)]
        struct AuditRow {
            tool: String,
            summary: String,
            outcome: String,
            consent_id: Option<String>,
            apikey_id: Option<String>,
        }
        let rows: Vec<AuditRow> = sqlx::query_as(
            "SELECT tool, summary, outcome, consent_id, apikey_id FROM agent_action WHERE app_id = 'app-acme' ORDER BY created_at",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tool, "d1_write");
        assert_eq!(rows[0].summary, "inserted a row into users");
        assert_eq!(rows[0].outcome, "success");
        assert!(rows[0].consent_id.is_none() && rows[0].apikey_id.is_none());
        assert_eq!(rows[1].outcome, "error");
        assert!(
            !rows[1].summary.contains('x'),
            "values never land in the log: {}",
            rows[1].summary
        );
        // …and the action log labels those rows "Console".
        let listed = crate::mcp::actions::list(&pool, "app-acme", None, 10)
            .await
            .unwrap();
        assert!(
            listed.iter().all(|row| row.client_label == "Console"),
            "{listed:?}"
        );
    }

    #[tokio::test]
    async fn bulk_delete_audits_and_caps() {
        let pool = test_pool().await;
        let state = tempfile::tempdir().unwrap();
        let (app, _health) = router_app(&pool, &state).await;
        let ana = login(&pool, "ana@example.com").await;
        let (status, _, body) = send(
            &app,
            same_origin(Request::post(
                "/api/console/apps/acme/data/d1/DB/tables/users/delete-rows",
            ))
            .header("cookie", ana.clone())
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"keys": [{"id": "1"}, {"id": "2"}]}).to_string(),
            ))
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deleted"], 2);
        drop(ana);
    }
}
