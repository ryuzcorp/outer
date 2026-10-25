---
title: KV guide
description: Sessions, counters and flags in KV with void/kv — works on Outer today.
---

**Verdict: works.** KV is a standard binding for small state — sessions,
counters, rate flags. Anything relational belongs in [D1](/guides/d1).

## 1. Declare it

In `void.config.ts`, under `cloudflare`. Edits need `bun run build` —
bindings are read from the built config:

```ts
cloudflare: {
  kv_namespaces: [{ binding: "KV", id: "sessions" }],
},
```

## 2. Use it

```ts
import { kv } from "void/kv";

await kv.put("user:123", { name: "Alice" }, { ttl: 3600 });
const user = await kv.get<User>("user:123"); // JSON-parsed; invalid JSON → raw string
await kv.delete("user:123");
await kv.list({ prefix: "user:", limit: 100, cursor });

// Typed map with an automatic prefix
const sessions = kv.map<Session>("sessions");
```

Reads are eventually consistent across requests — never store the only copy
of anything you can't rebuild, and never secrets (use
[secret storage](/guides/celld) instead).

## 3. Deploy

`kv_namespaces` is an accepted deploy key
(`crates/outer/src/deploy/pipeline.rs`), so the namespace deploys on every
account — plan gating for bindings arrives with billing. celld creates the
namespace on first use; a typo opens a silent empty namespace, so
double-check the id.

## Sources

- Void KV guide: <https://void.cloud/guide/kv>
- celld KV: <https://celld.dev/docs/services/kv>
