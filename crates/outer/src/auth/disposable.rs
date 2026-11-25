//! Disposable-email blocklist for sign-up abuse control (spec §14.3).
//!
//! Source: https://github.com/disposable-email-domains/disposable-email-domains
//! (`disposable_email_blocklist.conf`), dedicated to the public domain under
//! CC0 1.0 (https://creativecommons.org/publicdomain/zero/1.0/). No
//! attribution required; vendored here for provenance. Upstream commit
//! `9cf2bee345b0276e1b65cee7484beb51e5c715b1` (2026-11-21). Refresh by
//! replacing `../../wordlists/disposable_email_blocklist.conf` with a newer
//! copy of that file and updating this header.
//!
//! The list is compiled into the binary (`include_str!`), so the check works
//! with no data dir and no network. Matching is exact-domain plus any
//! subdomain of a listed domain (`sub.listed.example` hits `listed.example`).

use std::collections::HashSet;
use std::sync::LazyLock;

/// The vendored blocklist: one lowercase domain per line, no comments.
const BLOCKLIST_TXT: &str = include_str!("../../wordlists/disposable_email_blocklist.conf");

/// Parsed blocklist, built once. `&'static str` borrows `BLOCKLIST_TXT`, so
/// the set costs one allocation per domain and no copy of the text.
static PARSED: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    BLOCKLIST_TXT
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
});

/// True when `domain` (case-insensitive) is listed or sits under a listed
/// domain. Callers pass the domain part of an already-normalized email.
pub fn is_disposable_domain(domain: &str) -> bool {
    // A single trailing dot is the FQDN root, not a different domain:
    // without this `u@0-mail.com.` would dodge the blocklist.
    let domain = domain.strip_suffix('.').unwrap_or(domain);
    let mut suffix = domain.trim().to_ascii_lowercase();
    loop {
        if PARSED.contains(suffix.as_str()) {
            return true;
        }
        let Some(dot) = suffix.find('.') else {
            return false;
        };
        suffix = suffix[dot + 1..].to_string();
    }
}

/// True when the address's domain is blocklisted (or under one). Malformed
/// input without a domain reads as not disposable; `valid_email` rejects it
/// separately.
pub fn is_disposable_email(email: &str) -> bool {
    let Some((_, domain)) = email.rsplit_once('@') else {
        return false;
    };
    !domain.is_empty() && is_disposable_domain(domain)
}

/// The canonical form abuse limits key on: lowercase with the `+tag` cut
/// from the local part, so `a+1@x` and `a+2@x` share one OTP bucket. Gmail
/// (and legacy googlemail) additionally drops dots and maps googlemail to
/// gmail, matching Gmail's own equivalence; other providers keep dots
/// because only some providers ignore them.
///
/// Account identity is deliberately NOT this form: accounts stay keyed on
/// the exact lowercased address, so folding never merges existing accounts
/// (a `user+tag@gmail` owner keeps their own row, history and billing).
pub fn canonical_email_for_limits(email: &str) -> String {
    let email = email.trim().to_ascii_lowercase();
    let Some((local, domain)) = email.split_once('@') else {
        return email;
    };
    // The FQDN root dot again: `F.O.O+b@gmail.com.` must fold into the
    // same OTP bucket as `foo@gmail.com`. `normalize_email` strips it too,
    // so every layer agrees.
    let domain = domain.strip_suffix('.').unwrap_or(domain);
    let local = local.split('+').next().unwrap_or(local);
    if domain == "gmail.com" || domain == "googlemail.com" {
        let local: String = local.chars().filter(|c| *c != '.').collect();
        return format!("{local}@gmail.com");
    }
    if local == email {
        email
    } else {
        format!("{local}@{domain}")
    }
}

/// Refuse a new sign-up on a blocklisted domain. Existing accounts never
/// reach this (the caller checks only unknown emails), and reviewer password
/// logins never reach it either.
pub fn check_new_email(email: &str) -> Result<(), super::AuthError> {
    if is_disposable_email(email) {
        return Err(super::AuthError {
            code: "disposable_email",
            message: format!("{email:?} is on a disposable email domain"),
            user_message: Some(
                "That looks like a throwaway email address. Sign up with a permanent address instead."
                    .to_string(),
            ),
            retry_after_s: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocklist_hits_listed_domains_and_subdomains() {
        assert!(is_disposable_domain("0-mail.com"));
        assert!(is_disposable_domain("sub.0-mail.com"));
        assert!(is_disposable_domain("deep.sub.0-mail.com"));
        assert!(!is_disposable_domain("example.com"));
        assert!(!is_disposable_domain("0-mail.com.example.com"));
        assert!(!is_disposable_domain(""));
    }

    #[test]
    fn limits_fold_plus_tags_and_gmail_dots() {
        assert_eq!(
            canonical_email_for_limits("A+tag1@Example.com"),
            "a@example.com"
        );
        assert_eq!(
            canonical_email_for_limits("a+b@x.com"),
            canonical_email_for_limits("a+c@x.com")
        );
        assert_ne!(
            canonical_email_for_limits("a+b@x.com"),
            canonical_email_for_limits("a+b@y.com")
        );
        // Gmail equivalence; other providers keep their dots.
        assert_eq!(
            canonical_email_for_limits("F.O.O+b@gmail.com"),
            canonical_email_for_limits("foo@googlemail.com")
        );
        assert_eq!(
            canonical_email_for_limits("f.o.o@outlook.com"),
            "f.o.o@outlook.com"
        );
        // Identity keeps the full address: only limits fold.
        assert_eq!(canonical_email_for_limits("plain@x.com"), "plain@x.com");
    }

    #[test]
    fn trailing_dot_dodges_nothing() {
        // The stored identity sees the stripped form too.
        assert_eq!(
            super::super::normalize_email("U@0-Mail.COM."),
            "u@0-mail.com"
        );
        assert!(is_disposable_domain("0-mail.com."));
        assert!(is_disposable_email("u@0-mail.com."));
        // ... nor split the Gmail OTP bucket.
        assert_eq!(
            canonical_email_for_limits("F.O.O+b@gmail.com."),
            canonical_email_for_limits("foo@gmail.com")
        );
        assert_eq!(
            canonical_email_for_limits("a+b@x.com."),
            canonical_email_for_limits("a+c@x.com")
        );
    }
}
