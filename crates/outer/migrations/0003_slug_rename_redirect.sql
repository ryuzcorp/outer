-- Rename redirects: `apps::rename` quarantines the OLD slug with
-- `renamed_to` naming the app's new slug, so the proxy can answer 308 to
-- the new host while the quarantine holds. Plain deletes leave it NULL and
-- keep the old 404 behavior. Rows expire with `until` like any quarantine.
ALTER TABLE slug_quarantine ADD COLUMN renamed_to TEXT;

-- Crash-safe rename cleanup: `apps::rename` records the target prefixes
-- before copying and swaps them for the old prefixes in the flip
-- transaction. Any row here blocks the slug it names (create and rename
-- both refuse it, even past its quarantine), and the maintenance drain
-- (`drain_prefix_cleanup`, run at startup and on every sleep-sweep tick)
-- deletes the prefix and the row. A leftover row is never inherited: the
-- drain deletes the data before the slug becomes claimable again.
CREATE TABLE prefix_cleanup (
    prefix TEXT PRIMARY KEY,
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
