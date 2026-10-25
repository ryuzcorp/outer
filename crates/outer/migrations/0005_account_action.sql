-- Console account actions (spec §13.1, wave 2): audit for account-level
-- console mutations (connections, API keys, sessions, delete). Unlike
-- `agent_action`, this table is NOT keyed by app (these actions target the
-- account), so `user_id` carries no foreign key: the `delete_account` row
-- itself survives the account it describes.
CREATE TABLE account_action (
  id          TEXT NOT NULL PRIMARY KEY,
  user_id     TEXT NOT NULL,
  tool        TEXT NOT NULL,
  summary     TEXT NOT NULL,
  outcome     TEXT NOT NULL,
  error_code  TEXT NULL,
  duration_ms INTEGER NOT NULL,
  created_at  INTEGER NOT NULL
);
CREATE INDEX idx_account_action_user_created ON account_action (user_id, created_at DESC);
CREATE INDEX idx_account_action_created ON account_action (created_at);
