//! Invite codes (spec §6.5.1): `OUTER-XXXX-XXXX`, 8 Crockford base32
//! characters (40 bits) from the OS CSPRNG, the same source the codebase
//! already uses for ids and secrets (`rand::random`).
//!
//! Every new account gets 3 `grant` codes to share (admins get none);
//! admins mint `admin` codes in bulk through the console. A code is valid
//! while it exists, is unredeemed and is unrevoked; redemption consumes it
//! in the same transaction that creates the account, so a code spent
//! between `sign_in` and `verify_code` refuses the second verifier.

use sqlx::{Sqlite, SqlitePool, Transaction};

/// Codes a fresh account gets to share (`kind = 'grant'`).
pub const GRANTS_PER_USER: usize = 3;

/// The most codes one admin generation may mint.
pub const MAX_ADMIN_BATCH: i64 = 50;

/// Crockford base32, no I/L/O/U: unambiguous when read aloud.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A fresh code: `OUTER-` + 4 chars + `-` + 4 chars, 40 bits of entropy.
pub fn new_invite_code() -> String {
    let bytes: [u8; 5] = rand::random();
    let mut bits: u64 = 0;
    for byte in bytes {
        bits = (bits << 8) | u64::from(byte);
    }
    let mut body = String::with_capacity(8);
    for shift in (0..8).rev() {
        body.push(CROCKFORD[((bits >> (shift * 5)) & 31) as usize] as char);
    }
    format!("OUTER-{}-{}", &body[..4], &body[4..])
}

/// Normalize user input to the canonical `OUTER-XXXX-XXXX` form: trim,
/// uppercase, Crockford aliases (`O` → `0`, `I`/`L` → `1`), optional
/// `OUTER-` prefix, optional dashes. `None` when it cannot be a code.
pub fn normalize_invite_code(raw: &str) -> Option<String> {
    let upper = raw.trim().to_ascii_uppercase();
    let body = upper.strip_prefix("OUTER").unwrap_or(&upper);
    let compact: String = body
        .chars()
        .filter(|c| *c != '-' && *c != ' ')
        .map(|c| match c {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect();
    if compact.len() != 8 || !compact.bytes().all(|b| CROCKFORD.contains(&b)) {
        return None;
    }
    Some(format!("OUTER-{}-{}", &compact[..4], &compact[4..]))
}

/// `true` while the code exists, is unredeemed and is unrevoked.
pub async fn is_valid(pool: &SqlitePool, code: &str) -> anyhow::Result<bool> {
    let found: Option<String> = sqlx::query_scalar(
        "SELECT code FROM invite_code WHERE code = ? AND redeemedBy IS NULL AND revokedAt IS NULL",
    )
    .bind(code)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Mint the 3 grant codes for a fresh account, inside the caller's account
/// creation transaction. A code collision (40 bits, near-impossible) retries
/// with a fresh code rather than failing sign-up.
pub async fn mint_grants(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: &str,
    now: i64,
) -> anyhow::Result<()> {
    for _ in 0..GRANTS_PER_USER {
        let mut minted = false;
        for _ in 0..5 {
            let code = new_invite_code();
            let inserted = sqlx::query(
                "INSERT INTO invite_code (code, createdBy, kind, createdAt) VALUES (?, ?, 'grant', ?)",
            )
            .bind(&code)
            .bind(user_id)
            .bind(now)
            .execute(&mut **tx)
            .await;
            match inserted {
                Ok(_) => {
                    minted = true;
                    break;
                }
                Err(err)
                    if err
                        .as_database_error()
                        .is_some_and(|db| db.is_unique_violation()) =>
                {
                    continue;
                }
                Err(err) => return Err(err.into()),
            }
        }
        if !minted {
            anyhow::bail!("could not mint a unique invite code");
        }
    }
    Ok(())
}

/// Mint `count` admin codes for `creator_id`. The caller validates the range.
pub async fn mint_admin(
    pool: &SqlitePool,
    creator_id: &str,
    count: i64,
    note: Option<&str>,
    now: i64,
) -> anyhow::Result<Vec<String>> {
    let mut codes = Vec::with_capacity(count as usize);
    for _ in 0..count {
        // Collisions are near-impossible at 40 bits; retry a few times
        // rather than failing the batch.
        let mut minted: Option<String> = None;
        for _ in 0..5 {
            let code = new_invite_code();
            let inserted = sqlx::query(
                "INSERT INTO invite_code (code, createdBy, kind, note, createdAt) VALUES (?, ?, 'admin', ?, ?)",
            )
            .bind(&code)
            .bind(creator_id)
            .bind(note)
            .bind(now)
            .execute(pool)
            .await;
            match inserted {
                Ok(_) => {
                    minted = Some(code);
                    break;
                }
                Err(err)
                    if err
                        .as_database_error()
                        .is_some_and(|db| db.is_unique_violation()) =>
                {
                    continue;
                }
                Err(err) => return Err(err.into()),
            }
        }
        match minted {
            Some(code) => codes.push(code),
            None => anyhow::bail!("could not mint a unique invite code"),
        }
    }
    Ok(codes)
}

/// Consume a code for a fresh account: the single statement that makes the
/// double-spend race safe. `true` only when this caller won the code.
pub async fn consume(
    tx: &mut Transaction<'_, Sqlite>,
    code: &str,
    user_id: &str,
    now: i64,
) -> anyhow::Result<bool> {
    let rows = sqlx::query(
        "UPDATE invite_code SET redeemedBy = ?, redeemedAt = ? \
         WHERE code = ? AND redeemedBy IS NULL AND revokedAt IS NULL",
    )
    .bind(user_id)
    .bind(now)
    .bind(code)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(rows == 1)
}

/// One code row with the creator/redeemer emails the console shows.
#[derive(Debug, sqlx::FromRow)]
pub struct InviteRow {
    pub code: String,
    pub kind: String,
    pub note: Option<String>,
    pub created_at: i64,
    pub redeemed_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub created_by_email: Option<String>,
    pub redeemed_by_email: Option<String>,
}

/// The caller's own codes, newest first.
pub async fn list_for_user(pool: &SqlitePool, user_id: &str) -> anyhow::Result<Vec<InviteRow>> {
    sqlx::query_as(
        "SELECT i.code, i.kind, i.note, i.createdAt AS created_at, \
                i.redeemedAt AS redeemed_at, i.revokedAt AS revoked_at, \
                u.email AS created_by_email, r.email AS redeemed_by_email \
         FROM invite_code i \
         LEFT JOIN user u ON u.id = i.createdBy \
         LEFT JOIN user r ON r.id = i.redeemedBy \
         WHERE i.createdBy = ? \
         ORDER BY i.createdAt DESC, i.code DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// Every code, newest first, paged by the opaque `<createdAt>:<code>` cursor.
pub async fn list_all(
    pool: &SqlitePool,
    limit: i64,
    before: Option<(i64, String)>,
) -> anyhow::Result<Vec<InviteRow>> {
    let rows = match before {
        Some((at, code)) => {
            sqlx::query_as(
                "SELECT i.code, i.kind, i.note, i.createdAt AS created_at, \
                        i.redeemedAt AS redeemed_at, i.revokedAt AS revoked_at, \
                        u.email AS created_by_email, r.email AS redeemed_by_email \
                 FROM invite_code i \
                 LEFT JOIN user u ON u.id = i.createdBy \
                 LEFT JOIN user r ON r.id = i.redeemedBy \
                 WHERE (i.createdAt < ? OR (i.createdAt = ? AND i.code < ?)) \
                 ORDER BY i.createdAt DESC, i.code DESC LIMIT ?",
            )
            .bind(at)
            .bind(at)
            .bind(code)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT i.code, i.kind, i.note, i.createdAt AS created_at, \
                        i.redeemedAt AS redeemed_at, i.revokedAt AS revoked_at, \
                        u.email AS created_by_email, r.email AS redeemed_by_email \
                 FROM invite_code i \
                 LEFT JOIN user u ON u.id = i.createdBy \
                 LEFT JOIN user r ON r.id = i.redeemedBy \
                 ORDER BY i.createdAt DESC, i.code DESC LIMIT ?",
            )
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows)
}

/// Revoking a code only un-admits unused ones.
#[derive(Debug, PartialEq, Eq)]
pub enum RevokeOutcome {
    Revoked,
    AlreadyRevoked,
    Missing,
    Redeemed,
}

pub async fn revoke(pool: &SqlitePool, code: &str, now: i64) -> anyhow::Result<RevokeOutcome> {
    let row: Option<(Option<String>, Option<i64>)> =
        sqlx::query_as("SELECT redeemedBy, revokedAt FROM invite_code WHERE code = ?")
            .bind(code)
            .fetch_optional(pool)
            .await?;
    let Some((redeemed_by, revoked_at)) = row else {
        return Ok(RevokeOutcome::Missing);
    };
    if redeemed_by.is_some() {
        return Ok(RevokeOutcome::Redeemed);
    }
    if revoked_at.is_some() {
        return Ok(RevokeOutcome::AlreadyRevoked);
    }
    sqlx::query("UPDATE invite_code SET revokedAt = ? WHERE code = ?")
        .bind(now)
        .bind(code)
        .execute(pool)
        .await?;
    Ok(RevokeOutcome::Revoked)
}

/// One waitlist row for the admin table.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct WaitlistRow {
    pub email: String,
    pub first_seen: i64,
    pub attempts: i64,
}

pub async fn waitlist(pool: &SqlitePool, limit: i64) -> anyhow::Result<Vec<WaitlistRow>> {
    sqlx::query_as(
        "SELECT email, first_seen, attempts FROM waitlist \
         ORDER BY first_seen DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_are_outer_prefixed_crockford() {
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let code = new_invite_code();
            assert!(code.starts_with("OUTER-"), "{code}");
            assert_eq!(code.len(), 15, "{code}");
            assert!(seen.insert(code.clone()), "duplicate code {code}");
            let body: String = code.chars().filter(|c| *c != '-').skip(5).collect();
            assert_eq!(body.len(), 8);
            assert!(body.bytes().all(|b| CROCKFORD.contains(&b)), "{code}");
        }
    }

    #[test]
    fn normalization_accepts_prefix_dashes_aliases() {
        // Acceptance: these three inputs normalize alike (O→0).
        assert_eq!(
            normalize_invite_code("outer-7k3m-q9xd"),
            Some("OUTER-7K3M-Q9XD".to_string())
        );
        assert_eq!(
            normalize_invite_code("7K3MQ9XD"),
            Some("OUTER-7K3M-Q9XD".to_string())
        );
        assert_eq!(
            normalize_invite_code("OUTER-7K3M-Q9XO"),
            Some("OUTER-7K3M-Q9X0".to_string())
        );
        // I/L aliases, whitespace, lowercase, dashes optional.
        assert_eq!(
            normalize_invite_code("  outer-il0o-1234 "),
            Some("OUTER-1100-1234".to_string())
        );
        // Reject: wrong length, non-Crockford (U, and symbols).
        assert_eq!(normalize_invite_code("OUTER-SHORT"), None);
        assert_eq!(normalize_invite_code("OUTER-7K3M-Q9XU"), None);
        assert_eq!(normalize_invite_code("not a code!!"), None);
        assert_eq!(normalize_invite_code(""), None);
    }
}
