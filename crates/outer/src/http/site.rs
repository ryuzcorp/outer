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
}

impl SiteDir {
    /// Canonicalize `dir` once. A missing dir is not an error: every site
    /// path then answers a plain 404 until a build appears.
    pub fn new(dir: &Path) -> Self {
        Self {
            root: std::fs::canonicalize(dir).ok(),
        }
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
                ));
            }
        }
        if let Some(file) = read_within(root, "404.html").await {
            let mut res =
                file_response(method, file, "text/html; charset=utf-8", false, "404.html");
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
) -> Response {
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
    res
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
