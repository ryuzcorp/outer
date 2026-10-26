---
title: Sandboxes for agents
description: Code-execution sandboxes are not available on Outer — do not ship sandbox code.
---

**Verdict: not available.** Do not build on `void/sandbox` — there is no container runtime behind Outer apps today. Never ship sandbox code. The person-facing page is [Not available](/backend/not-available).

## Know why it cannot run

Void sandboxes (`getSandbox(id)`, `sandbox.run/files/fetch`) are isolated Linux containers fronted by a Durable Object. They need Cloudflare Containers: a container runtime on every machine that can start the app's image. Outer runs each app as a celld worker fleet — plain processes under cgroup and network isolation, no Docker/Podman daemon on the app host. A sandbox has nowhere to start. The `containers` wrangler key passes Outer's deploy validation, so the failure would come at runtime, not at deploy — do not ship sandbox code expecting the deploy gate to catch it.

## Do this instead

| Need | Approach |
| --- | --- |
| Deterministic computation | Run code at build time: compute in `bun run build` and ship the result as static data |
| Slow or flaky work | Fan out to [queues](/agents/backend/queues) for background workers with retries |
| Untrusted or heavy execution | Call out over HTTP to an external sandbox/execution API with a stored [secret](/agents/security/secrets) |
