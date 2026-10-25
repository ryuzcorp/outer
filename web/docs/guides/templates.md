---
title: Templates
description: Two starters — an interactive app and a content site — and how to pick between them.
---

Every `create_app` seeds from one of two pinned starters. The default is
`app`; pass `template: "website"` (or `--template website` on the CLI) for
the other. Both build with `bun run build` and deploy to celld with the
same pipeline.

## The app template

Interactive Ilha + Void apps: accounts, live state, dashboards, anything
with more than one database table. Server-rendered pages in `pages/`,
loaders + actions on D1, `useForm` for mutations. Read the [Ilha
guide](/guides/ilha) and the [Void guide](/guides/void) before writing code.

## The website template

Content sites: landing pages, portfolios, blogs, event pages. Mostly
static, prerendered Astro pages with Ilha islands for the interactive
parts, at most one D1 database and one R2 bucket. Read the [Website
guide](/guides/website) before writing code.

## Which one

| Site | Template |
| --- | --- |
| Landing page, portfolio, blog, event page | `website` |
| Grocery list, dashboard, anything with accounts or live data | `app` |
| Starts static, might grow accounts later | `app` — a site that outgrows islands wants loaders and actions, not more of them |

A website that needs auth, live collaboration, or a second database table
has outgrown the website template: rebuild it on `app` rather than
stretching server routes into an application.
