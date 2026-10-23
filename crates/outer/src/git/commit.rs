// Derived from noite apps/runner/src/host/web_commit.rs @ 58a977a (Apache-2.0).
// Outer change: single ref (main), Store-backed publish via super::manifest,
// exact-match edits, rollback, and import_dir are new; branch/preview paths
// are gone.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::manifest::{self, GitConfig, GitSync};

/// Most files one commit may write or edit, so a folder of uploads fits.
pub const MAX_WRITES_EDITS: usize = 100;
/// Largest utf8 (text) file.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Largest base64 (uploaded) file, decoded.
pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
/// Largest decoded total across one commit's writes and edits.
pub const MAX_COMMIT_BYTES: usize = 25 * 1024 * 1024;
/// Most moves one commit may carry.
pub const MAX_MOVES: usize = 1000;
/// Most deletes one commit may carry.
pub const MAX_DELETES: usize = 1000;
/// Commit message length, in chars.
pub const MAX_MESSAGE_CHARS: usize = 500;
/// Path length ceiling, in chars.
pub const MAX_PATH_CHARS: usize = 512;
/// Who the commit is attributed to. Set through `GIT_AUTHOR_*`/`GIT_COMMITTER_*`.
#[derive(Debug, Clone)]
pub struct Author {
    pub name: String,
    pub email: String,
}

/// How a commit file's content is carried: `Text` is UTF-8, `Base64` raw bytes.
#[derive(Debug, Clone)]
pub enum WriteBody {
    Text(String),
    Base64(String),
}

/// One committed file: validated path plus content.
#[derive(Debug, Clone)]
pub struct FileWrite {
    pub path: String,
    pub body: WriteBody,
}

/// An exact-match replacement in the file at the base commit.
#[derive(Debug, Clone)]
pub struct Edit {
    pub path: String,
    pub old: String,
    pub new: String,
    pub replace_all: bool,
}

/// A blob re-pointed within the tree, without reading its content.
#[derive(Debug, Clone)]
pub struct Move {
    pub from: String,
    pub to: String,
}

/// Everything one server-side commit carries.
#[derive(Debug, Clone)]
pub struct CommitRequest {
    pub message: String,
    pub base: Option<String>,
    pub writes: Vec<FileWrite>,
    pub edits: Vec<Edit>,
    pub deletes: Vec<String>,
    pub moves: Vec<Move>,
    pub author: Author,
}

/// Every way a commit can fail. Each input cap has its own variant with a
/// message an agent can act on; only genuinely unexpected failures are
/// `Internal`.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("commit needs at least one write, edit, move or delete")]
    EmptyChange,
    #[error("commit exceeds {MAX_WRITES_EDITS} combined writes and edits (got {count})")]
    TooManyFiles { count: usize },
    #[error("commit exceeds {MAX_MOVES} moves (got {count})")]
    TooManyMoves { count: usize },
    #[error("commit exceeds {MAX_DELETES} deletes (got {count})")]
    TooManyDeletes { count: usize },
    #[error("commit exceeds {MAX_COMMIT_BYTES} bytes of files (got {bytes})")]
    TotalTooLarge { bytes: usize },
    #[error("{path} exceeds {MAX_TEXT_BYTES} bytes")]
    TextTooLarge { path: String },
    #[error("{path} looks binary (NUL byte in text)")]
    HasNul { path: String },
    #[error("bad base64 in {path}: {source}")]
    BadBase64 {
        path: String,
        #[source]
        source: base64::DecodeError,
    },
    #[error("{path} exceeds {MAX_UPLOAD_BYTES} decoded bytes")]
    FileTooLarge { path: String },
    #[error("commit message must be 1-{MAX_MESSAGE_CHARS} chars")]
    BadMessage,
    #[error("bad author: {0}")]
    BadAuthor(String),
    #[error("bad path {path:?}: {reason}")]
    BadPath { path: String, reason: &'static str },
    #[error("{path} is both written and deleted")]
    BothWrittenAndDeleted { path: String },
    #[error("{path} is written more than once in one commit")]
    DuplicateWrite { path: String },
    #[error("{path} is edited more than once in one commit")]
    DuplicateEdit { path: String },
    #[error("{path} is both written and edited in one commit")]
    BothWrittenAndEdited { path: String },
    #[error("invalid move: {0}")]
    BadMove(String),
    #[error("invalid edit in {path}: {reason}")]
    InvalidEdit { path: String, reason: &'static str },
    #[error("edit in {path} matched {count} times (need exactly 1, or set replace_all)")]
    EditMatch { path: String, count: usize },
    #[error("{path} not found")]
    NotFound { path: String },
    #[error("unknown base {base:?}; main is {current}")]
    UnknownBase { base: String, current: String },
    #[error("tip moved under this commit: main is {current_sha}")]
    Conflict { current_sha: String },
    #[error("cannot roll back to {target}: {reason}")]
    BadRollback {
        target: String,
        reason: &'static str,
    },
    #[error("repo already has commit {current_sha}")]
    AlreadyInitialized { current_sha: String },
    #[error("bad slug {0:?}")]
    BadSlug(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// One decoded file, ready for the scratch index.
struct RawFile {
    path: String,
    bytes: Vec<u8>,
}

/// Commit `req` onto `slug`'s main without a worktree and publish it through
/// the manifest WAL. Returns the new commit sha.
///
/// With `base` set, a moved head fails with `Conflict`; without it, the head
/// is read and CASed against it, so concurrent commits serialize instead of
/// silently dropping each other.
pub async fn commit(
    store: &crate::store::Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
    req: CommitRequest,
) -> Result<String, CommitError> {
    if manifest::validate_slug(slug).is_err() {
        return Err(CommitError::BadSlug(slug.to_string()));
    }
    let (message, mut files) = validate_request(&req)?;
    let bare = manifest::ensure_synced(store, cfg, sync, slug)
        .await
        .map_err(CommitError::Internal)?;
    let head = manifest::head_of(&bare)
        .await
        .map_err(CommitError::Internal)?;
    if let Some(base) = &req.base {
        let full = resolve_commit(&bare, base)
            .await
            .map_err(|_| CommitError::UnknownBase {
                base: base.clone(),
                current: head.clone().unwrap_or_else(|| "<unborn>".to_string()),
            })?;
        if head.as_deref() != Some(full.as_str()) {
            return Err(CommitError::Conflict {
                current_sha: head.clone().unwrap_or_else(|| "<unborn>".to_string()),
            });
        }
    }
    apply_edits(&bare, head.as_deref(), &req.edits, &mut files).await?;
    require_existing(&bare, head.as_deref(), &req.deletes, &req.moves).await?;
    let sha = build_commit(
        &bare,
        head.as_deref(),
        &req.author,
        &message,
        &files,
        &req.moves,
        &req.deletes,
    )
    .await
    .map_err(CommitError::Internal)?;
    publish_mapped(store, cfg, sync, slug, &bare, head.as_deref(), &sha).await
}

/// Make a new commit on main whose tree is `target`'s tree and whose parent is
/// the current head, then publish it. `target` must be an earlier commit on
/// main: the head itself and commits off main are refused. History is never
/// rewritten: the old head stays reachable as the new commit's parent.
pub async fn rollback(
    store: &crate::store::Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
    target: &str,
    author: &Author,
) -> Result<String, CommitError> {
    if manifest::validate_slug(slug).is_err() {
        return Err(CommitError::BadSlug(slug.to_string()));
    }
    validate_author(author)?;
    let bare = manifest::ensure_synced(store, cfg, sync, slug)
        .await
        .map_err(CommitError::Internal)?;
    let head = manifest::head_of(&bare)
        .await
        .map_err(CommitError::Internal)?
        .ok_or_else(|| CommitError::NotFound {
            path: manifest::MAIN_REF.to_string(),
        })?;
    let full = resolve_commit(&bare, target)
        .await
        .map_err(|_| CommitError::UnknownBase {
            base: target.to_string(),
            current: head.clone(),
        })?;
    if full == head {
        return Err(CommitError::BadRollback {
            target: full,
            reason: "target is the current head",
        });
    }
    let git_dir = format!("--git-dir={}", bare.display());
    let is_ancestor = manifest::run_git(
        &[&git_dir, "merge-base", "--is-ancestor", &full, &head],
        &[],
        Duration::from_secs(15),
    )
    .await
    .is_ok();
    if !is_ancestor {
        return Err(CommitError::BadRollback {
            target: full,
            reason: "target is not an ancestor of main",
        });
    }
    let tree = manifest::run_git(
        &[
            &git_dir,
            "rev-parse",
            "--verify",
            &format!("{full}^{{tree}}"),
        ],
        &[],
        Duration::from_secs(15),
    )
    .await
    .map(|s| s.trim().to_string())
    .map_err(CommitError::Internal)?;
    let message = format!("Roll back to {}", full.get(..7).unwrap_or(&full));
    let sha = manifest::run_git(
        &[&git_dir, "commit-tree", &tree, "-p", &head, "-m", &message],
        &author_env(author),
        Duration::from_secs(30),
    )
    .await
    .map(|s| s.trim().to_string())
    .map_err(CommitError::Internal)?;
    publish_mapped(store, cfg, sync, slug, &bare, Some(&head), &sha).await
}

/// Create the first commit on an empty repo from a directory tree, skipping
/// `.git`. Used to seed apps from the vendored template.
pub async fn import_dir(
    store: &crate::store::Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
    dir: &Path,
    message: &str,
    author: &Author,
) -> Result<String, CommitError> {
    if manifest::validate_slug(slug).is_err() {
        return Err(CommitError::BadSlug(slug.to_string()));
    }
    let message = validate_message(message)?;
    validate_author(author)?;
    let bare = manifest::ensure_synced(store, cfg, sync, slug)
        .await
        .map_err(CommitError::Internal)?;
    if let Some(current) = manifest::head_of(&bare)
        .await
        .map_err(CommitError::Internal)?
    {
        return Err(CommitError::AlreadyInitialized {
            current_sha: current,
        });
    }
    let files = collect_dir(dir).map_err(CommitError::Internal)?;
    let sha = build_commit(&bare, None, author, &message, &files, &[], &[])
        .await
        .map_err(CommitError::Internal)?;
    publish_mapped(store, cfg, sync, slug, &bare, None, &sha).await
}

async fn publish_mapped(
    store: &crate::store::Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
    bare: &Path,
    expected_old: Option<&str>,
    sha: &str,
) -> Result<String, CommitError> {
    match manifest::publish(store, cfg, sync, slug, expected_old, sha).await {
        Ok(sha) => Ok(sha),
        Err(e) => {
            if format!("{e:#}").contains("tip moved") {
                let current = manifest::head_of(bare)
                    .await
                    .unwrap_or(None)
                    .unwrap_or_else(|| "<unborn>".to_string());
                return Err(CommitError::Conflict {
                    current_sha: current,
                });
            }
            Err(CommitError::Internal(e))
        }
    }
}

fn validate_message(message: &str) -> Result<String, CommitError> {
    let trimmed = message.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_MESSAGE_CHARS {
        return Err(CommitError::BadMessage);
    }
    Ok(trimmed.to_string())
}

fn validate_author(author: &Author) -> Result<(), CommitError> {
    let name = author.name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.contains(['\n', '\r']) {
        return Err(CommitError::BadAuthor(
            "name must be 1-100 chars, one line".to_string(),
        ));
    }
    if author.email.is_empty()
        || author.email.len() > 254
        || !author.email.contains('@')
        || author.email.contains(['\n', '\r', ' '])
    {
        return Err(CommitError::BadAuthor(
            "email must look like a@b".to_string(),
        ));
    }
    Ok(())
}

/// Reject worktree escapes and git internals before a path touches git.
fn validate_path(path: &str) -> Result<(), CommitError> {
    let bad = |reason: &'static str| CommitError::BadPath {
        path: path.to_string(),
        reason,
    };
    if path.is_empty() {
        return Err(bad("must not be empty"));
    }
    if path.chars().count() > MAX_PATH_CHARS {
        return Err(bad("longer than 512 chars"));
    }
    if path.starts_with('/') {
        return Err(bad("must be relative, not absolute"));
    }
    if path.contains('\\') {
        return Err(bad("must not contain backslashes"));
    }
    if path.contains('\0') {
        return Err(bad("must not contain NUL"));
    }
    for comp in path.split('/') {
        if comp.is_empty() {
            return Err(bad("must not contain empty segments"));
        }
        if comp == "." {
            return Err(bad("must not contain `.`"));
        }
        if comp == ".." {
            return Err(bad("must not escape with `..`"));
        }
        if comp == ".git" {
            return Err(bad("must not touch `.git`"));
        }
    }
    Ok(())
}

/// Reject bad file moves before anything touches git: same path, duplicate
/// ends, a `from` that is also written, edited or deleted, and a `to` that
/// collides with a write, an edit or a delete. A `to` that is another
/// move's `from` stays allowed: that is a chain (or swap), which
/// [`build_commit`] resolves from the parent tree. The parent-tree checks
/// (a `from` must be a blob, a `to` must be free) happen in
/// [`build_commit`].
fn validate_moves(
    moves: &[Move],
    writes: &[FileWrite],
    edits: &[Edit],
    deletes: &[String],
) -> Result<(), CommitError> {
    let mut froms = HashSet::new();
    let mut tos = HashSet::new();
    for m in moves {
        validate_path(&m.from)?;
        validate_path(&m.to)?;
        if m.from == m.to {
            return Err(CommitError::BadMove(format!("{} targets itself", m.from)));
        }
        if !froms.insert(m.from.as_str()) {
            return Err(CommitError::BadMove(format!("{} is moved twice", m.from)));
        }
        if !tos.insert(m.to.as_str()) {
            return Err(CommitError::BadMove(format!("{} is a target twice", m.to)));
        }
        if deletes.contains(&m.from) {
            return Err(CommitError::BadMove(format!("{} is also deleted", m.from)));
        }
        if writes.iter().any(|f| f.path == m.from) || edits.iter().any(|e| e.path == m.from) {
            return Err(CommitError::BadMove(format!("{} is also written", m.from)));
        }
        if writes.iter().any(|f| f.path == m.to) {
            return Err(CommitError::BadMove(format!("{} is also written", m.to)));
        }
        if edits.iter().any(|e| e.path == m.to) {
            return Err(CommitError::BadMove(format!("{} is also edited", m.to)));
        }
        if deletes.contains(&m.to) {
            return Err(CommitError::BadMove(format!("{} is also deleted", m.to)));
        }
    }
    Ok(())
}

fn validate_request(req: &CommitRequest) -> Result<(String, Vec<RawFile>), CommitError> {
    if req.writes.is_empty()
        && req.edits.is_empty()
        && req.moves.is_empty()
        && req.deletes.is_empty()
    {
        return Err(CommitError::EmptyChange);
    }
    if req.writes.len() + req.edits.len() > MAX_WRITES_EDITS {
        return Err(CommitError::TooManyFiles {
            count: req.writes.len() + req.edits.len(),
        });
    }
    if req.moves.len() > MAX_MOVES {
        return Err(CommitError::TooManyMoves {
            count: req.moves.len(),
        });
    }
    if req.deletes.len() > MAX_DELETES {
        return Err(CommitError::TooManyDeletes {
            count: req.deletes.len(),
        });
    }
    let message = validate_message(&req.message)?;
    validate_author(&req.author)?;
    let mut decoded = 0usize;
    let mut files = Vec::with_capacity(req.writes.len());
    let mut seen_writes = HashSet::new();
    for f in &req.writes {
        validate_path(&f.path)?;
        if !seen_writes.insert(f.path.as_str()) {
            return Err(CommitError::DuplicateWrite {
                path: f.path.clone(),
            });
        }
        let bytes = match &f.body {
            WriteBody::Text(text) => {
                if text.len() > MAX_TEXT_BYTES {
                    return Err(CommitError::TextTooLarge {
                        path: f.path.clone(),
                    });
                }
                if text.as_bytes().contains(&0) {
                    return Err(CommitError::HasNul {
                        path: f.path.clone(),
                    });
                }
                text.as_bytes().to_vec()
            }
            WriteBody::Base64(s) => {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(|source| CommitError::BadBase64 {
                        path: f.path.clone(),
                        source,
                    })?;
                if bytes.len() > MAX_UPLOAD_BYTES {
                    return Err(CommitError::FileTooLarge {
                        path: f.path.clone(),
                    });
                }
                bytes
            }
        };
        decoded += bytes.len();
        files.push(RawFile {
            path: f.path.clone(),
            bytes,
        });
    }
    if decoded > MAX_COMMIT_BYTES {
        return Err(CommitError::TotalTooLarge { bytes: decoded });
    }
    for path in &req.deletes {
        validate_path(path)?;
        if req.writes.iter().any(|f| &f.path == path) || req.edits.iter().any(|e| &e.path == path) {
            return Err(CommitError::BothWrittenAndDeleted { path: path.clone() });
        }
    }
    let mut seen_edits = HashSet::new();
    for e in &req.edits {
        validate_path(&e.path)?;
        if !seen_edits.insert(e.path.as_str()) {
            return Err(CommitError::DuplicateEdit {
                path: e.path.clone(),
            });
        }
        if seen_writes.contains(e.path.as_str()) {
            return Err(CommitError::BothWrittenAndEdited {
                path: e.path.clone(),
            });
        }
        if e.old.is_empty() {
            return Err(CommitError::InvalidEdit {
                path: e.path.clone(),
                reason: "`old` must not be empty",
            });
        }
    }
    validate_moves(&req.moves, &req.writes, &req.edits, &req.deletes)?;
    Ok((message, files))
}

/// Apply exact-match edits against the base commit's blobs. `old` must occur
/// exactly once unless `replace_all` (then at least once).
///
/// Edits share the writes' caps: the base blob must fit in
/// [`MAX_TEXT_BYTES`] (checked with `cat-file -s` before reading, so a huge
/// blob never lands in memory), and each edit result must be UTF-8 without
/// NUL, fit in [`MAX_TEXT_BYTES`], and count toward the same
/// [`MAX_COMMIT_BYTES`] total as writes.
async fn apply_edits(
    bare: &Path,
    parent: Option<&str>,
    edits: &[Edit],
    files: &mut Vec<RawFile>,
) -> Result<(), CommitError> {
    if edits.is_empty() {
        return Ok(());
    }
    let git_dir = format!("--git-dir={}", bare.display());
    let mut total: usize = files.iter().map(|f| f.bytes.len()).sum();
    for e in edits {
        let Some(parent) = parent else {
            return Err(CommitError::NotFound {
                path: e.path.clone(),
            });
        };
        let entry = tree_blob(&git_dir, parent, &e.path)
            .await
            .map_err(CommitError::Internal)?
            .ok_or_else(|| CommitError::NotFound {
                path: e.path.clone(),
            })?;
        let size_out = manifest::run_git(
            &[&git_dir, "cat-file", "-s", &entry.sha],
            &[],
            Duration::from_secs(15),
        )
        .await
        .map_err(CommitError::Internal)?;
        let size: usize = size_out
            .trim()
            .parse()
            .map_err(|_| CommitError::Internal(anyhow::anyhow!("bad blob size for {0}", e.path)))?;
        if size > MAX_TEXT_BYTES {
            return Err(CommitError::TextTooLarge {
                path: e.path.clone(),
            });
        }
        let bytes = manifest::run_git_raw(
            &[&git_dir, "cat-file", "-p", &entry.sha],
            &[],
            Duration::from_secs(30),
        )
        .await
        .map_err(CommitError::Internal)?;
        let content = std::str::from_utf8(&bytes).map_err(|_| CommitError::InvalidEdit {
            path: e.path.clone(),
            reason: "file is not UTF-8 text",
        })?;
        let count = content.matches(&e.old).count();
        if count == 0 || (!e.replace_all && count > 1) {
            return Err(CommitError::EditMatch {
                path: e.path.clone(),
                count,
            });
        }
        let replaced = if e.replace_all {
            content.replace(&e.old, &e.new)
        } else {
            content.replacen(&e.old, &e.new, 1)
        };
        if replaced.len() > MAX_TEXT_BYTES {
            return Err(CommitError::TextTooLarge {
                path: e.path.clone(),
            });
        }
        if replaced.as_bytes().contains(&0) {
            return Err(CommitError::HasNul {
                path: e.path.clone(),
            });
        }
        total = total.saturating_add(replaced.len());
        if total > MAX_COMMIT_BYTES {
            return Err(CommitError::TotalTooLarge { bytes: total });
        }
        files.push(RawFile {
            path: e.path.clone(),
            bytes: replaced.into_bytes(),
        });
    }
    Ok(())
}

/// Deletes and move sources must name a file in the base tree. Without this
/// check git's index update drops a missing path silently and the agent never
/// learns its delete did nothing.
async fn require_existing(
    bare: &Path,
    parent: Option<&str>,
    deletes: &[String],
    moves: &[Move],
) -> Result<(), CommitError> {
    let git_dir = format!("--git-dir={}", bare.display());
    for path in deletes.iter().chain(moves.iter().map(|m| &m.from)) {
        let Some(parent) = parent else {
            return Err(CommitError::NotFound { path: path.clone() });
        };
        if tree_blob(&git_dir, parent, path)
            .await
            .map_err(CommitError::Internal)?
            .is_none()
        {
            return Err(CommitError::NotFound { path: path.clone() });
        }
    }
    Ok(())
}

/// `<mode> <sha>` of the blob at `path` in `rev`, or `None` when there is none.
struct TreeEntry {
    mode: String,
    kind: String,
    sha: String,
}

async fn tree_entry(git_dir: &str, rev: &str, path: &str) -> anyhow::Result<Option<TreeEntry>> {
    let listing = manifest::run_git(
        &[git_dir, "ls-tree", rev, "--", &format!(":(literal){path}")],
        &[],
        Duration::from_secs(15),
    )
    .await?;
    let mut fields = listing.split_whitespace();
    let Some(mode) = fields.next() else {
        return Ok(None);
    };
    Ok(Some(TreeEntry {
        mode: mode.to_string(),
        kind: fields.next().unwrap_or_default().to_string(),
        sha: fields.next().unwrap_or_default().to_string(),
    }))
}

async fn tree_blob(git_dir: &str, rev: &str, path: &str) -> anyhow::Result<Option<TreeEntry>> {
    match tree_entry(git_dir, rev, path).await? {
        Some(entry) if entry.kind == "blob" => Ok(Some(entry)),
        _ => Ok(None),
    }
}

/// Tree + commit objects for the changes, in a scratch index so the mirror's
/// own index (there is none) is never touched. Order: moves (re-point blobs
/// read from the parent tree), then writes, then deletes.
#[allow(clippy::too_many_arguments)]
async fn build_commit(
    bare: &Path,
    parent: Option<&str>,
    author: &Author,
    message: &str,
    files: &[RawFile],
    moves: &[Move],
    deletes: &[String],
) -> anyhow::Result<String> {
    let git_dir = format!("--git-dir={}", bare.display());
    static INDEX_SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = INDEX_SEQ.fetch_add(1, Ordering::Relaxed);
    let index = std::env::temp_dir().join(format!(
        "outer-commit-{}-{nanos}-{n}.idx",
        std::process::id()
    ));
    let index_str = index.to_string_lossy().to_string();
    let env_index = [("GIT_INDEX_FILE", index_str.as_str())];
    let result = async {
        // A root commit starts from an empty index (the scratch file does not
        // exist yet); any other commit starts from its parent's tree.
        if let Some(parent) = parent {
            manifest::run_git(
                &[git_dir.as_str(), "read-tree", parent],
                &env_index,
                Duration::from_secs(30),
            )
            .await?;
        }
        // Moves first: read every `from` blob from the parent tree (immutable),
        // then remove all `from`s and add all `to`s in one index update, so a
        // swap or a chain of moves lands on the right blobs.
        let mut removals = String::new();
        let mut additions = String::new();
        for m in moves {
            let Some(parent) = parent else {
                anyhow::bail!("{} not found", m.from);
            };
            let entry = tree_blob(git_dir.as_str(), parent, &m.from)
                .await?
                .ok_or_else(|| anyhow::anyhow!("{} not found", m.from))?;
            // A `to` may only overwrite a path this commit already vacates:
            // another move's `from`, or a delete.
            let vacated = deletes.contains(&m.to) || moves.iter().any(|o| o.from == m.to);
            if !vacated && tree_entry(git_dir.as_str(), parent, &m.to).await?.is_some() {
                anyhow::bail!("{} already exists", m.to);
            }
            removals.push_str(&format!("0 {}\t{}\0", "0".repeat(40), m.from));
            additions.push_str(&format!("{} {}\t{}\0", entry.mode, entry.sha, m.to));
        }
        if !moves.is_empty() {
            let info = format!("{removals}{additions}");
            manifest::run_git_stdin(
                &[git_dir.as_str(), "update-index", "-z", "--index-info"],
                info.as_bytes(),
                &env_index,
                Duration::from_secs(30),
            )
            .await?;
        }
        for f in files {
            // Existing mode wins (executable bit survives); new files 644.
            // `:(literal)` keeps glob chars in names from acting as pathspec.
            let mode = match parent {
                Some(parent) => {
                    let listing = manifest::run_git(
                        &[
                            git_dir.as_str(),
                            "ls-tree",
                            parent,
                            "--",
                            &format!(":(literal){}", f.path),
                        ],
                        &[],
                        Duration::from_secs(15),
                    )
                    .await
                    .unwrap_or_default();
                    listing
                        .split_whitespace()
                        .next()
                        .unwrap_or("100644")
                        .to_string()
                }
                None => "100644".to_string(),
            };
            // Raw bytes, no filters: an upload's content is committed verbatim.
            let blob = manifest::run_git_stdin(
                &[
                    git_dir.as_str(),
                    "hash-object",
                    "-w",
                    "--stdin",
                    "--no-filters",
                ],
                &f.bytes,
                &[],
                Duration::from_secs(60),
            )
            .await?;
            let sha = String::from_utf8(blob)?.trim().to_string();
            manifest::run_git(
                &[
                    git_dir.as_str(),
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("{mode},{sha},{}", f.path),
                ],
                &env_index,
                Duration::from_secs(30),
            )
            .await?;
        }
        // Deletes last, so a moved-then-edited file is not removed after it
        // was written.
        for path in deletes {
            // Only a file in the parent's tree can be deleted; a folder is
            // deleted by naming every file under it, and a root commit has no
            // files to delete.
            let Some(parent) = parent else {
                anyhow::bail!("{path} not found");
            };
            if tree_blob(git_dir.as_str(), parent, path).await?.is_none() {
                anyhow::bail!("{path} not found");
            }
            // `--index-info` (mode 0) removes by name without stat'ing a work
            // tree: the mirror is bare, so `update-index <path>` is refused.
            let removal = format!("0 {}\t{path}\0", "0".repeat(40));
            manifest::run_git_stdin(
                &[git_dir.as_str(), "update-index", "-z", "--index-info"],
                removal.as_bytes(),
                &env_index,
                Duration::from_secs(30),
            )
            .await?;
        }
        let tree = manifest::run_git(
            &[git_dir.as_str(), "write-tree"],
            &env_index,
            Duration::from_secs(30),
        )
        .await?;
        let mut args = vec![git_dir.as_str(), "commit-tree", tree.trim()];
        if let Some(parent) = parent {
            args.push("-p");
            args.push(parent);
        }
        args.push("-m");
        args.push(message);
        let commit = manifest::run_git(&args, &author_env(author), Duration::from_secs(30)).await?;
        Ok::<_, anyhow::Error>(commit.trim().to_string())
    }
    .await;
    let _ = tokio::fs::remove_file(&index).await;
    result
}

fn author_env(author: &Author) -> [(&str, &str); 4] {
    [
        ("GIT_AUTHOR_NAME", author.name.as_str()),
        ("GIT_AUTHOR_EMAIL", author.email.as_str()),
        ("GIT_COMMITTER_NAME", author.name.as_str()),
        ("GIT_COMMITTER_EMAIL", author.email.as_str()),
    ]
}

/// Resolve a full or short sha to a commit sha. The string is validated before
/// git sees it; an unknown ref is an error.
async fn resolve_commit(bare: &Path, rev: &str) -> anyhow::Result<String> {
    if rev.is_empty() || rev.len() > 128 {
        anyhow::bail!("bad revision {rev:?}");
    }
    let git_dir = format!("--git-dir={}", bare.display());
    let out = manifest::run_git(
        &[
            &git_dir,
            "rev-parse",
            "--verify",
            &format!("{rev}^{{commit}}"),
        ],
        &[],
        Duration::from_secs(15),
    )
    .await?;
    Ok(out.trim().to_string())
}

/// Collect a directory tree into files with `/`-separated relative paths,
/// skipping `.git` anywhere and anything that is not a regular file.
fn collect_dir(dir: &Path) -> anyhow::Result<Vec<RawFile>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<RawFile>) -> anyhow::Result<()> {
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            entries.push(entry?.path());
        }
        entries.sort();
        for path in entries {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| anyhow::anyhow!("{e:#}"))?;
            if rel.components().any(|c| c.as_os_str() == ".git") {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                walk(root, &path, out)?;
            } else if meta.is_file() {
                let rel_str = rel
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path {}", rel.display()))?
                    .to_string();
                // Symlink targets and `.git` already excluded; separators are
                // `/` on this platform's template trees.
                let bytes = std::fs::read(&path)?;
                out.push(RawFile {
                    path: rel_str,
                    bytes,
                });
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    async fn setup(slug: &str) -> (tempfile::TempDir, GitConfig, Store, GitSync) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig {
            repos_dir: dir.path().join("git"),
        };
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, slug)
            .await
            .unwrap();
        (dir, cfg, store, sync)
    }

    fn author() -> Author {
        Author {
            name: "Agent".to_string(),
            email: "agent@example.com".to_string(),
        }
    }

    fn req(message: &str) -> CommitRequest {
        CommitRequest {
            message: message.to_string(),
            base: None,
            writes: Vec::new(),
            edits: Vec::new(),
            deletes: Vec::new(),
            moves: Vec::new(),
            author: author(),
        }
    }

    fn write(path: &str, content: &str) -> FileWrite {
        FileWrite {
            path: path.to_string(),
            body: WriteBody::Text(content.to_string()),
        }
    }

    async fn show(bare: &Path, sha: &str, path: &str) -> String {
        let git_dir = format!("--git-dir={}", bare.display());
        manifest::run_git(
            &[&git_dir, "show", &format!("{sha}:{path}")],
            &[],
            Duration::from_secs(15),
        )
        .await
        .unwrap()
    }

    async fn tree_of(bare: &Path, rev: &str) -> String {
        let git_dir = format!("--git-dir={}", bare.display());
        manifest::run_git(
            &[&git_dir, "rev-parse", &format!("{rev}^{{tree}}")],
            &[],
            Duration::from_secs(15),
        )
        .await
        .unwrap()
        .trim()
        .to_string()
    }

    async fn rev_list_count(bare: &Path) -> usize {
        let git_dir = format!("--git-dir={}", bare.display());
        manifest::run_git(
            &[&git_dir, "rev-list", "--count", "main"],
            &[],
            Duration::from_secs(15),
        )
        .await
        .unwrap()
        .trim()
        .parse()
        .unwrap()
    }

    #[tokio::test]
    async fn commit_advances_seq_and_prunes_the_old_bundle() {
        let (_dir, cfg, store, sync) = setup("seq").await;

        let mut r1 = req("first");
        r1.writes.push(write("hello.txt", "one"));
        let h1 = commit(&store, &cfg, &sync, "seq", r1).await.unwrap();

        let m1 = manifest::read_manifest(&store, "seq")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m1.seq, 1);
        let b1 = m1.main.clone().unwrap();
        assert_eq!(b1.sha, h1);
        assert!(
            store.get(&b1.bundle).await.unwrap().is_some(),
            "tip bundle uploaded"
        );

        let mut r2 = req("second");
        r2.writes.push(write("hello.txt", "two"));
        let h2 = commit(&store, &cfg, &sync, "seq", r2).await.unwrap();
        assert_ne!(h1, h2);

        let m2 = manifest::read_manifest(&store, "seq")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m2.seq, 2);
        assert!(
            store.get(&b1.bundle).await.unwrap().is_none(),
            "superseded bundle pruned"
        );
        assert!(store.get(&m2.main.unwrap().bundle).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn rehydrate_into_a_fresh_mirror_reproduces_head_and_files() {
        let (_dir, cfg, store, sync) = setup("rehyd").await;
        let mut r = req("seed");
        r.writes.push(write("app/index.ts", "export const x = 1;"));
        let head = commit(&store, &cfg, &sync, "rehyd", r).await.unwrap();

        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh_cfg = GitConfig {
            repos_dir: fresh_dir.path().join("git"),
        };
        let fresh_sync = GitSync::default();
        let got = manifest::rehydrate(&store, &fresh_cfg, &fresh_sync, "rehyd")
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(head.as_str()));
        let bare = manifest::mirror_path(&fresh_cfg, "rehyd");
        assert_eq!(
            show(&bare, &head, "app/index.ts").await,
            "export const x = 1;"
        );
    }

    #[tokio::test]
    async fn stale_base_conflicts_and_unbased_commits_serialize() {
        let (_dir, cfg, store, sync) = setup("cas").await;
        let mut r1 = req("one");
        r1.writes.push(write("f.txt", "1"));
        let h1 = commit(&store, &cfg, &sync, "cas", r1).await.unwrap();

        let mut r2 = req("two");
        r2.base = Some(h1.clone());
        r2.writes.push(write("f.txt", "2"));
        let h2 = commit(&store, &cfg, &sync, "cas", r2).await.unwrap();
        assert_ne!(h1, h2);

        let mut stale = req("stale");
        stale.base = Some(h1);
        stale.writes.push(write("f.txt", "stale"));
        match commit(&store, &cfg, &sync, "cas", stale).await {
            Err(CommitError::Conflict { current_sha }) => assert_eq!(current_sha, h2),
            other => panic!("a stale base must conflict, got {other:?}"),
        }

        // No base: reads the head and CASes, so the second commit builds on
        // the first instead of dropping it.
        let mut r3 = req("three");
        r3.writes.push(write("g.txt", "3"));
        let h3 = commit(&store, &cfg, &sync, "cas", r3).await.unwrap();
        let bare = manifest::mirror_path(&cfg, "cas");
        assert_eq!(rev_list_count(&bare).await, 3, "history stays linear");
        let git_dir = format!("--git-dir={}", bare.display());
        let parent = manifest::run_git(
            &[&git_dir, "rev-parse", &format!("{h3}^")],
            &[],
            Duration::from_secs(15),
        )
        .await
        .unwrap();
        assert_eq!(parent.trim(), h2);
    }

    #[tokio::test]
    async fn edits_match_exactly_once_by_default() {
        let (_dir, cfg, store, sync) = setup("edits").await;
        let mut seed = req("seed");
        seed.writes
            .push(write("page.tsx", "<h1>x</h1>\n<p>y</p>\n<h1>x</h1>\n"));
        let head = commit(&store, &cfg, &sync, "edits", seed).await.unwrap();
        let bare = manifest::mirror_path(&cfg, "edits");

        // Zero matches: error names the path and the count.
        let mut zero = req("zero");
        zero.edits.push(Edit {
            path: "page.tsx".to_string(),
            old: "missing".to_string(),
            new: "n".to_string(),
            replace_all: false,
        });
        match commit(&store, &cfg, &sync, "edits", zero).await {
            Err(CommitError::EditMatch { path, count }) => {
                assert_eq!(path, "page.tsx");
                assert_eq!(count, 0);
            }
            other => panic!("zero matches must fail, got {other:?}"),
        }

        // Two matches without replace_all: error counts both.
        let mut two = req("two");
        two.edits.push(Edit {
            path: "page.tsx".to_string(),
            old: "<h1>x</h1>".to_string(),
            new: "<h1>z</h1>".to_string(),
            replace_all: false,
        });
        match commit(&store, &cfg, &sync, "edits", two).await {
            Err(CommitError::EditMatch { path, count }) => {
                assert_eq!(path, "page.tsx");
                assert_eq!(count, 2);
            }
            other => panic!("two matches must fail, got {other:?}"),
        }

        // replace_all rewrites both.
        let mut all = req("all");
        all.edits.push(Edit {
            path: "page.tsx".to_string(),
            old: "<h1>x</h1>".to_string(),
            new: "<h1>z</h1>".to_string(),
            replace_all: true,
        });
        let h_all = commit(&store, &cfg, &sync, "edits", all).await.unwrap();
        assert_eq!(
            show(&bare, &h_all, "page.tsx").await,
            "<h1>z</h1>\n<p>y</p>\n<h1>z</h1>\n"
        );

        // Exactly one match without replace_all.
        let mut one = req("one");
        one.base = Some(h_all);
        one.edits.push(Edit {
            path: "page.tsx".to_string(),
            old: "<p>y</p>".to_string(),
            new: "<p>w</p>".to_string(),
            replace_all: false,
        });
        let h_one = commit(&store, &cfg, &sync, "edits", one).await.unwrap();
        assert_eq!(
            show(&bare, &h_one, "page.tsx").await,
            "<h1>z</h1>\n<p>w</p>\n<h1>z</h1>\n"
        );
        assert_eq!(head.len(), 40);
    }

    #[tokio::test]
    async fn caps_reject_just_over_and_accept_just_under() {
        // writes+edits count: 100 passes, 101 fails.
        {
            let (_dir, cfg, store, sync) = setup("cap-count").await;
            let mut ok = req("ok");
            for i in 0..100 {
                ok.writes.push(write(&format!("f{i}.txt"), "x"));
            }
            commit(&store, &cfg, &sync, "cap-count", ok).await.unwrap();
            let mut over = req("over");
            for i in 0..101 {
                over.writes.push(write(&format!("g{i}.txt"), "x"));
            }
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-count", over).await,
                Err(CommitError::TooManyFiles { count: 101 })
            ));
        }
        // text: 256 KiB passes, one byte more fails.
        {
            let (_dir, cfg, store, sync) = setup("cap-text").await;
            let mut ok = req("ok");
            ok.writes.push(FileWrite {
                path: "t.txt".to_string(),
                body: WriteBody::Text("a".repeat(MAX_TEXT_BYTES)),
            });
            commit(&store, &cfg, &sync, "cap-text", ok).await.unwrap();
            let mut over = req("over");
            over.writes.push(FileWrite {
                path: "t.txt".to_string(),
                body: WriteBody::Text("a".repeat(MAX_TEXT_BYTES + 1)),
            });
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-text", over).await,
                Err(CommitError::TextTooLarge { .. })
            ));
            // NUL bytes are refused even under the size cap.
            let mut nul = req("nul");
            nul.writes.push(FileWrite {
                path: "t.txt".to_string(),
                body: WriteBody::Text("a\0b".to_string()),
            });
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-text", nul).await,
                Err(CommitError::HasNul { .. })
            ));
        }
        // base64 per-file: 10 MiB passes, one byte more fails.
        {
            use base64::Engine as _;
            let (_dir, cfg, store, sync) = setup("cap-b64").await;
            let enc = |n: usize| base64::engine::general_purpose::STANDARD.encode(vec![b'a'; n]);
            let mut ok = req("ok");
            ok.writes.push(FileWrite {
                path: "b.bin".to_string(),
                body: WriteBody::Base64(enc(MAX_UPLOAD_BYTES)),
            });
            commit(&store, &cfg, &sync, "cap-b64", ok).await.unwrap();
            let mut over = req("over");
            over.writes.push(FileWrite {
                path: "b.bin".to_string(),
                body: WriteBody::Base64(enc(MAX_UPLOAD_BYTES + 1)),
            });
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-b64", over).await,
                Err(CommitError::FileTooLarge { .. })
            ));
        }
        // total: 25 MiB passes, one byte more fails.
        {
            use base64::Engine as _;
            let (_dir, cfg, store, sync) = setup("cap-total").await;
            let enc = |n: usize| base64::engine::general_purpose::STANDARD.encode(vec![b'a'; n]);
            let mut ok = req("ok");
            ok.writes.push(FileWrite {
                path: "a.bin".to_string(),
                body: WriteBody::Base64(enc(10 * 1024 * 1024)),
            });
            ok.writes.push(FileWrite {
                path: "b.bin".to_string(),
                body: WriteBody::Base64(enc(10 * 1024 * 1024)),
            });
            ok.writes.push(FileWrite {
                path: "c.bin".to_string(),
                body: WriteBody::Base64(enc(5 * 1024 * 1024)),
            });
            commit(&store, &cfg, &sync, "cap-total", ok).await.unwrap();
            let mut over = req("over");
            over.writes.push(FileWrite {
                path: "a.bin".to_string(),
                body: WriteBody::Base64(enc(10 * 1024 * 1024)),
            });
            over.writes.push(FileWrite {
                path: "b.bin".to_string(),
                body: WriteBody::Base64(enc(10 * 1024 * 1024)),
            });
            over.writes.push(FileWrite {
                path: "c.bin".to_string(),
                body: WriteBody::Base64(enc(5 * 1024 * 1024 + 1)),
            });
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-total", over).await,
                Err(CommitError::TotalTooLarge { .. })
            ));
        }
        // moves/deletes: over the cap fails before any tree lookup.
        {
            let (_dir, cfg, store, sync) = setup("cap-mv").await;
            let mut over = req("over");
            over.moves = (0..1001)
                .map(|i| Move {
                    from: format!("a{i}"),
                    to: format!("b{i}"),
                })
                .collect();
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-mv", over).await,
                Err(CommitError::TooManyMoves { count: 1001 })
            ));
            let mut over = req("over");
            over.deletes = (0..1001).map(|i| format!("d{i}")).collect();
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-mv", over).await,
                Err(CommitError::TooManyDeletes { count: 1001 })
            ));
        }
        // message: 500 chars passes, 501 fails.
        {
            let (_dir, cfg, store, sync) = setup("cap-msg").await;
            let mut ok = req(&"m".repeat(500));
            ok.writes.push(write("f.txt", "x"));
            commit(&store, &cfg, &sync, "cap-msg", ok).await.unwrap();
            let mut over = req(&"m".repeat(501));
            over.writes.push(write("f.txt", "x"));
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-msg", over).await,
                Err(CommitError::BadMessage)
            ));
        }
        // path: 512 chars passes, 513 fails.
        {
            let (_dir, cfg, store, sync) = setup("cap-path").await;
            let mut ok = req("ok");
            ok.writes
                .push(write(&format!("{}.txt", "p".repeat(508)), "x"));
            commit(&store, &cfg, &sync, "cap-path", ok).await.unwrap();
            let mut over = req("over");
            over.writes
                .push(write(&format!("{}.txt", "p".repeat(509)), "x"));
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-path", over).await,
                Err(CommitError::BadPath { .. })
            ));
        }
    }

    #[tokio::test]
    async fn bad_paths_are_refused() {
        let (_dir, cfg, store, sync) = setup("paths").await;
        for bad in [
            "../escape.txt",
            "a/../b.txt",
            ".git/config",
            "a/.git/x",
            "/absolute.txt",
            "a\\.txt",
            "",
        ] {
            let mut r = req("bad");
            r.writes.push(write(bad, "x"));
            assert!(
                matches!(
                    commit(&store, &cfg, &sync, "paths", r).await,
                    Err(CommitError::BadPath { .. })
                ),
                "{bad:?} must be refused"
            );
        }
        let mut r = req("missing");
        r.deletes.push("nope.txt".to_string());
        assert!(matches!(
            commit(&store, &cfg, &sync, "paths", r).await,
            Err(CommitError::NotFound { .. })
        ));
        let r = req("empty");
        assert!(matches!(
            commit(&store, &cfg, &sync, "paths", r).await,
            Err(CommitError::EmptyChange)
        ));
    }

    #[tokio::test]
    async fn rollback_restores_the_target_tree_without_rewriting_history() {
        let (_dir, cfg, store, sync) = setup("rollback").await;
        let mut v1 = req("v1");
        v1.writes.push(write("app.ts", "v1"));
        let h1 = commit(&store, &cfg, &sync, "rollback", v1).await.unwrap();
        let mut v2 = req("v2");
        v2.writes.push(write("app.ts", "v2"));
        let h2 = commit(&store, &cfg, &sync, "rollback", v2).await.unwrap();

        let h3 = rollback(&store, &cfg, &sync, "rollback", &h1, &author())
            .await
            .unwrap();
        let bare = manifest::mirror_path(&cfg, "rollback");
        assert_eq!(tree_of(&bare, &h3).await, tree_of(&bare, &h1).await);
        assert_eq!(show(&bare, &h3, "app.ts").await, "v1");
        assert_eq!(
            rev_list_count(&bare).await,
            3,
            "rollback appends, never rewrites"
        );
        let git_dir = format!("--git-dir={}", bare.display());
        let parent = manifest::run_git(
            &[&git_dir, "rev-parse", &format!("{h3}^")],
            &[],
            Duration::from_secs(15),
        )
        .await
        .unwrap();
        assert_eq!(parent.trim(), h2);
    }

    #[tokio::test]
    async fn import_dir_seeds_an_empty_repo_and_skips_git() {
        let (_dir, cfg, store, sync) = setup("import").await;
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("pages")).unwrap();
        std::fs::write(src.path().join("pages/index.tsx"), "export default 1;").unwrap();
        std::fs::write(src.path().join("readme.md"), "hi").unwrap();
        std::fs::create_dir_all(src.path().join(".git/objects")).unwrap();
        std::fs::write(src.path().join(".git/HEAD"), "ref: refs/heads/main").unwrap();

        let head = import_dir(
            &store,
            &cfg,
            &sync,
            "import",
            src.path(),
            "seed template",
            &author(),
        )
        .await
        .unwrap();
        let bare = manifest::mirror_path(&cfg, "import");
        assert_eq!(
            show(&bare, &head, "pages/index.tsx").await,
            "export default 1;"
        );
        assert_eq!(show(&bare, &head, "readme.md").await, "hi");
        let git_dir = format!("--git-dir={}", bare.display());
        let ls = manifest::run_git(
            &[&git_dir, "ls-tree", "-r", "--name-only", &head],
            &[],
            Duration::from_secs(15),
        )
        .await
        .unwrap();
        assert!(!ls.contains(".git"), ".git must not be imported");

        match import_dir(
            &store,
            &cfg,
            &sync,
            "import",
            src.path(),
            "again",
            &author(),
        )
        .await
        {
            Err(CommitError::AlreadyInitialized { current_sha }) => assert_eq!(current_sha, head),
            other => panic!("a second import must fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn edits_share_the_writes_size_caps() {
        // An edit result just under MAX_TEXT_BYTES passes...
        let (_dir, cfg, store, sync) = setup("edit-caps").await;
        let mut seed = req("seed");
        seed.writes.push(write("f.txt", "aaa"));
        commit(&store, &cfg, &sync, "edit-caps", seed)
            .await
            .unwrap();
        let mut ok = req("ok");
        ok.edits.push(Edit {
            path: "f.txt".to_string(),
            old: "aaa".to_string(),
            new: "a".repeat(MAX_TEXT_BYTES),
            replace_all: false,
        });
        commit(&store, &cfg, &sync, "edit-caps", ok).await.unwrap();

        // ...and one byte over fails, against the new base.
        let mut over = req("over");
        over.edits.push(Edit {
            path: "f.txt".to_string(),
            old: "a".repeat(MAX_TEXT_BYTES),
            new: "a".repeat(MAX_TEXT_BYTES + 1),
            replace_all: false,
        });
        match commit(&store, &cfg, &sync, "edit-caps", over).await {
            Err(CommitError::TextTooLarge { path }) => assert_eq!(path, "f.txt"),
            other => panic!("an over-cap edit result must fail, got {other:?}"),
        }

        // A NUL byte in the edit result is refused like a NUL write.
        let (_dir, cfg, store, sync) = setup("edit-nul").await;
        let mut seed = req("seed");
        seed.writes.push(write("f.txt", "aaa"));
        commit(&store, &cfg, &sync, "edit-nul", seed).await.unwrap();
        let mut nul = req("nul");
        nul.edits.push(Edit {
            path: "f.txt".to_string(),
            old: "aaa".to_string(),
            new: "a\0b".to_string(),
            replace_all: false,
        });
        match commit(&store, &cfg, &sync, "edit-nul", nul).await {
            Err(CommitError::HasNul { path }) => assert_eq!(path, "f.txt"),
            other => panic!("a NUL edit result must fail, got {other:?}"),
        }

        // A base file over MAX_TEXT_BYTES is refused before it is read.
        // Seeded through Base64 (a Text write that large is refused up
        // front), so the blob exists but is never editable.
        use base64::Engine as _;
        let (_dir, cfg, store, sync) = setup("edit-big-base").await;
        let mut seed = req("seed");
        seed.writes.push(FileWrite {
            path: "big.txt".to_string(),
            body: WriteBody::Base64(base64::engine::general_purpose::STANDARD.encode(vec![
                b'a';
                MAX_TEXT_BYTES
                    + 1
            ])),
        });
        commit(&store, &cfg, &sync, "edit-big-base", seed)
            .await
            .unwrap();
        let mut big = req("big");
        big.edits.push(Edit {
            path: "big.txt".to_string(),
            old: "a".to_string(),
            new: "b".to_string(),
            replace_all: true,
        });
        match commit(&store, &cfg, &sync, "edit-big-base", big).await {
            Err(CommitError::TextTooLarge { path }) => assert_eq!(path, "big.txt"),
            other => panic!("editing an over-cap base must fail, got {other:?}"),
        }

        // Edit results count toward the same MAX_COMMIT_BYTES total as
        // writes: writes at exactly the cap plus any edit overflow it.
        let (_dir, cfg, store, sync) = setup("edit-total").await;
        let mut seed = req("seed");
        seed.writes.push(write("e.txt", "aaa"));
        commit(&store, &cfg, &sync, "edit-total", seed)
            .await
            .unwrap();
        let enc = |n: usize| base64::engine::general_purpose::STANDARD.encode(vec![b'a'; n]);
        let mut big = req("big");
        big.writes.push(FileWrite {
            path: "a.bin".to_string(),
            body: WriteBody::Base64(enc(10 * 1024 * 1024)),
        });
        big.writes.push(FileWrite {
            path: "b.bin".to_string(),
            body: WriteBody::Base64(enc(10 * 1024 * 1024)),
        });
        big.writes.push(FileWrite {
            path: "c.bin".to_string(),
            body: WriteBody::Base64(enc(5 * 1024 * 1024)),
        });
        big.edits.push(Edit {
            path: "e.txt".to_string(),
            old: "aaa".to_string(),
            new: "bbb".to_string(),
            replace_all: false,
        });
        match commit(&store, &cfg, &sync, "edit-total", big).await {
            Err(CommitError::TotalTooLarge { .. }) => {}
            other => panic!("an edit overflowing the commit total must fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rollback_refuses_head_and_foreign_commits() {
        let (_dir, cfg, store, sync) = setup("rb-guard").await;
        let mut v1 = req("v1");
        v1.writes.push(write("app.ts", "v1"));
        let h1 = commit(&store, &cfg, &sync, "rb-guard", v1).await.unwrap();
        let mut v2 = req("v2");
        v2.writes.push(write("app.ts", "v2"));
        let h2 = commit(&store, &cfg, &sync, "rb-guard", v2).await.unwrap();

        // The head itself: an empty no-op commit is refused.
        match rollback(&store, &cfg, &sync, "rb-guard", &h2, &author()).await {
            Err(CommitError::BadRollback { .. }) => {}
            other => panic!("rollback to head must fail, got {other:?}"),
        }

        // A commit object off main (commit-tree without updating the ref)
        // is not an ancestor of main and is refused.
        let bare = manifest::mirror_path(&cfg, "rb-guard");
        let git_dir = format!("--git-dir={}", bare.display());
        let tree = tree_of(&bare, &h2).await;
        let dangling = manifest::run_git(
            &[&git_dir, "commit-tree", &tree, "-p", &h2, "-m", "dangling"],
            &[
                ("GIT_AUTHOR_NAME", "t"),
                ("GIT_AUTHOR_EMAIL", "t@example.com"),
                ("GIT_COMMITTER_NAME", "t"),
                ("GIT_COMMITTER_EMAIL", "t@example.com"),
            ],
            Duration::from_secs(15),
        )
        .await
        .unwrap();
        let dangling = dangling.trim().to_string();
        match rollback(&store, &cfg, &sync, "rb-guard", &dangling, &author()).await {
            Err(CommitError::BadRollback { .. }) => {}
            other => panic!("rollback off main must fail, got {other:?}"),
        }

        // A real ancestor still succeeds.
        rollback(&store, &cfg, &sync, "rb-guard", &h1, &author())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn overlapping_paths_in_one_commit_are_rejected() {
        let (_dir, cfg, store, sync) = setup("overlap").await;

        // The same path written twice.
        let mut dup = req("dup");
        dup.writes.push(write("f.txt", "one"));
        dup.writes.push(write("f.txt", "two"));
        match commit(&store, &cfg, &sync, "overlap", dup).await {
            Err(CommitError::DuplicateWrite { path }) => assert_eq!(path, "f.txt"),
            other => panic!("duplicate writes must fail, got {other:?}"),
        }

        // A write and an edit on one path.
        let mut both = req("both");
        both.writes.push(write("g.txt", "x"));
        both.edits.push(Edit {
            path: "g.txt".to_string(),
            old: "x".to_string(),
            new: "y".to_string(),
            replace_all: false,
        });
        match commit(&store, &cfg, &sync, "overlap", both).await {
            Err(CommitError::BothWrittenAndEdited { path }) => assert_eq!(path, "g.txt"),
            other => panic!("a write and an edit on one path must fail, got {other:?}"),
        }

        // The same path edited twice.
        let mut dedup = req("dedup");
        dedup.edits.push(Edit {
            path: "h.txt".to_string(),
            old: "x".to_string(),
            new: "y".to_string(),
            replace_all: false,
        });
        dedup.edits.push(Edit {
            path: "h.txt".to_string(),
            old: "y".to_string(),
            new: "z".to_string(),
            replace_all: false,
        });
        match commit(&store, &cfg, &sync, "overlap", dedup).await {
            Err(CommitError::DuplicateEdit { path }) => assert_eq!(path, "h.txt"),
            other => panic!("duplicate edits must fail, got {other:?}"),
        }

        // A move whose target is written in the same commit.
        let mut mv_write = req("mv-write");
        mv_write.writes.push(write("w.txt", "x"));
        mv_write.moves.push(Move {
            from: "a.txt".to_string(),
            to: "w.txt".to_string(),
        });
        match commit(&store, &cfg, &sync, "overlap", mv_write).await {
            Err(CommitError::BadMove(msg)) => assert!(msg.contains("w.txt"), "wrong path: {msg}"),
            other => panic!("a move onto a written path must fail, got {other:?}"),
        }

        // A move whose target is edited in the same commit.
        let mut mv_edit = req("mv-edit");
        mv_edit.edits.push(Edit {
            path: "e.txt".to_string(),
            old: "x".to_string(),
            new: "y".to_string(),
            replace_all: false,
        });
        mv_edit.moves.push(Move {
            from: "a.txt".to_string(),
            to: "e.txt".to_string(),
        });
        match commit(&store, &cfg, &sync, "overlap", mv_edit).await {
            Err(CommitError::BadMove(msg)) => assert!(msg.contains("e.txt"), "wrong path: {msg}"),
            other => panic!("a move onto an edited path must fail, got {other:?}"),
        }

        // A move whose target is deleted in the same commit.
        let mut mv_delete = req("mv-delete");
        mv_delete.deletes.push("d.txt".to_string());
        mv_delete.moves.push(Move {
            from: "a.txt".to_string(),
            to: "d.txt".to_string(),
        });
        match commit(&store, &cfg, &sync, "overlap", mv_delete).await {
            Err(CommitError::BadMove(msg)) => assert!(msg.contains("d.txt"), "wrong path: {msg}"),
            other => panic!("a move onto a deleted path must fail, got {other:?}"),
        }

        // A move chain still succeeds: a `to` that is another move's `from`
        // is how chains (and swaps) name their middle path.
        let mut seed = req("seed");
        seed.writes.push(write("x.txt", "X"));
        seed.writes.push(write("y.txt", "Y"));
        commit(&store, &cfg, &sync, "overlap", seed).await.unwrap();
        let mut chain = req("chain");
        chain.moves.push(Move {
            from: "x.txt".to_string(),
            to: "y.txt".to_string(),
        });
        chain.moves.push(Move {
            from: "y.txt".to_string(),
            to: "z.txt".to_string(),
        });
        let hc = commit(&store, &cfg, &sync, "overlap", chain).await.unwrap();
        let bare = manifest::mirror_path(&cfg, "overlap");
        assert_eq!(show(&bare, &hc, "y.txt").await, "X");
        assert_eq!(show(&bare, &hc, "z.txt").await, "Y");
    }
}
