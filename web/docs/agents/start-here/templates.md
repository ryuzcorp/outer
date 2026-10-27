---
title: Templates
description: Pick the app or website shell, then copy proven patterns from the examples.
---

Every `create_app` seeds an empty branded shell from one of two pinned minimal templates: nothing to delete, no demo to strip out. The default is `app`. Pass `template: "website"` for the content-site shell. Both build with `bun run build` and deploy through the same [Pipeline](/agents/deploy/pipeline).

## Brand the app at creation

Pass `name` (1–60 chars) and `description` (1–160 chars) to `create_app`: both land in the repo-root `brand.json` with a deterministic accent colour in a purplish-blue band (HSL with hue 230 + FNV-1a(slug) mod 60, saturation 62%, lightness 48%), and the app's pages, manifest and icons pick them up. Leave them out and the name defaults to the slug title-cased (`grocery-list` → `Grocery List`) with the description matching it. Both are content-checked like the slug, with the same refusal.

`brand.json` is the one place to rename or rebrand an app: edit it with `commit`, never the generated outputs.

## Pick the right shell

| Build | Template |
| --- | --- |
| Landing page, portfolio, blog, event page | `website` |
| Grocery list, dashboard, anything with accounts or live data | `app` |
| Starts static, might grow accounts later | `app` — a site that outgrows islands wants loaders and actions, not more of them |

Both live in [ryuzcorp/outer-templates](https://github.com/ryuzcorp/outer-templates): `templates/app/` and `templates/website/`. A new app gets a copy of one folder as its first commit, so its repo starts at the project root. The person-facing picker lives at [Pick a template](/get-started/templates).

## Know what each ships

Both shells are minimal: layout, brand pipeline, settings, PWA shell, 404, and (for `app`) the D1 binding with an empty `db/migrations/` folder. No demo feature, no sample data.

The `app` shell serves server-rendered Ilha pages in `pages/` with loaders + actions on D1 and `useForm` for mutations. Call `open_app {app}` first (it carries the `AGENTS.md` rules digest), then read the [Ilha](/agents/frontend/ilha) and [Void](/agents/frontend/void) guides before writing code.

The `website` shell serves mostly static, prerendered Astro pages with Ilha islands for interactive parts, at most one D1 database and one R2 bucket. Call `open_app {app}` first (it carries the `AGENTS.md` rules digest), then read [Website](/agents/frontend/website).

## Copy patterns from the examples

Real features live in [Examples](/agents/start-here/examples): `examples` lists them, `examples {name}` returns the README and files to copy onto the fresh shell. Copy each file to the same path, add the page to `pages/_nav.ts`, add any dependency or binding, and rename the migration when the app already has migrations.

## Rebuild, never stretch

A website that needs auth, live collaboration, or a second database table has outgrown the `website` template. Rebuild it on `app` rather than stretching server routes into an application.
