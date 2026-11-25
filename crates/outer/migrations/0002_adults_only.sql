-- Per-app adults-only (18+) flag for the age gate the proxy enforces.
ALTER TABLE app ADD COLUMN adults_only INTEGER NOT NULL DEFAULT 0 CHECK (adults_only IN (0, 1));
