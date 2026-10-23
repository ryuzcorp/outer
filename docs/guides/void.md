---
title: Void guide
description: Routing, loaders, actions, forms and islands with Void.
---

Void owns routing, loaders, actions and deployment. Pages live in `pages/`:
`pages/index.tsx` renders, `pages/index.server.ts` loads and mutates.
Server code imports from `"void"`; components import UI helpers from
`"@ilha/void"`. Every snippet below is copied from the template.

## Loaders and actions

```ts
// pages/index.server.ts
import { defineHandler } from "void";
import type { InferProps } from "void";

export type Props = InferProps<typeof loader>;

export const loader = defineHandler(() => ({ todos: ["Ship it"] }));

export const action = defineHandler(async (c) => {
  const { text } = await c.req.json<{ text?: string }>();
  const trimmed = text?.trim() ?? "";
  if (!trimmed) {
    return c.json({ errors: { text: "Write something first." } }, 422);
  }
  // ...mutate, then return nothing: Void re-runs the loader and applies
  // the fresh page JSON in place. Returning data would force a full reload.
});
```

The loader's return becomes the page's props (`Props<typeof loader>`).
A `422` with `{ errors: { field: "message" } }` fills the form's errors.

## Forms

```tsx
import { Form, useForm } from "@ilha/void";

export default function NewTodo() {
  const form = useForm("/", { text: "" });
  return (
    <Form form={form}>
      <input
        name="text"
        value={form.data.text}
        oninput={(event) => form.setData("text", event.currentTarget.value)}
      />
      {form.errors.text ? <p>{form.errors.text}</p> : null}
      <button type="submit" disabled={form.pending}>
        Add
      </button>
    </Form>
  );
}
```

`form` exposes `data`, `setData()`, `errors`, `pending` and more; `Form`
submits through the Void router, so a successful submit refreshes the page
props without a reload.

## Routing, layouts, islands

- The file path is the route: `pages/index.tsx` → `/`. Layouts live in
  `pages/layout.tsx` and wrap every page below them.
- `Link` from `"@ilha/void"` navigates without a full page load:
  `<Link href="/about" prefetch>About</Link>`.
- Name a page `*.island.tsx` (for example `pages/about.island.tsx`) to
  render it as static HTML, and import a component with
  `import Counter from "./_counter" with { island: "visible" }` to hydrate
  only that component. Island props must be JSON.
- Files starting with `_` (like `pages/_counter.tsx`) are components,
  not routes.

## Sources

- The template's pages (loader, action, form, island wiring):
  <https://github.com/ryuzcorp/noite-template-ilha/tree/636715ea44d8944701652abda734ab610e75c69d/pages>
- The integration reference: <https://ilha.build/guide/void>
