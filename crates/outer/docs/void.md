# Void guide

Void owns routing, loaders, actions and deployment. Pages live in `pages/`:
`pages/items.tsx` renders, `pages/items.server.ts` loads and mutates.

## Routing

The file path is the route: `pages/index.tsx` → `/`, `pages/items.tsx` →
`/items`. Layouts wrap their directory. Forms post to the same URL's
action; `Form` from `void` wires the pending state.

## Loaders

```ts
import type { Loader } from "void";

export const loader: Loader = async ({ db, params, request }) => {
  const rows = await db.query("SELECT id, label FROM item WHERE id = ?", [
    params.id,
  ]);
  return { item: rows[0] ?? null };
};
```

The loader's return becomes the page's `data` prop (`Props<typeof loader>`).
Loaders run on the server for every request — keep them fast and never put
secrets in what they return.

## Actions

```ts
import type { Action } from "void";

export const action: Action = async ({ db, form, request }) => {
  const label = String(form.get("label") ?? "").trim();
  if (!label || label.length > 80) return { error: "Name it (max 80 chars)." };
  await db.execute("INSERT INTO item (label) VALUES (?)", [label]);
  return { ok: true };
};
```

Read the result with `useForm()`: `form.error` shows validation failures
without losing the visitor's input. Redirect after a successful post when
the URL should change.

## Secrets and external APIs

Secrets are set outside code and read through the environment at runtime —
never hardcode keys, and never return them from loaders. To fetch an
external API, call it from a loader or action with the secret in an
`Authorization` header, then pass only the rendered data to the page.

## Cron

Scheduled work is declared in the app config (see `celld.md` for the
interval floor: 1 h on Free, 1 min on Pro). The handler runs like a loader
with no visitor attached.
