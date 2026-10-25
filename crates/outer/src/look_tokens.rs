//! Single-use tokens binding one `look_at_app` render to one app host.
//!
//! Cloudflare's `/snapshot` call accepts `setExtraHTTPHeaders`
//! (<https://developers.cloudflare.com/api/resources/browser_rendering/subresources/snapshot/methods/create/>),
//! so the tool mints a random token and asks Cloudflare to send it back as
//! [`LOOK_HEADER`] on the navigation. The app proxy injects the console
//! recorder only when that header carries a live token for the request's
//! own host, then consumes it: a leaked or replayed token renders nothing
//! extra. In memory, passed via state, never global.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The header Cloudflare sends back on the navigation when `look_at_app`
/// passes it through `setExtraHTTPHeaders`.
pub const LOOK_HEADER: &str = "x-outer-look";

/// How long a minted token stays valid: one render, consumed on use.
const LOOK_TOKEN_TTL: Duration = Duration::from_secs(60);

struct Entry {
    host: String,
    expires: Instant,
}

/// Single-use render tokens for console capture. Cloning shares the same
/// table: the MCP state mints, the proxy state claims.
#[derive(Clone, Default)]
pub struct LookTokens {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

impl LookTokens {
    /// An empty token table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a 128-bit token for `host`, valid for [`LOOK_TOKEN_TTL`].
    /// Prunes expired entries so the table cannot grow on misses.
    pub fn mint(&self, host: &str) -> String {
        self.mint_with_ttl(host, LOOK_TOKEN_TTL)
    }

    fn mint_with_ttl(&self, host: &str, ttl: Duration) -> String {
        let token: u128 = rand::random();
        let token = hex::encode(token.to_be_bytes());
        let mut inner = self.inner.lock().expect("look token table poisoned");
        let now = Instant::now();
        inner.retain(|_, entry| entry.expires > now);
        inner.insert(
            token.clone(),
            Entry {
                host: host.to_string(),
                expires: now + ttl,
            },
        );
        token
    }

    /// Consume `token` iff it is live and was minted for `host`. Wrong host,
    /// expired, unknown and replayed tokens all refuse.
    pub fn claim(&self, host: &str, token: &str) -> bool {
        let mut inner = self.inner.lock().expect("look token table poisoned");
        match inner.remove(token) {
            Some(entry) => entry.host == host && Instant::now() <= entry.expires,
            None => false,
        }
    }

    /// A token that is already expired (tests only): deterministic expiry
    /// without sleeping.
    #[cfg(test)]
    pub(crate) fn mint_expired(&self, host: &str) -> String {
        let token: u128 = rand::random();
        let token = hex::encode(token.to_be_bytes());
        let mut inner = self.inner.lock().expect("look token table poisoned");
        inner.insert(
            token.clone(),
            Entry {
                host: host.to_string(),
                expires: Instant::now() - Duration::from_secs(1),
            },
        );
        token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_token_claims_once_for_its_host() {
        let tokens = LookTokens::new();
        let token = tokens.mint("shop.outer.test");
        assert_eq!(token.len(), 32, "128 bits as hex");
        assert!(tokens.claim("shop.outer.test", &token));
        assert!(!tokens.claim("shop.outer.test", &token), "single use");
    }

    #[test]
    fn wrong_host_unknown_and_expired_tokens_refuse() {
        let tokens = LookTokens::new();
        let token = tokens.mint("shop.outer.test");
        assert!(!tokens.claim("other.outer.test", &token));
        assert!(!tokens.claim("shop.outer.test", "no-such-token"));
        let stale = tokens.mint_expired("shop.outer.test");
        assert!(!tokens.claim("shop.outer.test", &stale));
    }
}
