---
title: Storage guide
description: Bindings, file uploads, and what never to store.
---

## Bindings

Extra state arrives as bindings in `void.config.ts`, validated from the
built `dist/wrangler.json` at deploy time. Only keys celld accepts pass:

- `kv_namespaces` and `durable_objects` for small state (sessions, flags,
  low-write counters — a write-hot counter belongs in a Durable Object,
  since KV writes to one namespace run one at a time). Anything relational
  belongs in [D1](/guides/d1). Patterns: [KV](/guides/kv),
  [WebSockets](/guides/websockets) (rooms are Durable Objects).
- `r2_buckets` for file uploads. `queues` and `workflows` for background
  work ([Queues](/guides/queues)). Schedules are [Cron](/guides/cron).
  [Sandboxes](/guides/sandboxes) and native [Email](/guides/email) are not
  available — read those guides before promising them.

One D1 database and one R2 bucket per app for now: a second
`d1_databases` or `r2_buckets` entry fails the deploy at its validation
step. Plan gating for bindings arrives with billing. Until then every
accepted key deploys on every account.

## Files

Uploaded images and video are **not** profanity-classified — keep uploads
behind your own checks, and never serve user uploads as executable
content.

## Browse your data in the console

Console → your app → **Data → R2** opens the app's single `r2_buckets`
binding directly: browse folders, preview text (up to 256 KiB) and images
inline, download (up to 64 MiB), upload (up to 64 MiB), create folders, and
delete with confirmation.
Reads use the same scoped credentials as your app; writes go through celld
and are recorded in the agent action log. The binding list comes from the
deployed manifest in your bucket, not the local build directory.

Console → your app → **Data → Durable Objects** lists every
`durable_objects` binding's instances (read-only — celld exposes no generic
storage API for an instance). Selecting an instance shows what the app's own
`?read=1` handler returns for it. To preview state, implement that
read-only branch without side effects, e.g.:

```js
if (new URL(request.url).searchParams.has("read")) {
  return Response.json({ n: this.state.n ?? 0 });
}
```


Secrets go in secret storage (set outside code), never in the repo, the
database or KV. API keys, tokens and connection strings in committed code
are visible to anyone who can read the app's files.

## Sources

- Accepted wrangler keys: `crates/outer/src/deploy/pipeline.rs`
  (`ACCEPTED_CONFIG_KEYS`)
- celld's runtime docs: <https://celld.dev/docs/>
