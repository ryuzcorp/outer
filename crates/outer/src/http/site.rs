//! The built docs site on the platform host: static files from `dist/`.
//!
//! After Outer's own routes (`/mcp`, `/oauth/*`, `/.well-known/oauth-*`,
//! `/health`, `/ready`), the platform fallback serves Blume's output:
//! `/` → `index.html`, `/x` → `x/index.html` or `x.html`, a trailing-slash
//! redirect (`/x/` → `/x`), `Accept: text/markdown` negotiation onto
//! `<route>.md` when it exists, and Blume's `404.html` with a 404 status.
//! A missing site dir (dev without a build) answers plain 404s, never a
//! panic. Paths can never escape the dir: `..` is rejected and every hit is
//! canonicalized and checked against the canonical root, so symlinks that
//! point out are refused too.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::path::{Path, PathBuf};

/// The canonicalized site root, or `None` when the dir is missing at boot.
#[derive(Clone, Debug)]
pub struct SiteDir {
    root: Option<PathBuf>,
    origin_trial: Option<String>,
}

impl SiteDir {
    /// Canonicalize `dir` once. A missing dir is not an error: every site
    /// path then answers a plain 404 until a build appears.
    pub fn new(dir: &Path) -> Self {
        Self {
            root: std::fs::canonicalize(dir).ok(),
            origin_trial: None,
        }
    }

    /// Carry a WebMCP origin-trial token on `/console*` responses (the
    /// token comes from `OUTER_WEBMCP_ORIGIN_TRIAL` via `Config`).
    pub fn with_origin_trial(mut self, token: String) -> Self {
        self.origin_trial = Some(token);
        self
    }

    /// Answer a platform-host request that Outer's own routes didn't claim.
    /// `None` means "not a site path": the caller falls through to the auth
    /// router (OAuth, well-known, and its 404).
    pub async fn serve_request(
        &self,
        method: &str,
        path: &str,
        query: Option<&str>,
        headers: &HeaderMap,
    ) -> Option<Response> {
        if method != "GET" && method != "HEAD" {
            return None;
        }
        let Some(root) = self.root.as_deref() else {
            return Some(text(
                StatusCode::NOT_FOUND,
                "not found",
                "text/plain; charset=utf-8",
            ));
        };
        if path != "/" && path.ends_with('/') {
            let to = match query {
                Some(q) if !q.is_empty() => format!("{}?{q}", &path[..path.len() - 1]),
                _ => path[..path.len() - 1].to_string(),
            };
            return Some(redirect(&to));
        }
        let Some(route) = Route::parse(path) else {
            return Some(text(
                StatusCode::NOT_FOUND,
                "not found",
                "text/plain; charset=utf-8",
            ));
        };
        if wants_markdown(headers) {
            if let Some(md) = route.markdown_twin() {
                if let Some(file) = read_within(root, &md).await {
                    return Some(file_response(
                        method,
                        file,
                        "text/markdown; charset=utf-8",
                        true,
                        &md,
                        self.origin_trial.as_deref(),
                    ));
                }
            }
        }
        for candidate in route.candidates() {
            if let Some(file) = read_within(root, &candidate).await {
                return Some(file_response(
                    method,
                    file,
                    content_type(&candidate),
                    false,
                    &candidate,
                    self.origin_trial.as_deref(),
                ));
            }
        }
        if let Some(file) = read_within(root, "404.html").await {
            let mut res = file_response(
                method,
                file,
                "text/html; charset=utf-8",
                false,
                "404.html",
                self.origin_trial.as_deref(),
            );
            *res.status_mut() = StatusCode::NOT_FOUND;
            return Some(res);
        }
        Some(text(
            StatusCode::NOT_FOUND,
            "not found",
            "text/plain; charset=utf-8",
        ))
    }
}

/// A site path split into its routing pieces. Rejects anything that could
/// escape the root: `..` segments, backslashes and NUL bytes.
struct Route<'a> {
    /// The path without the leading `/`.
    relative: &'a str,
}

impl<'a> Route<'a> {
    fn parse(path: &'a str) -> Option<Self> {
        let relative = path.strip_prefix('/')?;
        if relative.starts_with('/') {
            // `//etc/passwd`: `Path::join` would discard the root.
            return None;
        }
        if relative.split('/').any(|seg| seg == "..") {
            return None;
        }
        Some(Self { relative })
    }

    /// The `.md` twin for content negotiation: `/` → `index.md`,
    /// `/connect` → `connect.md`. Paths that already name a file keep no
    /// twin (their bytes are served as-is).
    fn markdown_twin(&self) -> Option<String> {
        if self.relative.is_empty() {
            return Some("index.md".to_string());
        }
        if self.relative.ends_with(".md") || self.relative.ends_with(".mdx") {
            return None;
        }
        if last_segment(self.relative).contains('.') {
            return None;
        }
        Some(format!("{}.md", self.relative))
    }

    /// File candidates in order: `/` → `index.html`; `/x` → `x/index.html`,
    /// then `x.html`; `/x/y` likewise nested. A path that already names a
    /// file is served as-is (extension decides the content type).
    fn candidates(&self) -> Vec<String> {
        if self.relative.is_empty() {
            return vec!["index.html".to_string()];
        }
        if last_segment(self.relative).contains('.') {
            return vec![self.relative.to_string()];
        }
        vec![
            format!("{}/index.html", self.relative),
            format!("{}.html", self.relative),
        ]
    }
}

fn last_segment(relative: &str) -> &str {
    relative.rsplit('/').next().unwrap_or(relative)
}

/// Read `relative` under `root`, refusing anything that canonicalizes
/// outside the root (symlink escape) or isn't a file.
async fn read_within(root: &Path, relative: &str) -> Option<Vec<u8>> {
    let full = root.join(relative);
    let canonical = std::fs::canonicalize(&full).ok()?;
    if !canonical.starts_with(root) {
        return None;
    }
    if !canonical.is_file() {
        return None;
    }
    tokio::fs::read(&canonical).await.ok()
}

/// `true` when the `Accept` header prefers Markdown: `text/markdown` (or
/// `text/x-markdown`) listed with a q-value above zero and at least as high
/// as `text/html`'s (Blume's content-negotiation rule).
fn wants_markdown(headers: &HeaderMap) -> bool {
    let Some(accept) = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase)
    else {
        return false;
    };
    let mut markdown_q: f32 = 0.0;
    let mut html_q: Option<f32> = None;
    for entry in accept.split(',') {
        let mut parts = entry.split(';');
        let media = parts.next().unwrap_or("").trim();
        let mut q: f32 = 1.0;
        for param in parts {
            let param = param.trim();
            if let Some(value) = param.strip_prefix("q=") {
                q = value.trim().parse().unwrap_or(1.0);
            }
        }
        if media == "text/markdown" || media == "text/x-markdown" {
            markdown_q = markdown_q.max(q);
        } else if media == "text/html" {
            html_q = Some(html_q.map_or(q, |prev: f32| prev.max(q)));
        } else if media == "*/*" && html_q.is_none() {
            html_q = Some(q);
        }
    }
    markdown_q > 0.0 && markdown_q >= html_q.unwrap_or(0.0)
}

fn content_type(relative: &str) -> &'static str {
    let ext = relative
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "md" | "mdx" => "text/markdown; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "xml" | "rss" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// `Cache-Control` for one served path: hashed `/_astro/*` assets are
/// immutable for a year, HTML is short-lived, everything else in between.
fn cache_control(relative: &str) -> &'static str {
    if relative.starts_with("_astro/") {
        return "public, max-age=31536000, immutable";
    }
    if relative.ends_with(".html") {
        return "public, max-age=60";
    }
    "public, max-age=3600"
}

fn file_response(
    method: &str,
    bytes: Vec<u8>,
    content_type: &'static str,
    negotiated: bool,
    relative: &str,
    origin_trial: Option<&str>,
) -> Response {
    // The console CSP hashes the page's own inline blocks, so hash the
    // bytes before they move into the body.
    let console_csp = if is_console(relative) {
        Some(console_csp(&bytes))
    } else {
        None
    };
    let body = if method == "HEAD" {
        axum::body::Body::empty()
    } else {
        axum::body::Body::from(bytes)
    };
    let mut res = (StatusCode::OK, body).into_response();
    let headers = res.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        content_type.parse().expect("static content type parses"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        cache_control(relative)
            .parse()
            .expect("static cache value parses"),
    );
    if negotiated {
        headers.insert(axum::http::header::VARY, "Accept".parse().unwrap());
    }
    if let Some(csp) = console_csp {
        headers.insert(
            "content-security-policy",
            csp.parse().expect("hashes are header-safe"),
        );
        headers.insert(
            "x-content-type-options",
            "nosniff".parse().expect("static header parses"),
        );
        headers.insert(
            "referrer-policy",
            "same-origin".parse().expect("static header parses"),
        );
    }
    if let Some(token) = origin_trial.filter(|_| is_console(relative)) {
        if let Ok(value) = token.parse() {
            headers.insert("origin-trial", value);
        }
    }
    res
}

/// The console shells: `console` itself and everything beneath it.
fn is_console(relative: &str) -> bool {
    relative == "console" || relative.starts_with("console/")
}

/// The `/console/*` content security policy (spec §13.1): strict
/// same-origin, no framing. Astro inlines its island runtime (and a small
/// style rule) into these shells, so `script-src`/`style-src` carry sha256
/// hashes of every inline block, computed over the exact bytes being served:
/// rebuilds change the hashes, never the checking. Anything else stays
/// `'self`-only; the no-sniff and referrer headers join it in
/// [`file_response`].
fn console_csp(html: &[u8]) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let mut csp = String::from("default-src 'none'; script-src 'self'");
    for body in inline_bodies(html, b"script") {
        let hash = Sha256::digest(body);
        csp.push_str(&format!(
            " 'sha256-{}'",
            base64::engine::general_purpose::STANDARD.encode(hash)
        ));
    }
    csp.push_str("; style-src 'self'");
    for body in inline_bodies(html, b"style") {
        let hash = Sha256::digest(body);
        csp.push_str(&format!(
            " 'sha256-{}'",
            base64::engine::general_purpose::STANDARD.encode(hash)
        ));
    }
    csp.push_str(
        "; img-src 'self' data:; font-src 'self'; connect-src 'self'; \
         base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
    );
    csp
}

/// Bodies of the inline `<script>`/`<style>` elements in `html`: every open
/// tag without a `src` attribute through its matching close tag, over the
/// exact served bytes. External files (`src=`) are already covered by
/// `'self` and contribute no hash.
fn inline_bodies<'a>(html: &'a [u8], tag: &[u8]) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut rest = html;
    // `<script` / `<style`, then the open tag, then `</tag>`.
    let mut open_tag = Vec::with_capacity(tag.len() + 1);
    open_tag.push(b'<');
    open_tag.extend_from_slice(tag);
    let mut close_tag = Vec::with_capacity(tag.len() + 3);
    close_tag.extend_from_slice(b"</");
    close_tag.extend_from_slice(tag);
    close_tag.push(b'>');
    while let Some(start) = find_bytes(rest, &open_tag) {
        let after_open = &rest[start + open_tag.len()..];
        let Some(end_of_open) = find_bytes(after_open, b">") else {
            break;
        };
        let attrs = &after_open[..end_of_open];
        let body_starts = start + open_tag.len() + end_of_open + 1;
        // A tag boundary, not `<scripts>` or `<stylesheet>`: the name ends
        // here (whitespace, `/`, or the `>` that left `attrs` empty).
        if !attrs.is_empty() && !matches!(attrs.first(), Some(b' ' | b'\t' | b'\n' | b'\r' | b'/'))
        {
            rest = &rest[start + 1..];
            continue;
        }
        if attrs.windows(4).any(|w| w.eq_ignore_ascii_case(b"src=")) {
            rest = &rest[body_starts..];
            continue;
        }
        let after_body = &rest[body_starts..];
        let Some(end) = find_bytes(after_body, &close_tag) else {
            break;
        };
        out.push(&after_body[..end]);
        rest = &after_body[end + close_tag.len()..];
    }
    out
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn text(status: StatusCode, body: &str, content_type: &'static str) -> Response {
    let mut res = (status, body.to_string()).into_response();
    res.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        content_type.parse().expect("static content type parses"),
    );
    res
}

fn redirect(to: &str) -> Response {
    let mut res = (StatusCode::MOVED_PERMANENTLY, String::new()).into_response();
    res.headers_mut().insert(
        axum::http::header::LOCATION,
        to.parse().unwrap_or_else(|_| "/".parse().unwrap()),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(accept: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            headers.insert(axum::http::header::ACCEPT, accept.parse().unwrap());
        }
        headers
    }

    async fn body_text(res: Response) -> (StatusCode, HeaderMap, String) {
        let (mut parts, body) = res.into_parts();
        let headers = std::mem::take(&mut parts.headers);
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (
            parts.status,
            headers,
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }

    /// A temp site dir: `/index.html`, `/connect/index.html`,
    /// `/connect.md`, `/llms.txt`, a hashed `/_astro/` asset and `404.html`.
    fn fixture() -> (tempfile::TempDir, SiteDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("connect")).unwrap();
        std::fs::create_dir_all(root.join("_astro")).unwrap();
        std::fs::write(root.join("index.html"), "<h1>Outer</h1>").unwrap();
        std::fs::write(root.join("connect/index.html"), "<h1>Connect</h1>").unwrap();
        std::fs::write(root.join("connect.md"), "# Connect\n").unwrap();
        std::fs::write(root.join("llms.txt"), "# Outer\n").unwrap();
        std::fs::write(root.join("_astro/app.js"), "console.log(1)").unwrap();
        std::fs::write(root.join("404.html"), "<h1>Missing</h1>").unwrap();
        let site = SiteDir::new(root);
        assert!(site.root.is_some(), "fixture dir must canonicalize");
        (dir, site)
    }

    #[tokio::test]
    async fn root_and_nested_routes() {
        let (_dir, site) = fixture();
        let (status, hdrs, body) = body_text(
            site.serve_request("GET", "/", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "<h1>Outer</h1>");
        assert_eq!(
            hdrs[axum::http::header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(
            hdrs[axum::http::header::CACHE_CONTROL],
            "public, max-age=60"
        );

        let (status, _, body) = body_text(
            site.serve_request("GET", "/connect", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "<h1>Connect</h1>");
    }

    #[tokio::test]
    async fn trailing_slash_redirects() {
        let (_dir, site) = fixture();
        let res = site
            .serve_request("GET", "/connect/", None, &headers(None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(res.headers()[axum::http::header::LOCATION], "/connect");

        let res = site
            .serve_request("GET", "/connect/", Some("a=b"), &headers(None))
            .await
            .unwrap();
        assert_eq!(res.headers()[axum::http::header::LOCATION], "/connect?a=b");
    }

    #[tokio::test]
    async fn content_types_and_immutable_assets() {
        let (_dir, site) = fixture();
        let (status, hdrs, _) = body_text(
            site.serve_request("GET", "/llms.txt", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            hdrs[axum::http::header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );

        let (_, hdrs, _) = body_text(
            site.serve_request("GET", "/_astro/app.js", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            hdrs[axum::http::header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            hdrs[axum::http::header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn markdown_negotiation_serves_the_twin() {
        let (_dir, site) = fixture();
        let (status, hdrs, body) = body_text(
            site.serve_request("GET", "/connect", None, &headers(Some("text/markdown")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "# Connect\n");
        assert_eq!(
            hdrs[axum::http::header::CONTENT_TYPE],
            "text/markdown; charset=utf-8"
        );
        assert_eq!(hdrs[axum::http::header::VARY], "Accept");

        // HTML wins when it outranks Markdown.
        let (_, _, body) = body_text(
            site.serve_request(
                "GET",
                "/connect",
                None,
                &headers(Some("text/html, text/markdown;q=0.9")),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(body, "<h1>Connect</h1>");

        // No twin, no negotiation: the HTML stands.
        let (_, _, body) = body_text(
            site.serve_request("GET", "/", None, &headers(Some("text/markdown")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body, "<h1>Outer</h1>");
    }

    #[tokio::test]
    async fn missing_pages_use_blumes_404() {
        let (_dir, site) = fixture();
        let (status, _, body) = body_text(
            site.serve_request("GET", "/nope", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "<h1>Missing</h1>");
    }

    #[tokio::test]
    async fn traversal_is_a_404() {
        let (_dir, site) = fixture();
        for path in ["/../secret", "/%2e%2e/x", "/a/../../b", "/\\windows"] {
            let res = site
                .serve_request("GET", path, None, &headers(None))
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::NOT_FOUND,
                "path {path} escaped or errored"
            );
        }
    }

    #[tokio::test]
    async fn missing_site_dir_is_plain_404s() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-build-yet");
        let site = SiteDir::new(&missing);
        assert!(site.root.is_none());
        let (status, headers, _) = body_text(
            site.serve_request("GET", "/", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            headers[axum::http::header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn head_has_headers_but_no_body() {
        let (_dir, site) = fixture();
        let (status, headers, body) = body_text(
            site.serve_request("HEAD", "/", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "");
        assert_eq!(
            headers[axum::http::header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
    }
    #[tokio::test]
    async fn non_get_falls_through_to_auth() {
        let (_dir, site) = fixture();
        assert!(site
            .serve_request("POST", "/", None, &headers(None))
            .await
            .is_none());
    }

    /// `/console/*` pages carry the strict console headers; docs pages do
    /// not.
    #[tokio::test]
    async fn console_pages_carry_strict_headers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("console")).unwrap();
        std::fs::write(
            dir.path().join("console/index.html"),
            "<h1>Console</h1><script>island()</script>",
        )
        .unwrap();
        std::fs::write(dir.path().join("index.html"), "<h1>Outer</h1>").unwrap();
        let site = SiteDir::new(dir.path());
        let (status, hdrs, _) = body_text(
            site.serve_request("GET", "/console", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let csp = hdrs["content-security-policy"].to_str().unwrap();
        assert!(csp.contains("script-src 'self'"), "got: {csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "got: {csp}");
        // The inline block's hash rides along, so the page's own scripts
        // pass while anything else stays blocked.
        let body = b"island()";
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(body);
        use base64::Engine as _;
        let expect = format!(
            "'sha256-{}'",
            base64::engine::general_purpose::STANDARD.encode(hash)
        );
        assert!(csp.contains(&expect), "inline script hashed, got: {csp}");

        let (_, hdrs, _) = body_text(
            site.serve_request("GET", "/", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert!(!hdrs.contains_key("content-security-policy"));
    }

    /// The WebMCP origin trial rides an `Origin-Trial` response header on
    /// `/console*` only; docs pages stay untouched, and an absent token
    /// sends nothing.
    #[tokio::test]
    async fn origin_trial_header_only_on_console() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("console")).unwrap();
        std::fs::write(dir.path().join("console/index.html"), "<h1>Console</h1>").unwrap();
        std::fs::write(dir.path().join("index.html"), "<h1>Outer</h1>").unwrap();
        let site = SiteDir::new(dir.path()).with_origin_trial("token-abc".to_string());
        let (_, hdrs, _) = body_text(
            site.serve_request("GET", "/console", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(hdrs["origin-trial"], "token-abc");
        let (_, hdrs, _) = body_text(
            site.serve_request("GET", "/", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert!(!hdrs.contains_key("origin-trial"));
        let plain = SiteDir::new(dir.path());
        let (_, hdrs, _) = body_text(
            plain
                .serve_request("GET", "/console", None, &headers(None))
                .await
                .unwrap(),
        )
        .await;
        assert!(!hdrs.contains_key("origin-trial"));
    }

    #[test]
    fn accept_parsing_prefers_markdown_correctly() {
        assert!(wants_markdown(&headers(Some("text/markdown"))));
        assert!(wants_markdown(&headers(Some("text/markdown, text/html"))));
        assert!(!wants_markdown(&headers(Some(
            "text/html, text/markdown;q=0.9"
        ))));
        assert!(wants_markdown(&headers(Some("text/x-markdown"))));
        assert!(!wants_markdown(&headers(Some("*/*"))));
        assert!(!wants_markdown(&headers(None)));
    }
}
