-- Invite codes (spec §6.5.1): single-use admission codes plus the 3
-- grant codes every new account gets to share. Stored as-is (shown again
-- to their owner); 40 bits plus the sign-up rate limits make guessing
-- pointless. camelCase like the Better Auth tables in 0001.
CREATE TABLE invite_code (
  code       TEXT PRIMARY KEY,
  createdBy  TEXT REFERENCES user(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('admin', 'grant')),
  note       TEXT,
  redeemedBy TEXT UNIQUE REFERENCES user(id) ON DELETE SET NULL,
  redeemedAt INTEGER,
  revokedAt  INTEGER,
  createdAt  INTEGER NOT NULL
);
CREATE INDEX invite_code_createdBy ON invite_code (createdBy);

-- The pending OTP row carries the invite code that gated admission, so
-- `verify_code` can consume it in the same transaction that creates the
-- account. NULL for existing/allowlisted/admin emails (ignored, §6.5.1).
ALTER TABLE verification ADD COLUMN inviteCode TEXT;
