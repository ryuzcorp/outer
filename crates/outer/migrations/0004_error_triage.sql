-- Error triage (console wave 2, spec §13): when the owner last put an
-- issue into `resolved` or `ignored`. NULL for never-triaged issues. A
-- resolved issue that recurs (a new event with `last_seen` past
-- `resolved_at`) flips back to `open` automatically but keeps
-- `resolved_at`, so the console can badge it `Regressed`. Ignored issues
-- keep counting and never flip. A manual reopen clears `resolved_at`,
-- so only an automatic reopen reads as a regression.
ALTER TABLE error_issue ADD COLUMN resolved_at INTEGER;
