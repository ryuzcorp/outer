// Derived from noite apps/runner/src/lifecycle.rs @ 58a977a (Apache-2.0).
// Outer change: a thiserror SlugError with fix-it messages, and the spec §7.1
// reserved list instead of Noite's edge-route list.

/// Brand and account-page fragments refused in slugs (spec §7.1). One entry
/// per line, `#` comments; embedded so the check works with no data dir.
const BRANDS_TXT: &str = include_str!("../../wordlists/brands.txt");

/// The parsed wordlist: lowercase entries, comments and blanks dropped.
/// Entries are matched by [`brand_hit`], never by callers directly.
static BRANDS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    BRANDS_TXT
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
});

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
    /// Imitates a well-known brand or an account page (spec §7.1).
    #[error("slug {slug:?} looks like it imitates {entry:?}, and names imitating well-known brands or account pages aren't allowed; choose another slug")]
    Brand { slug: String, entry: &'static str },
}

/// The brand-list entry `slug` trips, if any.
///
/// An entry hits when it is a whole hyphen-separated token (`paypal-login`
/// trips `paypal`, `secure-bank-verify` trips `bank`) or — for entries of 5+
/// characters — when it appears anywhere in the slug (`appleid-check` trips
/// `apple`). Short entries stay token-only: `aws`, `gov`, `irs` and `ups`
/// are substrings of too many ordinary words (`paws`, `governor`, `cups`),
/// so `awsome-paws` and `governor` stay claimable while `gov-verify` and
/// `dhl-express` refuse on their whole tokens.
///
/// The substring half deliberately over-refuses: any slug containing a 5+
/// entry refuses, so `appleton`, `snapple`, `pineapple` and `applet` all
/// refuse on `apple`. Phishing slugs concatenate (`appleid`,
/// `paypalverify`) far more often than legitimate apps embed a brand name,
/// so the rule errs on refusing and the agent picks another slug.
pub fn brand_hit(slug: &str) -> Option<&'static str> {
    let slug = slug.to_lowercase();
    BRANDS.iter().copied().find(|entry| {
        slug.split('-').any(|token| token == *entry) || (entry.len() >= 5 && slug.contains(entry))
    })
}

/// Noite's slug rule plus the spec §7.1 reserved list and brand filter.
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
    // Preview hosts live at `<slug>--preview`: no app may claim that shape,
    // or its live URL and its preview URL would collide on one hostname.
    if slug.ends_with(crate::preview::PREVIEW_HOST_SUFFIX) {
        return Err(SlugError::Reserved(slug.to_string()));
    }
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Static invariant: a compile-time literal, so construction never fails.
        regex::Regex::new(r"^[a-z0-9]([a-z0-9\-]{0,46}[a-z0-9])?$").expect("slug re")
    });
    if !RE.is_match(slug) {
        return Err(SlugError::BadShape(slug.to_string()));
    }
    if let Some(entry) = brand_hit(slug) {
        return Err(SlugError::Brand {
            slug: slug.to_string(),
            entry,
        });
    }
    Ok(())
}

/// Why the Jev name check refused a slug (or app name). The flagged message
/// names the offending text so the agent can pick another name; the
/// unavailable message is retryable, mirroring the gate's fail-closed line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfanityError {
    /// Jev scored the text at or past a gate block threshold.
    #[error("the name {0:?} was flagged by the content check; choose another name")]
    Flagged(String),
    /// Jev was unreachable: fail closed, exactly like the gate.
    #[error("Couldn't check this name right now; try again in a minute")]
    Unavailable,
}

/// Classify the slug (and the app `name` and `description` when given) with
/// the same Jev client and the same block decision as the pre-live gate
/// (spec §15.1): hyphens become spaces, and any chunk the gate would block
/// on ([`crate::moderation::gate::judge`] — profanity, minors, phishing,
/// severe) refuses the name. Hate only queues in the gate and never blocks,
/// so it passes here too.
///
/// `None` is localhost dev without the Cloudflare pair: log a warning and
/// pass, exactly like the gate's [`crate::moderation::gate::run_skipped`].
/// Call only after [`validate_slug`] passes, so Jev never pays for a name
/// the cheap checks already refused.
pub async fn check_slug_profanity(
    jev: Option<&crate::moderation::JevClient>,
    slug: &str,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<(), ProfanityError> {
    let Some(jev) = jev else {
        tracing::warn!(
            slug = %slug,
            "slug check: no Jev client (localhost dev without the Cloudflare pair); passing unchecked"
        );
        return Ok(());
    };
    let mut texts = vec![slug.replace('-', " ")];
    for extra in [name, description] {
        if let Some(text) = extra.filter(|text| !text.is_empty()) {
            texts.push(text.to_string());
        }
    }
    let scores = jev
        .classify(&texts)
        .await
        .map_err(|_| ProfanityError::Unavailable)?;
    for (text, score) in texts.iter().zip(scores) {
        if matches!(
            crate::moderation::gate::judge(&score),
            crate::moderation::gate::ChunkVerdict::BlockProfanity
                | crate::moderation::gate::ChunkVerdict::BlockHarmful
        ) {
            return Err(ProfanityError::Flagged(text.clone()));
        }
    }
    Ok(())
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
    fn brand_tokens_and_long_substrings_are_refused() {
        // Whole-token hits (any entry length) and substring hits (5+
        // entries): the phishing shapes `paypal-login`, `secure-bank-verify`
        // and `appleid-check` all refuse.
        for (slug, entry) in [
            ("paypal-login", "paypal"),
            ("secure-bank-verify", "bank"),
            ("appleid-check", "apple"),
            ("bankofamerica-login", "bankofamerica"),
            ("dhl-express", "dhl"),
            ("gov-verify", "verify"),
            ("mypaypal", "paypal"),
            ("paypalverify", "paypal"),
            ("appleton", "apple"),
            // Documented over-refusals: ordinary words embedding a 5+
            // entry. The rule errs on refusing; the agent picks another slug.
            ("snapple", "apple"),
            ("pineapple-express", "apple"),
            ("applet", "apple"),
        ] {
            assert_eq!(brand_hit(slug), Some(entry), "brand hit for {slug:?}");
            assert_eq!(
                validate_slug(slug),
                Err(SlugError::Brand {
                    slug: slug.to_string(),
                    entry
                }),
                "brand slug {slug:?} must be refused"
            );
        }
        // Short entries are token-only: their substrings stay claimable.
        for slug in [
            "governor",
            "governor-app",
            "awsome-paws",
            "paws",
            "cups",
            "supper-club",
            "grocery-list",
            "my-shop",
        ] {
            assert_eq!(brand_hit(slug), None, "no brand hit for {slug:?}");
            assert!(validate_slug(slug).is_ok(), "should accept {slug:?}");
        }
    }

    #[test]
    fn brand_refusal_names_the_entry() {
        let err = validate_slug("paypal-login").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("paypal") && message.contains("aren't allowed"),
            "refusal must name the brand and the rule: {message}"
        );
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
