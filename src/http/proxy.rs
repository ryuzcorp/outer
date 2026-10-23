//! Reverse proxy: `<slug>.<apps_domain>` → the app's fleet on loopback.
//!
//! A sleeping fleet wakes on its first request; the request then forwards
//! over HTTP/1 with a streaming body, WebSocket upgrades splice both halves,
//! and activity is recorded (throttled) for the sleep sweep.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::apps;
use crate::fleet::manager::FleetManager;

/// State the integration owner mounts as the fallback for every `Host`
/// other than the platform host.
#[derive(Clone)]
pub struct ProxyState {
    pub pool: sqlx::SqlitePool,
    pub fleets: FleetManager,
    pub apps_domain: String,
}

type UpstreamClient =
    hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, Body>;

/// One shared client: HTTP/1 only, idle pool entries recycled.
static CLIENT: LazyLock<UpstreamClient> = LazyLock::new(|| {
    let connector = hyper_util::client::legacy::connect::HttpConnector::new();
    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .build(connector)
});

/// Request headers are capped at 16 KiB in total; larger is 431.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// `touch_request` writes at most this often per app.
const TOUCH_EVERY_MS: i64 = 30_000;

static LAST_TOUCH_MS: LazyLock<Mutex<HashMap<String, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn text(status: StatusCode, body: &str) -> Response {
    (status, body.to_string()).into_response()
}

/// Strip a trailing `:port` off a bare hostname or IPv4 address. Bracketed
/// or unbracketed IPv6 literals are left alone: they never match an app.
fn strip_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((bare, port))
            if !bare.contains(':')
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            bare
        }
        _ => host,
    }
}

/// The single label in front of the apps domain, if the host is exactly
/// `<slug>.<apps_domain>`.
fn slug_for(host: &str, apps_domain: &str) -> Option<String> {
    let domain = apps_domain.trim().trim_matches('.').to_lowercase();
    let host = host.trim().trim_matches('.').to_lowercase();
    let slug = host.strip_suffix(&domain)?.strip_suffix('.')?;
    if slug.is_empty() || slug.contains('.') {
        return None;
    }
    Some(slug.to_string())
}

fn header_bytes(headers: &HeaderMap) -> usize {
    headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.len())
        .sum()
}

/// Hop-by-hop headers (RFC 9110 §7.6.1) plus the nonstandard
/// `proxy-connection`: stripped both ways so neither hyper side sees them.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn wants_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
}

async fn touch_once(pool: &sqlx::SqlitePool, app_id: &str) {
    let now = apps::now_ms();
    {
        let mut last = LAST_TOUCH_MS.lock().unwrap();
        match last.get(app_id) {
            Some(prev) if now.saturating_sub(*prev) < TOUCH_EVERY_MS => return,
            _ => {
                last.insert(app_id.to_string(), now);
            }
        }
    }
    if let Err(err) = apps::touch_request(pool, app_id, now).await {
        tracing::warn!(app = app_id, error = %err, "touch_request failed");
    }
}

/// Map an upstream response back onto the downstream connection: status,
/// end-to-end headers, streaming body. Hop-by-hop headers go both ways;
/// on a 101 the upgrade pair stays so the handshake completes.
fn from_upstream(res: hyper::Response<hyper::body::Incoming>, is_unlisted: bool) -> Response {
    let keep_upgrade = res.status() == StatusCode::SWITCHING_PROTOCOLS;
    let (mut parts, body) = res.into_parts();
    let mut out = Response::builder().status(parts.status);
    let mut last_name: Option<HeaderName> = None;
    for (name, value) in parts.headers.drain() {
        if let Some(name) = name {
            last_name = Some(name);
        }
        let name = last_name.clone().expect("header value without a name");
        if is_hop_by_hop(name.as_str())
            && !(keep_upgrade
                && (name == axum::http::header::UPGRADE || name == axum::http::header::CONNECTION))
        {
            continue;
        }
        out = out.header(name, value);
    }
    if is_unlisted {
        out = out.header("x-robots-tag", "noindex, nofollow");
    }
    out.body(Body::new(body)).unwrap()
}

/// Route `<slug>.<apps_domain>` to the app's fleet on loopback.
pub async fn handle(state: ProxyState, mut req: Request) -> Response {
    let Some(raw_host) = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        return text(StatusCode::NOT_FOUND, "no such app");
    };
    let Some(slug) = slug_for(strip_port(&raw_host), &state.apps_domain) else {
        return text(StatusCode::NOT_FOUND, "no such app");
    };
    let app = match apps::get_by_slug(&state.pool, &slug).await {
        Ok(Some(app)) => app,
        Ok(None) => return text(StatusCode::NOT_FOUND, "no such app"),
        Err(err) => {
            tracing::warn!(slug = %slug, error = %err, "app lookup failed");
            return text(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    if header_bytes(req.headers()) > MAX_HEADER_BYTES {
        return text(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "headers too large",
        );
    }
    let is_unlisted = app.visibility == apps::Visibility::Unlisted;
    if req.method() == axum::http::Method::GET && req.uri().path() == "/robots.txt" && is_unlisted {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain")
            .header("x-robots-tag", "noindex, nofollow")
            .body(Body::from("User-agent: *\nDisallow: /\n"))
            .unwrap();
    }
    if let Err(err) = state.fleets.ensure_awake(&state.pool, &app).await {
        tracing::warn!(slug = %app.slug, error = %err, "wake failed");
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("retry-after", "2")
            .body(Body::from("this app is starting, try again"))
            .unwrap();
    }
    touch_once(&state.pool, &app.id).await;

    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string());
    let upgrade = wants_upgrade(req.headers());
    // Grabbed before the request is split: the future resolves once the
    // handshake completes, after the 101 goes back downstream.
    let server_upgrade = if upgrade {
        Some(hyper::upgrade::on(&mut req))
    } else {
        None
    };
    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let uri = format!("http://127.0.0.1:{}{path}", app.port);
    let mut outbound = hyper::Request::builder()
        .method(parts.method)
        .uri(uri)
        .version(axum::http::Version::HTTP_11);
    for (name, value) in parts.headers.iter() {
        if *name == axum::http::header::HOST {
            continue;
        }
        if is_hop_by_hop(name.as_str())
            && !(upgrade
                && (*name == axum::http::header::UPGRADE
                    || *name == axum::http::header::CONNECTION))
        {
            continue;
        }
        outbound = outbound.header(name, value);
    }
    if let Some(peer) = peer {
        let prior = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .filter(|prev| !prev.is_empty());
        let value = match prior {
            Some(prev) => format!("{prev}, {peer}"),
            None => peer,
        };
        outbound = outbound.header("x-forwarded-for", value);
    }
    outbound = outbound
        .header("x-forwarded-host", raw_host)
        .header("x-forwarded-proto", "http");
    let outbound = match outbound.body(body) {
        Ok(req) => req,
        Err(_) => return text(StatusCode::BAD_GATEWAY, "app unavailable"),
    };
    let mut upstream = match CLIENT.request(outbound).await {
        Ok(res) => res,
        Err(err) => {
            tracing::warn!(slug = %app.slug, error = %err, "upstream connect failed");
            return text(StatusCode::BAD_GATEWAY, "app unavailable");
        }
    };
    if upgrade && upstream.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(server_upgrade) = server_upgrade {
            let client_upgrade = hyper::upgrade::on(&mut upstream);
            tokio::spawn(async move {
                let (server, client) = tokio::join!(server_upgrade, client_upgrade);
                let (Ok(server), Ok(client)) = (server, client) else {
                    return;
                };
                let mut server = hyper_util::rt::TokioIo::new(server);
                let mut client = hyper_util::rt::TokioIo::new(client);
                let _ = tokio::io::copy_bidirectional(&mut server, &mut client).await;
            });
            return from_upstream(upstream, is_unlisted);
        }
    }
    from_upstream(upstream, is_unlisted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use axum::body::to_bytes;
    use axum::routing::Router;
    use tower::ServiceExt;

    struct FakeFleet {
        addr: SocketAddr,
        hits: Arc<AtomicUsize>,
    }

    /// Tiny axum app as the fake fleet: healthy at the celld gate (so the
    /// manager's pre-start probe passes and no celld spawns), echoing the
    /// request as JSON everywhere else.
    async fn start_fake_fleet() -> FakeFleet {
        let hits = Arc::new(AtomicUsize::new(0));
        let echoed = hits.clone();
        let app = Router::new()
            .route(
                "/.well-known/celld/health",
                axum::routing::get(|| async { "ok" }),
            )
            .fallback(move |req: Request| {
                let echoed = echoed.clone();
                async move {
                    echoed.fetch_add(1, Ordering::SeqCst);
                    let (parts, body) = req.into_parts();
                    let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
                    let headers = parts
                        .headers
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.to_string(),
                                serde_json::Value::String(value.to_str().unwrap_or("").to_string()),
                            )
                        })
                        .collect::<serde_json::Map<String, serde_json::Value>>();
                    let payload = serde_json::json!({
                        "method": parts.method.to_string(),
                        "path": parts.uri.path(),
                        "query": parts.uri.query().unwrap_or(""),
                        "headers": headers,
                        "body": String::from_utf8_lossy(&bytes).into_owned(),
                    })
                    .to_string();
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .header("connection", "close")
                        .body(Body::from(payload))
                        .unwrap()
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakeFleet { addr, hits }
    }

    struct Fixture {
        pool: sqlx::SqlitePool,
        _dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query("INSERT INTO account (id, email, last_active_at, created_at) VALUES ('a1', 'o@example.com', 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        Fixture { pool, _dir: dir }
    }

    async fn insert_app(
        pool: &sqlx::SqlitePool,
        id: &str,
        slug: &str,
        port: u16,
        visibility: &str,
    ) {
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, last_activity_at, created_at) \
             VALUES (?, ?, 'a1', (SELECT COALESCE(MAX(uid), 200000) + 1 FROM app), ?, ?, ?, 0, 1000)",
        )
        .bind(id)
        .bind(slug)
        .bind(i64::from(port))
        .bind(i64::from(port) + 10000)
        .bind(visibility)
        .execute(pool)
        .await
        .unwrap();
    }

    fn manager(data: &std::path::Path) -> FleetManager {
        FleetManager::new(crate::fleet::manager::ManagerSettings {
            data_dir: data.to_path_buf(),
            celld_bin: "celld".to_string(),
            storage: crate::fleet::manager::FleetStorage {
                endpoint: "https://example.com".to_string(),
                region: "auto".to_string(),
                bucket: "outer-dev".to_string(),
                access_key_id: "key".to_string(),
                secret_access_key: "secret".to_string(),
                session_token: None,
            },
            cgroups: None,
            memory_max_bytes: 0,
            cpu_max: "50000 100000".to_string(),
            sleep_after: Duration::from_secs(60),
            wake_timeout: Duration::from_secs(5),
            wake_concurrency: 4,
        })
    }

    fn router(state: ProxyState) -> Router {
        Router::new().fallback(move |req: Request| handle(state.clone(), req))
    }

    async fn body_text(res: Response) -> (StatusCode, HeaderMap, String) {
        let (parts, body) = res.into_parts();
        let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
        (
            parts.status,
            parts.headers,
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }

    fn echo(body: &str) -> serde_json::Value {
        serde_json::from_str(body).unwrap()
    }

    #[tokio::test]
    async fn host_routing_rejects_foreign_hosts_and_unknown_slugs() {
        let fix = fixture().await;
        let state = ProxyState {
            pool: fix.pool.clone(),
            fleets: manager(fix._dir.path()),
            apps_domain: "outer.test".to_string(),
        };
        for host in [
            "shop.other.com",
            "outer.test",
            "a.b.outer.test",
            ".outer.test",
        ] {
            let req = Request::builder()
                .uri("/")
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            let (status, _, body) =
                body_text(router(state.clone()).oneshot(req).await.unwrap()).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "host {host}");
            assert_eq!(body, "no such app");
        }
        let req = Request::builder()
            .uri("/")
            .header("host", "ghost.outer.test")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn forwards_method_path_query_and_streaming_body() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-shop", "shop", fleet.addr.port(), "public").await;
        let state = ProxyState {
            pool: fix.pool.clone(),
            fleets: manager(fix._dir.path()),
            apps_domain: "outer.test".to_string(),
        };
        let req = Request::builder()
            .method("POST")
            .uri("/items?page=2")
            .header("host", "shop.outer.test:3473")
            .header("x-ff-test", "1")
            .body(Body::from("hello"))
            .unwrap();
        let (status, _, body) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let got = echo(&body);
        assert_eq!(got["method"], "POST");
        assert_eq!(got["path"], "/items");
        assert_eq!(got["query"], "page=2");
        assert_eq!(got["body"], "hello");
        assert_eq!(got["headers"]["x-ff-test"], "1");
        assert_eq!(got["headers"]["x-forwarded-host"], "shop.outer.test:3473");
        assert_eq!(got["headers"]["x-forwarded-proto"], "http");
    }

    #[tokio::test]
    async fn unlisted_apps_get_robots_tag_and_override() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(
            &fix.pool,
            "app-quiet",
            "quiet",
            fleet.addr.port(),
            "unlisted",
        )
        .await;
        let state = ProxyState {
            pool: fix.pool.clone(),
            fleets: manager(fix._dir.path()),
            apps_domain: "outer.test".to_string(),
        };
        let before = fleet.hits.load(Ordering::SeqCst);
        let req = Request::builder()
            .uri("/robots.txt")
            .header("host", "quiet.outer.test")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) =
            body_text(router(state.clone()).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "User-agent: *\nDisallow: /\n");
        assert_eq!(headers.get("x-robots-tag").unwrap(), "noindex, nofollow");
        assert_eq!(
            fleet.hits.load(Ordering::SeqCst),
            before,
            "robots override must not hit the app"
        );
        let req = Request::builder()
            .uri("/")
            .header("host", "quiet.outer.test")
            .body(Body::empty())
            .unwrap();
        let (_, headers, _) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(headers.get("x-robots-tag").unwrap(), "noindex, nofollow");
    }

    #[tokio::test]
    async fn public_apps_pass_robots_through_untagged() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-open", "open", fleet.addr.port(), "public").await;
        let state = ProxyState {
            pool: fix.pool.clone(),
            fleets: manager(fix._dir.path()),
            apps_domain: "outer.test".to_string(),
        };
        let req = Request::builder()
            .uri("/robots.txt")
            .header("host", "open.outer.test")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get("x-robots-tag").is_none());
        let got = echo(&body);
        assert_eq!(got["path"], "/robots.txt", "public robots reach the app");
    }

    #[tokio::test]
    async fn hop_by_hop_headers_are_stripped_both_ways() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-hop", "hop", fleet.addr.port(), "public").await;
        let state = ProxyState {
            pool: fix.pool.clone(),
            fleets: manager(fix._dir.path()),
            apps_domain: "outer.test".to_string(),
        };
        let req = Request::builder()
            .uri("/")
            .header("host", "hop.outer.test")
            .header("connection", "keep-alive")
            .header("keep-alive", "timeout=5")
            .header("te", "trailers")
            .header("trailer", "x-t")
            .header("proxy-authorization", "Basic eA==")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let got = echo(&body);
        for name in [
            "connection",
            "keep-alive",
            "te",
            "trailer",
            "proxy-authorization",
        ] {
            assert!(
                got["headers"].get(name).is_none(),
                "{name} must not reach the app"
            );
        }
        // The fake fleet answers `connection: close`; it must not leak back.
        assert!(headers.get("connection").is_none());
    }

    #[tokio::test]
    async fn touch_request_records_activity() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-touch", "touch", fleet.addr.port(), "public").await;
        let state = ProxyState {
            pool: fix.pool.clone(),
            fleets: manager(fix._dir.path()),
            apps_domain: "outer.test".to_string(),
        };
        let (before,): (Option<i64>,) =
            sqlx::query_as("SELECT last_request_at FROM app WHERE id = 'app-touch'")
                .fetch_one(&fix.pool)
                .await
                .unwrap();
        assert_eq!(before, None);
        let req = Request::builder()
            .uri("/")
            .header("host", "touch.outer.test")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let (after,): (Option<i64>,) =
            sqlx::query_as("SELECT last_request_at FROM app WHERE id = 'app-touch'")
                .fetch_one(&fix.pool)
                .await
                .unwrap();
        assert!(after.is_some(), "touch_request must write last_request_at");
    }

    #[tokio::test]
    async fn oversized_headers_are_rejected() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-big", "big", fleet.addr.port(), "public").await;
        let state = ProxyState {
            pool: fix.pool.clone(),
            fleets: manager(fix._dir.path()),
            apps_domain: "outer.test".to_string(),
        };
        let req = Request::builder()
            .uri("/")
            .header("host", "big.outer.test")
            .header("x-pad", "p".repeat(20 * 1024))
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
    }

    #[tokio::test]
    async fn failed_wake_is_a_503_with_retry_after() {
        let fix = fixture().await;
        // A closed loopback port: nothing answers, and the binary does not
        // exist, so the wake fails without touching the network.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        insert_app(&fix.pool, "app-down", "down", closed, "public").await;
        let state = ProxyState {
            pool: fix.pool.clone(),
            fleets: FleetManager::new(crate::fleet::manager::ManagerSettings {
                data_dir: fix._dir.path().to_path_buf(),
                celld_bin: "/nonexistent/celld".to_string(),
                storage: crate::fleet::manager::FleetStorage {
                    endpoint: "https://example.com".to_string(),
                    region: "auto".to_string(),
                    bucket: "outer-dev".to_string(),
                    access_key_id: "key".to_string(),
                    secret_access_key: "secret".to_string(),
                    session_token: None,
                },
                cgroups: None,
                memory_max_bytes: 0,
                cpu_max: "50000 100000".to_string(),
                sleep_after: Duration::from_secs(60),
                wake_timeout: Duration::from_secs(2),
                wake_concurrency: 4,
            }),
            apps_domain: "outer.test".to_string(),
        };
        let req = Request::builder()
            .uri("/")
            .header("host", "down.outer.test")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(headers.get("retry-after").unwrap(), "2");
        assert_eq!(body, "this app is starting, try again");
    }
}
