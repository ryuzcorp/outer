//! Text extraction for the pre-live gate (spec §15.1).
//!
//! What visitors can read: JSX text, string literals and template-literal
//! quasis from `.ts/.tsx/.js/.jsx/.mjs` (parsed with `oxc_parser`, with
//! line numbers); content files (`.html`, `.md`, `.txt`, `.json` under
//! `public/` plus `manifest.webmanifest`, SVG `<text>`, string literals in
//! SQL migrations/seed files); the visible text of
//! `dist/client/**/*.html`; and app metadata (slug + name).
//!
//! Skipped: `node_modules/`, dot-dirs, build output, test files, comments
//! and identifiers. Only copy-like strings count: a word of at least two
//! letters, not an import specifier, URL or CSS class list.
//!
//! Caps (spec §15.1): at most 3,000 files, 256 KiB each, 8 MiB of text.
//! Anything over is [`TooLarge`], which blocks the deploy as
//! `too_large_to_review`.
//!
//! Files are visited in sorted path order, so findings are deterministic.

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::Visit;
use oxc_span::{SourceType, Span};

/// One reviewable string: the deploying file, 1-based line, raw text.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// The deploy carries more text than the gate reviews.
#[derive(Debug, Clone)]
pub struct TooLarge {
    pub files: usize,
    pub bytes: u64,
}

/// Spec caps: file count, single-file size, total extracted text.
pub const MAX_FILES: usize = 3000;
pub const MAX_FILE_BYTES: u64 = 256 * 1024;
pub const MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;

const SCRIPT_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs"];

/// Directory names never descended into (plus any dot-dir).
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "build",
    "out",
    ".next",
    "coverage",
    ".turbo",
];

/// Collect every chunk under `work`, plus slug/name metadata chunks.
pub fn collect(work: &Path, slug: &str, name: Option<&str>) -> Result<Vec<Chunk>, TooLarge> {
    let mut paths = Vec::new();
    walk_sorted(work, &mut paths);
    let mut chunks = Vec::new();
    let mut bytes: u64 = 0;
    let mut files = 0usize;
    for rel in &paths {
        if files >= MAX_FILES {
            return Err(TooLarge { files, bytes });
        }
        let full = work.join(rel);
        let meta = match std::fs::metadata(&full) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > MAX_FILE_BYTES {
            return Err(TooLarge {
                files: files + 1,
                bytes,
            });
        }
        let display = rel.replace('\\', "/");
        let Some(kind) = classify(&display) else {
            continue;
        };
        files += 1;
        let raw = match std::fs::read(&full) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if raw.contains(&0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&raw);
        let mut file_chunks = extract_file(&display, kind, &text);
        for c in &file_chunks {
            bytes += c.text.len() as u64;
            if bytes > MAX_TEXT_BYTES {
                return Err(TooLarge { files, bytes });
            }
        }
        chunks.append(&mut file_chunks);
    }
    chunks.push(Chunk {
        path: "slug".to_string(),
        line: 1,
        text: slug.to_string(),
    });
    if let Some(name) = name {
        chunks.push(Chunk {
            path: "name".to_string(),
            line: 1,
            text: name.to_string(),
        });
    }
    Ok(chunks)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Script,
    Html,
    Markdown,
    Text,
    Json,
    Svg,
    Sql,
    DistHtml,
}

/// Route a worktree-relative display path to its extractor, or `None` to
/// ignore it. `dist/` only counts under `dist/client/**/*.html`.
fn classify(display: &str) -> Option<Kind> {
    let lower = display.to_ascii_lowercase();
    if lower.starts_with("dist/") {
        return if lower.starts_with("dist/client/") && lower.ends_with(".html") {
            Some(Kind::DistHtml)
        } else {
            None
        };
    }
    if lower.ends_with(".svg") {
        return Some(Kind::Svg);
    }
    if lower.ends_with(".sql") {
        return Some(Kind::Sql);
    }
    if lower.ends_with(".html") {
        return Some(Kind::Html);
    }
    if lower.ends_with(".md") {
        return Some(Kind::Markdown);
    }
    if lower.ends_with(".txt") {
        return Some(Kind::Text);
    }
    if lower.ends_with(".json")
        && (lower.starts_with("public/")
            || lower == "manifest.webmanifest"
            || lower.ends_with("/manifest.webmanifest"))
    {
        return Some(Kind::Json);
    }
    let ext = lower.rsplit('.').next().unwrap_or("");
    if SCRIPT_EXTS.contains(&ext) && !is_test_file(&lower) {
        return Some(Kind::Script);
    }
    None
}

/// Test files never carry reviewable copy.
fn is_test_file(lower: &str) -> bool {
    lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.split('/').any(|c| {
            matches!(c, "test" | "tests" | "__tests__" | "__test__") || c.starts_with("test_")
        })
        || file_stem(lower).ends_with("_test")
}

fn file_stem(lower: &str) -> &str {
    lower
        .rsplit('/')
        .next()
        .unwrap_or(lower)
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(lower)
}

/// Depth-first walk in sorted order, pruning skipped dirs. Test-file and
/// dot-dir pruning happens here; extension routing in [`classify`].
fn walk_sorted(work: &Path, out: &mut Vec<String>) {
    let mut stack = vec![(work.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(_) => continue,
        };
        entries.sort();
        for path in entries.into_iter().rev() {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let child_rel = if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            };
            if path.is_dir() {
                if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                    continue;
                }
                if child_rel == "dist" || child_rel.starts_with("dist/") {
                    // Only dist/client/** is reviewable; prune other dist dirs.
                    // ("dist" itself must stay traversable.)
                    if child_rel != "dist"
                        && child_rel != "dist/client"
                        && !child_rel.starts_with("dist/client/")
                    {
                        continue;
                    }
                }
                if child_rel
                    .split('/')
                    .any(|c| matches!(c, "test" | "tests" | "__tests__" | "__test__"))
                {
                    continue;
                }
                stack.push((path, child_rel));
            } else {
                out.push(child_rel);
            }
        }
    }
    out.sort();
}

fn extract_file(display: &str, kind: Kind, text: &str) -> Vec<Chunk> {
    match kind {
        Kind::Script => script_chunks(display, text),
        Kind::Html | Kind::DistHtml => html_chunks(display, text),
        Kind::Markdown | Kind::Text | Kind::Json => line_chunks(display, text),
        Kind::Svg => svg_chunks(display, text),
        Kind::Sql => sql_chunks(display, text),
    }
}

/// Plain content files: one chunk per non-empty line.
fn line_chunks(display: &str, text: &str) -> Vec<Chunk> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| Chunk {
            path: display.to_string(),
            line: (i + 1) as u32,
            text: l.trim().to_string(),
        })
        .collect()
}

/// Visible text per line: tags stripped, script/style/comments dropped.
fn html_chunks(display: &str, text: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut in_skip = false; // inside <script> or <style>
    let mut in_comment = false;
    for (i, raw) in text.lines().enumerate() {
        let mut line = raw.to_string();
        if in_comment {
            if let Some(end) = line.find("-->") {
                line = line[end + 3..].to_string();
                in_comment = false;
            } else {
                continue;
            }
        }
        while let Some(start) = line.find("<!--") {
            if let Some(end) = line[start..].find("-->") {
                line.replace_range(start..start + end + 3, " ");
            } else {
                line.truncate(start);
                in_comment = true;
                break;
            }
        }
        // Same-line <script>…</script> / <style>…</style> blocks are
        // stripped; multi-line ones skip lines until the close tag.
        line = strip_same_line_blocks(&line);
        let lower = line.to_ascii_lowercase();
        if lower.contains("<script") || lower.contains("<style") {
            in_skip = true;
            continue;
        }
        if in_skip {
            if lower.contains("</script") || lower.contains("</style") {
                in_skip = false;
                // Text after the close tag on the same line still counts.
                if let Some(pos) = lower.find("</script").or_else(|| lower.find("</style")) {
                    let rest = line[pos..]
                        .find('>')
                        .map(|i| pos + i + 1)
                        .unwrap_or(line.len());
                    line = line[rest..].to_string();
                } else {
                    continue;
                }
            } else {
                continue;
            }
        }
        let visible = strip_tags(&line);
        let decoded = decode_entities(&visible);
        if !decoded.trim().is_empty() {
            out.push(Chunk {
                path: display.to_string(),
                line: (i + 1) as u32,
                text: decoded.trim().to_string(),
            });
        }
    }
    out
}

fn strip_same_line_blocks(s: &str) -> String {
    let mut out = s.to_string();
    for (open, close) in [("<script", "</script>"), ("<style", "</style>")] {
        loop {
            let lower = out.to_ascii_lowercase();
            let Some(start) = lower.find(open) else {
                break;
            };
            let Some(end) = lower[start..].find(close) else {
                break;
            };
            out.replace_range(start..start + end + close.len(), " ");
        }
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// SVG `<text>` elements: one chunk each.
fn svg_chunks(display: &str, text: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut offset = 0;
    while let Some(open) = rest.find("<text") {
        let abs_open = offset + open;
        let after_tag = match rest[open..].find('>') {
            Some(i) => open + i + 1,
            None => break,
        };
        let Some(close) = rest[after_tag..].find("</text>") else {
            break;
        };
        let inner = strip_tags(&rest[after_tag..after_tag + close]);
        let decoded = decode_entities(&inner);
        if !decoded.trim().is_empty() {
            out.push(Chunk {
                path: display.to_string(),
                line: line_of(&line_starts(text), abs_open as u32) as u32,
                text: decoded.trim().to_string(),
            });
        }
        let consumed = after_tag + close + "</text>".len();
        rest = &rest[consumed..];
        offset = abs_open + consumed;
    }
    out
}

/// Single-quoted SQL string literals (`''` escapes), skipping `--` and
/// `/* */` comments. Double-quoted identifiers are not copy.
fn sql_chunks(display: &str, text: &str) -> Vec<Chunk> {
    let chars: Vec<char> = text.chars().collect();
    let starts = line_starts(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            '\'' => {
                let start = i;
                i += 1;
                let mut lit = String::new();
                while i < chars.len() {
                    if chars[i] == '\'' {
                        if chars.get(i + 1) == Some(&'\'') {
                            lit.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        lit.push(chars[i]);
                        i += 1;
                    }
                }
                let byte: usize = text.char_indices().nth(start).map(|(b, _)| b).unwrap_or(0);
                if is_copy_like(&lit) {
                    out.push(Chunk {
                        path: display.to_string(),
                        line: line_of(&starts, byte as u32) as u32,
                        text: lit,
                    });
                }
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// 1-based line number of `offset` (bytes) in `src`.
fn line_of(starts: &[usize], offset: u32) -> usize {
    starts.partition_point(|&s| s <= offset as usize).max(1)
}

fn line_starts(src: &str) -> Vec<usize> {
    let mut out = vec![0];
    for (i, c) in src.char_indices() {
        if c == '\n' {
            out.push(i + 1);
        }
    }
    out
}

/// Copy-like: a word of at least two letters, and not an import
/// specifier, URL, path or CSS class list.
fn is_copy_like(s: &str) -> bool {
    let t = s.trim();
    if !has_word2(t) || looks_like_url(t) || looks_like_path(t) || looks_like_class_list(t) {
        return false;
    }
    true
}

fn has_word2(s: &str) -> bool {
    let mut run = 0;
    for c in s.chars() {
        if c.is_alphabetic() {
            run += 1;
            if run >= 2 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

fn looks_like_url(s: &str) -> bool {
    s.contains("://") || s.starts_with("//") || s.starts_with('#') || starts_with_scheme(s)
}

fn starts_with_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return false;
        }
    }
    false
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || s.starts_with('.')
}

/// `flex items-center gap-2`: ≥2 tokens, all class-shaped, one with a
/// separator, none capitalized. Copy (`Add to cart`, `hello world`)
/// keeps its uppercase or its plain words.
fn looks_like_class_list(s: &str) -> bool {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.len() < 2 {
        return false;
    }
    tokens.iter().all(|t| {
        !t.is_empty()
            && t.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(
                        c,
                        '_' | '-' | ':' | '.' | '[' | ']' | '(' | ')' | '/' | '#' | '!' | '>' | '+'
                    )
            })
            && !t.chars().any(|c| c.is_uppercase())
    }) && tokens.iter().any(|t| t.contains(['-', ':', '.', '[']))
}

/// JSX text, string literals and template quasis via `oxc_parser`, with
/// line numbers. Comments and identifiers never reach the visitor;
/// import/export sources are recorded and skipped.
fn script_chunks(display: &str, text: &str) -> Vec<Chunk> {
    let allocator = Allocator::default();
    let Ok(source_type) = SourceType::from_path(display) else {
        return Vec::new();
    };
    let parsed = oxc_parser::Parser::new(&allocator, text, source_type).parse();
    if parsed.program.is_empty() {
        return Vec::new();
    }
    let starts = line_starts(text);
    let mut collector = Strings::new();
    collector.visit_program(&parsed.program);
    collector
        .raw
        .into_iter()
        .filter(|(_, s)| is_copy_like(s))
        .map(|(offset, s)| Chunk {
            path: display.to_string(),
            line: line_of(&starts, offset) as u32,
            text: s,
        })
        .collect()
}

struct Strings {
    raw: Vec<(u32, String)>,
    skip: Vec<Span>,
}

impl Strings {
    fn new() -> Self {
        Self {
            raw: Vec::new(),
            skip: Vec::new(),
        }
    }

    fn skipped(&self, span: Span) -> bool {
        self.skip
            .iter()
            .any(|s| s.start <= span.start && span.end <= s.end)
    }
}

impl<'a> Visit<'a> for Strings {
    fn visit_string_literal(&mut self, it: &StringLiteral<'a>) {
        if !self.skipped(it.span) {
            self.raw.push((
                it.span.start,
                it.value.as_str().unwrap_or_default().to_string(),
            ));
        }
    }

    fn visit_jsx_text(&mut self, it: &JSXText<'a>) {
        self.raw
            .push((it.span.start, it.value.as_str().to_string()));
    }

    fn visit_template_element(&mut self, it: &TemplateElement<'a>) {
        let cooked = it
            .value
            .cooked
            .as_ref()
            .and_then(|s| s.as_str())
            .unwrap_or_else(|| it.value.raw.as_str())
            .to_string();
        self.raw.push((it.span.start, cooked));
    }

    /// Directives (`"use client"`) are pragmas, not copy.
    fn visit_directive(&mut self, _it: &Directive<'a>) {}

    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        self.skip.push(it.source.span);
        oxc_ast_visit::walk::walk_import_declaration(self, it);
    }

    fn visit_export_from_declaration(&mut self, it: &ExportFromDeclaration<'a>) {
        self.skip.push(it.source.span);
        oxc_ast_visit::walk::walk_export_from_declaration(self, it);
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        self.skip.push(it.source.span);
        oxc_ast_visit::walk::walk_export_all_declaration(self, it);
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Expression::StringLiteral(lit) = &it.source {
            self.skip.push(lit.span);
        }
        oxc_ast_visit::walk::walk_import_expression(self, it);
    }

    fn visit_ts_import_type(&mut self, it: &TSImportType<'a>) {
        self.skip.push(it.source.span);
        oxc_ast_visit::walk::walk_ts_import_type(self, it);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        dir
    }

    fn texts(chunks: &[Chunk]) -> Vec<(&str, u32, &str)> {
        chunks
            .iter()
            .map(|c| (c.path.as_str(), c.line, c.text.as_str()))
            .collect()
    }

    #[test]
    fn tsx_strings_carry_lines_and_skip_imports_comments_identifiers() {
        let dir = worktree(&[(
            "pages/index.tsx",
            "import x from './lib';\nimport y from 'https://cdn.example.com/a.js';\n// a fuck comment\nconst label = 'hello';\nconst name = identifier;\nexport default () => <h1 className=\"flex items-center\">hi fuck</h1>;\nconst t = `yo ${name} shit`;\n",
        )]);
        let out = collect(dir.path(), "s", None).unwrap();
        let got = texts(&out);
        assert!(got.contains(&("pages/index.tsx", 4, "hello")), "{got:?}");
        assert!(got.contains(&("pages/index.tsx", 6, "hi fuck")), "{got:?}");
        assert!(got.contains(&("pages/index.tsx", 7, "yo ")), "{got:?}");
        assert!(got.contains(&("pages/index.tsx", 7, " shit")), "{got:?}");
        assert!(!got.iter().any(|(_, _, t)| t.contains("./lib")));
        assert!(!got.iter().any(|(_, _, t)| t.contains("comment")));
        assert!(!got.iter().any(|(_, _, t)| t.contains("items-center")));
        assert!(got.contains(&("slug", 1, "s")));
    }

    #[test]
    fn skips_node_modules_test_files_and_build_output() {
        let dir = worktree(&[
            ("node_modules/pkg/index.js", "const a = 'fuck';"),
            ("pages/a.test.ts", "const a = 'fuck';"),
            ("__tests__/b.ts", "const a = 'fuck';"),
            ("dist/bundle.js", "const a = 'fuck';"),
            (
                "dist/client/index.html",
                "<html><body><p>oh shit</p><script>const a = 'ignored';</script></body></html>",
            ),
            ("pages/ok.ts", "const a = 'clean';"),
        ]);
        let out = collect(dir.path(), "s", None).unwrap();
        let got = texts(&out);
        assert!(!got.iter().any(|(p, _, _)| p.starts_with("node_modules")));
        assert!(!got.iter().any(|(p, _, _)| p.contains(".test.")));
        assert!(!got.iter().any(|(p, _, _)| p.starts_with("__tests__")));
        assert!(!got.iter().any(|(p, _, _)| p == &"dist/bundle.js"));
        assert!(
            got.iter()
                .any(|(p, _, t)| p == &"dist/client/index.html" && t.contains("oh shit")),
            "{got:?}"
        );
        assert!(!got.iter().any(|(_, _, t)| t.contains("ignored")));
    }

    #[test]
    fn content_files_html_sql_svg_and_metadata() {
        let dir = worktree(&[
            ("public/copy.md", "# Title\n\nsome damn text\n"),
            ("public/manifest.webmanifest", "{\"name\": \"My App\"}"),
            (
                "migrations/0002_seed.sql",
                "-- a fuck comment\nINSERT INTO t (note) VALUES ('que porra');\n",
            ),
            ("assets/i.svg", "<svg><text>hi hell</text></svg>"),
        ]);
        let out = collect(dir.path(), "my-app", Some("My App")).unwrap();
        let got = texts(&out);
        assert!(
            got.contains(&("public/copy.md", 3, "some damn text")),
            "{got:?}"
        );
        assert!(
            got.contains(&("migrations/0002_seed.sql", 2, "que porra")),
            "{got:?}"
        );
        assert!(!got.iter().any(|(_, _, t)| t.contains("comment")));
        assert!(got
            .iter()
            .any(|(p, _, t)| p == &"assets/i.svg" && t.contains("hi hell")));
        assert!(got.contains(&("slug", 1, "my-app")));
        assert!(got.contains(&("name", 1, "My App")));
    }

    #[test]
    fn caps_block_as_too_large() {
        let dir = worktree(&[("pages/big.ts", &"x".repeat(300 * 1024))]);
        assert!(collect(dir.path(), "s", None).is_err());

        // 40 files × 300 KiB > 8 MiB, each under the per-file cap.
        let many: Vec<(String, String)> = (0..40)
            .map(|i| (format!("p/{i}.txt"), "y".repeat(300 * 1024)))
            .collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let dir = worktree(&refs);
        assert!(collect(dir.path(), "s", None).is_err());
    }

    #[test]
    fn too_many_files_block() {
        let many: Vec<(String, String)> = (0..3100)
            .map(|i| (format!("p/{i}.txt"), "hi".to_string()))
            .collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let dir = worktree(&refs);
        assert!(collect(dir.path(), "s", None).is_err());
    }
}
