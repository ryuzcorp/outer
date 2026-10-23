-- Outer's platform state (spec §17). Timestamps are unix milliseconds.
-- Shipped migrations are never edited; changes go in a new file.

CREATE TABLE account (
  id                     TEXT PRIMARY KEY,
  email                  TEXT NOT NULL UNIQUE,
  plan                   TEXT NOT NULL DEFAULT 'free' CHECK (plan IN ('free', 'pro')),
  plan_status            TEXT CHECK (plan_status IN ('active', 'past_due', 'canceled')),
  stripe_customer_id     TEXT UNIQUE,
  stripe_subscription_id TEXT,
  grace_until            INTEGER,
  terms_version          TEXT,
  terms_accepted_at      INTEGER,
  last_active_at         INTEGER NOT NULL,
  banned_at              INTEGER,
  ban_reason             TEXT,
  created_at             INTEGER NOT NULL
);

CREATE TABLE beta_allowlist (
  email    TEXT PRIMARY KEY,
  added_by TEXT,
  added_at INTEGER NOT NULL
);

CREATE TABLE waitlist (
  email      TEXT PRIMARY KEY,
  first_seen INTEGER NOT NULL,
  attempts   INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE email_code (
  email      TEXT PRIMARY KEY,
  code_hash  TEXT NOT NULL,
  expires_at INTEGER NOT NULL,
  attempts   INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);

CREATE TABLE api_key (
  id             TEXT PRIMARY KEY,
  account_id     TEXT NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  name           TEXT,
  prefix         TEXT NOT NULL,
  key_hash       TEXT NOT NULL UNIQUE,
  client_name    TEXT,
  client_version TEXT,
  created_at     INTEGER NOT NULL,
  last_used_at   INTEGER,
  revoked_at     INTEGER
);
CREATE INDEX api_key_account ON api_key (account_id);

CREATE TABLE oauth_client (
  client_id     TEXT PRIMARY KEY,
  kind          TEXT NOT NULL CHECK (kind IN ('cimd', 'dcr')),
  name          TEXT,
  host          TEXT,
  redirect_uris TEXT NOT NULL,
  verified      INTEGER NOT NULL DEFAULT 0,
  fetched_at    INTEGER,
  created_at    INTEGER NOT NULL,
  last_token_at INTEGER
);

CREATE TABLE oauth_request (
  id             TEXT PRIMARY KEY,
  client_id      TEXT NOT NULL,
  redirect_uri   TEXT NOT NULL,
  state          TEXT,
  code_challenge TEXT NOT NULL,
  scope          TEXT,
  resource       TEXT,
  account_id     TEXT REFERENCES account (id) ON DELETE CASCADE,
  csrf           TEXT NOT NULL,
  code_hash      TEXT UNIQUE,
  expires_at     INTEGER NOT NULL
);

CREATE TABLE oauth_grant (
  id             TEXT PRIMARY KEY,
  account_id     TEXT NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  client_id      TEXT NOT NULL,
  scope          TEXT NOT NULL,
  resource       TEXT NOT NULL,
  client_name    TEXT,
  client_version TEXT,
  created_at     INTEGER NOT NULL,
  last_used_at   INTEGER,
  revoked_at     INTEGER
);
CREATE INDEX oauth_grant_account ON oauth_grant (account_id);

CREATE TABLE oauth_token (
  token_hash TEXT PRIMARY KEY,
  grant_id   TEXT NOT NULL REFERENCES oauth_grant (id) ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('access', 'refresh')),
  expires_at INTEGER NOT NULL,
  rotated_at INTEGER
);
CREATE INDEX oauth_token_grant ON oauth_token (grant_id);

CREATE TABLE web_session (
  id_hash    TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  expires_at INTEGER NOT NULL
);

CREATE TABLE app (
  id                 TEXT PRIMARY KEY,
  slug               TEXT NOT NULL UNIQUE,
  owner_id           TEXT NOT NULL REFERENCES account (id),
  name               TEXT,
  uid                INTEGER NOT NULL UNIQUE,
  port               INTEGER UNIQUE,
  internal_port      INTEGER UNIQUE,
  desired_state      TEXT NOT NULL DEFAULT 'running' CHECK (desired_state IN ('running', 'stopped')),
  asleep_since       INTEGER,
  last_request_at    INTEGER,
  last_deploy_sha    TEXT,
  visibility         TEXT NOT NULL DEFAULT 'unlisted' CHECK (visibility IN ('unlisted', 'public')),
  moderation_state   TEXT NOT NULL DEFAULT 'ok' CHECK (moderation_state IN ('ok', 'held', 'suspended')),
  paused_reason      TEXT,
  last_activity_at   INTEGER NOT NULL,
  inactivity_notices INTEGER NOT NULL DEFAULT 0,
  expired_at         INTEGER,
  created_at         INTEGER NOT NULL
);
CREATE INDEX app_owner ON app (owner_id);

CREATE TABLE app_collaborator (
  app_id     TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  email      TEXT NOT NULL,
  account_id TEXT REFERENCES account (id) ON DELETE SET NULL,
  added_at   INTEGER NOT NULL,
  PRIMARY KEY (app_id, email)
);
CREATE INDEX app_collaborator_account ON app_collaborator (account_id);

CREATE TABLE deploy (
  id          TEXT PRIMARY KEY,
  app_id      TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  sha         TEXT NOT NULL,
  actor_id    TEXT,
  status      TEXT NOT NULL CHECK (status IN ('queued', 'building', 'reviewing', 'deploying', 'success', 'failed', 'blocked')),
  steps       TEXT,
  error       TEXT,
  findings    TEXT,
  log_tail    TEXT,
  created_at  INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE INDEX deploy_app ON deploy (app_id, created_at);

CREATE TABLE app_secret (
  app_id     TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  value_enc  BLOB NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (app_id, name)
);

CREATE TABLE app_domain (
  hostname              TEXT PRIMARY KEY,
  app_id                TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  cf_custom_hostname_id TEXT NOT NULL,
  status                TEXT NOT NULL,
  created_at            INTEGER NOT NULL
);

CREATE TABLE slug_quarantine (
  slug  TEXT PRIMARY KEY,
  until INTEGER NOT NULL
);

CREATE TABLE metric_minute (
  app_id       TEXT NOT NULL,
  minute       INTEGER NOT NULL,
  requests     INTEGER NOT NULL DEFAULT 0,
  s4xx         INTEGER NOT NULL DEFAULT 0,
  s5xx         INTEGER NOT NULL DEFAULT 0,
  latency_hist TEXT,
  bytes_out    INTEGER NOT NULL DEFAULT 0,
  visitors     INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (app_id, minute)
);

CREATE TABLE path_hour (
  app_id   TEXT NOT NULL,
  hour     INTEGER NOT NULL,
  path     TEXT NOT NULL,
  requests INTEGER NOT NULL DEFAULT 0,
  s5xx     INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (app_id, hour, path)
);

CREATE TABLE app_log (
  app_id TEXT NOT NULL,
  seq    INTEGER NOT NULL,
  ts     INTEGER NOT NULL,
  stream TEXT NOT NULL,
  line   TEXT NOT NULL,
  PRIMARY KEY (app_id, seq)
);

CREATE TABLE error_issue (
  app_id      TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  message     TEXT NOT NULL,
  frame       TEXT,
  count       INTEGER NOT NULL DEFAULT 0,
  first_seen  INTEGER NOT NULL,
  last_seen   INTEGER NOT NULL,
  status      TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'resolved', 'ignored')),
  PRIMARY KEY (app_id, fingerprint)
);

CREATE TABLE usage_day (
  account_id    TEXT NOT NULL,
  day           TEXT NOT NULL,
  code_changes  INTEGER NOT NULL DEFAULT 0,
  build_ms      INTEGER NOT NULL DEFAULT 0,
  codemode_runs INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (account_id, day)
);

CREATE TABLE usage_month (
  account_id         TEXT NOT NULL,
  month              TEXT NOT NULL,
  requests           INTEGER NOT NULL DEFAULT 0,
  storage_peak_bytes INTEGER NOT NULL DEFAULT 0,
  egress_bytes       INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (account_id, month)
);

CREATE TABLE rate_limit_events (
  subject    TEXT NOT NULL,
  action     TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX rate_limit_events_lookup ON rate_limit_events (subject, action, created_at);

CREATE TABLE moderation_text (
  app_id     TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  text_hash  TEXT NOT NULL,
  verdict    TEXT NOT NULL CHECK (verdict IN ('clear', 'allowed', 'recheck')),
  checked_at INTEGER NOT NULL,
  PRIMARY KEY (app_id, text_hash)
);

CREATE TABLE moderation_check (
  id          TEXT PRIMARY KEY,
  app_id      TEXT NOT NULL,
  deploy_id   TEXT,
  sha         TEXT,
  stage       TEXT NOT NULL CHECK (stage IN ('gate', 'crawl', 'report')),
  findings    TEXT,
  scores      TEXT,
  verdict     TEXT,
  reviewed_by TEXT,
  reviewed_at INTEGER,
  created_at  INTEGER NOT NULL
);
CREATE INDEX moderation_check_app ON moderation_check (app_id, created_at);

CREATE TABLE stripe_event (
  id          TEXT PRIMARY KEY,
  type        TEXT NOT NULL,
  received_at INTEGER NOT NULL
);
