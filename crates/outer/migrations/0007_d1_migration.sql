-- D1 migrations applied by the deploy pipeline (spec §10.1 step 7): one
-- row per migration file Outer applied to an app's database, so an
-- edited-after-apply file fails the deploy instead of silently drifting
-- from what celld recorded in the app's own `d1_migrations` table.
-- celld skips by name; this table pins the content hash Outer saw.
-- The row is keyed by the effective identity celld applies to:
-- `database_id` when the wrangler declaration has one, else
-- `database_name` (celld uses the id as the stable identity when present).
-- `database_name` stays alongside for humans. Changing the id under the
-- same name therefore starts clean against the new (empty) database
-- instead of skipping as "already applied".
CREATE TABLE d1_migration (
  app_id            TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  database_name     TEXT NOT NULL,
  database_identity TEXT NOT NULL,
  name              TEXT NOT NULL,
  sha256            TEXT NOT NULL,
  applied_at        INTEGER NOT NULL,
  PRIMARY KEY (app_id, database_identity, name)
);
