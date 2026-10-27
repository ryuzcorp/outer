-- MCP drafts (spec §1.5, owner proposal 2026-10-26): per-account staged
-- changes that survive restarts. `ops` is JSON
-- `{writes: [{path, content?, base64?}], edits: [{path, old, new,
-- replace_all?}], deletes: [path], moves: [{from, to}]}` in the same shapes
-- as `commit`; `base_sha` is the main head the draft was staged against, so
-- `ship` can rebase when the head moved underneath it.
CREATE TABLE draft (
  app_id     TEXT NOT NULL,
  account_id TEXT NOT NULL,
  base_sha   TEXT NOT NULL,
  ops        TEXT NOT NULL DEFAULT '{}',
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (app_id, account_id),
  FOREIGN KEY (app_id) REFERENCES app(id) ON DELETE CASCADE
);
