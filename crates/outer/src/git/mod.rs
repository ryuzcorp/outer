//! One app is one repo with one ref (`refs/heads/main`), changed only by Outer.

pub mod commit;
pub mod local;
pub mod manifest;
pub use commit::{Author, CommitError, CommitRequest, Edit, FileWrite, Move, WriteBody};
pub use manifest::{GitConfig, GitSync, Manifest, ManifestRef, MAIN_REF};
