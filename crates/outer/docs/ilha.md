# Ilha guide

Every Outer app is a server-rendered [Ilha](https://ilha.build) app on
[Void](https://void.cloud). Pages live in `pages/`. Void owns routing,
loaders, actions and deployment; `@ilha/void` renders and hydrates pages
with Ilha.

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
  init or a stored function value.
- Derive with `atom(Atom.map(source.atom, …))` or `atom(Atom.transform(…))`
  from `effect/reactivity/Atom`. Pass `handle.atom` to Effect APIs.
- Give every component returned from `.map()` a stable `key`.
- Use `yield* when(…)` only inside a generator component.
- Island props are JSON only. Never put JSX in atoms.

## DOM and events

Use lowercase DOM names on native elements: `onclick`, `class`, `for`.
Never `onClick`, `className`, `htmlFor`.

## Data with Void

Void owns data: loaders and actions live in `pages/*.server.ts`; read them
with `Props` and `useForm`, not with ad-hoc `fetch`.

```tsx
// pages/items.server.ts
import type { Loader, Action } from "void";

export const loader: Loader = async ({ db, params }) => {
  const rows = await db.query("SELECT id, label FROM item ORDER BY id");
  return { items: rows };
};

export const action: Action = async ({ db, form, request }) => {
  const label = String(form.get("label") ?? "").trim();
  if (!label) return { error: "Give the item a name." };
  const clean = moderate(label); // mask strong profanity before storing
  await db.execute("INSERT INTO item (label) VALUES (?)", [clean]);
  return { ok: true };
};
```

```tsx
// pages/items.tsx
import { Form, useForm, type Props } from "void";
import type { loader } from "./items.server";

export default function Items({ data }: Props<typeof loader>) {
  const form = useForm();
  return (
    <main>
      <ul>{data.items.map((item) => <li key={item.id}>{item.label}</li>)}</ul>
      <Form method="post">
        <input name="label" maxlength={80} required />
        <button type="submit">Add</button>
        {form.error && <p>{form.error}</p>}
      </Form>
    </main>
  );
}
```

## celld rules (the fleet your code runs on)

- No module-scope state: every request must read fresh state. A module
  variable set by one visitor leaks into the next visitor's request.
- `fetch` handlers only. No Workers AI, Vectorize or Cache bindings.
- Responses you fetch count against your app's own memory: stream large
  downloads instead of buffering them.
- Never call `http://localhost`, `127.0.0.1`, metadata addresses or SMTP
  ports (25/465/587) — egress to those is blocked. Send email through an
  HTTP API with a secret (see `void.md`).

## Visitor text

Anything a visitor can type must go through `moderate()` from
`lib/moderate.ts` before it is stored — it masks the gate's strong
wordlist matches. The D1-backed grocery-list example in the template shows
the pattern. No strong profanity in page text, seed data or the app name:
the pre-live gate blocks the deploy.

## Worked examples in the template

- A D1-backed list (the grocery list): loader + action + `moderate()`.
- A form with validation (error returned from the action, shown with
  `useForm`).
- A page with an island (JSON props, `atom.lazy` per-instance state).
- Fetching an external API through a secret.

## Commands and checks

| Command | Use it to |
| --- | --- |
| `bun run check` | Typecheck, lint and test. Run before you finish. |
| `bun run typecheck` | `tsc --noEmit`. |
| `bun run build` | Production build (what the deploy runs). |

- `renderHtml(Component)` from `ilha/test` returns the server HTML; assert
  on it in `pages/*.test.tsx`.
- Every Ilha error carries a code such as `ILHA002`, a `Fix:` line and a
  link. Read the fix first; the full list is at https://ilha.build/errors.

## Agent habits

- Commit small with `edits`, one idea per commit message.
- Read `app_logs` after every deploy that touches server code.
- "Damn, the cock crowed at dawn" deploys; slurs and the f-word family
  block it. When a deploy is blocked, fix the flagged lines and commit again.
