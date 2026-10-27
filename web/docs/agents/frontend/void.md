---
title: Load data and handle forms with Void
description: Write Void loaders, intent actions, forms, links, and routes.
---

Put data loading in `pages/*.server.ts` and rendering in `pages/*.tsx`. Copy this page-loader shape for a form:

```ts
// pages/index.server.ts
import { defineHandler } from "void";
import type { InferProps } from "void";

export type Props = InferProps<typeof loader>;
export const loader = defineHandler(async () => ({ items: [] }));
// One action per page; an `intent` field tells the forms apart.
export const action = defineHandler(async (c) => {
  const { intent, text } = await c.req.json<{ intent?: string; text?: string }>();
  if (intent === "add" && !text?.trim()) {
    return c.json({ errors: { text: "Write something first." } }, 422);
  }
  // Mutate here, then return nothing: Void re-runs the loader in place.
});
```

```tsx
import { Form, useForm } from "@ilha/void";

const add = useForm("/" as string, { intent: "add", text: "" });
// add.data.text, add.setData("text", v), add.errors.text, add.pending
```

## Handle each form with its own intent

1. Declare one `action` per page and branch on the `intent` field.
2. Return `422` with `{ errors: { field: "message" } }` for validation failures.
3. Create one `useForm` per form with its own `intent` value.
4. Render each form with `Form` from `@ilha/void`.
5. Return nothing on success so Void re-runs the loader without a reload.

## Route with files and link without reloads

1. Map file paths to routes: `pages/index.tsx` renders `/`.
2. Prefix shared components with `_`; they never become routes.
3. Wrap pages in `layout` files placed beside the routes.
4. Navigate with `Link` from `@ilha/void`: `<Link href="/about" prefetch>About</Link>`.
5. Style with daisyUI classes (`card`, `btn`, `input`, `join`, `list`, `badge`, `alert`); reserve Tailwind for layout.
6. Build island pages as `*.island.tsx` with `with { island: "visible" }` imports and `useIslandForm()`.

For the person-side walkthrough, see [Add interactive pages](/frontend/interactive-pages).
