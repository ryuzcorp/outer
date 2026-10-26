-- Tool telemetry (Wave 4, `open_app` + INSTRUCTIONS + telemetry): one row
-- per MCP `tools/call` — tool, caller, app slug when the call names one,
-- client name, session key, latency, outcome and response size. Arguments
-- and outputs are NEVER stored here (they hold code and secrets).
-- Pruned after 30 days by the metrics sweep.
CREATE TABLE tool_call (
  id             TEXT NOT NULL PRIMARY KEY,
  tool           TEXT NOT NULL,
  account_id     TEXT NULL,
  app_id         TEXT NULL REFERENCES app (id) ON DELETE SET NULL,
  app_slug       TEXT NULL,
  client         TEXT NULL,
  session        TEXT NOT NULL,
  duration_ms    INTEGER NOT NULL,
  ok             INTEGER NOT NULL,
  error_code     TEXT NULL,
  response_bytes INTEGER NOT NULL,
  created_at     INTEGER NOT NULL
);
CREATE INDEX idx_tool_call_tool_created ON tool_call (tool, created_at);
CREATE INDEX idx_tool_call_session_created ON tool_call (session, created_at);
CREATE INDEX idx_tool_call_created ON tool_call (created_at);
