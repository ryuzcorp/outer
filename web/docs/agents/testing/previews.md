---
title: Trial changes with preview deploys
description: Ship preview commits, promote or discard them, and respect preview rules.
---

Trial a change before it goes live: commit with `preview: true` to deploy the same app at `https://<slug>--preview.outer.now`. Live stays untouched until promotion.

```jsonc
// Trial a change:
{ "tool": "commit", "app": "grocery", "message": "Reorder the list", "edits": […], "preview": true }
// Ship it:
{ "tool": "promote_preview", "app": "grocery" }
// Throw it away:
{ "tool": "discard_preview", "app": "grocery" }
```

## Run the preview loop

1. Commit with `preview: true` to refresh the app's one preview.
2. Expect the same pipeline as live: install, build, moderation gate, migrations from scratch, then celld deploy.
3. Expect an empty namespace: the preview fleet starts with fresh D1/KV/R2 state and never touches live data.
4. Share the preview URL for click-through; unlisted previews answer `noindex` like live.
5. Promote with `promote_preview` to fast-forward main and deploy normally.
6. Discard with `discard_preview` to stop the fleet and remove trial state.
7. Expect automatic discard after 24 hours without requests.
8. Pass `preview: true` to `deploy_status` and `deploy_log` when following a preview run.

## Respect the preview rules

1. Require the owner's Pro plan for preview commits and promotion; otherwise expect `payment_required`.
2. Count each preview commit as one code change; promotion moves a ref and costs nothing.
3. Compare `base` against the preview tip on preview commits, not main.
4. Keep a clean line for promotion; a later live commit blocks it with `conflict`.
5. Recover from `conflict` by running `discard_preview`, committing a fresh preview from the new live version, then promoting.
6. Respect host capacity `OUTER_PREVIEW_MAX` (default 20); past it expect `rate_limited` with `retry_after_s`.
7. Never name an app `*--preview`; those slugs stay reserved so live and preview URLs never collide.
8. Discard the active preview before renaming; renames with an active preview refuse.

For the person-side walkthrough, see [Preview changes](/testing/preview-deploys).
