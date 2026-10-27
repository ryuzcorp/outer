//! The data tools: `app_metrics` (traffic, errors and latency from the
//! same `metric_minute`/`path_hour`/`visitor_day` rows the Console metrics
//! tab reads) and `db_query` (read-only SQL over the app's live D1 through
//! the same celld path as the Console D1 panel).
//!
//! `db_query` reads go through the celld path, never the migration path, so
//! the storage-freeze rules don't apply to them: reads stay allowed while
//! the app is frozen, exactly like the Console grid. Writes stay with
//! migrations and `restore_backup`; the read-only refusal says so.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{ok_untrusted, visible_app, Ctx};
use crate::mcp::error::ToolError;

// ---------------------------------------------------------------------------
// app_metrics
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppMetricsInput {
    /// The app slug.
    pub app: String,
    /// Window in hours: 24, 168 or 720 (default 24), like the Console metrics tab.
    pub hours: Option<i64>,
}

/// How many top paths the snapshot carries: the Console shows 20, the tool
/// stays compact.
const MAX_TOP_PATHS: usize = 10;

pub async fn app_metrics(input: AppMetricsInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let hours = input.hours.unwrap_or(24);
    if !matches!(hours, 24 | 168 | 720) {
        return Err(ToolError::invalid_argument(
            "hours must be one of 24, 168 or 720",
        ));
    }
    let summary = crate::observe::metrics::summary(&ctx.state.pool, &app.id, hours)
        .await
        .map_err(super::internal)?;
    let error_rate = if summary.requests > 0 {
        summary.s5xx as f64 / summary.requests as f64
    } else {
        0.0
    };
    let top_paths: Vec<Value> = summary
        .top_paths
        .into_iter()
        .take(MAX_TOP_PATHS)
        .map(|p| json!({"path": p.path, "requests": p.requests, "errors": p.s5xx}))
        .collect();
    // Paths name app routes, which visitor input can shape (`/evil<script>`
    // is a real path row): the list rides behind the §8.4 notice.
    let data = if top_paths.is_empty() {
        "(no traffic in this window)".to_string()
    } else {
        top_paths
            .iter()
            .map(|p| {
                format!(
                    "{} — {} requests, {} errors",
                    p["path"].as_str().unwrap_or("?"),
                    p["requests"].as_i64().unwrap_or(0),
                    p["errors"].as_i64().unwrap_or(0)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(ok_untrusted(
        format!(
            "{}: {} requests, {} visitors, {} errors over {}h (p50 {}ms, p95 {}ms).",
            app.slug,
            summary.requests,
            summary.visitors,
            summary.s5xx,
            hours,
            summary.p50_ms,
            summary.p95_ms,
        ),
        data,
        json!({
            "app": app.slug,
            "hours": hours,
            "requests": summary.requests,
            "visitors": summary.visitors,
            "errors": summary.s5xx,
            "error_rate": error_rate,
            "s4xx": summary.s4xx,
            "s5xx": summary.s5xx,
            "p50_ms": summary.p50_ms,
            "p95_ms": summary.p95_ms,
            "top_paths": top_paths,
        }),
    ))
}

// ---------------------------------------------------------------------------
// db_query: read-only SQL over live D1
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbQueryInput {
    /// The app slug.
    pub app: String,
    /// The D1 binding (defaults to the single declared binding).
    pub binding: Option<String>,
    /// One SELECT, WITH … SELECT or PRAGMA table_info statement, with `?`
    /// placeholders filled from `params`.
    pub sql: String,
    /// Values for the `?` placeholders, in order (numbers stay bare, strings
    /// are quoted server-side).
    pub params: Option<Vec<Value>>,
}

/// Most rows one call returns; further rows set `truncated`.
const MAX_QUERY_ROWS: usize = 200;
/// Longest string value kept per cell (characters); longer values are cut.
const MAX_VALUE_CHARS: usize = 5000;
/// The rows JSON stays under this many bytes; trailing rows drop first.
/// (`celld d1 execute` kills the child past
/// [`crate::http::console_data::d1::D1_TIMEOUT`], so a runaway query always
/// answers in time.)
const MAX_RESULT_BYTES: usize = 250_000;
/// The read-only refusal: names the allowed shapes and where writes live.
fn read_only_msg() -> ToolError {
    ToolError::invalid_argument(
        "db_query is read-only: pass a single SELECT, WITH … SELECT or PRAGMA table_info \
         statement (? placeholders come from params). Ship writes as migrations in your next \
         commit — db/migrations/NNNN_name.sql applies on deploy — restore a dump with \
         restore_backup, or edit single rows in Console → Data → D1.",
    )
}

/// Words that can never appear in a read, UPPERCASED to match the
/// tokenizer's output (quoted identifiers and string literals never match,
/// so only bare words are compared). Any one of them anywhere in the
/// statement — including inside a `WITH` body, where SQLite only holds
/// reads anyway — refuses the query.
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT",
    "UPDATE",
    "DELETE",
    "DROP",
    "CREATE",
    "ALTER",
    "REPLACE",
    "ATTACH",
    "DETACH",
    "VACUUM",
    "REINDEX",
    "ANALYZE",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE",
    "GRANT",
    "REVOKE",
    "COPY",
    "CALL",
    "EXECUTE",
    "EXEC",
    "LOAD",
    "LOAD_EXTENSION",
    "TRANSACTION",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokKind {
    /// A bare word, uppercased for the keyword checks.
    Word,
    /// `;` — only one, and only trailing, survives.
    Semi,
    /// A bare `?` placeholder.
    QMark,
    /// Anything else (punctuation, numbers): structure for the PRAGMA
    /// shape check, never a keyword.
    Other,
}

#[derive(Debug)]
struct Tok {
    kind: TokKind,
    /// Uppercase text for [`TokKind::Word`], else empty.
    text: String,
    start: usize,
    end: usize,
}

/// Split `sql` into significant tokens: whitespace and comments vanish,
/// string literals and quoted identifiers vanish (they can neither smuggle
/// a keyword nor a statement separator), everything else is classified.
/// Fails on unterminated comments, strings and quoted names.
fn tokenize(sql: &str) -> Result<Vec<Tok>, ToolError> {
    let bytes = sql.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    // A `;` already seen: anything significant after it starts a second
    // statement (a trailing comment or whitespace is fine — they leave no
    // token). The push sites check this inline: a closure would borrow
    // `seen_semi` across the `;` assignment.
    let mut seen_semi = false;
    macro_rules! push_tok {
        ($tok:expr) => {
            if seen_semi {
                return Err(read_only_msg());
            }
            toks.push($tok);
        };
    }
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // `--` line comment.
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // `/*` block comment (`SEL/**/ECT` tokenizes as one word, exactly
        // like SQLite reads it — and `SELECT` is still a read).
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                j += 1;
            }
            if j + 1 >= bytes.len() {
                return Err(read_only_msg());
            }
            i = j + 2;
            continue;
        }
        // `'...'` strings ('' escapes), `"..."`/`` `...` `` quoted idents.
        if c == b'\'' || c == b'"' || c == b'`' {
            let mut j = i + 1;
            loop {
                if j >= bytes.len() {
                    return Err(read_only_msg());
                }
                if bytes[j] == c {
                    if bytes.get(j + 1) == Some(&c) {
                        j += 2;
                        continue;
                    }
                    break;
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        // `[...]` quoted idents.
        if c == b'[' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b']' {
                j += 1;
            }
            if j >= bytes.len() {
                return Err(read_only_msg());
            }
            i = j + 1;
            continue;
        }
        if c == b';' {
            if seen_semi {
                return Err(read_only_msg());
            }
            seen_semi = true;
            toks.push(Tok {
                kind: TokKind::Semi,
                text: String::new(),
                start: i,
                end: i + 1,
            });
            i += 1;
            continue;
        }
        // `?` placeholders; `?NNN` numbered ones are refused (params bind
        // positionally, so a numbered hole could silently take the wrong
        // value).
        if c == b'?' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                return Err(ToolError::invalid_argument(
                    "db_query params bind positionally: use bare ? placeholders, not ?NNN",
                ));
            }
            push_tok!(Tok {
                kind: TokKind::QMark,
                text: String::new(),
                start: i,
                end: j,
            });
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let mut j = i + 1;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric()
                    || bytes[j] == b'_'
                    || bytes[j] == b'$'
                    || bytes[j] >= 0x80)
            {
                j += 1;
            }
            // Keywords compare ASCII-only: non-ASCII bytes never match.
            push_tok!(Tok {
                kind: TokKind::Word,
                text: sql[i..j].to_ascii_uppercase(),
                start: i,
                end: j,
            });
            i = j;
            continue;
        }
        push_tok!(Tok {
            kind: TokKind::Other,
            text: String::new(),
            start: i,
            end: i + 1,
        });
        i += 1;
    }
    Ok(toks)
}

/// The words of `sql` in order (bare words only).
fn words(toks: &[Tok]) -> Vec<&str> {
    toks.iter()
        .filter(|t| t.kind == TokKind::Word)
        .map(|t| t.text.as_str())
        .collect()
}

/// Refuse anything that is not a single `SELECT`, `WITH … SELECT` or
/// `PRAGMA table_info` statement.
fn check_read_only(sql: &str) -> Result<Vec<Tok>, ToolError> {
    let toks = tokenize(sql)?;
    let bare: Vec<&Tok> = toks.iter().filter(|t| t.kind != TokKind::Semi).collect();
    if bare.is_empty() {
        return Err(read_only_msg());
    }
    let first = match bare[0].kind {
        TokKind::Word => bare[0].text.as_str(),
        _ => return Err(read_only_msg()),
    };
    if !matches!(first, "SELECT" | "WITH" | "PRAGMA") {
        return Err(read_only_msg());
    }
    for w in words(&toks) {
        if w == "PRAGMA" && first != "PRAGMA" {
            return Err(read_only_msg());
        }
        if WRITE_KEYWORDS.contains(&w) {
            return Err(read_only_msg());
        }
    }
    if first == "PRAGMA" {
        check_pragma_table_info(sql, &bare)?;
    }
    Ok(toks)
}

/// After a leading `PRAGMA`, only `[schema .] table_info[(name)]` passes:
/// every other pragma either writes (`writable_schema`, `journal_mode`) or
/// reaches outside the read. Punctuation compares against the source span,
/// so `PRAGMA table_info(x)` and `PRAGMA main.table_info(x)` pass while
/// `PRAGMA table_info = x` or a second call after `;` fails.
fn check_pragma_table_info(sql: &str, bare: &[&Tok]) -> Result<(), ToolError> {
    let punct = |t: &&Tok, want: &str| t.kind == TokKind::Other && sql[t.start..t.end] == *want;
    // bare[0] is PRAGMA.
    let mut i = 1;
    // Optional `schema .` qualifier.
    if bare.get(i).is_some_and(|t| t.kind == TokKind::Word)
        && bare.get(i + 1).is_some_and(|t| punct(t, "."))
        && bare.get(i + 2).is_some_and(|t| t.kind == TokKind::Word)
    {
        i += 2;
    }
    let name = bare.get(i).filter(|t| t.kind == TokKind::Word);
    if !name.is_some_and(|t| t.text == "TABLE_INFO") {
        return Err(read_only_msg());
    }
    i += 1;
    if i == bare.len() {
        return Ok(());
    }
    // Optional balanced `(...)` argument; the engine resolves the name.
    if !bare.get(i).is_some_and(|t| punct(t, "(")) {
        return Err(read_only_msg());
    }
    let mut depth = 0;
    while i < bare.len() {
        let t = bare[i];
        if punct(&t, "(") {
            depth += 1;
        } else if punct(&t, ")") {
            depth -= 1;
            if depth == 0 {
                i += 1;
                break;
            }
        }
        i += 1;
    }
    if depth != 0 || i != bare.len() {
        return Err(read_only_msg());
    }
    Ok(())
}

/// Fill the `?` placeholders of an already-checked statement, in order.
/// Strings are quoted server-side, so a param can never smuggle syntax.
fn substitute(sql: &str, toks: &[Tok], params: &[Value]) -> Result<String, ToolError> {
    let holes = toks.iter().filter(|t| t.kind == TokKind::QMark).count();
    if holes != params.len() {
        return Err(ToolError::invalid_argument(format!(
            "the query has {holes} ? placeholder(s) but {n} param(s)",
            n = params.len()
        )));
    }
    let mut out = String::with_capacity(sql.len() + 64);
    let mut last = 0;
    let mut values = params.iter();
    for t in toks.iter().filter(|t| t.kind == TokKind::QMark) {
        out.push_str(&sql[last..t.start]);
        let v = values.next().unwrap_or(&Value::Null);
        match v {
            Value::Null => out.push_str("NULL"),
            Value::Bool(b) => out.push_str(if *b { "1" } else { "0" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => {
                out.push('\'');
                out.push_str(&s.replace('\'', "''"));
                out.push('\'');
            }
            _ => {
                return Err(ToolError::invalid_argument(
                    "db_query params must be scalars: string, number, boolean or null",
                ));
            }
        }
        last = t.end;
    }
    out.push_str(&sql[last..]);
    Ok(out)
}

fn map_d1(err: crate::http::console_data::d1::D1Error) -> ToolError {
    use crate::http::console_data::d1::D1Error;
    match err {
        D1Error::BadQuery(message) => ToolError::invalid_argument(message),
        D1Error::NoConfig => ToolError::invalid_argument(
            "this app has no deployed database config yet; deploy once before querying",
        ),
        D1Error::UnknownDatabase(binding) => {
            ToolError::invalid_argument(format!("unknown database {binding:?}"))
        }
        // The child ran and refused the statement — a missing table, a bad
        // column — so the query, not the platform, is at fault. celld
        // prints engine errors, bounded to a few lines.
        D1Error::Celld(detail) => {
            ToolError::invalid_argument(format!("the database refused the query: {detail}"))
        }
        D1Error::AppStarting => {
            ToolError::internal("the app is still starting; try again in a minute")
        }
        D1Error::Timeout => ToolError::internal("the query timed out after 30 s; narrow it down"),
        D1Error::Unavailable(source) => super::internal(source),
        // Table/backup variants only arise from the curated grid paths,
        // never from a free-text query; the engine names them instead.
        D1Error::UnknownTable(table) => ToolError::invalid_argument(format!(
            "the database refused the query: unknown table {table:?}"
        )),
        D1Error::UnknownBackup(id) => ToolError::internal(format!("unknown backup {id:?}")),
        D1Error::TooLarge(detail) => {
            ToolError::internal(format!("query result too large: {detail}"))
        }
    }
}

pub async fn db_query(input: DbQueryInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    let sql = input.sql.trim();
    if sql.len() > 20_000 {
        return Err(ToolError::invalid_argument(
            "the query is too long (20 000 characters max)",
        ));
    }
    // The read-only gate runs before any celld call: a refused shape never
    // reaches the database.
    let toks = check_read_only(sql)?;
    let params = input.params.unwrap_or_default();
    let final_sql = substitute(sql, &toks, &params)?;
    // The declared bindings come from the live manifest — the same source
    // as Console → Data → D1 — so a typo'd binding fails here, before any
    // celld call.
    let bindings = crate::http::console_data::d1::database_bindings(&ctx.state.store, &app.slug)
        .await
        .map_err(super::internal)?;
    let binding = match (input.binding, bindings.as_slice()) {
        (Some(want), list) if list.contains(&want) => want,
        (Some(want), _) => {
            return Err(ToolError::invalid_argument(format!(
                "unknown database {want:?}"
            )));
        }
        (None, [only]) => only.clone(),
        (None, []) => {
            return Err(ToolError::invalid_argument(
                "this app has no D1 database: declare d1_databases in void.config.ts and deploy first",
            ));
        }
        (None, list) => {
            let names = list.join(", ");
            return Err(ToolError::invalid_argument(format!(
                "this app has several D1 databases ({names}): pass binding explicitly"
            )));
        }
    };
    // Same path as the Console panel: wake the fleet, resolve the binding
    // to its celld identity, run one `celld d1 execute` with the shared
    // 30 s budget. Reads only — the storage-freeze write guard that the
    // Console grid and commit path check does not apply here.
    let d1ctx = crate::http::console_data::d1::D1Ctx {
        celld_bin: ctx.state.fleets.celld_bin(),
        storage: ctx.state.fleets.storage().clone(),
        pool: ctx.state.pool.clone(),
        store: ctx.state.store.clone(),
        wake: Some(ctx.state.fleets.clone()),
    };
    let (identity, project) = crate::http::console_data::d1::ready_backup(&d1ctx, &app, &binding)
        .await
        .map_err(map_d1)?;
    let out = crate::http::console_data::d1::d1_exec(
        &d1ctx,
        &app.slug,
        &identity,
        &final_sql,
        true,
        project.path(),
    )
    .await
    .map_err(map_d1)?;
    let mut rows: Vec<Value> = crate::http::console_data::d1::parse_json_rows(&out)
        .into_iter()
        .map(Value::Object)
        .collect();
    // Bounds: at most MAX_QUERY_ROWS rows, values cut past MAX_VALUE_CHARS,
    // trailing rows dropped past MAX_RESULT_BYTES. Every cut sets
    // `truncated`.
    let mut truncated = false;
    if rows.len() > MAX_QUERY_ROWS {
        rows.truncate(MAX_QUERY_ROWS);
        truncated = true;
    }
    for row in &mut rows {
        if let Value::Object(map) = row {
            for value in map.values_mut() {
                if let Some(s) = value.as_str() {
                    if s.chars().count() > MAX_VALUE_CHARS {
                        let cut: String = s.chars().take(MAX_VALUE_CHARS).collect();
                        *value = Value::String(cut + "…");
                        truncated = true;
                    }
                }
            }
        }
    }
    while serde_json::to_string(&rows).map(|s| s.len()).unwrap_or(0) > MAX_RESULT_BYTES
        && rows.len() > 1
    {
        rows.pop();
        truncated = true;
    }
    if serde_json::to_string(&rows).map(|s| s.len()).unwrap_or(0) > MAX_RESULT_BYTES {
        for row in &mut rows {
            if let Value::Object(map) = row {
                for value in map.values_mut() {
                    if let Some(s) = value.as_str() {
                        if s.len() > 200 {
                            let cut: String = s.chars().take(200).collect();
                            *value = Value::String(cut + "…");
                        }
                    }
                }
            }
        }
        truncated = true;
    }
    let mut columns: Vec<String> = Vec::new();
    for row in &rows {
        if let Value::Object(map) = row {
            for key in map.keys() {
                if !columns.contains(key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    let row_count = rows.len();
    let data = if rows.is_empty() {
        "(no rows)".to_string()
    } else {
        rows.iter()
            .map(|r| serde_json::to_string(r).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(ok_untrusted(
        format!(
            "{row_count} row(s) from {binding}{trunc}.",
            trunc = if truncated { " (truncated)" } else { "" },
        ),
        data,
        json!({
            "app": app.slug,
            "binding": binding,
            "columns": columns,
            "rows": rows,
            "row_count": row_count,
            "truncated": truncated,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(sql: &str) -> bool {
        check_read_only(sql).is_err()
    }

    fn allowed(sql: &str) -> bool {
        check_read_only(sql).is_ok()
    }

    #[test]
    fn plain_selects_pass() {
        assert!(allowed("SELECT 1"));
        assert!(allowed("select * from users"));
        assert!(allowed(
            "SeLeCt id, name FROM users WHERE age > 3 ORDER BY id LIMIT 10"
        ));
        assert!(allowed("SELECT 1;"));
        assert!(allowed("  SELECT 1  "));
        assert!(allowed(
            "WITH top AS (SELECT * FROM users) SELECT * FROM top"
        ));
        assert!(allowed("PRAGMA table_info(users)"));
        assert!(allowed("PRAGMA table_info(\"users\")"));
        assert!(allowed("pragma main.table_info(users)"));
    }

    #[test]
    fn comments_and_strings_cannot_smuggle_statements() {
        // A `;` inside a string or comment is data, not a separator.
        assert!(allowed("SELECT * FROM t WHERE name = 'a;b'"));
        assert!(allowed("/* drop the table; really */ SELECT 1"));
        assert!(allowed("SELECT 1 -- ; DROP TABLE t"));
        // ...but a real second statement is refused, however hidden.
        assert!(refused("SELECT 1; DROP TABLE t"));
        assert!(refused("SELECT 1; /* hidden */ DELETE FROM t"));
        assert!(refused("SELECT 'a'; SELECT 'b'"));
        assert!(refused("SELECT 1;;"));
    }

    #[test]
    fn writes_are_refused_including_cte_bodies() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "DROP TABLE t",
            "CREATE TABLE t (a)",
            "ALTER TABLE t ADD COLUMN a TEXT",
            "REPLACE INTO t VALUES (1)",
            "ATTACH DATABASE 'x' AS y",
            "DETACH y",
            "VACUUM",
            "BEGIN; SELECT 1",
            "SELECT 1; COMMIT",
            // A WITH body only ever holds reads: the outer verb decides.
            "WITH x AS (SELECT 1) DELETE FROM t",
            "WITH x AS (SELECT 1) UPDATE t SET a = 2",
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
            // PRAGMAs that write or escape the read.
            "PRAGMA writable_schema = ON",
            "PRAGMA journal_mode = WAL",
            "PRAGMA table_info(users); PRAGMA writable_schema = ON",
            "SELECT * FROM t; PRAGMA table_info(t)",
            "EXPLAIN DELETE FROM t",
            "VALUES (1)",
            "",
            "   ",
            "SELECT load_extension('evil')",
        ] {
            assert!(refused(sql), "must refuse: {sql}");
        }
    }

    #[test]
    fn quoted_keywords_stay_data() {
        assert!(allowed("SELECT \"update\" FROM t"));
        assert!(allowed("SELECT * FROM t WHERE name = 'delete'"));
        assert!(allowed("SELECT [drop] FROM t"));
    }

    #[test]
    fn unterminated_literals_are_refused() {
        assert!(refused("SELECT 'abc"));
        assert!(refused("SELECT \"abc"));
        assert!(refused("SELECT /* abc"));
        assert!(refused("SELECT [abc"));
    }

    #[test]
    fn params_bind_positionally_and_quote() {
        let toks = check_read_only("SELECT * FROM t WHERE a = ? AND b = ?").unwrap();
        let out = substitute(
            "SELECT * FROM t WHERE a = ? AND b = ?",
            &toks,
            &[json!(7), json!("o'brien")],
        )
        .unwrap();
        assert_eq!(out, "SELECT * FROM t WHERE a = 7 AND b = 'o''brien'");
        // Scalars only, and the count must match.
        assert!(substitute(
            "SELECT ?",
            &check_read_only("SELECT ?").unwrap(),
            &[json!(null)]
        )
        .is_ok());
        assert!(substitute(
            "SELECT ?",
            &check_read_only("SELECT ?").unwrap(),
            &[json!(true)]
        )
        .unwrap()
        .contains('1'));
        assert!(substitute(
            "SELECT ?",
            &check_read_only("SELECT ?").unwrap(),
            &[json!({"a": 1})]
        )
        .is_err());
        assert!(substitute(
            "SELECT ?",
            &check_read_only("SELECT ?").unwrap(),
            &[json!(1), json!(2)]
        )
        .is_err());
        assert!(substitute(
            "SELECT ? AND ?",
            &check_read_only("SELECT ? AND ?").unwrap(),
            &[json!(1)]
        )
        .is_err());
        assert!(check_read_only("SELECT * FROM t WHERE a = ?1").is_err());
    }
}
