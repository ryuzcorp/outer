---
title: Storage guide
description: Bindings, file uploads, and what never to store.
---

## Bindings

Extra state arrives as bindings in `void.config.ts`, validated from the
built `dist/wrangler.json` at deploy time. Only keys celld accepts pass:

- `kv_namespaces` and `durable_objects` for small state (sessions,
  counters, rate flags). Anything relational belongs in D1 (see `d1.md`).
- `r2_buckets` for file uploads. `queues` and `workflows` for background
  work.

Plan gating for bindings arrives with billing. Until then every accepted
key deploys on every account.

## Files

Uploaded images and video are **not** profanity-classified — keep uploads
behind your own checks, and never serve user uploads as executable
content.

## What never to store

Secrets go in secret storage (set outside code), never in the repo, the
database or KV. API keys, tokens and connection strings in committed code
are visible to anyone who can read the app's files.

## Sources

- Accepted wrangler keys: `crates/outer/src/deploy/pipeline.rs`
  (`ACCEPTED_CONFIG_KEYS`)
- celld's runtime docs: <https://celld.dev/docs/>
