-- Pro app access gate (PIN/password): per-app lock enforced by the proxy.
-- `access_mode` is 'open' (default), 'pin' or 'password'; `access_hash` is the
-- Argon2id hash (NULL when open); `access_version` bumps on every change so a
-- secret change signs every issued cookie out.
ALTER TABLE app ADD COLUMN access_mode TEXT NOT NULL DEFAULT 'open';
ALTER TABLE app ADD COLUMN access_hash TEXT;
ALTER TABLE app ADD COLUMN access_version INTEGER NOT NULL DEFAULT 0;
