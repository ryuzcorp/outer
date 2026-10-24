-- Outer's platform state (spec §17). Schema reset on 2026-10-24: the auth
-- tables now follow the Better Auth database structure (table names and
-- column names from https://better-auth.com/docs/concepts/database and the
-- api-key / admin / oauth-provider / mcp plugin schemas), so the schema is
-- the one the Better Auth ecosystem knows. Migrations are append-only from
-- this 0001 on; earlier history was squashed because production restarts
-- from a wiped database.
--
-- Auth tables follow Better Auth (`user`, `session`, `account`,
-- `verification`, `apikey`, `oauth*`); `oauth_request`, `beta_allowlist`
-- and `waitlist` stay Outer-shaped. Secret columns hold ONLY sha256 hashes.
--
-- Deliberate departures from Better Auth, documented where used:
-- 1. Dates are INTEGER unix milliseconds everywhere (Outer convention),
--    not the `date` affinity (ISO-8601 text) Better Auth's SQLite DDL
--    emits. Every timestamp comparison in the codebase is integer math.
-- 2. Secret columns hold ONLY sha256 hex digests, never raw secrets:
--    `session.token`, `verification.value`, `apikey.key`,
--    `oauthAccessToken.token`, `oauthRefreshToken.token`. Better Auth
--    stores session tokens raw and hashes API keys; Outer hashes both.
-- 3. `*_uris` / `scopes`-style `string[]` fields and `json`/`metadata`
--    fields are TEXT holding JSON (same storage as Better Auth on SQLite).
--    `oauthConsent.scopes`/`resources` hold Outer's single scope/resource
--    string verbatim (e.g. `apps`), not a JSON array.
-- 4. Outer-specific additional fields (Better Auth's additional-fields
--    mechanism): `user.plan/planStatus/stripe*/graceUntil/terms*/lastActiveAt`,
--    `apikey.clientName/clientVersion`, `oauthClient.kind/host/verified/
--    fetchedAt/lastTokenAt`, `oauthConsent.clientName/clientVersion/
--    lastUsedAt/revokedAt`, `verification.attempts`. OAuth token rows point
--    at their consent via `referenceId`.
-- 5. No Better Auth equivalent exists for the pending-authorization row, so
--    `oauthRequest` stays Outer-shaped (only `account_id` renamed to
--    `userId`). `beta_allowlist` and `waitlist` (closed-beta state) also
--    stay Outer-shaped. The `account` table (social/credential logins) is
--    created for ecosystem familiarity; Outer only does email OTP and never
--    writes it.

CREATE TABLE user (
  id                   TEXT PRIMARY KEY,
  name                 TEXT,
  email                TEXT NOT NULL UNIQUE,
  emailVerified        INTEGER NOT NULL DEFAULT 0,
  image                TEXT,
  role                 TEXT,
  banned               INTEGER NOT NULL DEFAULT 0,
  banReason            TEXT,
  banExpires           INTEGER,
  plan                 TEXT NOT NULL DEFAULT 'free' CHECK (plan IN ('free', 'pro')),
  planStatus           TEXT CHECK (planStatus IN ('active', 'past_due', 'canceled')),
  stripeCustomerId     TEXT UNIQUE,
  stripeSubscriptionId TEXT,
  graceUntil           INTEGER,
  termsVersion         TEXT,
  termsAcceptedAt      INTEGER,
  lastActiveAt         INTEGER NOT NULL,
  createdAt            INTEGER NOT NULL,
  updatedAt            INTEGER NOT NULL
);

CREATE TABLE session (
  id             TEXT PRIMARY KEY,
  token          TEXT NOT NULL UNIQUE,
  expiresAt      INTEGER NOT NULL,
  createdAt      INTEGER NOT NULL,
  updatedAt      INTEGER NOT NULL,
  ipAddress      TEXT,
  userAgent      TEXT,
  userId         TEXT NOT NULL REFERENCES user (id) ON DELETE CASCADE,
  impersonatedBy TEXT
);
CREATE INDEX session_userId ON session (userId);

CREATE TABLE account (
  id                    TEXT PRIMARY KEY,
  accountId             TEXT NOT NULL,
  providerId            TEXT NOT NULL,
  userId                TEXT NOT NULL REFERENCES user (id) ON DELETE CASCADE,
  accessToken           TEXT,
  refreshToken          TEXT,
  idToken               TEXT,
  accessTokenExpiresAt  INTEGER,
  refreshTokenExpiresAt INTEGER,
  scope                 TEXT,
  password              TEXT,
  createdAt             INTEGER NOT NULL,
  updatedAt             INTEGER NOT NULL
);
CREATE INDEX account_userId ON account (userId);

CREATE TABLE verification (
  id         TEXT PRIMARY KEY,
  identifier TEXT NOT NULL UNIQUE,
  value      TEXT NOT NULL,
  expiresAt  INTEGER NOT NULL,
  createdAt  INTEGER NOT NULL,
  updatedAt  INTEGER NOT NULL,
  attempts   INTEGER NOT NULL DEFAULT 0
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

CREATE TABLE apikey (
  id                   TEXT PRIMARY KEY,
  name                 TEXT,
  start                TEXT,
  prefix               TEXT,
  key                  TEXT NOT NULL UNIQUE,
  referenceId          TEXT NOT NULL REFERENCES user (id) ON DELETE CASCADE,
  refillInterval       INTEGER,
  refillAmount         INTEGER,
  lastRefillAt         INTEGER,
  enabled              INTEGER NOT NULL DEFAULT 1,
  rateLimitEnabled     INTEGER NOT NULL DEFAULT 1,
  rateLimitTimeWindow  INTEGER,
  rateLimitMax         INTEGER,
  requestCount         INTEGER NOT NULL DEFAULT 0,
  remaining            INTEGER,
  lastRequest          INTEGER,
  expiresAt            INTEGER,
  createdAt            INTEGER NOT NULL,
  updatedAt            INTEGER NOT NULL,
  permissions          TEXT,
  metadata             TEXT,
  configId             TEXT NOT NULL DEFAULT 'default',
  clientName           TEXT,
  clientVersion        TEXT
);
CREATE INDEX apikey_reference ON apikey (referenceId);

CREATE TABLE oauthClient (
  id                              TEXT PRIMARY KEY,
  clientId                        TEXT NOT NULL UNIQUE,
  clientSecret                    TEXT,
  clientDiscoveryId               TEXT,
  disabled                        INTEGER,
  skipConsent                     INTEGER,
  enableEndSession                INTEGER,
  subjectType                     TEXT,
  scopes                          TEXT,
  clientCredentialsScopes         TEXT,
  userId                          TEXT REFERENCES user (id) ON DELETE CASCADE,
  createdAt                       INTEGER,
  updatedAt                       INTEGER,
  name                            TEXT,
  uri                             TEXT,
  icon                            TEXT,
  contacts                        TEXT,
  tos                             TEXT,
  policy                          TEXT,
  softwareId                      TEXT,
  softwareVersion                 TEXT,
  softwareStatement               TEXT,
  redirectUris                    TEXT NOT NULL,
  postLogoutRedirectUris         TEXT,
  backchannelLogoutUri            TEXT,
  backchannelLogoutSessionRequired INTEGER,
  tokenEndpointAuthMethod         TEXT,
  applicationType                 TEXT,
  jwks                            TEXT,
  jwksUri                         TEXT,
  grantTypes                      TEXT,
  responseTypes                   TEXT,
  requirePKCE                     INTEGER,
  dpopBoundAccessTokens           INTEGER NOT NULL DEFAULT 0,
  referenceId                     TEXT,
  metadata                        TEXT,
  kind                            TEXT NOT NULL CHECK (kind IN ('cimd', 'dcr')),
  host                            TEXT,
  verified                        INTEGER NOT NULL DEFAULT 0,
  fetchedAt                       INTEGER,
  lastTokenAt                     INTEGER
);
CREATE INDEX oauthClient_user ON oauthClient (userId);

CREATE TABLE oauth_request (
  id             TEXT PRIMARY KEY,
  client_id      TEXT NOT NULL,
  redirect_uri   TEXT NOT NULL,
  state          TEXT,
  code_challenge TEXT NOT NULL,
  scope          TEXT,
  resource       TEXT,
  userId         TEXT REFERENCES user (id) ON DELETE CASCADE,
  csrf           TEXT NOT NULL,
  code_hash      TEXT UNIQUE,
  expires_at     INTEGER NOT NULL
);

CREATE TABLE oauthConsent (
  id                      TEXT PRIMARY KEY,
  clientId                TEXT NOT NULL REFERENCES oauthClient (clientId),
  userId                  TEXT NOT NULL REFERENCES user (id) ON DELETE CASCADE,
  referenceId             TEXT,
  resources               TEXT,
  requestedUserInfoClaims TEXT,
  scopes                  TEXT NOT NULL,
  createdAt               INTEGER,
  updatedAt               INTEGER,
  clientName              TEXT,
  clientVersion           TEXT,
  lastUsedAt              INTEGER,
  revokedAt               INTEGER
);
CREATE INDEX oauthConsent_user ON oauthConsent (userId);
CREATE INDEX oauthConsent_client ON oauthConsent (clientId);

CREATE TABLE oauthAccessToken (
  id                      TEXT PRIMARY KEY,
  token                   TEXT UNIQUE,
  clientId                TEXT NOT NULL REFERENCES oauthClient (clientId),
  sessionId               TEXT REFERENCES session (id) ON DELETE SET NULL,
  userId                  TEXT REFERENCES user (id) ON DELETE CASCADE,
  referenceId             TEXT,
  authorizationCodeId     TEXT,
  resources               TEXT,
  requestedUserInfoClaims TEXT,
  refreshId               TEXT REFERENCES oauthRefreshToken (id),
  expiresAt               INTEGER,
  createdAt               INTEGER,
  revoked                 INTEGER,
  confirmation            TEXT,
  scopes                  TEXT NOT NULL
);
CREATE INDEX oauthAccessToken_client ON oauthAccessToken (clientId);
CREATE INDEX oauthAccessToken_user ON oauthAccessToken (userId);
CREATE INDEX oauthAccessToken_reference ON oauthAccessToken (referenceId);

CREATE TABLE oauthRefreshToken (
  id                      TEXT PRIMARY KEY,
  token                   TEXT NOT NULL UNIQUE,
  clientId                TEXT NOT NULL REFERENCES oauthClient (clientId),
  sessionId               TEXT REFERENCES session (id) ON DELETE SET NULL,
  userId                  TEXT NOT NULL REFERENCES user (id) ON DELETE CASCADE,
  referenceId             TEXT,
  authorizationCodeId     TEXT,
  resources               TEXT,
  requestedUserInfoClaims TEXT,
  expiresAt               INTEGER,
  createdAt               INTEGER,
  revoked                 INTEGER,
  rotatedAt               INTEGER,
  rotationReplayResponse  TEXT,
  rotationReplayExpiresAt INTEGER,
  authTime                INTEGER,
  confirmation            TEXT,
  scopes                  TEXT NOT NULL
);
CREATE INDEX oauthRefreshToken_client ON oauthRefreshToken (clientId);
CREATE INDEX oauthRefreshToken_user ON oauthRefreshToken (userId);
CREATE INDEX oauthRefreshToken_reference ON oauthRefreshToken (referenceId);

CREATE TABLE oauthClientAssertion (
  id        TEXT PRIMARY KEY,
  expiresAt INTEGER NOT NULL
);

-- The MCP plugin uses the same tables as the OAuth Provider plugin
-- (`oauthClient`, `oauthAccessToken`, `oauthRefreshToken`, `oauthConsent`,
-- `oauthClientAssertion`); no extra tables.

CREATE TABLE app (
  id                 TEXT PRIMARY KEY,
  slug               TEXT NOT NULL UNIQUE,
  owner_id           TEXT NOT NULL REFERENCES user (id),
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
  userId     TEXT REFERENCES user (id) ON DELETE SET NULL,
  added_at   INTEGER NOT NULL,
  PRIMARY KEY (app_id, email)
);
CREATE INDEX app_collaborator_user ON app_collaborator (userId);

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
  userId         TEXT NOT NULL,
  day            TEXT NOT NULL,
  code_changes   INTEGER NOT NULL DEFAULT 0,
  build_ms       INTEGER NOT NULL DEFAULT 0,
  codemode_runs  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (userId, day)
);

CREATE TABLE usage_month (
  userId             TEXT NOT NULL,
  month              TEXT NOT NULL,
  requests           INTEGER NOT NULL DEFAULT 0,
  storage_peak_bytes INTEGER NOT NULL DEFAULT 0,
  egress_bytes       INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (userId, month)
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

CREATE TABLE app_cron (
  app_id   TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  expr     TEXT NOT NULL,
  next_at  INTEGER NOT NULL,
  PRIMARY KEY (app_id, expr)
);
CREATE INDEX app_cron_due ON app_cron (next_at);

CREATE TABLE rollback_day (
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  day    TEXT NOT NULL,
  count  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (app_id, day)
);

-- Console agent action log (spec §13.1, §17): one row per MCP tool call
-- that targets an app. `user_id` is the acting user, `consent_id` the
-- `oauthConsent` row (OAuth calls) or NULL, `apikey_id` the `apikey` row
-- (API-key calls) or NULL. Pruned with the plan's log retention (7 d Free,
-- 30 d Pro); the app row's CASCADE drops an app's log on delete.
CREATE TABLE agent_action (
  id          TEXT NOT NULL PRIMARY KEY,
  app_id      TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  user_id     TEXT NOT NULL,
  consent_id  TEXT NULL,
  apikey_id   TEXT NULL,
  tool        TEXT NOT NULL,
  summary     TEXT NOT NULL,
  outcome     TEXT NOT NULL,
  error_code  TEXT NULL,
  duration_ms INTEGER NOT NULL,
  created_at  INTEGER NOT NULL
);
CREATE INDEX idx_agent_action_app_created
  ON agent_action (app_id, created_at DESC);
CREATE INDEX idx_agent_action_created ON agent_action (created_at);

CREATE TABLE visitor_day (
  app_id   TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  day      INTEGER NOT NULL,
  visitors INTEGER NOT NULL,
  PRIMARY KEY (app_id, day)
);

-- D1 migrations applied by the deploy pipeline (spec §10.1 step 7): one
-- row per migration file Outer applied to an app's database, so an
-- edited-after-apply file fails the deploy instead of silently drifting
-- from what celld recorded in the app's own `d1_migrations` table.
-- celld skips by name; this table pins the content hash Outer saw.
-- The row is keyed by the effective identity celld applies to:
-- `database_id` when the wrangler declaration has one, else
-- `database_name` (celld uses the id as the stable identity when present).
-- `database_name` stays alongside for humans. Changing the id under the
-- same name therefore starts clean against the new (empty) database
-- instead of skipping as "already applied".
CREATE TABLE d1_migration (
  app_id            TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  database_name     TEXT NOT NULL,
  database_identity TEXT NOT NULL,
  name              TEXT NOT NULL,
  sha256            TEXT NOT NULL,
  applied_at        INTEGER NOT NULL,
  PRIMARY KEY (app_id, database_identity, name)
);
