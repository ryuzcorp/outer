//! App secrets (spec §8.2, §17): names agents manage over MCP, values stored
//! AES-256-GCM and injected as Worker `vars` at deploy time.
//!
//! Layout: `0x01 || nonce(12) || ciphertext || tag(16)`. AAD is
//! `app_id || 0x00 || name`, so a value moved to another app or key refuses
//! to decrypt. The key comes from `OUTER_SECRET` via HKDF-SHA256 with the
//! [`KEY_INFO`] label, domain-separated from every other use of that secret
//! (API keys and tokens use plain `sha256`, never HKDF).
//!
//! Plaintext values live in exactly two places: process memory during a tool
//! call or deploy, and the deploy-root wrangler config between injection and
//! the post-upload scrub ([`crate::deploy`]). The `app_secret.value_enc`
//! column, the agent action log and every tool result only ever carry names.

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use hkdf::Hkdf;
use sha2::Sha256;
use sqlx::SqlitePool;

/// HKDF `info`: the distinct label separating the app-secret key from any
/// other key derived from `OUTER_SECRET`.
pub const KEY_INFO: &[u8] = b"outer app-secret v1";

/// Blob version byte, first byte of every value we write.
const BLOB_V1: u8 = 1;
/// `0x01 || nonce(12)`.
const NONCE_LEN: usize = 12;
/// GCM tag length.
const TAG_LEN: usize = 16;

/// At most this many secrets per app (spec §8.2 names no number; the cap
/// bounds one deploy's `vars` object and the per-deploy decrypt work).
pub const MAX_SECRETS_PER_APP: u64 = 50;
/// Values are API keys and short config strings, not files.
pub const MAX_VALUE_BYTES: usize = 4096;

/// The AES-256-GCM key. `Debug` redacts: the key must never reach a log.
#[derive(Clone)]
pub struct SecretsKey([u8; 32]);

impl SecretsKey {
    /// Derive the app-secret key from `OUTER_SECRET`.
    pub fn derive(outer_secret: &str) -> Self {
        let hk = Hkdf::<Sha256>::new(None, outer_secret.as_bytes());
        let mut key = [0u8; 32];
        hk.expand(KEY_INFO, &mut key)
            .expect("HKDF-SHA256 expands to 32 bytes");
        Self(key)
    }

    /// The shared non-production key for unit, MCP and pipeline tests.
    /// Never leaves a test binary: production always derives from config.
    pub fn test_key() -> Self {
        Self::derive("outer-test-secret (never production)")
    }
}

impl std::fmt::Debug for SecretsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretsKey(<redacted>)")
    }
}

/// AAD binding a value to its app and key: unambiguous, since neither ids
/// nor names (both `[A-Za-z0-9_-]`-shaped) contain `0x00`.
fn aad(app_id: &str, name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(app_id.len() + 1 + name.len());
    out.extend_from_slice(app_id.as_bytes());
    out.push(0);
    out.extend_from_slice(name.as_bytes());
    out
}

/// Encrypt `value` for (`app_id`, `name`): fresh random 96-bit nonce per
/// value, returned as `0x01 || nonce || ciphertext || tag`.
pub fn encrypt(key: &SecretsKey, app_id: &str, name: &str, value: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(&key.0).expect("AES-256 takes 32 bytes");
    let nonce_bytes: [u8; NONCE_LEN] = rand::random();
    let nonce = Nonce::from(nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: value,
                aad: &aad(app_id, name),
            },
        )
        .expect("AES-GCM encryption from memory succeeds");
    let mut blob = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
    blob.push(BLOB_V1);
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    blob
}

/// Decrypt a blob written by [`encrypt`]. Any tampering — moved app/key,
/// flipped bit, wrong key — is one `Err`, carrying no plaintext.
pub fn decrypt(key: &SecretsKey, app_id: &str, name: &str, blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    let (version, rest) = blob
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty secret"))?;
    if *version != BLOB_V1 {
        anyhow::bail!("unsupported secret version {version}");
    }
    if rest.len() < NONCE_LEN + TAG_LEN {
        anyhow::bail!("truncated secret");
    }
    let (nonce_bytes, ciphertext) = rest.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new_from_slice(&key.0).expect("AES-256 takes 32 bytes");
    let plaintext = cipher
        .decrypt(
            &Nonce::from(
                <[u8; 12]>::try_from(nonce_bytes)
                    .map_err(|_| anyhow::anyhow!("truncated secret"))?,
            ),
            Payload {
                msg: ciphertext,
                aad: &aad(app_id, name),
            },
        )
        .map_err(|_| anyhow::anyhow!("secret failed authentication"))?;
    Ok(plaintext)
}

/// `KEY` in `set_secret {app, key, value}`: shell/Worker-env shaped
/// (`^[A-Z_][A-Z0-9_]{0,63}$`), so it interpolates safely into `env.NAME`.
/// Reserved platform names are refused separately ([`check_reserved`]).
pub fn check_name(name: &str) -> Result<(), String> {
    if !name.is_ascii() {
        return Err(format!("secret name {name:?} must be ASCII"));
    }
    if name.is_empty() || name.len() > 64 {
        return Err(format!(
            "secret name must be 1-64 chars, got {}",
            name.len()
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("nonempty checked above");
    let head_ok = first == '_' || first.is_ascii_uppercase();
    // Exactly `^[A-Z_][A-Z0-9_]{0,63}$`: lowercase is refused, so a name
    // can never smuggle shell case-folding surprises into `env.NAME`.
    if !head_ok || !chars.all(|c| c == '_' || c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return Err(format!(
            "secret name {name:?} must match ^[A-Z_][A-Z0-9_]{{0,63}}$"
        ));
    }
    Ok(())
}
/// Refuse platform-reserved names (spec §12.4): `PORT`, `AWS_*`, `OUTER_*`
/// and friends stay the platform's, so an app secret can never shadow or
/// capture platform configuration at runtime.
///
/// All `BUN_*` names are refused for secrets, including
/// `BUN_INSTALL_CACHE_DIR`: the sandbox lets that one through for builds,
/// but secrets never reach builds — they only land in Worker `vars` — so
/// there is no reason to let a secret claim any `BUN_*` name.
pub fn check_reserved(name: &str) -> Result<(), String> {
    if name.starts_with("BUN_") {
        return Err(format!(
            "secret name {name:?} is reserved for the platform and cannot be used for app secrets"
        ));
    }
    if crate::fleet::sandbox::is_reserved_env_name(name) {
        return Err(format!(
            "secret name {name:?} is reserved for the platform and cannot be used for app secrets"
        ));
    }
    Ok(())
}

/// One `list_secrets` row: the name and when it was last set. Never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretMeta {
    pub name: String,
    pub updated_at: i64,
}

/// Names + `updated_at` for `app_id`, ordered by name.
pub async fn list(pool: &SqlitePool, app_id: &str) -> anyhow::Result<Vec<SecretMeta>> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT name, updated_at FROM app_secret WHERE app_id = ?1 ORDER BY name")
            .bind(app_id)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(name, updated_at)| SecretMeta { name, updated_at })
        .collect())
}

/// Insert or replace (`app_id`, `name`), refusing past [`MAX_SECRETS_PER_APP`]
/// distinct names. The `WHERE` guard is part of the single statement, so two
/// racing inserts cannot both pass the cap; updates to an existing name
/// always pass. Returns `true` when the cap refused the write.
pub async fn set(
    pool: &SqlitePool,
    app_id: &str,
    name: &str,
    value_enc: &[u8],
    now_ms: i64,
) -> anyhow::Result<bool> {
    let done = sqlx::query(
         "INSERT INTO app_secret (app_id, name, value_enc, updated_at) \
          SELECT ?1, ?2, ?3, ?4 \
          WHERE (SELECT COUNT(*) FROM app_secret WHERE app_id = ?1) < ?5 \
             OR EXISTS (SELECT 1 FROM app_secret WHERE app_id = ?1 AND name = ?2) \
          ON CONFLICT (app_id, name) DO UPDATE SET value_enc = excluded.value_enc, updated_at = excluded.updated_at",
     )
     .bind(app_id)
     .bind(name)
     .bind(value_enc)
     .bind(now_ms)
     .bind(MAX_SECRETS_PER_APP as i64)
     .execute(pool)
     .await?;
    Ok(done.rows_affected() == 0)
}

/// Delete (`app_id`, `name`): `true` when a row existed.
pub async fn unset(pool: &SqlitePool, app_id: &str, name: &str) -> anyhow::Result<bool> {
    let done = sqlx::query("DELETE FROM app_secret WHERE app_id = ?1 AND name = ?2")
        .bind(app_id)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Every secret for a deploy: decrypted (`name`, plaintext) pairs, ordered
/// by name. One corrupt row fails the deploy closed rather than shipping a
/// half-secret environment.
pub async fn decrypt_all(
    pool: &SqlitePool,
    app_id: &str,
    key: &SecretsKey,
) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let rows: Vec<(String, Vec<u8>)> =
        sqlx::query_as("SELECT name, value_enc FROM app_secret WHERE app_id = ?1 ORDER BY name")
            .bind(app_id)
            .fetch_all(pool)
            .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (name, blob) in rows {
        let value = decrypt(key, app_id, &name, &blob)
            .map_err(|e| anyhow::anyhow!("secret {name:?} failed to decrypt: {e:#}"))?;
        out.push((name, value));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SecretsKey {
        SecretsKey::derive("test-outer-secret")
    }

    #[test]
    fn round_trip() {
        let key = key();
        let blob = encrypt(&key, "app1", "API_KEY", b"s3cr3t-value");
        assert_eq!(
            decrypt(&key, "app1", "API_KEY", &blob).unwrap(),
            b"s3cr3t-value"
        );
        // Fresh nonce per value: same input encrypts differently.
        let again = encrypt(&key, "app1", "API_KEY", b"s3cr3t-value");
        assert_ne!(blob, again);
        assert_eq!(
            decrypt(&key, "app1", "API_KEY", &again).unwrap(),
            b"s3cr3t-value"
        );
    }

    #[test]
    fn blob_layout_is_versioned_with_a_12_byte_nonce() {
        let blob = encrypt(&key(), "app1", "K", b"v");
        assert_eq!(blob[0], BLOB_V1);
        // 1 version + 12 nonce + 1 byte + 16 tag.
        assert_eq!(blob.len(), 1 + NONCE_LEN + 1 + TAG_LEN);
    }

    #[test]
    fn aad_binds_app_and_name() {
        let key = key();
        let blob = encrypt(&key, "app1", "API_KEY", b"s3cr3t-value");
        // Moved to another app or key, the blob refuses to open.
        assert!(decrypt(&key, "app2", "API_KEY", &blob).is_err());
        assert!(decrypt(&key, "app1", "OTHER", &blob).is_err());
        // So does a flipped bit, and so does the wrong OUTER_SECRET.
        let mut bit = blob.clone();
        bit[20] ^= 1;
        assert!(decrypt(&key, "app1", "API_KEY", &bit).is_err());
        let wrong = SecretsKey::derive("another-secret");
        assert!(decrypt(&wrong, "app1", "API_KEY", &blob).is_err());
        // Truncations and unknown versions fail, never panic.
        assert!(decrypt(&key, "app1", "API_KEY", &[]).is_err());
        assert!(decrypt(&key, "app1", "API_KEY", &blob[..10]).is_err());
        let mut v2 = blob.clone();
        v2[0] = 2;
        assert!(decrypt(&key, "app1", "API_KEY", &v2).is_err());
    }

    #[test]
    fn key_derivation_is_stable_and_labelled() {
        assert_eq!(
            SecretsKey::derive("s").0,
            SecretsKey::derive("s").0,
            "same secret derives the same key"
        );
        assert_ne!(
            SecretsKey::derive("a").0,
            SecretsKey::derive("b").0,
            "different secrets diverge"
        );
        // Not a bare hash of the secret: the HKDF label domain-separates it
        // from the sha256 token hashes in auth/.
        let hex = crate::auth::tokens::sha256_hex("s");
        assert_ne!(
            SecretsKey::derive("s").0.as_slice(),
            &hex.as_bytes()[..32],
            "derivation is not sha256(secret)"
        );
    }

    #[test]
    fn secret_key_never_formats() {
        assert_eq!(format!("{:?}", key()), "SecretsKey(<redacted>)");
    }

    #[test]
    fn name_shape() {
        for good in ["A", "_", "_X", "API_KEY", "K9", &"N".repeat(64)] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "a",
            "MyKey",
            "Port",
            "Aws_key",
            "APIkey",
            "9LIVES",
            "API-KEY",
            "API KEY",
            "API.KEY",
            "é",
            &"N".repeat(65),
        ] {
            assert!(check_name(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn reserved_names_are_refused_with_a_clear_message() {
        for reserved in [
            "PORT",
            "AWS_SECRET_ACCESS_KEY",
            "OUTER_SECRET",
            "LD_PRELOAD",
            "NODE_OPTIONS",
        ] {
            let err = check_reserved(reserved).unwrap_err();
            assert!(err.contains("reserved"), "{reserved}: {err}");
        }
        assert!(check_reserved("RESEND_API_KEY").is_err());
        // Secrets never reach builds, so every `BUN_*` name is refused for
        // secrets — even the cache-dir override the sandbox allows builds.
        assert!(check_reserved("BUN_FOO").is_err());
        assert!(check_reserved("BUN_INSTALL_CACHE_DIR").is_err());
        assert!(check_reserved("MY_KEY").is_ok());
    }

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query("INSERT INTO account (id, email, last_active_at, created_at) VALUES ('owner1', 'o@example.com', 0, 0)")
             .execute(&pool)
             .await
             .unwrap();
        (dir, pool)
    }

    async fn app(pool: &SqlitePool, id: &str) {
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) \
             VALUES (?1, ?1, 'owner1', 1, 1, 2, 0, 0)",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn set_get_unset_round_trip() {
        let (_dir, pool) = pool().await;
        app(&pool, "app1").await;
        let key = SecretsKey::test_key();
        assert!(list(&pool, "app1").await.unwrap().is_empty());

        let blob = encrypt(&key, "app1", "K", b"v");
        assert!(
            !set(&pool, "app1", "K", &blob, 7).await.unwrap(),
            "first write fits"
        );
        let metas = list(&pool, "app1").await.unwrap();
        assert_eq!(
            metas,
            vec![SecretMeta {
                name: "K".to_string(),
                updated_at: 7
            }]
        );

        // Replace: same name updates the value and the timestamp.
        let blob2 = encrypt(&key, "app1", "K", b"v2");
        assert!(!set(&pool, "app1", "K", &blob2, 9).await.unwrap());
        let metas = list(&pool, "app1").await.unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].updated_at, 9);
        let all = decrypt_all(&pool, "app1", &key).await.unwrap();
        assert_eq!(all, vec![("K".to_string(), b"v2".to_vec())]);

        assert!(unset(&pool, "app1", "K").await.unwrap());
        assert!(
            !unset(&pool, "app1", "K").await.unwrap(),
            "second unset finds nothing"
        );
        assert!(list(&pool, "app1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cap_counts_distinct_names_and_updates_always_pass() {
        let (_dir, pool) = pool().await;
        app(&pool, "app1").await;
        let key = SecretsKey::test_key();
        for n in 0..MAX_SECRETS_PER_APP {
            let name = format!("K{n:02}");
            let blob = encrypt(&key, "app1", &name, b"v");
            assert!(
                !set(&pool, "app1", &name, &blob, 1).await.unwrap(),
                "{name} fits"
            );
        }
        // The 51st distinct name is refused…
        let blob = encrypt(&key, "app1", "ONE_TOO_MANY", b"v");
        assert!(set(&pool, "app1", "ONE_TOO_MANY", &blob, 1).await.unwrap());
        // …while replacing an existing name still passes.
        let blob = encrypt(&key, "app1", "K00", b"new");
        assert!(!set(&pool, "app1", "K00", &blob, 2).await.unwrap());
        let all = decrypt_all(&pool, "app1", &key).await.unwrap();
        assert_eq!(all.len(), MAX_SECRETS_PER_APP as usize);
    }

    #[tokio::test]
    async fn decrypt_all_fails_closed_on_a_corrupt_row() {
        let (_dir, pool) = pool().await;
        app(&pool, "app1").await;
        let key = SecretsKey::test_key();
        let blob = encrypt(&key, "app1", "K", b"v");
        set(&pool, "app1", "K", &blob, 1).await.unwrap();
        sqlx::query("UPDATE app_secret SET value_enc = ?1 WHERE app_id = 'app1'")
            .bind([vec![1u8], vec![0u8; 12 + 16]].concat())
            .execute(&pool)
            .await
            .unwrap();
        let err = decrypt_all(&pool, "app1", &key).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("\"K\""), "names the row: {msg}");
        assert!(!msg.contains('v'), "never echoes plaintext: {msg}");
    }
}
