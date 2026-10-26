//! The Free "outer.now" badge (branding Contract 2).
//!
//! While an app's owner is not on Pro, the proxy appends [`badge_snippet`]
//! to the end of every HTML page the app serves. Pro pages carry nothing,
//! so templates keep fixed bottom UI clear with
//! `var(--outer-badge-space, 0px)` and never hide, cover, or restyle
//! `#outer-badge`.
//!
//! The snippet is plain HTML with inline `!important` styles and no
//! JavaScript. Slugs match `^[a-z0-9]([a-z0-9-]{0,46}[a-z0-9])?$`
//! ([`crate::apps::slug`]), so interpolating one into the `href` and
//! link text needs no escaping.

use axum::http::HeaderMap;

/// The badge for `slug`, built once per request: a style element setting
/// the `--outer-badge-space` offset templates honour, then a small neutral
/// pill fixed at the bottom right, above `env(safe-area-inset-bottom)`.
pub fn badge_snippet(slug: &str) -> Vec<u8> {
    format!(
        "<style>:root{{--outer-badge-space:44px}}</style>\
         <a id=\"outer-badge\" href=\"https://outer.now/?ref={slug}\" target=\"_blank\" rel=\"noopener\" \
         aria-label=\"Made with outer.now\" \
         style=\"all:initial!important;position:fixed!important;right:12px!important;\
         bottom:calc(env(safe-area-inset-bottom,0px) + 12px)!important;z-index:2147483647!important;\
         display:inline-flex!important;align-items:center!important;gap:6px!important;\
         background:#111111!important;color:#ffffff!important;\
         font:500 12px/1 -apple-system,BlinkMacSystemFont,'Segoe UI',system-ui,sans-serif!important;\
         letter-spacing:.02em!important;padding:7px 12px!important;border:0!important;\
         border-radius:9999px!important;text-decoration:none!important;\
         box-shadow:0 2px 8px rgba(0,0,0,.25)!important;cursor:pointer!important\">\
         <svg width=\"10\" height=\"10\" viewBox=\"0 0 10 10\" aria-hidden=\"true\" \
         style=\"display:block!important;width:10px!important;height:10px!important\">\
         <circle cx=\"5\" cy=\"5\" r=\"5\" fill=\"#ffffff\"/></svg>\
         <span style=\"color:#ffffff!important;font:inherit!important\">outer.now</span></a>"
    )
    .into_bytes()
}

/// Whether the request accepts an HTML document: a missing `Accept` means
/// the client takes anything, otherwise `text/html` (or a wildcard covering
/// it). Anything else (API and asset fetches) keeps its encoding so the
/// proxy never pays to decode bodies it must not touch.
pub fn accepts_html(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(axum::http::header::ACCEPT) else {
        return true;
    };
    let Ok(accept) = value.to_str() else {
        return true;
    };
    accept.split(',').any(|part| {
        matches!(
            part.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "text/html" | "text/*" | "*/*"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                name.parse::<axum::http::HeaderName>().unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    /// HTML has no backslash escapes: a `"` inside a double-quoted
    /// attribute ends it. Read each `style` the way a browser does, up to
    /// the next `"`, and require every declaration to survive, or the
    /// badge renders as bare serif text.
    #[test]
    fn every_style_attribute_survives_html_parsing() {
        let badge = String::from_utf8(badge_snippet("shop")).unwrap();
        let styles: Vec<&str> = badge
            .split("style=\"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        assert_eq!(styles.len(), 3, "{badge}");
        let pill = styles[0];
        for declaration in [
            "all:initial!important",
            "position:fixed!important",
            "font:500 12px/1",
            "padding:7px 12px!important",
            "border-radius:9999px!important",
        ] {
            assert!(pill.contains(declaration), "{declaration} lost: {pill}");
        }
        assert!(pill.ends_with("cursor:pointer!important"), "{pill}");
        assert!(!badge.contains("<script"), "no JS: {badge}");
    }

    #[test]
    fn accepts_html_matrix() {
        assert!(accepts_html(&headers(&[])), "missing accepts anything");
        assert!(accepts_html(&headers(&[(
            "accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        )])));
        assert!(accepts_html(&headers(&[("accept", "*/*")])));
        assert!(accepts_html(&headers(&[("accept", "text/*")])));
        assert!(!accepts_html(&headers(&[("accept", "application/json")])));
        assert!(!accepts_html(&headers(&[(
            "accept",
            "image/avif,image/webp,image/png"
        )])));
    }
}
