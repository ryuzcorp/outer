---
title: Preview deploys
description: Try a change at a temporary URL before it goes live (Pro).
---

# Preview deploys (Pro)

A preview trials a change at a temporary URL before it goes live.
Commit with `preview: true` and you get a second deployment of the same app
at `https://<slug>--preview.outer.now` — same code pipeline as a live
deploy, but live is untouched until you promote.

```jsonc
// Trial a change:
{ "tool": "commit", "app": "grocery", "message": "Reorder the list", "edits": […], "preview": true }
// → { commit: { sha }, deploy: { status: "success", url: "https://grocery--preview.outer.now" }, preview_url }

// Ship it:
{ "tool": "promote_preview", "app": "grocery" }
// → fast-forwards main to the preview commit, then deploys normally.

// Throw it away:
{ "tool": "discard_preview", "app": "grocery" }
```

## What a preview is

- **One per app.** A new preview commit refreshes the existing preview;
  `discard_preview` (or 24 h without a request) clears it.
- **Same pipeline as live.** `bun install`, `bun run build`, the
  [moderation gate](moderation.md) (profanity blocks a preview exactly like
  a live deploy), D1 migrations, then `celld deploy` — and the same
  `success` / `failed` + log tail / `blocked` + offending lines answers.
- **No production data.** The preview fleet reads a fresh, empty namespace:
  its own bucket prefix, so its D1/KV/R2 state starts empty and migrations
  apply from scratch. Anything the preview writes (test rows, uploads)
  never touches the live database, and promoting does not copy preview
  data — it only ships the code.
- **Same visibility rules.** An unlisted app's preview answers `noindex`,
  same as live. Share the preview URL with your human to click through it.
- **Short-lived.** No request for 24 hours discards the preview
  automatically (fleet stopped, trial state removed). Idle previews sleep
  like idle live fleets and wake on the next request.

## Rules that carry over

- **Pro only.** Preview commits and promotion need the app owner's Pro
  plan; otherwise you get `payment_required`. Discarding always works,
  even off Pro.
- **Counts as a code change.** A preview commit burns one of the owner's
  daily code changes, like a live commit. Promotion does not: it moves a
  ref, it does not write a commit.
- **CAS works on the preview head.** `base` in a preview commit compares
  against the preview tip, not main — live commits never disturb a
  preview's compare-and-swap.
- **Promotion needs a clean line.** If someone committed to live after the
  preview branched, promotion refuses with `conflict` instead of silently
  superseding their change: make a fresh preview from the new live version
  and promote that.
- **Host capacity.** The host holds at most `OUTER_PREVIEW_MAX` previews
  (default 20). Past that, a new preview refuses with `rate_limited` and a
  `retry_after_s` naming when the oldest preview expires; discard an idle
  one or wait.

## In the console

The app's Deploys view shows the active preview — URL, commit, status —
with **Promote** and **Discard** buttons next to it. Promotion and discard
run the same checks as the MCP tools.

## Limits of trials

- The preview URL is one DNS label (`<slug>--preview`), covered by the
  wildcard certificate. The other order (`preview.<slug>…`) is not a
  preview and 404s.
- No app may be named `*--preview`: those slugs are reserved so a live
  URL and a preview URL can never collide.
- Renaming an app with an active preview refuses: discard the preview
  first (trial state is slug-derived and does not move).
- `deploy_status` and `deploy_log` default to live runs; pass
  `preview: true` to follow a preview deploy.
