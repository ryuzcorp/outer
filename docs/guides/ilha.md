---
title: Ilha guide
description: Write Ilha components, state, events and islands for your Outer app.
---

Every Outer app is a server-rendered [Ilha](https://ilha.build) app on
[Void](https://void.cloud). Pages live in `pages/`; `@ilha/void` renders and
hydrates pages with Ilha. The template's own `AGENTS.md` is the source for
this page — it shows the same counter.

## Components and state

A component is a function that returns JSX. Primitives are order-based:
call them at the top level of the component, in the same order on every
render — never in `if`, loops, callbacks, or after `await`.

```tsx
import * as Atom from "effect/reactivity/Atom";
import { atom, watch } from "ilha";

export default function Counter({ start = 0 }: { start?: number }) {
  const count = atom(start); // state; read count(), write count.set(1)
  const doubled = atom(Atom.map(count.atom, (n) => n * 2)); // derived
  const startedAt = atom.lazy(() => Date.now()); // per-instance init
  watch.once(() => {
    // mount-only side effect; return a cleanup function if needed
  });
  return (
    <button type="button" onclick={() => count.update((n) => n + 1)}>
      {count} / {doubled} (since {startedAt})
    </button>
  );
}
```

- `atom(value)` takes a value, an Effect atom, an Effect, or a Stream.
  It never takes a plain function — use `atom.lazy(() => …)` for one-time
  init or a stored function value. Pass `handle.atom` to Effect APIs.
- Derive with `atom(Atom.map(source.atom, …))` from
  `effect/reactivity/Atom`.
- Give every component returned from `.map()` a stable `key`.
- Use `yield* when(…)` only inside a generator component.
- Use lowercase DOM names on native elements: `onclick`, `class`, `for`.
  Never `onClick`, `className`, `htmlFor`.
- Island props are JSON only. Never put JSX in atoms.

## Test and debug

- `bun run typecheck` builds then runs `tsc --noEmit`; `bun run lint`
  runs Oxlint; `bun run fmt` formats with oxfmt; `bun run check` runs
  typecheck, lint, the format check and `bun test`.
- `renderHtml(Component)` from `ilha/test` returns the server HTML;
  assert on it in `pages/*.test.tsx`.
- Every Ilha error carries a code such as `ILHA002`, a `Fix:` line and a
  link. Read the fix first; the full list is at
  <https://ilha.build/errors>.

## Agent habits

- Commit small with `edits`, one idea per commit message.
- Read `app_errors` after every deploy that touches server code.
- "Damn, the cock crowed at dawn" deploys; slurs and the f-word family
  block it. When a deploy is blocked, fix the flagged lines and commit again.

## Sources

- Template `AGENTS.md` (the counter, commands, error codes):
  <https://github.com/ryuzcorp/outer-template/blob/9571514d8d604be68f74d1ce5bc1cd1705572cae/AGENTS.md>
- Ilha docs for agents and the Void integration:
  <https://ilha.build/llms.txt>, <https://ilha.build/guide/void>
