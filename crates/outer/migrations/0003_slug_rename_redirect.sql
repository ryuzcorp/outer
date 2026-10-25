-- Rename redirects: `apps::rename` quarantines the OLD slug with
-- `renamed_to` naming the app's new slug, so the proxy can answer 308 to
-- the new host while the quarantine holds. Plain deletes leave it NULL and
-- keep the old 404 behavior. Rows expire with `until` like any quarantine.
ALTER TABLE slug_quarantine ADD COLUMN renamed_to TEXT;
