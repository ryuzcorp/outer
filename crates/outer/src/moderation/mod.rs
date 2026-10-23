//! The pre-live profanity gate (spec §15.1, step 6 of the pipeline).
//!
//! Source strings, content files, prerendered HTML and app metadata are
//! extracted ([`extract`]), normalized ([`normalize`]), matched against
//! embedded wordlists ([`wordlist`]), and decided ([`gate`]) while Jev is
//! still unavailable: `strong` hits block, `ambiguous` hits block too
//! (fail closed), other text passes. The decision function already takes
//! an `Option<JevScores>` so a Jev client plugs in later without
//! restructuring.

pub mod extract;
pub mod gate;
pub mod normalize;
pub mod wordlist;

pub use extract::{Chunk, TooLarge, MAX_FILES, MAX_FILE_BYTES, MAX_TEXT_BYTES};
pub use gate::{run_gate, BlockReason, Decision, Finding, GateOutcome, GateStats, JevScores};
pub use normalize::{mask_excerpt, normalize, NormalizedText};
pub use wordlist::{Tier, WordHit};

/// The agent-facing line for a blocked deploy, per spec §15.1.
pub fn user_message(first: &Finding) -> String {
    format!(
        "Outer didn't publish this change because the page text has strong profanity ({}, line {}). Your app still shows the previous version.",
        first.path, first.line
    )
}

/// The agent-facing line for an over-caps deploy.
pub fn too_large_message(too_large: &TooLarge) -> String {
    format!(
        "Outer didn't publish this change because it carries more text than the review gate can check ({} files, {} bytes). Split the change or drop generated files. Your app still shows the previous version.",
        too_large.files, too_large.bytes
    )
}
