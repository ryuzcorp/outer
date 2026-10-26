---
title: Store small state in KV
description: Sessions, counters, and flags in KV with void/kv — declaration, API, and limits.
---

**Verdict: works.** KV is a standard binding for small state: sessions, counters, rate flags. Anything relational belongs in [Store data in D1](/agents/backend/database).

## Declare it

In `void.config.ts`, under `cloudflare`. Edits need `bun run build`. Bindings are read from the built config:

```ts
cloudflare: {
  kv_namespaces: [{ binding: "KV", id: "sessions" }],
},
```

`kv_namespaces` is an accepted deploy key, so the namespace deploys on every account. Binding keys are validated at deploy time. See [Store uploaded files](/backend/files-storage). Celld creates the namespace on first use; a typo opens a silent empty namespace, so double-check the id.

## Use it

```ts
import { kv } from "void/kv";

await kv.put("user:123", { name: "Alice" }, { ttl: 3600 });
const user = await kv.get<User>("user:123"); // JSON-parsed; invalid JSON → raw string
await kv.delete("user:123");
await kv.list({ prefix: "user:", limit: 100, cursor });

// Typed map with an automatic prefix
const sessions = kv.map<Session>("sessions");
```

Reads reach the cell that owns the namespace, so they never return a stale value. Writes to one namespace run one at a time, so a write-hot value like a busy counter belongs in a Durable Object.

## Respect the limits

Never store the only copy of anything you cannot rebuild, and never store secrets in KV. Use [Secrets](/agents/security/secrets) instead.
