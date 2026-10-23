//! Text normalization for the pre-live profanity gate (spec §15.1).
//!
//! Every chunk goes through the same pipeline: NFKC, lowercase, diacritic
//! stripping, zero-width removal, separator-tolerant splitting, leetspeak
//! folding, and run collapsing. Two views come out:
//!
//! - `spaced`: separator characters become single spaces. Whole-word
//!   matching runs here.
//! - `compact`: separator characters are removed entirely. Substring
//!   matching runs here to catch split-letter evasions (`f.u.c.k`); a
//!   compact hit only counts when the matched span covers a separator in
//!   the base text, so plain substrings of larger words (Scunthorpe)
//!   never match through this path.
//!
//! Both views keep a per-character map back to the base text, and the base
//! keeps one back to the original excerpt, so findings can mask the exact
//! matched word in the author's own text.

use unicode_normalization::UnicodeNormalization;

/// Normalized views of one chunk plus the maps back to the original.
#[derive(Debug, Clone)]
pub struct NormalizedText {
    /// Separator-folded text: letters/digits kept, everything else a space.
    pub spaced: String,
    /// Separators removed entirely: catches `f.u.c.k`, `f-u-c-k`, `f u c k`.
    pub compact: String,
    /// `spaced` char offset → `base` char offset.
    pub spaced_base: Vec<usize>,
    /// `compact` char offset → `base` char offset.
    pub compact_base: Vec<usize>,
    /// Post-fold characters (NFKC, lowercase, no diacritics, leet folded,
    /// runs collapsed). Non-alphanumeric entries are the separators.
    pub base: Vec<char>,
    /// `base` char offset → original excerpt char offset.
    pub base_orig: Vec<usize>,
}

/// Normalize `text` into [`NormalizedText`].
pub fn normalize(text: &str) -> NormalizedText {
    let (mut base, mut base_orig) = (Vec::new(), Vec::new());
    for (orig_idx, c) in text.chars().enumerate() {
        // NFKC per character: fullwidth/halfwidth and compatibility forms
        // fold (evasion: "ｆｕｃｋ"). Cross-boundary composition is lost,
        // which is fine: combining marks are stripped below anyway.
        for norm in c.to_string().nfkc() {
            for lower in norm.to_lowercase() {
                // NFD then drop combining marks: "fück" → "fuck".
                for part in lower.to_string().nfd() {
                    if is_combining_mark(part) || is_zero_width(part) {
                        continue;
                    }
                    // `!`/`+`/`|` stay raw here; `resolve_contextual_leet`
                    // below folds them only between letters/digits.
                    base.push(if is_contextual_leet(part) {
                        part
                    } else {
                        fold_leet(part)
                    });
                    base_orig.push(orig_idx);
                }
            }
        }
    }
    resolve_contextual_leet(&mut base);
    let mut spaced = String::new();
    let mut spaced_base = Vec::new();
    let mut compact = String::new();
    let mut compact_base = Vec::new();
    let mut last_space = true; // collapse separator runs in `spaced`
    let mut prev_compact: Option<char> = None; // collapse runs in `compact`
    for (i, c) in base.iter().enumerate() {
        if c.is_alphanumeric() {
            spaced.push(*c);
            spaced_base.push(i);
            // Runs collapse to one in `compact`, so repeat-letter evasions
            // (`fuuuck`) fold onto the same collapsed patterns.
            if Some(*c) != prev_compact {
                compact.push(*c);
                compact_base.push(i);
                prev_compact = Some(*c);
            }
            last_space = false;
        } else if !last_space {
            spaced.push(' ');
            spaced_base.push(i);
            last_space = true;
            prev_compact = None;
        }
    }
    NormalizedText {
        spaced: spaced.trim_end().to_string(),
        compact,
        spaced_base,
        compact_base,
        base,
        base_orig,
    }
}

/// Normalize a wordlist pattern through the same folds as text, then
/// collapse runs: the compact-view form (`fuck` → `fuk`).
pub(crate) fn normalize_pattern(raw: &str) -> String {
    let mut out = fold_pattern(raw);
    collapse_str_runs(&mut out);
    out
}

/// The same folds without run collapsing: the spaced-view form, where
/// `ass` must stay `ass` (never the innocent word `as`).
pub(crate) fn fold_pattern(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        for norm in c.to_string().nfkc() {
            for lower in norm.to_lowercase() {
                for part in lower.to_string().nfd() {
                    if is_combining_mark(part) || is_zero_width(part) {
                        continue;
                    }
                    out.push(fold_leet(part));
                }
            }
        }
    }
    out
}

/// A word character for boundary checks: letters and digits. `_` never
/// appears in `spaced` (it folds to a space), which also catches
/// `fuck_you`-style joins.
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Mask `orig_range` (original-excerpt char offsets) inside `original`:
/// the matched word keeps its first letter, the rest becomes `*`.
pub fn mask_excerpt(original: &str, orig_range: std::ops::Range<usize>) -> String {
    let chars: Vec<char> = original.chars().collect();
    let mut out = String::with_capacity(original.len());
    for (i, c) in chars.iter().enumerate() {
        if i >= orig_range.start && i < orig_range.end {
            out.push(if i == orig_range.start { *c } else { '*' });
        } else {
            out.push(*c);
        }
    }
    out
}

fn is_combining_mark(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036f}'
        | '\u{1ab0}'..='\u{1aff}'
        | '\u{1dc0}'..='\u{1dff}'
        | '\u{20d0}'..='\u{20ff}'
        | '\u{fe20}'..='\u{fe2f}')
}

fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}')
}

/// The 1337 subset worth folding. `@→a 0→o 1→i 3→e $→s` are the spec's;
/// the rest (`4→a 5→s 7→t 6→g 8→b 9→g 2→z`) are common extensions.
/// `!`, `+` and `|` stay contextual ([`resolve_contextual_leet`]).
fn fold_leet(c: char) -> char {
    match c {
        '@' | '4' => 'a',
        '0' => 'o',
        '1' => 'i',
        '3' => 'e',
        '$' | '5' => 's',
        '7' => 't',
        '6' | '9' => 'g',
        '8' => 'b',
        '2' => 'z',
        other => other,
    }
}

/// Leet that doubles as punctuation (`!`, `+`, `|`). Folded only when both
/// neighbors are alphanumeric (checked post-fold, so neighbors are final):
/// `sh!t` → `shit`, but `wow!` separates. Unsurrounded ones become spaces,
/// which keeps every map aligned.
fn is_contextual_leet(c: char) -> bool {
    matches!(c, '!' | '+' | '|')
}

fn resolve_contextual_leet(base: &mut [char]) {
    for i in 0..base.len() {
        if !is_contextual_leet(base[i]) {
            continue;
        }
        let surrounded = i
            .checked_sub(1)
            .and_then(|j| base.get(j))
            .is_some_and(|c| c.is_alphanumeric() && !is_contextual_leet(*c))
            && base
                .get(i + 1)
                .is_some_and(|c| c.is_alphanumeric() && !is_contextual_leet(*c));
        base[i] = if surrounded {
            match base[i] {
                '!' => 'i',
                '+' => 't',
                _ => 'l', // '|'
            }
        } else {
            ' '
        };
    }
}

pub(crate) fn collapse_str_runs(s: &mut String) {
    let mut out = String::with_capacity(s.len());
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if Some(c) != prev {
            out.push(c);
        }
        prev = Some(c);
    }
    *s = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_width_case_diacritics_and_zero_width() {
        let n = normalize("Ｆück\u{200b}!");
        assert_eq!(n.spaced, "fuck", "fullwidth + case + umlaut + ZW: {n:?}");
        let n = normalize("Café");
        assert_eq!(n.spaced, "cafe");
    }

    #[test]
    fn separators_split_words_but_compact_joins() {
        let n = normalize("f.u.c.k");
        assert_eq!(n.spaced, "f u c k");
        assert_eq!(n.compact, "fuck");
        let n = normalize("f-u-c-k");
        assert_eq!(n.compact, "fuck");
        let n = normalize("f u c k");
        assert_eq!(n.compact, "fuck");
    }

    #[test]
    fn folds_common_leetspeak() {
        assert_eq!(normalize("sh1t").spaced, "shit");
        assert_eq!(normalize("@ss").spaced, "ass");
        assert_eq!(normalize("b00bs").spaced, "boobs");
        assert_eq!(normalize("p0rn").spaced, "porn");
    }

    #[test]
    fn collapses_repeated_letters() {
        assert_eq!(normalize("fuuuck").compact, "fuck");
        assert_eq!(normalize("fuuuck").spaced, "fuuuck");
        assert_eq!(normalize_pattern("ass"), "as");
        assert_eq!(normalize_pattern("fuck"), "fuck");
        assert_eq!(fold_pattern("ass"), "ass");
    }

    #[test]
    fn punct_leets_fold_only_between_letters() {
        assert_eq!(normalize("sh!t").spaced, "shit");
        assert_eq!(normalize("wow!").spaced, "wow");
        assert_eq!(normalize("C++").spaced, "c");
    }

    #[test]
    fn maps_hit_spans_back_to_the_original_excerpt() {
        let original = "this f.u.c.k list";
        let n = normalize(original);
        let hit = n.compact.find("fuck").unwrap();
        let base_start = n.compact_base[hit];
        let base_end = n.compact_base[hit + "fuck".len() - 1] + 1;
        let orig: Vec<usize> = (base_start..base_end).map(|i| n.base_orig[i]).collect();
        let masked = mask_excerpt(original, orig[0]..orig[orig.len() - 1] + 1);
        assert_eq!(masked, "this f****** list", "first letter kept: {masked}");
    }

    #[test]
    fn masks_plain_word_keeping_first_letter() {
        assert_eq!(mask_excerpt("oh fuck yeah", 3..7), "oh f*** yeah");
    }
}
