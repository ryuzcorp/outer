// Derived from noite apps/runner/src/lifecycle.rs @ 58a977a (Apache-2.0).
// Outer change: a thiserror SlugError with fix-it messages, and the spec §7.1
// reserved list instead of Noite's edge-route list.

/// Slugs that would collide with platform routes or invite abuse (spec §7.1).
/// Checked before the shape rule so the error names the real problem.
const RESERVED_SLUGS: &[&str] = &[
    "www", "mcp", "api", "app", "git", "admin", "auth", "oauth", "login", "billing", "docs",
    "help", "status", "mail", "static", "cdn", "outer", "abuse", "ilha",
];

/// Why a slug was refused. Every message tells the calling agent what to change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SlugError {
    /// Nothing to check: the caller passed an empty slug.
    #[error("slug is empty; pass a lowercase name like \"grocery-list\"")]
    Empty,
    /// Over the 48-character cap Noite's rule bakes in.
    #[error("slug {slug:?} is {len} characters; keep slugs to 48 characters or fewer")]
    TooLong { slug: String, len: usize },
    /// Fails `^[a-z0-9]([a-z0-9-]{0,46}[a-z0-9])?$`.
    #[error("slug {0:?} must match ^[a-z0-9]([a-z0-9-]{{0,46}}[a-z0-9])?$; use lowercase letters, digits, and hyphens that are neither first nor last")]
    BadShape(String),
    /// On the spec §7.1 reserved list.
    #[error("slug {0:?} is reserved for the platform; choose another slug")]
    Reserved(String),
}

/// Noite's slug rule plus the spec §7.1 reserved list.
pub fn validate_slug(slug: &str) -> Result<(), SlugError> {
    if slug.is_empty() {
        return Err(SlugError::Empty);
    }
    let chars = slug.chars().count();
    if chars > 48 {
        return Err(SlugError::TooLong {
            slug: slug.to_string(),
            len: chars,
        });
    }
    if RESERVED_SLUGS.contains(&slug) {
        return Err(SlugError::Reserved(slug.to_string()));
    }
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Static invariant: a compile-time literal, so construction never fails.
        regex::Regex::new(r"^[a-z0-9]([a-z0-9\-]{0,46}[a-z0-9])?$").expect("slug re")
    });
    if RE.is_match(slug) {
        Ok(())
    } else {
        Err(SlugError::BadShape(slug.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_slugs_pass() {
        let max = "a".repeat(48);
        for slug in [
            "a",
            "0",
            "ab",
            "grocery",
            "grocery-list",
            "a1-2b",
            max.as_str(),
        ] {
            assert!(validate_slug(slug).is_ok(), "should accept {slug:?}");
        }
    }

    #[test]
    fn shape_violations_name_the_rule() {
        for (slug, variant) in [
            ("", SlugError::Empty),
            (
                &"a".repeat(49),
                SlugError::TooLong {
                    slug: "a".repeat(49),
                    len: 49,
                },
            ),
            ("Grocery", SlugError::BadShape("Grocery".to_string())),
            ("-lead", SlugError::BadShape("-lead".to_string())),
            ("trail-", SlugError::BadShape("trail-".to_string())),
            (
                "under_score",
                SlugError::BadShape("under_score".to_string()),
            ),
            (
                "two  spaces",
                SlugError::BadShape("two  spaces".to_string()),
            ),
            ("café", SlugError::BadShape("café".to_string())),
        ] {
            assert_eq!(
                validate_slug(slug),
                Err(variant),
                "wrong error for {slug:?}"
            );
        }
    }

    #[test]
    fn every_reserved_slug_is_refused() {
        for slug in [
            "www", "mcp", "api", "app", "git", "admin", "auth", "oauth", "login", "billing",
            "docs", "help", "status", "mail", "static", "cdn", "outer", "abuse", "ilha",
        ] {
            assert_eq!(
                validate_slug(slug),
                Err(SlugError::Reserved(slug.to_string())),
                "reserved slug {slug:?} must be refused"
            );
        }
        // Near-misses are fine: only exact matches are reserved.
        for slug in ["outer2", "my-outer", "apiary", "www2"] {
            assert!(validate_slug(slug).is_ok(), "should accept {slug:?}");
        }
    }
    #[test]
    fn length_counts_characters_not_bytes() {
        // 25 CJK characters are 75 bytes: short enough to keep, but the
        // shape rule (ASCII only) still refuses them — as BadShape, not
        // TooLong with a byte count.
        let short = "あ".repeat(25);
        assert_eq!(
            validate_slug(&short),
            Err(SlugError::BadShape(short.clone())),
            "25 characters must not be TooLong"
        );
        // 49 CJK characters are TooLong, and the error names characters.
        let long = "あ".repeat(49);
        assert_eq!(
            validate_slug(&long),
            Err(SlugError::TooLong {
                slug: long.clone(),
                len: 49,
            }),
        );
    }
}
