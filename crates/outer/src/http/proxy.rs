//! Reverse proxy: `<slug>.<apps_domain>` → the app's fleet on loopback.
//!
//! A sleeping fleet wakes on its first request; the request then forwards
//! over HTTP/1 with a streaming body, WebSocket upgrades splice both halves,
//! and activity is recorded (throttled) for the sleep sweep.
//! Apps with no successful deploy get a 503 building page instead: a first
//! deploy uploads before its migrations apply, so its code must never be
//! forwarded until the migrate step succeeds.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::apps;
use crate::browser::MAX_LOOK_HTML_BYTES;
use crate::fleet::manager::FleetManager;
use crate::http::client_ip::{cf_connecting_ip, client_ip};
use crate::look_tokens::{LookTokens, LOOK_HEADER};
use crate::observe::metrics::{MetricsAgg, RequestRecord};

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
    /// Per-app request metrics (spec §13): owned here, flushed to SQLite by
    /// the sweep task. Cloning shares the same counters, never global.
    pub metrics: MetricsAgg,
    /// Single-use console-capture tokens `look_at_app` mints: only a live
    /// token for the request's own host arms the recorder injection below.
    /// Shared with the MCP state, never global.
    pub look_tokens: LookTokens,
}
impl ProxyState {
    /// Build the per-state upstream client and the per-state touch-throttle
    /// map alongside the state the integration owner already mounts.
    pub fn new(
        pool: sqlx::SqlitePool,
        fleets: FleetManager,
        apps_domain: String,
        origin_locked: bool,
        metrics: MetricsAgg,
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
            metrics,
            look_tokens: LookTokens::new(),
        }
    }

    /// Share one console-capture token table with the MCP state (the
    /// integration owner calls this once; tests that need tokens do too).
    pub fn with_look_tokens(mut self, look_tokens: LookTokens) -> Self {
        self.look_tokens = look_tokens;
        self
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

/// A 308 to the renamed app's hostname, preserving any dev port on the
/// authority and the request's path + query. The old hostname's quarantine
/// guarantees nobody else can take it while this answers.
fn redirect_to_slug(state: &ProxyState, raw_host: &str, target: &str, req: &Request) -> Response {
    let rest = raw_host
        .split_once('.')
        .map(|(_, rest)| rest)
        .unwrap_or(&state.apps_domain);
    let scheme = if state.origin_locked { "https" } else { "http" };
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let location = format!("{scheme}://{target}.{rest}{path}");
    Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header("location", location)
        .body(Body::from(format!("this app moved to {target}")))
        .unwrap()
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

/// Bytes the upstream claims in `Content-Length`, else 0. Chunked/streamed
/// bodies are never buffered to count them (see `observe::metrics`).
fn response_bytes(res: &hyper::Response<hyper::body::Incoming>) -> u64 {
    res.headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0)
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

/// Console capture for `look_at_app`: Cloudflare's REST render cannot stream
/// console logs, so when the navigation carries a live [`LOOK_HEADER`]
/// token for this host the proxy injects a tiny inline recorder as the
/// first child of `<head>` (or prepends it when the page has none). The
/// recorder mirrors `window.onerror`, `unhandledrejection`,
/// `console.error`/`console.warn`, resource load failures and failed
/// `fetch`/XHR into a `<script type="application/json" id="__outer_look">`
/// data element, which Cloudflare's returned HTML carries back to the tool.
/// Without a valid token this whole path is skipped and the response is
/// byte-for-byte what the upstream sent.
///
/// Skip markers (HTML comments, so no CSP can block them) tell the tool why
/// a tokened page carries no recorder. Only tokened requests ever gain one.
/// `csp` means an enforced policy blocks inline scripts; `oversize` means
/// the HTML exceeded [`MAX_LOOK_HTML_BYTES`].
const LOOK_SKIP_CSP: &[u8] = b"<!--__outer_look:csp-->";
const LOOK_SKIP_OVERSIZE: &[u8] = b"<!--__outer_look:oversize-->";

/// The page-side recorder: no external `src`, no dependencies. At most 50
/// console entries and 50 failed requests, each at most 500 chars, mirrored
/// into `#__outer_look` on every record so the serialized DOM Cloudflare
/// returns carries the log.
const LOOK_RECORDER_JS: &str = "!function(){var N=null,C=[],F=[];function T(s){s=String(s);return s.length>500?s.slice(0,500):s}function S(){try{if(!N)N=document.getElementById(\"__outer_look\");if(N)N.textContent=JSON.stringify({console:C,failed:F})}catch(e){}}function c(l,m,s){if(C.length>=50)return;var o={level:l,message:T(m)};if(s)o.source=T(s);C.push(o);S()}function f(u,st,er){if(F.length>=50)return;var o={url:T(u||\"(unknown)\")};if(st)o.status=st;else o.error=T(er||\"failed\");F.push(o);S()}window.addEventListener(\"error\",function(e){var t=e.target;if(t&&t!==window){f(t.src||t.href||\"\",0,\"load failed\");return}c(\"error\",e.message||\"script error\",e.filename)},true);window.addEventListener(\"unhandledrejection\",function(e){var r=e.reason,m=r&&r.stack?String(r.stack).split(\"\\n\")[0]:(r&&r.message?r.message:String(r));c(\"error\",\"unhandled rejection: \"+m)});[\"error\",\"warn\"].forEach(function(k){try{var o=console[k];console[k]=function(){try{c(k,Array.prototype.map.call(arguments,function(a){try{return typeof a===\"string\"?a:JSON.stringify(a)}catch(_){return String(a)}}).join(\" \"))}catch(_){}return o.apply(console,arguments)}}catch(_){}});if(window.fetch){var of=window.fetch;window.fetch=function(u,o){var s=typeof u===\"string\"?u:(u&&u.url)||\"\";return of(u,o).then(function(r){if(!r.ok)f(s,r.status,\"failed\");return r},function(e){f(s,0,\"fetch failed\");throw e})}}if(window.XMLHttpRequest){try{var op=XMLHttpRequest.prototype.open,sd=XMLHttpRequest.prototype.send;XMLHttpRequest.prototype.open=function(m,u){this._olu=u;return op.apply(this,arguments)};XMLHttpRequest.prototype.send=function(b){var x=this;try{x.addEventListener(\"loadend\",function(){try{var s=x.status;if((s<200||s>=300||s===0)&&x._olu!==undefined)f(x._olu,s,\"request failed\")}catch(_){}})}catch(_){}return sd.call(this,b)}}catch(_){}}S()}();";

/// The injected head: the recorder plus the data element it mirrors into.
fn look_snippet() -> Vec<u8> {
    let mut out = Vec::with_capacity(LOOK_RECORDER_JS.len() + 128);
    out.extend_from_slice(b"<script>");
    out.extend_from_slice(LOOK_RECORDER_JS.as_bytes());
    out.extend_from_slice(
        b"</script><script type=\"application/json\" id=\"__outer_look\">{\"console\":[],\"failed\":[]}</script>",
    );
    out
}

/// Insert the recorder as the first child of `<head>` (case-insensitive,
/// attributes allowed, never matching `<header>`), or prepend it when the
/// page has no head element.
fn inject_look_snippet(html: &[u8]) -> Vec<u8> {
    let snippet = look_snippet();
    let mut out = Vec::with_capacity(html.len() + snippet.len());
    if html.len() > 6 {
        let mut i = 0;
        while i + 5 < html.len() {
            let is_head = html[i] == b'<'
                && (html[i + 1] == b'h' || html[i + 1] == b'H')
                && (html[i + 2] == b'e' || html[i + 2] == b'E')
                && (html[i + 3] == b'a' || html[i + 3] == b'A')
                && (html[i + 4] == b'd' || html[i + 4] == b'D')
                && matches!(html[i + 5], b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/');
            if is_head {
                if let Some(rel) = html[i..].iter().position(|&byte| byte == b'>') {
                    let end = i + rel + 1;
                    out.extend_from_slice(&html[..end]);
                    out.extend_from_slice(&snippet);
                    out.extend_from_slice(&html[end..]);
                    return out;
                }
                break;
            }
            i += 1;
        }
    }
    out.extend_from_slice(&snippet);
    out.extend_from_slice(html);
    out
}

/// Whether any enforced content-security-policy on this response would block
/// the recorder's inline script, in which case injection is skipped. The
/// first of `script-src-elem`, `script-src`, `default-src` wins; without
/// `'unsafe-inline'` our script cannot run (no nonce or hash can cover a
/// proxy-injected script), so the tool reports `csp-blocked` instead.
fn csp_blocks_inline(headers: &HeaderMap) -> bool {
    headers
        .get_all("content-security-policy")
        .iter()
        .any(|value| value.to_str().is_ok_and(csp_policy_blocks_inline))
}

fn csp_policy_blocks_inline(policy: &str) -> bool {
    let mut script_src = None;
    let mut script_elem = None;
    let mut default_src = None;
    for directive in policy.split(';') {
        let mut words = directive.split_whitespace();
        match words.next().map(str::to_ascii_lowercase).as_deref() {
            Some("script-src-elem") => script_elem = Some(directive),
            Some("script-src") => script_src = Some(directive),
            Some("default-src") => default_src = Some(directive),
            _ => {}
        }
    }
    let Some(active) = script_elem.or(script_src).or(default_src) else {
        return false;
    };
    !active
        .split_whitespace()
        .skip(1)
        .any(|token| token == "'unsafe-inline'")
}

/// Copy end-to-end response headers onto a rebuilt look response: the same
/// hop-by-hop filtering as [`from_upstream`] (look responses never upgrade,
/// so no 101 pair is kept). `content-length` must already be removed by the
/// caller when the body changes; oversize streams set their own.
fn copy_response_headers(
    out: http::response::Builder,
    headers: &mut HeaderMap,
    is_unlisted: bool,
) -> http::response::Builder {
    let nominated = connection_tokens(headers);
    let mut rebuilt = out;
    let mut last_name: Option<HeaderName> = None;
    for (name, value) in headers.drain() {
        if let Some(name) = name {
            last_name = Some(name);
        }
        let name = last_name.clone().expect("header value without a name");
        if is_hop_by_hop(name.as_str()) || nominated.iter().any(|t| t == name.as_str()) {
            continue;
        }
        rebuilt = rebuilt.header(name, value);
    }
    if is_unlisted {
        rebuilt = rebuilt.header("x-robots-tag", "noindex, nofollow");
    }
    rebuilt
}

/// Rebuild a buffered look response: same end-to-end headers, fresh
/// `Content-Length` for the new body.
fn rebuild_look_body(mut parts: http::response::Parts, is_unlisted: bool, body: &[u8]) -> Response {
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    let out = copy_response_headers(
        Response::builder().status(parts.status),
        &mut parts.headers,
        is_unlisted,
    );
    out.header("content-length", body.len())
        .body(Body::from(body.to_vec()))
        .unwrap()
}

/// Prepend `prefix` (a skip marker, plus any bytes already read) to the
/// still-open remainder: the oversize path never holds a huge page fully in
/// memory. The caller drops `content-length` so the body chunks.
fn prepend_look_stream(
    prefix: Vec<u8>,
    rest: http_body_util::combinators::BoxBody<bytes::Bytes, hyper::Error>,
) -> Body {
    use futures::{StreamExt as _, TryStreamExt as _};
    let head = futures::stream::iter([Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(
        prefix,
    ))]);
    let tail = http_body_util::BodyDataStream::new(rest).map_err(std::io::Error::other);
    Body::from_stream(head.chain(tail))
}

/// Buffer the upstream body up to one byte past [`MAX_LOOK_HTML_BYTES`]:
/// the bytes plus `None` fit; the bytes plus `Some` overflowed and carry
/// the still-open remainder for the streaming oversize path. `Err` means
/// the upstream broke mid-body.
async fn read_look_body(
    body: hyper::body::Incoming,
) -> Result<
    (
        Vec<u8>,
        Option<http_body_util::combinators::BoxBody<bytes::Bytes, hyper::Error>>,
    ),
    hyper::Error,
> {
    use http_body_util::BodyExt as _;
    let mut body = body.boxed();
    let mut buf = Vec::new();
    loop {
        match body.frame().await {
            None => return Ok((buf, None)),
            Some(Err(err)) => return Err(err),
            Some(Ok(frame)) => match frame.into_data() {
                // Trailers carry no document bytes; the loop ends next.
                Err(_) => {}
                Ok(data) => {
                    buf.extend_from_slice(&data);
                    if buf.len() > MAX_LOOK_HTML_BYTES + 1 {
                        return Ok((buf, Some(body)));
                    }
                }
            },
        }
    }
}

/// The look-token path: buffer HTML (bounded), inject the recorder or a
/// skip marker, and fix `Content-Length`. Non-HTML and unexpectedly encoded
/// bodies pass through untouched with the normal header filtering; oversize
/// HTML streams with a marker prepended.
async fn look_response(
    upstream: hyper::Response<hyper::body::Incoming>,
    is_unlisted: bool,
) -> Response {
    let (mut parts, body) = upstream.into_parts();
    let is_html = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            content_type
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/html"))
        });
    // The tokened request asks for identity below; anything else here means
    // upstream encoded anyway, and encoded bytes must never be edited.
    let encoded = parts
        .headers
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|encoding| {
            !encoding.is_empty() && !encoding.trim().eq_ignore_ascii_case("identity")
        });
    if !is_html || encoded {
        return from_upstream(hyper::Response::from_parts(parts, body), is_unlisted);
    }
    // Everything below rewrites the page for one browser render. The edge
    // must never store it: Cloudflare's cache key ignores the look header,
    // so a cached copy would serve the recorder to real visitors.
    parts.headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store, private"),
    );
    let declared = parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.parse::<usize>().ok());
    if declared.is_some_and(|len| len > MAX_LOOK_HTML_BYTES) {
        // Oversize on paper: never buffer, stream the body with the marker
        // in front and a corrected length.
        use http_body_util::BodyExt as _;
        let total = declared.unwrap_or(0) + LOOK_SKIP_OVERSIZE.len();
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
        let out = copy_response_headers(
            Response::builder().status(parts.status),
            &mut parts.headers,
            is_unlisted,
        );
        return out
            .header("content-length", total)
            .body(prepend_look_stream(
                LOOK_SKIP_OVERSIZE.to_vec(),
                body.boxed(),
            ))
            .unwrap();
    }
    match read_look_body(body).await {
        Err(_) => text(StatusCode::BAD_GATEWAY, "app unavailable"),
        Ok((buf, None)) => {
            if csp_blocks_inline(&parts.headers) {
                let mut marked = LOOK_SKIP_CSP.to_vec();
                marked.extend_from_slice(&buf);
                rebuild_look_body(parts, is_unlisted, &marked)
            } else {
                rebuild_look_body(parts, is_unlisted, &inject_look_snippet(&buf))
            }
        }
        Ok((buf, Some(rest))) => {
            // Overflow discovered mid-read: marker, what was read, the rest.
            parts.headers.remove(axum::http::header::CONTENT_LENGTH);
            let out = copy_response_headers(
                Response::builder().status(parts.status),
                &mut parts.headers,
                is_unlisted,
            );
            let mut prefix = LOOK_SKIP_OVERSIZE.to_vec();
            prefix.extend_from_slice(&buf);
            out.body(prepend_look_stream(prefix, rest)).unwrap()
        }
    }
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

/// What a visitor sees on a custom domain that isn't serving yet: pending
/// (create the DNS records, then wait for validation) vs failed (fix DNS,
/// refresh in the console). Never proxied, never metered: the caller
/// returns before the metering closure exists.
fn domain_setup_page(host: &str, status: &str) -> Response {
    let body = if status == "failed" {
        format!(
            "this custom domain ({host}) failed validation — check its DNS records, \
             then refresh it in the console (Settings → Custom domain)"
        )
    } else {
        format!(
            "this custom domain ({host}) is verifying — create its CNAME and TXT records, \
             then wait a little and refresh"
        )
    };
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("retry-after", "30")
        .body(Body::from(body))
        .unwrap()
}

/// Route `<slug>.<apps_domain>` — and Pro custom hostnames from `app_domain`
/// (spec §7.7) — to the app's fleet on loopback. Only `active` custom
/// domains forward; a `pending`/`failed` one gets a setup page explaining
/// what to do at its DNS provider, and an unknown host gets a 404.
pub async fn handle(state: ProxyState, mut req: Request) -> Response {
    let Some(raw_host) = request_host(&req) else {
        return text(StatusCode::NOT_FOUND, "no such app");
    };
    let bare = strip_port(&raw_host);
    let app = match slug_for(bare, &state.apps_domain) {
        Some(slug) => match apps::get_by_slug(&state.pool, &slug).await {
            Ok(Some(app)) => app,
            Ok(None) => match apps::quarantine_lookup(&state.pool, &slug).await {
                // A renamed-away slug redirects only while its quarantine holds
                // (`until` in the future — the same live-set definition as
                // `prune_quarantine`); past expiry it 404s exactly like a
                // plain-deleted (or never-existing) slug.
                Ok(Some(quarantine)) => match quarantine.renamed_to {
                    Some(target) if quarantine.until > apps::now_ms() => {
                        return redirect_to_slug(&state, &raw_host, &target, &req);
                    }
                    _ => return text(StatusCode::NOT_FOUND, "no such app"),
                },
                Ok(None) => return text(StatusCode::NOT_FOUND, "no such app"),
                Err(err) => {
                    tracing::warn!(slug = %slug, error = %err, "quarantine lookup failed");
                    return text(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
                }
            },
            Err(err) => {
                tracing::warn!(slug = %slug, error = %err, "app lookup failed");
                return text(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
            }
        },
        // Not an app host: maybe a Pro custom domain.
        None => match crate::apps::domains::resolve_active(&state.pool, bare).await {
            Ok(Some(app)) => app,
            Ok(None) => {
                // A known-but-not-active domain gets a setup page; anything
                // else is exactly like a missing app host (spec §8.2).
                match crate::apps::domains::pending_status(&state.pool, bare).await {
                    Ok(Some(status)) => return domain_setup_page(bare, &status),
                    Ok(None) => return text(StatusCode::NOT_FOUND, "no such app"),
                    Err(err) => {
                        tracing::warn!(host = %bare, error = %format!("{err:#}"), "domain lookup failed");
                        return text(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
                    }
                }
            }
            Err(err) => {
                tracing::warn!(host = %bare, error = %format!("{err:#}"), "domain lookup failed");
                return text(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
            }
        },
    };
    // No successful deploy yet: never forward. A first deploy uploads before
    // its migrations apply, so a fleet can be awake on unmigrated code while
    // the migrate step is still running — forwarding would serve it. The
    // deploy marks `last_deploy_sha` only after the migrations succeed, so
    // this gate makes the upload-then-migrate order safe, without waking a
    // fleet for an app with nothing live.
    if app.last_deploy_sha.is_none() {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("retry-after", "2")
            .body(Body::from("this app is being built, try again"))
            .unwrap();
    }
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
        .map(|info| info.0);
    let upgrade = wants_upgrade(req.headers());
    let cf_ip = cf_connecting_ip(req.headers());
    let client_ip = client_ip(req.headers(), state.origin_locked, peer);
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
    // Outer's own traffic is never app usage: the celld health gate the
    // wake path probes directly is excluded even when it arrives here.
    let raw_path = parts.uri.path();
    let metered = raw_path != "/.well-known/celld/health";
    // Recorded iff forwarded: only requests that reach the fleet's port
    // (or fail connecting to it) count. Outer's synthetic answers above
    // return before this closure can run.
    let meter = |status: u16, latency_ms: u64, bytes_out: u64| {
        if !metered {
            return;
        }
        state.metrics.record(RequestRecord {
            now_ms: apps::now_ms(),
            app_id: &app.id,
            path: raw_path,
            status,
            latency_ms,
            bytes_out,
            client_ip: client_ip.as_deref(),
        });
    };
    let uri = format!("http://127.0.0.1:{}{path}", app.port);
    let mut outbound = hyper::Request::builder()
        .method(parts.method)
        .uri(uri)
        .version(axum::http::Version::HTTP_11);
    // Collected up front: every name this request's `Connection` nominates
    // is hop-by-hop for this hop, alongside the fixed list.
    let nominated = connection_tokens(&parts.headers);
    // Console capture: consume a live token for this host, if present. The
    // header never reaches upstream either way; a live token additionally
    // asks for an identity body so the recorder can be injected below.
    let look_armed = parts
        .headers
        .get(LOOK_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|token| state.look_tokens.claim(bare, token));
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
        // The console-capture token is ours: never forward it, valid or not.
        if name.as_str() == LOOK_HEADER {
            continue;
        }
        // With a live token the recorder needs plain HTML below, so the
        // client's encoding preference is replaced by identity.
        if look_armed && name == axum::http::header::ACCEPT_ENCODING {
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
    if let Some(ip) = client_ip.as_deref() {
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
    if look_armed {
        outbound = outbound.header(axum::http::header::ACCEPT_ENCODING, "identity");
    }
    let outbound = match outbound.body(body) {
        Ok(req) => req,
        Err(_) => return text(StatusCode::BAD_GATEWAY, "app unavailable"),
    };
    // Proxy-observed latency: time to response headers (or to the send
    // failing), excluding the fleet wake above.
    let upstream_start = Instant::now();
    let mut upstream = match state.client.request(outbound).await {
        Ok(res) => res,
        Err(err) => {
            tracing::warn!(slug = %app.slug, error = %err, "upstream connect failed");
            meter(
                StatusCode::BAD_GATEWAY.as_u16(),
                upstream_start.elapsed().as_millis() as u64,
                0,
            );
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
            meter(
                StatusCode::SWITCHING_PROTOCOLS.as_u16(),
                upstream_start.elapsed().as_millis() as u64,
                0,
            );
            return from_upstream(upstream, is_unlisted);
        }
    }
    if look_armed {
        // A 101 took the upgrade branch above, so this is a plain response:
        // buffer it bounded, inject the recorder or a skip marker, fix the
        // length. The body only ever changes on this tokened request.
        let status = upstream.status();
        let bytes_out = response_bytes(&upstream);
        let latency_ms = upstream_start.elapsed().as_millis() as u64;
        let response = look_response(upstream, is_unlisted).await;
        meter(status.as_u16(), latency_ms, bytes_out);
        return response;
    }
    let status = upstream.status();
    let bytes_out = response_bytes(&upstream);
    let latency_ms = upstream_start.elapsed().as_millis() as u64;
    let response = from_upstream(upstream, is_unlisted);
    meter(status.as_u16(), latency_ms, bytes_out);
    response
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
            .route(
                "/look-html",
                axum::routing::get(|req: Request| async move {
                    // Tells the test what the proxy forwarded: the encoding
                    // it asked for, and whether the capture token leaked.
                    let encoding = req
                        .headers()
                        .get(axum::http::header::ACCEPT_ENCODING)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    let leaked = req.headers().contains_key(LOOK_HEADER);
                    let page = format!(
                        "<html><head><title>T</title></head><body>hi<!--ae:{encoding} leaked:{leaked}--></body></html>"
                    );
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/html; charset=utf-8")
                        .body(Body::from(page))
                        .unwrap()
                }),
            )
            .route(
                "/look-csp",
                axum::routing::get(|| async {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/html")
                        .header("content-security-policy", "script-src 'self'")
                        .body(Body::from("<html><head></head><body>csp</body></html>"))
                        .unwrap()
                }),
            )
            .route(
                "/look-csp-ok",
                axum::routing::get(|| async {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/html")
                        .header(
                            "content-security-policy",
                            "script-src 'self' 'unsafe-inline'",
                        )
                        .body(Body::from("<html><head></head><body>csp</body></html>"))
                        .unwrap()
                }),
            )
            .route(
                "/look-big",
                axum::routing::get(|| async {
                    let mut page =
                        String::with_capacity(MAX_LOOK_HTML_BYTES + 1024);
                    page.push_str("<html><head></head><body>");
                    while page.len() < MAX_LOOK_HTML_BYTES + 1000 {
                        page.push('x');
                    }
                    page.push_str("</body></html>");
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/html")
                        .body(Body::from(page))
                        .unwrap()
                }),
            )
            .route(
                "/look-gzip",
                axum::routing::get(|| async {
                    // Opaque bytes under a content-encoding: the proxy must
                    // never edit encoded bodies, only pass them through.
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/html")
                        .header("content-encoding", "gzip")
                        .body(Body::from("opaque-not-really-gzip"))
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
        sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('a1', 'o@example.com', 0, 0, 0)")
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
        // Fixture apps are live: `last_deploy_sha` set, so the not-live
        // gate passes and requests forward. The new test below NULLs it to
        // pin the building-page behavior.
        sqlx::query(
        "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, last_activity_at, created_at, last_deploy_sha) \
         VALUES (?, ?, 'a1', (SELECT COALESCE(MAX(uid), 200000) + 1 FROM app), ?, ?, ?, 0, 1000, '0123456789abcdef0123456789abcdef01234567')",
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
            MetricsAgg::new(),
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
    async fn refuses_to_forward_before_first_success() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-new", "new", fleet.addr.port(), "public").await;
        // No successful deploy yet: the app is still being built.
        sqlx::query("UPDATE app SET last_deploy_sha = NULL WHERE id = 'app-new'")
            .execute(&fix.pool)
            .await
            .unwrap();
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
            MetricsAgg::new(),
        );
        let req = Request::builder()
            .uri("/items")
            .header("host", "new.outer.test")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            headers.get("retry-after").and_then(|v| v.to_str().ok()),
            Some("2")
        );
        assert_eq!(body, "this app is being built, try again");
        assert_eq!(
            fleet.hits.load(Ordering::SeqCst),
            0,
            "an unmigrated first deploy is never forwarded, so the wake never runs"
        );
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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
            MetricsAgg::new(),
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

    #[tokio::test]
    async fn forwarded_requests_count_for_that_app_only() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(
            &fix.pool,
            "app-metered",
            "metered",
            fleet.addr.port(),
            "public",
        )
        .await;
        // Idle rows only (never requested, so nothing wakes them): they
        // prove a flush attributes counts to exactly the hit app.
        insert_app(
            &fix.pool,
            "app-other",
            "other",
            fleet.addr.port() + 1,
            "public",
        )
        .await;
        insert_app(
            &fix.pool,
            "app-quiet2",
            "quiet2",
            fleet.addr.port() + 2,
            "unlisted",
        )
        .await;
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
            MetricsAgg::new(),
        );
        async fn get(state: &ProxyState, host: &str, uri: &str) -> (StatusCode, String) {
            let mut req = Request::builder()
                .uri(uri)
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo("9.9.9.9:1234".parse::<SocketAddr>().unwrap()));
            let (status, _, body) =
                body_text(router(state.clone()).oneshot(req).await.unwrap()).await;
            (status, body)
        }
        for _ in 0..3 {
            let (status, _) = get(&state, "metered.outer.test", "/items/42").await;
            assert_eq!(status, StatusCode::OK);
        }
        // Excluded from metrics: the celld health gate, the unlisted
        // robots override Outer serves itself, and unknown hosts.
        let (status, _) = get(&state, "metered.outer.test", "/.well-known/celld/health").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get(&state, "quiet2.outer.test", "/robots.txt").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get(&state, "ghost.outer.test", "/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        state.metrics.flush(&fix.pool).await.unwrap();
        let minutes: Vec<(String, i64, i64)> =
            sqlx::query_as("SELECT app_id, requests, visitors FROM metric_minute ORDER BY app_id")
                .fetch_all(&fix.pool)
                .await
                .unwrap();
        assert_eq!(
            minutes,
            [("app-metered".to_string(), 3, 1)],
            "three hits from one peer IP: 3 requests, 1 visitor; nothing else counts"
        );
        let paths: Vec<(String, i64)> =
            sqlx::query_as("SELECT path, requests FROM path_hour ORDER BY path")
                .fetch_all(&fix.pool)
                .await
                .unwrap();
        assert_eq!(paths, [("/items/:id".to_string(), 3)]);
    }

    #[tokio::test]
    async fn renamed_slug_redirects_to_the_new_host() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-new", "new", fleet.addr.port(), "public").await;
        let until = crate::apps::now_ms() + 30 * 24 * 60 * 60 * 1000;
        sqlx::query(
            "INSERT INTO slug_quarantine (slug, until, renamed_to) VALUES ('old', ?1, 'new')",
        )
        .bind(until)
        .execute(&fix.pool)
        .await
        .unwrap();
        // A plain-deleted slug carries no target and keeps its 404.
        sqlx::query(
            "INSERT INTO slug_quarantine (slug, until, renamed_to) VALUES ('gone', ?1, NULL)",
        )
        .bind(until)
        .execute(&fix.pool)
        .await
        .unwrap();
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
            MetricsAgg::new(),
        );
        async fn get(state: &ProxyState, host: &str, uri: &str) -> (StatusCode, HeaderMap, String) {
            let req = Request::builder()
                .uri(uri)
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            body_text(router(state.clone()).oneshot(req).await.unwrap()).await
        }
        // Path and query survive; the dev port on the authority survives.
        let (status, headers, _) = get(&state, "old.outer.test", "/items/42?page=2").await;
        assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            headers.get("location").and_then(|v| v.to_str().ok()),
            Some("http://new.outer.test/items/42?page=2")
        );
        let (status, headers, _) = get(&state, "old.outer.test:8080", "/").await;
        assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            headers.get("location").and_then(|v| v.to_str().ok()),
            Some("http://new.outer.test:8080/")
        );
        // Deleted and unknown slugs still 404.
        let (status, _, body) = get(&state, "gone.outer.test", "/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "no such app");
        let (status, _, _) = get(&state, "ghost.outer.test", "/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // The new hostname itself is untouched by the quarantine row.
        let (status, _, _) = get(&state, "new.outer.test", "/ok").await;
        assert_eq!(status, StatusCode::OK);
    }
    #[tokio::test]
    async fn expired_rename_quarantine_stops_redirecting() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-new", "new", fleet.addr.port(), "public").await;
        let live_until = crate::apps::now_ms() + 30 * 24 * 60 * 60 * 1000;
        sqlx::query(
            "INSERT INTO slug_quarantine (slug, until, renamed_to) VALUES ('old', ?1, 'new')",
        )
        .bind(live_until)
        .execute(&fix.pool)
        .await
        .unwrap();
        // Same shape, but the quarantine expired: the redirect must stop.
        sqlx::query(
            "INSERT INTO slug_quarantine (slug, until, renamed_to) VALUES ('stale', ?1, 'new')",
        )
        .bind(crate::apps::now_ms() - 1)
        .execute(&fix.pool)
        .await
        .unwrap();
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
            MetricsAgg::new(),
        );
        async fn get(state: &ProxyState, host: &str, uri: &str) -> (StatusCode, HeaderMap, String) {
            let req = Request::builder()
                .uri(uri)
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            body_text(router(state.clone()).oneshot(req).await.unwrap()).await
        }
        // Live quarantine: still redirects.
        let (status, headers, _) = get(&state, "old.outer.test", "/items/42").await;
        assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            headers.get("location").and_then(|v| v.to_str().ok()),
            Some("http://new.outer.test/items/42")
        );
        // Expired quarantine: 404s exactly like a deleted slug.
        let (status, _, body) = get(&state, "stale.outer.test", "/items/42").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "no such app");
    }

    #[tokio::test]
    async fn custom_domains_route_only_when_active() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-shop", "shop", fleet.addr.port(), "unlisted").await;
        async fn add(pool: &sqlx::SqlitePool, host: &str, status: &str) {
            sqlx::query(
                "INSERT INTO app_domain (hostname, app_id, cf_custom_hostname_id, status, created_at) \
                 VALUES (?1, 'app-shop', 'cf-1', ?2, 0)",
            )
            .bind(host)
            .bind(status)
            .execute(pool)
            .await
            .unwrap();
        }
        add(&fix.pool, "shop.example.com", "active").await;
        add(&fix.pool, "soon.example.com", "pending").await;
        add(&fix.pool, "dead.example.com", "failed").await;
        let state = ProxyState::new(
            fix.pool.clone(),
            manager(fix._dir.path()),
            "outer.test".to_string(),
            false,
            MetricsAgg::new(),
        );
        // Active: forwards to the fleet, with the unlisted robots header.
        let req = Request::builder()
            .uri("/items")
            .header("host", "shop.example.com")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) =
            body_text(router(state.clone()).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("x-robots-tag").and_then(|v| v.to_str().ok()),
            Some("noindex, nofollow"),
            "custom hosts keep the app's visibility headers"
        );
        assert_eq!(echo(&body)["path"], "/items");
        // Pending: a setup page, never the fleet.
        let req = Request::builder()
            .uri("/")
            .header("host", "soon.example.com")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = body_text(router(state.clone()).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("verifying"), "{body}");
        // Failed: names the fix and the console card.
        let req = Request::builder()
            .uri("/")
            .header("host", "dead.example.com")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = body_text(router(state.clone()).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("failed validation"), "{body}");
        // Unknown: exactly like a missing app host.
        let req = Request::builder()
            .uri("/")
            .header("host", "nope.example.com")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = body_text(router(state.clone()).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "no such app");
        assert_eq!(
            fleet.hits.load(Ordering::SeqCst),
            1,
            "only the active host forwards"
        );
    }
    /// One proxy state whose token table the test also holds: mint here,
    /// the proxy claims on the request.
    fn look_state(
        pool: sqlx::SqlitePool,
        dir: &std::path::Path,
        tokens: crate::look_tokens::LookTokens,
    ) -> ProxyState {
        ProxyState::new(
            pool,
            manager(dir),
            "outer.test".to_string(),
            false,
            MetricsAgg::new(),
        )
        .with_look_tokens(tokens)
    }

    fn look_html_request(token: Option<&str>) -> Request {
        let mut req = Request::builder()
            .uri("/look-html")
            .header("host", "shop.outer.test");
        if let Some(token) = token {
            req = req.header(LOOK_HEADER, token);
        }
        req.body(Body::empty()).unwrap()
    }

    /// A live token injects the recorder right after `<head>`, asks
    /// upstream for identity, and never leaks the token upstream.
    #[tokio::test]
    async fn look_token_injects_recorder_and_hides_token_upstream() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-shop", "shop", fleet.addr.port(), "public").await;
        let tokens = crate::look_tokens::LookTokens::new();
        let token = tokens.mint("shop.outer.test");
        let state = look_state(fix.pool.clone(), fix._dir.path(), tokens);

        let (status, headers, body) = body_text(
            router(state)
                .oneshot(look_html_request(Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let mut expected = String::from("<html><head>");
        expected.push_str(&String::from_utf8(look_snippet()).unwrap());
        expected.push_str(
            "<title>T</title></head><body>hi<!--ae:identity leaked:false--></body></html>",
        );
        assert_eq!(
            body, expected,
            "recorder injected, token stripped, identity asked"
        );
        assert_eq!(
            headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(
            headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|text| text.parse::<usize>().ok()),
            Some(body.len()),
            "content-length fixed for the injected body"
        );
        assert!(body.contains("id=\"__outer_look\""), "{body}");
        assert_eq!(
            headers.get("cache-control").and_then(|v| v.to_str().ok()),
            Some("no-store, private"),
            "the edge must never cache the recorder page"
        );
    }

    /// Without any token the proxy path is byte-for-byte what upstream
    /// sent: the recorder never runs for real visitors.
    #[tokio::test]
    async fn look_without_token_leaves_bytes_untouched() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-shop", "shop", fleet.addr.port(), "public").await;
        let state = look_state(
            fix.pool.clone(),
            fix._dir.path(),
            crate::look_tokens::LookTokens::new(),
        );

        let (status, headers, body) = body_text(
            router(state)
                .oneshot(look_html_request(None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            "<html><head><title>T</title></head><body>hi<!--ae: leaked:false--></body></html>"
        );
        assert_eq!(
            headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|text| text.parse::<usize>().ok()),
            Some(body.len())
        );
    }

    /// Tokens are single-use and host-bound: a replay or a token minted for
    /// another host injects nothing.
    #[tokio::test]
    async fn look_token_replays_and_foreign_hosts_inject_nothing() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-shop", "shop", fleet.addr.port(), "public").await;
        let tokens = crate::look_tokens::LookTokens::new();
        let token = tokens.mint("shop.outer.test");
        let foreign = tokens.mint("other.outer.test");
        let state = look_state(fix.pool.clone(), fix._dir.path(), tokens);
        let (_, _, first) = body_text(
            router(state.clone())
                .oneshot(look_html_request(Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            first.contains("id=\"__outer_look\""),
            "live token injects: {first}"
        );
        // Replay of the consumed token and a token minted for another host
        // both inject nothing.
        for attempt in [token, foreign] {
            let (_, _, body) = body_text(
                router(state.clone())
                    .oneshot(look_html_request(Some(&attempt)))
                    .await
                    .unwrap(),
            )
            .await;
            assert!(
                !body.contains("__outer_look"),
                "no recorder, no marker: {body}"
            );
        }
    }

    /// An expired token is exactly like no token: the response is the
    /// upstream page untouched.
    #[tokio::test]
    async fn look_expired_token_leaves_bytes_untouched() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-shop", "shop", fleet.addr.port(), "public").await;
        let tokens = crate::look_tokens::LookTokens::new();
        let stale = tokens.mint_expired("shop.outer.test");
        let state = look_state(fix.pool.clone(), fix._dir.path(), tokens);

        let (status, _, body) = body_text(
            router(state)
                .oneshot(look_html_request(Some(&stale)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            "<html><head><title>T</title></head><body>hi<!--ae: leaked:false--></body></html>"
        );
    }

    /// A CSP blocking inline scripts skips the recorder but leaves a marker
    /// the tool reports; `'unsafe-inline'` allows the injection.
    #[tokio::test]
    async fn look_csp_block_skips_with_marker_and_allowance_injects() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-shop", "shop", fleet.addr.port(), "public").await;
        let tokens = crate::look_tokens::LookTokens::new();
        let state = look_state(fix.pool.clone(), fix._dir.path(), tokens.clone());

        let blocked = tokens.mint("shop.outer.test");
        let req = Request::builder()
            .uri("/look-csp")
            .header("host", "shop.outer.test")
            .header(LOOK_HEADER, blocked)
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) =
            body_text(router(state.clone()).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("id=\"__outer_look\""), "no recorder: {body}");
        assert!(
            body.starts_with("<!--__outer_look:csp-->"),
            "skip marker first: {body}"
        );
        assert_eq!(
            headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|text| text.parse::<usize>().ok()),
            Some(body.len())
        );

        let allowed = tokens.mint("shop.outer.test");
        let req = Request::builder()
            .uri("/look-csp-ok")
            .header("host", "shop.outer.test")
            .header(LOOK_HEADER, allowed)
            .body(Body::empty())
            .unwrap();
        let (_, _, body) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert!(body.contains("id=\"__outer_look\""), "injected: {body}");
        assert!(!body.contains("__outer_look:csp"), "{body}");
    }

    /// An oversize page streams with the marker prepended instead of
    /// sitting fully in memory, and the tail still arrives intact.
    #[tokio::test]
    async fn look_oversize_page_streams_with_marker() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-shop", "shop", fleet.addr.port(), "public").await;
        let tokens = crate::look_tokens::LookTokens::new();
        let token = tokens.mint("shop.outer.test");
        let state = look_state(fix.pool.clone(), fix._dir.path(), tokens);

        let req = Request::builder()
            .uri("/look-big")
            .header("host", "shop.outer.test")
            .header(LOOK_HEADER, token)
            .body(Body::empty())
            .unwrap();
        // Past `body_text`'s 1 MiB helper cap: read the whole streamed page.
        let response = router(state).oneshot(req).await.unwrap();
        let (parts, body) = response.into_parts();
        let status = parts.status;
        let headers = parts.headers;
        let bytes = to_bytes(body, 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.starts_with("<!--__outer_look:oversize-->"),
            "marker first"
        );
        assert!(body.ends_with("</body></html>"), "tail intact");
        assert!(
            !body.contains("id=\"__outer_look\""),
            "no recorder: oversize"
        );
        assert!(
            body.len() > MAX_LOOK_HTML_BYTES,
            "page still whole: {} bytes",
            body.len()
        );
        assert_eq!(
            headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|text| text.parse::<usize>().ok()),
            Some(body.len())
        );
    }

    /// Encoded bodies and non-HTML responses pass through untouched with a
    /// live token: only plain HTML is ever edited.
    #[tokio::test]
    async fn look_encoded_and_non_html_bodies_pass_through() {
        let fix = fixture().await;
        let fleet = start_fake_fleet().await;
        insert_app(&fix.pool, "app-shop", "shop", fleet.addr.port(), "public").await;
        let tokens = crate::look_tokens::LookTokens::new();
        let state = look_state(fix.pool.clone(), fix._dir.path(), tokens.clone());

        let token = tokens.mint("shop.outer.test");
        let req = Request::builder()
            .uri("/look-gzip")
            .header("host", "shop.outer.test")
            .header(LOOK_HEADER, token)
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) =
            body_text(router(state.clone()).oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "opaque-not-really-gzip");
        assert!(!body.contains("__outer_look"), "{body}");
        assert_eq!(
            headers
                .get("content-encoding")
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );

        // The JSON echo fallback is not HTML: identical to the untokened
        // render of the same request shape.
        let token = tokens.mint("shop.outer.test");
        let req = Request::builder()
            .uri("/items?page=2")
            .header("host", "shop.outer.test")
            .header(LOOK_HEADER, token)
            .body(Body::empty())
            .unwrap();
        let (_, _, tokened) = body_text(router(state.clone()).oneshot(req).await.unwrap()).await;
        let req = Request::builder()
            .uri("/items?page=2")
            .header("host", "shop.outer.test")
            .header("accept-encoding", "identity")
            .body(Body::empty())
            .unwrap();
        let (_, _, plain) = body_text(router(state).oneshot(req).await.unwrap()).await;
        assert_eq!(tokened, plain, "non-HTML untouched");
        assert!(!tokened.contains("__outer_look"), "{tokened}");
    }
}
