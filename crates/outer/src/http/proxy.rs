//! Reverse proxy: `<slug>.<apps_domain>` → the app's fleet on loopback.
//!
//! A sleeping fleet wakes on its first request; the request then forwards
//! over HTTP/1 with a streaming body, WebSocket upgrades splice both halves,
//! and activity is recorded (throttled) for the sleep sweep.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
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
    /// Behind Cloudflare origin pull (TLS + required client cert): trust the
    /// `CF-Connecting-IP` header Cloudflare attests and report `https`.
    /// Otherwise the header is spoofable and the peer address is used.
    pub origin_locked: bool,
    /// Upstream HTTP/1 client: idle pool entries recycled. Per state, never
    /// global, so tests cannot share connections or throttle entries.
    pub client: UpstreamClient,
    /// Last `touch_request` write per app: `touch_request` writes at most
    /// [`TOUCH_EVERY_MS`] often per app.
    pub last_touch_ms: Arc<Mutex<HashMap<String, i64>>>,
}

impl ProxyState {
    /// Build the per-state upstream client and the per-state touch-throttle
    /// map alongside the state the integration owner already mounts.
    pub fn new(
        pool: sqlx::SqlitePool,
        fleets: FleetManager,
        apps_domain: String,
        origin_locked: bool,
    ) -> Self {
        let connector = hyper_util::client::legacy::connect::HttpConnector::new();
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .pool_idle_timeout(Duration::from_secs(90))
                .build(connector);
        Self {
            pool,
            fleets,
            apps_domain,
            origin_locked,
            client,
            last_touch_ms: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

type UpstreamClient =
    hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, Body>;

/// `touch_request` writes at most this often per app.
const TOUCH_EVERY_MS: i64 = 30_000;
/// Requests whose headers total more than this are refused with 431.
const MAX_HEADER_BYTES: usize = 16 * 1024;

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

/// Header names nominated by `Connection` (RFC 9110 §7.6.1): comma-separated,
/// case-insensitive (`Connection: keep-alive, x-internal` nominates both).
/// Stripped alongside the fixed list in both directions.
fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(axum::http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

/// Client-supplied forwarding headers: the proxy sets its own values below,
/// so anything the client sent is dropped. Otherwise upstream sees two
/// values with the client's first (a spoofed scheme, host or client IP).
fn is_forwarded_spoof(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "x-forwarded-for" | "x-forwarded-host" | "x-forwarded-proto" | "forwarded"
    )
}

/// The client IP Cloudflare attests in `CF-Connecting-IP`, when the header
/// holds a single valid IP. Only trustworthy behind origin pull: otherwise
/// the header is spoofable and callers must use the peer address instead.
fn cf_connecting_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("cf-connecting-ip")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|ip| !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok())
        .map(str::to_string)
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

async fn touch_once(state: &ProxyState, app_id: &str) {
    let now = apps::now_ms();
    {
        let mut last = state.last_touch_ms.lock().unwrap();
        match last.get(app_id) {
            Some(prev) if now.saturating_sub(*prev) < TOUCH_EVERY_MS => return,
            _ => {
                last.insert(app_id.to_string(), now);
            }
        }
    }
    if let Err(err) = apps::touch_request(&state.pool, app_id, now).await {
        tracing::warn!(app = app_id, error = %err, "touch_request failed");
    }
}

/// Map an upstream response back onto the downstream connection: status,
/// end-to-end headers, streaming body. Hop-by-hop headers go both ways,
/// including any the upstream nominates through `Connection`;
/// on a 101 the upgrade pair stays so the handshake completes.
fn from_upstream(res: hyper::Response<hyper::body::Incoming>, is_unlisted: bool) -> Response {
    let keep_upgrade = res.status() == StatusCode::SWITCHING_PROTOCOLS;
    let (mut parts, body) = res.into_parts();
    // Collected before the drain: every name this response's `Connection`
    // nominates goes with the fixed hop-by-hop list.
    let nominated = connection_tokens(&parts.headers);
    let mut out = Response::builder().status(parts.status);
    let mut last_name: Option<HeaderName> = None;
    for (name, value) in parts.headers.drain() {
        if let Some(name) = name {
            last_name = Some(name);
        }
        let name = last_name.clone().expect("header value without a name");
        if (is_hop_by_hop(name.as_str()) || nominated.iter().any(|t| t == name.as_str()))
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

/// The host a request is for: the `Host` header (HTTP/1.1), else the URI
/// authority, which is where HTTP/2 carries it (`:authority`, no `Host`
/// header). Cloudflare speaks HTTP/2 to the origin.
pub fn request_host(req: &Request) -> Option<String> {
    req.headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_owned()))
}

/// Route `<slug>.<apps_domain>` to the app's fleet on loopback.
pub async fn handle(state: ProxyState, mut req: Request) -> Response {
    let Some(raw_host) = request_host(&req) else {
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
    touch_once(&state, &app.id).await;

    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string());
    // Origin-locked (behind Cloudflare origin pull): the `CF-Connecting-IP`
    // header Cloudflare attests is the client IP. Otherwise it is spoofable
    // and the peer address is used; an invalid value falls back to the peer.
    let upgrade = wants_upgrade(req.headers());
    let cf_ip = cf_connecting_ip(req.headers());
    let client_ip = if state.origin_locked {
        cf_ip.clone().or(peer.clone())
    } else {
        peer.clone()
    };
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
    // Collected up front: every name this request's `Connection` nominates
    // is hop-by-hop for this hop, alongside the fixed list.
    let nominated = connection_tokens(&parts.headers);
    for (name, value) in parts.headers.iter() {
        if *name == axum::http::header::HOST {
            continue;
        }
        if is_forwarded_spoof(name) {
            continue;
        }
        // Spoofable off origin pull, attested on it (re-added below only
        // when valid): never forward what the client sent.
        if name.as_str() == "cf-connecting-ip" {
            continue;
        }
        if (is_hop_by_hop(name.as_str()) || nominated.iter().any(|t| t == name.as_str()))
            && !(upgrade
                && (*name == axum::http::header::UPGRADE
                    || *name == axum::http::header::CONNECTION))
        {
            continue;
        }
        outbound = outbound.header(name, value);
    }
    // Fresh chain: client-supplied `x-forwarded-for` was dropped above, so
    // upstream sees exactly one value: the attested client IP when
    // origin-locked, else the real peer IP.
    if let Some(ip) = client_ip {
        outbound = outbound.header("x-forwarded-for", ip);
    }
    if state.origin_locked {
        if let Some(cf) = cf_ip {
            outbound = outbound.header("cf-connecting-ip", cf);
        }
    }
    outbound = outbound.header("x-forwarded-host", raw_host).header(
        "x-forwarded-proto",
        if state.origin_locked { "https" } else { "http" },
    );
    let outbound = match outbound.body(body) {
        Ok(req) => req,
        Err(_) => return text(StatusCode::BAD_GATEWAY, "app unavailable"),
    };
    let mut upstream = match state.client.request(outbound).await {
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
            .route(
                "/nominated",
                axum::routing::get(|| async {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/plain")
                        .header("connection", "x-resp-secret")
                        .header("x-resp-secret", "leak")
                        .body(Body::from("nominated"))
                        .unwrap()
                }),
            )
            .fallback(move |req: Request| {
                let echoed = echoed.clone();
                async move {
                    echoed.fetch_add(1, Ordering::SeqCst);
                    let (parts, body) = req.into_parts();
                    let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
                    // Joined per name: a proxied duplicate (e.g. a spoofed
                    // `x-forwarded-proto` next to the proxy's own) stays
                    // visible instead of collapsing last-wins.
                    let headers = parts
                        .headers
                        .keys()
                        .map(|name| {
                            let joined = parts
                                .headers
                                .get_all(name)
                                .iter()
                                .map(|value| value.to_str().unwrap_or(""))
                                .collect::<Vec<_>>()
                                .join(", ");
                            (name.to_string(), serde_json::Value::String(joined))
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
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: "key".to_string(),
                    secret_access_key: "secret".to_string(),
                },
            },
            cgroups: None,
            memory_max_bytes: 0,
            cpu_max: "50000 100000".to_string(),
            sleep_after: Duration::from_secs(60),
            credential_refresh_after: Duration::from_secs(6 * 86400),
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

    /// HTTP/2 (Cloudflare to the origin) has no `Host` header: the host
    /// comes from the URI authority, both for app routing and the platform
    /// split.
    #[test]
    fn http2_requests_take_the_host_from_the_authority() {
        let h2 = Request::builder()
            .version(axum::http::Version::HTTP_2)
            .uri("https://shop.outer.test/items?page=2")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_host(&h2).as_deref(), Some("shop.outer.test"));
        let h1 = Request::builder()
            .uri("/items")
            .header("host", "shop.outer.test:8443")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_host(&h1).as_deref(), Some("shop.outer.test:8443"));
        let none = Request::builder().uri("/").body(Body::empty()).unwrap();
        assert_eq!(request_host(&none), None);
    }

    #[tokio::test]
    async fn host_routing_rejects_foreign_hosts_and_unknown_slugs() {
        let fix = fixture().await;
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
        );
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
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
        );
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
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
        );
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
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
        );
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
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
        );
        let req = Request::builder()
            .uri("/")
            .header("host", "hop.outer.test")
            .header("connection", "keep-alive, x-internal")
            .header("keep-alive", "timeout=5")
            .header("x-internal", "1")
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
            // Nominated by `Connection`, not on the fixed list.
            "x-internal",
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
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
        );
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
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
        );
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
        let state = ProxyState::new(
            fix.pool.clone(),
            FleetManager::new(crate::fleet::manager::ManagerSettings {
                data_dir: fix._dir.path().to_path_buf(),
                celld_bin: "/nonexistent/celld".to_string(),
                storage: crate::fleet::manager::FleetStorage {
                    endpoint: "https://example.com".to_string(),
                    region: "auto".to_string(),
                    bucket: "outer-dev".to_string(),
                    credentials: crate::store::credentials::CredentialSource::Static {
                        access_key_id: "key".to_string(),
                        secret_access_key: "secret".to_string(),
                    },
                },
                cgroups: None,
                memory_max_bytes: 0,
                cpu_max: "50000 100000".to_string(),
                sleep_after: Duration::from_secs(60),
                credential_refresh_after: Duration::from_secs(6 * 86400),
                wake_timeout: Duration::from_secs(2),
                wake_concurrency: 4,
            }),
            "outer.test".to_string(),
            false,
        );
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

    #[test]
    fn connection_tokens_split_comma_separated_names_case_insensitively() {
        let mut headers = HeaderMap::new();
        let first: axum::http::HeaderValue = "Keep-Alive, X-Internal ".parse().unwrap();
        headers.insert("connection", first);
        let second: axum::http::HeaderValue = "x-extra".parse().unwrap();
        headers.append("connection", second);
        assert_eq!(
            connection_tokens(&headers),
            vec!["keep-alive", "x-internal", "x-extra"]
        );
        assert!(connection_tokens(&HeaderMap::new()).is_empty());
    }

    #[tokio::test]
    async fn connection_nominated_response_headers_are_stripped() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-nom", "nom", fleet.addr.port(), "public").await;
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
        );
        let req = Request::builder()
            .uri("/nominated")
            .header("host", "nom.outer.test")
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        // The fake fleet answers `connection: x-resp-secret` alongside the
        // nominated header; neither may leak downstream.
        assert!(headers.get("connection").is_none());
        assert!(
            headers.get("x-resp-secret").is_none(),
            "a Connection-nominated response header must not reach the client"
        );
    }

    #[tokio::test]
    async fn client_forwarded_headers_never_reach_upstream() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-fwd", "fwd", fleet.addr.port(), "public").await;
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
        );
        let mut req = Request::builder()
            .uri("/")
            .header("host", "fwd.outer.test")
            .header("x-forwarded-for", "evil")
            .header("x-forwarded-host", "evil.example")
            .header("x-forwarded-proto", "https")
            .header("forwarded", "for=1.2.3.4")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("9.9.9.9:1234".parse::<SocketAddr>().unwrap()));
        let (status, _, body) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let got = echo(&body);
        assert_eq!(
            got["headers"]["x-forwarded-for"], "9.9.9.9",
            "only the real peer IP reaches upstream, never the client's value"
        );
        assert_eq!(got["headers"]["x-forwarded-proto"], "http");
        assert_eq!(got["headers"]["x-forwarded-host"], "fwd.outer.test");
        assert!(
            got["headers"].get("forwarded").is_none(),
            "a client-sent forwarded header must not reach upstream"
        );
    }

    #[tokio::test]
    async fn origin_locked_uses_cf_connecting_ip() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-cf", "cf", fleet.addr.port(), "public").await;
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            true,
        );
        let mut req = Request::builder()
            .uri("/")
            .header("host", "cf.outer.test")
            .header("cf-connecting-ip", "203.0.113.7")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("9.9.9.9:1234".parse::<SocketAddr>().unwrap()));
        let (status, _, body) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let got = echo(&body);
        assert_eq!(got["headers"]["x-forwarded-for"], "203.0.113.7");
        assert_eq!(got["headers"]["cf-connecting-ip"], "203.0.113.7");
        assert_eq!(got["headers"]["x-forwarded-proto"], "https");
    }

    #[tokio::test]
    async fn unlocked_strips_spoofable_cf_connecting_ip() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-plain", "plain", fleet.addr.port(), "public").await;
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
        );
        let mut req = Request::builder()
            .uri("/")
            .header("host", "plain.outer.test")
            .header("cf-connecting-ip", "203.0.113.7")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("9.9.9.9:1234".parse::<SocketAddr>().unwrap()));
        let (status, _, body) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let got = echo(&body);
        assert_eq!(
            got["headers"]["x-forwarded-for"], "9.9.9.9",
            "off origin pull the peer IP wins, never the spoofable header"
        );
        assert!(
            got["headers"].get("cf-connecting-ip").is_none(),
            "a client-sent cf-connecting-ip must not reach upstream when unlocked"
        );
        assert_eq!(got["headers"]["x-forwarded-proto"], "http");
    }

    #[tokio::test]
    async fn invalid_cf_connecting_ip_falls_back_to_peer() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-badcf", "badcf", fleet.addr.port(), "public").await;
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            true,
        );
        let mut req = Request::builder()
            .uri("/")
            .header("host", "badcf.outer.test")
            .header("cf-connecting-ip", "not-an-ip")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("9.9.9.9:1234".parse::<SocketAddr>().unwrap()));
        let (status, _, body) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let got = echo(&body);
        assert_eq!(got["headers"]["x-forwarded-for"], "9.9.9.9");
        assert!(
            got["headers"].get("cf-connecting-ip").is_none(),
            "an invalid cf-connecting-ip must not reach upstream"
        );
    }
}
