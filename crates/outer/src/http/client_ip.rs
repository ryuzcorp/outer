//! The client IP every rate limit keys on.
//!
//! Behind Cloudflare origin pull (TLS + required client certificate) the
//! `CF-Connecting-IP` header Cloudflare attests is the client IP. Anywhere
//! else the header is spoofable and the TCP peer address is used instead, so
//! one egress range is never treated as one client beyond the per-IP limits
//! the spec gives.

use std::net::SocketAddr;

use axum::http::HeaderMap;

/// The client IP Cloudflare attests in `CF-Connecting-IP`, when the header
/// holds a single valid IP. Only trustworthy behind origin pull: otherwise
/// the header is spoofable and callers must use the peer address instead.
pub fn cf_connecting_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("cf-connecting-ip")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|ip| !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok())
        .map(str::to_string)
}

/// The rate-limit key for one request: the attested header when
/// `origin_locked`, else the TCP peer. An invalid header falls back to the
/// peer, and `None` means neither exists (no limit can key on it).
pub fn client_ip(
    headers: &HeaderMap,
    origin_locked: bool,
    peer: Option<SocketAddr>,
) -> Option<String> {
    let peer = peer.map(|addr| addr.ip().to_string());
    if origin_locked {
        cf_connecting_ip(headers).or(peer)
    } else {
        peer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_cf(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", value.parse().unwrap());
        headers
    }

    #[test]
    fn origin_locked_trusts_the_attested_header() {
        let peer = "127.0.0.1:1".parse().ok();
        assert_eq!(
            client_ip(&headers_with_cf("203.0.113.7"), true, peer),
            Some("203.0.113.7".to_string())
        );
    }

    #[test]
    fn unlocked_ignores_the_spoofable_header() {
        let peer = "127.0.0.1:1".parse().ok();
        assert_eq!(
            client_ip(&headers_with_cf("203.0.113.7"), false, peer),
            Some("127.0.0.1".to_string())
        );
    }

    #[test]
    fn invalid_header_falls_back_to_peer() {
        let peer = "127.0.0.1:1".parse().ok();
        assert_eq!(
            client_ip(&headers_with_cf("not-an-ip"), true, peer),
            Some("127.0.0.1".to_string())
        );
        assert_eq!(client_ip(&HeaderMap::new(), true, None), None);
    }
}
