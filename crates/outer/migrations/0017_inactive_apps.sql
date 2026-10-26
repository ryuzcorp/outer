-- Inactive-app deletion (spec §7.8): the app-side clock columns landed in
-- 0001 (`app.last_activity_at`, the `app.inactivity_notices` reminder
-- bitmask, `app.expired_at`); this migration only adds the alert-prefs
-- toggle behind the reminder emails. Absence of a row means all four on.
ALTER TABLE alert_prefs ADD COLUMN inactivity INTEGER NOT NULL DEFAULT 1;
