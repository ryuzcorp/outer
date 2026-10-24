---
title: Void guide
description: Routing, loaders, actions, forms and islands with Void.
---

Void owns routing, loaders, actions and deployment. Pages live in `pages/`:
`pages/index.tsx` renders a D1-backed list, `pages/index.server.ts` loads
and mutates it. Server code imports from `"void"`; components import UI
helpers from `"@ilha/void"`. Every snippet below is copied from the template.

## Loaders and actions

```ts
// pages/index.server.ts
import { env } from "cloudflare:workers";
import { defineHandler } from "void";
import type { InferProps } from "void";

export type Props = InferProps<typeof loader>;

async function database() {
  await env.DB.prepare(
    "CREATE TABLE IF NOT EXISTS item (id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL)",
  ).run();
  return env.DB;
}

export const loader = defineHandler(async () => {
  const db = await database();
  const { results } = await db
    .prepare("SELECT id, text FROM item ORDER BY id")
    .all();
  return { items: results ?? [] };
});

// One action per page; `intent` says which form sent it.
export const action = defineHandler(async (c) => {
  const body = await c.req.json<{ intent?: string; text?: string; id?: number }>();
  const db = await database();
  if (body.intent === "add") {
    const text = body.text?.trim() ?? "";
    if (!text) {
      return c.json({ errors: { text: "Write something first." } }, 422);
    }
    await db.prepare("INSERT INTO item (text) VALUES (?)").bind(text).run();
  } else if (body.intent === "remove" && Number.isInteger(body.id)) {
    await db.prepare("DELETE FROM item WHERE id = ?").bind(body.id).run();
  }
  // Return nothing: Void re-runs the loader and applies the page JSON in
  // place. Returning data would force a full reload.
});
```

The loader's return becomes the page's props (`InferProps<typeof loader>`).
A `422` with `{ errors: { field: "message" } }` fills the form's errors.

## Forms

```tsx
import { Form, useForm } from "@ilha/void";

export default function Home({ items }: Props) {
  const add = useForm("/" as string, { intent: "add", text: "" });
  return (
    <Form form={add} class="join w-full">
      <input
        name="text"
        aria-label="New item"
        placeholder="Add an item"
        value={add.data.text}
        oninput={(event) => add.setData("text", event.currentTarget.value)}
        class="input join-item grow"
      />
      <button type="submit" disabled={add.pending} class="btn btn-primary join-item">
        Add
      </button>
    </Form>
  );
}
```

`form` exposes `data`, `setData()`, `errors`, `pending` and more; `Form`
submits through the Void router, so a successful submit refreshes the page
props without a reload. One `useForm` per form, with its own `intent`.

## Routing, layouts, islands

- The file path is the route: `pages/index.tsx` → `/`. `pages/layout.tsx`
  wraps every page below it. Files starting with `_` are components or
  helpers, not routes.
- `Link` from `"@ilha/void"` navigates without a full page load:
  `<Link href="/about" prefetch>About</Link>`.
- Style with daisyUI components (https://daisyui.com/components/), not
  hand-rolled utility stacks: `card card-body card-title`, `btn
  btn-primary btn-ghost btn-sm`, `input`, `join join-item`, `list
  list-row list-col-grow`, `badge`, `alert alert-soft`. Tailwind utilities
  are for layout only (`flex`, `gap-*`, `max-w-*`).
- Name a page `*.island.tsx` to render static HTML with only islands
  hydrated: `import X from "./_x" with { island: "visible" }`. Island
  props must be JSON. On island pages use `useIslandForm()` from
  `@ilha/void`, not `useForm()`. The template ships no island page — see
  its `AGENTS.md` for the pattern.

## Sources

- The template's pages (loader, intent action, forms, daisyUI):
  <https://github.com/ryuzcorp/outer-template/blob/8145efd51451abe1d819fffff2476d097e2c9dcd/pages/index.server.ts>,
  <https://github.com/ryuzcorp/outer-template/blob/8145efd51451abe1d819fffff2476d097e2c9dcd/pages/index.tsx>
- Island and command reference in the template's `AGENTS.md`:
  <https://github.com/ryuzcorp/outer-template/blob/8145efd51451abe1d819fffff2476d097e2c9dcd/AGENTS.md>
- The integration reference: <https://ilha.build/guide/void>
