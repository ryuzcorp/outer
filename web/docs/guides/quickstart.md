---
title: Quickstart
description: Create an app, change code, share the link — the Outer loop in five steps.
---

Outer hosts your Ilha app: you describe changes, your agent calls Outer's
tools, Outer builds and deploys, and you get a live link to share.

1. `create_app {slug}` — starts a live Ilha + Void app at
   `https://<slug>.outer.now` in seconds. Unlisted by default (not indexed).
2. `docs {topic: "ilha"}` — read this before writing any code.
3. `list_files` / `read_file` — look at the code.
4. `commit {app, message, edits}` — change code with exact-match `edits`.
   Every code tool waits for the deploy and returns `success` + URL,
   `failed` + log, or `blocked` + the lines to fix. Written/edited code is
   formatted with oxfmt before the commit lands: `commit.formatted` lists
   the files it changed, so target that text with your next edits.
5. `share_app {app}` — URL, share text and QR code for the family chat.

Rules: no strong profanity in page text, seed data or the app name — the
deploy will be blocked and the previous version keeps serving. After
deploying server code, check `app_logs`. Undo with `rollback`.
Delete with `delete_app {app, confirm}` (confirm is the slug, typed again);
the name stays reserved for 30 days.
Relay any `user_message` you receive to your human verbatim.
