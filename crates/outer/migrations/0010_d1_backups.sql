-- Daily D1 backups (wave 3, feature #4): one row per stored `.sql` dump.
-- The bytes live in R2 under `outer/backups/{slug}/` (NOT under
-- `fleets/{slug}/`, so app code can never touch them); the row carries the
-- key, the binding, and the size for the Console/MCP list. Rows are keyed
-- by `app_id`, and a rename rewrites each row's `store_key` in the same
-- transaction that flips the slug; `delete_app` removes both explicitly.
CREATE TABLE d1_backup (
  id          TEXT NOT NULL PRIMARY KEY,
  app_id      TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  binding     TEXT NOT NULL,
  store_key   TEXT NOT NULL UNIQUE,
  created_at  INTEGER NOT NULL,
  size_bytes  INTEGER NOT NULL
);
CREATE INDEX idx_d1_backup_app_created ON d1_backup (app_id, created_at DESC);
