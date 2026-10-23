-- Cron schedules from each deploy's wrangler config (spec §12.3).
-- Outer wakes the app shortly before each due time; the running fleet
-- then fires the Worker's `scheduled` handler itself.
CREATE TABLE app_cron (
  app_id   TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  expr     TEXT NOT NULL,
  next_at  INTEGER NOT NULL,
  PRIMARY KEY (app_id, expr)
);
CREATE INDEX app_cron_due ON app_cron (next_at);
