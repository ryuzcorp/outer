---
title: Quickstart
description: Connect your chat app, ask for an app, see it live — the Outer loop in about ten minutes.
---

Get a live app in about ten minutes. The example below builds a family
grocery list — swap in your own idea, the steps are the same.

## 1. Connect your agent

One line per client (full table in [Connect](/connect)):

- Claude Code: `claude mcp add --transport http outer https://outer.now/mcp`, then `/mcp` → `outer` to sign in
- claude.ai, Desktop, mobile: Settings → Connectors → Add custom connector → `https://outer.now/mcp`
- Codex CLI, IDE, ChatGPT desktop app: `codex mcp add outer --url https://outer.now/mcp`, then `codex mcp login outer`
- Anything else: OAuth if supported, else the emailed-code fallback in [Sign in](/sign-in)

## 2. Ask for the app

Paste a scoped first prompt — one screen, no login, no database yet:

> Build a grocery list app for my family on Outer. One page: an input to
> add items, a list with checkboxes, a filter for bought vs. still needed.
> No login, no sharing yet — just the list. Read the Outer quickstart and
> Ilha guides first, then create the app and show me the live link.

Your agent calls `create_app` with a slug you pick — pass your own, since
taken slugs are rejected and deleted names stay reserved for 30 days. A live
Ilha + Void app serves at the returned `https://<your-slug>.outer.now` in
seconds. Unlisted by default: not indexed, reachable by link. Building a
landing page, portfolio, or blog instead? Say so in the prompt — the agent
passes `template: "website"` for the content-site starter. See
[Templates](/guides/templates).

## 3. Make one change

> Add a "who bought it" label: tapping a bought item lets you type a name.

The agent changes code with `commit {app, message, edits}` using exact-match
edits. Every code tool waits for the deploy and returns `success` + URL,
`failed` + log tail, or `blocked` + the lines to fix. Written code is
formatted before the commit lands; `commit.formatted` lists the files it
touched, so target that text with your next edits.

## 4. Share it

`share_app {app}` returns the URL, share text and a QR code for the family
chat. Open the [Console](/console) to watch the deploy, the logs, and who
visits.

## Rules that bite newcomers

- Read the [Ilha guide](/guides/ilha) before writing any code, and the
  [Void guide](/guides/void) before touching server code.
- No strong profanity in page text, seed data or the app name — the deploy
  is blocked and the previous version keeps serving. See [Moderation](/moderation).
- After deploying server code, check `app_errors`. Undo with `rollback`
  (never counts as a change). Delete with `delete_app {app, confirm}` where
  confirm is the slug typed again; the name stays reserved for 30 days.
- [Limits](/guides/limits): Free gives 1 app and 50 code changes a day.
  When a tool refuses something, relay its `user_message` to your human
  verbatim — never improvise around it.
