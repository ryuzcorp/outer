-- Drop the 30-day slug reservation: a deleted or renamed-away slug is
-- claimable again immediately. The crash-safety guard (`prefix_cleanup`,
-- created in 0003) is untouched: pending cleanup rows still block a claim
-- until the maintenance drain deletes the leftover prefixes.
--
-- Rename redirects survive as plain, never-blocking rows: the proxy 308s an
-- old hostname to its renamed app until someone claims the old slug (the
-- claim deletes the redirect row). Only live redirects carry over — an
-- already-expired one had stopped redirecting, so it stays gone.
CREATE TABLE slug_redirect (
  slug TEXT PRIMARY KEY,
  renamed_to TEXT NOT NULL
);
INSERT INTO slug_redirect (slug, renamed_to)
  SELECT slug, renamed_to FROM slug_quarantine
  WHERE renamed_to IS NOT NULL AND until > (strftime('%s', 'now') * 1000);
DROP TABLE slug_quarantine;
