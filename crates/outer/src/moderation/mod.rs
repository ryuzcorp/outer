//! The pre-live gate (spec §15.1, step 6 of the pipeline).
//!
//! Source strings, content files, prerendered HTML and app metadata are
//! extracted ([`extract`]) and classified by TypeSafe Jev through Cloudflare
//! AI Gateway ([`jev`]); the per-chunk scores are decided ([`gate`]).
//! There is no handwritten profanity machinery: no normalization, no
//! wordlists, no masking. Jev is the only classifier, and when it is
//! unreachable the deploy fails closed (`review_unavailable`).

pub mod extract;
pub mod gate;
pub mod jev;

pub use extract::{Chunk, TooLarge, MAX_FILES, MAX_FILE_BYTES, MAX_TEXT_BYTES};
pub use gate::{
    cache_key, judge, run_gate, run_skipped, run_too_large, BlockReason, ChunkVerdict, Decision,
    Finding, GateError, GateOutcome, GateStats,
};
pub use jev::{ChunkScores, JevClient, JevConfig, JevError, JEV_MODEL};

/// The agent-facing line for a blocked deploy, per spec §15.1 for profanity
/// and analogous for the other block categories.
pub fn user_message(first: &Finding) -> String {
    match first.category {
        "sexual_minors" | "phishing" | "severity" => format!(
            "Outer didn't publish this change because the page text was flagged as {} ({}, line {}). Your app still shows the previous version.",
            category_label(first.category),
            first.path,
            first.line
        ),
        _ => format!(
            "Outer didn't publish this change because the page text has strong profanity ({}, line {}). Your app still shows the previous version.",
            first.path, first.line
        ),
    }
}

fn category_label(category: &str) -> &'static str {
    match category {
        "sexual_minors" => "sexual content involving minors",
        "phishing" => "phishing or credential harvesting",
        "severity" => "severely harmful content",
        "hate" => "hate or harassment",
        _ => "strong profanity",
    }
}

/// The agent-facing line when Jev is unreachable: fail closed, keep serving
/// the previous version, try again shortly.
pub fn unavailable_message() -> String {
    "Outer didn't publish this change because the content check is temporarily unavailable. Your app still shows the previous version — try again shortly."
        .to_string()
}

/// The agent-facing line for an over-caps deploy.
pub fn too_large_message(too_large: &TooLarge) -> String {
    format!(
        "Outer didn't publish this change because it carries more text than the review gate can check ({} files, {} bytes). Split the change or drop generated files. Your app still shows the previous version.",
        too_large.files, too_large.bytes
    )
}
