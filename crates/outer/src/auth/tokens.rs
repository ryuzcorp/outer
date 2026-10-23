//! Token formats: OAuth access/refresh tokens and API keys.
//!
//! Every secret is 32 random bytes, stored as `sha256` hex. API keys use
//! Crockford base32 with a 10-character prefix; OAuth tokens use base64url.

use sha2::{Digest, Sha256};

/// Crockford base32 alphabet (no I, L, O, U): unambiguous when read aloud.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// `outer_` + 52 Crockford chars for 32 bytes.
pub const API_KEY_PREFIX: &str = "outer_";
/// OAuth access tokens: `outer_at_` + base64url of 32 bytes (43 chars).
pub const ACCESS_PREFIX: &str = "outer_at_";
/// OAuth refresh tokens: `outer_rt_` + base64url of 32 bytes (43 chars).
pub const REFRESH_PREFIX: &str = "outer_rt_";

/// Characters of the API-key body shown in `list_api_keys` and taken by
/// `revoke_api_key`.
pub const KEY_PREFIX_LEN: usize = 10;

/// `sha256` of `secret`, hex. What the database holds; the secret itself
/// never touches a log or an error.
pub fn sha256_hex(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Constant-time equality for secrets and hashes.
pub fn ct_eq(a: &str, b: &str) -> bool {
    constant_time_eq::constant_time_eq(a.as_bytes(), b.as_bytes())
}

/// Crockford base32 of `bytes`, no padding. 32 bytes become 52 chars.
pub fn encode_crockford(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut acc: u32 = 0;
    let mut bits: u8 = 0;
    for byte in bytes {
        acc = (acc << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(CROCKFORD[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(CROCKFORD[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn random_32() -> [u8; 32] {
    rand::random()
}

fn base64url_32() -> String {
    base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        random_32(),
    )
}

/// A fresh API key: the bearer token, its 10-char prefix, and the stored hash.
pub fn new_api_key() -> (String, String, String) {
    let body = encode_crockford(&random_32());
    let token = format!("{API_KEY_PREFIX}{body}");
    let prefix = body[..KEY_PREFIX_LEN].to_string();
    let hash = sha256_hex(&token);
    (token, prefix, hash)
}

/// A fresh OAuth access token and its stored hash.
pub fn new_access_token() -> (String, String) {
    let token = format!("{ACCESS_PREFIX}{}", base64url_32());
    let hash = sha256_hex(&token);
    (token, hash)
}

/// A fresh OAuth refresh token and its stored hash.
pub fn new_refresh_token() -> (String, String) {
    let token = format!("{REFRESH_PREFIX}{}", base64url_32());
    let hash = sha256_hex(&token);
    (token, hash)
}

/// A fresh web-session id (the cookie value) and its stored hash.
pub fn new_session_id() -> (String, String) {
    let id = base64url_32();
    let hash = sha256_hex(&id);
    (id, hash)
}

/// A fresh authorization code (never stored: only its hash is).
pub fn new_auth_code() -> (String, String) {
    let code = base64url_32();
    let hash = sha256_hex(&code);
    (code, hash)
}

/// A fresh CSRF token for the pending authorization row.
pub fn new_csrf() -> String {
    base64url_32()
}

/// A fresh six-digit numeric OTP, zero-padded.
pub fn new_otp_code() -> String {
    format!("{:06}", rand::random::<u32>() % 1_000_000)
}

/// A fresh row id.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// PKCE S256: `base64url(sha256(verifier))`. The authorize page only ever
/// issues S256 challenges, and the token endpoint only accepts them.
pub fn pkce_s256(verifier: &str) -> String {
    base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        Sha256::digest(verifier.as_bytes()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn crockford_round_trips_known_bytes() {
        assert_eq!(encode_crockford(&[0]), "00");
        assert_eq!(encode_crockford(&[255]), "ZW");
        let body = encode_crockford(&[0u8; 32]);
        assert_eq!(body.len(), 52);
        assert!(body.bytes().all(|b| CROCKFORD.contains(&b)));
        assert!(!body.bytes().any(|b| matches!(b, b'I' | b'L' | b'O' | b'U')));
    }

    #[test]
    fn token_shapes_carry_their_prefix_and_hash() {
        let (token, prefix, hash) = new_api_key();
        assert!(token.starts_with(API_KEY_PREFIX));
        assert_eq!(token.len(), API_KEY_PREFIX.len() + 52);
        assert_eq!(prefix.len(), KEY_PREFIX_LEN);
        assert!(token[API_KEY_PREFIX.len()..].starts_with(&prefix));
        assert_eq!(hash, sha256_hex(&token));

        let (access, access_hash) = new_access_token();
        assert!(access.starts_with(ACCESS_PREFIX));
        assert_eq!(access_hash, sha256_hex(&access));

        let (refresh, refresh_hash) = new_refresh_token();
        assert!(refresh.starts_with(REFRESH_PREFIX));
        assert_eq!(refresh_hash, sha256_hex(&refresh));
    }

    #[test]
    fn secrets_are_unique_and_hashes_do_not_leak_them() {
        let seen = (0..100).map(|_| new_api_key().0).collect::<HashSet<_>>();
        assert_eq!(seen.len(), 100);
        let (token, _, hash) = new_api_key();
        assert!(!hash.contains(&token[..16]));
    }

    #[test]
    fn otp_codes_are_six_digits_and_pkce_matches_rfc7636() {
        for _ in 0..50 {
            let code = new_otp_code();
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
        // RFC 7636 appendix B vector.
        assert_eq!(
            pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
