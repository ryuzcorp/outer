-- Per-app daily rollback budget (spec §8.3 throttle): at most one rollback
-- runs per app at a time (in-process lock), and each app gets
-- ROLLBACKS_PER_DAY per UTC day, counted here.
CREATE TABLE rollback_day (
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  day    TEXT NOT NULL,
  count  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (app_id, day)
);
