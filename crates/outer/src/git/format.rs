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
//! file Outer writes from bytes read as data out of the commit's tree,
//! minus the `sortTailwindcss` key (top level and inside every
//! `overrides[]` entry). oxfmt 0.74's `config`/`stylesheet` sub-options
//! take host paths and its tailwind auto-find walks up from the config
//! file, which would `jiti.import()` JS as the Outer uid; app files are
//! never on disk, so these options can never legitimately resolve. The
//! schema has no other filesystem/module key (there are no plugin keys;
//! globs and import-sort prefixes match in memory only), so removing the
//! whole key closes it entirely.
//! Ignore rules honored: the tree's `.oxfmtrc.json` `ignorePatterns` and the
//! tree's `.prettierignore`. `.gitignore` is NOT honored. Pattern syntax:
//! `*` (any run within a segment), `**` (any run across segments), `?` (one
//! character), a leading `/` or `./` is stripped, a trailing `/` matches the
//! whole directory, a pattern without `/` matches at any depth, and (in
//! `.prettierignore` only) a leading `!` negates with last-match-wins. No
//! character classes, no `\` escapes. Budgets below bound adversarial
//! configs; over-budget patterns are ignored (fail-open: those files
//! format).
//!
//! Failure policy: a file oxfmt cannot parse, a timeout, or an oversized
//! output commits that file UNFORMATTED — never block the agent's commit.
//! The build reports syntax errors, not the commit path.

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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

/// Total formatting budget per commit. A file's whole run — the wait for a
/// per-commit and a process-wide permit, the spawn, the stdin write, and
/// the formatter wait — is abandoned at this deadline and its process group
/// killed, so a hung formatter delays the commit by at most this long.
pub const FORMAT_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Most concurrent oxfmt processes per commit.
pub const FORMAT_CONCURRENCY: usize = 4;

/// Largest formatted output accepted per file (inputs are already capped by
/// the commit caps; a formatter must never blow one up unboundedly).
pub const FORMAT_OUTPUT_CAP: usize = 1024 * 1024;

/// Cap on a config/ignore blob read from the tree. Real configs are tiny;
/// anything larger is treated as absent.
const TREE_CONFIG_CAP: usize = 1024 * 1024;

/// Largest single formatter stderr kept in memory; only the first line ever
/// reaches the agent, the rest is diagnostic at best.
pub const FORMAT_STDERR_CAP: usize = 64 * 1024;

/// Ignore-pattern budgets: adversarial configs (up to 256 KB of patterns)
/// times long paths times 100 files must not wedge a tokio worker. At most
/// this many patterns are honored; longer ones are skipped and parsing
/// stops once the byte budget is spent. Fail-open: those files format.
pub const MAX_IGNORE_PATTERNS: usize = 256;
pub const MAX_IGNORE_PATTERN_CHARS: usize = 256;
pub const MAX_IGNORE_SEGMENT_CHARS: usize = 128;
pub const MAX_IGNORE_PATTERN_BYTES: usize = 16 * 1024;

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
        let mut budget = PatternBudget::default();
        if let Some(bytes) = oxfmt_config {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
                if let Some(patterns) = value.get("ignorePatterns").and_then(|v| v.as_array()) {
                    for raw in patterns.iter().filter_map(|v| v.as_str()) {
                        // oxfmtrc patterns don't negate: a `!` there is literal.
                        let Some(pattern) = compile_pattern(raw) else {
                            continue;
                        };
                        if !budget.admit(&pattern) {
                            return rules;
                        }
                        rules.patterns.push(pattern);
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
                    let Some(mut pattern) = compile_pattern(line) else {
                        continue;
                    };
                    pattern.negated = negated;
                    if !budget.admit(&pattern) {
                        return rules;
                    }
                    rules.patterns.push(pattern);
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

/// Running count/size of honored patterns: parsing stops (fail-open) once
/// an adversarial config would push matching work past the budgets.
#[derive(Default)]
struct PatternBudget {
    patterns: usize,
    bytes: usize,
}

impl PatternBudget {
    /// False when `pattern` would exceed the count or byte budget: the
    /// caller stops parsing and keeps what it has.
    fn admit(&mut self, pattern: &Pattern) -> bool {
        let bytes: usize = pattern.segments.iter().map(|s| s.len()).sum();
        if self.patterns >= MAX_IGNORE_PATTERNS || self.bytes + bytes > MAX_IGNORE_PATTERN_BYTES {
            return false;
        }
        self.patterns += 1;
        self.bytes += bytes;
        true
    }
}

fn compile_pattern(raw: &str) -> Option<Pattern> {
    if raw.len() > MAX_IGNORE_PATTERN_CHARS {
        return None;
    }
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
        if segment.len() > MAX_IGNORE_SEGMENT_CHARS {
            return None;
        }
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

/// The `-c` config Outer hands the formatter: this commit's `.oxfmtrc.json`
/// with every filesystem-touching option removed (see the module docs).
/// Unparseable or non-object configs become oxfmt defaults.
pub(crate) fn sanitize_oxfmt_config(bytes: &[u8]) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return b"{}".to_vec();
    };
    let Some(obj) = value.as_object_mut() else {
        return b"{}".to_vec();
    };
    obj.remove("sortTailwindcss");
    if let Some(overrides) = obj.get_mut("overrides").and_then(|v| v.as_array_mut()) {
        for entry in overrides.iter_mut().filter_map(|v| v.as_object_mut()) {
            entry.remove("sortTailwindcss");
        }
    }
    serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec())
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
/// works, which is the localhost/dev case. `slots` is the process-wide
/// formatter semaphore (shared by cloning `GitConfig`, never a global).
pub(crate) async fn format_committed_files(
    bin: Option<&Path>,
    slots: &Arc<tokio::sync::Semaphore>,
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
    // The `-c` config: this commit's `.oxfmtrc.json` with
    // filesystem-touching options removed (`sanitize_oxfmt_config`), else
    // oxfmt defaults from an empty config. Either way oxfmt discovers
    // nothing on disk: the scratch dir holds only this file.
    let config_bytes = match &oxfmt_config {
        Some(bytes) => sanitize_oxfmt_config(bytes),
        None => b"{}".to_vec(),
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
    let semaphore = Arc::new(tokio::sync::Semaphore::new(FORMAT_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();
    for &index in &candidates {
        let semaphore = semaphore.clone();
        let slots = Arc::clone(slots);
        let bin = bin.to_path_buf();
        let path = files[index].path.clone();
        let content = files[index].bytes.clone();
        let config_path = config_path.clone();
        let workdir = scratch.path().to_path_buf();
        set.spawn(async move {
            // The whole per-file body — both permit waits, the spawn, the
            // stdin write, the formatter wait — shares the commit deadline:
            // a formatter that never drains stdin blocks its writer, so
            // leaving any of this outside the timeout hangs the commit.
            // `acquire_owned` is cancel-safe: a timed-out wait holds no
            // permit.
            let finished = tokio::time::timeout_at(deadline.into(), async {
                let Ok(_process) = slots.acquire_owned().await else {
                    return FileResult::Skipped("formatter unavailable".to_string());
                };
                let Ok(_permit) = semaphore.acquire_owned().await else {
                    return FileResult::Skipped("formatter unavailable".to_string());
                };
                run_one(&bin, &path, &content, &config_path, &workdir, deadline).await
            })
            .await;
            let result = finished.unwrap_or(FileResult::Skipped("timeout".to_string()));
            (index, result)
        });
    }
    // Tasks never panic (every failure maps to `FileResult`), so a join
    // error only costs an index: its file keeps its bytes and is reported
    // as a timeout below. The join itself is bounded by the deadline —
    // anything still running is aborted (its drop kills its process group
    // via `kill_on_drop` plus the explicit `kill_group` in `run_one`).
    let mut unfinished: HashSet<usize> = candidates.iter().copied().collect();
    let mut results = Vec::new();
    while !unfinished.is_empty() {
        let next = tokio::time::timeout_at(deadline.into(), set.join_next()).await;
        let Ok(Some(joined)) = next else {
            break;
        };
        if let Ok((index, result)) = joined {
            unfinished.remove(&index);
            results.push((index, result));
        }
        // A join error (panic/cancel, which never happens: every failure
        // maps to `FileResult`) only costs an index: its file keeps its
        // bytes and lands in the timeout leftovers below.
    }
    set.abort_all();
    while set.join_next().await.is_some() {}
    for index in unfinished {
        results.push((index, FileResult::Skipped("timeout".to_string())));
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
    let pid = child.id();
    // Stdin, stdout, stderr and the exit wait share this one deadline: a
    // formatter that never drains stdin blocks `write_all` past the pipe
    // buffer (>64 KB inputs), so the write must die with the child, not
    // hang the commit on a held permit. On timeout the future is dropped
    // (`kill_on_drop` kills the process itself) and the group is killed
    // explicitly below.
    let finished = tokio::time::timeout_at(deadline.into(), async {
        if let Some(mut stdin) = child.stdin.take() {
            // A formatter may exit before reading stdin (an immediate
            // failure): ignore a broken pipe and report its exit below
            // instead of masking it as unavailable.
            let _ = stdin.write_all(content).await;
            // Close stdin so the formatter sees EOF instead of hanging.
            drop(stdin);
        }
        // Drain stdout/stderr concurrently — a formatter spewing on one
        // pipe must not wedge the other — each with a hard cap, mirroring
        // `MAX_GIT_OUTPUT_BYTES` in `local.rs`. Past the stdout cap the
        // group dies and the file commits unformatted.
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        {
            use tokio::io::AsyncReadExt as _;
            let out = async {
                if let Some(pipe) = child.stdout.take() {
                    let mut capped = pipe.take((FORMAT_OUTPUT_CAP + 1) as u64);
                    let _ = capped.read_to_end(&mut stdout).await;
                }
            };
            let err = async {
                if let Some(pipe) = child.stderr.take() {
                    let mut capped = pipe.take((FORMAT_STDERR_CAP + 1) as u64);
                    let _ = capped.read_to_end(&mut stderr).await;
                }
            };
            tokio::join!(out, err);
        }
        if stdout.len() > FORMAT_OUTPUT_CAP {
            if let Some(pid) = pid {
                kill_group(pid);
            }
            // Reap the kill; the drop's SIGKILL backs this up.
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            return Err(FileResult::Skipped("output too large".to_string()));
        }
        if stderr.len() > FORMAT_STDERR_CAP {
            stderr.truncate(FORMAT_STDERR_CAP);
        }
        match child.wait().await {
            Ok(status) => Ok((status, stdout, stderr)),
            Err(_) => Err(FileResult::Skipped("formatter unavailable".to_string())),
        }
    })
    .await;
    let (status, stdout, stderr) = match finished {
        Err(_) => {
            if let Some(pid) = pid {
                kill_group(pid);
            }
            return FileResult::Skipped("timeout".to_string());
        }
        Ok(Err(skip)) => return skip,
        Ok(Ok(done)) => done,
    };
    if !status.success() {
        let reason = first_stderr_line(&stderr)
            .map(|line| format!("oxfmt: {line}"))
            .unwrap_or_else(|| format!("oxfmt failed ({status})"));
        return FileResult::Skipped(reason);
    }
    if stdout == content {
        FileResult::Unchanged
    } else {
        FileResult::Changed(stdout)
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

    #[test]
    fn sanitize_removes_sort_tailwindcss_everywhere_but_keeps_the_rest() {
        let raw = br#"{
            "singleQuote": true,
            "ignorePatterns": ["gen/**"],
            "sortTailwindcss": {"config": "/tmp/evil/tailwind.config.js", "stylesheet": "/tmp/evil/t.css"},
            "overrides": [
                {"files": ["*.ts"], "sortTailwindcss": true},
                {"files": ["*.css"], "singleQuote": false}
            ]
        }"#;
        let cleaned = sanitize_oxfmt_config(raw);
        let value: serde_json::Value = serde_json::from_slice(&cleaned).unwrap();
        assert!(value.get("sortTailwindcss").is_none(), "{value}");
        for entry in value.get("overrides").unwrap().as_array().unwrap() {
            assert!(entry.get("sortTailwindcss").is_none(), "{entry}");
        }
        assert_eq!(
            value.get("singleQuote"),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(value.get("ignorePatterns").is_some(), "{value}");
        assert_eq!(value.get("overrides").unwrap().as_array().unwrap().len(), 2);
    }

    #[test]
    fn sanitize_unparseable_or_non_object_config_becomes_empty() {
        assert_eq!(sanitize_oxfmt_config(b"{not json"), b"{}");
        assert_eq!(sanitize_oxfmt_config(br#"[1, 2]"#), b"{}");
        assert_eq!(
            sanitize_oxfmt_config(br#"{"singleQuote": true}"#),
            br#"{"singleQuote":true}"#
        );
    }

    #[test]
    fn ignore_budgets_cap_count_and_fail_open() {
        // 300 distinct patterns: only the first 256 are honored.
        let mut patterns = Vec::new();
        for i in 0..300 {
            patterns.push(format!("\"dir{i}/**\""));
        }
        let config = format!("{{\"ignorePatterns\": [{}]}}", patterns.join(","));
        let rules = IgnoreRules::parse(Some(config.as_bytes()), None);
        assert_eq!(rules.patterns.len(), MAX_IGNORE_PATTERNS);
        assert!(rules.matches("dir0/a.ts"));
        assert!(rules.matches("dir255/a.ts"));
        assert!(!rules.matches("dir299/a.ts"), "over budget: formats");
    }

    #[test]
    fn ignore_budgets_skip_oversize_patterns_and_continue() {
        // One 300-char pattern is skipped; parsing continues after it.
        let long = "a".repeat(300);
        let rules = IgnoreRules::parse(
            Some(format!("{{\"ignorePatterns\": [\"{long}\", \"gen/**\"]}}").as_bytes()),
            None,
        );
        assert!(!rules.matches("src/app.ts"));
        assert!(rules.matches("gen/a.ts"));
    }

    #[test]
    fn ignore_budgets_stop_at_byte_budget_before_count_cap() {
        // 300 patterns of ~90 bytes each: the 16 KB cumulative budget stops
        // parsing (~180 in) before the 256-pattern count cap matters.
        let pad = "x".repeat(80);
        let mut patterns = vec!["gen/**".to_string()];
        for i in 0..300 {
            patterns.push(format!("seg-{i:03}-{pad}/**"));
        }
        let quoted: Vec<String> = patterns.iter().map(|p| format!("\"{p}\"")).collect();
        let config = format!("{{\"ignorePatterns\": [{}]}}", quoted.join(","));
        let rules = IgnoreRules::parse(Some(config.as_bytes()), None);
        assert!(rules.matches("gen/a.ts"));
        assert!(
            rules.patterns.len() < MAX_IGNORE_PATTERNS,
            "byte budget binds first: {}",
            rules.patterns.len()
        );
        assert!(
            !rules.matches(&format!("seg-299-{pad}/a.ts")),
            "fail-open: over-budget patterns format"
        );
    }

    #[test]
    fn ignore_budgets_skip_oversize_segments_and_prettierignore_lines() {
        let long_segment = "b".repeat(200);
        let rules = IgnoreRules::parse(
            None,
            Some(format!("{long_segment}/**\nkeep/**\n").as_bytes()),
        );
        assert!(rules.matches("keep/a.ts"));
        assert!(
            !rules.matches(&format!("{long_segment}/a.ts")),
            "fail-open: formats"
        );
        // Oversize prettierignore lines are skipped, the rest still applies.
        let long_line = "c".repeat(300);
        let rules = IgnoreRules::parse(None, Some(format!("{long_line}\nskip/**\n").as_bytes()));
        assert!(rules.matches("skip/a.ts"));
    }
}
