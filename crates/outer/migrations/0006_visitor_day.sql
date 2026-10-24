-- Per-app daily distinct visitors (console §13.1): one row per
-- (app, UTC day) holding that day's distinct client-IP count. IPs are
-- hashed with the day's process-random salt in memory and never stored;
-- only the count reaches SQLite. `day` counts UTC days since the epoch.
-- The flush upserts with max() so a restart mid-day (which resets the
-- in-memory set) keeps the count monotonic; restarts can undercount.
CREATE TABLE visitor_day (
  app_id   TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  day      INTEGER NOT NULL,
  visitors INTEGER NOT NULL,
  PRIMARY KEY (app_id, day)
);
