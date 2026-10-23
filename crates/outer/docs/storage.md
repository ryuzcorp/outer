# Storage guide

## App storage

- Free: 500 MB. Pro: 25 GB. Measured hourly; at 100 % on Free the app is
  paused until next month (data is kept) or until you upgrade.
- Requests: 250k/month (Free) / 5M/month (Pro).

## Files (Pro)

File uploads use the R2 binding, Pro-only: a deploy that configures R2 on
a Free plan fails with a clear error. Uploaded images and video are not
profanity-classified — keep uploads behind your own checks, and never
serve user uploads as executable content.

## Key-value and objects

KV and Durable Objects bindings work on all plans for small state
(sessions, counters, rate flags). Anything relational belongs in D1
(see `d1.md`).

## What never to store

Secrets go in secret storage (set outside code), never in the repo, the
database or KV. API keys, tokens and connection strings in committed code
are visible to anyone who can read the app's files.
