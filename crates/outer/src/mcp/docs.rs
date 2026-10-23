//! The `docs` topics: Markdown embedded from `crates/outer/docs/*.md`.

/// Topic name → Markdown, in `tools/list` order.
pub const TOPICS: &[(&str, &str)] = &[
    ("quickstart", include_str!("../../docs/quickstart.md")),
    ("ilha", include_str!("../../docs/ilha.md")),
    ("void", include_str!("../../docs/void.md")),
    ("celld", include_str!("../../docs/celld.md")),
    ("d1", include_str!("../../docs/d1.md")),
    ("storage", include_str!("../../docs/storage.md")),
    ("limits", include_str!("../../docs/limits.md")),
    ("moderation", include_str!("../../docs/moderation.md")),
];

/// The Markdown for `topic`, or `None` when it names no guide.
pub fn get(topic: &str) -> Option<&'static str> {
    TOPICS
        .iter()
        .find(|(name, _)| *name == topic)
        .map(|(_, md)| *md)
}

/// Every topic name, for the `docs` description and error messages.
pub fn topic_list() -> Vec<&'static str> {
    TOPICS.iter().map(|(name, _)| *name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_topic_is_nonempty() {
        assert_eq!(TOPICS.len(), 8);
        for (name, md) in TOPICS {
            assert!(md.trim().len() > 200, "{name} is suspiciously short");
        }
    }
}
