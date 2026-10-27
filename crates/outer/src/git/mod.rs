//! One app is one repo with one live ref (`refs/heads/main`) plus one trial
//! ref (`refs/heads/preview`, Pro preview deploys), changed only by Outer.

pub mod commit;
pub mod drafts;
pub mod export;
pub mod format;
pub mod local;
pub mod manifest;
pub mod symbol;
pub use commit::{
    Author, CommitChanges, CommitError, CommitOutcome, CommitRequest, Edit, FileWrite, Insert,
    LineEdit, Move, WriteBody,
};
pub use drafts::{Draft, DraftEdit, DraftMove, DraftOps, DraftWrite};
pub use manifest::{GitConfig, GitSync, Manifest, ManifestRef, MAIN_REF, PREVIEW_REF};
