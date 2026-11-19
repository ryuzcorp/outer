-- Schema reset on 2026-11-19: production restarts from a wiped database.

CREATE TABLE user (
  id TEXT PRIMARY KEY,
  name TEXT,
  email TEXT NOT NULL UNIQUE,
  emailVerified INTEGER NOT NULL DEFAULT 0,
  image TEXT,
  role TEXT,
  banned INTEGER NOT NULL DEFAULT 0,
  banReason TEXT,
  banExpires INTEGER,
  plan TEXT NOT NULL DEFAULT 'free' CHECK (plan IN ('free', 'pro')),
  planStatus TEXT CHECK (planStatus IN ('active', 'past_due', 'canceled')),
  stripeCustomerId TEXT UNIQUE,
  stripeSubscriptionId TEXT,
  graceUntil INTEGER,
  termsVersion TEXT,
  termsAcceptedAt INTEGER,
  lastActiveAt INTEGER NOT NULL,
  createdAt INTEGER NOT NULL,
  updatedAt INTEGER NOT NULL
);

CREATE TABLE session (
  id TEXT PRIMARY KEY,
  token TEXT NOT NULL UNIQUE,
  expiresAt INTEGER NOT NULL,
  createdAt INTEGER NOT NULL,
  updatedAt INTEGER NOT NULL,
  ipAddress TEXT,
  userAgent TEXT,
  userId TEXT NOT NULL REFERENCES user (id) ON DELETE CASCADE,
  impersonatedBy TEXT
);

CREATE TABLE account (
  id TEXT PRIMARY KEY,
  accountId TEXT NOT NULL,
  providerId TEXT NOT NULL,
  userId TEXT NOT NULL REFERENCES user (id) ON DELETE CASCADE,
  accessToken TEXT,
  refreshToken TEXT,
  idToken TEXT,
  accessTokenExpiresAt INTEGER,
  refreshTokenExpiresAt INTEGER,
  scope TEXT,
  password TEXT,
  createdAt INTEGER NOT NULL,
  updatedAt INTEGER NOT NULL
);

CREATE TABLE verification (
  id TEXT PRIMARY KEY,
  identifier TEXT NOT NULL UNIQUE,
  value TEXT NOT NULL,
  expiresAt INTEGER NOT NULL,
  createdAt INTEGER NOT NULL,
  updatedAt INTEGER NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  inviteCode TEXT
);

CREATE TABLE beta_allowlist (
  email TEXT PRIMARY KEY,
  added_by TEXT,
  added_at INTEGER NOT NULL
);

CREATE TABLE waitlist (
  email TEXT PRIMARY KEY,
  first_seen INTEGER NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE apikey (
  id TEXT PRIMARY KEY,
  name TEXT,
  start TEXT,
  prefix TEXT,
  key TEXT NOT NULL UNIQUE,
  referenceId TEXT NOT NULL REFERENCES user (id) ON DELETE CASCADE,
  refillInterval INTEGER,
  refillAmount INTEGER,
  lastRefillAt INTEGER,
  enabled INTEGER NOT NULL DEFAULT 1,
  rateLimitEnabled INTEGER NOT NULL DEFAULT 1,
  rateLimitTimeWindow INTEGER,
  rateLimitMax INTEGER,
  requestCount INTEGER NOT NULL DEFAULT 0,
  remaining INTEGER,
  lastRequest INTEGER,
  expiresAt INTEGER,
  createdAt INTEGER NOT NULL,
  updatedAt INTEGER NOT NULL,
  permissions TEXT,
  metadata TEXT,
  configId TEXT NOT NULL DEFAULT 'default',
  clientName TEXT,
  clientVersion TEXT
);

CREATE TABLE oauthClient (
  id TEXT PRIMARY KEY,
  clientId TEXT NOT NULL UNIQUE,
  clientSecret TEXT,
  clientDiscoveryId TEXT,
  disabled INTEGER,
  skipConsent INTEGER,
  enableEndSession INTEGER,
  subjectType TEXT,
  scopes TEXT,
  clientCredentialsScopes TEXT,
  userId TEXT REFERENCES user (id) ON DELETE CASCADE,
  createdAt INTEGER,
  updatedAt INTEGER,
  name TEXT,
  uri TEXT,
  icon TEXT,
  contacts TEXT,
  tos TEXT,
  policy TEXT,
  softwareId TEXT,
  softwareVersion TEXT,
  softwareStatement TEXT,
  redirectUris TEXT NOT NULL,
  postLogoutRedirectUris TEXT,
  backchannelLogoutUri TEXT,
  backchannelLogoutSessionRequired INTEGER,
  tokenEndpointAuthMethod TEXT,
  applicationType TEXT,
  jwks TEXT,
  jwksUri TEXT,
  grantTypes TEXT,
  responseTypes TEXT,
  requirePKCE INTEGER,
  dpopBoundAccessTokens INTEGER NOT NULL DEFAULT 0,
  referenceId TEXT,
  metadata TEXT,
  kind TEXT NOT NULL CHECK (kind IN ('cimd', 'dcr')),
  host TEXT,
  verified INTEGER NOT NULL DEFAULT 0,
  fetchedAt INTEGER,
  lastTokenAt INTEGER
);

CREATE TABLE oauth_request (
  id TEXT PRIMARY KEY,
  client_id TEXT NOT NULL,
  redirect_uri TEXT NOT NULL,
  state TEXT,
  code_challenge TEXT NOT NULL,
  scope TEXT,
  resource TEXT,
  userId TEXT REFERENCES user (id) ON DELETE CASCADE,
  csrf TEXT NOT NULL,
  code_hash TEXT UNIQUE,
  expires_at INTEGER NOT NULL
);

CREATE TABLE oauthConsent (
  id TEXT PRIMARY KEY,
  clientId TEXT NOT NULL REFERENCES oauthClient (clientId),
  userId TEXT NOT NULL REFERENCES user (id) ON DELETE CASCADE,
  referenceId TEXT,
  resources TEXT,
  requestedUserInfoClaims TEXT,
  scopes TEXT NOT NULL,
  createdAt INTEGER,
  updatedAt INTEGER,
  clientName TEXT,
  clientVersion TEXT,
  lastUsedAt INTEGER,
  revokedAt INTEGER
);

CREATE TABLE oauthAccessToken (
  id TEXT PRIMARY KEY,
  token TEXT UNIQUE,
  clientId TEXT NOT NULL REFERENCES oauthClient (clientId),
  sessionId TEXT REFERENCES session (id) ON DELETE SET NULL,
  userId TEXT REFERENCES user (id) ON DELETE CASCADE,
  referenceId TEXT REFERENCES oauthConsent (id) ON DELETE CASCADE,
  authorizationCodeId TEXT,
  resources TEXT,
  requestedUserInfoClaims TEXT,
  refreshId TEXT REFERENCES oauthRefreshToken (id),
  expiresAt INTEGER,
  createdAt INTEGER,
  revoked INTEGER,
  confirmation TEXT,
  scopes TEXT NOT NULL
);

CREATE TABLE oauthRefreshToken (
  id TEXT PRIMARY KEY,
  token TEXT NOT NULL UNIQUE,
  clientId TEXT NOT NULL REFERENCES oauthClient (clientId),
  sessionId TEXT REFERENCES session (id) ON DELETE SET NULL,
  userId TEXT NOT NULL REFERENCES user (id) ON DELETE CASCADE,
  referenceId TEXT REFERENCES oauthConsent (id) ON DELETE CASCADE,
  authorizationCodeId TEXT,
  resources TEXT,
  requestedUserInfoClaims TEXT,
  expiresAt INTEGER,
  createdAt INTEGER,
  revoked INTEGER,
  rotatedAt INTEGER,
  rotationReplayResponse TEXT,
  rotationReplayExpiresAt INTEGER,
  authTime INTEGER,
  confirmation TEXT,
  scopes TEXT NOT NULL
);

CREATE TABLE oauthClientAssertion (
  id TEXT PRIMARY KEY,
  expiresAt INTEGER NOT NULL
);

CREATE TABLE app (
  id TEXT PRIMARY KEY,
  slug TEXT NOT NULL UNIQUE,
  owner_id TEXT NOT NULL REFERENCES user (id),
  name TEXT,
  uid INTEGER NOT NULL UNIQUE,
  port INTEGER UNIQUE,
  internal_port INTEGER UNIQUE,
  desired_state TEXT NOT NULL DEFAULT 'running' CHECK (desired_state IN ('running', 'stopped')),
  asleep_since INTEGER,
  last_request_at INTEGER,
  last_deploy_sha TEXT,
  visibility TEXT NOT NULL DEFAULT 'unlisted' CHECK (visibility IN ('unlisted', 'public')),
  moderation_state TEXT NOT NULL DEFAULT 'ok' CHECK (moderation_state IN ('ok', 'held', 'suspended')),
  paused_reason TEXT,
  last_activity_at INTEGER NOT NULL,
  inactivity_notices INTEGER NOT NULL DEFAULT 0,
  expired_at INTEGER,
  created_at INTEGER NOT NULL,
  always_on INTEGER NOT NULL DEFAULT 0 CHECK (always_on IN (0, 1)),
  preview_sha TEXT,
  preview_updated_at INTEGER,
  preview_port INTEGER,
  preview_internal_port INTEGER,
  downgraded_stopped_at INTEGER,
  downgrade_purge_warned_at INTEGER,
  storage_frozen_at INTEGER,
  storage_frozen_detail TEXT,
  r2_measured_bytes INTEGER,
  last_crawl_at INTEGER,
  access_mode TEXT NOT NULL DEFAULT 'open',
  access_hash TEXT,
  access_version INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE app_collaborator (
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  email TEXT NOT NULL,
  userId TEXT REFERENCES user (id) ON DELETE SET NULL,
  added_at INTEGER NOT NULL,
  PRIMARY KEY (app_id, email)
);

CREATE TABLE deploy (
  id TEXT PRIMARY KEY,
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  sha TEXT NOT NULL,
  actor_id TEXT,
  status TEXT NOT NULL CHECK (status IN ('queued', 'building', 'reviewing', 'deploying', 'success', 'failed', 'blocked')),
  steps TEXT,
  error TEXT,
  findings TEXT,
  log_tail TEXT,
  created_at INTEGER NOT NULL,
  finished_at INTEGER,
  kind TEXT NOT NULL DEFAULT 'main' CHECK (kind IN ('main', 'preview'))
);

CREATE TABLE app_secret (
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  value_enc BLOB NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (app_id, name)
);

CREATE TABLE app_domain (
  hostname TEXT PRIMARY KEY,
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  cf_custom_hostname_id TEXT NOT NULL,
  status TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE metric_minute (
  app_id TEXT NOT NULL,
  minute INTEGER NOT NULL,
  requests INTEGER NOT NULL DEFAULT 0,
  s4xx INTEGER NOT NULL DEFAULT 0,
  s5xx INTEGER NOT NULL DEFAULT 0,
  latency_hist TEXT,
  bytes_out INTEGER NOT NULL DEFAULT 0,
  visitors INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (app_id, minute)
);

CREATE TABLE path_hour (
  app_id TEXT NOT NULL,
  hour INTEGER NOT NULL,
  path TEXT NOT NULL,
  requests INTEGER NOT NULL DEFAULT 0,
  s5xx INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (app_id, hour, path)
);

CREATE TABLE app_log (
  app_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  ts INTEGER NOT NULL,
  stream TEXT NOT NULL,
  line TEXT NOT NULL,
  PRIMARY KEY (app_id, seq)
);

CREATE TABLE error_issue (
  app_id TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  message TEXT NOT NULL,
  frame TEXT,
  count INTEGER NOT NULL DEFAULT 0,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'resolved', 'ignored')),
  resolved_at INTEGER,
  PRIMARY KEY (app_id, fingerprint)
);

CREATE TABLE usage_day (
  userId TEXT NOT NULL,
  day TEXT NOT NULL,
  code_changes INTEGER NOT NULL DEFAULT 0,
  build_ms INTEGER NOT NULL DEFAULT 0,
  codemode_runs INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (userId, day)
);

CREATE TABLE usage_month (
  userId TEXT NOT NULL,
  month TEXT NOT NULL,
  requests INTEGER NOT NULL DEFAULT 0,
  storage_peak_bytes INTEGER NOT NULL DEFAULT 0,
  egress_bytes INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (userId, month)
);

CREATE TABLE rate_limit_events (
  subject TEXT NOT NULL,
  action TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE moderation_text (
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  text_hash TEXT NOT NULL,
  verdict TEXT NOT NULL CHECK (verdict IN ('clear', 'allowed', 'recheck')),
  checked_at INTEGER NOT NULL,
  PRIMARY KEY (app_id, text_hash)
);

CREATE TABLE moderation_check (
  id TEXT PRIMARY KEY,
  app_id TEXT NOT NULL,
  deploy_id TEXT,
  sha TEXT,
  stage TEXT NOT NULL CHECK (stage IN ('gate', 'crawl', 'report')),
  findings TEXT,
  scores TEXT,
  verdict TEXT,
  reviewed_by TEXT,
  reviewed_at INTEGER,
  created_at INTEGER NOT NULL
);

CREATE TABLE stripe_event (
  id TEXT PRIMARY KEY,
  type TEXT NOT NULL,
  received_at INTEGER NOT NULL
);

CREATE TABLE app_cron (
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  expr TEXT NOT NULL,
  next_at INTEGER NOT NULL,
  PRIMARY KEY (app_id, expr)
);

CREATE TABLE rollback_day (
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  day TEXT NOT NULL,
  count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (app_id, day)
);

CREATE TABLE agent_action (
  id TEXT NOT NULL PRIMARY KEY,
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  consent_id TEXT NULL,
  apikey_id TEXT NULL,
  tool TEXT NOT NULL,
  summary TEXT NOT NULL,
  outcome TEXT NOT NULL,
  error_code TEXT NULL,
  duration_ms INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE visitor_day (
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  day INTEGER NOT NULL,
  visitors INTEGER NOT NULL,
  PRIMARY KEY (app_id, day)
);

CREATE TABLE d1_migration (
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  database_name TEXT NOT NULL,
  database_identity TEXT NOT NULL,
  name TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  applied_at INTEGER NOT NULL,
  PRIMARY KEY (app_id, database_identity, name)
);

CREATE TABLE invite_code (
  code TEXT PRIMARY KEY,
  createdBy TEXT REFERENCES user(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('admin', 'grant')),
  note TEXT,
  redeemedBy TEXT UNIQUE REFERENCES user(id) ON DELETE SET NULL,
  redeemedAt INTEGER,
  revokedAt INTEGER,
  createdAt INTEGER NOT NULL
);

CREATE TABLE prefix_cleanup (
  prefix TEXT PRIMARY KEY,
  reason TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE account_action (
  id TEXT NOT NULL PRIMARY KEY,
  user_id TEXT NOT NULL,
  tool TEXT NOT NULL,
  summary TEXT NOT NULL,
  outcome TEXT NOT NULL,
  error_code TEXT NULL,
  duration_ms INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE look_hour (
  userId TEXT NOT NULL,
  hour TEXT NOT NULL,
  looks INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (userId, hour)
);

CREATE TABLE app_export (
  key TEXT PRIMARY KEY,
  app_id TEXT NOT NULL REFERENCES app(id) ON DELETE CASCADE,
  sha TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE alert_prefs (
  user_id TEXT NOT NULL PRIMARY KEY,
  deploy_failed INTEGER NOT NULL DEFAULT 1,
  error_spike INTEGER NOT NULL DEFAULT 1,
  quota INTEGER NOT NULL DEFAULT 1,
  updated_at INTEGER NOT NULL,
  inactivity INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE alert_send (
  id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  subject TEXT NOT NULL,
  kind TEXT NOT NULL,
  sent_at INTEGER NOT NULL
);

CREATE TABLE d1_backup (
  id TEXT NOT NULL PRIMARY KEY,
  app_id TEXT NOT NULL REFERENCES app (id) ON DELETE CASCADE,
  binding TEXT NOT NULL,
  store_key TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL,
  size_bytes INTEGER NOT NULL
);

CREATE TABLE slug_redirect (
  slug TEXT PRIMARY KEY,
  renamed_to TEXT NOT NULL
);

CREATE TABLE tool_call (
  id TEXT NOT NULL PRIMARY KEY,
  tool TEXT NOT NULL,
  account_id TEXT NULL,
  app_id TEXT NULL REFERENCES app (id) ON DELETE SET NULL,
  app_slug TEXT NULL,
  client TEXT NULL,
  session TEXT NOT NULL,
  duration_ms INTEGER NOT NULL,
  ok INTEGER NOT NULL,
  error_code TEXT NULL,
  response_bytes INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE usage_rollup_cursor (
  id TEXT PRIMARY KEY,
  last_minute INTEGER NOT NULL
);

CREATE TABLE draft (
  app_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  base_sha TEXT NOT NULL,
  ops TEXT NOT NULL DEFAULT '{}',
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (app_id, account_id),
  FOREIGN KEY (app_id) REFERENCES app(id) ON DELETE CASCADE
);

CREATE INDEX session_userId ON session (userId);

CREATE INDEX account_userId ON account (userId);

CREATE INDEX apikey_reference ON apikey (referenceId);

CREATE INDEX oauthClient_user ON oauthClient (userId);

CREATE INDEX oauthConsent_user ON oauthConsent (userId);

CREATE INDEX oauthConsent_client ON oauthConsent (clientId);

CREATE INDEX oauthAccessToken_client ON oauthAccessToken (clientId);

CREATE INDEX oauthAccessToken_user ON oauthAccessToken (userId);

CREATE INDEX oauthAccessToken_reference ON oauthAccessToken (referenceId);

CREATE INDEX oauthRefreshToken_client ON oauthRefreshToken (clientId);

CREATE INDEX oauthRefreshToken_user ON oauthRefreshToken (userId);

CREATE INDEX oauthRefreshToken_reference ON oauthRefreshToken (referenceId);

CREATE INDEX app_owner ON app (owner_id);

CREATE INDEX app_collaborator_user ON app_collaborator (userId);

CREATE INDEX deploy_app ON deploy (app_id, created_at);

CREATE INDEX rate_limit_events_lookup ON rate_limit_events (subject, action, created_at);

CREATE INDEX moderation_check_app ON moderation_check (app_id, created_at);

CREATE INDEX app_cron_due ON app_cron (next_at);

CREATE INDEX idx_agent_action_app_created ON agent_action (app_id, created_at DESC);

CREATE INDEX idx_agent_action_created ON agent_action (created_at);

CREATE INDEX invite_code_createdBy ON invite_code (createdBy);

CREATE INDEX idx_account_action_user_created ON account_action (user_id, created_at DESC);

CREATE INDEX idx_account_action_created ON account_action (created_at);

CREATE INDEX idx_app_export_created ON app_export (created_at);

CREATE INDEX idx_alert_send_subject_kind_sent ON alert_send (subject, kind, sent_at DESC);

CREATE INDEX idx_d1_backup_app_created ON d1_backup (app_id, created_at DESC);

CREATE INDEX idx_deploy_app_kind_created ON deploy (app_id, kind, created_at DESC);

CREATE INDEX idx_tool_call_tool_created ON tool_call (tool, created_at);

CREATE INDEX idx_tool_call_session_created ON tool_call (session, created_at);

CREATE INDEX idx_tool_call_created ON tool_call (created_at);
