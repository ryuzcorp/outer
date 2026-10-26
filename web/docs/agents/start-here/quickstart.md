---
title: Quickstart for agents
description: Connect, create an app, change it, share it — the Outer loop for the agent builder.
---

Build a family grocery list in about ten minutes. Swap in your person's idea; the steps stay the same. New here? Read [Start here](/agents/start-here/) first.

## Connect first

Your person connects their chat client once (full table in the human [Quickstart](/get-started/quickstart)):

1. Claude Code: `claude mcp add --transport http outer https://outer.now/mcp`, then `/mcp` → `outer` to sign in.
2. claude.ai, Desktop, mobile: Settings → Connectors → Add custom connector → `https://outer.now/mcp`.
3. Codex CLI, IDE, ChatGPT desktop app: `codex mcp add outer --url https://outer.now/mcp`, then `codex mcp login outer`.
4. Anything else: OAuth if supported, else the emailed-code fallback.

## Ask for a scoped first build

Keep the first prompt to one screen: no login, no database yet.

> Build a grocery list app for my family on Outer. One page: an input to add items, a list with checkboxes, a filter for bought vs. still needed. No login, no sharing yet — just the list.

Before writing code, read the Ilha guide and the Void guide ([Void](/agents/frontend/void), [Ilha](/agents/frontend/ilha)). Then call `create_app` with a caller-picked slug. A live app serves at `https://<slug>.outer.now` in seconds. Apps are unlisted by default: reachable by link, not indexed. For a landing page, portfolio, or blog, pass `template: "website"` — see [Templates](/agents/start-here/templates).

## Make one small change

> Add a "who bought it" label: tapping a bought item lets you type a name.

Change code with `commit {app, message, edits}` using exact-match edits. Written code is formatted before the commit lands; `commit.formatted` lists touched files, so target that text with your next edits. Every code tool returns `success` + URL, `failed` + log tail, or `blocked` + lines to fix.

## Share it

Call `share_app {app}` for the URL, share text, and QR code. Check [Look](/agents/testing/look) output and [Logs](/agents/testing/logs) after server changes. Undo with `rollback` (never counts as a code change). Delete with `delete_app {app, confirm}` where confirm repeats the slug; the name stays reserved 30 days.

## Rules that bite newcomers

| Rule | Detail |
| --- | --- |
| No strong profanity | Page text, seed data, SQL literals, and app names — blocked deploys keep the old version serving ([Moderation](/agents/security/moderation)) |
| Check errors after server code | Read `app_errors` after every server-side deploy |
| Relay refusals verbatim | Pass `user_message` to your person unchanged |
| Watch quotas | Free gives 1 app and 50 code changes a day ([Limits](/agents/platform/limits)) |
