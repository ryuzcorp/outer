-- Plan limits (owner-set 2026-10-26): per-app storage freeze flag for the
-- storage sweep (refuse writes at the D1/R2 caps; never delete data), and
ALTER TABLE app ADD COLUMN storage_frozen_at INTEGER;
ALTER TABLE app ADD COLUMN storage_frozen_detail TEXT;
CREATE TABLE usage_rollup_cursor (
  id          TEXT PRIMARY KEY,
  last_minute INTEGER NOT NULL
);
