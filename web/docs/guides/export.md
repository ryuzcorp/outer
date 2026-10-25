---
title: Export guide
description: Download your app's whole code as a zip (Pro).
---

Pro accounts can download any owned app's code as a zip, at the live
version or any earlier commit.

## From your agent

`export_code {app, sha?}` zips the tree at `sha` (default: `main`) and
returns a download `url` with its `sha`, `bytes` and `expires_at`. The
link works for one hour. Owner only; Free accounts get
`payment_required` with an upgrade message to relay.

## From the console

Settings → **Export code**: pick a version (the same shas as the Code
view, live first) and press **Download .zip**. The card shows only for
the app owner, and only on Pro — Free accounts see it disabled with a
Pro badge.

## Details

- The zip holds exactly the committed tree under `<slug>-<shortsha>/`.
- Exports up to 256 MiB; anything bigger is refused with a clear error.
- Download links expire after 1 hour; the stored zip is deleted after
  24 hours.
