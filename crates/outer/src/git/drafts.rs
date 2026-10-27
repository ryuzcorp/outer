//! MCP drafts (spec §1.5): staged changes that survive restarts.
//!
//! A draft is per account per app: the ops (same shapes as `commit`) plus
//! the main head they were staged against. Staging validates shapes only;
//! building the draft tree, diffing it and shipping it happen on demand, so
//! `stage` never touches git blobs.
//!
//! The draft tree is a dangling commit parented on the base head: it never
//! touches a ref, so mirror rehydrates (which rebuild the bare dir) simply
//! drop it and it is rebuilt from the stored ops. When the shared
//! `apply_changes` seam lands (McpPreciseEdits), validation and the
//! tree build here delegate to it; until then the shapes mirror
//! [`crate::git::commit`] exactly.

use std::path::Path;
use std::time::Duration;

use super::commit::{
    Author, CommitError, Edit, FileWrite, Move, WriteBody, MAX_COMMIT_BYTES, MAX_DELETES,
    MAX_MOVES, MAX_PATH_CHARS, MAX_TEXT_BYTES, MAX_UPLOAD_BYTES, MAX_WRITES_EDITS,
};

/// One staged full-file write: exactly one of `content`, `base64`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DraftWrite {
    pub path: String,
    pub content: Option<String>,
    pub base64: Option<String>,
}

/// One staged exact-match edit.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DraftEdit {
    pub path: String,
    pub old: String,
    pub new: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// One staged move.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DraftMove {
    pub from: String,
    pub to: String,
}

/// Everything staged in one draft, in `commit` input shapes.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DraftOps {
    #[serde(default)]
    pub writes: Vec<DraftWrite>,
    #[serde(default)]
    pub edits: Vec<DraftEdit>,
    #[serde(default)]
    pub deletes: Vec<String>,
    #[serde(default)]
    pub moves: Vec<DraftMove>,
}

impl DraftOps {
    /// True when nothing is staged.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
            && self.edits.is_empty()
            && self.deletes.is_empty()
            && self.moves.is_empty()
    }

    /// Append one `stage` call's ops.
    pub fn append(
        &mut self,
        writes: Vec<DraftWrite>,
        edits: Vec<DraftEdit>,
        deletes: Vec<String>,
        moves: Vec<DraftMove>,
    ) {
        self.writes.extend(writes);
        self.edits.extend(edits);
        self.deletes.extend(deletes);
        self.moves.extend(moves);
    }
}

/// The stored row: the base head plus the staged ops.
#[derive(Debug, Clone)]
pub struct Draft {
    pub base_sha: String,
    pub ops: DraftOps,
}

/// Validate staged ops the same way `commit` validates its request: every
/// cap, path rule and cross-field clash, without touching git. The merged
/// ops of repeated `stage` calls validate as one, so a file written twice
/// across two calls fails here exactly like one call writing it twice.
pub fn validate_ops(ops: &DraftOps) -> Result<(), CommitError> {
    if ops.is_empty() {
        return Err(CommitError::EmptyChange);
    }
    if ops.writes.len() + ops.edits.len() > MAX_WRITES_EDITS {
        return Err(CommitError::TooManyFiles {
            count: ops.writes.len() + ops.edits.len(),
        });
    }
    if ops.moves.len() > MAX_MOVES {
        return Err(CommitError::TooManyMoves {
            count: ops.moves.len(),
        });
    }
    if ops.deletes.len() > MAX_DELETES {
        return Err(CommitError::TooManyDeletes {
            count: ops.deletes.len(),
        });
    }
    let mut decoded = 0usize;
    let mut seen_writes = std::collections::HashSet::new();
    for f in &ops.writes {
        validate_path(&f.path)?;
        if !seen_writes.insert(f.path.as_str()) {
            return Err(CommitError::DuplicateWrite {
                path: f.path.clone(),
            });
        }
        decoded += match (&f.content, &f.base64) {
            (Some(text), None) => {
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
                text.len()
            }
            (None, Some(encoded)) => {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|source| CommitError::BadBase64 {
                        path: f.path.clone(),
                        source,
                    })?;
                if bytes.len() > MAX_UPLOAD_BYTES {
                    return Err(CommitError::FileTooLarge {
                        path: f.path.clone(),
                    });
                }
                bytes.len()
            }
            _ => {
                return Err(CommitError::InvalidEdit {
                    path: f.path.clone(),
                    reason: "needs exactly one of content, base64",
                });
            }
        };
    }
    if decoded > MAX_COMMIT_BYTES {
        return Err(CommitError::TotalTooLarge { bytes: decoded });
    }
    for path in &ops.deletes {
        validate_path(path)?;
        if ops.writes.iter().any(|f| &f.path == path) || ops.edits.iter().any(|e| &e.path == path) {
            return Err(CommitError::BothWrittenAndDeleted { path: path.clone() });
        }
    }
    let mut seen_edits = std::collections::HashSet::new();
    for e in &ops.edits {
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
    let moves: Vec<Move> = ops
        .moves
        .iter()
        .map(|m| Move {
            from: m.from.clone(),
            to: m.to.clone(),
        })
        .collect();
    let writes: Vec<FileWrite> = ops
        .writes
        .iter()
        .map(|w| FileWrite {
            path: w.path.clone(),
            body: WriteBody::Text(String::new()),
        })
        .collect();
    let edits: Vec<Edit> = ops
        .edits
        .iter()
        .map(|e| Edit {
            path: e.path.clone(),
            old: String::new(),
            new: String::new(),
            replace_all: false,
        })
        .collect();
    validate_moves(&moves, &writes, &edits, &ops.deletes)?;
    Ok(())
}

/// Reject worktree escapes and git internals before a path touches git.
/// Mirrors `commit.rs` until the shared seam lands.
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

/// Reject bad moves before anything touches git. Mirrors `commit.rs` until
/// the shared seam lands.
fn validate_moves(
    moves: &[Move],
    writes: &[FileWrite],
    edits: &[Edit],
    deletes: &[String],
) -> Result<(), CommitError> {
    let mut froms = std::collections::HashSet::new();
    let mut tos = std::collections::HashSet::new();
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

/// Build the draft tree on `base` and return a dangling commit parented on
/// it (no ref is touched). Applying against the current head is the rebase
/// check: it succeeds exactly when the staged ops still apply cleanly.
pub async fn build_draft_commit(
    bare: &Path,
    base: &str,
    ops: &DraftOps,
    author: &Author,
) -> Result<String, CommitError> {
    validate_ops(ops)?;
    use super::manifest::{run_git, run_git_stdin};
    let git_dir = format!("--git-dir={}", bare.display());
    static INDEX_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = INDEX_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let index = std::env::temp_dir().join(format!(
        "outer-draft-{}-{nanos}-{n}.idx",
        std::process::id()
    ));
    let index_str = index.to_string_lossy().to_string();
    let env_index = [("GIT_INDEX_FILE", index_str.as_str())];
    let result = async {
        run_git(
            &[&git_dir, "read-tree", base],
            &env_index,
            Duration::from_secs(30),
        )
        .await?;
        // Moves first: every `from` resolves in the base tree, chains and
        // swaps landing on the right blobs.
        let mut removals = String::new();
        let mut additions = String::new();
        for m in &ops.moves {
            let entry =
                tree_blob(&git_dir, base, &m.from)
                    .await?
                    .ok_or_else(|| CommitError::NotFound {
                        path: m.from.clone(),
                    })?;
            let vacated = ops.deletes.contains(&m.to) || ops.moves.iter().any(|o| o.from == m.to);
            if !vacated && tree_entry(&git_dir, base, &m.to).await?.is_some() {
                return Err(CommitError::Internal(anyhow::anyhow!(
                    "{} already exists",
                    m.to
                )));
            }
            removals.push_str(&format!("0 {}\t{}\0", "0".repeat(40), m.from));
            additions.push_str(&format!("{} {}\t{}\0", entry.mode, entry.sha, m.to));
        }
        if !ops.moves.is_empty() {
            let info = format!("{removals}{additions}");
            run_git_stdin(
                &[&git_dir, "update-index", "-z", "--index-info"],
                info.as_bytes(),
                &env_index,
                Duration::from_secs(30),
            )
            .await?;
        }
        // Writes, then exact-match edits (same semantics as `commit`).
        let mut total = 0usize;
        let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(ops.writes.len());
        for w in &ops.writes {
            let bytes = match (&w.content, &w.base64) {
                (Some(text), None) => text.as_bytes().to_vec(),
                (None, Some(encoded)) => {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|source| CommitError::BadBase64 {
                            path: w.path.clone(),
                            source,
                        })?
                }
                _ => {
                    return Err(CommitError::Internal(anyhow::anyhow!(
                        "unvalidated write for {}",
                        w.path
                    )));
                }
            };
            total = total.saturating_add(bytes.len());
            files.push((w.path.clone(), bytes));
        }
        for e in &ops.edits {
            let bytes = super::manifest::run_git_raw(
                &[&git_dir, "cat-file", "-p", &format!("{base}:{}", e.path)],
                &[],
                Duration::from_secs(30),
            )
            .await
            .map_err(|_| CommitError::NotFound {
                path: e.path.clone(),
            })?;
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
            files.push((e.path.clone(), replaced.into_bytes()));
        }
        for (path, bytes) in &files {
            let blob = run_git_stdin(
                &[&git_dir, "hash-object", "-w", "--stdin", "--no-filters"],
                bytes,
                &[],
                Duration::from_secs(60),
            )
            .await?;
            let sha = String::from_utf8(blob)
                .map_err(|e| CommitError::Internal(e.into()))?
                .trim()
                .to_string();
            run_git(
                &[
                    &git_dir,
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{sha},{path}"),
                ],
                &env_index,
                Duration::from_secs(30),
            )
            .await?;
        }
        // Deletes last.
        for path in &ops.deletes {
            if tree_blob(&git_dir, base, path).await?.is_none() {
                return Err(CommitError::NotFound { path: path.clone() });
            }
            let removal = format!("0 {}\t{path}\0", "0".repeat(40));
            run_git_stdin(
                &[&git_dir, "update-index", "-z", "--index-info"],
                removal.as_bytes(),
                &env_index,
                Duration::from_secs(30),
            )
            .await?;
        }
        let tree = run_git(
            &[&git_dir, "write-tree"],
            &env_index,
            Duration::from_secs(30),
        )
        .await?;
        let commit = run_git(
            &[
                &git_dir,
                "commit-tree",
                tree.trim(),
                "-p",
                base,
                "-m",
                "draft",
            ],
            &author_env(author),
            Duration::from_secs(30),
        )
        .await?;
        Ok::<_, CommitError>(commit.trim().to_string())
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

struct TreeEntry {
    mode: String,
    sha: String,
}

async fn tree_entry(git_dir: &str, rev: &str, path: &str) -> anyhow::Result<Option<TreeEntry>> {
    let listing = super::manifest::run_git(
        &[git_dir, "ls-tree", rev, "--", &format!(":(literal){path}")],
        &[],
        Duration::from_secs(15),
    )
    .await?;
    let mut fields = listing.split_whitespace();
    let Some(mode) = fields.next() else {
        return Ok(None);
    };
    let kind = fields.next().unwrap_or_default();
    let sha = fields.next().unwrap_or_default().to_string();
    if kind != "blob" {
        return Ok(None);
    }
    Ok(Some(TreeEntry {
        mode: mode.to_string(),
        sha,
    }))
}

async fn tree_blob(git_dir: &str, rev: &str, path: &str) -> anyhow::Result<Option<TreeEntry>> {
    tree_entry(git_dir, rev, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops_write(path: &str, content: &str) -> DraftOps {
        DraftOps {
            writes: vec![DraftWrite {
                path: path.to_string(),
                content: Some(content.to_string()),
                base64: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn empty_stage_is_an_error() {
        let err = validate_ops(&DraftOps::default()).unwrap_err();
        assert!(matches!(err, CommitError::EmptyChange));
    }

    #[test]
    fn write_needs_exactly_one_body() {
        let mut ops = ops_write("a.txt", "hi");
        ops.writes[0].base64 = Some("aGk=".to_string());
        let err = validate_ops(&ops).unwrap_err();
        assert!(matches!(err, CommitError::InvalidEdit { .. }));
    }

    #[test]
    fn duplicate_write_across_stage_calls_fails_merged() {
        let mut ops = ops_write("a.txt", "one");
        ops.append(
            vec![DraftWrite {
                path: "a.txt".to_string(),
                content: Some("two".to_string()),
                base64: None,
            }],
            vec![],
            vec![],
            vec![],
        );
        let err = validate_ops(&ops).unwrap_err();
        assert!(matches!(err, CommitError::DuplicateWrite { .. }));
    }

    #[test]
    fn dotdot_path_is_refused() {
        let ops = ops_write("../evil.txt", "x");
        let err = validate_ops(&ops).unwrap_err();
        assert!(matches!(err, CommitError::BadPath { .. }));
    }
}
