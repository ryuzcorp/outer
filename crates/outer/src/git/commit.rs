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
/// Line-anchored edits and inserts count here too; a `patch` touches at most
/// this many files.
pub const MAX_WRITES_EDITS: usize = 100;
/// Largest `patch` input, in bytes.
pub const MAX_PATCH_BYTES: usize = 1024 * 1024;
/// Largest utf8 (text) file.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Largest base64 (uploaded) file, decoded.
pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
/// Largest decoded total across one commit's writes and edits (line edits,
/// inserts and patch results count the same as edits).
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
    /// Per-file CAS: the blob id `read_files` reported for this path. A
    /// mismatch fails with [`CommitError::BlobConflict`], so an unrelated
    /// concurrent edit never conflicts. `None` keeps last-writer-wins.
    pub expect_blob: Option<String>,
}

/// An exact-match replacement in the file at the base commit.
#[derive(Debug, Clone)]
pub struct Edit {
    pub path: String,
    pub old: String,
    pub new: String,
    pub replace_all: bool,
    /// Per-file CAS: the blob id `read_files` reported. See [`FileWrite`].
    pub expect_blob: Option<String>,
}

/// A line-range replacement in the file at the base commit. Lines are 1-based
/// and match `read_files` numbering; the range is inclusive, and an empty
/// `new` deletes the lines.
#[derive(Debug, Clone)]
pub struct LineEdit {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub new: String,
    /// Per-file CAS: the blob id `read_files` reported. See [`FileWrite`].
    pub expect_blob: Option<String>,
}

/// An insertion after a line in the file at the base commit. `after_line` 0
/// is the top of the file; `after_line` N (the file's line count) appends.
#[derive(Debug, Clone)]
pub struct Insert {
    pub path: String,
    pub after_line: u64,
    pub text: String,
    /// Per-file CAS: the blob id `read_files` reported. See [`FileWrite`].
    pub expect_blob: Option<String>,
}

/// A blob re-pointed within the tree, without reading its content.
#[derive(Debug, Clone)]
pub struct Move {
    pub from: String,
    pub to: String,
}

/// Everything one server-side commit changes, without the message, head CAS
/// and author. `commit`, `commit_preview` and `stage` share this shape plus
/// [`validate_changes`] and [`apply_changes`], so every path applies the
/// same order with the same errors.
#[derive(Debug, Clone, Default)]
pub struct CommitChanges {
    pub writes: Vec<FileWrite>,
    pub edits: Vec<Edit>,
    pub line_edits: Vec<LineEdit>,
    pub inserts: Vec<Insert>,
    /// One unified diff (or Codex `*** Begin Patch` block). A commit carrying
    /// a patch carries nothing else: mixing is refused with
    /// [`CommitError::MixedPatch`].
    pub patch: Option<String>,
    pub deletes: Vec<String>,
    pub moves: Vec<Move>,
}

/// Everything one server-side commit carries.
#[derive(Debug, Clone)]
pub struct CommitRequest {
    pub message: String,
    pub base: Option<String>,
    pub changes: CommitChanges,
    pub author: Author,
}

/// Every way a commit can fail. Each input cap has its own variant with a
/// message an agent can act on; only genuinely unexpected failures are
/// `Internal`.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("commit needs at least one write, edit, line edit, insert, patch, move or delete")]
    EmptyChange,
    #[error("commit exceeds {MAX_WRITES_EDITS} combined writes, edits, line edits and inserts (got {count})")]
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
    #[error("blob conflict in {path}: expected blob {expected}, current {current}; re-read the file and retry with the fresh blob")]
    BlobConflict {
        path: String,
        expected: String,
        current: String,
    },
    #[error("line range {start_line}-{end_line} in {path} is out of range (the file has {total} lines)")]
    OutOfRange {
        path: String,
        start_line: u64,
        end_line: u64,
        total: u64,
    },
    #[error("insert after line {after_line} in {path} is out of range (the file has {total} lines)")]
    InsertOutOfRange {
        path: String,
        after_line: u64,
        total: u64,
    },
    #[error("overlapping line edits in {path}: ranges must not overlap, and inserts must not land inside a replaced range")]
    OverlappingLines { path: String },
    #[error("a commit carrying patch cannot also carry writes, edits, line edits, inserts, deletes or moves")]
    MixedPatch,
    #[error("patch is too large ({bytes} bytes, over {MAX_PATCH_BYTES})")]
    PatchTooLarge { bytes: usize },
    #[error("bad patch: {0}")]
    BadPatch(String),
    #[error("patch hunk {hunk} in {file} does not apply: {reason}")]
    PatchReject {
        file: String,
        hunk: usize,
        reason: String,
    },
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
    #[error("unknown preview base {base:?}; preview is {current}")]
    UnknownPreviewBase { base: String, current: String },
    #[error("tip moved under this commit: main is {current_sha}")]
    Conflict { current_sha: String },
    #[error("tip moved under this preview commit: preview is {current_sha}")]
    PreviewConflict { current_sha: String },
    #[error("no live version on main to preview from yet")]
    NoMainToPreview,
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

/// One decoded file, ready for the scratch index. `is_text` marks full-file
/// text writes and edit results: the only candidates oxfmt ever sees.
/// Uploads (base64), moves and deletes are never formatted.
pub(crate) struct RawFile {
    pub(crate) path: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) is_text: bool,
}

/// What [`commit_with_outcome`] did: the new commit sha, the repo-relative
/// paths whose content oxfmt changed, and the files committed unformatted
/// with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub sha: String,
    pub formatted: Vec<String>,
    pub format_skipped: Vec<super::format::FormatSkip>,
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
    commit_with_outcome(store, cfg, sync, slug, req)
        .await
        .map(|outcome| outcome.sha)
}

/// Like [`commit`], but also formats written/edited text files through the
/// platform oxfmt and reports what changed. `rollback` and `import_dir`
/// build their commits directly and never format.
///
/// Formatting runs after exact-match edits apply (so it sees the final
/// content) and before the commit is built. Files oxfmt cannot parse, that
/// time out, or whose output is oversized commit UNFORMATTED with a reason
/// in `format_skipped` — the commit never fails for formatting.
pub async fn commit_with_outcome(
    store: &crate::store::Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
    req: CommitRequest,
) -> Result<CommitOutcome, CommitError> {
    commit_to_ref(store, cfg, sync, slug, req, Target::Main).await
}

/// Commit `req` onto `slug`'s preview ref instead of main and publish it
/// through the manifest WAL. The first preview commit parents on the main
/// head (there must be a live version to preview from); later ones parent
/// on the preview head, and `base` CASes against the preview head, not
/// main — live commits never disturb a preview's CAS.
pub async fn commit_preview(
    store: &crate::store::Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
    req: CommitRequest,
) -> Result<CommitOutcome, CommitError> {
    commit_to_ref(store, cfg, sync, slug, req, Target::Preview).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Main,
    Preview,
}

async fn commit_to_ref(
    store: &crate::store::Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
    req: CommitRequest,
    target: Target,
) -> Result<CommitOutcome, CommitError> {
    if manifest::validate_slug(slug).is_err() {
        return Err(CommitError::BadSlug(slug.to_string()));
    }
    let (message, mut files) = validate_request(&req)?;
    let bare = manifest::ensure_synced(store, cfg, sync, slug)
        .await
        .map_err(CommitError::Internal)?;
    let main_head = manifest::head_of(&bare)
        .await
        .map_err(CommitError::Internal)?;
    // The CAS head and the commit parent. Main commits parent on the main
    // head. Preview commits parent on the preview head once it exists; the
    // first preview commit parents on the main head, or on an explicit base
    // (a fresh trial from any commit). An unborn preview ref takes any
    // resolvable base without a CAS check — the publish's CAS (absent →
    // new) serializes racing first commits.
    let (cas_head, parent) = match target {
        Target::Main => (main_head.clone(), main_head.clone()),
        Target::Preview => {
            let main = main_head.clone().ok_or(CommitError::NoMainToPreview)?;
            let preview = manifest::head_of_ref(&bare, manifest::PREVIEW_REF)
                .await
                .map_err(CommitError::Internal)?;
            match &req.base {
                Some(base) => {
                    let full = resolve_commit(&bare, base).await.map_err(|_| {
                        CommitError::UnknownPreviewBase {
                            base: base.clone(),
                            current: preview.clone().unwrap_or_else(|| "<unborn>".to_string()),
                        }
                    })?;
                    match preview {
                        Some(head) if head == full => (Some(head), Some(full)),
                        Some(head) => {
                            return Err(CommitError::PreviewConflict { current_sha: head });
                        }
                        None => (None, Some(full)),
                    }
                }
                None => (preview.clone(), Some(preview.unwrap_or(main))),
            }
        }
    };
    if target == Target::Main {
        if let Some(base) = &req.base {
            let full = resolve_commit(&bare, base)
                .await
                .map_err(|_| CommitError::UnknownBase {
                    base: base.clone(),
                    current: cas_head.clone().unwrap_or_else(|| "<unborn>".to_string()),
                })?;
            if cas_head.as_deref() != Some(full.as_str()) {
                return Err(CommitError::Conflict {
                    current_sha: cas_head.unwrap_or_else(|| "<unborn>".to_string()),
                });
            }
        }
    }
    // Patch deletes and renames land here too, so the existence checks and
    // the commit see one combined set.
    let mut req_deletes = req.changes.deletes.clone();
    let mut req_moves = req.changes.moves.clone();
    apply_changes(
        &bare,
        parent.as_deref(),
        &req.changes,
        &mut files,
        &mut req_deletes,
        &mut req_moves,
    )
    .await?;
    let outcome = super::format::format_committed_files(
        cfg.oxfmt_bin.as_deref(),
        &cfg.oxfmt_slots,
        &bare,
        parent.as_deref(),
        &mut files,
    )
    .await;
    require_existing(&bare, parent.as_deref(), &req_deletes, &req_moves).await?;
    let sha = build_commit(
        &bare,
        parent.as_deref(),
        &req.author,
        &message,
        &files,
        &req_moves,
        &req_deletes,
    )
    .await
    .map_err(CommitError::Internal)?;
    match target {
        Target::Main => {
            publish_mapped(store, cfg, sync, slug, &bare, cas_head.as_deref(), &sha).await?;
        }
        Target::Preview => {
            publish_preview_mapped(store, cfg, sync, slug, &bare, cas_head.as_deref(), &sha)
                .await?;
        }
    }
    Ok(CommitOutcome {
        sha,
        formatted: outcome.formatted,
        format_skipped: outcome.skipped,
    })
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

async fn publish_preview_mapped(
    store: &crate::store::Store,
    cfg: &GitConfig,
    sync: &GitSync,
    slug: &str,
    bare: &Path,
    expected_old: Option<&str>,
    sha: &str,
) -> Result<String, CommitError> {
    match manifest::publish_preview(store, cfg, sync, slug, expected_old, sha).await {
        Ok(sha) => Ok(sha),
        Err(e) => {
            if format!("{e:#}").contains("tip moved") {
                let current = manifest::head_of_ref(bare, manifest::PREVIEW_REF)
                    .await
                    .unwrap_or(None)
                    .unwrap_or_else(|| "<unborn>".to_string());
                return Err(CommitError::PreviewConflict {
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
/// ends, a `from` that is also written, edited (exact, line or insert) or
/// deleted, and a `to` that collides with a write, an edit or a delete. A
/// `to` that is another move's `from` stays allowed: that is a chain (or
/// swap), which [`build_commit`] resolves from the parent tree. The
/// parent-tree checks (a `from` must be a blob, a `to` must be free) happen
/// in [`build_commit`].
fn validate_moves(changes: &CommitChanges, deletes: &[String]) -> Result<(), CommitError> {
    let edited = |path: &str| {
        changes.edits.iter().any(|e| e.path == path)
            || changes.line_edits.iter().any(|e| e.path == path)
            || changes.inserts.iter().any(|i| i.path == path)
    };
    let mut froms = HashSet::new();
    let mut tos = HashSet::new();
    for m in &changes.moves {
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
        if changes.writes.iter().any(|f| f.path == m.from) || edited(&m.from) {
            return Err(CommitError::BadMove(format!("{} is also written", m.from)));
        }
        if changes.writes.iter().any(|f| f.path == m.to) {
            return Err(CommitError::BadMove(format!("{} is also written", m.to)));
        }
        if edited(&m.to) {
            return Err(CommitError::BadMove(format!("{} is also edited", m.to)));
        }
        if deletes.contains(&m.to) {
            return Err(CommitError::BadMove(format!("{} is also deleted", m.to)));
        }
    }
    Ok(())
}

/// `expect_blob` must be the full 40-hex blob id `read_files` reported for
/// the file. Malformed ids are the caller's bug (`invalid_argument`); a
/// well-formed but stale id fails later with [`CommitError::BlobConflict`].
fn validate_blob(path: &str, expect: Option<&str>) -> Result<(), CommitError> {
    if let Some(blob) = expect {
        if blob.len() != 40 || !blob.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(CommitError::InvalidEdit {
                path: path.to_string(),
                reason: "expect_blob must be the 40-hex blob id from read_files",
            });
        }
    }
    Ok(())
}

/// Line replaces in one file must not overlap, and an insert must not land
/// strictly inside a replaced range (its line would not survive). Bounds
/// against the file's line count need the base blob, so they are checked in
/// [`apply_line_ops`] with [`CommitError::OutOfRange`].
fn validate_line_overlap(line_edits: &[LineEdit], inserts: &[Insert]) -> Result<(), CommitError> {
    use std::collections::HashMap;
    let mut ranges: HashMap<&str, Vec<(u64, u64)>> = HashMap::new();
    for e in line_edits {
        ranges
            .entry(e.path.as_str())
            .or_default()
            .push((e.start_line, e.end_line));
    }
    for (path, mut spans) in ranges {
        spans.sort();
        for w in spans.windows(2) {
            if w[0].1 >= w[1].0 {
                return Err(CommitError::OverlappingLines {
                    path: path.to_string(),
                });
            }
        }
    }
    for i in inserts {
        if line_edits.iter().any(|e| {
            e.path == i.path && e.start_line <= i.after_line && i.after_line < e.end_line
        }) {
            return Err(CommitError::OverlappingLines {
                path: i.path.clone(),
            });
        }
    }
    Ok(())
}

fn validate_request(req: &CommitRequest) -> Result<(String, Vec<RawFile>), CommitError> {
    let changes = &req.changes;
    if changes.writes.is_empty()
        && changes.edits.is_empty()
        && changes.line_edits.is_empty()
        && changes.inserts.is_empty()
        && changes.patch.is_none()
        && changes.moves.is_empty()
        && changes.deletes.is_empty()
    {
        return Err(CommitError::EmptyChange);
    }
    let touched = changes.writes.len()
        + changes.edits.len()
        + changes.line_edits.len()
        + changes.inserts.len();
    if touched > MAX_WRITES_EDITS {
        return Err(CommitError::TooManyFiles { count: touched });
    }
    if changes.moves.len() > MAX_MOVES {
        return Err(CommitError::TooManyMoves {
            count: changes.moves.len(),
        });
    }
    if changes.deletes.len() > MAX_DELETES {
        return Err(CommitError::TooManyDeletes {
            count: changes.deletes.len(),
        });
    }
    if let Some(patch) = changes.patch.as_deref() {
        if touched > 0 || !changes.moves.is_empty() || !changes.deletes.is_empty() {
            return Err(CommitError::MixedPatch);
        }
        if patch.len() > MAX_PATCH_BYTES {
            return Err(CommitError::PatchTooLarge { bytes: patch.len() });
        }
        if patch.trim().is_empty() {
            return Err(CommitError::BadPatch("patch is empty".to_string()));
        }
    }
    let message = validate_message(&req.message)?;
    validate_author(&req.author)?;
    let mut decoded = 0usize;
    let mut files = Vec::with_capacity(changes.writes.len());
    let mut seen_writes = HashSet::new();
    for f in &changes.writes {
        validate_path(&f.path)?;
        validate_blob(&f.path, f.expect_blob.as_deref())?;
        if !seen_writes.insert(f.path.as_str()) {
            return Err(CommitError::DuplicateWrite {
                path: f.path.clone(),
            });
        }
        let (bytes, is_text) = match &f.body {
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
                (text.as_bytes().to_vec(), true)
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
                (bytes, false)
            }
        };
        decoded += bytes.len();
        files.push(RawFile {
            path: f.path.clone(),
            bytes,
            is_text,
        });
    }
    if decoded > MAX_COMMIT_BYTES {
        return Err(CommitError::TotalTooLarge { bytes: decoded });
    }
    let line_touched = |path: &str| {
        changes.line_edits.iter().any(|e| e.path == path)
            || changes.inserts.iter().any(|i| i.path == path)
    };
    for path in &changes.deletes {
        validate_path(path)?;
        if changes.writes.iter().any(|f| &f.path == path)
            || changes.edits.iter().any(|e| &e.path == path)
            || line_touched(path)
        {
            return Err(CommitError::BothWrittenAndDeleted { path: path.clone() });
        }
    }
    let mut seen_edits = HashSet::new();
    for e in &changes.edits {
        validate_path(&e.path)?;
        validate_blob(&e.path, e.expect_blob.as_deref())?;
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
    for e in &changes.line_edits {
        validate_path(&e.path)?;
        validate_blob(&e.path, e.expect_blob.as_deref())?;
        if e.start_line < 1 {
            return Err(CommitError::InvalidEdit {
                path: e.path.clone(),
                reason: "start_line is 1-based",
            });
        }
        if e.end_line < e.start_line {
            return Err(CommitError::InvalidEdit {
                path: e.path.clone(),
                reason: "end_line must not precede start_line",
            });
        }
        if seen_writes.contains(e.path.as_str()) {
            return Err(CommitError::BothWrittenAndEdited {
                path: e.path.clone(),
            });
        }
        if changes.deletes.iter().any(|d| d == &e.path) {
            return Err(CommitError::BothWrittenAndDeleted {
                path: e.path.clone(),
            });
        }
    }
    for i in &changes.inserts {
        validate_path(&i.path)?;
        validate_blob(&i.path, i.expect_blob.as_deref())?;
        if seen_writes.contains(i.path.as_str()) {
            return Err(CommitError::BothWrittenAndEdited {
                path: i.path.clone(),
            });
        }
        if changes.deletes.iter().any(|d| d == &i.path) {
            return Err(CommitError::BothWrittenAndDeleted {
                path: i.path.clone(),
            });
        }
    }
    validate_line_overlap(&changes.line_edits, &changes.inserts)?;
    validate_moves(changes, &changes.deletes)?;
    Ok((message, files))
}

/// Load one base blob as UTF-8 text with its blob sha. The blob must fit in
/// [`MAX_TEXT_BYTES`] (checked with `cat-file -s` before reading, so a huge
/// blob never lands in memory); a missing path is [`CommitError::NotFound`].
async fn read_text_blob(
    git_dir: &str,
    parent: &str,
    path: &str,
) -> Result<(String, String), CommitError> {
    let entry = tree_blob(git_dir, parent, path)
        .await
        .map_err(CommitError::Internal)?
        .ok_or_else(|| CommitError::NotFound {
            path: path.to_string(),
        })?;
    let size_out = manifest::run_git(
        &[git_dir, "cat-file", "-s", &entry.sha],
        &[],
        Duration::from_secs(15),
    )
    .await
    .map_err(CommitError::Internal)?;
    let size: usize = size_out
        .trim()
        .parse()
        .map_err(|_| CommitError::Internal(anyhow::anyhow!("bad blob size for {0}", path)))?;
    if size > MAX_TEXT_BYTES {
        return Err(CommitError::TextTooLarge {
            path: path.to_string(),
        });
    }
    let bytes = manifest::run_git_raw(
        &[git_dir, "cat-file", "-p", &entry.sha],
        &[],
        Duration::from_secs(30),
    )
    .await
    .map_err(CommitError::Internal)?;
    let content = std::str::from_utf8(&bytes).map_err(|_| CommitError::InvalidEdit {
        path: path.to_string(),
        reason: "file is not UTF-8 text",
    })?;
    Ok((content.to_string(), entry.sha))
}

/// Fail when `expect` (a validated 40-hex blob id) is not the blob at `path`
/// in the base commit. A missing file counts as `<missing>`, never as a
/// match, so deleting the file under the reader also conflicts.
async fn check_blob(
    git_dir: &str,
    parent: Option<&str>,
    path: &str,
    expect: Option<&str>,
) -> Result<(), CommitError> {
    let Some(expect) = expect else {
        return Ok(());
    };
    let current = match parent {
        Some(parent) => tree_blob(git_dir, parent, path)
            .await
            .map_err(CommitError::Internal)?
            .map(|entry| entry.sha),
        None => None,
    };
    if current.as_deref() != Some(expect) {
        return Err(CommitError::BlobConflict {
            path: path.to_string(),
            expected: expect.to_string(),
            current: current.unwrap_or_else(|| "<missing>".to_string()),
        });
    }
    Ok(())
}

/// Apply the content changes in `changes` against the base commit `parent`,
/// appending full-file results to `files` and patch deletes/renames to
/// `deletes`/`moves`. `commit`, `commit_preview` and `stage` share this, so
/// every path applies the same order with the same errors.
///
/// Order (a `patch` commit carries nothing else, so there is no interaction
/// with the rest): per-file blob checks first, then exact-match edits, then
/// line replaces and inserts, then the patch. A `patch` applies with exact
/// context; any reject fails the whole commit and appends nothing, so a
/// half-applied patch never lands.
pub async fn apply_changes(
    bare: &Path,
    parent: Option<&str>,
    changes: &CommitChanges,
    files: &mut Vec<RawFile>,
    deletes: &mut Vec<String>,
    moves: &mut Vec<Move>,
) -> Result<(), CommitError> {
    let git_dir = format!("--git-dir={}", bare.display());
    for (path, expect) in changes
        .writes
        .iter()
        .map(|f| (f.path.as_str(), f.expect_blob.as_deref()))
        .chain(
            changes
                .edits
                .iter()
                .map(|e| (e.path.as_str(), e.expect_blob.as_deref())),
        )
        .chain(
            changes
                .line_edits
                .iter()
                .map(|e| (e.path.as_str(), e.expect_blob.as_deref())),
        )
        .chain(
            changes
                .inserts
                .iter()
                .map(|i| (i.path.as_str(), i.expect_blob.as_deref())),
        )
    {
        check_blob(&git_dir, parent, path, expect).await?;
    }
    let mut total: usize = files.iter().map(|f| f.bytes.len()).sum();
    apply_edits(&git_dir, parent, &changes.edits, files, &mut total).await?;
    apply_line_ops(
        &git_dir,
        parent,
        &changes.line_edits,
        &changes.inserts,
        files,
        &mut total,
    )
    .await?;
    if let Some(patch) = changes.patch.as_deref() {
        apply_patch(&git_dir, parent, patch, files, deletes, moves, &mut total).await?;
    }
    Ok(())
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
    git_dir: &str,
    parent: Option<&str>,
    edits: &[Edit],
    files: &mut Vec<RawFile>,
    total: &mut usize,
) -> Result<(), CommitError> {
    for e in edits {
        let Some(parent) = parent else {
            return Err(CommitError::NotFound {
                path: e.path.clone(),
            });
        };
        let (content, _) = read_text_blob(git_dir, parent, &e.path).await?;
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
        push_text_result(&e.path, replaced, files, total)?;
    }
    Ok(())
}

/// Check one produced text file against the caps and account its bytes, like
/// an exact-match edit result.
fn push_text_result(
    path: &str,
    content: String,
    files: &mut Vec<RawFile>,
    total: &mut usize,
) -> Result<(), CommitError> {
    if content.len() > MAX_TEXT_BYTES {
        return Err(CommitError::TextTooLarge {
            path: path.to_string(),
        });
    }
    if content.as_bytes().contains(&0) {
        return Err(CommitError::HasNul {
            path: path.to_string(),
        });
    }
    *total = total.saturating_add(content.len());
    if *total > MAX_COMMIT_BYTES {
        return Err(CommitError::TotalTooLarge { bytes: *total });
    }
    match files.iter_mut().find(|f| f.path == path) {
        Some(f) => f.bytes = content.into_bytes(),
        None => files.push(RawFile {
            path: path.to_string(),
            bytes: content.into_bytes(),
            is_text: true,
        }),
    }
    Ok(())
}

/// Apply line replaces and inserts for every touched file in one ascending
/// pass over the original `read_files` numbering: replaces run first at
/// their range, inserts run after the line they follow (several inserts at
/// one line run in input order). An insert after a replaced range's last
/// line lands after the new block.
///
/// The file's trailing newline is preserved unless an op touches the last
/// line: then it comes from that op's text (the last insert at end-of-file
/// wins over a replace ending there). An empty result stays empty.
async fn apply_line_ops(
    git_dir: &str,
    parent: Option<&str>,
    line_edits: &[LineEdit],
    inserts: &[Insert],
    files: &mut Vec<RawFile>,
    total: &mut usize,
) -> Result<(), CommitError> {
    if line_edits.is_empty() && inserts.is_empty() {
        return Ok(());
    }
    use std::collections::HashMap;
    let mut order: Vec<&str> = Vec::new();
    let mut grouped: HashMap<&str, (Vec<&LineEdit>, Vec<&Insert>)> = HashMap::new();
    for e in line_edits {
        grouped
            .entry(e.path.as_str())
            .or_insert_with(|| {
                order.push(e.path.as_str());
                (Vec::new(), Vec::new())
            })
            .0
            .push(e);
    }
    for i in inserts {
        grouped
            .entry(i.path.as_str())
            .or_insert_with(|| {
                order.push(i.path.as_str());
                (Vec::new(), Vec::new())
            })
            .1
            .push(i);
    }
    for path in order {
        let (replaces, insertions) = &grouped[path];
        // The exact-edit result when this file was also exactly edited
        // (exact edits run first), else the base blob.
        let base_text = match files.iter().find(|f| f.path == path) {
            Some(f) => String::from_utf8(f.bytes.clone()).map_err(|_| {
                CommitError::Internal(anyhow::anyhow!("edit result for {path} is not UTF-8"))
            })?,
            None => {
                let Some(parent) = parent else {
                    return Err(CommitError::NotFound {
                        path: path.to_string(),
                    });
                };
                read_text_blob(git_dir, parent, path).await?.0
            }
        };
        let lines: Vec<&str> = base_text.lines().collect();
        let n = lines.len() as u64;
        let base_trailing = base_text.ends_with('\n');
        let mut reps: Vec<&LineEdit> = replaces.iter().copied().collect();
        reps.sort_by_key(|e| e.start_line);
        for e in &reps {
            if e.end_line > n {
                return Err(CommitError::OutOfRange {
                    path: path.to_string(),
                    start_line: e.start_line,
                    end_line: e.end_line,
                    total: n,
                });
            }
        }
        for i in insertions.iter() {
            if i.after_line > n {
                return Err(CommitError::InsertOutOfRange {
                    path: path.to_string(),
                    after_line: i.after_line,
                    total: n,
                });
            }
        }
        let mut out: Vec<String> = Vec::new();
        for i in insertions.iter().filter(|i| i.after_line == 0) {
            out.extend(i.text.lines().map(str::to_string));
        }
        let mut ri = 0usize;
        let mut line = 1u64;
        while line <= n {
            if ri < reps.len() && reps[ri].start_line == line {
                let e = reps[ri];
                out.extend(e.new.lines().map(str::to_string));
                for i in insertions.iter().filter(|i| i.after_line == e.end_line) {
                    out.extend(i.text.lines().map(str::to_string));
                }
                line = e.end_line + 1;
                ri += 1;
                continue;
            }
            out.push(lines[(line - 1) as usize].to_string());
            for i in insertions.iter().filter(|i| i.after_line == line) {
                out.extend(i.text.lines().map(str::to_string));
            }
            line += 1;
        }
        let trailing = if out.is_empty() {
            false
        } else if let Some(last) = insertions.iter().filter(|i| i.after_line == n).last() {
            last.text.ends_with('\n')
        } else if let Some(e) = reps.iter().find(|e| e.end_line == n) {
            e.new.ends_with('\n')
        } else {
            base_trailing
        };
        let mut content = out.join("\n");
        if trailing {
            content.push('\n');
        }
        push_text_result(path, content, files, total)?;
    }
    Ok(())
}

/// One hunk line: ` ` context, `-` removal, `+` addition, or the
/// `\ No newline at end of file` marker, which always directly follows the
/// line it applies to.
#[derive(Debug, Clone)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
    NoNewline,
}

/// One hunk: `old_start` is 1-based (`Some(0)` with an empty old range means
/// before line 1); `None` is a Codex bare `@@`, located by unique context.
#[derive(Debug, Clone)]
struct Hunk {
    old_start: Option<u64>,
    old_count: u64,
    new_count: u64,
    /// False for Codex bare `@@` hunks, whose counts are inferred.
    numbered: bool,
    lines: Vec<HunkLine>,
}

/// One file block of a patch: either side is `None` for `/dev/null`.
#[derive(Debug, Clone)]
struct PatchFile {
    old_path: Option<String>,
    new_path: Option<String>,
    rename_from: Option<String>,
    rename_to: Option<String>,
    hunks: Vec<Hunk>,
}

/// What a [`PatchFile`] does, resolved before anything is read.
#[derive(Debug, Clone)]
enum PatchKind {
    Add(String),
    Delete(String),
    Modify(String),
    Rename { from: String, to: String },
}

/// Parse `patch` (unified diff, or Codex `*** Begin Patch` when it starts
/// with that marker) and apply every file against the base commit `parent`,
/// appending results to `files` and patch deletes/renames to
/// `deletes`/`moves`. Nothing is appended when any file fails, so a
/// rejected hunk fails the whole commit atomically.
async fn apply_patch(
    git_dir: &str,
    parent: Option<&str>,
    patch: &str,
    files: &mut Vec<RawFile>,
    deletes: &mut Vec<String>,
    moves: &mut Vec<Move>,
    total: &mut usize,
) -> Result<(), CommitError> {
    let parsed = if patch.trim_start().starts_with("*** Begin Patch") {
        parse_codex_patch(patch)?
    } else {
        parse_unified_patch(patch)?
    };
    if parsed.is_empty() {
        return Err(CommitError::BadPatch("patch touches no files".to_string()));
    }
    if parsed.len() > MAX_WRITES_EDITS {
        return Err(CommitError::TooManyFiles {
            count: parsed.len(),
        });
    }
    let mut kinds = Vec::with_capacity(parsed.len());
    let mut seen = HashSet::new();
    for f in &parsed {
        let kind = patch_kind(f)?;
        let target = match &kind {
            PatchKind::Add(p) | PatchKind::Delete(p) | PatchKind::Modify(p) => p.clone(),
            PatchKind::Rename { to, .. } => to.clone(),
        };
        validate_path(&target)?;
        if !seen.insert(target.clone()) {
            return Err(CommitError::BadPatch(format!(
                "patch touches {target} twice"
            )));
        }
        kinds.push(kind);
    }
    let mut new_files = Vec::new();
    let mut new_deletes = Vec::new();
    let mut new_moves = Vec::new();
    for (f, kind) in parsed.iter().zip(kinds) {
        apply_patch_file(git_dir, parent, f, &kind, &mut new_files, &mut new_deletes, &mut new_moves, total)
            .await?;
    }
    files.extend(new_files);
    deletes.extend(new_deletes);
    moves.extend(new_moves);
    Ok(())
}

/// Resolve what a parsed file block does. Renames come only from explicit
/// `rename from`/`rename to` headers (or Codex `*** Move to:`): differing
/// `---`/`+++` paths without them are refused, so the classic
/// `--- foo.orig` / `+++ foo` pair never renames by accident.
fn patch_kind(f: &PatchFile) -> Result<PatchKind, CommitError> {
    match (&f.rename_from, &f.rename_to) {
        (Some(from), Some(to)) => {
            validate_path(from)?;
            validate_path(to)?;
            if from == to {
                return Err(CommitError::BadMove(format!("{from} targets itself")));
            }
            return Ok(PatchKind::Rename {
                from: from.clone(),
                to: to.clone(),
            });
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(CommitError::BadPatch(
                "rename needs both `rename from` and `rename to`".to_string(),
            ));
        }
        (None, None) => {}
    }
    match (&f.old_path, &f.new_path) {
        (None, Some(new)) => Ok(PatchKind::Add(new.clone())),
        (Some(old), None) => Ok(PatchKind::Delete(old.clone())),
        (Some(old), Some(new)) if old == new => Ok(PatchKind::Modify(new.clone())),
        (Some(old), Some(new)) => Err(CommitError::BadPatch(format!(
            "patch renames {old} to {new} without `rename from`/`rename to` headers"
        ))),
        (None, None) => Err(CommitError::BadPatch(
            "patch file block has no paths".to_string(),
        )),
    }
}

/// Apply one parsed file: check existence against the base tree, run the
/// hunks, and stage the result.
#[allow(clippy::too_many_arguments)]
async fn apply_patch_file(
    git_dir: &str,
    parent: Option<&str>,
    f: &PatchFile,
    kind: &PatchKind,
    files: &mut Vec<RawFile>,
    deletes: &mut Vec<String>,
    moves: &mut Vec<Move>,
    total: &mut usize,
) -> Result<(), CommitError> {
    let Some(parent) = parent else {
        let path = match kind {
            PatchKind::Add(p) | PatchKind::Delete(p) | PatchKind::Modify(p) => p.clone(),
            PatchKind::Rename { from, .. } => from.clone(),
        };
        return Err(CommitError::NotFound { path });
    };
    match kind {
        PatchKind::Add(path) => {
            if tree_blob(git_dir, parent, path)
                .await
                .map_err(CommitError::Internal)?
                .is_some()
            {
                return Err(CommitError::BadPatch(format!(
                    "{path} already exists; a patch can only add new files"
                )));
            }
            let (lines, trailing) = apply_hunks(&[], false, &f.hunks, path)?;
            let mut content = lines.join("\n");
            if trailing {
                content.push('\n');
            }
            push_text_result(path, content, files, total)?;
        }
        PatchKind::Delete(path) => {
            if tree_blob(git_dir, parent, path)
                .await
                .map_err(CommitError::Internal)?
                .is_none()
            {
                return Err(CommitError::NotFound { path: path.clone() });
            }
            if !f.hunks.is_empty() {
                // A delete hunk removes every line; anything left behind
                // means the hunk is stale.
                let (base, _) = read_text_blob(git_dir, parent, path).await?;
                let old: Vec<String> = base.lines().map(str::to_string).collect();
                let (lines, _) = apply_hunks(&old, base.ends_with('\n'), &f.hunks, path)?;
                if !lines.is_empty() {
                    return Err(CommitError::PatchReject {
                        file: path.clone(),
                        hunk: f.hunks.len(),
                        reason: "delete hunk leaves lines behind".to_string(),
                    });
                }
            }
            deletes.push(path.clone());
        }
        PatchKind::Modify(path) => {
            let (base, _) = read_text_blob(git_dir, parent, path).await?;
            let old: Vec<String> = base.lines().map(str::to_string).collect();
            let (lines, trailing) = apply_hunks(&old, base.ends_with('\n'), &f.hunks, path)?;
            let mut content = lines.join("\n");
            if trailing {
                content.push('\n');
            }
            push_text_result(path, content, files, total)?;
        }
        PatchKind::Rename { from, to } => {
            if tree_blob(git_dir, parent, from)
                .await
                .map_err(CommitError::Internal)?
                .is_none()
            {
                return Err(CommitError::NotFound { path: from.clone() });
            }
            if tree_blob(git_dir, parent, to)
                .await
                .map_err(CommitError::Internal)?
                .is_some()
            {
                return Err(CommitError::BadPatch(format!("{to} already exists")));
            }
            if f.hunks.is_empty() {
                moves.push(Move {
                    from: from.clone(),
                    to: to.clone(),
                });
            } else {
                let (base, _) = read_text_blob(git_dir, parent, from).await?;
                let old: Vec<String> = base.lines().map(str::to_string).collect();
                let (lines, trailing) = apply_hunks(&old, base.ends_with('\n'), &f.hunks, to)?;
                let mut content = lines.join("\n");
                if trailing {
                    content.push('\n');
                }
                // The move re-points the blob and the write carries the
                // modified content; [`build_commit`] applies moves first.
                moves.push(Move {
                    from: from.clone(),
                    to: to.clone(),
                });
                let len = content.len();
                if len > MAX_TEXT_BYTES {
                    return Err(CommitError::TextTooLarge { path: to.clone() });
                }
                if content.as_bytes().contains(&0) {
                    return Err(CommitError::HasNul { path: to.clone() });
                }
                *total = total.saturating_add(len);
                if *total > MAX_COMMIT_BYTES {
                    return Err(CommitError::TotalTooLarge { bytes: *total });
                }
                files.push(RawFile {
                    path: to.clone(),
                    bytes: content.into_bytes(),
                    is_text: true,
                });
            }
        }
    }
    Ok(())
}

/// Run a file's hunks over its lines with exact context. `old_trailing` is
/// the base file's trailing-newline state. Returns the new lines and their
/// trailing-newline state: false when the last emitted line carries a
/// `\ No newline` marker, true when a hunk touches end-of-file, else the
/// base state.
fn apply_hunks(
    old_lines: &[String],
    old_trailing: bool,
    hunks: &[Hunk],
    file: &str,
) -> Result<(Vec<String>, bool), CommitError> {
    // Markers must directly follow a content line; a marker after an added
    // or context line must sit on the file's last emitted new line (it
    // states the new file has no trailing newline), while a marker after a
    // removal only notes the old side and is ignored.
    for (hi, h) in hunks.iter().enumerate() {
        for (li, line) in h.lines.iter().enumerate() {
            if matches!(line, HunkLine::NoNewline) {
                match li.checked_sub(1).and_then(|p| h.lines.get(p)) {
                    Some(HunkLine::Context(_))
                    | Some(HunkLine::Remove(_))
                    | Some(HunkLine::Add(_)) => {}
                    _ => {
                        return Err(CommitError::BadPatch(format!(
                            "stray `\\ No newline at end of file` marker in hunk {} of {file}",
                            hi + 1
                        )));
                    }
                }
            }
        }
    }
    let mut last_emit: Option<(usize, usize)> = None;
    for (hi, h) in hunks.iter().enumerate() {
        for (li, line) in h.lines.iter().enumerate() {
            if matches!(line, HunkLine::Context(_) | HunkLine::Add(_)) {
                last_emit = Some((hi, li));
            }
        }
    }
    for (hi, h) in hunks.iter().enumerate() {
        for (li, line) in h.lines.iter().enumerate() {
            if matches!(line, HunkLine::NoNewline) {
                let prev = li.checked_sub(1).and_then(|p| h.lines.get(p));
                if matches!(prev, Some(HunkLine::Add(_) | HunkLine::Context(_)))
                    && last_emit != Some((hi, li - 1))
                {
                    return Err(CommitError::BadPatch(format!(
                        "stray `\\ No newline at end of file` marker in hunk {} of {file}",
                        hi + 1
                    )));
                }
            }
        }
    }
    let marked_final = last_emit.is_some_and(|(hi, li)| {
        matches!(hunks[hi].lines.get(li + 1), Some(HunkLine::NoNewline))
    });
    let mut new_lines: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    let mut touches_eof = false;
    for (hi, h) in hunks.iter().enumerate() {
        let hunk_no = hi + 1;
        let reject = |reason: String| CommitError::PatchReject {
            file: file.to_string(),
            hunk: hunk_no,
            reason,
        };
        let start = match h.old_start {
            Some(s) => {
                if s == 0 {
                    if h.old_count != 0 {
                        return Err(reject(
                            "old start 0 with a nonempty old range".to_string(),
                        ));
                    }
                    if cursor != 0 {
                        return Err(reject("hunk overlaps an earlier hunk".to_string()));
                    }
                    0
                } else {
                    (s - 1) as usize
                }
            }
            None => locate_hunk(old_lines, h).map_err(reject)?,
        };
        if start < cursor {
            return Err(reject("hunk overlaps an earlier hunk".to_string()));
        }
        let (mut want_old, mut want_new) = (0u64, 0u64);
        for line in &h.lines {
            match line {
                HunkLine::Context(_) => {
                    want_old += 1;
                    want_new += 1;
                }
                HunkLine::Remove(_) => want_old += 1,
                HunkLine::Add(_) => want_new += 1,
                HunkLine::NoNewline => {}
            }
        }
        if h.numbered && (want_old != h.old_count || want_new != h.new_count) {
            return Err(CommitError::BadPatch(format!(
                "hunk {hunk_no} of {file} holds {want_old}/{want_new} old/new lines, header says {}/{}",
                h.old_count, h.new_count
            )));
        }
        if start + want_old as usize > old_lines.len() {
            return Err(reject("hunk runs past the end of the file".to_string()));
        }
        if start == old_lines.len()
            || (want_old > 0 && start + want_old as usize == old_lines.len())
        {
            touches_eof = true;
        }
        new_lines.extend(old_lines[cursor..start].iter().cloned());
        cursor = start;
        for line in &h.lines {
            match line {
                HunkLine::Context(t) => {
                    if old_lines.get(cursor) != Some(t) {
                        return Err(reject(format!(
                            "context mismatch at line {}",
                            cursor + 1
                        )));
                    }
                    new_lines.push(t.clone());
                    cursor += 1;
                }
                HunkLine::Remove(t) => {
                    if old_lines.get(cursor) != Some(t) {
                        return Err(reject(format!(
                            "removal mismatch at line {}",
                            cursor + 1
                        )));
                    }
                    cursor += 1;
                }
                HunkLine::Add(t) => new_lines.push(t.clone()),
                HunkLine::NoNewline => {}
            }
        }
    }
    new_lines.extend(old_lines[cursor..].iter().cloned());
    let trailing = if new_lines.is_empty() {
        false
    } else if marked_final {
        false
    } else if touches_eof {
        true
    } else {
        old_trailing
    };
    Ok((new_lines, trailing))
}

/// Locate a Codex bare-`@@` hunk: its context and removal lines must occur
/// as one window exactly once in the file.
fn locate_hunk(old_lines: &[String], hunk: &Hunk) -> Result<usize, String> {
    let seq: Vec<&str> = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(t) | HunkLine::Remove(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    if seq.is_empty() {
        return Err("hunk has no context to locate it".to_string());
    }
    let mut found = Vec::new();
    if seq.len() <= old_lines.len() {
        for w in 0..=old_lines.len() - seq.len() {
            if old_lines[w..w + seq.len()]
                .iter()
                .map(String::as_str)
                .eq(seq.iter().copied())
            {
                found.push(w);
            }
        }
    }
    match found.as_slice() {
        [one] => Ok(*one),
        [] => Err("context matches nowhere in the file".to_string()),
        _ => Err(format!(
            "context matches {} places; use numbered hunks",
            found.len()
        )),
    }
}

/// Parse a unified diff into file blocks. `diff --git` lines only separate
/// blocks; renames come from explicit `rename from`/`rename to` headers.
/// A `--- ` line starts a file block only when a `+++ ` line follows —
/// otherwise it removes a line starting with `--`.
fn parse_unified_patch(patch: &str) -> Result<Vec<PatchFile>, CommitError> {
    let bad = |msg: String| CommitError::BadPatch(msg);
    let lines: Vec<&str> = patch
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut i = 0;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    let mut out = Vec::new();
    let mut rename_from: Option<String> = None;
    let mut rename_to: Option<String> = None;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if line.starts_with("diff --git ") {
            rename_from = None;
            rename_to = None;
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("rename from ") {
            rename_from = Some(clean_patch_path(rest));
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("rename to ") {
            rename_to = Some(clean_patch_path(rest));
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("Binary files ") {
            let who = rest.strip_suffix(" differ").unwrap_or(rest);
            return Err(bad(format!(
                "binary patches are refused ({who}); use full-file writes with base64"
            )));
        }
        if line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
            || line.starts_with("index ")
            || line.starts_with("similarity ")
            || line.starts_with("dissimilarity ")
        {
            i += 1;
            continue;
        }
        if line.starts_with("--- ")
            && lines.get(i + 1).is_some_and(|next| next.starts_with("+++ "))
        {
            let old_raw = clean_patch_path(&line[4..]);
            let new_raw = clean_patch_path(&lines[i + 1][4..]);
            i += 2;
            let old_path = if old_raw == "/dev/null" {
                None
            } else {
                Some(strip_patch_prefix(&old_raw, 'a'))
            };
            let new_path = if new_raw == "/dev/null" {
                None
            } else {
                Some(strip_patch_prefix(&new_raw, 'b'))
            };
            let label = old_path
                .as_deref()
                .or(new_path.as_deref())
                .unwrap_or("patch")
                .to_string();
            let mut hunks = Vec::new();
            while i < lines.len() && lines[i].starts_with("@@") {
                let (hunk, next) = parse_numbered_hunk(&lines, i, &label)?;
                hunks.push(hunk);
                i = next;
            }
            out.push(PatchFile {
                old_path,
                new_path,
                rename_from: rename_from.take(),
                rename_to: rename_to.take(),
                hunks,
            });
            continue;
        }
        return Err(bad(format!("bad patch: unexpected line {line:?}")));
    }
    Ok(out)
}

/// Drop the timestamp (`\t...`) and surrounding quotes git appends to `---`
/// and `+++` paths.
fn clean_patch_path(raw: &str) -> String {
    let cut = raw.split('\t').next().unwrap_or("").trim_end();
    if cut.len() >= 2 && cut.starts_with('"') && cut.ends_with('"') {
        cut[1..cut.len() - 1].to_string()
    } else {
        cut.to_string()
    }
}

/// Drop one `a/` or `b/` prefix from a diff path, leaving other paths alone.
fn strip_patch_prefix(path: &str, prefix: char) -> String {
    let mut chars = path.chars();
    if chars.next() == Some(prefix) && chars.next() == Some('/') {
        path[2..].to_string()
    } else {
        path.to_string()
    }
}

/// Parse one `@@ -a[,b] +c[,d] @@` hunk and its body. A body line starting
/// with `---` removes a `--` line unless a `+++ ` line follows it.
fn parse_numbered_hunk(
    lines: &[&str],
    i: usize,
    label: &str,
) -> Result<(Hunk, usize), CommitError> {
    let bad = |msg: String| CommitError::BadPatch(msg);
    let header = lines[i];
    let after_first = header
        .strip_prefix("@@")
        .ok_or_else(|| bad(format!("bad hunk header in {label}: {header:?}")))?;
    let end = after_first
        .find("@@")
        .ok_or_else(|| bad(format!("bad hunk header in {label}: {header:?}")))?;
    let (old_start, old_count, _new_start, new_count) =
        parse_hunk_ranges(after_first[..end].trim(), label)?;
    let mut j = i + 1;
    let mut body = Vec::new();
    while j < lines.len() {
        let l = lines[j];
        if l.starts_with("@@") || l.starts_with("diff --git ") {
            break;
        }
        if l.starts_with("--- ")
            && lines.get(j + 1).is_some_and(|next| next.starts_with("+++ "))
        {
            break;
        }
        if let Some(text) = l.strip_prefix('+') {
            body.push(HunkLine::Add(text.to_string()));
        } else if let Some(text) = l.strip_prefix('-') {
            body.push(HunkLine::Remove(text.to_string()));
        } else if let Some(text) = l.strip_prefix(' ') {
            body.push(HunkLine::Context(text.to_string()));
        } else if l == "\\ No newline at end of file" {
            body.push(HunkLine::NoNewline);
        } else if l.is_empty() {
            body.push(HunkLine::Context(String::new()));
        } else {
            return Err(bad(format!("bad patch line in {label}: {l:?}")));
        }
        j += 1;
    }
    Ok((
        Hunk {
            old_start: Some(old_start),
            old_count,
            new_count,
            numbered: true,
            lines: body,
        },
        j,
    ))
}

/// Parse the `-a[,b] +c[,d]` ranges of a hunk header.
fn parse_hunk_ranges(s: &str, label: &str) -> Result<(u64, u64, u64, u64), CommitError> {
    let bad = |msg: String| CommitError::BadPatch(msg);
    let mut parts = s.split_whitespace();
    let old = parts
        .next()
        .ok_or_else(|| bad(format!("bad hunk header in {label}: {s:?}")))?;
    let new = parts
        .next()
        .ok_or_else(|| bad(format!("bad hunk header in {label}: {s:?}")))?;
    if parts.next().is_some() {
        return Err(bad(format!("bad hunk header in {label}: {s:?}")))?;
    }
    let (old_start, old_count) = parse_hunk_range(old, '-', label)?;
    let (new_start, new_count) = parse_hunk_range(new, '+', label)?;
    Ok((old_start, old_count, new_start, new_count))
}

/// Parse one `-a[,b]` range: a bare number covers one line.
fn parse_hunk_range(s: &str, sign: char, label: &str) -> Result<(u64, u64), CommitError> {
    let bad = |msg: String| CommitError::BadPatch(msg);
    let body = s
        .strip_prefix(sign)
        .ok_or_else(|| bad(format!("bad hunk header in {label}: {s:?}")))?;
    match body.split_once(',') {
        Some((start, count)) => {
            let start: u64 = start
                .parse()
                .map_err(|_| bad(format!("bad hunk header in {label}: {s:?}")))?;
            let count: u64 = count
                .parse()
                .map_err(|_| bad(format!("bad hunk header in {label}: {s:?}")))?;
            Ok((start, count))
        }
        None => {
            let start: u64 = body
                .parse()
                .map_err(|_| bad(format!("bad hunk header in {label}: {s:?}")))?;
            Ok((start, 1))
        }
    }
}

/// Parse a Codex `*** Begin Patch` block into the same file blocks. Update
/// hunks take an optional numbered header; a bare `@@` hunk is located by
/// unique context. Blank lines separate sections; an empty line inside an
/// added file or a hunk cannot be expressed (use a unified diff for those).
fn parse_codex_patch(patch: &str) -> Result<Vec<PatchFile>, CommitError> {
    let bad = |msg: String| CommitError::BadPatch(msg);
    let mut out = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut cur_add = false;
    let mut cur_hunks: Vec<Hunk> = Vec::new();
    let mut cur_hunk: Option<Hunk> = None;
    let mut rename_to: Option<String> = None;
    let mut flush_hunk = |cur_hunk: &mut Option<Hunk>, cur_hunks: &mut Vec<Hunk>| {
        if let Some(mut h) = cur_hunk.take() {
            if !h.numbered {
                let (mut old_count, mut new_count) = (0u64, 0u64);
                for line in &h.lines {
                    match line {
                        HunkLine::Context(_) => {
                            old_count += 1;
                            new_count += 1;
                        }
                        HunkLine::Remove(_) => old_count += 1,
                        HunkLine::Add(_) => new_count += 1,
                        HunkLine::NoNewline => {}
                    }
                }
                h.old_count = old_count;
                h.new_count = new_count;
            }
            cur_hunks.push(h);
        }
    };
    let mut flush_file = |cur_path: &mut Option<String>,
                          cur_add: &mut bool,
                          cur_hunk: &mut Option<Hunk>,
                          cur_hunks: &mut Vec<Hunk>,
                          rename_to: &mut Option<String>,
                          out: &mut Vec<PatchFile>| {
        flush_hunk(cur_hunk, cur_hunks);
        if let Some(path) = cur_path.take() {
            let hunks = std::mem::take(cur_hunks);
            if *cur_add {
                out.push(PatchFile {
                    old_path: None,
                    new_path: Some(path),
                    rename_from: None,
                    rename_to: rename_to.take(),
                    hunks,
                });
            } else {
                out.push(PatchFile {
                    old_path: Some(path.clone()),
                    new_path: Some(path),
                    rename_from: None,
                    rename_to: rename_to.take(),
                    hunks,
                });
            }
            *cur_add = false;
        } else {
            rename_to.take();
        }
    };
    let mut begun = false;
    let mut done = false;
    for raw in patch.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if !begun {
            if line.trim().is_empty() {
                continue;
            }
            if line == "*** Begin Patch" {
                begun = true;
                continue;
            }
            return Err(bad(format!("bad patch: unexpected line {line:?}")));
        }
        if line == "*** End Patch" {
            flush_file(
                &mut cur_path,
                &mut cur_add,
                &mut cur_hunk,
                &mut cur_hunks,
                &mut rename_to,
                &mut out,
            );
            done = true;
            break;
        }
        if line == "*** Begin Patch" {
            return Err(bad("bad patch: nested `*** Begin Patch`".to_string()));
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            flush_file(
                &mut cur_path,
                &mut cur_add,
                &mut cur_hunk,
                &mut cur_hunks,
                &mut rename_to,
                &mut out,
            );
            cur_path = Some(path.trim().to_string());
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            flush_file(
                &mut cur_path,
                &mut cur_add,
                &mut cur_hunk,
                &mut cur_hunks,
                &mut rename_to,
                &mut out,
            );
            cur_path = Some(path.trim().to_string());
            cur_add = true;
            cur_hunk = Some(Hunk {
                old_start: Some(0),
                old_count: 0,
                new_count: 0,
                numbered: false,
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            flush_file(
                &mut cur_path,
                &mut cur_add,
                &mut cur_hunk,
                &mut cur_hunks,
                &mut rename_to,
                &mut out,
            );
            let path = path.trim().to_string();
            out.push(PatchFile {
                old_path: Some(path),
                new_path: None,
                rename_from: None,
                rename_to: None,
                hunks: Vec::new(),
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Move to: ") {
            let to = path.trim().to_string();
            if to.is_empty() {
                return Err(bad("bad patch: empty `*** Move to:`".to_string()));
            }
            if cur_path.is_none() {
                return Err(bad("bad patch: `*** Move to:` without a file".to_string()));
            }
            if rename_to.replace(to).is_some() {
                return Err(bad("bad patch: two `*** Move to:` for one file".to_string()));
            }
            continue;
        }
        if line.starts_with("*** ") {
            return Err(bad(format!("bad patch: unexpected line {line:?}")));
        }
        if line.starts_with("@@") {
            if cur_path.is_none() {
                return Err(bad(format!("bad patch: hunk outside a file: {line:?}")));
            }
            if cur_add {
                return Err(bad(
                    "bad patch: hunks are refused in `*** Add File:`".to_string()
                ));
            }
            flush_hunk(&mut cur_hunk, &mut cur_hunks);
            let inner = &line[2..];
            let end = inner.find("@@");
            let numbered = end.is_some_and(|e| {
                parse_hunk_ranges(inner[..e].trim(), "__codex__").is_ok()
            });
            if numbered {
                let end = end.unwrap_or(0);
                let (old_start, old_count, _, new_count) =
                    parse_hunk_ranges(inner[..end].trim(), "__codex__")?;
                cur_hunk = Some(Hunk {
                    old_start: Some(old_start),
                    old_count,
                    new_count,
                    numbered: true,
                    lines: Vec::new(),
                });
            } else {
                cur_hunk = Some(Hunk {
                    old_start: None,
                    old_count: 0,
                    new_count: 0,
                    numbered: false,
                    lines: Vec::new(),
                });
            }
            continue;
        }
        if line.trim().is_empty() {
            // A blank line ends the open hunk, so formatting blanks never
            // become content; an empty line inside a hunk or an added file
            // cannot be expressed (use a unified diff for those).
            flush_hunk(&mut cur_hunk, &mut cur_hunks);
            continue;
        }
        if line == "\\ No newline at end of file" {
            match cur_hunk.as_mut() {
                Some(h) => h.lines.push(HunkLine::NoNewline),
                None => return Err(bad("bad patch: content outside a hunk".to_string())),
            }
            continue;
        }
        if cur_add {
            match cur_hunk.as_mut() {
                Some(h) => {
                    if let Some(text) = line.strip_prefix('+') {
                        h.lines.push(HunkLine::Add(text.to_string()));
                    } else {
                        return Err(bad(format!("bad patch line: {line:?}")));
                    }
                }
                None => return Err(bad("bad patch: content outside a file".to_string())),
            }
            continue;
        }
        if let Some(h) = cur_hunk.as_mut() {
            if let Some(text) = line.strip_prefix('+') {
                h.lines.push(HunkLine::Add(text.to_string()));
            } else if let Some(text) = line.strip_prefix('-') {
                h.lines.push(HunkLine::Remove(text.to_string()));
            } else if let Some(text) = line.strip_prefix(' ') {
                h.lines.push(HunkLine::Context(text.to_string()));
            } else {
                return Err(bad(format!("bad patch line: {line:?}")));
            }
            continue;
        }
        return Err(bad("bad patch: content outside a hunk".to_string()));
    }
    if !done {
        return Err(bad("bad patch: missing `*** End Patch`".to_string()));
    }
    Ok(out)
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
                    // Seeding never formats; the flag is unused here.
                    is_text: false,
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
        let cfg = GitConfig::new(dir.path().join("git"), None);
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
            changes: CommitChanges::default(),
            author: author(),
        }
    }

    fn write(path: &str, content: &str) -> FileWrite {
        FileWrite {
            path: path.to_string(),
            body: WriteBody::Text(content.to_string()),
            expect_blob: None,
        }
    }

    fn edit(path: &str, old: &str, new: &str) -> Edit {
        Edit {
            path: path.to_string(),
            old: old.to_string(),
            new: new.to_string(),
            replace_all: false,
            expect_blob: None,
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
        r1.changes.writes.push(write("hello.txt", "one"));
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
        r2.changes.writes.push(write("hello.txt", "two"));
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
        r.changes.writes.push(write("app/index.ts", "export const x = 1;"));
        let head = commit(&store, &cfg, &sync, "rehyd", r).await.unwrap();

        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh_cfg = GitConfig::new(fresh_dir.path().join("git"), None);
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
        r1.changes.writes.push(write("f.txt", "1"));
        let h1 = commit(&store, &cfg, &sync, "cas", r1).await.unwrap();

        let mut r2 = req("two");
        r2.base = Some(h1.clone());
        r2.changes.writes.push(write("f.txt", "2"));
        let h2 = commit(&store, &cfg, &sync, "cas", r2).await.unwrap();
        assert_ne!(h1, h2);

        let mut stale = req("stale");
        stale.base = Some(h1);
        stale.changes.writes.push(write("f.txt", "stale"));
        match commit(&store, &cfg, &sync, "cas", stale).await {
            Err(CommitError::Conflict { current_sha }) => assert_eq!(current_sha, h2),
            other => panic!("a stale base must conflict, got {other:?}"),
        }

        // No base: reads the head and CASes, so the second commit builds on
        // the first instead of dropping it.
        let mut r3 = req("three");
        r3.changes.writes.push(write("g.txt", "3"));
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
        seed.changes.writes.push(write("page.tsx", "<h1>x</h1>\n<p>y</p>\n<h1>x</h1>\n"));
        let head = commit(&store, &cfg, &sync, "edits", seed).await.unwrap();
        let bare = manifest::mirror_path(&cfg, "edits");

        // Zero matches: error names the path and the count.
        let mut zero = req("zero");
        zero.changes.edits.push(Edit { path: "page.tsx".to_string(), old: "missing".to_string(), new: "n".to_string(), replace_all: false, expect_blob: None });
        match commit(&store, &cfg, &sync, "edits", zero).await {
            Err(CommitError::EditMatch { path, count }) => {
                assert_eq!(path, "page.tsx");
                assert_eq!(count, 0);
            }
            other => panic!("zero matches must fail, got {other:?}"),
        }

        // Two matches without replace_all: error counts both.
        let mut two = req("two");
        two.changes.edits.push(Edit { path: "page.tsx".to_string(), old: "<h1>x</h1>".to_string(), new: "<h1>z</h1>".to_string(), replace_all: false, expect_blob: None });
        match commit(&store, &cfg, &sync, "edits", two).await {
            Err(CommitError::EditMatch { path, count }) => {
                assert_eq!(path, "page.tsx");
                assert_eq!(count, 2);
            }
            other => panic!("two matches must fail, got {other:?}"),
        }

        // replace_all rewrites both.
        let mut all = req("all");
        all.changes.edits.push(Edit { path: "page.tsx".to_string(), old: "<h1>x</h1>".to_string(), new: "<h1>z</h1>".to_string(), replace_all: true, expect_blob: None });
        let h_all = commit(&store, &cfg, &sync, "edits", all).await.unwrap();
        assert_eq!(
            show(&bare, &h_all, "page.tsx").await,
            "<h1>z</h1>\n<p>y</p>\n<h1>z</h1>\n"
        );

        // Exactly one match without replace_all.
        let mut one = req("one");
        one.base = Some(h_all);
        one.changes.edits.push(Edit { path: "page.tsx".to_string(), old: "<p>y</p>".to_string(), new: "<p>w</p>".to_string(), replace_all: false, expect_blob: None });
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
                ok.changes.writes.push(write(&format!("f{i}.txt"), "x"));
            }
            commit(&store, &cfg, &sync, "cap-count", ok).await.unwrap();
            let mut over = req("over");
            for i in 0..101 {
                over.changes.writes.push(write(&format!("g{i}.txt"), "x"));
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
            ok.changes.writes.push(FileWrite { path: "t.txt".to_string(), body: WriteBody::Text("a".repeat(MAX_TEXT_BYTES)), expect_blob: None });
            commit(&store, &cfg, &sync, "cap-text", ok).await.unwrap();
            let mut over = req("over");
            over.changes.writes.push(FileWrite { path: "t.txt".to_string(), body: WriteBody::Text("a".repeat(MAX_TEXT_BYTES + 1)), expect_blob: None });
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-text", over).await,
                Err(CommitError::TextTooLarge { .. })
            ));
            // NUL bytes are refused even under the size cap.
            let mut nul = req("nul");
            nul.changes.writes.push(FileWrite { path: "t.txt".to_string(), body: WriteBody::Text("a\0b".to_string()), expect_blob: None });
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
            ok.changes.writes.push(FileWrite { path: "b.bin".to_string(), body: WriteBody::Base64(enc(MAX_UPLOAD_BYTES)), expect_blob: None });
            commit(&store, &cfg, &sync, "cap-b64", ok).await.unwrap();
            let mut over = req("over");
            over.changes.writes.push(FileWrite { path: "b.bin".to_string(), body: WriteBody::Base64(enc(MAX_UPLOAD_BYTES + 1)), expect_blob: None });
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
            ok.changes.writes.push(FileWrite { path: "a.bin".to_string(), body: WriteBody::Base64(enc(10 * 1024 * 1024)), expect_blob: None });
            ok.changes.writes.push(FileWrite { path: "b.bin".to_string(), body: WriteBody::Base64(enc(10 * 1024 * 1024)), expect_blob: None });
            ok.changes.writes.push(FileWrite { path: "c.bin".to_string(), body: WriteBody::Base64(enc(5 * 1024 * 1024)), expect_blob: None });
            commit(&store, &cfg, &sync, "cap-total", ok).await.unwrap();
            let mut over = req("over");
            over.changes.writes.push(FileWrite { path: "a.bin".to_string(), body: WriteBody::Base64(enc(10 * 1024 * 1024)), expect_blob: None });
            over.changes.writes.push(FileWrite { path: "b.bin".to_string(), body: WriteBody::Base64(enc(10 * 1024 * 1024)), expect_blob: None });
            over.changes.writes.push(FileWrite { path: "c.bin".to_string(), body: WriteBody::Base64(enc(5 * 1024 * 1024 + 1)), expect_blob: None });
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-total", over).await,
                Err(CommitError::TotalTooLarge { .. })
            ));
        }
        // moves/deletes: over the cap fails before any tree lookup.
        {
            let (_dir, cfg, store, sync) = setup("cap-mv").await;
            let mut over = req("over");
            over.changes.moves = (0..1001)
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
            over.changes.deletes = (0..1001).map(|i| format!("d{i}")).collect();
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-mv", over).await,
                Err(CommitError::TooManyDeletes { count: 1001 })
            ));
        }
        // message: 500 chars passes, 501 fails.
        {
            let (_dir, cfg, store, sync) = setup("cap-msg").await;
            let mut ok = req(&"m".repeat(500));
            ok.changes.writes.push(write("f.txt", "x"));
            commit(&store, &cfg, &sync, "cap-msg", ok).await.unwrap();
            let mut over = req(&"m".repeat(501));
            over.changes.writes.push(write("f.txt", "x"));
            assert!(matches!(
                commit(&store, &cfg, &sync, "cap-msg", over).await,
                Err(CommitError::BadMessage)
            ));
        }
        // path: 512 chars passes, 513 fails.
        {
            let (_dir, cfg, store, sync) = setup("cap-path").await;
            let mut ok = req("ok");
            ok.changes.writes.push(write(&format!("{}.txt", "p".repeat(508)), "x"));
            commit(&store, &cfg, &sync, "cap-path", ok).await.unwrap();
            let mut over = req("over");
            over.changes.writes.push(write(&format!("{}.txt", "p".repeat(509)), "x"));
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
            r.changes.writes.push(write(bad, "x"));
            assert!(
                matches!(
                    commit(&store, &cfg, &sync, "paths", r).await,
                    Err(CommitError::BadPath { .. })
                ),
                "{bad:?} must be refused"
            );
        }
        let mut r = req("missing");
        r.changes.deletes.push("nope.txt".to_string());
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
        v1.changes.writes.push(write("app.ts", "v1"));
        let h1 = commit(&store, &cfg, &sync, "rollback", v1).await.unwrap();
        let mut v2 = req("v2");
        v2.changes.writes.push(write("app.ts", "v2"));
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
        seed.changes.writes.push(write("f.txt", "aaa"));
        commit(&store, &cfg, &sync, "edit-caps", seed)
            .await
            .unwrap();
        let mut ok = req("ok");
        ok.changes.edits.push(Edit { path: "f.txt".to_string(), old: "aaa".to_string(), new: "a".repeat(MAX_TEXT_BYTES), replace_all: false, expect_blob: None });
        commit(&store, &cfg, &sync, "edit-caps", ok).await.unwrap();

        // ...and one byte over fails, against the new base.
        let mut over = req("over");
        over.changes.edits.push(Edit { path: "f.txt".to_string(), old: "a".repeat(MAX_TEXT_BYTES), new: "a".repeat(MAX_TEXT_BYTES + 1), replace_all: false, expect_blob: None });
        match commit(&store, &cfg, &sync, "edit-caps", over).await {
            Err(CommitError::TextTooLarge { path }) => assert_eq!(path, "f.txt"),
            other => panic!("an over-cap edit result must fail, got {other:?}"),
        }

        // A NUL byte in the edit result is refused like a NUL write.
        let (_dir, cfg, store, sync) = setup("edit-nul").await;
        let mut seed = req("seed");
        seed.changes.writes.push(write("f.txt", "aaa"));
        commit(&store, &cfg, &sync, "edit-nul", seed).await.unwrap();
        let mut nul = req("nul");
        nul.changes.edits.push(Edit { path: "f.txt".to_string(), old: "aaa".to_string(), new: "a\0b".to_string(), replace_all: false, expect_blob: None });
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
        seed.changes.writes.push(FileWrite { path: "big.txt".to_string(), body: WriteBody::Base64(base64::engine::general_purpose::STANDARD.encode(vec![
            b'a';
            MAX_TEXT_BYTES
                + 1
        ])), expect_blob: None });
        commit(&store, &cfg, &sync, "edit-big-base", seed)
            .await
            .unwrap();
        let mut big = req("big");
        big.changes.edits.push(Edit { path: "big.txt".to_string(), old: "a".to_string(), new: "b".to_string(), replace_all: true, expect_blob: None });
        match commit(&store, &cfg, &sync, "edit-big-base", big).await {
            Err(CommitError::TextTooLarge { path }) => assert_eq!(path, "big.txt"),
            other => panic!("editing an over-cap base must fail, got {other:?}"),
        }

        // Edit results count toward the same MAX_COMMIT_BYTES total as
        // writes: writes at exactly the cap plus any edit overflow it.
        let (_dir, cfg, store, sync) = setup("edit-total").await;
        let mut seed = req("seed");
        seed.changes.writes.push(write("e.txt", "aaa"));
        commit(&store, &cfg, &sync, "edit-total", seed)
            .await
            .unwrap();
        let enc = |n: usize| base64::engine::general_purpose::STANDARD.encode(vec![b'a'; n]);
        let mut big = req("big");
        big.changes.writes.push(FileWrite { path: "a.bin".to_string(), body: WriteBody::Base64(enc(10 * 1024 * 1024)), expect_blob: None });
        big.changes.writes.push(FileWrite { path: "b.bin".to_string(), body: WriteBody::Base64(enc(10 * 1024 * 1024)), expect_blob: None });
        big.changes.writes.push(FileWrite { path: "c.bin".to_string(), body: WriteBody::Base64(enc(5 * 1024 * 1024)), expect_blob: None });
        big.changes.edits.push(Edit { path: "e.txt".to_string(), old: "aaa".to_string(), new: "bbb".to_string(), replace_all: false, expect_blob: None });
        match commit(&store, &cfg, &sync, "edit-total", big).await {
            Err(CommitError::TotalTooLarge { .. }) => {}
            other => panic!("an edit overflowing the commit total must fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rollback_refuses_head_and_foreign_commits() {
        let (_dir, cfg, store, sync) = setup("rb-guard").await;
        let mut v1 = req("v1");
        v1.changes.writes.push(write("app.ts", "v1"));
        let h1 = commit(&store, &cfg, &sync, "rb-guard", v1).await.unwrap();
        let mut v2 = req("v2");
        v2.changes.writes.push(write("app.ts", "v2"));
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
        dup.changes.writes.push(write("f.txt", "one"));
        dup.changes.writes.push(write("f.txt", "two"));
        match commit(&store, &cfg, &sync, "overlap", dup).await {
            Err(CommitError::DuplicateWrite { path }) => assert_eq!(path, "f.txt"),
            other => panic!("duplicate writes must fail, got {other:?}"),
        }

        // A write and an edit on one path.
        let mut both = req("both");
        both.changes.writes.push(write("g.txt", "x"));
        both.changes.edits.push(Edit { path: "g.txt".to_string(), old: "x".to_string(), new: "y".to_string(), replace_all: false, expect_blob: None });
        match commit(&store, &cfg, &sync, "overlap", both).await {
            Err(CommitError::BothWrittenAndEdited { path }) => assert_eq!(path, "g.txt"),
            other => panic!("a write and an edit on one path must fail, got {other:?}"),
        }

        // The same path edited twice.
        let mut dedup = req("dedup");
        dedup.changes.edits.push(Edit { path: "h.txt".to_string(), old: "x".to_string(), new: "y".to_string(), replace_all: false, expect_blob: None });
        dedup.changes.edits.push(Edit { path: "h.txt".to_string(), old: "y".to_string(), new: "z".to_string(), replace_all: false, expect_blob: None });
        match commit(&store, &cfg, &sync, "overlap", dedup).await {
            Err(CommitError::DuplicateEdit { path }) => assert_eq!(path, "h.txt"),
            other => panic!("duplicate edits must fail, got {other:?}"),
        }

        // A move whose target is written in the same commit.
        let mut mv_write = req("mv-write");
        mv_write.changes.writes.push(write("w.txt", "x"));
        mv_write.changes.moves.push(Move {
            from: "a.txt".to_string(),
            to: "w.txt".to_string(),
        });
        match commit(&store, &cfg, &sync, "overlap", mv_write).await {
            Err(CommitError::BadMove(msg)) => assert!(msg.contains("w.txt"), "wrong path: {msg}"),
            other => panic!("a move onto a written path must fail, got {other:?}"),
        }

        // A move whose target is edited in the same commit.
        let mut mv_edit = req("mv-edit");
        mv_edit.changes.edits.push(Edit { path: "e.txt".to_string(), old: "x".to_string(), new: "y".to_string(), replace_all: false, expect_blob: None });
        mv_edit.changes.moves.push(Move {
            from: "a.txt".to_string(),
            to: "e.txt".to_string(),
        });
        match commit(&store, &cfg, &sync, "overlap", mv_edit).await {
            Err(CommitError::BadMove(msg)) => assert!(msg.contains("e.txt"), "wrong path: {msg}"),
            other => panic!("a move onto an edited path must fail, got {other:?}"),
        }

        // A move whose target is deleted in the same commit.
        let mut mv_delete = req("mv-delete");
        mv_delete.changes.deletes.push("d.txt".to_string());
        mv_delete.changes.moves.push(Move {
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
        seed.changes.writes.push(write("x.txt", "X"));
        seed.changes.writes.push(write("y.txt", "Y"));
        commit(&store, &cfg, &sync, "overlap", seed).await.unwrap();
        let mut chain = req("chain");
        chain.changes.moves.push(Move {
            from: "x.txt".to_string(),
            to: "y.txt".to_string(),
        });
        chain.changes.moves.push(Move {
            from: "y.txt".to_string(),
            to: "z.txt".to_string(),
        });
        let hc = commit(&store, &cfg, &sync, "overlap", chain).await.unwrap();
        let bare = manifest::mirror_path(&cfg, "overlap");
        assert_eq!(show(&bare, &hc, "y.txt").await, "X");
        assert_eq!(show(&bare, &hc, "z.txt").await, "Y");
    }

    /// A fake oxfmt: an executable shell script. The runner clears the
    /// environment, so absolute paths are baked into the script text.
    fn fake_oxfmt(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn setup_with_bin(
        slug: &str,
        bin: Option<std::path::PathBuf>,
    ) -> (tempfile::TempDir, GitConfig, Store, GitSync) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GitConfig::new(dir.path().join("git"), bin);
        let store = Store::memory();
        let sync = GitSync::default();
        manifest::init_repo(&store, &cfg, &sync, slug)
            .await
            .unwrap();
        (dir, cfg, store, sync)
    }

    /// Uppercase-everything fake that logs each formatted path (`$2` is the
    /// `--stdin-filepath` value) and the `-c` config it was given (`$4`).
    /// `.oxfmtrc.json` passes through unchanged so later commits in the same
    /// repo still read a valid tree config from head.
    fn logging_uppercaser(
        dir: &tempfile::TempDir,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let files_log = dir.path().join("files.log");
        let cfg_log = dir.path().join("cfg.log");
        let bin = fake_oxfmt(
            dir,
            "upper.sh",
            &format!(
                "echo \"$2\" >> {}\ncat \"$4\" >> {}\necho >> {}\ncase \"$2\" in *.oxfmtrc.json) cat;; *) tr 'a-z' 'A-Z';; esac",
                files_log.display(),
                cfg_log.display(),
                cfg_log.display(),
            ),
        );
        (bin, files_log, cfg_log)
    }

    fn logged_paths(log: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[tokio::test]
    async fn format_touches_only_formattable_text_and_lists_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, files_log, _cfg_log) = logging_uppercaser(&dir);
        let (_dir, cfg, store, sync) = setup_with_bin("fmt-only", Some(bin)).await;
        let mut r = req("format");
        r.changes.writes.push(write("app/a.ts", "const x = 1;\n"));
        r.changes.writes.push(write("app/clean.ts", "ABC\n"));
        r.changes.writes.push(write("app/note.txt", "hello\n"));
        r.changes.writes.push(FileWrite { path: "app/upload.ts".to_string(), body: WriteBody::Base64("Y29uc3QgeSA9IDI7Cg==".to_string()), expect_blob: None });
        let outcome = commit_with_outcome(&store, &cfg, &sync, "fmt-only", r)
            .await
            .unwrap();
        let bare = manifest::mirror_path(&cfg, "fmt-only");
        assert_eq!(
            show(&bare, &outcome.sha, "app/a.ts").await,
            "CONST X = 1;\n"
        );
        assert_eq!(show(&bare, &outcome.sha, "app/clean.ts").await, "ABC\n");
        assert_eq!(show(&bare, &outcome.sha, "app/note.txt").await, "hello\n");
        assert_eq!(
            show(&bare, &outcome.sha, "app/upload.ts").await,
            "const y = 2;\n",
            "base64 uploads are bytes, never formatted"
        );
        assert!(outcome.format_skipped.is_empty());
        // Only the two formattable text files reached the formatter (log
        // order is nondeterministic: files format concurrently); txt and
        // base64 never did.
        let mut logged = logged_paths(&files_log);
        logged.sort();
        assert_eq!(logged, vec!["app/a.ts", "app/clean.ts"]);
    }

    #[tokio::test]
    async fn format_covers_edits_but_not_moves_or_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, files_log, _cfg_log) = logging_uppercaser(&dir);
        let (_dir, cfg, store, sync) = setup_with_bin("fmt-mv", Some(bin)).await;
        let mut seed = req("seed");
        seed.changes.writes.push(write("edit.ts", "HELLO WORLD\n"));
        seed.changes.writes.push(write("move.ts", "HELLO\n"));
        seed.changes.writes.push(write("del.ts", "HELLO\n"));
        commit_with_outcome(&store, &cfg, &sync, "fmt-mv", seed)
            .await
            .unwrap();
        let logged_after_seed = logged_paths(&files_log).len();
        assert_eq!(logged_after_seed, 3);
        let mut change = req("change");
        change.changes.edits.push(Edit { path: "edit.ts".to_string(), old: "WORLD".to_string(), new: "world".to_string(), replace_all: false, expect_blob: None });
        change.changes.moves.push(Move {
            from: "move.ts".to_string(),
            to: "moved.ts".to_string(),
        });
        change.changes.deletes.push("del.ts".to_string());
        let outcome = commit_with_outcome(&store, &cfg, &sync, "fmt-mv", change)
            .await
            .unwrap();
        let bare = manifest::mirror_path(&cfg, "fmt-mv");
        assert_eq!(
            show(&bare, &outcome.sha, "edit.ts").await,
            "HELLO WORLD\n",
            "edit result passes through the formatter"
        );
        assert_eq!(show(&bare, &outcome.sha, "moved.ts").await, "HELLO\n");
        assert_eq!(outcome.formatted, vec!["edit.ts".to_string()]);
        assert!(outcome.format_skipped.is_empty());
        // Only the edited file reached the formatter; the move (a blob
        // re-point, content untouched) and the delete did not.
        assert_eq!(logged_paths(&files_log).len(), logged_after_seed + 1);
        assert_eq!(&logged_paths(&files_log)[logged_after_seed..], ["edit.ts"]);
    }

    #[tokio::test]
    async fn format_failure_commits_unformatted_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_oxfmt(
            &dir,
            "fail.sh",
            "case \"$2\" in *bad.ts*) echo '  × Unexpected token' >&2; exit 2;; *) tr 'a-z' 'A-Z';; esac",
        );
        let (_dir, cfg, store, sync) = setup_with_bin("fmt-fail", Some(bin)).await;
        let mut r = req("format");
        r.changes.writes.push(write("good.ts", "const x = 1;\n"));
        r.changes.writes.push(write("bad.ts", "const = SYNTAX-BROKEN ;;;\n"));
        let outcome = commit_with_outcome(&store, &cfg, &sync, "fmt-fail", r)
            .await
            .unwrap();
        let bare = manifest::mirror_path(&cfg, "fmt-fail");
        assert_eq!(show(&bare, &outcome.sha, "good.ts").await, "CONST X = 1;\n");
        assert_eq!(
            show(&bare, &outcome.sha, "bad.ts").await,
            "const = SYNTAX-BROKEN ;;;\n"
        );
        assert_eq!(outcome.formatted, vec!["good.ts".to_string()]);
        assert_eq!(outcome.format_skipped.len(), 1);
        assert_eq!(outcome.format_skipped[0].path, "bad.ts");
        assert!(
            outcome.format_skipped[0]
                .reason
                .contains("Unexpected token"),
            "reason carries the actionable line: {}",
            outcome.format_skipped[0].reason
        );
    }

    #[tokio::test]
    async fn format_timeout_commits_unformatted() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_oxfmt(&dir, "slow.sh", "sleep 30");
        let (_dir, cfg, store, sync) = setup_with_bin("fmt-slow", Some(bin)).await;
        let mut r = req("format");
        r.changes.writes.push(write("slow.ts", "const x = 1;\n"));
        let outcome = commit_with_outcome(&store, &cfg, &sync, "fmt-slow", r)
            .await
            .unwrap();
        let bare = manifest::mirror_path(&cfg, "fmt-slow");
        assert_eq!(show(&bare, &outcome.sha, "slow.ts").await, "const x = 1;\n");
        assert!(outcome.formatted.is_empty());
        assert_eq!(outcome.format_skipped.len(), 1);
        assert_eq!(outcome.format_skipped[0].path, "slow.ts");
        assert_eq!(outcome.format_skipped[0].reason, "timeout");
    }

    #[tokio::test]
    async fn format_hung_stdin_consumer_times_out_within_budget() {
        // The formatter never reads stdin and never exits. A >64 KB file
        // fills the pipe buffer, so `write_all` blocks: if any of the
        // per-file body sat outside the shared deadline, this commit would
        // hang forever (and hold its permit with it).
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("hung.pid");
        let bin = fake_oxfmt(
            &dir,
            "hung.sh",
            &format!("echo $$ > {}\nsleep 30", pidfile.display()),
        );
        let (_dir, cfg, store, sync) = setup_with_bin("fmt-hung", Some(bin)).await;
        let mut r = req("format");
        let big = "const x = 1;\n".repeat(8192);
        assert!(big.len() > 64 * 1024, "must fill the pipe buffer");
        r.changes.writes.push(FileWrite { path: "big.ts".to_string(), body: WriteBody::Text(big.clone()), expect_blob: None });
        let start = std::time::Instant::now();
        let outcome = commit_with_outcome(&store, &cfg, &sync, "fmt-hung", r)
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < super::super::format::FORMAT_TOTAL_TIMEOUT + Duration::from_secs(5),
            "commit must return within the deadline + slack, took {elapsed:?}",
        );
        assert_eq!(
            show(
                &manifest::mirror_path(&cfg, "fmt-hung"),
                &outcome.sha,
                "big.ts"
            )
            .await,
            big
        );
        assert!(outcome.formatted.is_empty());
        assert_eq!(outcome.format_skipped.len(), 1);
        assert_eq!(outcome.format_skipped[0].path, "big.ts");
        assert_eq!(outcome.format_skipped[0].reason, "timeout");
        // The hung process group actually died: no lingering `sleep`.
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut gone = false;
        for _ in 0..50 {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            gone,
            "timed-out formatter group must die, pid {pid} still alive"
        );
    }

    #[tokio::test]
    async fn format_oversized_output_commits_unformatted() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_oxfmt(
            &dir,
            "flood.sh",
            "head -c 2000000 /dev/zero | tr '\\000' 'y'",
        );
        let (_dir, cfg, store, sync) = setup_with_bin("fmt-flood", Some(bin)).await;
        let mut r = req("format");
        r.changes.writes.push(write("flood.ts", "const x = 1;\n"));
        let outcome = commit_with_outcome(&store, &cfg, &sync, "fmt-flood", r)
            .await
            .unwrap();
        let bare = manifest::mirror_path(&cfg, "fmt-flood");
        assert_eq!(
            show(&bare, &outcome.sha, "flood.ts").await,
            "const x = 1;\n"
        );
        assert!(outcome.formatted.is_empty());
        assert_eq!(outcome.format_skipped.len(), 1);
        assert_eq!(outcome.format_skipped[0].path, "flood.ts");
        assert_eq!(outcome.format_skipped[0].reason, "output too large");
    }

    #[tokio::test]
    async fn format_strips_tailwind_sorting_from_config_given_to_formatter() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _files_log, cfg_log) = logging_uppercaser(&dir);
        let (_dir, cfg, store, sync) = setup_with_bin("fmt-sanitize", Some(bin)).await;
        let mut r = req("format");
        r.changes.writes.push(write(
            ".oxfmtrc.json",
            "{\"singleQuote\": true, \"sortTailwindcss\": {\"config\": \"/tmp/evil/tailwind.config.js\"}, \"overrides\": [{\"files\": [\"*.ts\"], \"sortTailwindcss\": true}]}",
        ));
        r.changes.writes.push(write("app/a.ts", "const x = 1;\n"));
        commit_with_outcome(&store, &cfg, &sync, "fmt-sanitize", r)
            .await
            .unwrap();
        let cfgs = std::fs::read_to_string(&cfg_log).unwrap();
        assert!(
            !cfgs.contains("sortTailwindcss"),
            "formatter must never see it: {cfgs:?}"
        );
        assert!(
            !cfgs.contains("tailwind.config"),
            "no host paths leak: {cfgs:?}"
        );
        assert!(
            cfgs.contains("singleQuote"),
            "other options pass through: {cfgs:?}"
        );
    }
    #[tokio::test]
    async fn format_without_binary_commits_unchanged() {
        let (_dir, cfg, store, sync) = setup_with_bin("fmt-none", None).await;
        let mut r = req("format");
        r.changes.writes.push(write("app/a.ts", "const x = 1;\n"));
        let outcome = commit_with_outcome(&store, &cfg, &sync, "fmt-none", r)
            .await
            .unwrap();
        let bare = manifest::mirror_path(&cfg, "fmt-none");
        assert_eq!(
            show(&bare, &outcome.sha, "app/a.ts").await,
            "const x = 1;\n"
        );
        assert!(outcome.formatted.is_empty());
        assert!(outcome.format_skipped.is_empty());
    }

    #[tokio::test]
    async fn format_honors_tree_config_and_ignore_files() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, files_log, cfg_log) = logging_uppercaser(&dir);
        let (_dir, cfg, store, sync) = setup_with_bin("fmt-ignore", Some(bin)).await;
        let mut first = req("first");
        first.changes.writes.push(write(
            ".oxfmtrc.json",
            "{\"ignorePatterns\": [\"gen/**\"]}\n",
        ));
        first.changes.writes.push(write(".prettierignore", "skip/\n"));
        first.changes.writes.push(write("gen/a.ts", "const x = 1;\n"));
        first.changes.writes.push(write("skip/b.ts", "const x = 1;\n"));
        first.changes.writes.push(write("src/a.ts", "const x = 1;\n"));
        let outcome = commit_with_outcome(&store, &cfg, &sync, "fmt-ignore", first)
            .await
            .unwrap();
        let bare = manifest::mirror_path(&cfg, "fmt-ignore");
        assert_eq!(
            show(&bare, &outcome.sha, "gen/a.ts").await,
            "const x = 1;\n",
            "oxfmtrc ignorePatterns skips the whole subtree"
        );
        assert_eq!(
            show(&bare, &outcome.sha, "skip/b.ts").await,
            "const x = 1;\n",
            ".prettierignore skips the directory"
        );
        assert_eq!(
            show(&bare, &outcome.sha, "src/a.ts").await,
            "CONST X = 1;\n"
        );
        assert!(outcome.format_skipped.is_empty());
        let logged = logged_paths(&files_log);
        assert!(logged.contains(&"src/a.ts".to_string()), "{logged:?}");
        assert!(!logged.contains(&"gen/a.ts".to_string()), "{logged:?}");
        assert!(!logged.contains(&"skip/b.ts".to_string()), "{logged:?}");
        // The `-c` config the formatter saw came from the commit's own tree
        // state, not from disk.
        let cfgs = std::fs::read_to_string(&cfg_log).unwrap();
        assert!(cfgs.contains("gen/**"), "unexpected -c content: {cfgs:?}");

        // A later commit reads the config from head: the ignore still holds
        // without rewriting `.oxfmtrc.json`.
        let mut second = req("second");
        second.changes.writes.push(write("src/b.ts", "const y = 2;\n"));
        second.changes.writes.push(write("gen/b.ts", "const y = 2;\n"));
        let outcome = commit_with_outcome(&store, &cfg, &sync, "fmt-ignore", second)
            .await
            .unwrap();
        assert_eq!(
            show(&bare, &outcome.sha, "src/b.ts").await,
            "CONST Y = 2;\n"
        );
        assert_eq!(
            show(&bare, &outcome.sha, "gen/b.ts").await,
            "const y = 2;\n"
        );
        assert_eq!(outcome.formatted, vec!["src/b.ts".to_string()]);
    }

    #[tokio::test]
    #[ignore = "needs the real oxfmt binary: OUTER_TEST_OXFMT_BIN=/path/to/oxfmt"]
    async fn format_real_oxfmt_end_to_end() {
        let bin = match std::env::var("OUTER_TEST_OXFMT_BIN") {
            Ok(bin) => bin,
            Err(_) => {
                eprintln!("skipping: set OUTER_TEST_OXFMT_BIN to run");
                return;
            }
        };
        let (_dir, cfg, store, sync) =
            setup_with_bin("fmt-real", Some(std::path::PathBuf::from(bin))).await;
        let mut r = req("format");
        r.changes.writes.push(write(".oxfmtrc.json", "{\"singleQuote\": true}\n"));
        r.changes.writes.push(write("app/a.js", "const s = \"hi\"\n"));
        r.changes.writes.push(write("app/b.json", "{\"b\":1,\"a\":2}\n"));
        r.changes.writes.push(write("app/c.css", "a{color:red}\n"));
        r.changes.writes.push(write("app/d.md", "# hi\n"));
        let outcome = commit_with_outcome(&store, &cfg, &sync, "fmt-real", r)
            .await
            .unwrap();
        let bare = manifest::mirror_path(&cfg, "fmt-real");
        // The tree's `-c` config applied (single quotes), proving stdin mode
        // plus pinned-config plumbing with no on-disk discovery.
        assert_eq!(
            show(&bare, &outcome.sha, "app/a.js").await,
            "const s = 'hi';\n"
        );
        assert_eq!(
            show(&bare, &outcome.sha, "app/b.json").await,
            "{ \"b\": 1, \"a\": 2 }\n"
        );
        assert_eq!(
            show(&bare, &outcome.sha, "app/c.css").await,
            "a {\n  color: red;\n}\n"
        );
        assert_eq!(show(&bare, &outcome.sha, "app/d.md").await, "# hi\n");
        // The config formats itself (JSON spacing) without changing meaning.
        assert_eq!(
            show(&bare, &outcome.sha, ".oxfmtrc.json").await,
            "{ \"singleQuote\": true }\n"
        );
        assert_eq!(
            outcome.formatted,
            vec![
                ".oxfmtrc.json".to_string(),
                "app/a.js".to_string(),
                "app/b.json".to_string(),
                "app/c.css".to_string(),
            ]
        );
        assert!(outcome.format_skipped.is_empty());
    }

    #[tokio::test]
    async fn preview_branches_from_main_and_promotes() {
        let (_dir, cfg, store, sync) = setup("prev").await;
        let mut live = req("live one");
        live.changes.writes.push(write("app.txt", "live"));
        let main1 = commit(&store, &cfg, &sync, "prev", live).await.unwrap();

        // The first preview commit parents on the main head.
        let mut p1 = req("trial one");
        p1.changes.writes.push(write("app.txt", "trial"));
        let t1 = commit_preview(&store, &cfg, &sync, "prev", p1)
            .await
            .unwrap()
            .sha;
        let bare = manifest::mirror_path(&cfg, "prev");
        assert_eq!(
            manifest::head_of_ref(&bare, manifest::PREVIEW_REF)
                .await
                .unwrap()
                .as_deref(),
            Some(t1.as_str())
        );
        assert_eq!(
            manifest::head_of(&bare).await.unwrap().as_deref(),
            Some(main1.as_str()),
            "main is untouched by preview commits"
        );
        let manifest = manifest::read_manifest(&store, "prev")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(manifest.main.unwrap().sha, main1);
        let preview_entry = manifest.preview.clone().unwrap();
        assert_eq!(preview_entry.sha, t1);
        assert!(
            store.get(&preview_entry.bundle).await.unwrap().is_some(),
            "preview bundle uploaded"
        );
        assert!(
            !preview_entry.bundle.contains("/main/"),
            "preview bundles live outside the main prefix"
        );

        // A second preview commit parents on the preview head; a live
        // commit in between does not disturb it.
        let mut live2 = req("live two");
        live2.changes.writes.push(write("other.txt", "live"));
        let main2 = commit(&store, &cfg, &sync, "prev", live2).await.unwrap();
        let mut p2 = req("trial two");
        p2.changes.writes.push(write("app.txt", "trial two"));
        p2.base = Some(t1.clone());
        let t2 = commit_preview(&store, &cfg, &sync, "prev", p2)
            .await
            .unwrap()
            .sha;
        assert_eq!(
            manifest::head_of(&bare).await.unwrap().as_deref(),
            Some(main2.as_str())
        );
        // A tip that no longer descends from main refuses the promote
        // instead of superseding the live change; the agent remakes the
        // preview from the new main (discard + fresh trial) and promotes
        // that.
        let moved = manifest::promote_preview(&store, &cfg, &sync, "prev", &t2).await;
        assert!(
            format!("{moved:#?}").contains("main moved"),
            "got: {moved:#?}"
        );
        assert!(manifest::clear_preview(&store, &cfg, &sync, "prev")
            .await
            .unwrap());
        let mut p3 = req("trial three");
        p3.changes.writes.push(write("app.txt", "trial three"));
        p3.base = Some(main2.clone());
        let t3 = commit_preview(&store, &cfg, &sync, "prev", p3)
            .await
            .unwrap()
            .sha;
        // Promote fast-forwards main onto the preview tip and clears it.
        let promoted = manifest::promote_preview(&store, &cfg, &sync, "prev", &t3)
            .await
            .unwrap();
        assert_eq!(promoted, t3);
        assert_eq!(
            manifest::head_of(&bare).await.unwrap().as_deref(),
            Some(t3.as_str())
        );
        assert!(
            manifest::head_of_ref(&bare, manifest::PREVIEW_REF)
                .await
                .unwrap()
                .is_none(),
            "the mirror preview ref is gone after promote"
        );
        let manifest = manifest::read_manifest(&store, "prev")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(manifest.main.unwrap().sha, t3);
        assert!(manifest.preview.is_none());
        assert!(
            store.list("git/prev/preview/").await.unwrap().is_empty(),
            "preview bundles deleted on promote"
        );
    }

    #[tokio::test]
    async fn preview_cas_and_promote_conflicts() {
        let (_dir, cfg, store, sync) = setup("prevcas").await;
        let mut live = req("live");
        live.changes.writes.push(write("app.txt", "live"));
        let main1 = commit(&store, &cfg, &sync, "prevcas", live).await.unwrap();

        // A preview on an empty main is refused: there is nothing to trial.
        let (_dir2, cfg2, store2, sync2) = setup("prevempty").await;
        let mut seedless = req("x");
        seedless.changes.writes.push(write("x.txt", "x"));
        let err = commit_preview(&store2, &cfg2, &sync2, "prevempty", seedless)
            .await
            .unwrap_err();
        assert!(matches!(err, CommitError::NoMainToPreview));

        // Stale preview base conflicts against the preview head, naming it.
        let mut p1 = req("trial");
        p1.changes.writes.push(write("app.txt", "trial"));
        let t1 = commit_preview(&store, &cfg, &sync, "prevcas", p1)
            .await
            .unwrap()
            .sha;
        let mut stale = req("stale trial");
        stale.changes.writes.push(write("app.txt", "stale"));
        stale.base = Some(main1.clone());
        let err = commit_preview(&store, &cfg, &sync, "prevcas", stale)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CommitError::PreviewConflict { ref current_sha } if current_sha == &t1
        ));

        // A live commit after the preview branched refuses the promote:
        // main is no longer an ancestor of the preview tip.
        let mut live2 = req("live two");
        live2.changes.writes.push(write("other.txt", "live"));
        commit(&store, &cfg, &sync, "prevcas", live2).await.unwrap();
        let err = manifest::promote_preview(&store, &cfg, &sync, "prevcas", &t1)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("main moved"), "got: {err:#}");
    }

    #[tokio::test]
    async fn rehydrate_restores_both_refs_and_clear_drops_preview() {
        let (_dir, cfg, store, sync) = setup("prevhyd").await;
        let mut live = req("live");
        live.changes.writes.push(write("app.txt", "live"));
        let main = commit(&store, &cfg, &sync, "prevhyd", live).await.unwrap();
        let mut trial = req("trial");
        trial.changes.writes.push(write("app.txt", "trial"));
        let tip = commit_preview(&store, &cfg, &sync, "prevhyd", trial)
            .await
            .unwrap()
            .sha;

        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh_cfg = GitConfig::new(fresh_dir.path().join("git"), None);
        let fresh_sync = GitSync::default();
        let got = manifest::rehydrate(&store, &fresh_cfg, &fresh_sync, "prevhyd")
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(main.as_str()));
        let bare = manifest::mirror_path(&fresh_cfg, "prevhyd");
        assert_eq!(
            manifest::head_of_ref(&bare, manifest::PREVIEW_REF)
                .await
                .unwrap()
                .as_deref(),
            Some(tip.as_str())
        );

        assert!(manifest::clear_preview(&store, &cfg, &sync, "prevhyd")
            .await
            .unwrap());
        let manifest = manifest::read_manifest(&store, "prevhyd")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(manifest.main.unwrap().sha, main);
        assert!(manifest.preview.is_none());
        manifest::delete_preview_bundles(&store, "prevhyd").await;
        // Clearing twice is quiet.
        assert!(!manifest::clear_preview(&store, &cfg, &sync, "prevhyd")
            .await
            .unwrap());
    }
}
