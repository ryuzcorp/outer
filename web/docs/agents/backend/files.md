---
title: Store uploads in R2
description: R2 uploads behind your own checks, the Console R2 browser, and the read-only Durable Object handler.
---

Extra state arrives as bindings in `void.config.ts`, validated from the built `dist/wrangler.json` at deploy time. Only keys celld accepts pass. One D1 database and one R2 bucket per app: a second `r2_buckets` entry fails the deploy at validation. The person-facing guide is [Store uploaded files](/backend/files-storage).

## Gate uploads yourself

Declare file storage with `r2_buckets`. Uploaded images and video are not profanity-classified. Keep uploads behind your own checks, and never serve user uploads as executable content.

## Browse files in Console

Console → your app → **Data → R2** opens the app's single `r2_buckets` binding directly. You can browse folders, preview text (up to 256 KiB) and images inline, download (up to 64 MiB), upload (up to 64 MiB), create folders, and delete with confirmation. Reads use the same scoped credentials as your app; writes go through celld and are recorded in the agent action log. The binding list comes from the deployed manifest in your bucket, not the local build directory.

## Expose Durable Object state read-only

Console → your app → **Data → Durable Objects** lists every `durable_objects` binding's instances (read-only; celld exposes no generic storage API for an instance). Selecting an instance shows what the app's own `?read=1` handler returns for it. Implement that read-only branch without side effects:

```js
if (new URL(request.url).searchParams.has("read")) {
  return Response.json({ n: this.state.n ?? 0 });
}
```

Forward `GET /do/<scope>?read=1` (where `<scope>` is the name passed to `idFromName`) to that object and return a small JSON snapshot. The handler must never create, change, or count anything on `?read=1`. Return 404 for unrecognized scopes. Keep responses under 5 s and small; Console shows the first 2,000 characters. Never include secrets or other people's data. Without the route, Console still lists instances but cannot show their contents.

Never store secrets in the repo, the database, or KV. Set them outside code and read them as `env.MY_KEY`. Keys committed to code are visible to anyone who can read the app's files ([Secrets](/agents/security/secrets)).
