//! Plan an example apply onto an app tree: the pure half of `add_example`.
//!
//! [`plan_apply`] takes the app's head tree (path → full text), the app's
//! template id, the known examples, and the requested names, and returns the
//! file writes for one commit — or a refusal naming what to fix by hand.
//! No filesystem or git here, so the TypeScript apply engine (slice T) and
//! this one stay byte-identical on the same inputs: every formatting rule is
//! pinned below and by the golden fixtures in `tests/fixtures/apply/`.
//!
//! Formatting rules (both engines must match exactly):
//!
//! - Nav items insert above the `// outer:nav-items` marker at the marker's
//!   own indentation, one per line:
//!   `{ href: "/photos", label: "Photos", icon: Camera },`.
//! - Nav icons merge into the braces on the `// outer:nav-icons` line,
//!   deduplicated and sorted byte-wise, joined with `", "`.
//! - Binding lines insert above the `// outer:bindings` marker at the
//!   marker's own indentation, one key per line:
//!   `r2_buckets: [{ binding: "PHOTOS", bucket_name: "photos" }],`.
//!   Object keys sort byte-wise; entries join with `", "`. Keys insert in
//!   byte-wise order.
//! - `package.json` is re-serialized whole: all keys sorted byte-wise,
//!   2-space indent, one trailing newline. New dependencies keep their given
//!   versions; an existing different version refuses.
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
    pub dependencies: BTreeMap<String, String>,
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
                "example {:?} is for template {:?}, not {:?}",
                example.name, example.template, app_template
            )));
        }
    }
    let fresh: Vec<&ExampleInput> = order
        .iter()
        .copied()
        .filter(|example| !is_applied(tree, example))
        .collect();
    if fresh.is_empty() {
        let names: Vec<&str> = order.iter().map(|example| example.name.as_str()).collect();
        return Err(refuse(format!(
            "{} {} already applied: every file matches, nothing to do",
            names.join(", "),
            if names.len() == 1 { "is" } else { "are" },
        )));
    }
    let mut writes: BTreeMap<String, String> = BTreeMap::new();
    let mut nav: Vec<String> = Vec::new();
    // Migrations renumber against the app's highest existing number, shared
    // across every example in this apply.
    let mut next_migration = highest_migration(tree) + 1;
    for example in &fresh {
        copy_files(tree, example, &mut writes, &mut next_migration)?;
    }
    for example in &fresh {
        if !example.nav.is_empty() {
            if app_template != "app" {
                return Err(refuse(format!(
                    "example {:?} declares nav but the {:?} template has no nav file",
                    example.name, app_template
                )));
            }
            // Earlier examples in this apply win the read: prefer the
            // accumulated writes over the head tree.
            let current = writes
                .get("pages/_nav.ts")
                .or_else(|| tree.get("pages/_nav.ts"))
                .ok_or_else(|| refuse("pages/_nav.ts is missing: restore it, then retry"))?;
            let (updated, inserted) = apply_nav(current, &example.nav, &example.name)?;
            if !inserted.is_empty() {
                writes.insert("pages/_nav.ts".to_string(), updated);
                nav.extend(inserted);
            }
        }
        if !example.bindings.is_empty() {
            let current = writes
                .get("void.config.ts")
                .or_else(|| tree.get("void.config.ts"))
                .ok_or_else(|| refuse("void.config.ts is missing: restore it, then retry"))?;
            let updated = apply_bindings(current, &example.bindings, &example.name)?;
            writes.insert("void.config.ts".to_string(), updated);
        }
        if !example.dependencies.is_empty() {
            let current = writes
                .get("package.json")
                .or_else(|| tree.get("package.json"))
                .ok_or_else(|| refuse("package.json is missing: restore it, then retry"))?;
            let updated = apply_package_json(current, &example.dependencies)?;
            writes.insert("package.json".to_string(), updated);
        }
    }
    let files: Vec<String> = writes.keys().cloned().collect();
    Ok(ApplyPlan {
        added: fresh.iter().map(|example| example.name.clone()).collect(),
        files,
        nav,
        writes: writes.into_iter().collect(),
    })
}

/// Depth-first `requires` resolution: dependencies first, de-duplicated,
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
        depth: usize,
    ) -> Result<(), Refusal> {
        if seen.contains(name) {
            return Ok(());
        }
        if stack.contains(&name.to_string()) {
            let mut cycle = stack.clone();
            cycle.push(name.to_string());
            return Err(refuse(format!("requires cycle: {}", cycle.join(" -> "))));
        }
        if depth > 3 {
            return Err(refuse(format!(
                "requires chain for {name:?} is deeper than 3"
            )));
        }
        let example = known
            .get(name)
            .ok_or_else(|| refuse(format!("unknown example {name:?}")))?;
        stack.push(name.to_string());
        for dep in &example.requires {
            visit(dep, known, order, seen, stack, depth + 1)?;
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
        visit(name, known, &mut order, &mut seen, &mut stack, 0)?;
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
/// byte-identical.
fn copy_files(
    tree: &BTreeMap<String, String>,
    example: &ExampleInput,
    writes: &mut BTreeMap<String, String>,
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
                return Err(refuse(format!(
                    "{path} already exists with different content: move it aside or merge it by hand, then retry"
                )));
            }
        } else {
            writes.insert(path.clone(), text.clone());
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
        if let Some(have) = tree.get(&dest).or_else(|| writes.get(&dest)) {
            if have != text {
                return Err(refuse(format!(
                    "{dest} already exists with different content: move it aside or merge it by hand, then retry"
                )));
            }
        } else {
            writes.insert(dest, text.clone());
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
fn apply_nav(
    nav_ts: &str,
    items: &[NavItem],
    example: &str,
) -> Result<(String, Vec<String>), Refusal> {
    let manual = |marker: &str| {
        refuse(format!(
            "pages/_nav.ts has no {marker} marker (the agent restructured it): add the {example} nav items by hand — see the {example} README's \"Without add_example\" section"
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
                "{indent}{{ href: {:?}, label: {:?}, icon: {} }},",
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
            refuse(format!(
                "void.config.ts has no // outer:bindings marker (the agent restructured it): add the {example} bindings by hand — see the {example} README's \"Without add_example\" section"
            ))
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
                        "void.config.ts already declares {key} with a different value ({have}): the one-D1 / one-R2 cap allows a single entry — align it by hand, then retry"
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
            let fields: Vec<String> = entry.iter().map(|(k, v)| format!("{k}: {v:?}")).collect();
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

/// Merge `deps` into `package.json`'s `dependencies`. An existing different
/// version refuses. The file re-serializes whole: keys sorted, 2-space
/// indent, one trailing newline.
fn apply_package_json(
    package_json: &str,
    deps: &BTreeMap<String, String>,
) -> Result<String, Refusal> {
    let mut value: serde_json::Value = serde_json::from_str(package_json)
        .map_err(|err| refuse(format!("package.json is not valid JSON: {err}")))?;
    let Some(obj) = value.as_object_mut() else {
        return Err(refuse("package.json is not a JSON object".to_string()));
    };
    let entry = obj
        .entry("dependencies".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(current) = entry.as_object_mut() else {
        return Err(refuse(
            "package.json \"dependencies\" is not an object".to_string(),
        ));
    };
    for (name, version) in deps {
        match current.get(name) {
            None => {
                current.insert(name.clone(), serde_json::Value::String(version.clone()));
            }
            Some(have) => {
                if have.as_str() != Some(version.as_str()) {
                    return Err(refuse(format!(
                        "package.json already depends on {name}@{have} (want {version}): align it by hand, then retry",
                        have = have
                            .as_str()
                            .unwrap_or(&have.to_string())
                            .trim_matches('"')
                    )));
                }
            }
        }
    }
    let mut out = serde_json::to_string_pretty(&value)
        .map_err(|err| refuse(format!("package.json failed to serialize: {err}")))?;
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests;
