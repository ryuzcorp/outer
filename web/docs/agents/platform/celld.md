---
title: Run inside the celld runtime
description: Follow celld fetch, state, binding, network, and memory rules.
---

## Obey the runtime contract

1. Keep no module-scope state across requests; read fresh state per request.
2. Export `fetch` handlers only.
3. Request no Workers AI, Vectorize, or Cache bindings.
4. Treat `caches.default` as always missing; never rely on the Cache API.
5. Stream large downloads instead of buffering them into memory; fetched responses count against app memory.

## Stay inside the network sandbox

1. Never target loopback, private ranges, link-local, or metadata addresses.
2. Avoid SMTP ports 25, 465, and 587; send mail through an HTTP API with a secret.

For the person-side walkthrough, see [How deploys work](/deploy/how-deploys-work).
