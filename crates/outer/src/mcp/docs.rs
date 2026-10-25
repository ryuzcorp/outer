//! The `docs` topics: Markdown embedded from `web/docs/guides/*.md`.
//!
//! The guides carry Blume frontmatter (`title`/`description`) for the
//! website; the MCP tool serves them as plain Markdown with that block
//! stripped, so agents see exactly what the page says.

/// Topic name → Markdown, in `tools/list` order.
pub const TOPICS: &[(&str, &str)] = &[
    (
        "quickstart",
        include_str!("../../../../web/docs/guides/quickstart.md"),
    ),
    ("ilha", include_str!("../../../../web/docs/guides/ilha.md")),
    ("void", include_str!("../../../../web/docs/guides/void.md")),
    (
        "celld",
        include_str!("../../../../web/docs/guides/celld.md"),
    ),
    ("d1", include_str!("../../../../web/docs/guides/d1.md")),
    (
        "storage",
        include_str!("../../../../web/docs/guides/storage.md"),
    ),
    (
        "limits",
        include_str!("../../../../web/docs/guides/limits.md"),
    ),
    (
        "moderation",
        include_str!("../../../../web/docs/guides/moderation.md"),
    ),
    (
        "custom-domains",
        include_str!("../../../../web/docs/guides/custom-domains.md"),
    ),
];

/// Strip one leading `---` YAML frontmatter block, if present. Anything else
/// (including a body that starts with `---`) passes through untouched.
pub fn strip_frontmatter(md: &'static str) -> &'static str {
    let Some(body) = md
        .strip_prefix("---\n")
        .or_else(|| md.strip_prefix("---\r\n"))
    else {
        return md;
    };
    let (crlf, nl) = ("\r\n---\r\n", "\n---\n");
    if let Some(end) = body.find(nl) {
        return body[end + nl.len()..].trim_start_matches('\n');
    }
    if let Some(end) = body.find(crlf) {
        return body[end + crlf.len()..].trim_start_matches('\n');
    }
    md
}

/// The Markdown for `topic`, frontmatter stripped, or `None` when it names
/// no guide.
pub fn get(topic: &str) -> Option<&'static str> {
    TOPICS
        .iter()
        .find(|(name, _)| *name == topic)
        .map(|(_, md)| strip_frontmatter(md))
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
        assert_eq!(TOPICS.len(), 9);
        for (name, md) in TOPICS {
            let stripped = strip_frontmatter(md);
            assert!(stripped.trim().len() > 200, "{name} is suspiciously short");
            assert!(
                !stripped.starts_with("---\n"),
                "{name} still carries frontmatter"
            );
        }
    }

    #[test]
    fn frontmatter_strips_once() {
        let md: &'static str = "---\ntitle: T\ndescription: D\n---\n\n# T\n\nBody.\n";
        assert_eq!(strip_frontmatter(md), "# T\n\nBody.\n");
    }

    #[test]
    fn no_frontmatter_passes_through() {
        let md: &'static str = "# T\n\n---\n\nBody.\n";
        assert_eq!(strip_frontmatter(md), md);
    }

    #[test]
    fn unterminated_frontmatter_passes_through() {
        let md: &'static str = "---\ntitle: T\n";
        assert_eq!(strip_frontmatter(md), md);
    }
}
