//! Wordlist tiers matched as whole words with Aho–Corasick (spec §15.1).
//!
//! Lists live in `wordlists/profanity/<lang>/{strong,ambiguous,mild}.txt`
//! plus `allowlist.txt`, and are embedded with `include_str!` so the gate
//! needs no files at runtime. Patterns run through the same normalization
//! as text ([`normalize_pattern`]), so `flûte` matches `flute`.
//!
//! - `find_evasion`: Aho–Corasick over the compact view (no boundaries).
//!   A hit only counts when its base-text span covers a separator, so
//!   plain substrings of larger words never match here either.
//!
//! Both paths drop a hit when the containing word — the maximal non-space
//! run around the match with separators stripped — is allowlisted
//! (Scunthorpe: `cocktail`, `Scunthorpe`, `assassin`).

use std::collections::HashSet;
use std::sync::LazyLock;

use aho_corasick::AhoCorasick;

use super::normalize::{
    collapse_str_runs, fold_pattern, is_word_char, normalize_pattern, NormalizedText,
};

#[cfg(test)]
use super::normalize::normalize;
#[cfg(test)]
use std::collections::HashMap;

/// One matched term with its location in every view.
#[derive(Debug, Clone)]
pub struct WordHit {
    pub tier: Tier,
    pub pattern: String,
    /// Char offsets in the spaced view (`0..0` for evasion-only hits).
    pub spaced: std::ops::Range<usize>,
    /// Char offsets in the base text.
    pub base: std::ops::Range<usize>,
    /// Compact-view hit: located via `base`, needs span-feature checks.
    pub evasion: bool,
}
/// Which tier a pattern belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Strong,
    Ambiguous,
    Mild,
}

const LANGS: &[&str] = &["en", "pt", "es", "fr", "de", "it"];

macro_rules! tier_file {
    ($lang:literal, $tier:literal) => {
        include_str!(concat!(
            "../../wordlists/profanity/",
            $lang,
            "/",
            $tier,
            ".txt"
        ))
    };
}

static MATCHER: LazyLock<Matcher> = LazyLock::new(build_matcher);
static ALLOWLIST: LazyLock<HashSet<String>> = LazyLock::new(build_allowlist);

fn raw_tier(lang: &str, tier: &str) -> &'static str {
    match (lang, tier) {
        ("en", "strong") => tier_file!("en", "strong"),
        ("en", "ambiguous") => tier_file!("en", "ambiguous"),
        ("en", "mild") => tier_file!("en", "mild"),
        ("pt", "strong") => tier_file!("pt", "strong"),
        ("pt", "ambiguous") => tier_file!("pt", "ambiguous"),
        ("pt", "mild") => tier_file!("pt", "mild"),
        ("es", "strong") => tier_file!("es", "strong"),
        ("es", "ambiguous") => tier_file!("es", "ambiguous"),
        ("es", "mild") => tier_file!("es", "mild"),
        ("fr", "strong") => tier_file!("fr", "strong"),
        ("fr", "ambiguous") => tier_file!("fr", "ambiguous"),
        ("fr", "mild") => tier_file!("fr", "mild"),
        ("de", "strong") => tier_file!("de", "strong"),
        ("de", "ambiguous") => tier_file!("de", "ambiguous"),
        ("de", "mild") => tier_file!("de", "mild"),
        ("it", "strong") => tier_file!("it", "strong"),
        ("it", "ambiguous") => tier_file!("it", "ambiguous"),
        ("it", "mild") => tier_file!("it", "mild"),
        _ => "",
    }
}

struct Pattern {
    text: String,
    tier: Tier,
    /// Collapsed variants (`as` from `ass`) only match the compact view:
    /// in `spaced` they would hit innocent words.
    in_spaced: bool,
}

struct Matcher {
    ac: AhoCorasick,
    patterns: Vec<Pattern>,
}

fn build_matcher() -> Matcher {
    let mut patterns = Vec::new();
    for lang in LANGS {
        for (name, tier) in [
            ("strong", Tier::Strong),
            ("ambiguous", Tier::Ambiguous),
            ("mild", Tier::Mild),
        ] {
            for folded in parse_list(raw_tier(lang, name)) {
                let mut collapsed = folded.clone();
                collapse_str_runs(&mut collapsed);
                patterns.push(Pattern {
                    text: folded,
                    tier,
                    in_spaced: true,
                });
                if collapsed != patterns.last().expect("just pushed").text && !collapsed.is_empty()
                {
                    patterns.push(Pattern {
                        text: collapsed,
                        tier,
                        in_spaced: false,
                    });
                }
            }
        }
    }
    let texts: Vec<&str> = patterns.iter().map(|p| p.text.as_str()).collect();
    let ac = AhoCorasick::new(&texts).expect("wordlist automaton builds");
    Matcher { ac, patterns }
}

fn build_allowlist() -> HashSet<String> {
    include_str!("../../wordlists/profanity/allowlist.txt")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(normalize_pattern)
        .collect()
}

fn parse_list(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(fold_pattern)
        .filter(|p| !p.is_empty())
        .collect()
}

/// Pattern counts per language and tier, for the M1 report.
pub fn pattern_counts() -> Vec<(String, Tier, usize)> {
    let mut out = Vec::new();
    for lang in LANGS {
        for (name, tier) in [
            ("strong", Tier::Strong),
            ("ambiguous", Tier::Ambiguous),
            ("mild", Tier::Mild),
        ] {
            out.push((
                (*lang).to_string(),
                tier,
                parse_list(raw_tier(lang, name)).len(),
            ));
        }
    }
    out
}

/// Whole-word hits in the spaced view. The allowlist drops hits whose
/// containing word is innocent.
pub fn find_spaced(n: &NormalizedText) -> Vec<WordHit> {
    let m = &MATCHER;
    let chars: Vec<char> = n.spaced.chars().collect();
    let mut byte_to_char = vec![0usize; n.spaced.len() + 1];
    let mut ci = 0;
    for (bi, _) in n.spaced.char_indices() {
        byte_to_char[bi] = ci;
        ci += 1;
    }
    byte_to_char[n.spaced.len()] = ci;
    let mut hits = Vec::new();
    for mat in m.ac.find_iter(&n.spaced) {
        let pat = &m.patterns[mat.pattern()];
        if !pat.in_spaced {
            continue; // collapsed variants would hit innocent words here
        }
        let start = byte_to_char[mat.start()];
        let end = byte_to_char[mat.end()];
        let before = start.checked_sub(1).and_then(|i| chars.get(i));
        let after = chars.get(end);
        if before.is_some_and(|c| is_word_char(*c)) || after.is_some_and(|c| is_word_char(*c)) {
            continue;
        }
        let base_start = n.spaced_base[start];
        let base_end = n.spaced_base[end - 1] + 1;
        if allowlisted(n, base_start..base_end) {
            continue;
        }
        hits.push(WordHit {
            tier: pat.tier,
            pattern: pat.text.clone(),
            spaced: start..end,
            base: base_start..base_end,
            evasion: false,
        });
    }
    hits
}

/// Evasion hits in the compact view: only when the base span covers a
/// separator (something was stripped to form the match).
pub fn find_evasion(n: &NormalizedText) -> Vec<WordHit> {
    let m = &MATCHER;
    let mut byte_to_char = vec![0usize; n.compact.len() + 1];
    let mut ci = 0;
    for (bi, _) in n.compact.char_indices() {
        byte_to_char[bi] = ci;
        ci += 1;
    }
    byte_to_char[n.compact.len()] = ci;
    let mut hits = Vec::new();
    for mat in m.ac.find_iter(&n.compact) {
        let pat = &m.patterns[mat.pattern()];
        if pat.tier == Tier::Mild {
            continue; // mild never blocks; no evasion signal needed
        }
        let start = byte_to_char[mat.start()];
        let end = byte_to_char[mat.end()];
        let base_start = n.compact_base[start];
        let base_end = n.compact_base[end - 1] + 1;
        // A hit whose span covers neither a separator nor a repeat run
        // is a plain substring of a larger word (Scunthorpe), not an
        // evasion.
        if !has_evasion_feature(n, base_start..base_end) {
            continue;
        }
        if allowlisted(n, base_start..base_end) {
            continue;
        }
        hits.push(WordHit {
            tier: pat.tier,
            pattern: pat.text.clone(),
            spaced: 0..0, // evasion hits locate in `base`, not `spaced`
            base: base_start..base_end,
            evasion: true,
        });
    }
    hits
}

/// A separator was stripped, or a repeat run was collapsed, inside the
/// span: the match needed an evasion to form.
fn has_evasion_feature(n: &NormalizedText, span: std::ops::Range<usize>) -> bool {
    let mut prev: Option<char> = None;
    for (k, i) in (span.start..span.end).enumerate() {
        let Some(c) = n.base.get(i) else {
            continue;
        };
        if !c.is_alphanumeric() {
            return true;
        }
        if k > 0 && Some(*c) == prev {
            return true;
        }
        prev = Some(*c);
    }
    false
}

/// The containing word: the maximal non-space run around the base span
/// with non-alphanumerics stripped. Allowlisted → drop the hit.
fn allowlisted(n: &NormalizedText, span: std::ops::Range<usize>) -> bool {
    if n.base.is_empty() {
        return false;
    }
    let mut start = span.start.min(n.base.len());
    while start > 0 && n.base[start - 1] != ' ' {
        start -= 1;
    }
    let mut end = span.end.min(n.base.len());
    while end < n.base.len() && n.base[end] != ' ' {
        end += 1;
    }
    // Base keeps spaces only where the original had whitespace (separators
    // like `.`/`-` stay as-is), so this run is one word-ish token.
    let word: String = n.base[start..end]
        .iter()
        .filter(|c| c.is_alphanumeric())
        .collect();
    !word.is_empty() && ALLOWLIST.contains(&word)
}

/// Convenience: all blocking-relevant hits (strong + ambiguous) for text.
#[cfg(test)]
fn blocking_hits(text: &str) -> Vec<WordHit> {
    let n = normalize(text);
    find_spaced(&n)
        .into_iter()
        .chain(find_evasion(&n))
        .filter(|h| h.tier != Tier::Mild)
        .collect()
}

#[cfg(test)]
pub(crate) fn wordlist_sizes_for_tests() -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for (lang, tier, count) in pattern_counts() {
        out.insert(format!("{lang}/{}", tier_name(tier)), count);
    }
    out
}

#[cfg(test)]
fn tier_name(t: Tier) -> &'static str {
    match t {
        Tier::Strong => "strong",
        Tier::Ambiguous => "ambiguous",
        Tier::Mild => "mild",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_words_match_as_whole_words() {
        let hits = blocking_hits("this list is fuck awesome");
        assert!(hits.iter().any(|h| h.tier == Tier::Strong));
        let hits = blocking_hits("que porra é essa");
        assert!(hits.iter().any(|h| h.tier == Tier::Strong));
    }

    #[test]
    fn whole_word_matching_ignores_substrings() {
        assert!(blocking_hits("hello world").is_empty());
        assert!(blocking_hits("the shellfish shuffle").is_empty());
    }

    #[test]
    fn scunthorpe_words_pass() {
        for innocent in [
            "Scunthorpe",
            "a cocktail at dusk",
            "the cockpit drill",
            "an assassin creed",
        ] {
            assert!(
                blocking_hits(innocent).is_empty(),
                "{innocent:?} must not hit"
            );
        }
    }

    #[test]
    fn ambiguous_words_hit_for_fail_closed() {
        for text in ["the cock crowed at dawn", "Dick arrived", "that bitch ran"] {
            let hits = blocking_hits(text);
            assert!(
                hits.iter().any(|h| h.tier == Tier::Ambiguous),
                "{text:?} must be ambiguous"
            );
        }
    }

    #[test]
    fn mild_words_never_block() {
        let n = normalize("Damn, this is crap");
        let blocking: Vec<_> = find_spaced(&n)
            .into_iter()
            .chain(find_evasion(&n))
            .filter(|h| h.tier != Tier::Mild)
            .collect();
        assert!(blocking.is_empty());
        assert!(find_spaced(&n).iter().any(|h| h.tier == Tier::Mild));
    }

    #[test]
    fn separator_evasions_hit_but_plain_substrings_do_not() {
        for evil in ["f.u.c.k", "f-u-c-k", "s h i t", "p*o*r*n"] {
            assert!(!find_evasion(&normalize(evil)).is_empty(), "{evil:?}");
        }
        assert!(find_evasion(&normalize("cocktail hour")).is_empty());
    }

    #[test]
    fn lists_are_nonempty_in_every_language() {
        let sizes = wordlist_sizes_for_tests();
        for lang in LANGS {
            for tier in ["strong", "ambiguous", "mild"] {
                let n = sizes[&format!("{lang}/{tier}")];
                assert!(n >= 3, "{lang}/{tier} has {n} patterns");
            }
        }
    }
}
