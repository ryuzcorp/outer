---
title: Declare bindings with accepted config keys
description: Use allowlisted wrangler keys, scoped D1 migrations, and read-only DO previews.
---

## Stay inside the accepted keys

Use only these top-level keys in `dist/wrangler.json` (from `ACCEPTED_CONFIG_KEYS`):

| Keys |
| --- |
| `$schema`, `name`, `main`, `no_bundle` |
| `compatibility_date`, `compatibility_flags` |
| `durable_objects`, `migrations`, `assets`, `services` |
| `triggers`, `vars`, `d1_databases`, `kv_namespaces` |
| `queues`, `workflows`, `r2_buckets`, `worker_loaders` |
| `containers`, `define`, `rules` |

1. Expect any other key to fail the deploy naming the key (`ai`, `Vectorize`, `Cache`, `send_email` included).
2. Set bindings in `void.config.ts` and rebuild; never hand-write `dist/wrangler.json`.
3. Stay inside your plan's cron floor (hourly or slower on Free, down to a minute on Pro); the deploy pipeline does not enforce floors yet, so hold the line yourself.

## Preview Durable Object state read-only

1. Declare classes under `durable_objects` in `void.config.ts`.
2. Answer `GET /do/<scope>?read=1` by forwarding to `env.ROOMS.get(env.ROOMS.idFromName(scope))`.
3. Return a small JSON snapshot; keep it read-only (`?read=1` never creates, changes, or counts anything).
4. Return 404 for unrecognized scopes, stay under 5 s and 2 KB, and never include secrets.

For the person-side walkthrough, see [How deploys work](/deploy/how-deploys-work).
