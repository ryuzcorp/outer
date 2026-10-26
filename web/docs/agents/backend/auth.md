---
title: Add user sign-in
description: Sign-in with Better Auth plus D1 — providers, secrets, gating, migrations, and rate limits.
---

**Verdict: works.** App auth is Better Auth backed by your app's D1 database. Standard bindings only, no external service. The person-facing guide is [Let people sign in to your app](/backend/user-sign-in).

## Declare providers

In `void.config.ts`, pick providers and keep the D1 binding the template ships (`DB`). Edits need `bun run build`. Bindings are read from the built config:

```ts
export default defineConfig({
  auth: { providers: ["email", "google", "github"] },
  cloudflare: {
    d1_databases: [{ binding: "DB", database_name: "app-db", migrations_dir: "db/migrations" }],
  },
});
```

## Sign users in and gate pages

```tsx
// Client component
import { auth } from "void/client";

await auth.signUp.email({ email, password, name });
await auth.signIn.email({ email, password });
await auth.signOut();
```

```ts
// routes/api/profile.ts (or any page loader)
import { requireAuth } from "void/auth";
import { defineHandler } from "void";

export const GET = defineHandler((c) => {
  const user = requireAuth(c); // throws 401 when signed out
  return { email: user.email };
});
```

`getUser()` / `getSession()` are the non-throwing reads for page loaders. Signed-in responses default to `Cache-Control: no-store`.

## Set secrets before the first auth deploy

Set secrets with MCP (`set_secret`) or Console → Secrets. Values apply on the next deploy:

| Secret | Source |
| --- | --- |
| `BETTER_AUTH_SECRET` | Random string, required |
| `AUTH_GOOGLE_CLIENT_ID` / `AUTH_GOOGLE_CLIENT_SECRET` | Provider developer console, callback URL pointing at your `<slug>.outer.now` host |
| `AUTH_GITHUB_CLIENT_ID` / `AUTH_GITHUB_CLIENT_SECRET` | Same shape as Google |

## Commit auth migrations

Auth tables live alongside your tables. Run `void db generate` and commit the migration files. Outer applies them on deploy before the new code goes live ([Store data in D1](/agents/backend/database)). Never edit an applied migration; write a new one.

## Respect the limits

Rate limits are Better Auth's defaults (roughly 3 sign-ins per 10 s per IP), tracked per Worker instance. Everyone behind one proxy IP shares that IP's count. Enough for families, not for brute force. Test sign-in on the live link, not only locally: OAuth callbacks must match the app's public host.
