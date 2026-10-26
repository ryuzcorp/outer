//! `brand.json`: the per-app brand Outer writes at creation (BRANDING.md
//! Contract 1). Templates render these values as text; agents rebrand an app
//! by editing `brand.json` like any other file (`commit`).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Display names are 1–60 chars, no control chars (BRANDING.md Contract 1).
pub const MAX_NAME_CHARS: usize = 60;
/// Descriptions are 1–160 chars, no control chars.
pub const MAX_DESCRIPTION_CHARS: usize = 160;
/// Home-screen labels break past this many chars.
pub const SHORT_NAME_CHARS: usize = 12;

/// One app's brand, as written to the repo root's `brand.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brand {
    /// Display name, 1–60 chars.
    pub name: String,
    /// Home-screen label, ≤ 12 chars.
    #[serde(rename = "shortName")]
    pub short_name: String,
    /// One-line pitch, 1–160 chars.
    pub description: String,
    /// Deterministic `#rrggbb` from the slug.
    pub accent: String,
}

/// Why a `name` or `description` argument was refused. Every message tells
/// the calling agent what to change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrandError {
    /// An explicit empty name: leave it out for the slug default instead.
    #[error("name is empty; pass a display name or leave it out for the slug default")]
    EmptyName,
    /// Over the 60-char cap.
    #[error("name is {len} characters; keep names to 60 characters or fewer")]
    NameTooLong { len: usize },
    /// Control characters never render safely.
    #[error("name must not contain control characters")]
    NameControlChars,
    /// An explicit empty description: leave it out to reuse the name.
    #[error("description is empty; pass a description or leave it out to reuse the name")]
    EmptyDescription,
    /// Over the 160-char cap.
    #[error("description is {len} characters; keep descriptions to 160 characters or fewer")]
    DescriptionTooLong { len: usize },
    /// Control characters never render safely.
    #[error("description must not contain control characters")]
    DescriptionControlChars,
}

/// Check an explicit `create_app` `name`: 1–60 chars, no control chars.
pub fn validate_name(name: &str) -> Result<(), BrandError> {
    if name.is_empty() {
        return Err(BrandError::EmptyName);
    }
    let len = name.chars().count();
    if len > MAX_NAME_CHARS {
        return Err(BrandError::NameTooLong { len });
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(BrandError::NameControlChars);
    }
    Ok(())
}

/// Check an explicit `create_app` `description`: 1–160 chars, no control
/// chars. Same refusal shape as [`validate_name`].
pub fn validate_description(description: &str) -> Result<(), BrandError> {
    if description.is_empty() {
        return Err(BrandError::EmptyDescription);
    }
    let len = description.chars().count();
    if len > MAX_DESCRIPTION_CHARS {
        return Err(BrandError::DescriptionTooLong { len });
    }
    if description.chars().any(|c| c.is_control()) {
        return Err(BrandError::DescriptionControlChars);
    }
    Ok(())
}

/// The slug title-cased: `kalkulator-pizzy` → `Kalkulator Pizzy`. Callers
/// validate the slug first, so segments are never empty; empties are still
/// skipped rather than rendered.
pub fn default_name_for_slug(slug: &str) -> String {
    slug.split('-')
        .filter(|word| !word.is_empty())
        .map(capitalize)
        .collect::<Vec<_>>()
        .join(" ")
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// The home-screen label: `name` when it fits, else the first word when
/// that fits, else the first 12 chars of `name`, trimmed.
pub fn short_name_for(name: &str) -> String {
    if name.chars().count() <= SHORT_NAME_CHARS {
        return name.to_string();
    }
    if let Some(first) = name.split_whitespace().next() {
        if first.chars().count() <= SHORT_NAME_CHARS {
            return first.to_string();
        }
    }
    name.chars()
        .take(SHORT_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// FNV-1a 32-bit: the standard test vectors are `""` → `0x811c9dc5` and
/// `"a"` → `0xe40c292c`.
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// `HSL(hue, 62%, 48%)` as lowercase `#rrggbb`.
fn hsl_to_hex(hue_deg: u32, saturation: f64, lightness: f64) -> String {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let hue_prime = f64::from(hue_deg) / 60.0;
    let second = chroma * (1.0 - ((hue_prime % 2.0) - 1.0).abs());
    let (red, green, blue) = match hue_deg {
        0..60 => (chroma, second, 0.0),
        60..120 => (second, chroma, 0.0),
        120..180 => (0.0, chroma, second),
        180..240 => (0.0, second, chroma),
        240..300 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let shift = lightness - chroma / 2.0;
    let channel = |value: f64| -> u8 { ((value + shift) * 255.0).round().clamp(0.0, 255.0) as u8 };
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(red),
        channel(green),
        channel(blue)
    )
}

/// The deterministic accent for `slug`: hue = FNV-1a 32-bit(slug) mod 360,
/// then `HSL(hue, 62%, 48%)`.
pub fn accent_for_slug(slug: &str) -> String {
    hsl_to_hex(fnv1a_32(slug.as_bytes()) % 360, 0.62, 0.48)
}

/// Resolve the brand for a new app: explicit values win when valid, else
/// the slug title-cased (`name`) and `name` itself (`description`).
pub fn resolve_brand(
    slug: &str,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<Brand, BrandError> {
    let name = match name {
        Some(explicit) => {
            validate_name(explicit)?;
            explicit.to_string()
        }
        None => default_name_for_slug(slug),
    };
    let description = match description {
        Some(explicit) => {
            validate_description(explicit)?;
            explicit.to_string()
        }
        None => name.clone(),
    };
    Ok(Brand {
        short_name: short_name_for(&name),
        accent: accent_for_slug(slug),
        name,
        description,
    })
}

/// Serialize `brand` over the tree's `brand.json`, replacing the template's
/// copy. `serde_json` escapes `"` and `\`; `<`/`>`/`&` pass through as
/// UTF-8, so templates must render the values as text (never `innerHTML`).
pub fn write_brand_file(tree: &Path, brand: &Brand) -> anyhow::Result<()> {
    let mut json = serde_json::to_vec_pretty(brand)?;
    json.push(b'\n');
    std::fs::write(tree.join("brand.json"), json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_title_case_is_the_default_name_and_description() {
        let brand = resolve_brand("kalkulator-pizzy", None, None).unwrap();
        assert_eq!(brand.name, "Kalkulator Pizzy");
        assert_eq!(brand.description, "Kalkulator Pizzy");
        assert_eq!(brand.short_name, "Kalkulator");
    }

    #[test]
    fn short_name_boundaries() {
        // Exactly 12 chars fits whole.
        assert_eq!(short_name_for("Abcdefghijkl"), "Abcdefghijkl");
        // 13 chars falls back to the first word when it fits.
        assert_eq!(short_name_for("Pizza Calculator"), "Pizza");
        // A first word past 12 chars falls back to the first 12 chars.
        assert_eq!(short_name_for("Extraordinary pizza"), "Extraordinar");
        // The 12-char cut is trimmed, never a dangling space.
        assert_eq!(short_name_for("Extraordinarypizza x"), "Extraordinar");
        // A single word past the cap truncates.
        assert_eq!(short_name_for("Supercalifragilistic"), "Supercalifra");
    }

    #[test]
    fn accent_is_deterministic_and_hex() {
        let first = accent_for_slug("groceries");
        assert_eq!(first, accent_for_slug("groceries"));
        assert!(
            first.len() == 7 && first.starts_with('#'),
            "valid #rrggbb: {first}"
        );
        assert!(
            first[1..].chars().all(|c| c.is_ascii_hexdigit()),
            "hex digits: {first}"
        );
        assert_ne!(accent_for_slug("groceries"), accent_for_slug("pizza"));
    }

    #[test]
    fn accent_pin_computed_by_hand() {
        // FNV-1a 32-bit("groceries") = 1784649060; 1784649060 mod 360 = 180.
        // HSL(180, 62%, 48%): chroma = 0.5952, second = 0.5952, shift = 0.1824,
        // sector 180..240 is (0, second, chroma): r = 0.1824*255 = 46.5 -> 47,
        // g = b = 0.7776*255 = 198.3 -> 198, i.e. #2fc6c6.
        assert_eq!(fnv1a_32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a_32(b"a"), 0xe40c_292c);
        assert_eq!(fnv1a_32(b"groceries"), 1784649060);
        assert_eq!(accent_for_slug("groceries"), "#2fc6c6");
    }

    #[test]
    fn json_escaping_round_trips() {
        let brand = resolve_brand(
            "pizza",
            Some(r#"Pizza "Pepperoni" </script>"#),
            Some("Half & <half> \"quoted\""),
        )
        .unwrap();
        let json = serde_json::to_string_pretty(&brand).unwrap();
        let back: Brand = serde_json::from_str(&json).unwrap();
        assert_eq!(back, brand);
        assert!(json.contains(r#"Pizza \"Pepperoni\" </script>"#));
    }

    #[test]
    fn name_and_description_limits_refuse() {
        let name_61: String = "n".repeat(61);
        assert_eq!(
            resolve_brand("pizza", Some(&name_61), None).unwrap_err(),
            BrandError::NameTooLong { len: 61 }
        );
        assert_eq!(
            resolve_brand("pizza", None, Some(&"d".repeat(161))).unwrap_err(),
            BrandError::DescriptionTooLong { len: 161 }
        );
        assert_eq!(
            resolve_brand("pizza", Some(""), None).unwrap_err(),
            BrandError::EmptyName
        );
        assert_eq!(
            resolve_brand("pizza", None, Some("")).unwrap_err(),
            BrandError::EmptyDescription
        );
        assert_eq!(
            resolve_brand("pizza", Some("Pi\tzza"), None).unwrap_err(),
            BrandError::NameControlChars
        );
        // Boundaries pass: 60-char name, 160-char description.
        let brand = resolve_brand("pizza", Some(&"n".repeat(60)), Some(&"d".repeat(160))).unwrap();
        assert_eq!(brand.name.chars().count(), 60);
        assert_eq!(brand.description.chars().count(), 160);
    }

    #[test]
    fn explicit_values_win_over_defaults() {
        let brand = resolve_brand(
            "pizza",
            Some("Pizza Calculator"),
            Some("Split pizza orders fairly between friends."),
        )
        .unwrap();
        assert_eq!(brand.name, "Pizza Calculator");
        assert_eq!(brand.short_name, "Pizza");
        assert_eq!(
            brand.description,
            "Split pizza orders fairly between friends."
        );
        assert_eq!(brand.accent, accent_for_slug("pizza"));
    }
}
