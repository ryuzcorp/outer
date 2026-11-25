//! Host-served legal pages: `GET` (and `HEAD`) `/terms`, `/privacy`,
//! `/pl/terms` and `/pl/privacy`.
//!
//! The documents live only on the host (`OUTER_LEGAL_DIR`, mounted
//! read-only at `/legal` in production), never in the repo, so the operator
//! stays unnamed in public. Each request reads its file fresh: host edits
//! go live without a rollout. A file that disappears at runtime answers 500
//! (logged) instead of falling through to the static site, so a half-written
//! legal dir can never serve the wrong page. Without the dir (localhost dev
//! without `OUTER_LEGAL_DIR`) the routes answer a short plain placeholder
//! with 200, so dev keeps working.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::path::{Path, PathBuf};

/// The host legal dir, or `None` when `OUTER_LEGAL_DIR` is unset.
#[derive(Clone, Debug)]
pub struct LegalDir {
    root: Option<PathBuf>,
}

impl LegalDir {
    /// Canonicalize `dir` once, like [`crate::http::site::SiteDir`]. A
    /// missing dir is not an error: every legal path then answers the
    /// localhost placeholder until the operator sets one.
    pub fn new(dir: Option<&Path>) -> Self {
        Self {
            root: dir.and_then(|dir| std::fs::canonicalize(dir).ok()),
        }
    }

    /// No legal dir configured: every path answers the placeholder.
    pub fn missing() -> Self {
        Self { root: None }
    }
}

/// One served page: its host file and its route (named in the 500 log when
/// the file disappears at runtime).
struct Page {
    file: &'static str,
    route: &'static str,
}

const TERMS: Page = Page {
    file: "terms.html",
    route: "/terms",
};
const PRIVACY: Page = Page {
    file: "privacy.html",
    route: "/privacy",
};
const TERMS_PL: Page = Page {
    file: "terms.pl.html",
    route: "/pl/terms",
};
const PRIVACY_PL: Page = Page {
    file: "privacy.pl.html",
    route: "/pl/privacy",
};

pub async fn serve_terms(State(state): State<super::AppState>) -> Response {
    respond(&state.legal, &TERMS).await
}

pub async fn serve_privacy(State(state): State<super::AppState>) -> Response {
    respond(&state.legal, &PRIVACY).await
}

pub async fn serve_terms_pl(State(state): State<super::AppState>) -> Response {
    respond(&state.legal, &TERMS_PL).await
}

pub async fn serve_privacy_pl(State(state): State<super::AppState>) -> Response {
    respond(&state.legal, &PRIVACY_PL).await
}

async fn respond(legal: &LegalDir, page: &Page) -> Response {
    let Some(root) = legal.root.as_deref() else {
        return (
            StatusCode::OK,
            [("content-type", "text/plain; charset=utf-8")],
            "legal pages not configured (OUTER_LEGAL_DIR)",
        )
            .into_response();
    };
    let bytes = match tokio::fs::read(root.join(page.file)).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::error!(
                route = page.route,
                file = page.file,
                error = %err,
                "legal page missing at runtime; answering 500 instead of the static site"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "text/plain; charset=utf-8")],
                "legal page unavailable",
            )
                .into_response();
        }
    };
    let mut res = (StatusCode::OK, bytes).into_response();
    let headers = res.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().unwrap(),
    );
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        crate::auth::pages::CSP.parse().unwrap(),
    );
    headers.insert(
        "x-content-type-options",
        "nosniff".parse().expect("static header parses"),
    );
    headers.insert(
        "referrer-policy",
        "same-origin".parse().expect("static header parses"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        "public, max-age=300".parse().unwrap(),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    /// The whole platform router over a temp database, a missing site dir
    /// and a temp legal dir (or none): the four pages serve their files
    /// verbatim, and the placeholder/missing-file paths never touch the
    /// static site.
    #[tokio::test]
    async fn legal_pages_serve_host_files_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&db.path().join("outer.sqlite"))
            .await
            .unwrap();
        let auth = crate::auth::test_support::test_state(pool.clone());
        let pages = [
            ("terms.html", "/terms", "<html><body>Terms EN</body></html>"),
            (
                "privacy.html",
                "/privacy",
                "<html><body>Privacy EN</body></html>",
            ),
            (
                "terms.pl.html",
                "/pl/terms",
                "<html><body>Terms PL</body></html>",
            ),
            (
                "privacy.pl.html",
                "/pl/privacy",
                "<html><body>Privacy PL</body></html>",
            ),
        ];
        for (file, _, body) in &pages {
            std::fs::write(dir.path().join(file), body).unwrap();
        }
        let app = super::super::router(super::super::AppState {
            pool: pool.clone(),
            sync: crate::store::snapshot::StateSync::new("test"),
            auth: auth.clone(),
            mcp: crate::mcp::McpState::for_tests(pool.clone(), auth),
            site: crate::http::site::SiteDir::new(std::path::Path::new("/nonexistent")),
            legal: LegalDir::new(Some(dir.path())),
        });
        for (_, route, body) in &pages {
            let res = app
                .clone()
                .oneshot(Request::get(*route).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{route}");
            assert_eq!(
                res.headers().get("content-type").unwrap(),
                "text/html; charset=utf-8",
                "{route}"
            );
            assert!(
                res.headers().contains_key("content-security-policy"),
                "{route}"
            );
            assert_eq!(
                res.headers().get("cache-control").unwrap(),
                "public, max-age=300",
                "{route}"
            );
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(&bytes[..], body.as_bytes(), "{route}");
        }
        // HEAD answers the same headers with no body.
        let res = app
            .clone()
            .oneshot(Request::head("/terms").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8",
        );

        // Edits go live without a rollout: the file is read per request.
        std::fs::write(
            dir.path().join("terms.html"),
            "<html><body>v2</body></html>",
        )
        .unwrap();
        let res = app
            .clone()
            .oneshot(Request::get("/terms").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"<html><body>v2</body></html>");

        // A file deleted at runtime is a 500, never the static site.
        std::fs::remove_file(dir.path().join("privacy.html")).unwrap();
        let res = app
            .clone()
            .oneshot(Request::get("/privacy").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // No legal dir at all (localhost dev): a short plain 200.
        let auth = crate::auth::test_support::test_state(pool.clone());
        let bare = super::super::router(super::super::AppState {
            pool: pool.clone(),
            sync: crate::store::snapshot::StateSync::new("test"),
            auth: auth.clone(),
            mcp: crate::mcp::McpState::for_tests(pool.clone(), auth),
            site: crate::http::site::SiteDir::new(std::path::Path::new("/nonexistent")),
            legal: LegalDir::missing(),
        });
        let res = bare
            .oneshot(Request::get("/terms").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"legal pages not configured (OUTER_LEGAL_DIR)");
    }
}
