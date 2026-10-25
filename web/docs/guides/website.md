---
title: Website guide
description: Prerendered Astro pages, Ilha islands, one server route on D1 — the website template.
---

The website template ([Templates](/guides/templates)) is an [Astro](https://astro.build)
site with [Ilha islands](https://ilha.build/guide/astro), deployed the Void
way on celld. Pages prerender to static HTML; the Worker only runs the
server routes you opt out of prerendering.

## Routes are files

`src/pages/` is the router: `index.astro` → `/`, `blog/index.astro` →
`/blog`, `api/newsletter.ts` → `/api/newsletter`. A new page is a new
file. The blog reads Markdown from `src/content/posts/` through the
collection in `src/content.config.ts` — each file needs `title`,
`description`, `date`; `draft: true` stays out of the build.

## Islands

Interactive parts are plain Ilha components in `src/islands/`, hydrated
where the page says:

```astro
---
import NewsletterForm from "../islands/NewsletterForm.tsx";
---
<NewsletterForm client:load />
```

`client:load` for above-the-fold, `client:idle` or `client:visible` below
it. Island props are JSON only — never forward an Astro `<slot />` into an
island. Until hydration, handlers don't exist: keep controls usable or
disabled, never silently dead.

## One server route

Pages prerender by default — no server runs for them. A route that needs
bindings opts out:

```ts
// src/pages/api/newsletter.ts
import type { APIRoute } from "astro";

export const prerender = false;

export const POST: APIRoute = async ({ locals, request }) => {
  const { DB } = locals.runtime.env;
  const { results } = await DB.prepare("SELECT id FROM newsletter").all();
  return Response.json({ count: results.length });
};
```

`export const prerender = false` is the whole switch: without it,
`locals.runtime` doesn't exist. Read bindings only through
`Astro.locals.runtime.env` (raw D1/KV/R2 access — no `void/*` helpers, so
nothing needs AsyncLocalStorage). Keep server routes few and small: one D1
database, one R2 bucket per site.

## Database

D1 is on by default (`void.config.ts`, `d1_databases`); Outer provisions it
on first deploy. Schema changes ride the deploy as committed migration
files in `db/migrations/`. Outer applies pending migrations before the new
code goes live; a failing migration fails the deploy and the previous
version keeps serving. Never edit an applied migration — add a new one. The
route's `CREATE TABLE IF NOT EXISTS` stays as a local-dev backstop, with
the same statement as the migration.

## Build and deploy

`bun run build` prerenders the pages, bundles the Worker, and derives
`dist/wrangler.json` from the adapter's `dist/server/wrangler.json`
(`celld-config` in `astro.config.mjs`: allowlisted keys only, `main` →
`server/entry.mjs`, assets → `client`, `vars` → `{}`). Outer deploys `dist/`
with celld. Never hand-write `dist/wrangler.json`.
