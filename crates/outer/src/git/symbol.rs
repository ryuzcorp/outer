//! Top-level symbol extraction for the `read_files` / `search_code` tools.
//!
//! `top_level_decls` parses a JS/TS source file with `oxc_parser` (already a
//! dependency via moderation) and returns every top-level function, class,
//! `const`/`let`/`var` declarator and TS type/interface/enum declaration with
//! its 1-based line span. `.astro` files resolve inside their frontmatter
//! fence. Anything else (unsupported extension, parse errors, no match, two
//! matches for one name) is a refused lookup with a plain message, never a
//! guess: the MCP layer reports it as a per-file error.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_span::SourceType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decl {
    pub name: String,
    pub start_line: u64,
    pub end_line: u64,
}

/// Why a symbol lookup failed. The MCP layer turns these into per-file
/// errors; none of them is the caller's whole-call bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolError {
    /// The extension has no supported parser (`foo.py`, `README.md`, ...).
    Unsupported(String),
    /// The file has no frontmatter fence (`.astro` only).
    NoFrontmatter,
    /// `oxc` reported syntax errors: extracting a span would be a guess.
    Parse(String),
    /// No top-level declaration binds `name`.
    NotFound(String),
    /// Two or more top-level declarations bind `name` (duplicate `const`s,
    /// function overloads): picking one would be a guess.
    Ambiguous(String, usize),
}

impl std::fmt::Display for SymbolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SymbolError::Unsupported(path) => {
                write!(f, "{path}: symbol extraction needs a .ts/.tsx/.js/.jsx/.mjs/.cjs/.mts/.cts file or .astro frontmatter")
            }
            SymbolError::NoFrontmatter => {
                write!(f, "no frontmatter fence (---) found for symbol extraction")
            }
            SymbolError::Parse(e) => write!(f, "could not parse the file: {e}"),
            SymbolError::NotFound(name) => {
                write!(
                    f,
                    "no top-level function, class, const or type named {name:?}"
                )
            }
            SymbolError::Ambiguous(name, n) => write!(
                f,
                "{n} top-level declarations named {name:?}; specify lines instead"
            ),
        }
    }
}

/// Byte offsets of every line start, for span → line mapping.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// 1-based line holding byte `offset`.
fn line_of(starts: &[usize], offset: usize) -> u64 {
    match starts.binary_search(&offset) {
        Ok(i) => (i + 1) as u64,
        Err(i) => i as u64,
    }
}

/// Split an `.astro` file into its frontmatter source and the 0-based line
/// offset where that source starts. `None` when the file has no fence.
fn astro_frontmatter(text: &str) -> Option<(&str, u64)> {
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end_matches(&['\n', '\r'][..]) != "---" {
        return None;
    }
    let mut end = first.len();
    for line in lines.by_ref() {
        end += line.len();
        if line.trim_end_matches(&['\n', '\r'][..]) == "---" {
            let body = &text[first.len()..end - line.len()];
            // The frontmatter body starts on file line 2 (1-based).
            return Some((body, 1));
        }
    }
    None
}

/// The parseable source for `path`: the whole file, or the frontmatter body
/// for `.astro`, with the 0-based file line offset of the source start.
fn source_for<'a>(path: &str, text: &'a str) -> Result<(&'a str, u64), SymbolError> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => Ok((text, 0)),
        "astro" => astro_frontmatter(text).ok_or(SymbolError::NoFrontmatter),
        _ => Err(SymbolError::Unsupported(path.to_string())),
    }
}

/// Every top-level declaration in the file with its line span.
pub fn top_level_decls(path: &str, text: &str) -> Result<Vec<Decl>, SymbolError> {
    let (source, line_offset) = source_for(path, text)?;
    let source_type = SourceType::from_path(format!("x.{}", display_ext(path)))
        .map_err(|_| SymbolError::Unsupported(path.to_string()))?;
    let allocator = Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, source, source_type).parse();
    if !parsed.diagnostics.is_empty() {
        let msg = parsed.diagnostics[0].message.to_string();
        let msg = msg.chars().take(200).collect::<String>();
        return Err(SymbolError::Parse(msg));
    }
    let starts = line_starts(source);
    let mut decls = Vec::new();
    let mut push = |name: &str, span: oxc_span::Span| {
        decls.push(Decl {
            name: name.to_string(),
            start_line: line_of(&starts, span.start as usize) + line_offset,
            end_line: line_of(&starts, span.end.max(span.start + 1) as usize - 1) + line_offset,
        });
    };
    for stmt in &parsed.program.body {
        match stmt {
            Statement::VariableDeclaration(decl) => {
                for d in &decl.declarations {
                    if let BindingPattern::BindingIdentifier(id) = &d.id {
                        push(id.name.as_str(), d.span);
                    }
                }
            }
            Statement::FunctionDeclaration(func) => {
                push(
                    func.id.as_ref().map_or("default", |id| id.name.as_str()),
                    func.span,
                );
            }
            Statement::ClassDeclaration(class) => {
                push(
                    class.id.as_ref().map_or("default", |id| id.name.as_str()),
                    class.span,
                );
            }
            Statement::TSTypeAliasDeclaration(decl) => push(decl.id.name.as_str(), decl.span),
            Statement::TSInterfaceDeclaration(decl) => push(decl.id.name.as_str(), decl.span),
            Statement::TSEnumDeclaration(decl) => push(decl.id.name.as_str(), decl.span),
            Statement::ExportDeclaration(export) => {
                push_export_decl(&mut push, &export.declaration);
            }
            Statement::ExportDefaultDeclaration(export) => match &export.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(func) => push(
                    func.id.as_ref().map_or("default", |id| id.name.as_str()),
                    func.span,
                ),
                ExportDefaultDeclarationKind::ClassDeclaration(class) => push(
                    class.id.as_ref().map_or("default", |id| id.name.as_str()),
                    class.span,
                ),
                ExportDefaultDeclarationKind::TSInterfaceDeclaration(decl) => {
                    push(decl.id.name.as_str(), decl.span);
                }
                // `export default <expr>` / `export default Foo`: not a
                // declaration site, so not a definition.
                _ => {}
            },
            // `export {a, b}` re-exports bindings declared elsewhere; the
            // declaration site is what we report, so these add nothing.
            _ => {}
        }
    }
    Ok(decls)
}

/// The extension `oxc` should parse as: real extensions pass through,
/// `.astro` frontmatter parses as TSX.
fn display_ext(path: &str) -> &str {
    if path.to_ascii_lowercase().ends_with(".astro") {
        "tsx"
    } else {
        path.rsplit('.').next().unwrap_or("tsx")
    }
}

fn push_export_decl(push: &mut impl FnMut(&str, oxc_span::Span), decl: &Declaration<'_>) {
    match decl {
        Declaration::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let BindingPattern::BindingIdentifier(id) = &d.id {
                    push(id.name.as_str(), d.span);
                }
            }
        }
        Declaration::FunctionDeclaration(func) => {
            push(
                func.id.as_ref().map_or("default", |id| id.name.as_str()),
                func.span,
            );
        }
        Declaration::ClassDeclaration(class) => {
            push(
                class.id.as_ref().map_or("default", |id| id.name.as_str()),
                class.span,
            );
        }
        Declaration::TSTypeAliasDeclaration(decl) => push(decl.id.name.as_str(), decl.span),
        Declaration::TSInterfaceDeclaration(decl) => push(decl.id.name.as_str(), decl.span),
        Declaration::TSEnumDeclaration(decl) => push(decl.id.name.as_str(), decl.span),
        _ => {}
    }
}

/// The single top-level declaration binding `name`, or the refusal.
pub fn extract_symbol(path: &str, text: &str, name: &str) -> Result<Decl, SymbolError> {
    let decls = top_level_decls(path, text)?;
    let mut hits = decls.iter().filter(|d| d.name == name);
    let first = hits
        .next()
        .ok_or_else(|| SymbolError::NotFound(name.to_string()))?;
    if hits.next().is_some() {
        let count = decls.iter().filter(|d| d.name == name).count();
        return Err(SymbolError::Ambiguous(name.to_string(), count));
    }
    Ok(first.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = r#"import { Island } from "@outer/ilha";
import { count } from "./store";

const PAGE_SIZE = 25;

export function Card({ title }: { title: string }) {
  return <Island><h1>{title}</h1></Island>;
}

export const formatTitle = (t: string) => t.trim();

export default Card;
"#;

    #[test]
    fn finds_functions_consts_and_classes() {
        let decls = top_level_decls("src/Card.tsx", CARD).unwrap();
        let names: Vec<_> = decls.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Card"));
        assert!(names.contains(&"formatTitle"));
        assert!(names.contains(&"PAGE_SIZE"));
        // `export default Card` is a re-export, not a second definition.
        assert_eq!(decls.iter().filter(|d| d.name == "Card").count(), 1);
    }

    #[test]
    fn symbol_lines_cover_the_declaration() {
        let decl = extract_symbol("src/Card.tsx", CARD, "Card").unwrap();
        assert_eq!(decl.start_line, 6);
        assert_eq!(decl.end_line, 8);
        let decl = extract_symbol("src/Card.tsx", CARD, "formatTitle").unwrap();
        assert_eq!(decl.start_line, 10);
        assert_eq!(decl.end_line, 10);
    }

    #[test]
    fn default_exported_anonymous_function_is_default() {
        let decls = top_level_decls("x.js", "export default function() {\nreturn 1;\n}\n").unwrap();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].name, "default");
    }

    #[test]
    fn astro_resolves_inside_frontmatter() {
        let text = "---\nimport Layout from \"../layouts/Base.astro\";\nconst title = \"Shop\";\n---\n<h1>{title}</h1>\n";
        let decl = extract_symbol("pages/index.astro", text, "title").unwrap();
        assert_eq!((decl.start_line, decl.end_line), (3, 3));
        assert!(matches!(
            extract_symbol("pages/index.astro", text, "h1"),
            Err(SymbolError::NotFound(_))
        ));
    }

    #[test]
    fn astro_without_frontmatter_is_refused() {
        assert!(matches!(
            top_level_decls("pages/x.astro", "<h1>hi</h1>\n"),
            Err(SymbolError::NoFrontmatter)
        ));
    }

    #[test]
    fn duplicates_are_ambiguous() {
        let text = "export const Item = 1;\nconst Item = 2;\n";
        assert!(matches!(
            extract_symbol("x.ts", text, "Item"),
            Err(SymbolError::Ambiguous(_, 2))
        ));
    }

    #[test]
    fn unsupported_extension_and_parse_errors_are_refused() {
        assert!(matches!(
            top_level_decls("a.py", "x = 1\n"),
            Err(SymbolError::Unsupported(_))
        ));
        assert!(matches!(
            top_level_decls("x.ts", "const = ;;;\n"),
            Err(SymbolError::Parse(_))
        ));
        assert!(matches!(
            extract_symbol("src/Card.tsx", CARD, "Missing"),
            Err(SymbolError::NotFound(_))
        ));
    }

    #[test]
    fn interface_and_type_alias_are_declarations() {
        let text =
            "export interface Props {\ntitle: string;\n}\nexport type Mode = \"a\" | \"b\";\n";
        let decls = top_level_decls("x.ts", text).unwrap();
        assert!(decls.iter().any(|d| d.name == "Props"));
        assert!(decls.iter().any(|d| d.name == "Mode"));
    }
}
