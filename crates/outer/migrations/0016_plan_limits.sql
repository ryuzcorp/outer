-- Plan limits (owner-set 2026-10-26): per-app storage freeze flag for the
-- storage sweep (refuse writes at the D1/R2 caps; never delete data), the
-- sweep's last measured R2 bytes per app (NULL until the first sweep sizes
-- it; the Console upload gate reads it prospectively and adjusts it on
-- put/delete), and the request-rollup watermark cursor.
ALTER TABLE app ADD COLUMN storage_frozen_at INTEGER;
ALTER TABLE app ADD COLUMN storage_frozen_detail TEXT;
ALTER TABLE app ADD COLUMN r2_measured_bytes INTEGER;
CREATE TABLE usage_rollup_cursor (
  id          TEXT PRIMARY KEY,
  last_minute INTEGER NOT NULL
);
