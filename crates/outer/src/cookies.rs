//! Cross-app cookie isolation on the shared apps domain.
//!
//! Every app serves under `<slug>.<apps_domain>` on one registrable domain,
//! and `outer.now` is not on the Public Suffix List — so without platform
//! help one app could set a `Domain=outer.now` cookie that every other app
//! receives. This module is the platform help: the proxy rewrites app
//! cookies into host-locked `__Host-` cookies on the way out and maps them
//! back on the way in, and a tiny client shim does the same mapping for the
//! app's own JavaScript. Custom domains are their own sites and stay
//! untouched (the proxy only calls this module for shared-domain hosts).
//!
//! Server rules (both directions are pure functions, tested below):
//!
//! - Response: `foo` becomes `__Host-o.foo`; a name already starting
//!   `__Host-` passes through unchanged (browsers already scope it to the
//!   setting host); `Domain` is dropped, `Path=/` and `Secure` are forced,
//!   and only `Max-Age`/`Expires`/`HttpOnly`/`SameSite`/`Partitioned` survive
//!   alongside them. A name colliding with a platform cookie
//!   (`__Host-outer_sid`, `__Host-outer-access`, `__Host-outer-age`) is
//!   dropped and logged at debug.
//! - Request: only `__Host-` cookies reach the app — `__Host-o.foo` maps
//!   back to `foo`, the app's own `__Host-x` passes as-is, platform cookies
//!   stay stripped, everything else (including any `Domain`-tossed cookie,
//!   which can never carry a `__Host-` name) is dropped.
//!
//! Client rules (best effort, [`COOKIE_SHIM_JS`]): the shim wraps
//! `Document.prototype.cookie` and `window.cookieStore` with the same
//! mapping, so the victim app's JS never sees a tossed cookie and the app's
//! own JS cookies stay host-locked and keep round-tripping to the server.

use axum::http::{HeaderMap, HeaderValue};

/// The prefix the proxy gives every app cookie on a shared-domain host:
/// `sid` is stored (and sent back) as `__Host-o.sid`.
pub const APP_PREFIX: &str = "__Host-o.";

/// The `__Host-` prefix browsers already scope to the setting host: an app
/// cookie carrying it passes through the response rewrite unchanged.
const HOST_PREFIX: &str = "__Host-";

/// Platform cookies, reserved on every host. An app response that sets one
/// is dropped; an app request that carries one never reaches the app. The
/// match is case-insensitive: no casing of these names is ever an app
/// cookie.
pub const PLATFORM_COOKIES: [&str; 3] = [
    crate::auth::SESSION_COOKIE,
    crate::access::COOKIE_NAME,
    crate::age::COOKIE_NAME,
];

/// Whether `name` is one of [`PLATFORM_COOKIES`], any casing.
pub fn is_platform_cookie(name: &str) -> bool {
    PLATFORM_COOKIES
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// Whether `name` is one of [`PLATFORM_COOKIES`], any casing (byte form, so
/// non-UTF-8 cookie names still match).
fn is_platform_cookie_bytes(name: &[u8]) -> bool {
    PLATFORM_COOKIES.iter().any(|reserved| {
        name.len() == reserved.len()
            && name
                .iter()
                .zip(reserved.bytes())
                .all(|(a, b)| a.eq_ignore_ascii_case(&b))
    })
}

/// Trim ASCII spaces and tabs, the only whitespace a cookie pair can carry.
fn trim_ws(raw: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = raw.len();
    while start < end && (raw[start] == b' ' || raw[start] == b'\t') {
        start += 1;
    }
    while end > start && (raw[end - 1] == b' ' || raw[end - 1] == b'\t') {
        end -= 1;
    }
    &raw[start..end]
}

/// Split a `Set-Cookie` value on `;` separators, ignoring `;` inside a
/// double-quoted value (`Expires` carries a comma but no semicolon, so only
/// quoted values need the care).
fn split_attrs(value: &[u8]) -> Vec<&[u8]> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, byte) in value.iter().enumerate() {
        if *byte == b'"' {
            quoted = !quoted;
        } else if *byte == b';' && !quoted {
            parts.push(&value[start..i]);
            start = i + 1;
        }
    }
    parts.push(&value[start..]);
    parts
}

/// The attribute half before the first `=`, trimmed (`Secure` and
/// `HttpOnly` have no `=` at all).
fn attr_name(attr: &[u8]) -> &[u8] {
    let end = attr
        .iter()
        .position(|byte| *byte == b'=')
        .unwrap_or(attr.len());
    trim_ws(&attr[..end])
}

/// Whether `attr` names one of the attributes the rewrite keeps alongside
/// the forced `Path`/`Secure` (case-insensitive, per RFC 6265 §5.2).
fn is_kept_attr(name: &[u8]) -> bool {
    ["max-age", "expires", "httponly", "samesite", "partitioned"]
        .iter()
        .any(|kept| {
            name.len() == kept.len()
                && name
                    .iter()
                    .zip(kept.bytes())
                    .all(|(a, b)| a.eq_ignore_ascii_case(&b))
        })
}

/// Rewrite one upstream `Set-Cookie` value into its host-locked form, or
/// `None` when the app must not set it at all (empty, nameless, or a
/// platform-cookie collision — the collision is logged at debug).
pub fn rewrite_set_cookie(value: &HeaderValue) -> Option<HeaderValue> {
    let raw = value.as_bytes();
    let mut attrs = split_attrs(raw).into_iter();
    let head = trim_ws(attrs.next().unwrap_or_default());
    let split = head.iter().position(|byte| *byte == b'=');
    let (raw_name, raw_value) = match split {
        Some(i) => (&head[..i], &head[i + 1..]),
        None => (head, &[][..]),
    };
    let name = trim_ws(raw_name);
    if name.is_empty() {
        return None;
    }
    if is_platform_cookie_bytes(name) {
        tracing::debug!(
            name = %String::from_utf8_lossy(name),
            "dropping app cookie that collides with a platform cookie",
        );
        return None;
    }
    if name.starts_with(HOST_PREFIX.as_bytes()) {
        return Some(value.clone());
    }
    let mut out = Vec::with_capacity(raw.len() + APP_PREFIX.len() + 16);
    out.extend_from_slice(APP_PREFIX.as_bytes());
    out.extend_from_slice(name);
    out.push(b'=');
    out.extend_from_slice(raw_value);
    out.extend_from_slice(b"; Path=/; Secure");
    for attr in attrs {
        let kept = trim_ws(attr);
        if kept.is_empty() {
            continue;
        }
        let name = attr_name(kept);
        if name.eq_ignore_ascii_case(b"domain")
            || name.eq_ignore_ascii_case(b"path")
            || name.eq_ignore_ascii_case(b"secure")
        {
            continue;
        }
        if is_kept_attr(name) {
            out.extend_from_slice(b"; ");
            out.extend_from_slice(kept);
        }
    }
    HeaderValue::from_bytes(&out).ok()
}

/// Rewrite every `Set-Cookie` on a shared-domain upstream response in
/// place: each value maps through [`rewrite_set_cookie`], drops vanish,
/// survivors keep their separate headers (they must never be combined).
pub fn rewrite_set_cookie_headers(headers: &mut HeaderMap) {
    let values: Vec<HeaderValue> = headers
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .cloned()
        .collect();
    if values.is_empty() {
        return;
    }
    headers.remove(axum::http::header::SET_COOKIE);
    for value in &values {
        if let Some(rewritten) = rewrite_set_cookie(value) {
            headers.append(axum::http::header::SET_COOKIE, rewritten);
        }
    }
}

/// Map one already-merged `Cookie` value (platform cookies already
/// stripped) to what the app sees on a shared-domain host: `__Host-o.foo`
/// back to `foo`, the app's own `__Host-x` as-is, everything else dropped.
/// `None` when nothing survives (so no empty `Cookie` header is sent).
pub fn isolate_request_cookies(merged: &str) -> Option<String> {
    let mut kept = Vec::new();
    for pair in merged.split(';') {
        let trimmed = pair.trim_matches([' ', '\t']);
        if trimmed.is_empty() {
            continue;
        }
        let (raw_name, rest) = match trimmed.split_once('=') {
            Some((name, _)) => (name, &trimmed[name.len()..]),
            None => (trimmed, ""),
        };
        let name = raw_name.trim_matches([' ', '\t']);
        if name.is_empty() || is_platform_cookie(name) {
            continue;
        }
        if let Some(suffix) = name.strip_prefix(APP_PREFIX) {
            kept.push(format!("{suffix}{}", rest.trim_end_matches([' ', '\t'])));
        } else if name.starts_with(HOST_PREFIX) {
            kept.push(trimmed.to_string());
        }
        // Anything else is a foreign tossed cookie: dropped.
    }
    if kept.is_empty() {
        None
    } else {
        Some(kept.join("; "))
    }
}

/// The client shim: wraps `document.cookie` and `window.cookieStore` with
/// the same mapping the server applies, so tossed cookies stay invisible to
/// the app's JS and the app's own JS cookies keep round-tripping. Served as
/// one inline `<script>` (see [`cookie_shim_snippet`]); its CSP hash is
/// [`shim_csp_hash`].
pub const COOKIE_SHIM_JS: &str = r#"!function(){var P="__Host-o.",H="__Host-",SID="__Host-outer_sid",ACC="__Host-outer-access",AGE="__Host-outer-age";function plat(n){return n===SID||n===ACC||n===AGE}function pubOf(n){if(plat(n))return null;if(n.indexOf(P)===0)return n.slice(P.length);if(n.indexOf(H)===0)return n;return null}function storedOf(n){n=String(n);return n.indexOf(H)===0?n:P+n}function storedName(n){n=String(n);if(plat(n))return null;return storedOf(n)}function splitPair(p){var i=p.indexOf("=");return i<0?[p,""]:[p.slice(0,i),p.slice(i+1)]}try{var d=Object.getOwnPropertyDescriptor(Document.prototype,"cookie");if(d&&typeof d.get=="function"&&typeof d.set=="function"){Object.defineProperty(Document.prototype,"cookie",{configurable:true,enumerable:d.enumerable,get:function(){var raw="";try{raw=d.get.call(this)}catch(e){return ""}if(!raw)return raw;var out=[];var parts=raw.split(";");for(var i=0;i<parts.length;i++){var kv=splitPair(parts[i]);var nm=kv[0].trim();if(!nm)continue;var pub_=pubOf(nm);if(pub_===null)continue;out.push(pub_+"="+kv[1])}return out.join("; ")},set:function(v){v=String(v);var parts=v.split(";");var head=parts.shift();var kv=splitPair(head);var nm=kv[0].trim();if(!nm)return;var rest="";for(var i=0;i<parts.length;i++){if(/^\s*domain\s*=/i.test(parts[i]))continue;rest+=";"+parts[i]}try{d.set.call(this,storedOf(nm)+"="+kv[1]+"; Path=/; Secure"+rest)}catch(e){}}});}}catch(e){}try{var cs=window.cookieStore;if(cs&&typeof cs.get=="function"&&typeof cs.set=="function"&&typeof cs.delete=="function"&&typeof cs.getAll=="function"){var _get=cs.get.bind(cs),_getAll=cs.getAll.bind(cs),_set=cs.set.bind(cs),_del=cs.delete.bind(cs);function mapItem(c){if(!c||typeof c.name!="string")return null;var m=pubOf(c.name);if(m===null)return null;var o={};for(var k in c)o[k]=c[k];o.name=m;return o}function mapList(l){var out=[];for(var i=0;i<l.length;i++){var m=mapItem(l[i]);if(m!==null)out.push(m)}return out}cs.get=function(n){if(n==null)return _get().then(mapList);var s=storedName(n);if(s===null)return Promise.resolve(null);return _get(s).then(function(c){return mapItem(c)})};cs.getAll=function(n){if(n==null)return _getAll().then(mapList);if(typeof n==="string"){var s=storedName(n);if(s===null)return Promise.resolve([]);return _getAll(s).then(mapList)}return _getAll(n).then(mapList)};cs.set=function(a,b){if(typeof a==="string"){var s=storedName(a);if(s===null)return Promise.resolve();return _set({name:s,value:b,path:"/",secure:true})}var o={};for(var k in a)o[k]=a[k];var s2=storedName(o.name);if(s2===null)return Promise.resolve();o.name=s2;delete o.domain;o.path="/";o.secure=true;return _set(o,b)};cs.delete=function(a){if(typeof a==="string"){var s=storedName(a);if(s===null)return Promise.resolve();return _del({name:s,path:"/"})}var o={};for(var k in a)o[k]=a[k];var s2=storedName(o.name);if(s2===null)return Promise.resolve();o.name=s2;o.path="/";return _del(o)};}}catch(e){}}();"#;

/// The injected head element: the shim script, nothing else.
pub fn cookie_shim_snippet() -> Vec<u8> {
    let mut out = Vec::with_capacity(COOKIE_SHIM_JS.len() + 32);
    out.extend_from_slice(b"<script>");
    out.extend_from_slice(COOKIE_SHIM_JS.as_bytes());
    out.extend_from_slice(b"</script>");
    out
}

/// The CSP source expression allowlisting exactly [`COOKIE_SHIM_JS`],
/// computed from the constant so the two can never drift apart.
pub fn shim_csp_hash() -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(COOKIE_SHIM_JS.as_bytes());
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// The insertion point for the shim: just past the closing `>` of the first
/// `<head ...>` (case-insensitive, attributes allowed, never matching
/// `<header>`), or `None` when the prefix carries no head element.
pub fn find_head_insert(html: &[u8]) -> Option<usize> {
    if html.len() < 6 {
        return None;
    }
    let mut i = 0;
    while i + 5 < html.len() {
        let is_head = html[i] == b'<'
            && (html[i + 1] == b'h' || html[i + 1] == b'H')
            && (html[i + 2] == b'e' || html[i + 2] == b'E')
            && (html[i + 3] == b'a' || html[i + 3] == b'A')
            && (html[i + 4] == b'd' || html[i + 4] == b'D')
            && matches!(html[i + 5], b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/');
        if is_head {
            if let Some(rel) = html[i..].iter().position(|byte| *byte == b'>') {
                return Some(i + rel + 1);
            }
            return None;
        }
        i += 1;
    }
    None
}

/// Insert the shim as the first child of `<head>`, or prepend it when the
/// page has no head element: either way it runs before any app script.
pub fn inject_cookie_shim(html: &[u8]) -> Vec<u8> {
    let snippet = cookie_shim_snippet();
    let mut out = Vec::with_capacity(html.len() + snippet.len());
    match find_head_insert(html) {
        Some(pos) => {
            out.extend_from_slice(&html[..pos]);
            out.extend_from_slice(&snippet);
            out.extend_from_slice(&html[pos..]);
        }
        None => {
            out.extend_from_slice(&snippet);
            out.extend_from_slice(html);
        }
    }
    out
}

/// Append the shim's CSP hash to the effective script directive of every
/// enforced content-security-policy on a shim-injected response: the first
/// of `script-src-elem`, `script-src`, `default-src` wins. Policies without
/// any of them already allow inline scripts and stay untouched, and nothing
/// else is weakened. Report-only policies never block, so they are left
/// alone.
pub fn append_shim_csp_hash(headers: &mut HeaderMap) {
    let policies: Vec<String> = headers
        .get_all("content-security-policy")
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_string))
        .collect();
    if policies.is_empty() {
        return;
    }
    let hash = shim_csp_hash();
    headers.remove("content-security-policy");
    for policy in &policies {
        let updated = append_hash_to_policy(policy, &hash);
        match HeaderValue::from_str(&updated) {
            Ok(value) => {
                headers.append("content-security-policy", value);
            }
            Err(_) => {
                if let Ok(original) = HeaderValue::from_str(policy) {
                    headers.append("content-security-policy", original);
                }
            }
        }
    }
}

/// Append `'sha256-…'` to the effective script directive of one CSP policy
/// string (see [`append_shim_csp_hash`]); unchanged when no script or
/// default directive is present.
fn append_hash_to_policy(policy: &str, hash: &str) -> String {
    let mut elem = None;
    let mut src = None;
    let mut default = None;
    for (i, directive) in policy.split(';').enumerate() {
        match directive
            .split_whitespace()
            .next()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("script-src-elem") => elem.get_or_insert(i),
            Some("script-src") => src.get_or_insert(i),
            Some("default-src") => default.get_or_insert(i),
            _ => continue,
        };
    }
    let Some(active) = elem.or(src).or(default) else {
        return policy.to_string();
    };
    let mut rebuilt: Vec<String> = policy.split(';').map(str::to_string).collect();
    rebuilt[active] = format!("{} '{hash}'", rebuilt[active].trim_end());
    rebuilt.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_cookie(headers: &HeaderMap) -> Vec<String> {
        headers
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect()
    }

    fn rewrite_all(values: &[&str]) -> Vec<String> {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                axum::http::header::SET_COOKIE,
                HeaderValue::from_str(value).unwrap(),
            );
        }
        rewrite_set_cookie_headers(&mut headers);
        set_cookie(&headers)
    }

    #[test]
    fn set_cookie_rewrite_matrix() {
        // Plain cookie: prefixed, Domain dropped, Path forced, Secure added,
        // kept attributes survive in order.
        assert_eq!(
            rewrite_all(&[
                "sid=abc; Domain=outer.now; Path=/app; HttpOnly; SameSite=Lax; Max-Age=60"
            ]),
            ["__Host-o.sid=abc; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=60"],
        );
        // Odd spacing and lowercase attribute names work the same.
        assert_eq!(
            rewrite_all(&["sid=abc;domain=outer.now;path=/app;secure;httponly;samesite=lax"]),
            ["__Host-o.sid=abc; Path=/; Secure; httponly; samesite=lax"],
        );
        // Unknown attributes (Priority here) are dropped, not passed through.
        assert_eq!(
            rewrite_all(&["sid=abc; Priority=High; SameSite=Lax"]),
            ["__Host-o.sid=abc; Path=/; Secure; SameSite=Lax"],
        );
        // A quoted value carrying a semicolon stays one value.
        assert_eq!(
            rewrite_all(&[
                r#"prefs="a;b"; Domain=outer.now; Expires=Wed, 21 Oct 2015 07:28:00 GMT"#
            ]),
            [r#"__Host-o.prefs="a;b"; Path=/; Secure; Expires=Wed, 21 Oct 2015 07:28:00 GMT"#],
        );
        // `__Host-` names pass through byte-identical (the browser already
        // scopes them to the setting host).
        assert_eq!(
            rewrite_all(&["__Host-x=1; Path=/; Secure"]),
            ["__Host-x=1; Path=/; Secure"],
        );
        // Multiple headers each rewrite on their own line, never combined.
        assert_eq!(
            rewrite_all(&["a=1; Domain=outer.now", "b=2; HttpOnly"]),
            [
                "__Host-o.a=1; Path=/; Secure",
                "__Host-o.b=2; Path=/; Secure; HttpOnly",
            ],
        );
        // Nameless and value-less headers are dropped.
        assert_eq!(rewrite_all(&["; Path=/"]), Vec::<String>::new());
        assert_eq!(
            rewrite_all(&["flag; HttpOnly"]),
            ["__Host-o.flag=; Path=/; Secure; HttpOnly"],
        );
    }

    #[test]
    fn platform_cookie_collisions_are_dropped() {
        for name in PLATFORM_COOKIES {
            let header = format!("{name}=forged; Domain=outer.now; Path=/");
            assert_eq!(
                rewrite_all(&[&header]),
                Vec::<String>::new(),
                "app must not set {name}"
            );
            // Any casing counts: no spelling of a platform name is an app cookie.
            let header = format!("{}=forged", name.to_ascii_uppercase());
            assert_eq!(
                rewrite_all(&[&header]),
                Vec::<String>::new(),
                "app must not set {} either",
                name.to_ascii_uppercase()
            );
        }
    }

    #[test]
    fn request_filter_maps_back_and_drops_tossed_cookies() {
        // Prefixed cookies map back; foreign tossed names drop; platform
        // names stay stripped; the app's own `__Host-` passes as-is.
        assert_eq!(
            isolate_request_cookies(
                "__Host-o.sid=abc; tossed=evil; __Host-x=1; \
                 __Host-outer_sid=s; __Host-outer-access=g; __Host-outer-age=c; \
                 __Host-o.theme=dark"
            )
            .as_deref(),
            Some("sid=abc; __Host-x=1; theme=dark"),
        );
        // An all-foreign Cookie header forwards nothing at all.
        assert_eq!(isolate_request_cookies("tossed=evil; other=1"), None);

        // Messy separators and spacing still parse: the separator
        // whitespace around names and values is not significant.
        assert_eq!(
            isolate_request_cookies("  __Host-o.a=1;;__Host-o.b =2 ;  tossed=3").as_deref(),
            Some("a=1; b=2"),
        );
        // Empty segments never produce an empty header.
        assert_eq!(isolate_request_cookies(";;;"), None);
    }

    #[test]
    fn shim_inserts_once_after_head_or_prepends() {
        let page = inject_cookie_shim(b"<html><head><title>T</title></head><body>hi</body></html>");
        let text = String::from_utf8(page).unwrap();
        assert_eq!(text.matches("<script>").count(), 1, "exactly one shim");
        let head_end = text.find("<title>").unwrap();
        let shim_at = text.find("<script>").unwrap();
        assert!(shim_at < head_end, "shim runs before any app script");
        assert!(
            text.contains("__Host-o."),
            "shim carries the prefix mapping"
        );

        // No head element: prepended, still before everything.
        let page = inject_cookie_shim(b"<html><body>hi</body></html>");
        let text = String::from_utf8(page).unwrap();
        assert!(text.starts_with("<script>"), "{text}");
        assert_eq!(text.matches("<script>").count(), 1);

        // `<header>` is not `<head>`: no false match mid-word.
        assert!(find_head_insert(b"<html><header>x").is_none());
        // Attributes and case vary; the insert lands past the `>`.
        let html = b"<HTML><HEAD lang=en><title>T</title></HEAD></html>";
        let pos = find_head_insert(html).unwrap();
        assert_eq!(&html[..pos], b"<HTML><HEAD lang=en>");
    }

    #[test]
    fn csp_hash_is_stable_and_wired_to_the_shim() {
        use sha2::Digest as _;
        let raw = sha2::Sha256::digest(COOKIE_SHIM_JS.as_bytes());
        use base64::Engine as _;
        let expected = format!(
            "sha256-{}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        );
        assert_eq!(shim_csp_hash(), expected);
        // No `</script>` inside the JS: the inline element cannot truncate.
        assert!(!COOKIE_SHIM_JS.contains("</script>"));
    }

    #[test]
    fn csp_hash_appends_to_the_effective_directive_only() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-security-policy",
            HeaderValue::from_str("default-src 'self'; script-src 'self'; style-src 'self'")
                .unwrap(),
        );
        append_shim_csp_hash(&mut headers);
        let policy = headers["content-security-policy"].to_str().unwrap();
        let hash = shim_csp_hash();
        assert!(
            policy.contains(&format!("script-src 'self' '{hash}'")),
            "{policy}"
        );
        assert!(
            !policy.contains(&format!("default-src 'self' '{hash}'")),
            "{policy}"
        );
        assert!(!policy.contains("style-src 'self' 'sha256-"), "{policy}");

        // `script-src-elem` wins over `script-src` for script elements.
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-security-policy",
            HeaderValue::from_str("script-src 'self'; script-src-elem 'self'").unwrap(),
        );
        append_shim_csp_hash(&mut headers);
        let policy = headers["content-security-policy"].to_str().unwrap();
        assert!(
            policy.contains(&format!("script-src-elem 'self' '{hash}'")),
            "{policy}"
        );
        assert!(
            !policy.contains(&format!("script-src 'self' '{hash}'")),
            "{policy}"
        );

        // No script or default directive: inline already runs, untouched.
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-security-policy",
            HeaderValue::from_str("img-src 'self'").unwrap(),
        );
        append_shim_csp_hash(&mut headers);
        assert_eq!(headers["content-security-policy"], "img-src 'self'");

        // Every enforced policy is updated; report-only never blocks, left alone.
        let mut headers = HeaderMap::new();
        headers.append(
            "content-security-policy",
            HeaderValue::from_str("script-src 'self'").unwrap(),
        );
        headers.append(
            "content-security-policy",
            HeaderValue::from_str("default-src 'none'").unwrap(),
        );
        headers.insert(
            "content-security-policy-report-only",
            HeaderValue::from_str("script-src 'self'").unwrap(),
        );
        append_shim_csp_hash(&mut headers);
        let policies: Vec<_> = headers
            .get_all("content-security-policy")
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect();
        assert_eq!(policies.len(), 2);
        assert!(
            policies.iter().all(|policy| policy.contains(&hash)),
            "{policies:?}"
        );
        assert_eq!(
            headers["content-security-policy-report-only"],
            "script-src 'self'"
        );
    }
}
