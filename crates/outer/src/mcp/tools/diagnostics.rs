//! Structured diagnostics for failed builds (spec §1.6): `check` and failed
//! `commit`/`ship` deploys return parsed diagnostics alongside the log tail
//! instead of raw text alone.
//!
//! Each parser is a pure function over captured tool output, so the unit
//! tests run them against real-format samples with no toolchain. The
//! combined entry point tries every parser and caps the result; callers
//! attach it to failed outcomes only.

use serde_json::json;
use serde_json::Value;

/// One agent-actionable build problem.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Diagnostic {
    pub file: String,
    pub line: Option<u64>,
    pub col: Option<u64>,
    pub code: Option<String>,
    pub severity: String,
    pub message: String,
    pub snippet: Option<String>,
}

impl Diagnostic {
    fn error(
        file: impl Into<String>,
        line: Option<u64>,
        col: Option<u64>,
        code: Option<String>,
        message: String,
    ) -> Self {
        Self {
            file: file.into(),
            line,
            col,
            code,
            severity: "error".to_string(),
            message: message.trim().to_string(),
            snippet: None,
        }
    }

    /// The JSON shape agents receive.
    pub fn json(&self) -> Value {
        let mut out = json!({
            "file": self.file,
            "severity": self.severity,
            "message": self.message,
        });
        if let Some(line) = self.line {
            out["line"] = json!(line);
        }
        if let Some(col) = self.col {
            out["col"] = json!(col);
        }
        if let Some(code) = &self.code {
            out["code"] = json!(code);
        }
        if let Some(snippet) = &self.snippet {
            out["snippet"] = json!(snippet);
        }
        out
    }
}

/// Most diagnostics one outcome carries; the rest stay in the log tail.
pub const MAX_DIAGNOSTICS: usize = 50;

/// Parse every supported tool's failures out of `log`.
pub fn parse_diagnostics(log: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    parse_tsc(&mut out, log);
    parse_oxlint_text(&mut out, log);
    parse_oxlint_json(&mut out, log);
    parse_esbuild(&mut out, log);
    parse_bun_test(&mut out, log);
    // First-seen order is build order (install, build, typecheck, lint,
    // tests); keep it, dedup identical rows.
    let mut seen = std::collections::HashSet::new();
    out.retain(|d| {
        seen.insert(format!(
            "{}|{:?}|{:?}|{:?}|{}",
            d.file, d.line, d.col, d.code, d.message
        ))
    });
    out.truncate(MAX_DIAGNOSTICS);
    out
}

/// `tsc --noEmit`: `src/app.ts(12,34): error TS2322: message` and the
/// `src/app.ts:12:34 - error TS2322: message` pretty form.
fn parse_tsc(out: &mut Vec<Diagnostic>, log: &str) {
    for line in log.lines() {
        let line = line.trim();
        // Pretty form first: `path:line:col - error TS1234: msg`.
        if let Some(d) = parse_tsc_pretty(line).or_else(|| parse_tsc_paren(line)) {
            out.push(d);
        }
    }
}

fn parse_tsc_pretty(line: &str) -> Option<Diagnostic> {
    let (loc, rest) = line.split_once(" - error ")?;
    let (file, line_no, col) = split_file_line_col(loc)?;
    let (code, message) = rest.split_once(": ")?;
    if !code.starts_with("TS") {
        return None;
    }
    Some(Diagnostic::error(
        file,
        Some(line_no),
        Some(col),
        Some(code.to_string()),
        message.to_string(),
    ))
}

fn parse_tsc_paren(line: &str) -> Option<Diagnostic> {
    let paren = line.find('(')?;
    let file = line[..paren].trim();
    if file.is_empty() || file.contains(' ') {
        return None;
    }
    let rest = &line[paren + 1..];
    let close = rest.find("): error ")?;
    let (line_no, col) = rest[..close].split_once(',')?;
    let after = &rest[close + 9..];
    let (code, message) = after.split_once(": ")?;
    if !code.starts_with("TS") {
        return None;
    }
    Some(Diagnostic::error(
        file,
        line_no.trim().parse().ok(),
        col.trim().parse().ok(),
        Some(code.to_string()),
        message.to_string(),
    ))
}

fn split_file_line_col(loc: &str) -> Option<(String, u64, u64)> {
    let loc = loc.trim();
    // `path:line:col` from the right: Windows drive letters never appear
    // in repo-relative sandbox paths, so two trailing numbers win.
    let mut parts = loc.rsplit(':');
    let col: u64 = parts.next()?.trim().parse().ok()?;
    let line_no: u64 = parts.next()?.trim().parse().ok()?;
    let file: Vec<&str> = parts.collect();

    let mut file = file.into_iter().rev().collect::<Vec<_>>().join(":");
    file = file.trim().to_string();
    if file.is_empty() || file.contains(' ') {
        return None;
    }
    Some((file, line_no, col))
}

/// oxlint default output, one diagnostic per `×` row with its location on
/// the following `╭─[path:line:col]` line:
///
/// ```text
///   × eslint(no-unused-vars): 'token' is defined but never used
///    ╭─[src/auth.ts:3:7]
/// ```
fn parse_oxlint_text(out: &mut Vec<Diagnostic>, log: &str) {
    let mut pending: Option<(String, String)> = None;
    // Flush a location-less row: it still counts, file unknown.
    let flush = |out: &mut Vec<Diagnostic>, pending: &mut Option<(String, String)>| {
        if let Some((code, message)) = pending.take() {
            out.push(Diagnostic::error(
                "(unknown)",
                None,
                None,
                Some(code),
                message,
            ));
        }
    };
    for line in log.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix('×')
            .or_else(|| trimmed.strip_prefix('x'))
        {
            flush(out, &mut pending);
            let rest = rest.trim();
            let (code, message) = match rest.split_once("): ") {
                Some((rule, msg)) => (Some(rule.trim_start_matches('(').to_string()), msg),
                None => (None, rest),
            };
            pending = Some((
                code.unwrap_or_else(|| "oxlint".to_string()),
                message.to_string(),
            ));
            continue;
        }
        if pending.is_some() {
            if let Some(bracket) = trimmed.strip_prefix("╭─[") {
                if let Some(loc) = bracket.strip_suffix(']') {
                    if let Some((file, line_no, col)) = split_file_line_col(loc) {
                        if let Some((code, message)) = pending.take() {
                            out.push(Diagnostic::error(
                                file,
                                Some(line_no),
                                Some(col),
                                Some(code),
                                message,
                            ));
                            continue;
                        }
                    }
                }
            }
            flush(out, &mut pending);
        }
    }
    flush(out, &mut pending);
}

/// oxlint `--format=json`: `{"diagnostics": [{"message",
/// "severity": "Error"|"Warning", "labels": [{"filename", "span":
/// {"start": {"line", "column"}}}, ...], "code": {"value": "rule"}}]}`.
/// Only the first label locates the diagnostic.
fn parse_oxlint_json(out: &mut Vec<Diagnostic>, log: &str) {
    let start = match log.find("\"diagnostics\"") {
        Some(i) => i,
        None => return,
    };
    // The JSON block is the brace-balanced object around the key.
    let bytes = log.as_bytes();
    let mut depth = 0i64;
    let mut obj_start = None;
    for (i, b) in bytes.iter().enumerate().take(start + 1) {
        if *b == b'{' {
            if depth == 0 {
                obj_start = Some(i);
            }
            depth += 1;
        } else if *b == b'}' {
            depth -= 1;
        }
    }
    let Some(from) = obj_start else { return };
    let mut end = None;
    depth = 0;
    for (i, b) in bytes.iter().enumerate().skip(from) {
        if *b == b'{' {
            depth += 1;
        } else if *b == b'}' {
            depth -= 1;
            if depth == 0 {
                end = Some(i + 1);
                break;
            }
        }
    }
    let Some(end) = end else { return };
    let parsed: Value = match serde_json::from_str(&log[from..end]) {
        Ok(v) => v,
        Err(_) => return,
    };
    let diagnostics = parsed
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for d in diagnostics {
        let message = d
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if message.is_empty() {
            continue;
        }
        let severity = match d.get("severity").and_then(Value::as_str) {
            Some("Warning") => "warning",
            _ => "error",
        }
        .to_string();
        let code = d
            .get("code")
            .and_then(|c| c.get("value"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let label = d
            .get("labels")
            .and_then(Value::as_array)
            .and_then(|l| l.first());
        let (file, line, col) = match label {
            Some(l) => (
                l.get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or("(unknown)")
                    .to_string(),
                l.get("span")
                    .and_then(|s| s.get("start"))
                    .and_then(|s| s.get("line"))
                    .and_then(Value::as_u64),
                l.get("span")
                    .and_then(|s| s.get("start"))
                    .and_then(|s| s.get("column"))
                    .and_then(Value::as_u64),
            ),
            None => ("(unknown)".to_string(), None, None),
        };
        out.push(Diagnostic {
            file,
            line,
            col,
            code,
            severity,
            message: message.trim().to_string(),
            snippet: None,
        });
    }
}

/// esbuild / Vite build errors: `✘ [ERROR] message` rows followed by a
/// `    file:line:col:` location line:
///
/// ```text
/// ✘ [ERROR] Could not resolve "missing-dep"
///     src/main.ts:3:7:
/// ```
fn parse_esbuild(out: &mut Vec<Diagnostic>, log: &str) {
    let mut pending: Option<String> = None;
    for line in log.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("✘ [ERROR]") {
            if let Some(message) = pending.take() {
                out.push(Diagnostic::error("(unknown)", None, None, None, message));
            }
            pending = Some(rest.trim().to_string());
            continue;
        }
        if pending.is_some() {
            // Location lines read `path:line:col:` with a trailing colon.
            let loc = trimmed.trim_end_matches(':');
            if let Some((file, line_no, col)) = split_file_line_col(loc) {
                out.push(Diagnostic::error(
                    file,
                    Some(line_no),
                    Some(col),
                    None,
                    pending.take().unwrap_or_default(),
                ));
            }
        }
    }
    if let Some(message) = pending.take() {
        out.push(Diagnostic::error("(unknown)", None, None, None, message));
    }
}

/// `bun test` failures: `error: <message>` rows with the failing file on a
/// preceding `(fail)` line:
///
/// ```text
/// (fail) my test > adds one [12.3ms]
/// error: expect(received).toBe(expected)
/// ```
///
/// The file comes from the most recent `path.test.*:` header line.
fn parse_bun_test(out: &mut Vec<Diagnostic>, log: &str) {
    let mut file = "(unknown)".to_string();
    let mut lines = log.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.ends_with(".test.ts:")
            || trimmed.ends_with(".test.tsx:")
            || trimmed.ends_with(".test.js:")
        {
            file = trimmed.trim_end_matches(':').to_string();
            continue;
        }
        if let Some(message) = trimmed.strip_prefix("error:") {
            let message = message.trim().to_string();
            if message.is_empty() {
                continue;
            }
            // Skip the summary countdown (`0 pass, 1 fail`) which is not a
            // diagnostic: it has no test context.
            let mut snippet = None;
            if let Some(next) = lines.peek() {
                let next = next.trim();
                if next.starts_with("at ") {
                    snippet = Some(next.to_string());
                }
            }
            let mut d = Diagnostic::error(file.clone(), None, None, None, message);
            d.snippet = snippet;
            out.push(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsc_paren_form_parses() {
        let log = "src/routes/index.tsx(12,34): error TS2322: Type 'number' is not assignable to type 'string'.\n";
        let ds = parse_diagnostics(log);
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].file, "src/routes/index.tsx");
        assert_eq!(ds[0].line, Some(12));
        assert_eq!(ds[0].col, Some(34));
        assert_eq!(ds[0].code.as_deref(), Some("TS2322"));
        assert!(ds[0].message.contains("not assignable"));
    }

    #[test]
    fn tsc_pretty_form_parses() {
        let log =
            "src/lib/db.ts:7:3 - error TS2561: Object literal may only specify known properties.\n";
        let ds = parse_diagnostics(log);
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].file, "src/lib/db.ts");
        assert_eq!(ds[0].line, Some(7));
        assert_eq!(ds[0].col, Some(3));
        assert_eq!(ds[0].code.as_deref(), Some("TS2561"));
    }

    #[test]
    fn oxlint_text_form_parses() {
        let log = "  × eslint(no-unused-vars): 'token' is defined but never used\n   ╭─[src/auth.ts:3:7]\n";
        let ds = parse_diagnostics(log);
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].file, "src/auth.ts");
        assert_eq!(ds[0].line, Some(3));
        assert_eq!(ds[0].col, Some(7));
        assert!(ds[0].code.as_deref().unwrap().contains("no-unused-vars"));
    }

    #[test]
    fn oxlint_json_form_parses() {
        let log = r#"{"diagnostics": [{"message": "'x' is never reassigned. Use 'const' instead.", "severity": "Warning", "code": {"value": "prefer-const"}, "labels": [{"filename": "src/a.ts", "span": {"start": {"line": 4, "column": 5}}}]}]}"#;
        let ds = parse_diagnostics(log);
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].file, "src/a.ts");
        assert_eq!(ds[0].line, Some(4));
        assert_eq!(ds[0].severity, "warning");
        assert_eq!(ds[0].code.as_deref(), Some("prefer-const"));
    }

    #[test]
    fn esbuild_error_parses() {
        let log = "✘ [ERROR] Could not resolve \"missing-dep\"\n    src/main.ts:3:7:\n";
        let ds = parse_diagnostics(log);
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].file, "src/main.ts");
        assert_eq!(ds[0].line, Some(3));
        assert!(ds[0].message.contains("missing-dep"));
    }

    #[test]
    fn bun_test_failure_parses() {
        let log = "src/lib/sum.test.ts:\n(fail) sum > adds [0.5ms]\nerror: expect(received).toBe(expected)\n";
        let ds = parse_diagnostics(log);
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].file, "src/lib/sum.test.ts");
        assert!(ds[0].message.contains("toBe"));
    }

    #[test]
    fn clean_log_parses_to_nothing() {
        assert!(parse_diagnostics("bun install ok\n✓ built in 1.2s\n 3 pass, 0 fail\n").is_empty());
    }
}
