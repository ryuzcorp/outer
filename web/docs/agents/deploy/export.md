---
title: Export app code as a zip
description: Download any owned commit as a time-limited zip (Pro, owner-only).
---

## Export one version

1. Run `export_code {app, sha?}`; `sha` defaults to `main`.
2. Require the owner's Pro plan; Free accounts get `payment_required`.
3. Download from the returned `url` with its `sha`, `bytes`, and `expires_at`.
4. Expect the zip to hold exactly the committed tree under `<slug>-<shortsha>/`.
5. Stay under 256 MiB; larger trees refuse with a clear error.
6. Download within 1 hour; the stored object is deleted after 24 hours.

For the person-side walkthrough, see [How deploys work](/deploy/how-deploys-work).
