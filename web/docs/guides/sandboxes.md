---
title: Sandboxes guide
description: Code-execution sandboxes are not available on Outer yet — what the Void pattern needs and why it can't run here.
---

**Verdict: not available yet.** Don't build on `void/sandbox` — there is no
container runtime behind Outer apps today.

## Why not

Void sandboxes (`getSandbox(id)`, `sandbox.run/files/fetch`) are isolated
Linux containers fronted by a Durable Object. They need Cloudflare
Containers: a container runtime on every machine that can start the app's
image.

Outer runs each app as a celld worker fleet — plain processes under cgroup
and network isolation, no Docker/Podman daemon on the app host. A sandbox
has nowhere to start. The `containers` wrangler key passes Outer's deploy
validation (`crates/outer/src/deploy/pipeline.rs`), so the failure would
come at runtime, not at deploy — do not ship sandbox code expecting the
deploy gate to catch it.

## What to do instead

- **Run code at build time**, not request time: compute in `bun run build`
  and ship the result as static data.
- **Fan out to queues**: [Queues](/guides/queues) give you background
  workers with retries for slow or flaky work.
- **Call out over HTTP**: reach an external sandbox/execution API with a
  stored [secret](/guides/celld), like any other third-party call.

If container support lands, this page will say so — and the pattern will be
Void's standard one: <https://void.cloud/guide/sandboxes> (`void/sandbox`,
`sandbox: { image }` in `void.config.ts`, ephemeral containers, persist in
DB/KV/R2).

## Sources

- Void sandboxes guide: <https://void.cloud/guide/sandboxes>
- celld containers: <https://celld.dev/docs/services/containers>
- Accepted wrangler keys: `crates/outer/src/deploy/pipeline.rs` (`ACCEPTED_CONFIG_KEYS`)
