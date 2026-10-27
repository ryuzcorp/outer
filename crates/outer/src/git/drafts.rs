//! MCP drafts (spec §1.5): staged changes that survive restarts.
//!
//! A draft is per account per app: the ops (the full `commit` shapes,
//! [`CommitChanges`]) plus the main head they were staged against, stored in
//! the `draft` table so they survive restarts.
//!
//! Staging validates exactly like `commit`: the merged ops become a
//! [`CommitRequest`] and run [`validate_request`] plus [`apply_changes`]
//! against the current head, so a later `ship` cannot fail on shapes, stale
//! blobs, ranges or patch rejects. Building the draft tree (for `diff_draft`,
//! `check` and the `ship` rebase check) runs the same two functions against
//! any head and commits the result with [`build_commit`] — the same code
//! `commit_to_ref` runs, minus formatting, the gate and the publish. The
//! draft tree is a dangling commit, never a ref, so mirror rehydrates simply
//! drop it and it is rebuilt from the stored ops.

use std::path::Path;

use super::commit::{
    apply_changes, build_commit, validate_request, Author, CommitChanges, CommitError,
    CommitRequest, Edit, FileWrite, Insert, LineEdit, Move, WriteBody,
};

/// One staged full-file write: exactly one of `content`, `base64`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DraftWrite {
    pub path: String,
    pub content: Option<String>,
    pub base64: Option<String>,
    pub expect_blob: Option<String>,
}

/// One staged exact-match edit.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DraftEdit {
    pub path: String,
    pub old: String,
    pub new: String,
    #[serde(default)]
    pub replace_all: bool,
    pub expect_blob: Option<String>,
}

/// One staged line-range replacement (1-based, inclusive, `read_files`
/// numbering; empty `new` deletes the lines).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DraftLineEdit {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub new: String,
    pub expect_blob: Option<String>,
}

/// One staged insertion after a line (0 is the top of the file).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DraftInsert {
    pub path: String,
    pub after_line: u64,
    pub text: String,
    pub expect_blob: Option<String>,
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
    pub line_edits: Vec<DraftLineEdit>,
    #[serde(default)]
    pub inserts: Vec<DraftInsert>,
    #[serde(default)]
    pub patch: Option<String>,
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
            && self.line_edits.is_empty()
            && self.inserts.is_empty()
            && self.patch.is_none()
            && self.deletes.is_empty()
            && self.moves.is_empty()
    }

    /// True when the staged ops hold anything besides a patch.
    pub fn has_non_patch(&self) -> bool {
        !self.writes.is_empty()
            || !self.edits.is_empty()
            || !self.line_edits.is_empty()
            || !self.inserts.is_empty()
            || !self.deletes.is_empty()
            || !self.moves.is_empty()
    }

    /// Append one `stage` call's ops. Two patches never fold: a patch
    /// applies with exact context against the base it was written for, so a
    /// second staged patch (written against the same base, before the first
    /// applied) would almost surely reject. The caller refuses it with
    /// [`CommitError::MixedPatch`], exactly like a patch mixed with any other
    /// change kind.
    pub fn append(&mut self, other: DraftOps) -> Result<(), CommitError> {
        if self.patch.is_some() && other.patch.is_some() {
            return Err(CommitError::MixedPatch);
        }
        self.writes.extend(other.writes);
        self.edits.extend(other.edits);
        self.line_edits.extend(other.line_edits);
        self.inserts.extend(other.inserts);
        if other.patch.is_some() {
            self.patch = other.patch;
        }
        self.deletes.extend(other.deletes);
        self.moves.extend(other.moves);
        Ok(())
    }

    /// Map staged ops onto the shared [`CommitChanges`]. A write needs
    /// exactly one of `content`, `base64`, exactly like `commit`.
    pub fn into_changes(self) -> Result<CommitChanges, CommitError> {
        let mut writes = Vec::with_capacity(self.writes.len());
        for w in self.writes {
            let body = match (w.content, w.base64) {
                (Some(text), None) => WriteBody::Text(text),
                (None, Some(encoded)) => WriteBody::Base64(encoded),
                _ => {
                    return Err(CommitError::InvalidEdit {
                        path: w.path,
                        reason: "needs exactly one of content, base64",
                    });
                }
            };
            writes.push(FileWrite {
                path: w.path,
                body,
                expect_blob: w.expect_blob,
            });
        }
        Ok(CommitChanges {
            writes,
            edits: self
                .edits
                .into_iter()
                .map(|e| Edit {
                    path: e.path,
                    old: e.old,
                    new: e.new,
                    replace_all: e.replace_all,
                    expect_blob: e.expect_blob,
                })
                .collect(),
            line_edits: self
                .line_edits
                .into_iter()
                .map(|e| LineEdit {
                    path: e.path,
                    start_line: e.start_line,
                    end_line: e.end_line,
                    new: e.new,
                    expect_blob: e.expect_blob,
                })
                .collect(),
            inserts: self
                .inserts
                .into_iter()
                .map(|i| Insert {
                    path: i.path,
                    after_line: i.after_line,
                    text: i.text,
                    expect_blob: i.expect_blob,
                })
                .collect(),
            patch: self.patch,
            deletes: self.deletes,
            moves: self
                .moves
                .into_iter()
                .map(|m| Move {
                    from: m.from,
                    to: m.to,
                })
                .collect(),
        })
    }
}

/// The stored row: the base head plus the staged ops.
#[derive(Debug, Clone)]
pub struct Draft {
    pub base_sha: String,
    pub ops: DraftOps,
}

/// Build the [`CommitRequest`] staging validates and the draft tree applies.
/// The message is never published (the draft tree is dangling); the author
/// seeds the commit-tree environment.
fn to_request(ops: DraftOps, author: &Author) -> Result<CommitRequest, CommitError> {
    Ok(CommitRequest {
        message: "draft".to_string(),
        base: None,
        changes: ops.into_changes()?,
        author: author.clone(),
    })
}

/// Validate staged ops exactly like `commit` (shapes, caps, blob ids) and
/// apply them against `base` without building anything: `stage` runs this,
/// so whatever it stores, `ship` can commit.
pub async fn check_ops(
    bare: &Path,
    base: &str,
    ops: DraftOps,
    author: &Author,
) -> Result<(), CommitError> {
    let req = to_request(ops, author)?;
    let (_, mut files) = validate_request(&req)?;
    let mut deletes = req.changes.deletes.clone();
    let mut moves = req.changes.moves.clone();
    apply_changes(
        bare,
        Some(base),
        &req.changes,
        &mut files,
        &mut deletes,
        &mut moves,
    )
    .await
}

/// Build the draft tree on `base` and return a dangling commit parented on
/// it (no ref is touched). Applying against the current head is the rebase
/// check: it succeeds exactly when the staged ops still apply cleanly.
pub async fn build_draft_commit(
    bare: &Path,
    base: &str,
    ops: DraftOps,
    author: &Author,
) -> Result<String, CommitError> {
    let req = to_request(ops, author)?;
    let (message, mut files) = validate_request(&req)?;
    let mut deletes = req.changes.deletes.clone();
    let mut moves = req.changes.moves.clone();
    apply_changes(
        bare,
        Some(base),
        &req.changes,
        &mut files,
        &mut deletes,
        &mut moves,
    )
    .await?;
    build_commit(
        bare,
        Some(base),
        &req.author,
        &message,
        &files,
        &moves,
        &deletes,
    )
    .await
    .map_err(CommitError::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn author() -> Author {
        Author {
            name: "test".to_string(),
            email: "test@example.com".to_string(),
        }
    }

    fn ops_write(path: &str, content: &str) -> DraftOps {
        DraftOps {
            writes: vec![DraftWrite {
                path: path.to_string(),
                content: Some(content.to_string()),
                base64: None,
                expect_blob: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn empty_changes_fail_like_commit() {
        let req = to_request(DraftOps::default(), &author()).unwrap();
        let err = match validate_request(&req) {
            Ok(_) => panic!("empty draft validates"),
            Err(e) => e,
        };
        assert!(matches!(err, CommitError::EmptyChange));
    }

    #[test]
    fn write_needs_exactly_one_body() {
        let mut ops = ops_write("a.txt", "hi");
        ops.writes[0].base64 = Some("aGk=".to_string());
        let err = ops.into_changes().unwrap_err();
        assert!(matches!(err, CommitError::InvalidEdit { .. }));
    }

    #[test]
    fn duplicate_write_across_stage_calls_fails_merged() {
        let mut ops = ops_write("a.txt", "one");
        ops.append(ops_write("a.txt", "two")).unwrap();
        let req = to_request(ops, &author()).unwrap();
        let err = match validate_request(&req) {
            Ok(_) => panic!("duplicate write validates"),
            Err(e) => e,
        };
        assert!(matches!(err, CommitError::DuplicateWrite { .. }));
    }

    #[test]
    fn two_patches_never_fold() {
        let mut ops = DraftOps {
            patch: Some("diff --git a/a b/a\n".to_string()),
            ..Default::default()
        };
        let err = ops
            .append(DraftOps {
                patch: Some("diff --git a/b b/b\n".to_string()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(err, CommitError::MixedPatch));
    }

    #[test]
    fn patch_mixed_with_other_ops_fails_like_commit() {
        let mut ops = ops_write("a.txt", "one");
        ops.patch = Some("diff --git a/a b/a\n".to_string());
        let req = to_request(ops, &author()).unwrap();
        let err = match validate_request(&req) {
            Ok(_) => panic!("mixed patch validates"),
            Err(e) => e,
        };
        assert!(matches!(err, CommitError::MixedPatch));
    }
}
