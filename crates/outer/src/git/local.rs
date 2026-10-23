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
/// Most matches [`grep`] returns (spec §8.2: `git grep` matches, ≤ 200).
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

/// One `git grep` hit.
#[derive(Debug, Clone)]
pub struct GrepMatch {
    pub path: String,
    pub line: u64,
    pub text: String,
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

/// `git grep` for `pattern` at `rev` (default: head). `regex` selects `-E`,
/// otherwise the query is a fixed string. Returns at most
/// [`MAX_GREP_MATCHES`] hits plus whether more existed.
pub async fn grep(
    bare: &Path,
    rev: Option<&str>,
    pattern: &str,
    regex: bool,
    pathspec: Option<&str>,
) -> anyhow::Result<(String, Vec<GrepMatch>, bool)> {
    anyhow::ensure!(!pattern.is_empty(), "query must not be empty");
    anyhow::ensure!(pattern.len() <= 1024, "query exceeds 1024 characters");
    if regex {
        regex::Regex::new(pattern).map_err(|e| anyhow::anyhow!("bad regex: {e}"))?;
    }
    if let Some(pathspec) = pathspec {
        validate_read_path(pathspec)?;
    }
    let Some(sha) = resolve_rev(bare, rev).await? else {
        anyhow::bail!("app has no commits yet");
    };
    let gd = git_dir(bare);
    let flag = if regex { "-E" } else { "-F" };
    let mut args = vec![
        gd.as_str(),
        "grep",
        "-n",
        "-I",
        "--no-color",
        flag,
        "-e",
        pattern,
        &sha,
    ];
    let prefix_arg;
    if let Some(pathspec) = pathspec {
        prefix_arg = format!("{}/", pathspec.trim_end_matches('/'));
        args.push("--");
        args.push(&prefix_arg);
    } else {
        args.push("--");
        args.push(".");
    }
    // `git grep` exits 1 on no matches: that is an empty result, not a failure.
    // The stream is capped: the child dies at 1 MiB and parsing stops at 200
    // hits, so both paths mark `truncated`.
    let run = run_capped(&args, MAX_GIT_OUTPUT_BYTES).await?;
    let out = if run.failed() {
        if run.status.code() == Some(1) {
            String::new()
        } else {
            anyhow::bail!("git grep failed\n{}", run.stderr)
        }
    } else {
        String::from_utf8_lossy(&run.stdout).into_owned()
    };
    let mut hits = Vec::new();
    let mut truncated = run.truncated;
    for line in out.lines() {
        if hits.len() >= MAX_GREP_MATCHES {
            truncated = true;
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
        let text = text[1..].chars().take(500).collect::<String>();
        hits.push(GrepMatch {
            path: path.to_string(),
            line: lineno.parse().unwrap_or(0),
            text,
        });
    }
    Ok((sha, hits, truncated))
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
    let record = meta.trim_start_matches('\n').trim_end_matches('\x1e');
    let mut parts = record.splitn(5, '\0');
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
        let cfg = super::super::manifest::GitConfig {
            repos_dir: dir.path().join("git"),
        };
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
        let (sha, hits, truncated) = grep(&bare, None, "Groceries", false, None).await.unwrap();
        assert!(!truncated && hits.len() == 1);
        assert_eq!(hits[0].path, "pages/index.tsx");
        assert_eq!(hits[0].line, 1);
        let (_, none, _) = grep(&bare, None, "zzz-no-match", false, None)
            .await
            .unwrap();
        assert!(none.is_empty());
        let (head, entries, next) = log(&bare, None, 0, 10).await.unwrap();
        assert_eq!(head.as_deref(), Some(sha.as_str()));
        assert_eq!(entries.len(), 1);
        assert_eq!(next, 1);
        let shown = show(&bare, &sha).await.unwrap();
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
        let (flood_sha, hits, truncated) =
            grep(&bare, None, "match me", false, None).await.unwrap();
        assert_eq!(hits.len(), MAX_GREP_MATCHES);
        assert!(truncated, "the 1 MiB stream cap must cut the flood");
        assert!(
            hits.iter()
                .all(|h| h.path.starts_with("big") && h.text == "match me"),
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
}
