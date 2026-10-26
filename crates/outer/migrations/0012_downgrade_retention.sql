-- Downgrade retention (spec §3): apps stopped by a downgrade are kept
-- 30 days, then purged after an email warning. `downgraded_stopped_at` is
-- stamped by `downgrade_account` on every stopped app (unix seconds) and
-- cleared for the kept oldest app and on re-upgrade; the retention sweep
-- records the warning in `downgrade_purge_warned_at` so it mails once.
ALTER TABLE app ADD COLUMN downgraded_stopped_at INTEGER;
ALTER TABLE app ADD COLUMN downgrade_purge_warned_at INTEGER;
