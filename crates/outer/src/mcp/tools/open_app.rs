//! The `open_app` orientation call (spec §8.2): one compact snapshot of an
//! app — head sha, role-tagged tree, file-based routes, D1 tables parsed
//! from the committed migrations, bindings, brand, AGENTS.md digest, last
//! deploy, open errors, quota left and `next` hints — so an agent can
//! orient in one call and then read with `read_files` (batched ranges),
//! `search_code` and `commit` edits.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    authed, change_limit, changes_used_today, check_rev, internal, latest_deploy, ok,
    resets_at_utc, visible_app, Ctx,
};
use crate::git::local;
use crate::mcp::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OpenAppInput {
    /// The app slug.
    pub app: String,
    /// A commit sha (default: the tip of main).
    pub sha: Option<String>,
    /// Only files changed since this sha (a commit sha on main): the tree
    /// lists just those, each with `status` added/modified/deleted.
    pub changed_since: Option<String>,
}

/// Most files the tree reports: ls-tree order is already sorted, this only
/// caps hostile repos before the per-file work starts.
const MAX_TREE_FILES: usize = 300;
/// First-N-lines cap for the AGENTS.md digest, in lines and chars.
const MAX_RULE_LINES: usize = 40;
const MAX_RULE_CHARS: usize = 2000;
/// Largest serialized `brand.json` the snapshot carries: bigger files are
/// replaced by a truncation marker, so one hostile blob cannot bloat the
/// orientation call.
const MAX_BRAND_CHARS: usize = 2000;
/// Most migration files parsed per call.
const MAX_MIGRATION_FILES: usize = 50;

/// The file's role, from its path and both templates' conventions (Ilha +
/// Void `pages/*.tsx`, `*.server.ts`, `pages/layout.tsx`, `middleware/`,
/// `db/migrations/*.sql`; Astro `src/pages`, `src/islands`, `src/layouts`,
/// `src/content`).
pub fn role_for(path: &str) -> &'static str {
    let file = path.rsplit('/').next().unwrap_or(path);
    if file.starts_with('_') && path.starts_with("pages/") {
        return "other";
    }
    // Test files before the page rules: `pages/x.test.tsx` is a test.
    if is_test_file(file) {
        return "test";
    }
    if path.starts_with("src/islands/")
        || (path.starts_with("pages/") && file.ends_with(".island.tsx"))
    {
        return "island";
    }
    if path == "pages/layout.tsx" || path.starts_with("src/layouts/") {
        return "layout";
    }
    if path.ends_with(".server.ts") || path.ends_with(".server.js") {
        return "server";
    }
    if path.starts_with("middleware/") {
        return "middleware";
    }
    if path.starts_with("db/migrations/") && file.ends_with(".sql") {
        return "migration";
    }
    if path.starts_with("src/pages/") {
        return "page";
    }
    if path.starts_with("pages/")
        && (file.ends_with(".tsx") || file.ends_with(".ts"))
        && !file.starts_with('_')
    {
        return "page";
    }
    if path.starts_with("src/content/") {
        return "content";
    }
    if file == "package.json"
        || file == "brand.json"
        || file == "tsconfig.json"
        || file.ends_with(".config.ts")
        || file.ends_with(".config.mjs")
        || file.ends_with(".config.js")
        || file == "wrangler.json"
        || file == "wrangler.jsonc"
        || file == "wrangler.toml"
    {
        return "config";
    }
    if file.ends_with(".css") {
        return "style";
    }
    if file.ends_with(".md") || file.ends_with(".mdx") {
        return "doc";
    }
    if file.ends_with(".png")
        || file.ends_with(".jpg")
        || file.ends_with(".jpeg")
        || file.ends_with(".gif")
        || file.ends_with(".webp")
        || file.ends_with(".svg")
        || file.ends_with(".ico")
        || path.starts_with("public/")
    {
        return "asset";
    }
    "other"
}

/// Paths the tree never reports: build output, installed dependencies,
/// lockfiles and the brand-generated icons (the templates' `scripts/brand.mjs`
/// draws `public/favicon.svg` from `brand.json`; never commit or read them).
fn hidden(path: &str) -> bool {
    if path.starts_with("dist/") || path.starts_with("node_modules/") {
        return true;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    if file == "bun.lock"
        || file == "package-lock.json"
        || file == "yarn.lock"
        || file == "pnpm-lock.yaml"
        || file == "Cargo.lock"
        || file == "Gemfile.lock"
        || file == "poetry.lock"
        || file.ends_with(".lock")
        || file.ends_with(".lock.json")
    {
        return true;
    }
    if path.starts_with("public/") {
        return file == "favicon.svg"
            || file == "og.svg"
            || file.starts_with("favicon-")
            || file.starts_with("apple-touch-icon")
            || file.starts_with("pwa-")
            || file.starts_with("android-")
            || file.starts_with("maskable-")
            || file.ends_with(".webmanifest");
    }
    false
}

/// Test files (`*.test.*`, `*.spec.*`), shared by the role and route rules.
fn is_test_file(file: &str) -> bool {
    file.ends_with(".test.tsx")
        || file.ends_with(".test.ts")
        || file.ends_with(".test.mjs")
        || file.ends_with(".test.js")
        || file.ends_with(".spec.ts")
        || file.ends_with(".spec.js")
}

/// A file-based route (URL path → repo file), or `None` for a file that is
/// never a route (`_`-prefixed partials, `*.server.ts` loaders, layouts,
/// test files, the Astro 404 page).
fn route_for(path: &str) -> Option<String> {
    if let Some(rel) = path.strip_prefix("pages/") {
        if rel == "layout.tsx" || rel.starts_with('_') || is_test_file(rel) {
            return None;
        }
        let stem = rel
            .strip_suffix(".tsx")
            .or_else(|| rel.strip_suffix(".ts"))?;
        if stem.ends_with(".server") || stem.starts_with('_') {
            return None;
        }
        return Some(ilha_route(stem));
    }
    if let Some(rel) = path.strip_prefix("src/pages/") {
        if rel == "404.astro" || is_test_file(rel) {
            return None;
        }
        // `foo.xml.ts` endpoints keep their `.xml`: `rss.xml.ts` → `/rss.xml`.
        let stem = if let Some(s) = rel.strip_suffix(".xml.ts") {
            format!("{s}.xml")
        } else {
            rel.strip_suffix(".astro")
                .or_else(|| rel.strip_suffix(".mdx"))
                .or_else(|| rel.strip_suffix(".md"))
                .or_else(|| rel.strip_suffix(".ts"))?
                .to_string()
        };
        // `foo.xml.ts` keeps its `.xml`: the strip above re-appends it.
        return Some(astro_route(&stem));
    }
    None
}

fn dynamic_segment(seg: &str) -> String {
    if let Some(inner) = seg.strip_prefix("[...").and_then(|s| s.strip_suffix(']')) {
        let _ = inner;
        return "*".to_string();
    }
    if let Some(inner) = seg.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return format!("<{inner}>");
    }
    seg.to_string()
}

fn ilha_route(stem: &str) -> String {
    if stem == "index" {
        return "/".to_string();
    }
    let parts: Vec<String> = stem.split('/').map(dynamic_segment).collect();
    let mut route = format!("/{}", parts.join("/"));
    if route.ends_with("/index") {
        route.truncate(route.len() - "/index".len());
        if route.is_empty() {
            route.push('/');
        }
    }
    route
}

// Endpoints included: `api/newsletter.ts` → `/api/newsletter`.
fn astro_route(stem: &str) -> String {
    astro_route_inner(stem)
}

fn astro_route_inner(stem: &str) -> String {
    if stem == "index" {
        return "/".to_string();
    }
    let parts: Vec<String> = stem.split('/').map(dynamic_segment).collect();
    let mut route = format!("/{}", parts.join("/"));
    if route.ends_with("/index") {
        route.truncate(route.len() - "/index".len());
        if route.is_empty() {
            route.push('/');
        }
    }
    route
}

/// D1 tables parsed from the committed migrations: `CREATE TABLE [IF NOT
/// EXISTS] name (col ..., ...)` plus `ALTER TABLE name ADD COLUMN col ...`.
/// The live DB is never touched.
fn parse_migrations(sql: &str, tables: &mut Vec<(String, Vec<String>)>) {
    let mut cleaned = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut in_string: Option<char> = None;
    while let Some(c) = chars.next() {
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                cleaned.push(c);
            }
            continue;
        }
        if in_block_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block_comment = false;
            }
            continue;
        }
        if let Some(q) = in_string {
            cleaned.push(c);
            if c == q {
                in_string = None;
            }
            continue;
        }
        if c == '-' && chars.peek() == Some(&'-') {
            chars.next();
            in_line_comment = true;
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_block_comment = true;
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            in_string = Some(c);
        }
        cleaned.push(c);
    }
    for stmt in cleaned.split(';') {
        let upper = stmt.to_uppercase();
        let trimmed = stmt.trim_start();
        let t_upper = trimmed.to_uppercase();
        if let Some(rest) = strip_kw(&t_upper, trimmed, "CREATE TABLE") {
            let rest = strip_kw_opt(rest, "IF NOT EXISTS");
            let name = ident(rest);
            if name.is_empty() {
                continue;
            }
            let Some(open) = rest.find('(') else { continue };
            let Some(close) = rest.rfind(')') else {
                continue;
            };
            let mut cols = Vec::new();
            for part in split_top_level(&rest[open + 1..close]) {
                let col = ident(part.trim_start());
                let cu = col.to_uppercase();
                if col.is_empty()
                    || cu == "PRIMARY"
                    || cu == "FOREIGN"
                    || cu == "UNIQUE"
                    || cu == "CHECK"
                    || cu == "CONSTRAINT"
                    || cu == "KEY"
                {
                    continue;
                }
                cols.push(col);
            }
            match tables.iter_mut().find(|(n, _)| n == &name) {
                Some((_, existing)) => {
                    for c in cols {
                        if !existing.iter().any(|e| e == &c) {
                            existing.push(c);
                        }
                    }
                }
                None => tables.push((name, cols)),
            }
        } else if let Some(rest) = strip_kw(&t_upper, trimmed, "ALTER TABLE") {
            // The remainder is measured from the consumed text (`ident` strips
            // quotes), so `name.len()` would slice mid-token on quoted or
            // multi-byte names.
            let (name, used) = split_ident(rest);
            let after = rest[used..].trim_start();
            let a_upper = after.to_uppercase();
            if let Some(after_add) = strip_kw(&a_upper, after, "ADD COLUMN") {
                let col = ident(after_add.trim_start());
                if !name.is_empty() && !col.is_empty() {
                    match tables.iter_mut().find(|(n, _)| n == &name) {
                        Some((_, existing)) => {
                            if !existing.iter().any(|e| e == &col) {
                                existing.push(col);
                            }
                        }
                        None => tables.push((name, vec![col])),
                    }
                }
            }
        }
        let _ = upper;
    }
}

/// Strip a leading keyword (already uppercased for compare) from the
/// original text, returning the rest past the keyword and whitespace.
fn strip_kw<'a>(upper: &str, orig: &'a str, kw: &str) -> Option<&'a str> {
    if !upper.starts_with(kw) {
        return None;
    }
    let rest = &orig[kw.len()..];
    // A keyword must end on a boundary: `CREATE TABLEX` is not a table.
    if let Some(c) = rest.chars().next() {
        if c.is_alphanumeric() || c == '_' {
            return None;
        }
    }
    Some(rest.trim_start())
}

fn strip_kw_opt<'a>(s: &'a str, kw: &str) -> &'a str {
    let upper = s.to_uppercase();
    strip_kw(&upper, s, kw).unwrap_or(s)
}

/// The next identifier plus the bytes it consumed: quoted (`"note"`,
/// `` `note` ``, `[note]`) and schema-qualified (`public.note`) names come
/// back unquoted, and every index is a char boundary, so multi-byte names
/// never panic.
fn split_ident(s: &str) -> (String, usize) {
    let trimmed = s.trim_start();
    let leading = s.len() - trimmed.len();
    let Some(first) = trimmed.chars().next() else {
        return (String::new(), s.len());
    };
    // Quoted name: consume through the matching close quote.
    if first == '"' || first == '\'' || first == '`' {
        if let Some(rel) = trimmed[1..].find(first) {
            let name = trimmed[1..1 + rel].to_string();
            return (name, leading + 1 + rel + 1);
        }
        return (String::new(), s.len());
    }
    if first == '[' {
        if let Some(rel) = trimmed[1..].find(']') {
            let name = trimmed[1..1 + rel].to_string();
            return (name, leading + 1 + rel + 1);
        }
        return (String::new(), s.len());
    }
    // Bare name: dotted segments may each be quoted (`public."note"`).
    let mut name = String::new();
    let mut pos = 0;
    loop {
        let rest = &trimmed[pos..];
        let (seg, used) = split_ident_segment(rest);
        if seg.is_empty() {
            break;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&seg);
        pos += used;
        if trimmed[pos..].starts_with('.') {
            // A trailing dot with nothing after it is not part of the name.
            if pos + 1 >= trimmed.len() {
                break;
            }
            pos += 1;
            // Dots must join segments: `note . x` ends the name at `note`.
            if trimmed[pos..].starts_with(char::is_whitespace) {
                break;
            }
            continue;
        }
        break;
    }
    (name, leading + pos)
}

/// One unquoted-or-bare segment and the bytes it consumed.
fn split_ident_segment(s: &str) -> (String, usize) {
    let Some(first) = s.chars().next() else {
        return (String::new(), 0);
    };
    if first == '"' || first == '\'' || first == '`' {
        if let Some(rel) = s[1..].find(first) {
            return (s[1..1 + rel].to_string(), 1 + rel + 1);
        }
        return (String::new(), 0);
    }
    if first == '[' {
        if let Some(rel) = s[1..].find(']') {
            return (s[1..1 + rel].to_string(), 1 + rel + 1);
        }
        return (String::new(), 0);
    }
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(s.len());
    (s[..end].to_string(), end)
}

/// The next identifier: strip quoting, stop at whitespace or punctuation.
fn ident(s: &str) -> String {
    split_ident(s).0
}

/// Split on commas that are not inside parens or quotes.
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_string: Option<char> = None;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if let Some(q) = in_string {
            if c == q {
                in_string = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => in_string = Some(c),
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth = (depth - 1).max(0),
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Bindings parsed from the committed Worker config (`void.config.ts`,
/// falling back to the Astro config and wrangler files): each `binding:
/// "NAME"` grouped by its block kind (`d1_databases`, `kv_namespaces`,
/// `r2_buckets`, `durable_objects`, `queues`), with the D1 database name
/// alongside when present.
fn parse_bindings(config: &str) -> Value {
    let kinds = [
        "d1_databases",
        "kv_namespaces",
        "r2_buckets",
        "durable_objects",
        "queues",
        "services",
        "hyperdrive",
        "analytics_engine_datasets",
    ];
    let mut out = json!({});
    for kind in kinds {
        let mut entries = Vec::new();
        let mut search_from = 0;
        while let Some(found) = config[search_from..].find(kind) {
            let abs = search_from + found;
            // The kind must be a key (`d1_databases:`), not prose. JSON
            // configs quote it (`"d1_databases":`), so tolerate one closing
            // quote before the colon.
            let after = strip_close_quote(&config[abs + kind.len()..]);
            if !after.starts_with(':') {
                search_from = abs + kind.len();
                continue;
            }
            // `after` is already past the colon and its whitespace: anchor
            // the bracket back to the config with pointer arithmetic.
            let bracket = match after.find('[') {
                Some(i) => after.as_ptr() as usize - config.as_ptr() as usize + i,
                None => {
                    search_from = abs + kind.len();
                    continue;
                }
            };
            // To the matching close bracket (entries nest objects, not arrays).
            let mut depth = 0i32;
            let mut end = None;
            for (i, c) in config[bracket..].char_indices() {
                if c == '[' {
                    depth += 1;
                } else if c == ']' {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(bracket + i);
                        break;
                    }
                }
            }
            let Some(end) = end else {
                search_from = abs + kind.len();
                continue;
            };
            let block = &config[bracket..end];
            for entry in split_top_level(&block[1..]) {
                let binding = capture_str(entry, "binding").or_else(|| {
                    (kind == "durable_objects")
                        .then(|| capture_str(entry, "name"))
                        .flatten()
                });
                let Some(binding) = binding else { continue };
                let mut obj = json!({"binding": binding});
                for key in [
                    "database_name",
                    "database_id",
                    "bucket_name",
                    "kv_name",
                    "queue",
                    "service",
                ] {
                    if let Some(v) = capture_str(entry, key) {
                        obj[key] = json!(v);
                    }
                }
                entries.push(obj);
            }
            search_from = end + 1;
        }
        if !entries.is_empty() {
            out[kind] = json!(entries);
        }
    }
    out
}

/// `key: "value"` (or `key: 'value'`) inside one config entry.
fn capture_str(entry: &str, key: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(found) = entry[search_from..].find(key) {
        let abs = search_from + found;
        let before_ok = abs == 0
            || !entry[..abs]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        // JSON configs quote the key (`"binding": "DB"`), so tolerate one
        // closing quote before the colon.
        let after = strip_close_quote(&entry[abs + key.len()..]);
        if before_ok && after.starts_with(':') {
            let val = after[1..].trim_start();
            let quote = val.chars().next()?;
            if quote == '"' || quote == '\'' {
                let end = val[1..].find(quote)?;
                return Some(val[1..1 + end].to_string());
            }
            return None;
        }
        search_from = abs + key.len();
    }
    None
}

/// Drop leading whitespace plus one optional closing quote (`"`, `'`),
/// so `"key":` matches as well as `key:`.
fn strip_close_quote(s: &str) -> &str {
    let t = s.trim_start();
    t.strip_prefix(['"', '\'']).unwrap_or(t)
}

/// The snapshot's `brand` value from the committed `brand.json`: absent
/// becomes `Null`; invalid JSON becomes a bounded error object; files whose
/// serialized form runs past [`MAX_BRAND_CHARS`] become a bounded truncation
/// marker. The whole file is never returned.
fn brand_value(text: Option<String>) -> Value {
    let Some(text) = text else {
        return Value::Null;
    };
    let Ok(value): Result<Value, _> = serde_json::from_str(&text) else {
        return json!({"error": "brand.json is not valid JSON"});
    };
    let serialized = serde_json::to_string(&value).unwrap_or_default();
    if serialized.len() <= MAX_BRAND_CHARS {
        return value;
    }
    let mut end = MAX_BRAND_CHARS;
    while !serialized.is_char_boundary(end) {
        end -= 1;
    }
    json!({
        "error": "brand.json exceeds 2000 characters",
        "preview": &serialized[..end],
    })
}

/// Read a small committed text file (`brand.json`, `AGENTS.md`, configs):
/// `None` when absent, an error only when git itself fails.
async fn read_small(
    bare: &std::path::Path,
    sha: &str,
    path: &str,
) -> Result<Option<String>, ToolError> {
    match local::read_blob(bare, Some(sha), path).await {
        Ok((_, bytes)) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") {
                Ok(None)
            } else {
                Err(internal(e))
            }
        }
    }
}

pub async fn open_app_impl(input: OpenAppInput, ctx: &Ctx<'_>) -> Result<Value, ToolError> {
    let (app, _) = visible_app(ctx, &input.app).await?;
    if let Some(rev) = input.sha.as_deref() {
        check_rev("sha", rev)?;
    }
    if let Some(rev) = input.changed_since.as_deref() {
        check_rev("changed_since", rev)?;
    }
    let bare = crate::git::manifest::ensure_synced(
        &ctx.state.store,
        &ctx.state.git,
        &ctx.state.sync,
        &app.slug,
    )
    .await
    .map_err(internal)?;
    let (head, mut files, list_truncated) =
        local::list_files(&bare, input.sha.as_deref(), None, 5000)
            .await
            .map_err(super::rev_err)?;
    files.retain(|f| !hidden(&f.path));
    let (_, counts) = local::line_counts(&bare, Some(&head))
        .await
        .map_err(internal)?;

    // `changed_since`: keep only files changed since that sha, with status.
    let mut statuses: Option<std::collections::HashMap<String, String>> = None;
    if let Some(since) = input.changed_since.as_deref() {
        // `resolve_rev` returns `Some` for explicit revs (or `UnknownRev`);
        // only a missing `main` yields `None`, which cannot happen here —
        // `head` above already resolved.
        let from = match local::resolve_rev(&bare, Some(since)).await {
            Ok(Some(sha)) => sha,
            Ok(None) => return Err(ToolError::not_found("unknown commit")),
            Err(e) => return Err(super::rev_err(e)),
        };
        let changed = local::changed_paths(&bare, &from, &head)
            .await
            .map_err(internal)?;
        let map: std::collections::HashMap<String, String> = changed
            .into_iter()
            .filter(|(p, _)| !hidden(p))
            .map(|(p, s)| {
                let status = match s {
                    'A' | 'C' => "added",
                    'D' => "deleted",
                    _ => "modified",
                };
                (p, status.to_string())
            })
            .collect();
        files.retain(|f| map.contains_key(&f.path));
        // Deleted files are gone from the head tree: re-add them by status.
        let mut deleted: Vec<String> = map
            .iter()
            .filter(|(_, s)| *s == "deleted")
            .map(|(p, _)| p.clone())
            .collect();
        deleted.sort();
        for p in deleted {
            files.push(local::FileEntry { path: p, size: 0 });
        }
        statuses = Some(map);
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    let truncated = list_truncated || files.len() > MAX_TREE_FILES;
    let files = &files[..files.len().min(MAX_TREE_FILES)];

    // Group by parent directory, files sorted: the compact nested tree.
    let mut dirs: Vec<(String, Vec<Value>)> = Vec::new();
    for f in files {
        let dir = f
            .path
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        let mut entry = json!({
            "path": f.path,
            "lines": counts.get(&f.path).copied(),
            "role": role_for(&f.path),
        });
        if let Some(map) = statuses.as_ref() {
            entry["status"] = json!(map
                .get(&f.path)
                .cloned()
                .unwrap_or_else(|| "modified".to_string()));
        }
        match dirs.iter_mut().find(|(d, _)| *d == dir) {
            Some((_, list)) => list.push(entry),
            None => dirs.push((dir, vec![entry])),
        }
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    let tree: Vec<Value> = dirs
        .into_iter()
        .map(|(dir, dir_files)| json!({"dir": dir, "files": dir_files}))
        .collect();

    let mut routes: Vec<Value> = Vec::new();
    for f in files {
        if let Some(route) = route_for(&f.path) {
            // Deleted files have no route at head... they may still map;
            // keep the mapping: it names where the route lived.
            routes.push(json!({"path": route, "file": f.path}));
        }
    }
    routes.sort_by(|a, b| {
        a["path"]
            .as_str()
            .unwrap_or("")
            .cmp(b["path"].as_str().unwrap_or(""))
    });

    // D1 tables from the committed migrations, never the live DB.
    let mut tables: Vec<(String, Vec<String>)> = Vec::new();
    let (_, mig_files, _) = local::list_files(&bare, Some(&head), Some("db/migrations"), 5000)
        .await
        .map_err(internal)?;
    let mut mig_files: Vec<String> = mig_files
        .into_iter()
        .map(|f| f.path)
        .filter(|p| p.ends_with(".sql"))
        .collect();
    mig_files.sort();
    for path in mig_files.into_iter().take(MAX_MIGRATION_FILES) {
        if let Some(sql) = read_small(&bare, &head, &path).await? {
            parse_migrations(&sql, &mut tables);
        }
    }
    let database = json!({
        "tables": tables.into_iter().map(|(name, columns)| json!({"name": name, "columns": columns})).collect::<Vec<_>>(),
    });

    // Bindings from the committed Worker config.
    let mut bindings = json!({});
    for config_path in [
        "void.config.ts",
        "astro.config.mjs",
        "wrangler.jsonc",
        "wrangler.json",
    ] {
        if let Some(text) = read_small(&bare, &head, config_path).await? {
            bindings = parse_bindings(&text);
            break;
        }
    }

    let brand = brand_value(read_small(&bare, &head, "brand.json").await?);

    let rules = match read_small(&bare, &head, "AGENTS.md").await? {
        Some(text) => {
            let lines: Vec<&str> = text.lines().take(MAX_RULE_LINES).collect();
            let mut digest = lines.join("\n");
            let cut = text.lines().count() > MAX_RULE_LINES || digest.len() > MAX_RULE_CHARS;
            if digest.len() > MAX_RULE_CHARS {
                let mut end = MAX_RULE_CHARS;
                while !digest.is_char_boundary(end) {
                    end -= 1;
                }
                digest.truncate(end);
            }
            if cut {
                digest.push_str("\n…");
            }
            json!({"path": "AGENTS.md", "digest": digest})
        }
        None => Value::Null,
    };

    let deploy = match latest_deploy(&ctx.state.pool, &app.id).await? {
        Some(row) => json!({
            "status": row.status,
            "url": ctx.state.app_url(&app.slug),
            "sha": row.sha,
        }),
        None => Value::Null,
    };
    let errors_open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM error_issue WHERE app_id = ?1 AND status = 'open'",
    )
    .bind(&app.id)
    .fetch_one(&ctx.state.pool)
    .await
    .map_err(internal)?;

    let caller = authed(ctx)?;
    let plan = super::plan_of(&ctx.state.pool, &caller.account_id).await?;
    let used = changes_used_today(&ctx.state.pool, &caller.account_id).await?;
    let quota = json!({
        "code_changes_used_today": used,
        "code_changes_per_day": change_limit(&plan),
        "resets_at": resets_at_utc(),
    });

    // Entry points first: one batched `read_files` for the index page and
    // server files plus the full AGENTS.md rules.
    let mut entries: Vec<String> = Vec::new();
    for f in files.iter().take(MAX_TREE_FILES) {
        let role = role_for(&f.path);
        if (role == "page" || role == "server")
            && (f.path.ends_with("index.tsx")
                || f.path.ends_with("index.astro")
                || f.path.ends_with("index.server.ts"))
        {
            entries.push(format!("{{path: {:?}}}", f.path));
            if entries.len() >= 2 {
                break;
            }
        }
    }
    entries.push("{path: \"AGENTS.md\"}".to_string());
    let mut next = vec![format!(
        "read_files {{app, files: [{}]}} reads the entry points and the full rules in one call",
        entries.join(", ")
    )];
    if truncated {
        next.push(
            "list_files {app, path_prefix} to page the tree: open_app truncated it".to_string(),
        );
    } else {
        next.push(
            "search_code {app, query} to find code; stage {app, ...} then check {app} then ship {app, message} to change it (commit {app, line_edits} ships at once)".to_string(),
        );
    }
    next.push(
        "app_metrics {app} shows requests, visitors, errors and latency; db_query {app, sql} reads live D1 rows (single SELECT only)".to_string(),
    );

    let summary = format!("{} at {}.", app.slug, &head[..head.len().min(12)]);
    Ok(ok(
        summary,
        json!({
            "app": app.slug,
            "head": head,
            "tree": tree,
            "truncated": truncated,
            "routes": routes,
            "database": database,
            "bindings": bindings,
            "brand": brand,
            "rules": rules,
            "deploy": deploy,
            "errors_open": errors_open,
            "quota": quota,
            "next": next,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables_of(sql: &str) -> Vec<(String, Vec<String>)> {
        let mut tables = Vec::new();
        parse_migrations(sql, &mut tables);
        tables
    }

    #[test]
    fn alter_table_quoted_names_add_columns() {
        for (sql, table, col) in [
            (r#"ALTER TABLE "note" ADD COLUMN x TEXT"#, "note", "x"),
            ("ALTER TABLE `note` ADD COLUMN y TEXT", "note", "y"),
            ("ALTER TABLE [note] ADD COLUMN z TEXT", "note", "z"),
        ] {
            let tables = tables_of(sql);
            assert_eq!(tables.len(), 1, "sql: {sql}");
            assert_eq!(tables[0].0, table);
            assert_eq!(tables[0].1, vec![col.to_string()]);
        }
    }

    #[test]
    fn alter_table_multibyte_name_does_not_panic() {
        let tables = tables_of(r#"ALTER TABLE "café" ADD COLUMN x TEXT"#);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].0, "café");
        assert_eq!(tables[0].1, vec!["x".to_string()]);
        // Quoted CREATE TABLE plus quoted ALTER on the same table merge.
        let tables = tables_of(
            "CREATE TABLE \"café\" (id TEXT);\nALTER TABLE \"café\" ADD COLUMN \"naïve\" TEXT",
        );
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].0, "café");
        assert_eq!(tables[0].1, vec!["id".to_string(), "naïve".to_string()]);
    }

    #[test]
    fn json_config_bindings_parse() {
        let config = "{\"d1_databases\": [{\"binding\": \"DB\", \"database_name\": \"app-db\"}]}";
        let bindings = parse_bindings(config);
        let dbs = bindings["d1_databases"].as_array().unwrap();
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0]["binding"], "DB");
        assert_eq!(dbs[0]["database_name"], "app-db");
        // The unquoted TS form still parses.
        let ts = "d1_databases: [{binding: 'DB2', database_name: 'x'}]";
        let bindings = parse_bindings(ts);
        assert_eq!(bindings["d1_databases"][0]["binding"], "DB2");
    }

    #[test]
    fn brand_value_bounds_large_and_invalid() {
        assert_eq!(brand_value(None), Value::Null);
        assert_eq!(
            brand_value(Some("not json{".to_string())),
            json!({"error": "brand.json is not valid JSON"})
        );
        let big = serde_json::json!({"name": "x".repeat(5000)});
        let branded = brand_value(Some(serde_json::to_string(&big).unwrap()));
        assert_eq!(branded["error"], "brand.json exceeds 2000 characters");
        let preview = branded["preview"].as_str().unwrap();
        assert!(preview.len() <= MAX_BRAND_CHARS);
        assert!(serde_json::to_string(&branded).unwrap().len() <= MAX_BRAND_CHARS + 128);
        // A small valid file passes through untouched.
        let small = serde_json::json!({"name": "Acme"});
        assert_eq!(
            brand_value(Some(serde_json::to_string(&small).unwrap())),
            small
        );
    }
}
