---
title: Build interactive pages with Ilha
description: Write Ilha components, state, events, and islands in app pages.
---

Call `atom` and `watch` at the top level of each component, in the same order on every render. Never call them inside `if`, loops, callbacks, or after `await`.

```tsx
import * as Atom from "effect/reactivity/Atom";
import { atom, watch } from "ilha";

export default function Counter({ start = 0 }: { start?: number }) {
  const count = atom(start); // read count(), write count.set(1)
  const doubled = atom(Atom.map(count.atom, (n) => n * 2)); // derived
  const startedAt = atom.lazy(() => Date.now()); // per-instance init
  watch.once(() => {
    // mount-only side effect; return a cleanup if needed
  });
  return (
    <button type="button" onclick={() => count.update((n) => n + 1)}>
      {count} / {doubled} (since {startedAt})
    </button>
  );
}
```

## Follow the atom rules

1. Pass `atom` a value, an Effect Atom, an Effect, or a Stream.
2. Never pass a plain function; use `atom.lazy(() => …)` for init or stored functions.
3. Pass `handle.atom` to Effect APIs that expect an atom handle.
4. Derive state with `atom(Atom.map(source.atom, (n) => …))`.
5. Give every component returned from `.map()` a stable `key`.
6. Use `yield* when(…)` only inside generator components.
7. Use lowercase DOM names: `onclick`, `class`, `for`.
8. Keep island props JSON; never put JSX in atoms.

## Render islands on island pages

1. Name the page `*.island.tsx` for static HTML with hydrated islands only.
2. Import islands with `import X from "./_x" with { island: "visible" }`.
3. Use `useIslandForm()` from `@ilha/void` on island pages, not `useForm()`.
4. Assert server output with `renderHtml(Component)` from `ilha/test`.

## Read the error code first

Every Ilha error carries a code such as `ILHA002`, a `Fix:` line, and a link. Read the fix before changing code.

For the person-side walkthrough, see [Add interactive pages](/frontend/interactive-pages).
