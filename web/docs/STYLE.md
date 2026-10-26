---
title: Style guide
description: Voice, terminology, and house rules for every Outer docs page. Not published.
draft: true
---

This file is for docs contributors. It is never published.

## Voice

- Second person ("you") and active voice. Name the actor.
- Sentences under 25 words, one idea each. Paragraphs of 2–4 sentences.
- Steps go in numbered lists. Choices go in tables.
- Headings serve the reader's intent ("How to add a custom domain"). Sentence case. Never skip a level.
- No manual H1. The title comes from frontmatter.
- Warm and plain in Get started. Dense and precise in the Agents tab.

## Banned filler

Never write: simply, just (as filler), please note, in order to, it's worth
noting, powerful, seamless, leverage (as a verb), basically, essentially,
rest assured.

## Terminology

One term per concept. Left column wins everywhere.

| Use | Never use | Notes |
| --- | --- | --- |
| app | site, project | One live app at `<slug>.outer.now`. `website` only for the `website` template. |
| Console | console, dashboard | Capitalized. The web UI at `/console`. |
| deploy | publish, release, ship (as noun) | Every change goes through build → gate → deploy. |
| code change | commit (as a billed unit) | `commit` is the tool; a code change is what billing counts. |
| collaborator | member, teammate | Invited per app, up to 10, Pro. |
| unlisted | private | Default visibility: anyone with the link, not indexed. Opposite of public. |
| public | published | Pro visibility: listed and indexable. |
| hold, suspend | ban, takedown | `hold` = serves the "being fixed" page. `suspend` = offline pending review. |
| block (deploy) | reject, fail (for the gate) | A blocked deploy keeps the old version serving. A failed deploy is a build or system error. |
| rollback | revert, undo (as noun) | The `rollback` tool or the Console button. Never counts as a code change. |
| invite code | referral code, coupon | `OUTER-XXXX-XXXX`, closed beta only. |
| plan (Free / Pro) | tier, subscription (as plan name) | Capitalized plan names. |
| quota | limit (for counted resources) | Code changes, apps, requests. `limits` only for the Deploy page title and hard technical caps. |
| preview deploy | staging, branch deploy | One per app, Pro, at `<slug>--preview.outer.now`. |
| fleet | server, container, instance | The celld worker pool an app runs on. |
| binding | integration, env (for config) | `d1_databases`, `kv_namespaces`, declared in `void.config.ts`. |
| secret | env var, token (for stored keys) | Set outside code, read as `env.MY_KEY`. |
| agent | bot, assistant (for the builder) | The AI building the app over MCP. |
| person, you | user (for the human reader) | Guide pages address the person. Agent pages address the agent ("you" = the agent). |

## Facts

- Every claim must match the code and the spec. Never promise unbuilt features.
- Inactive-app deletion is NOT built. Do not document it.
- Billing figures: Pro $8/month, 50% launch discount ($4) while the promo runs.
