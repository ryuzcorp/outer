-- Email alerts (wave 3, feature #2): per-account toggles and the send
-- ledger behind the debounce windows. `alert_prefs` has one row per
-- account that changed a default; absence means all three on.
-- `alert_send.subject` is the debounced scope: the app id for deploy and
-- error alerts, the account id for quota alerts.
CREATE TABLE alert_prefs (
  user_id       TEXT NOT NULL PRIMARY KEY,
  deploy_failed INTEGER NOT NULL DEFAULT 1,
  error_spike   INTEGER NOT NULL DEFAULT 1,
  quota         INTEGER NOT NULL DEFAULT 1,
  updated_at    INTEGER NOT NULL
);

CREATE TABLE alert_send (
  id       INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  subject  TEXT NOT NULL,
  kind     TEXT NOT NULL,
  sent_at  INTEGER NOT NULL
);
CREATE INDEX idx_alert_send_subject_kind_sent ON alert_send (subject, kind, sent_at DESC);
