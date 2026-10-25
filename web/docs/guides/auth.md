---
title: Auth guide
description: Sign-in for your app's own users with Better Auth + D1 — works on Outer today.
---

**Verdict: works.** App auth is Better Auth backed by your app's D1
database — standard bindings only, no external service.

## 1. Declare it

In `void.config.ts`, pick providers and keep the D1 binding the template
ships (`DB`). Edits need `bun run build` — bindings are read from the
built config:

```ts
export default defineConfig({
  auth: { providers: ["email", "google", "github"] },
  cloudflare: {
    d1_databases: [{ binding: "DB", database_name: "app-db", migrations_dir: "db/migrations" }],
  },
});
```

OAuth providers need client credentials: `AUTH_GOOGLE_CLIENT_ID` /
`AUTH_GOOGLE_CLIENT_SECRET` (same shape for GitHub) — see step 3.

## 2. Sign users in and gate pages

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

`getUser()` / `getSession()` are the non-throwing reads for page loaders.
Signed-in responses default to `Cache-Control: no-store`.

## 3. Secrets (before the first auth deploy)

Auth needs a signing secret plus one pair per OAuth provider. Set them with
MCP (`set_secret`) or Console → Secrets — values apply on the next deploy:

- `BETTER_AUTH_SECRET` — random string, required.
- `AUTH_GOOGLE_CLIENT_ID` / `AUTH_GOOGLE_CLIENT_SECRET`, same for `GITHUB`
  — from each provider's developer console, with the callback URL pointing
  at your `<slug>.outer.now` host.

## 4. Migrations

Auth tables live alongside your tables. Run `void db generate` and commit
the migration files — Outer applies them on deploy before the new code goes
live ([D1 guide](/guides/d1)). Never edit an applied migration; write a new
one.

## Notes

- Rate limits are Better Auth's defaults (roughly 3 sign-ins per 10 s per
  IP), tracked per Worker instance — and everyone behind one proxy IP
  shares that IP's count. Enough for families, not for brute force.
- Test sign-in on the live link, not only locally: OAuth callbacks must
  match the app's public host.

## Sources

- Void auth guide: <https://void.cloud/guide/auth>
- Outer's D1 and secrets: [D1](/guides/d1), [celld](/guides/celld) (Runtime rules → Secrets)
