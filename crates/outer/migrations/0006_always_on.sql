-- Keep-warm for Pro apps (spec §12.3, feature #6): a per-app `always_on`
-- flag. The sleep sweep skips flagged apps on Pro owners (up to
-- OUTER_ALWAYS_ON_MAX awake at once; the oldest-idle sleep past the cap),
-- and the warm-up wakes flagged apps that stopped (restart/crash).
-- Downgrade to Free leaves the flag stored but ignored, so an upgrade
-- restores warmth without re-toggling.
ALTER TABLE app ADD COLUMN always_on INTEGER NOT NULL DEFAULT 0
  CHECK (always_on IN (0, 1));
-- New Pro apps default on at create (the MCP `create_app` path); existing
-- Pro apps opt in retroactively here so the upgrade lever applies to them.
UPDATE app SET always_on = 1 WHERE owner_id IN (SELECT id FROM user WHERE plan = 'pro');
