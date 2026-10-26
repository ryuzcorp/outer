---
title: Templates
description: Pick the app or website starter, pass the template param, and rebuild instead of stretching.
---

Every `create_app` seeds from one of two pinned starters. The default is `app`. Pass `template: "website"` for the content-site starter. Both build with `bun run build` and deploy through the same [Pipeline](/agents/deploy/pipeline).

## Pick the right starter

| Build | Template |
| --- | --- |
| Landing page, portfolio, blog, event page | `website` |
| Grocery list, dashboard, anything with accounts or live data | `app` |
| Starts static, might grow accounts later | `app` — a site that outgrows islands wants loaders and actions, not more of them |

Both live in [ryuzcorp/outer-templates](https://github.com/ryuzcorp/outer-templates): `templates/app/` and `templates/website/`. A new app gets a copy of one folder as its first commit, so its repo starts at the project root. The person-facing picker lives at [Pick a template](/get-started/templates).

## Know what each ships

The `app` template serves server-rendered Ilha pages in `pages/` with loaders + actions on D1 and `useForm` for mutations. Read the template `AGENTS.md` first, then the [Ilha](/agents/frontend/ilha) and [Void](/agents/frontend/void) guides before writing code.

The `website` template serves mostly static, prerendered Astro pages with Ilha islands for interactive parts, at most one D1 database and one R2 bucket. Read its `AGENTS.md` first, then [Website](/agents/frontend/website).

## Rebuild, never stretch

A website that needs auth, live collaboration, or a second database table has outgrown the `website` template. Rebuild it on `app` rather than stretching server routes into an application.
