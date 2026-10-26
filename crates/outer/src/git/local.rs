//! Read-only git plumbing on an app's bare mirror (`refs/heads/main`).
//!
//! The MCP code tools call these after [`crate::git::manifest::ensure_synced`]
//! hands them the mirror. Nothing here writes: listing, reading, grepping,
//! log and show. All git output is size-capped before it lands in memory so a
//! hostile blob cannot OOM the platform.

use std::path::Path;
use std::time::Duration;

use super::manifest::run_git;

/// Largest blob [`read_blob`] loads: same ceiling as commit text files, so a
/// read can never hold more than a single commit file would.
pub const MAX_READ_BYTES: usize = 256 * 1024;
/// Most matches [`grep_search`] returns (spec §8.2: `git grep` matches, ≤ 200).
pub const MAX_GREP_MATCHES: usize = 200;
/// Most commits [`log`] returns per call.
pub const MAX_LOG_COMMITS: usize = 50;
/// Largest patch [`show`] returns before it truncates.
pub const MAX_PATCH_BYTES: usize = 64 * 1024;

const GIT_TIMEOUT: Duration = Duration::from_secs(15);

/// One path in a tree, with its blob size.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
}

/// One `git grep` hit, with its context lines (`search_code` `context`).
#[derive(Debug, Clone)]
pub struct GrepHit {
    pub line: u64,
    pub text: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

/// One file's hits, in `git grep` order.
#[derive(Debug, Clone)]
pub struct FileHits {
    pub path: String,
    pub matches: Vec<GrepHit>,
}

/// Options for [`grep_search`].
#[derive(Debug, Clone, Default)]
pub struct GrepOptions {
    pub regex: bool,
    pub case_insensitive: bool,
    pub word: bool,
    pub paths: Vec<String>,
    pub include_generated: bool,
    pub context: u8,
}

/// One commit in `main`'s history.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub sha: String,
    pub author_name: String,
    pub author_email: String,
    /// Author time, unix seconds.
    pub at: i64,
    pub message: String,
}

/// One changed file: its status against the parent (or, with `--root`, the
/// empty tree) and its added/removed line counts. `binary` files carry no
/// line counts (`- -` in numstat output).
#[derive(Debug, Clone)]
pub struct FileStat {
    pub path: String,
    pub status: char,
    pub added: u64,
    pub removed: u64,
    pub binary: bool,
}

/// A commit with its file list and unified patch.
#[derive(Debug, Clone)]
pub struct ShowResult {
    pub commit: LogEntry,
    pub files: Vec<String>,
    /// Unified patch, cut at [`MAX_PATCH_BYTES`] with `truncated` set.
    pub patch: String,
    pub truncated: bool,
}

/// Reject worktree escapes and git internals before a path reaches git, with
/// the same shape as the commit path rules: relative, no `..`, no `.git`,
/// ≤ 512 chars.
pub fn validate_read_path(path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!path.is_empty(), "path must not be empty");
    anyhow::ensure!(!path.starts_with('/'), "path {path:?} must be relative");
    anyhow::ensure!(
        !path.starts_with('-'),
        "path {path:?} must not start with `-`"
    );
    anyhow::ensure!(
        path.chars().count() <= 512,
        "path {path:?} exceeds 512 characters"
    );
    anyhow::ensure!(!path.contains('\0'), "path {path:?} holds a NUL byte");
    for part in path.split('/') {
        anyhow::ensure!(!part.is_empty(), "path {path:?} has an empty segment");
        anyhow::ensure!(part != ".", "path {path:?} must not contain `.`");
        anyhow::ensure!(part != "..", "path {path:?} must not contain `..`");
        anyhow::ensure!(part != ".git", "path {path:?} must not contain `.git`");
    }
    Ok(())
}

/// Accept only hex shas before they reach git: full (40) or abbreviated
/// (4+), so a `-` flag or shell metacharacter can never ride into
/// `rev-parse` or a `rev:path` spec.
pub fn validate_sha(rev: &str) -> anyhow::Result<()> {
    let ok = (4..=64).contains(&rev.len()) && rev.bytes().all(|b| b.is_ascii_hexdigit());
    anyhow::ensure!(ok, "rev {rev:?} is not a commit sha");
    Ok(())
}
fn git_dir(bare: &Path) -> String {
    format!("--git-dir={}", bare.display())
}

/// Resolve `rev` (full or short sha) to a commit sha. `None` when the mirror
/// has no `main` yet; an error when `rev` names nothing on `main`.
pub async fn resolve_rev(bare: &Path, rev: Option<&str>) -> anyhow::Result<Option<String>> {
    let gd = git_dir(bare);
    match rev {
        None => match super::manifest::head_of(bare).await? {
            Some(sha) => Ok(Some(sha)),
            None => Ok(None),
        },
        Some(rev) => {
            validate_sha(rev)?;
            let out = run_git(
                &[&gd, "rev-parse", "--verify", &format!("{rev}^{{commit}}")],
                &[],
                GIT_TIMEOUT,
            )
            .await?;
            Ok(Some(out.trim().to_string()))
        }
    }
}

/// Every blob under `rev`'s tree, optionally under `prefix/`. Returns the
/// resolved sha, entries in `ls-tree` order, and whether the cap cut the
/// listing.
pub async fn list_files(
    bare: &Path,
    rev: Option<&str>,
    prefix: Option<&str>,
    limit: usize,
) -> anyhow::Result<(String, Vec<FileEntry>, bool)> {
    let Some(sha) = resolve_rev(bare, rev).await? else {
        anyhow::bail!("app has no commits yet");
    };
    if let Some(prefix) = prefix {
        validate_read_path(prefix)?;
    }
    let gd = git_dir(bare);
    let mut args = vec![gd.as_str(), "ls-tree", "-r", "-l", "--", &sha];
    let prefix_arg;
    if let Some(prefix) = prefix {
        prefix_arg = format!("{}/", prefix.trim_end_matches('/'));
        args.push(&prefix_arg);
    }
    let run = run_capped(&args, MAX_GIT_OUTPUT_BYTES).await?;
    if run.failed() {
        anyhow::bail!("git ls-tree failed\n{}", run.stderr);
    }
    let out = String::from_utf8_lossy(&run.stdout);
    let mut files = Vec::new();
    let mut truncated = run.truncated;
    for line in out.lines() {
        if files.len() >= limit {
            truncated = true;
            break;
        }
        // `100644 blob <sha> <size>\t<path>`.
        let (meta, path) = line.split_once('\t').unwrap_or((line, ""));
        let mut meta = meta.split_whitespace();
        let (Some(_mode), Some(kind), Some(_hash), Some(size)) =
            (meta.next(), meta.next(), meta.next(), meta.next())
        else {
            continue;
        };
        if kind != "blob" || path.is_empty() {
            continue;
        }
        files.push(FileEntry {
            path: path.to_string(),
            size: size.parse().unwrap_or(0),
        });
    }
    Ok((sha, files, truncated))
}

/// The blob at `rev:path`: its sha and bytes. Blobs over [`MAX_READ_BYTES`]
/// are refused before they load, so callers can report truncation instead.
pub async fn read_blob(
    bare: &Path,
    rev: Option<&str>,
    path: &str,
) -> anyhow::Result<(String, Vec<u8>)> {
    validate_read_path(path)?;
    let Some(sha) = resolve_rev(bare, rev).await? else {
        anyhow::bail!("app has no commits yet");
    };
    let gd = git_dir(bare);
    let spec = format!("{sha}:{path}");
    let size_out = run_git(&[&gd, "cat-file", "-s", &spec], &[], GIT_TIMEOUT)
        .await
        .map_err(|_| anyhow::anyhow!("{path} not found"))?;
    let size: usize = size_out.trim().parse().unwrap_or(usize::MAX);
    anyhow::ensure!(
        size <= MAX_READ_BYTES,
        "{path} is {size} bytes, over the {MAX_READ_BYTES}-byte read cap"
    );
    let bytes = run_git_raw(&[&gd, "cat-file", "blob", &spec], GIT_TIMEOUT).await?;
    Ok((sha, bytes))
}

/// Raw-stdout variant of [`run_git`]: `git cat-file blob` output is bytes,
/// not text.
async fn run_git_raw(args: &[&str], timeout: Duration) -> anyhow::Result<Vec<u8>> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("git {args:?} timed out"))??;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {args:?} failed\n{err}");
    }
    Ok(output.stdout)
}

/// `git grep` for `pattern` at `rev` (default: head), grouped by file.
/// `regex` selects `-E` (otherwise the query is a fixed string),
/// `case_insensitive` adds `-i`, `word` adds `-w`. `paths` are globs
/// (`*`, `?`, `**`; a glob without `/` matches the basename at any depth);
/// generated files (`node_modules/`, `dist/`, lockfiles) are skipped unless
/// `include_generated`. `context` (0-5) attaches surrounding lines read back
/// from each matched blob. Returns at most [`MAX_GREP_MATCHES`] hits plus
/// whether more existed (or the fetch hit its internal scan cap).
pub async fn grep_search(
    bare: &Path,
    rev: Option<&str>,
    pattern: &str,
    opts: &GrepOptions,
) -> anyhow::Result<(String, Vec<FileHits>, bool)> {
    anyhow::ensure!(!pattern.is_empty(), "query must not be empty");
    anyhow::ensure!(pattern.len() <= 1024, "query exceeds 1024 characters");
    anyhow::ensure!(opts.context <= 5, "context is 0-5");
    anyhow::ensure!(opts.paths.len() <= 20, "at most 20 path globs");
    for glob in &opts.paths {
        validate_glob(glob)?;
    }
    if opts.regex {
        regex::Regex::new(pattern).map_err(|e| anyhow::anyhow!("bad regex: {e}"))?;
    }
    let Some(sha) = resolve_rev(bare, rev).await? else {
        anyhow::bail!("app has no commits yet");
    };
    let gd = git_dir(bare);
    let flag = if opts.regex { "-E" } else { "-F" };
    let mut args = vec![
        gd.as_str(),
        "grep",
        "-n",
        "-I",
        "--no-color",
        flag,
        "-e",
        pattern,
    ];
    if opts.case_insensitive {
        args.push("-i");
    }
    if opts.word {
        args.push("-w");
    }
    args.push(&sha);
    // `git grep` exits 1 on no matches: that is an empty result, not a failure.
    // The stream is capped: the child dies at 1 MiB and parsing stops at the
    // scan cap, so both paths mark `truncated`.
    let run = run_capped(&[&args[..], &["--", "."]].concat(), MAX_GIT_OUTPUT_BYTES).await?;
    let out = if run.failed() {
        if run.status.code() == Some(1) {
            String::new()
        } else {
            anyhow::bail!("git grep failed\n{}", run.stderr)
        }
    } else {
        String::from_utf8_lossy(&run.stdout).into_owned()
    };
    // Raw hits in output order: filtering (generated, globs) runs after the
    // fetch, so the fetch scans past the 200-match reporting cap.
    let mut raw: Vec<(String, u64, String)> = Vec::new();
    let mut scan_cut = false;
    for line in out.lines() {
        if raw.len() >= MAX_GREP_SCAN {
            scan_cut = true;
            break;
        }
        // `<sha>:<path>:<line>:<text>`.
        let rest = line.strip_prefix(&sha).and_then(|r| r.strip_prefix(':'));
        let Some(rest) = rest else { continue };
        let Some(colon) = rest.find(':') else {
            continue;
        };
        let (path, rest) = rest.split_at(colon);
        let rest = &rest[1..];
        let Some(colon) = rest.find(':') else {
            continue;
        };
        let (lineno, text) = rest.split_at(colon);
        raw.push((
            path.to_string(),
            lineno.parse().unwrap_or(0),
            text[1..].chars().take(500).collect(),
        ));
    }
    let mut total = 0usize;
    let mut files: Vec<FileHits> = Vec::new();
    let mut truncated = scan_cut || run.truncated;
    for (path, line, text) in raw {
        if !opts.include_generated && is_generated(&path) {
            continue;
        }
        if !opts.paths.is_empty() && !opts.paths.iter().any(|g| match_glob(g, &path)) {
            continue;
        }
        if total >= MAX_GREP_MATCHES {
            truncated = true;
            break;
        }
        total += 1;
        match files.last_mut() {
            Some(f) if f.path == path => f.matches.push(GrepHit {
                line,
                text,
                before: Vec::new(),
                after: Vec::new(),
            }),
            _ => files.push(FileHits {
                path,
                matches: vec![GrepHit {
                    line,
                    text,
                    before: Vec::new(),
                    after: Vec::new(),
                }],
            }),
        }
    }
    if opts.context > 0 {
        attach_context(bare, rev, &mut files, opts.context).await;
    }
    Ok((sha, files, truncated))
}

/// Most raw hits parsed before generated/glob filtering; the reporting cap
/// ([`MAX_GREP_MATCHES`]) applies after filtering.
const MAX_GREP_SCAN: usize = 2000;

/// Fill `before`/`after` for every hit by reading each matched blob once.
/// Blobs over the read cap or binary blobs keep their matches with empty
/// context: the hit stands, the surroundings don't.
async fn attach_context(bare: &Path, rev: Option<&str>, files: &mut [FileHits], context: u8) {
    let radius = context as usize;
    for file in files.iter_mut() {
        let Ok((_, bytes)) = read_blob(bare, rev, &file.path).await else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for hit in file.matches.iter_mut() {
            let at = hit.line.saturating_sub(1) as usize;
            let lo = at.saturating_sub(radius);
            let hi = (at + radius + 1).min(lines.len());
            hit.before = lines[lo..at.min(lines.len())]
                .iter()
                .map(|s| s.chars().take(500).collect())
                .collect();
            hit.after = lines[at.saturating_add(1).min(lines.len())..hi]
                .iter()
                .map(|s| s.chars().take(500).collect())
                .collect();
        }
    }
}

/// Basenames never worth searching unless `include_generated` opts back in.
const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "bun.lock",
    "bun.lockb",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "Gemfile.lock",
    "poetry.lock",
    "composer.lock",
    "Pipfile.lock",
];

/// Generated files are skipped by default: vendored deps, build output and
/// lockfiles — matches there drown the app's own code.
pub fn is_generated(path: &str) -> bool {
    if path
        .split('/')
        .any(|seg| seg == "node_modules" || seg == "dist")
    {
        return true;
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    LOCKFILES.contains(&base)
}

/// One user glob against one repo-relative path. `*` and `?` stay inside a
/// segment when the glob holds a `/` (`src/*.ts` is one level);
/// `**/` crosses segments, and a glob without `/` matches the basename at
/// any depth (`*.ts` finds `src/a.ts`).
pub fn match_glob(glob: &str, path: &str) -> bool {
    let full = glob.contains('/');
    let haystack = if full {
        path
    } else {
        path.rsplit('/').next().unwrap_or(path)
    };
    let mut re = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else if full {
                    re.push_str("[^/]*");
                } else {
                    re.push_str(".*");
                }
            }
            '?' => re.push_str(if full { "[^/]" } else { "." }),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    regex::Regex::new(&re)
        .map(|r| r.is_match(haystack))
        .unwrap_or(false)
}

/// Globs ride into a Rust match, never into git, so the rules are only
/// about shape: relative, bounded, no escapes.
fn validate_glob(glob: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!glob.is_empty(), "path globs must not be empty");
    anyhow::ensure!(glob.len() <= 256, "path glob exceeds 256 characters");
    anyhow::ensure!(!glob.starts_with('/'), "path glob must be relative");
    anyhow::ensure!(
        !glob.split('/').any(|seg| seg == ".."),
        "path glob must not escape (..)"
    );
    Ok(())
}

/// The blob id at `rev:path`: the per-file handle `read_files` reports so
/// later edits can check one file instead of the whole head.
pub async fn blob_sha(
    bare: &Path,
    rev: Option<&str>,
    path: &str,
) -> anyhow::Result<(String, String)> {
    validate_read_path(path)?;
    let Some(sha) = resolve_rev(bare, rev).await? else {
        anyhow::bail!("app has no commits yet");
    };
    let gd = git_dir(bare);
    let spec = format!("{sha}:{path}");
    let out = run_git(&[&gd, "rev-parse", "--verify", &spec], &[], GIT_TIMEOUT)
        .await
        .map_err(|_| anyhow::anyhow!("{path} not found"))?;
    Ok((sha, out.trim().to_string()))
}

/// Line counts for every text file at `rev`, in one `git grep` pass:
/// `--count` with an empty pattern counts every line, `-I` skips binary
/// blobs. Returns the resolved sha and `path -> lines`. Files `grep`
/// skips (binary, empty) are absent: callers treat a missing entry as
/// unknown, not zero.
pub async fn line_counts(
    bare: &Path,
    rev: Option<&str>,
) -> anyhow::Result<(String, std::collections::HashMap<String, u64>)> {
    let Some(sha) = resolve_rev(bare, rev).await? else {
        anyhow::bail!("app has no commits yet");
    };
    let gd = git_dir(bare);
    let run = run_capped(
        &[&gd, "grep", "--count", "-I", "-e", "", &sha, "--", "."],
        MAX_GIT_OUTPUT_BYTES,
    )
    .await?;
    // Exit 1 is "no matches" (an empty tree): not a failure.
    if run.failed() && run.status.code() != Some(1) {
        anyhow::bail!("git grep --count failed\n{}", run.stderr);
    }
    let mut counts = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&run.stdout).lines() {
        // `<sha>:<path>:<count>`.
        let rest = line.strip_prefix(&sha).and_then(|r| r.strip_prefix(':'));
        let Some(rest) = rest else { continue };
        let Some(colon) = rest.rfind(':') else {
            continue;
        };
        let (path, count) = rest.split_at(colon);
        if let Ok(n) = count[1..].parse::<u64>() {
            counts.insert(path.to_string(), n);
        }
    }
    Ok((sha, counts))
}

/// Name-status pairs (`path`, status char) between two commits, in
/// `git diff` order. Renames land on the new path with status `R`;
/// callers map the codes they care about.
pub async fn changed_paths(
    bare: &Path,
    from: &str,
    to: &str,
) -> anyhow::Result<Vec<(String, char)>> {
    for rev in [from, to] {
        validate_sha(rev)?;
    }
    let gd = git_dir(bare);
    let out = run_git(
        &[&gd, "diff", "--no-ext-diff", "--name-status", from, to],
        &[],
        GIT_TIMEOUT,
    )
    .await?;
    let mut out_paths = Vec::new();
    for line in out.lines() {
        let mut parts = line.split('\t');
        let Some(code) = parts.next() else { continue };
        let status = code.chars().next().unwrap_or('M');
        // Renames carry `old new`: the change lands on the new path.
        let path = parts.next_back().unwrap_or("").to_string();
        if !path.is_empty() {
            out_paths.push((path, status));
        }
    }
    Ok(out_paths)
}

/// Largest single git stdout kept in memory: `grep`, `ls-tree` and `show`
/// stream through this cap and kill the child when it fills, so a hostile
/// tree cannot OOM the platform through an unbounded `wait_with_output`.
pub const MAX_GIT_OUTPUT_BYTES: usize = 1024 * 1024;

/// One capped git run: the stdout bytes, whether the cap cut the stream
/// (the child was killed), the exit status, and a capped stderr for errors.
/// When cut, bytes after the last newline are dropped so callers never see
/// a partial line as a real entry.
struct CappedOutput {
    stdout: Vec<u8>,
    truncated: bool,
    status: std::process::ExitStatus,
    stderr: String,
}

impl CappedOutput {
    /// A real failure: we only kill the child ourselves when the cap hits,
    /// so a cut stream is never an error even though its status is "killed".
    fn failed(&self) -> bool {
        !self.truncated && !self.status.success()
    }
}
async fn run_capped(args: &[&str], cap: usize) -> anyhow::Result<CappedOutput> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt as _;
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("spawn git: {e}"))?;
    let mut stdout = Vec::new();
    let mut truncated = false;
    if let Some(pipe) = child.stdout.take() {
        let mut limited = pipe.take(cap as u64 + 1);
        tokio::time::timeout(GIT_TIMEOUT, limited.read_to_end(&mut stdout))
            .await
            .map_err(|_| anyhow::anyhow!("git {args:?} timed out"))?
            .map_err(|e| anyhow::anyhow!("git {args:?} stdout: {e}"))?;
        if stdout.len() > cap {
            stdout.truncate(cap);
            truncated = true;
            if let Some(pos) = stdout.iter().rposition(|&b| b == b'\n') {
                stdout.truncate(pos + 1);
            } else {
                stdout.clear();
            }
        }
    }
    // Kill only a child the cap cut off. One that reached EOF on its own may
    // still be between closing stdout and exiting: a SIGKILL in that gap
    // turns a clean exit (or grep's "no match" 1) into "killed by signal".
    if truncated {
        let _ = child.start_kill();
    }
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
        .await
        .map_err(|_| anyhow::anyhow!("git {args:?} would not die"))?
        .map_err(|e| anyhow::anyhow!("git {args:?} wait: {e}"))?;
    let mut stderr = Vec::new();
    if let Some(pipe) = child.stderr.take() {
        let mut limited = pipe.take(4096 + 1);
        let _ = limited.read_to_end(&mut stderr).await;
        if stderr.len() > 4096 {
            stderr.truncate(4096);
        }
    }
    Ok(CappedOutput {
        stdout,
        truncated,
        status,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// `skip` commits past the tip of `main`, then `limit` commits, optionally
/// touching `path`. Returns the entries and the `next_skip` cursor.
pub async fn log(
    bare: &Path,
    path: Option<&str>,
    skip: usize,
    limit: usize,
) -> anyhow::Result<(Option<String>, Vec<LogEntry>, usize)> {
    if let Some(path) = path {
        validate_read_path(path)?;
    }
    let Some(head) = super::manifest::head_of(bare).await? else {
        return Ok((None, Vec::new(), 0));
    };
    let gd = git_dir(bare);
    let skip_s = skip.to_string();
    let limit_s = format!("-{}", limit.clamp(1, MAX_LOG_COMMITS));
    let mut args = vec![
        gd.as_str(),
        "log",
        "--format=%H%x00%an%x00%ae%x00%at%x00%B%x1e",
        &limit_s,
        "--skip",
        &skip_s,
        "main",
    ];
    if let Some(path) = path {
        args.push("--");
        args.push(path);
    }
    let out = run_git(&args, &[], GIT_TIMEOUT).await?;
    let mut entries = Vec::new();
    for record in out.split('\x1e') {
        let record = record.trim_start_matches('\n');
        if record.trim().is_empty() {
            continue;
        }
        let mut parts = record.splitn(5, '\0');
        let (Some(sha), Some(name), Some(email), Some(at), Some(message)) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            continue;
        };
        entries.push(LogEntry {
            sha: sha.to_string(),
            author_name: name.to_string(),
            author_email: email.to_string(),
            at: at.parse().unwrap_or(0),
            message: message.trim().to_string(),
        });
    }
    let next_skip = skip + entries.len();
    Ok((Some(head), entries, next_skip))
}

/// `sha`'s metadata, changed files and unified patch (capped at
/// [`MAX_PATCH_BYTES`]).
pub async fn show(bare: &Path, sha: &str) -> anyhow::Result<ShowResult> {
    validate_sha(sha)?;
    let resolved = run_git(
        &[
            &git_dir(bare),
            "rev-parse",
            "--verify",
            &format!("{sha}^{{commit}}"),
        ],
        &[],
        GIT_TIMEOUT,
    )
    .await
    .map_err(|_| anyhow::anyhow!("unknown commit {sha:?}"))?;
    let sha = resolved.trim().to_string();
    let gd = git_dir(bare);
    let meta = run_git(
        &[
            &gd,
            "log",
            "-1",
            "--format=%H%x00%an%x00%ae%x00%at%x00%B%x1e",
            &sha,
        ],
        &[],
        GIT_TIMEOUT,
    )
    .await?;
    // Git appends a newline after the format, so the record ends with
    // `\n\x1e\n`: strip newlines and the separator together.
    let record = meta
        .trim_start_matches('\n')
        .trim_end_matches(['\n', '\x1e']);
    // The format emits the sha first (mirroring [`log`]); the caller
    // already holds the resolved sha, so skip it before the metadata.
    let mut parts = record.splitn(6, '\0');
    let _emitted_sha = parts.next();
    let commit = LogEntry {
        sha: sha.clone(),
        author_name: parts.next().unwrap_or("").to_string(),
        author_email: parts.next().unwrap_or("").to_string(),
        at: parts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
        message: parts.next().unwrap_or("").trim().to_string(),
    };
    let files_run = run_capped(
        &[
            &gd,
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            &sha,
        ],
        MAX_GIT_OUTPUT_BYTES,
    )
    .await?;
    if files_run.failed() {
        anyhow::bail!("git diff-tree failed\n{}", files_run.stderr);
    }
    let files: Vec<String> = String::from_utf8_lossy(&files_run.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let patch_run = run_capped(
        &[
            &gd,
            "show",
            "--format=",
            "--no-ext-diff",
            "--unified=3",
            &sha,
        ],
        MAX_GIT_OUTPUT_BYTES,
    )
    .await?;
    if patch_run.failed() {
        anyhow::bail!("git show failed\n{}", patch_run.stderr);
    }
    let patch = String::from_utf8_lossy(&patch_run.stdout).into_owned();
    let (patch, cut) = truncate_utf8(&patch, MAX_PATCH_BYTES);
    Ok(ShowResult {
        commit,
        files,
        patch,
        truncated: patch_run.truncated || cut,
    })
}
/// The first parent of `sha`, or `None` for a root commit. Errors on an
/// unknown sha, like [`show`].
pub async fn parent_of(bare: &Path, sha: &str) -> anyhow::Result<Option<String>> {
    validate_sha(sha)?;
    let resolved = run_git(
        &[
            &git_dir(bare),
            "rev-parse",
            "--verify",
            &format!("{sha}^{{commit}}"),
        ],
        &[],
        GIT_TIMEOUT,
    )
    .await
    .map_err(|_| anyhow::anyhow!("unknown commit {sha:?}"))?;
    let sha = resolved.trim().to_string();
    let gd = git_dir(bare);
    match run_git(
        &[&gd, "rev-parse", "--verify", &format!("{sha}^")],
        &[],
        GIT_TIMEOUT,
    )
    .await
    {
        Ok(out) => Ok(Some(out.trim().to_string())),
        Err(_) => Ok(None),
    }
}

/// `sha`'s changed files vs its parent (vs the empty tree for a root
/// commit), each with status and +/- counts.
pub async fn show_stats(bare: &Path, sha: &str) -> anyhow::Result<Vec<FileStat>> {
    validate_sha(sha)?;
    let resolved = run_git(
        &[
            &git_dir(bare),
            "rev-parse",
            "--verify",
            &format!("{sha}^{{commit}}"),
        ],
        &[],
        GIT_TIMEOUT,
    )
    .await
    .map_err(|_| anyhow::anyhow!("unknown commit {sha:?}"))?;
    stats_against_parent(bare, resolved.trim()).await
}

/// The changed files and unified patch between `from` and `to` (both shas,
/// resolved to commits first). The patch is cut at [`MAX_PATCH_BYTES`].
/// Returns the resolved shas, the per-file stats, the patch and whether the
/// patch was cut.
pub async fn diff_range(
    bare: &Path,
    from: &str,
    to: &str,
) -> anyhow::Result<(String, String, Vec<FileStat>, String, bool)> {
    for rev in [from, to] {
        validate_sha(rev)?;
    }
    let gd = git_dir(bare);
    let mut resolved = Vec::with_capacity(2);
    for rev in [from, to] {
        let out = run_git(
            &[&gd, "rev-parse", "--verify", &format!("{rev}^{{commit}}")],
            &[],
            GIT_TIMEOUT,
        )
        .await
        .map_err(|_| anyhow::anyhow!("unknown commit {rev:?}"))?;
        resolved.push(out.trim().to_string());
    }
    let stats = stats_range(bare, &resolved[0], &resolved[1]).await?;
    let patch_run = run_capped(
        &[
            &gd,
            "diff",
            "--no-ext-diff",
            "--unified=3",
            &resolved[0],
            &resolved[1],
        ],
        MAX_GIT_OUTPUT_BYTES,
    )
    .await?;
    if patch_run.failed() {
        anyhow::bail!("git diff failed\n{}", patch_run.stderr);
    }
    let patch = String::from_utf8_lossy(&patch_run.stdout).into_owned();
    let (patch, cut) = truncate_utf8(&patch, MAX_PATCH_BYTES);
    Ok((
        resolved[0].clone(),
        resolved[1].clone(),
        stats,
        patch,
        patch_run.truncated || cut,
    ))
}

/// One commit's stats against its parent, or against the empty tree for a
/// root commit (`diff-tree --root`).
async fn stats_against_parent(bare: &Path, sha: &str) -> anyhow::Result<Vec<FileStat>> {
    let gd = git_dir(bare);
    let numstat = run_capped(
        &[
            &gd,
            "diff-tree",
            "--no-commit-id",
            "--numstat",
            "-r",
            "--root",
            sha,
        ],
        MAX_GIT_OUTPUT_BYTES,
    )
    .await?;
    if numstat.failed() {
        anyhow::bail!("git numstat failed\n{}", numstat.stderr);
    }
    let status = run_capped(
        &[
            &gd,
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            "--root",
            sha,
        ],
        MAX_GIT_OUTPUT_BYTES,
    )
    .await?;
    if status.failed() {
        anyhow::bail!("git name-status failed\n{}", status.stderr);
    }
    Ok(join_stats(&numstat.stdout, &status.stdout))
}

/// A `from to` pair's stats through `git diff`.
async fn stats_range(bare: &Path, from: &str, to: &str) -> anyhow::Result<Vec<FileStat>> {
    let gd = git_dir(bare);
    let numstat = run_capped(
        &[&gd, "diff", "--no-ext-diff", "--numstat", from, to],
        MAX_GIT_OUTPUT_BYTES,
    )
    .await?;
    if numstat.failed() {
        anyhow::bail!("git numstat failed\n{}", numstat.stderr);
    }
    let status = run_capped(
        &[&gd, "diff", "--no-ext-diff", "--name-status", from, to],
        MAX_GIT_OUTPUT_BYTES,
    )
    .await?;
    if status.failed() {
        anyhow::bail!("git name-status failed\n{}", status.stderr);
    }
    Ok(join_stats(&numstat.stdout, &status.stdout))
}

/// Join one `numstat` run (`added removed path`, `- -` for binary) with one
/// `name-status` run (`STATUS path`, renames as `R100 old new`) by path.
/// The listing follows `name-status` order; a stat without counts keeps
/// zero counts rather than dropping the file.
fn join_stats(numstat: &[u8], status: &[u8]) -> Vec<FileStat> {
    let mut counts: std::collections::HashMap<String, (u64, u64, bool)> =
        std::collections::HashMap::new();
    for line in String::from_utf8_lossy(numstat).lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if added == "-" || removed == "-" {
            counts.insert(path.to_string(), (0, 0, true));
        } else if let (Ok(added), Ok(removed)) = (added.parse(), removed.parse()) {
            counts.insert(path.to_string(), (added, removed, false));
        }
    }
    let mut stats = Vec::new();
    for line in String::from_utf8_lossy(status).lines() {
        let mut parts = line.split('\t');
        let Some(code) = parts.next() else { continue };
        let status = code.chars().next().unwrap_or('M');
        // Renames carry `old new`: the change lands on the new path.
        let path = parts.next_back().unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }
        let (added, removed, binary) = counts.get(&path).copied().unwrap_or((0, 0, false));
        stats.push(FileStat {
            path,
            status,
            added,
            removed,
            binary,
        });
    }
    stats
}

/// The blob size at `rev:path` without loading it, so callers can name the
/// size when refusing an over-cap read.
pub async fn blob_size(
    bare: &Path,
    rev: Option<&str>,
    path: &str,
) -> anyhow::Result<(String, u64)> {
    validate_read_path(path)?;
    let Some(sha) = resolve_rev(bare, rev).await? else {
        anyhow::bail!("app has no commits yet");
    };
    let gd = git_dir(bare);
    let spec = format!("{sha}:{path}");
    let size_out = run_git(&[&gd, "cat-file", "-s", &spec], &[], GIT_TIMEOUT)
        .await
        .map_err(|_| anyhow::anyhow!("{path} not found"))?;
    let size: u64 = size_out.trim().parse().unwrap_or(u64::MAX);
    Ok((sha, size))
}

/// Cut `s` at `cap` bytes on a UTF-8 boundary.
fn truncate_utf8(s: &str, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s.to_string(), false);
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::commit::{self, Author, CommitRequest, FileWrite, WriteBody};
    use crate::git::manifest::{self, GitSync};
    use crate::store::Store;

    fn author() -> Author {
        Author {
            name: "ana".to_string(),
            email: "id@example.com".to_string(),
        }
    }

    fn write(path: &str, content: &str) -> FileWrite {
        FileWrite {
            path: path.to_string(),
            body: WriteBody::Text(content.to_string()),
        }
    }

    async fn seed() -> (
        tempfile::TempDir,
        super::super::manifest::GitConfig,
        Store,
        GitSync,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = super::super::manifest::GitConfig::new(dir.path().join("git"), None);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, "grocery")
            .await
            .unwrap();
        let first = CommitRequest {
            message: "first".to_string(),
            base: None,
            writes: vec![
                write("pages/index.tsx", "<h1>Groceries</h1>\n"),
                write("pages/about.ts", "about\n"),
            ],
            edits: vec![],
            deletes: vec![],
            moves: vec![],
            author: author(),
        };
        commit::commit(&store, &cfg, &sync, "grocery", first)
            .await
            .unwrap();
        (dir, cfg, store, sync)
    }

    async fn bare(
        dir: &tempfile::TempDir,
        cfg: &super::super::manifest::GitConfig,
        store: &Store,
        sync: &GitSync,
    ) -> std::path::PathBuf {
        manifest::ensure_synced(store, cfg, sync, "grocery")
            .await
            .unwrap();
        let _ = dir;
        manifest::mirror_path(cfg, "grocery")
    }

    #[tokio::test]
    async fn list_and_read_roundtrip() {
        let (dir, cfg, store, sync) = seed().await;
        let bare = bare(&dir, &cfg, &store, &sync).await;
        let (sha, files, truncated) = list_files(&bare, None, None, 100).await.unwrap();
        assert!(!sha.is_empty() && !truncated);
        assert_eq!(files.len(), 2);
        let (same, bytes) = read_blob(&bare, None, "pages/index.tsx").await.unwrap();
        assert_eq!(same, sha);
        assert_eq!(bytes, b"<h1>Groceries</h1>\n");
        let err = read_blob(&bare, None, "missing.ts").await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn grep_log_show_roundtrip() {
        let (dir, cfg, store, sync) = seed().await;
        let bare = bare(&dir, &cfg, &store, &sync).await;
        let (sha, files, truncated) =
            grep_search(&bare, None, "Groceries", &GrepOptions::default())
                .await
                .unwrap();
        assert!(!truncated && files.len() == 1);
        assert_eq!(files[0].path, "pages/index.tsx");
        assert_eq!(files[0].matches[0].line, 1);
        let (_, none, _) = grep_search(&bare, None, "zzz-no-match", &GrepOptions::default())
            .await
            .unwrap();
        assert!(none.is_empty());
        let (head, entries, next) = log(&bare, None, 0, 10).await.unwrap();
        assert_eq!(head.as_deref(), Some(sha.as_str()));
        assert_eq!(entries.len(), 1);
        assert_eq!(next, 1);
        let shown = show(&bare, &sha).await.unwrap();
        assert_eq!(shown.commit.sha, sha);
        assert_eq!(shown.commit.author_name, "ana");
        assert_eq!(shown.commit.author_email, "id@example.com");
        assert!(shown.commit.at > 0);
        assert_eq!(shown.commit.message, "first");
        assert_eq!(shown.files.len(), 2);
        assert!(shown.patch.contains("Groceries"));
        assert!(!shown.truncated);
    }

    #[tokio::test]
    async fn grep_streaming_caps_a_flood_of_matches() {
        let (dir, cfg, store, sync) = seed().await;
        // 12 files of ~144 KiB, every line matching: ~1.7 MiB of grep
        // output, far past the 1 MiB stream cap.
        let big: Vec<crate::git::commit::FileWrite> = (0..12)
            .map(|i| write(&format!("big{i}.txt"), &"match me\n".repeat(16_000)))
            .collect();
        commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "flood".to_string(),
                base: None,
                writes: big,
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let bare = bare(&dir, &cfg, &store, &sync).await;
        let (flood_sha, files, truncated) =
            grep_search(&bare, None, "match me", &GrepOptions::default())
                .await
                .unwrap();
        let hits: Vec<_> = files.iter().flat_map(|f| &f.matches).collect();
        assert_eq!(hits.len(), MAX_GREP_MATCHES);
        assert!(truncated, "the 1 MiB stream cap must cut the flood");
        assert!(
            files.iter().all(
                |f| f.path.starts_with("big") && f.matches.iter().all(|h| h.text == "match me")
            ),
            "only whole lines before the cap count as hits"
        );
        // The same flood through `show`: the streamed patch is cut too.
        let shown = show(&bare, &flood_sha).await.unwrap();
        assert_eq!(shown.files.len(), 12);
        assert!(shown.truncated);
        assert!(shown.patch.len() <= MAX_PATCH_BYTES);
    }

    #[tokio::test]
    async fn read_path_validation() {
        assert!(validate_read_path("../x").is_err());
        assert!(validate_read_path(".git/config").is_err());
        assert!(validate_read_path("/abs").is_err());
        assert!(validate_read_path("-rf").is_err());
        assert!(validate_read_path("pages/a.tsx").is_ok());
        assert!(validate_sha("abc").is_err());
        assert!(validate_sha("xyz!!").is_err());
        assert!(validate_sha("-5").is_err());
        assert!(validate_sha("a3f2c91d5e7b4a6c8f0e2d4b6a8c0e2f4a6b8d0e1").is_ok());
    }

    async fn seed_two() -> (
        tempfile::TempDir,
        super::super::manifest::GitConfig,
        Store,
        GitSync,
        String,
        String,
    ) {
        let (dir, cfg, store, sync) = seed().await;
        let second = CommitRequest {
            message: "second".to_string(),
            base: None,
            writes: vec![write("pages/new.txt", "new\n")],
            edits: vec![crate::git::commit::Edit {
                path: "pages/index.tsx".to_string(),
                old: "<h1>Groceries</h1>\n".to_string(),
                new: "<h1>Groceries</h1>\n<p>fresh</p>\n".to_string(),
                replace_all: false,
            }],
            deletes: vec!["pages/about.ts".to_string()],
            moves: vec![],
            author: author(),
        };
        let c2 = commit::commit(&store, &cfg, &sync, "grocery", second)
            .await
            .unwrap();
        let (_, entries, _) = log(&manifest::mirror_path(&cfg, "grocery"), None, 1, 1)
            .await
            .unwrap();
        (dir, cfg, store, sync, entries[0].sha.clone(), c2)
    }

    #[tokio::test]
    async fn stats_count_add_modify_delete() {
        let (dir, cfg, store, sync, c1, c2) = seed_two().await;
        let bare = bare(&dir, &cfg, &store, &sync).await;
        assert_eq!(
            parent_of(&bare, &c2).await.unwrap().as_deref(),
            Some(c1.as_str())
        );
        assert_eq!(parent_of(&bare, &c1).await.unwrap(), None);
        let mut stats = show_stats(&bare, &c2).await.unwrap();
        stats.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(stats.len(), 3);
        let edited = stats.iter().find(|s| s.path == "pages/index.tsx").unwrap();
        assert_eq!(edited.status, 'M');
        assert_eq!((edited.added, edited.removed), (1, 0));
        assert!(!edited.binary);
        let deleted = stats.iter().find(|s| s.path == "pages/about.ts").unwrap();
        assert_eq!(deleted.status, 'D');
        assert_eq!((deleted.added, deleted.removed), (0, 1));
        let added = stats.iter().find(|s| s.path == "pages/new.txt").unwrap();
        assert_eq!(added.status, 'A');
        assert_eq!((added.added, added.removed), (1, 0));
    }

    #[tokio::test]
    async fn diff_range_matches_commit_stats() {
        let (dir, cfg, store, sync, c1, c2) = seed_two().await;
        let bare = bare(&dir, &cfg, &store, &sync).await;
        let (from, to, stats, patch, truncated) = diff_range(&bare, &c1, &c2).await.unwrap();
        assert_eq!(from, c1);
        assert_eq!(to, c2);
        assert_eq!(stats.len(), 3);
        assert!(patch.contains("pages/new.txt") && patch.contains("fresh"));
        assert!(!truncated);
        // The same commit against itself: nothing changed, empty patch.
        let (_, _, same_stats, same_patch, _) = diff_range(&bare, &c2, &c2).await.unwrap();
        assert!(same_stats.is_empty());
        assert!(same_patch.is_empty());
        let err = diff_range(&bare, &c1, "0123456789abcdef0123456789abcdef01234567")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown commit"));
        let err = show_stats(&bare, "not-a-sha!!").await.unwrap_err();
        assert!(err.to_string().contains("not a commit sha"));
    }

    #[tokio::test]
    async fn binary_and_oversized_blobs_report_size() {
        use base64::Engine as _;
        let (dir, cfg, store, sync) = seed().await;
        let engine = base64::engine::general_purpose::STANDARD;
        let small_bin = vec![0u8, 1, 2, 3, 255, 10];
        let big = vec![b'x'; MAX_READ_BYTES + 1];
        commit::commit(
            &store,
            &cfg,
            &sync,
            "grocery",
            CommitRequest {
                message: "blobs".to_string(),
                base: None,
                writes: vec![
                    FileWrite {
                        path: "img.bin".to_string(),
                        body: WriteBody::Base64(engine.encode(&small_bin)),
                    },
                    FileWrite {
                        path: "big.bin".to_string(),
                        body: WriteBody::Base64(engine.encode(&big)),
                    },
                ],
                edits: vec![],
                deletes: vec![],
                moves: vec![],
                author: author(),
            },
        )
        .await
        .unwrap();
        let bare = bare(&dir, &cfg, &store, &sync).await;
        let (sha, bytes) = read_blob(&bare, None, "img.bin").await.unwrap();
        assert_eq!(bytes, small_bin);
        assert!(bytes.contains(&0), "callers detect binary by the NUL byte");
        let (_, size) = blob_size(&bare, Some(&sha), "img.bin").await.unwrap();
        assert_eq!(size, small_bin.len() as u64);
        let err = read_blob(&bare, None, "big.bin").await.unwrap_err();
        assert!(
            err.to_string().contains("over the"),
            "over-cap reads refuse before loading, got: {err}"
        );
        let (_, size) = blob_size(&bare, None, "big.bin").await.unwrap();
        assert_eq!(size, big.len() as u64);
        let mut stats = show_stats(&bare, &sha).await.unwrap();
        stats.sort_by(|a, b| a.path.cmp(&b.path));
        let bin = stats.iter().find(|s| s.path == "img.bin").unwrap();
        assert!(bin.binary && bin.status == 'A');
        let missing = blob_size(&bare, None, "nope.bin").await.unwrap_err();
        assert!(missing.to_string().contains("not found"));
    }

    #[test]
    fn glob_matching_rules() {
        // No `/`: the basename matches at any depth.
        assert!(match_glob("*.ts", "a.ts"));
        assert!(match_glob("*.ts", "src/nested/a.ts"));
        assert!(!match_glob("*.tsx", "src/a.ts.bak"));
        // With `/`: `*` stays inside one segment, `**/` crosses.
        assert!(match_glob("src/*.ts", "src/a.ts"));
        assert!(!match_glob("src/*.ts", "src/nested/a.ts"));
        assert!(match_glob("src/**/*.ts", "src/nested/a.ts"));
        assert!(match_glob("**/*.ts", "a.ts"));
        assert!(match_glob("**/*.ts", "src/a.ts"));
        assert!(match_glob("a?c", "abc"));
        assert!(!match_glob("a?c", "ac"));
        assert!(validate_glob("src/*.ts").is_ok());
        assert!(validate_glob("").is_err());
        assert!(validate_glob("/abs").is_err());
        assert!(validate_glob("../x").is_err());
        assert!(validate_glob(&"x/".repeat(200)).is_err());
    }

    #[test]
    fn generated_detection() {
        assert!(is_generated("node_modules/pkg/index.js"));
        assert!(is_generated("dist/bundle.js"));
        assert!(is_generated("a/dist/b.js"));
        assert!(is_generated("package-lock.json"));
        assert!(is_generated("sub/Cargo.lock"));
        assert!(is_generated("bun.lockb"));
        assert!(!is_generated("src/app.ts"));
        assert!(!is_generated("distribute/x.js"));
        assert!(!is_generated("package.json"));
    }
}
