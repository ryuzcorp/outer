-- look_at_app hourly quota (wave 3 feature 1): one row per caller per
-- UTC hour (`YYYY-MM-DDTHH`). The UPSERT guard in `claim_look` refuses
-- past 30 looks; exhaustion is `rate_limited` with `retry_after_s` to the
-- next hour. Stale rows are never read, so no sweep deletes them.
CREATE TABLE look_hour (
  userId TEXT NOT NULL,
  hour   TEXT NOT NULL,
  looks  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (userId, hour)
);
