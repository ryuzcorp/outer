-- Console agent action log (spec §13.1, §17): one row per MCP tool call
-- that targets an app. Pruned with the plan's log retention (7 d Free,
-- 30 d Pro); the app row's CASCADE drops an app's log on delete.
CREATE TABLE agent_action (
  id          TEXT NOT NULL PRIMARY KEY,
  app_id      TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  account_id  TEXT NOT NULL,
  grant_id    TEXT NULL,
  api_key_id  TEXT NULL,
  tool        TEXT NOT NULL,
  summary     TEXT NOT NULL,
  outcome     TEXT NOT NULL,
  error_code  TEXT NULL,
  duration_ms INTEGER NOT NULL,
  created_at  INTEGER NOT NULL
);
CREATE INDEX idx_agent_action_app_created
  ON agent_action (app_id, created_at DESC);
