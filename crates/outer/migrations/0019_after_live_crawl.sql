-- After-live crawl scheduling (spec §15.2): the last time the crawl engine
-- checked the app's rendered pages. NULL means never crawled since the
-- latest deploy; the crawl sweep treats NULL as due. Updated by the crawl
-- itself (pass, hold, suspend, queue or Jev-outage error alike), so the
-- periodic re-crawl of live public/unlisted apps has a watermark.
ALTER TABLE app ADD COLUMN last_crawl_at INTEGER;
