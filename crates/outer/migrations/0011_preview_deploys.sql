-- Preview deploys (Pro): one active preview per app, served at
-- `<slug>--preview.<apps_domain>` from a fresh data namespace
-- (`fleets/{slug}--preview/`), never touching live data. The preview fleet
-- is short-lived: the sleep sweep stops it on idleness and the preview
-- sweep discards the row after 24 h without a preview request.
--
-- `deploy.kind` separates preview pipeline runs from the live history:
-- every live-history query filters `kind = 'main'`, so a preview deploy
-- never becomes the rollback target or the console's latest deploy.
ALTER TABLE app ADD COLUMN preview_sha TEXT;
ALTER TABLE app ADD COLUMN preview_updated_at INTEGER;
ALTER TABLE app ADD COLUMN preview_port INTEGER;
ALTER TABLE app ADD COLUMN preview_internal_port INTEGER;
ALTER TABLE deploy ADD COLUMN kind TEXT NOT NULL DEFAULT 'main'
  CHECK (kind IN ('main', 'preview'));
CREATE INDEX idx_deploy_app_kind_created ON deploy (app_id, kind, created_at DESC);
