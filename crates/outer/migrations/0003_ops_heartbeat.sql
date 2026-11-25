CREATE TABLE ops_heartbeat (
  kind TEXT NOT NULL PRIMARY KEY,
  last_ok_ms INTEGER,
  first_err_ms INTEGER,
  last_err_ms INTEGER,
  consecutive_failures INTEGER NOT NULL DEFAULT 0,
  detail TEXT
);
