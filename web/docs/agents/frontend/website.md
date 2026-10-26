---
title: Build content sites with the website template
description: Prerender Astro pages, hydrate Ilha islands, and add small server routes.
---

Choose this template for content sites: landing pages, portfolios, blogs, event pages. Choose the app template for accounts, live state, or dashboards.

## Route with files

1. Add `src/pages/index.astro` for `/` and `src/pages/blog/[slug].astro` for each post.
2. Add API routes as files: `src/pages/api/newsletter.ts` serves `/api/newsletter`.
3. Keep frontmatter (`title`, `description`, `date`) on every post; `draft: true` stays out of the build.

## Hydrate islands where the page says

1. Write interactive parts as plain Ilha components in `src/islands/`.
2. Use `client:load` above the fold and `client:idle` or `client:visible` below it.
3. Keep island props JSON only.
4. Test islands with `renderHtml(Component)` from `ilha/test`.

## Opt server routes out of prerendering

Pages prerender by default and no server runs for them. A route that needs bindings opts out with one switch:

```ts
// src/pages/api/newsletter.ts
import type { APIRoute } from "astro";
import { env } from "cloudflare:workers";

export const prerender = false;

export const POST: APIRoute = async () => {
  const { DB } = env;
  const { results } = await DB.prepare("SELECT id FROM newsletter").all();
  return Response.json({ count: results.length });
};
```

1. Set `export const prerender = false` on every route that reads bindings.
2. Read bindings only through `import { env } from "cloudflare:workers"`.
3. Never use `Astro.locals.runtime.env`; Astro 6 removed it and it throws at runtime.
4. Declare new bindings on `Cloudflare.Env` in `src/env.d.ts`.
5. Keep server routes few and small: at most one or two against D1.

## Ship config and schema the celld way

1. Edit `astro.config.mjs` (`celld-config` integration), never hand-write `dist/wrangler.json`.
2. Read derived keys from `dist/wrangler.json` after `bun run build`.
3. Commit migration files under `db/migrations/`; never edit an applied migration.
4. Keep the route's `CREATE TABLE IF NOT EXISTS` as a local-dev backstop matching the migration.

For the person-side walkthrough, see [Websites](/frontend/websites).
