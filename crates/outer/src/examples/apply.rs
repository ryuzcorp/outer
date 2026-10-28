//! Plan an example apply onto an app tree: the pure half of `add_example`.
//!
//! [`plan_apply`] takes the app's head tree (path → full text), the app's
//! template id, the known examples, and the requested names, and returns the
//! file writes for one commit — or a refusal naming what to fix by hand.
//! No filesystem or git here, so the TypeScript apply engine (slice T,
//! `scripts/apply-example.ts`) and this one stay byte-identical on the same
//! inputs: every formatting rule is pinned below and by the shared golden
//! fixtures in `tests/fixtures/apply/` (a verbatim copy of outer-templates'
//! `scripts/apply-fixtures/`), including every refusal message byte for byte.
//!
//! Formatting rules (both engines must match exactly):
//!
//! - Nav items insert above the `// outer:nav-items` marker at the marker's
//!   own indentation, one per line:
//!   `{ href: "/photos", label: "Photos", icon: Camera },`.
//! - Nav icons merge into the braces on the `// outer:nav-icons` line,
//!   deduplicated and sorted byte-wise, joined with `", "`. An href already
//!   in the file is skipped. Nav is app-only: a website apply with nav
//!   refuses.
//! - Binding lines insert above the `// outer:bindings` marker at the
//!   marker's own indentation, one key per line:
//!   `r2_buckets: [{ binding: "PHOTOS", bucket_name: "photos" }],`.
//!   Object keys sort byte-wise; entries join with `", "`. Keys insert in
//!   byte-wise order per example. A key already in the file skips when its
//!   normalised value is equal, refuses when it differs.
//! - `dependencies` is not supported: an `example.json` carrying it is
//!   rejected by the loader, and neither engine touches `package.json`
//!   (merging it needs order-preserving edits neither engine does, and
//!   re-serialising would reorder the user's file).
//! - Migrations (`db/migrations/NNNN_*.sql`) renumber to the next free
//!   numbers after the app's highest existing migration number, keeping
//!   their relative (byte-wise path) order. Numbers pad to at least 4
//!   digits (`1` → `0001`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

/// One nav entry an example declares in `example.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavItem {
    pub href: String,
    pub label: String,
    pub icon: String,
}

/// One example as the apply engine sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExampleInput {
    pub name: String,
    /// `"app"` or `"website"`.
    pub template: String,
    /// Other examples, applied first when missing.
    pub requires: Vec<String>,
    /// App-only nav entries.
    pub nav: Vec<NavItem>,
    /// `d1_databases` / `r2_buckets`: key → entries of string pairs.
    pub bindings: BTreeMap<String, Vec<BTreeMap<String, String>>>,
    /// App-relative path → full text, byte-wise sorted.
    pub files: Vec<(String, String)>,
}

/// What one successful apply writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyPlan {
    /// Newly applied example names, requires first.
    pub added: Vec<String>,
    /// Every written path, byte-wise sorted.
    pub files: Vec<String>,
    /// Nav hrefs inserted by this apply.
    pub nav: Vec<String>,
    /// Path → new full text.
    pub writes: Vec<(String, String)>,
}

/// A refusal: invalid input or a tree the engine will not touch. The message
/// names the file and the manual step; tools surface it as `invalid_argument`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Refusal {}

fn refuse(message: impl Into<String>) -> Refusal {
    Refusal(message.into())
}

pub fn plan_apply(
    tree: &BTreeMap<String, String>,
    app_template: &str,
    examples: &[ExampleInput],
    requested: &[&str],
) -> Result<ApplyPlan, Refusal> {
    let known: HashMap<&str, &ExampleInput> = examples
        .iter()
        .map(|example| (example.name.as_str(), example))
        .collect();
    for name in requested {
        if !known.contains_key(name) {
            let mut valid: Vec<&str> = known.keys().copied().collect();
            valid.sort_unstable();
            return Err(refuse(format!(
                "unknown example {name:?}; pick one of: {}",
                valid.join(", ")
            )));
        }
    }
    let order = resolve(requested, &known)?;
    for example in &order {
        if example.template != app_template {
            return Err(refuse(format!(
                "example {:?} is for the {} template and cannot apply to a {} app",
                example.name, example.template, app_template
            )));
        }
    }
    let fresh: Vec<&ExampleInput> = order
        .iter()
        .copied()
        .filter(|example| !is_applied(tree, example))
        .collect();
    if let Some(navved) = order.iter().find(|example| !example.nav.is_empty()) {
        if app_template != "app" {
            return Err(refuse(format!(
                "example {:?} declares nav but the {:?} template has no nav file",
                navved.name, app_template
            )));
        }
    }
    if fresh.is_empty() {
        let names: Vec<&str> = order.iter().map(|example| example.name.as_str()).collect();
        return Err(refuse(format!(
            "{} {} already applied: every file matches, nothing to do",
            names.join(", "),
            if names.len() == 1 { "is" } else { "are" },
        )));
    }
    let mut writes: BTreeMap<String, String> = BTreeMap::new();
    let mut sources: HashMap<String, String> = HashMap::new();
    let mut nav: Vec<String> = Vec::new();
    // Migrations renumber against the app's highest existing number, shared
    // across every example in this apply.
    let mut next_migration = highest_migration(tree) + 1;
    for example in &fresh {
        copy_files(
            tree,
            example,
            &mut writes,
            &mut sources,
            &mut next_migration,
        )?;
    }
    // Nav first, then bindings: every fresh example reads the accumulated
    // writes, so shared hrefs insert once and shared keys compare equal.
    for example in &fresh {
        if example.nav.is_empty() {
            continue;
        }
        let current = writes
            .get("pages/_nav.ts")
            .or_else(|| tree.get("pages/_nav.ts"))
            .ok_or_else(|| {
                refuse(
                    "refusing to apply nav: pages/_nav.ts is missing (add the nav lines by hand — see the example README \"Without add_example\")",
                )
            })?;
        let (updated, inserted) = apply_nav(current, &example.nav)?;
        if !inserted.is_empty() {
            writes.insert("pages/_nav.ts".to_string(), updated);
            nav.extend(inserted);
        }
    }
    for example in &fresh {
        if example.bindings.is_empty() {
            continue;
        }
        let current = writes
            .get("void.config.ts")
            .or_else(|| tree.get("void.config.ts"))
            .ok_or_else(|| {
                refuse(
                    "refusing to apply bindings: void.config.ts is missing (add the bindings by hand — see the example README \"Without add_example\")",
                )
            })?;
        let updated = apply_bindings(current, &example.bindings, &example.name)?;
        writes.insert("void.config.ts".to_string(), updated);
    }
    let files: Vec<String> = writes.keys().cloned().collect();
    Ok(ApplyPlan {
        added: fresh.iter().map(|example| example.name.clone()).collect(),
        files,
        nav,
        writes: writes.into_iter().collect(),
    })
}

/// Depth-first `requires` resolution: requires first, de-duplicated,
/// request order kept. Refuses cycles and chains deeper than 3.
fn resolve<'a>(
    requested: &[&str],
    known: &HashMap<&str, &'a ExampleInput>,
) -> Result<Vec<&'a ExampleInput>, Refusal> {
    fn visit<'a>(
        name: &str,
        known: &HashMap<&str, &'a ExampleInput>,
        order: &mut Vec<&'a ExampleInput>,
        seen: &mut HashSet<String>,
        stack: &mut Vec<String>,
    ) -> Result<(), Refusal> {
        if seen.contains(name) {
            return Ok(());
        }
        if stack.contains(&name.to_string()) {
            let mut cycle = stack.clone();
            cycle.push(name.to_string());
            return Err(refuse(format!("requires cycle: {}", cycle.join(" -> "))));
        }
        if stack.len() >= 3 {
            let mut chain = stack.clone();
            chain.push(name.to_string());
            return Err(refuse(format!(
                "requires chain deeper than 3: {}",
                chain.join(" -> ")
            )));
        }
        let example = known.get(name).ok_or_else(|| {
            refuse(format!(
                "example {:?} requires unknown example {:?}",
                stack.last().map(String::as_str).unwrap_or(name),
                name
            ))
        })?;
        stack.push(name.to_string());
        for dep in &example.requires {
            visit(dep, known, order, seen, stack)?;
        }
        stack.pop();
        seen.insert(name.to_string());
        order.push(example);
        Ok(())
    }
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = Vec::new();
    for name in requested {
        visit(name, known, &mut order, &mut seen, &mut stack)?;
    }
    Ok(order)
}

/// An example counts as applied when every one of its non-migration files
/// already exists in the tree with identical content.
fn is_applied(tree: &BTreeMap<String, String>, example: &ExampleInput) -> bool {
    example
        .files
        .iter()
        .filter(|(path, _)| !is_migration_path(path))
        .all(|(path, text)| tree.get(path).is_some_and(|have| have == text))
}

fn is_migration_path(path: &str) -> bool {
    migration_number(path).is_some()
}

/// The leading number of a `db/migrations/NNNN_*.sql` path, if it is one.
fn migration_number(path: &str) -> Option<u64> {
    let rest = path.strip_prefix("db/migrations/")?;
    if rest.contains('/') || !rest.ends_with(".sql") {
        return None;
    }
    let stem = rest.strip_suffix(".sql")?;
    let (digits, _) = stem.split_once('_')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn highest_migration(tree: &BTreeMap<String, String>) -> u64 {
    tree.keys()
        .filter_map(|path| migration_number(path))
        .max()
        .unwrap_or(0)
}

/// Copy an example's files: migrations renumber, other paths must be new or
/// byte-identical. `sources` remembers which earlier example planned each
/// path, so a clash between two examples names both.
fn copy_files(
    tree: &BTreeMap<String, String>,
    example: &ExampleInput,
    writes: &mut BTreeMap<String, String>,
    sources: &mut HashMap<String, String>,
    next_migration: &mut u64,
) -> Result<(), Refusal> {
    let mut migrations: Vec<&(String, String)> = Vec::new();
    let mut plain: Vec<&(String, String)> = Vec::new();
    for file in &example.files {
        if is_migration_path(&file.0) {
            migrations.push(file);
        } else {
            plain.push(file);
        }
    }
    for (path, text) in plain {
        if let Some(have) = tree.get(path).or_else(|| writes.get(path)) {
            if have != text {
                if let Some(first) = sources.get(path) {
                    return Err(refuse(format!(
                        "refusing to apply {:?}: {path} is also shipped by {:?} with different content",
                        example.name, first
                    )));
                }
                return Err(refuse(format!(
                    "refusing to apply {:?}: {path} already exists with different content (delete or merge it by hand, then re-run)",
                    example.name
                )));
            }
        } else {
            writes.insert(path.clone(), text.clone());
            sources.insert(path.clone(), example.name.clone());
        }
    }
    migrations.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, text) in migrations {
        let suffix = path
            .strip_prefix("db/migrations/")
            .unwrap_or(path)
            .split_once('_')
            .map(|(_, rest)| rest)
            .unwrap_or(path.as_str());
        let dest = format!("db/migrations/{:04}_{suffix}", *next_migration);
        *next_migration += 1;
        if let Some(have) = writes.get(&dest) {
            if have != text {
                return Err(refuse(format!(
                    "refusing to apply {:?}: {dest} collides (also shipped by {:?})",
                    example.name,
                    sources.get(&dest).map(String::as_str).unwrap_or("?")
                )));
            }
        } else if let Some(have) = tree.get(&dest) {
            if have != text {
                return Err(refuse(format!(
                    "refusing to apply {:?}: {dest} already exists with different content (delete or merge it by hand, then re-run)",
                    example.name
                )));
            }
        } else {
            writes.insert(dest.clone(), text.clone());
            sources.insert(dest, example.name.clone());
        }
    }
    Ok(())
}

/// Is `name` a valid Lucide export for the nav-icons line.
pub fn is_valid_icon(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 2 || bytes.len() > 41 || !bytes[0].is_ascii_uppercase() {
        return false;
    }
    bytes.iter().all(|b| b.is_ascii_alphanumeric())
}

/// Merge `items` into `pages/_nav.ts` at its marker lines.
fn apply_nav(nav_ts: &str, items: &[NavItem]) -> Result<(String, Vec<String>), Refusal> {
    let manual = |marker: &str| {
        refuse(format!(
            "refusing to apply nav: pages/_nav.ts has no {marker} marker (the nav was restructured). Add the nav lines by hand — see the example README \"Without add_example\""
        ))
    };

    let icons_idx = nav_ts
        .lines()
        .position(|line| line.contains("outer:nav-icons"))
        .ok_or_else(|| manual("// outer:nav-icons"))?;
    let items_idx = nav_ts
        .lines()
        .position(|line| line.contains("outer:nav-items"))
        .ok_or_else(|| manual("// outer:nav-items"))?;
    let mut lines: Vec<String> = nav_ts.lines().map(str::to_string).collect();
    let trailing_newline = nav_ts.ends_with('\n');
    // Hrefs already in the file are skipped.
    let mut have_hrefs: HashSet<String> = HashSet::new();
    for line in &lines {
        for href in scan_hrefs(line) {
            have_hrefs.insert(href);
        }
    }
    let fresh: Vec<&NavItem> = items
        .iter()
        .filter(|item| !have_hrefs.contains(&item.href))
        .collect();
    if fresh.is_empty() {
        return Ok((nav_ts.to_string(), Vec::new()));
    }
    // Merge icons into the braces on the marker line, deduplicated + sorted.
    let icons_line = lines[icons_idx].clone();
    let (open, close) = (icons_line.find('{'), icons_line.find('}'));
    let (Some(open), Some(close)) = (open, close) else {
        return Err(manual("// outer:nav-icons"));
    };
    if open >= close {
        return Err(manual("// outer:nav-icons"));
    }
    let mut icons: HashSet<String> = icons_line[open + 1..close]
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    for item in &fresh {
        icons.insert(item.icon.clone());
    }
    let mut sorted: Vec<String> = icons.into_iter().collect();
    sorted.sort();
    lines[icons_idx] = format!(
        "{}{{ {} }}{}",
        &icons_line[..open],
        sorted.join(", "),
        &icons_line[close + 1..]
    );
    // Insert items above the marker at the marker's own indentation.
    let indent: String = lines[items_idx]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    let mut inserted = Vec::with_capacity(fresh.len());
    for (i, item) in fresh.iter().enumerate() {
        lines.insert(
            items_idx + i,
            format!(
                "{indent}{{ href: \"{}\", label: \"{}\", icon: {} }},",
                item.href, item.label, item.icon
            ),
        );
        inserted.push(item.href.clone());
    }
    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    Ok((out, inserted))
}

/// Scan a line for `href: "..."` values.
fn scan_hrefs(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(pos) = rest.find("href") {
        rest = &rest[pos + 4..];
        let value = rest.trim_start().strip_prefix(':');
        let Some(value) = value else { continue };
        let value = value.trim_start();
        if let Some(quoted) = value.strip_prefix('"') {
            if let Some(end) = quoted.find('"') {
                out.push(quoted[..end].to_string());
                rest = &quoted[end + 1..];
            } else {
                break;
            }
        }
    }
    out
}

/// Merge `bindings` into `void.config.ts` at its marker line. An existing
/// key wins: skipped when equal, refused when different.
fn apply_bindings(
    config: &str,
    bindings: &BTreeMap<String, Vec<BTreeMap<String, String>>>,
    example: &str,
) -> Result<String, Refusal> {
    let marker_idx = config
        .lines()
        .position(|line| line.contains("outer:bindings"))
        .ok_or_else(|| {
            refuse(
                "refusing to apply bindings: void.config.ts has no // outer:bindings marker (the config was restructured). Add the bindings by hand — see the example README \"Without add_example\"",
            )
        })?;
    let mut lines: Vec<String> = config.lines().map(str::to_string).collect();
    let trailing_newline = config.ends_with('\n');
    let indent: String = lines[marker_idx]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    let mut fresh: Vec<(String, String)> = Vec::new();
    for (key, entries) in bindings {
        let rendered = render_binding(key, entries);
        match find_key_value(&lines, key) {
            None => fresh.push((key.clone(), rendered)),
            Some(have) => {
                // Compare bracket-to-bracket: the stored value against the
                // rendered entries, ignoring surrounding whitespace.
                let want = rendered.split_once('[').map(|(_, rest)| rest).unwrap_or("");
                let want = want.trim_end_matches(',');
                if normalize(&have) != normalize(&format!("[{want}")) {
                    return Err(refuse(format!(
                        "refusing to apply {example:?}: void.config.ts already declares {key} with a different value (existing entry wins: {})",
                        have.trim()
                    )));
                }
            }
        }
    }
    fresh.sort_by(|a, b| a.0.cmp(&b.0));
    for (i, (_, rendered)) in fresh.iter().enumerate() {
        lines.insert(marker_idx + i, format!("{indent}{rendered}"));
    }
    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    Ok(out)
}

fn render_binding(key: &str, entries: &[BTreeMap<String, String>]) -> String {
    let parts: Vec<String> = entries
        .iter()
        .map(|entry| {
            let fields: Vec<String> = entry.iter().map(|(k, v)| format!("{k}: \"{v}\"")).collect();
            format!("{{ {} }}", fields.join(", "))
        })
        .collect();
    format!("{key}: [{}],", parts.join(", "))
}

/// The value after `key:` in `lines`, from its `[` to the matching `]`,
/// if the key appears at all.
fn find_key_value(lines: &[String], key: &str) -> Option<String> {
    let mut start: Option<(usize, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(pos) = find_key(line, key) {
            if line[pos..].contains('[') {
                start = Some((i, pos));
                break;
            }
        }
    }
    let (row, _) = start?;
    // Bracket-match from the first `[` on the key's line.
    let mut depth = 0;
    let mut begun = false;
    let mut value = String::new();
    for (offset, line) in lines[row..].iter().enumerate() {
        let from = if offset == 0 {
            lines[row].find('[').unwrap_or(0)
        } else {
            0
        };
        for c in line[from..].chars() {
            if c == '[' {
                depth += 1;
                begun = true;
            }
            if begun {
                value.push(c);
            }
            if c == ']' {
                depth -= 1;
                if begun && depth == 0 {
                    return Some(value);
                }
            }
        }
        if begun {
            value.push('\n');
        }
    }
    None
}

/// `key` as an object key on `line`: word-boundary match followed by `:`.
fn find_key(line: &str, key: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(pos) = line[from..].find(key) {
        let abs = from + pos;
        let before_ok = abs == 0
            || !line[..abs]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
        let after: Vec<char> = line[abs + key.len()..].chars().collect();
        let mut after = after.into_iter().skip_while(|c| c.is_whitespace());
        if before_ok && after.next() == Some(':') {
            return Some(abs);
        }
        from = abs + 1;
    }
    None
}

fn normalize(value: &str) -> String {
    value.chars().filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
mod tests;
