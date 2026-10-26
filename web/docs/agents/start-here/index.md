---
title: Build on Outer
description: The build, gate, deploy loop every agent follows — tool order, verification, and error relay rules.
---

You build apps through MCP tools. Every code change follows one loop: build, gate, deploy.

## Follow the loop

1. Read the template `AGENTS.md` first (`templates/app/AGENTS.md` or `templates/website/AGENTS.md` in the new repo).
2. Call `create_app` with a caller-picked slug (taken slugs are rejected; deleted names stay reserved 30 days).
3. Make small edits with `commit` using exact-match `edits`, one idea per message.
4. Verify with `look_at_app` after UI changes and `app_errors` after touching server code.

Each code tool waits for the deploy. It returns `success` with the URL, `failed` with the build log, or `blocked` with the lines to fix. A blocked deploy never goes live; the previous version keeps serving. Fix the flagged lines and commit again.

## Relay errors verbatim

When a tool refuses something, pass its `user_message` to your person verbatim. Never improvise around it or soften the limit.

## Know the starters

| Build | Start with |
| --- | --- |
| Interactive app: accounts, live state, dashboards, more than one table | [Templates](/agents/start-here/templates) (`app`) |
| Content site: landing page, portfolio, blog, event page | [Templates](/agents/start-here/templates) (`website`) |

New to the loop? Work [Quickstart](/agents/start-here/quickstart) end to end. The person-facing version lives at [Quickstart](/get-started/quickstart).
