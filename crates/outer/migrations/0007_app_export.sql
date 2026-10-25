-- Pro code exports (spec §7.5): one row per uploaded
-- `outer/exports/{slug}/<shortsha>-<random>.zip`, so the daily retention
-- sweep can delete objects older than 24 h. The app row cascades, taking
-- its exports with it on delete.
CREATE TABLE app_export (
  key        TEXT PRIMARY KEY,
  app_id     TEXT NOT NULL REFERENCES app(id) ON DELETE CASCADE,
  sha        TEXT NOT NULL,
  bytes      INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_app_export_created ON app_export (created_at);
