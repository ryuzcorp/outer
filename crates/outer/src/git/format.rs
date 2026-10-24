//! oxfmt-on-commit: format written/edited text files inside [`commit`](super::commit::commit).
//!
//! Agents edit apps only through the MCP `commit` tool, so they cannot run
//! the app's own formatter. Outer formats instead: after exact-match edits
//! apply and before the commit is built, every written/edited text file with
//! a formattable extension is piped through a platform-owned, pinned oxfmt
//! binary.
//!
//! Security: the app's code is never executed. oxfmt runs with stdin/stdout
//! pipes, `cwd` an empty temp dir, and a fully cleared environment, so it
//! discovers no config or code on disk: the only config it sees is the `-c`
//! file Outer writes from bytes read as data out of the commit's tree.
//! Ignore rules honored: the tree's `.oxfmtrc.json` `ignorePatterns` and the
//! tree's `.prettierignore`. `.gitignore` is NOT honored. Pattern syntax:
//! `*` (any run within a segment), `**` (any run across segments), `?` (one
//! character), a leading `/` or `./` is stripped, a trailing `/` matches the
//! whole directory, a pattern without `/` matches at any depth, and (in
//! `.prettierignore` only) a leading `!` negates with last-match-wins. No
//! character classes, no `\` escapes.
//!
//! Failure policy: a file oxfmt cannot parse, a timeout, or an oversized
//! output commits that file UNFORMATTED — never block the agent's commit.
//! The build reports syntax errors, not the commit path.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;

use super::commit::RawFile;
use super::manifest;
use crate::config::is_executable;

/// Extensions piped through oxfmt, verified against oxfmt 0.74.0
/// (`--stdin-filepath` mode): every one formats, everything else reports
/// "Unsupported file type" instead. Case-insensitive match on the path's
/// final extension.
pub const FORMATTABLE_EXTENSIONS: &[&str] = &[
    "js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts", "json", "jsonc", "css", "md",
];

/// Total formatting budget per commit. Every file run is killed at this
/// deadline, so a hung formatter delays the commit by at most this long.
pub const FORMAT_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Most concurrent oxfmt processes per commit.
pub const FORMAT_CONCURRENCY: usize = 4;

/// Largest formatted output accepted per file (inputs are already capped by
/// the commit caps; a formatter must never blow one up unboundedly).
pub const FORMAT_OUTPUT_CAP: usize = 1024 * 1024;

/// Cap on a config/ignore blob read from the tree. Real configs are tiny;
/// anything larger is treated as absent.
const TREE_CONFIG_CAP: usize = 1024 * 1024;

/// One file committed unformatted, with the reason the agent sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatSkip {
    pub path: String,
    pub reason: String,
}

/// What formatting did to one commit: paths whose content changed, plus the
/// files committed unformatted and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormatOutcome {
    pub formatted: Vec<String>,
    pub skipped: Vec<FormatSkip>,
}

/// The repo-relative paths of the tree files formatting reads as data.
const OXFMT_CONFIG_PATH: &str = ".oxfmtrc.json";
const PRETTIER_IGNORE_PATH: &str = ".prettierignore";

static MISSING_BIN_WARNED: AtomicBool = AtomicBool::new(false);

/// True when `path`'s final extension is one oxfmt formats.
pub(crate) fn is_formattable(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    let Some(ext) = file.rsplit('.').next() else {
        return false;
    };
    if !file.contains('.') || ext.is_empty() {
        return false;
    }
    FORMATTABLE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// Ignore patterns from `.oxfmtrc.json` `ignorePatterns` plus
/// `.prettierignore`, in that order (prettierignore last, so its `!`
/// negations can re-include).
#[derive(Debug, Default)]
pub(crate) struct IgnoreRules {
    patterns: Vec<Pattern>,
}

#[derive(Debug)]
struct Pattern {
    segments: Vec<String>,
    negated: bool,
}

impl IgnoreRules {
    pub(crate) fn parse(oxfmt_config: Option<&[u8]>, prettierignore: Option<&[u8]>) -> Self {
        let mut rules = IgnoreRules::default();
        if let Some(bytes) = oxfmt_config {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
                if let Some(patterns) = value.get("ignorePatterns").and_then(|v| v.as_array()) {
                    for raw in patterns.iter().filter_map(|v| v.as_str()) {
                        // oxfmtrc patterns don't negate: a `!` there is literal.
                        if let Some(pattern) = compile_pattern(raw) {
                            rules.patterns.push(pattern);
                        }
                    }
                }
            }
        }
        if let Some(bytes) = prettierignore {
            if let Ok(text) = std::str::from_utf8(bytes) {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let (negated, line) = match line.strip_prefix('!') {
                        Some(rest) => (true, rest),
                        None => (false, line),
                    };
                    if let Some(mut pattern) = compile_pattern(line) {
                        pattern.negated = negated;
                        rules.patterns.push(pattern);
                    }
                }
            }
        }
        rules
    }

    /// True when the last matching pattern ignores `path` (non-negated).
    pub(crate) fn matches(&self, path: &str) -> bool {
        let mut ignored = false;
        for pattern in &self.patterns {
            if pattern.matches(path) {
                ignored = !pattern.negated;
            }
        }
        ignored
    }
}

fn compile_pattern(raw: &str) -> Option<Pattern> {
    let mut text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix("./") {
        text = rest;
    }
    text = text.strip_prefix('/').unwrap_or(text);
    if text.is_empty() {
        return None;
    }
    let dir = text.strip_suffix('/').unwrap_or(text);
    if dir.is_empty() {
        return None;
    }
    let full = if dir.contains('/') {
        dir.to_string()
    } else {
        format!("**/{dir}")
    };
    // Collapse consecutive `**`: they match the same language and keep
    // adversarial patterns from blowing up the matcher.
    let mut segments = Vec::new();
    for segment in full.split('/') {
        if segment == "**" && segments.last().is_some_and(|s| s == "**") {
            continue;
        }
        segments.push(segment.to_string());
    }
    Some(Pattern {
        segments,
        negated: false,
    })
}

impl Pattern {
    fn matches(&self, path: &str) -> bool {
        let segments: Vec<&str> = path.split('/').collect();
        // A pattern also matches a directory's whole subtree: any leading
        // run of path segments may satisfy it (the gitignore dir rule), so
        // `keep/stuff` ignores `keep/stuff/a.ts` too.
        for width in 1..=segments.len() {
            if match_segments(&self.segments, &segments[..width]) {
                return true;
            }
        }
        false
    }
}

fn match_segments(pattern: &[String], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == "**" {
        for i in 0..=path.len() {
            if match_segments(&pattern[1..], &path[i..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    segment_match(&pattern[0], path[0]) && match_segments(&pattern[1..], &path[1..])
}

/// One-segment glob: `*` any run, `?` one character, else literal.
fn segment_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Format every eligible file in place, returning what changed and what was
/// committed unformatted. `bin` `None`, or a binary missing at runtime,
/// skips formatting entirely (empty outcome) with one warn: the commit still
/// works, which is the localhost/dev case.
pub(crate) async fn format_committed_files(
    bin: Option<&Path>,
    bare: &Path,
    head: Option<&str>,
    files: &mut [RawFile],
) -> FormatOutcome {
    let oxfmt_config = tree_file_content(bare, head, files, OXFMT_CONFIG_PATH).await;
    let prettierignore = tree_file_content(bare, head, files, PRETTIER_IGNORE_PATH).await;
    let rules = IgnoreRules::parse(oxfmt_config.as_deref(), prettierignore.as_deref());

    let mut candidates = Vec::new();
    for (index, file) in files.iter().enumerate() {
        if file.is_text && is_formattable(&file.path) && !rules.matches(&file.path) {
            candidates.push(index);
        }
    }
    if candidates.is_empty() {
        return FormatOutcome::default();
    }
    let Some(bin) = bin else {
        return FormatOutcome::default();
    };
    if !is_executable(bin) {
        warn_missing_once(bin);
        return FormatOutcome::default();
    }

    let scratch = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(_) => {
            return FormatOutcome {
                formatted: Vec::new(),
                skipped: candidates
                    .iter()
                    .map(|&index| FormatSkip {
                        path: files[index].path.clone(),
                        reason: "formatter unavailable".to_string(),
                    })
                    .collect(),
            };
        }
    };
    // The `-c` config: this commit's `.oxfmtrc.json` when it parses, else
    // oxfmt defaults from an empty config. Either way oxfmt discovers
    // nothing on disk: the scratch dir holds only this file.
    let config_bytes = match &oxfmt_config {
        Some(bytes) if serde_json::from_slice::<serde_json::Value>(bytes).is_ok() => bytes.clone(),
        _ => b"{}".to_vec(),
    };
    let config_path = scratch.path().join("oxfmt.json");
    if std::fs::write(&config_path, &config_bytes).is_err() {
        return FormatOutcome {
            formatted: Vec::new(),
            skipped: candidates
                .iter()
                .map(|&index| FormatSkip {
                    path: files[index].path.clone(),
                    reason: "formatter unavailable".to_string(),
                })
                .collect(),
        };
    }

    let deadline = Instant::now() + FORMAT_TOTAL_TIMEOUT;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(FORMAT_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();
    for index in candidates {
        let semaphore = semaphore.clone();
        let bin = bin.to_path_buf();
        let path = files[index].path.clone();
        let content = files[index].bytes.clone();
        let config_path = config_path.clone();
        let workdir = scratch.path().to_path_buf();
        set.spawn(async move {
            let permit = semaphore.acquire_owned().await;
            let Ok(_permit) = permit else {
                return (
                    index,
                    FileResult::Skipped("formatter unavailable".to_string()),
                );
            };
            let result = run_one(&bin, &path, &content, &config_path, &workdir, deadline).await;
            (index, result)
        });
    }
    // Tasks never panic (every failure maps to `FileResult`), so a join
    // error can only mean cancellation, which never happens here.
    let mut results = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(result) = joined {
            results.push(result);
        }
    }
    results.sort_by_key(|(index, _)| *index);
    let mut outcome = FormatOutcome::default();
    for (index, result) in results {
        match result {
            FileResult::Unchanged => {}
            FileResult::Changed(bytes) => {
                files[index].bytes = bytes;
                outcome.formatted.push(files[index].path.clone());
            }
            FileResult::Skipped(reason) => outcome.skipped.push(FormatSkip {
                path: files[index].path.clone(),
                reason,
            }),
        }
    }
    outcome
}

enum FileResult {
    Unchanged,
    Changed(Vec<u8>),
    Skipped(String),
}

async fn run_one(
    bin: &Path,
    repo_path: &str,
    content: &[u8],
    config_path: &Path,
    workdir: &Path,
    deadline: Instant,
) -> FileResult {
    use std::os::unix::process::CommandExt as _;

    fn kill_group(pid: u32) {
        unsafe {
            let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }

    let mut command = tokio::process::Command::new(bin);
    command
        .arg("--stdin-filepath")
        .arg(repo_path)
        // Never execute anything from the app: no node_modules lookup, no
        // JS/TS config discovery. `-c` pins the config; `cwd` is empty.
        .arg("-c")
        .arg(config_path)
        .current_dir(workdir)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own process group so a timed-out run's children (a `#!/bin/sh`
    // shim's, a worker pool's) die with it. Same shape as `run_git_inner`.
    command.as_std_mut().process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return FileResult::Skipped("formatter unavailable".to_string()),
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A formatter may exit before reading stdin (an immediate
        // failure): ignore a broken pipe and report its exit below
        // instead of masking it as unavailable.
        let _ = stdin.write_all(content).await;
        // Close stdin so the formatter sees EOF instead of hanging.
        drop(stdin);
    }
    let pid = child.id();
    // `wait_with_output` consumes the child; on timeout the future is
    // dropped and `kill_on_drop` kills the process itself.
    let output = match tokio::time::timeout_at(deadline.into(), child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => return FileResult::Skipped("formatter unavailable".to_string()),
        Err(_) => {
            if let Some(pid) = pid {
                kill_group(pid);
            }
            return FileResult::Skipped("timeout".to_string());
        }
    };
    if !output.status.success() {
        let reason = first_stderr_line(&output.stderr)
            .map(|line| format!("oxfmt: {line}"))
            .unwrap_or_else(|| format!("oxfmt failed ({})", output.status));
        return FileResult::Skipped(reason);
    }
    if output.stdout.len() > FORMAT_OUTPUT_CAP {
        return FileResult::Skipped("output too large".to_string());
    }
    if output.stdout == content {
        FileResult::Unchanged
    } else {
        FileResult::Changed(output.stdout)
    }
}

/// First non-empty stderr line, truncated to 160 chars: the actionable part
/// of an oxfmt parse error.
fn first_stderr_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            let mut short: String = line.chars().take(160).collect();
            if line.chars().count() > 160 {
                short.push('…');
            }
            short
        })
}

/// This commit's version of `name` wins (it may be what the commit writes),
/// else the head tree's. Blobs read as data, capped.
async fn tree_file_content(
    bare: &Path,
    head: Option<&str>,
    files: &[RawFile],
    name: &str,
) -> Option<Vec<u8>> {
    if let Some(file) = files.iter().find(|f| f.path == name) {
        return Some(file.bytes.clone());
    }
    let head = head?;
    let git_dir = format!("--git-dir={}", bare.display());
    let listing = manifest::run_git(
        &[
            &git_dir,
            "ls-tree",
            head,
            "--",
            &format!(":(literal){name}"),
        ],
        &[],
        Duration::from_secs(15),
    )
    .await
    .ok()?;
    let mut fields = listing.split_whitespace();
    let _mode = fields.next()?;
    let kind = fields.next()?;
    let sha = fields.next()?;
    if kind != "blob" {
        return None;
    }
    let bytes = manifest::run_git_raw(
        &[&git_dir, "cat-file", "-p", sha],
        &[],
        Duration::from_secs(15),
    )
    .await
    .ok()?;
    if bytes.len() > TREE_CONFIG_CAP {
        return None;
    }
    Some(bytes)
}

fn warn_missing_once(bin: &Path) {
    if !MISSING_BIN_WARNED.swap(true, Ordering::SeqCst) {
        tracing::warn!(
            "oxfmt binary {} missing or not executable: commits skip formatting (localhost/dev case)",
            bin.display(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formattable_extensions_match_case_insensitively() {
        for ext in FORMATTABLE_EXTENSIONS {
            assert!(is_formattable(&format!("app/file.{ext}")), "{ext}");
            assert!(
                is_formattable(&format!("app/file.{}", ext.to_ascii_uppercase())),
                "{ext} upper"
            );
        }
    }

    #[test]
    fn unformattable_paths_stay_out() {
        for path in [
            "app/readme.txt",
            "app/main.py",
            "app/code.rs",
            "Makefile",
            "app/noext",
            "app/trailing.",
            ".oxfmtrc",
            "app/archive.tar.gz",
        ] {
            assert!(!is_formattable(path), "{path}");
        }
    }

    #[test]
    fn ignore_rules_honor_oxfmtrc_patterns() {
        let rules = IgnoreRules::parse(
            Some(br#"{"ignorePatterns": ["dist/**", "*.generated.ts", "src/vendor/"]}"#),
            None,
        );
        assert!(rules.matches("dist/bundle.js"));
        assert!(rules.matches("dist/nested/bundle.js"));
        assert!(rules.matches("src/app.generated.ts"));
        assert!(rules.matches("app.generated.ts"));
        assert!(rules.matches("src/vendor/lib.ts"));
        assert!(rules.matches("src/vendor/nested/lib.ts"));
        assert!(!rules.matches("src/app.ts"));
        assert!(!rules.matches("distraction/app.ts"));
    }

    #[test]
    fn ignore_rules_honor_prettierignore_with_negation() {
        let rules = IgnoreRules::parse(
            None,
            Some(b"# generated\nbuild/\nkeep/but-not-this\n!keep/but-not-this/yes.ts\n"),
        );
        assert!(rules.matches("build/out.js"));
        assert!(rules.matches("keep/but-not-this/no.ts"));
        assert!(!rules.matches("keep/but-not-this/yes.ts"));
        assert!(!rules.matches("src/app.ts"));
    }

    #[test]
    fn prettierignore_runs_after_oxfmtrc_so_it_can_reinclude() {
        let rules = IgnoreRules::parse(
            Some(br#"{"ignorePatterns": ["gen/**"]}"#),
            Some(b"!gen/keep.ts\n"),
        );
        assert!(rules.matches("gen/drop.ts"));
        assert!(!rules.matches("gen/keep.ts"));
    }

    #[test]
    fn broken_config_json_falls_back_to_nothing_ignored() {
        let rules = IgnoreRules::parse(Some(b"{not json"), Some(b"\xff\xfe"));
        assert!(!rules.matches("src/app.ts"));
        let rules = IgnoreRules::parse(Some(br#"{"ignorePatterns": [42]}"#), None);
        assert!(!rules.matches("src/app.ts"));
    }
}
