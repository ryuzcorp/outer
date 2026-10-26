---
title: Serve from a custom domain
description: Add, verify, and remove one Pro custom domain per app.
---

Custom domains are Pro, one per app, and owner-only. The proxy routes the hostname to the app with the same visibility, gates, and moderation as `<slug>.outer.now`.

## Point DNS at Outer

1. Run `add_domain {app, hostname}` and copy the returned DNS records.
2. Create `CNAME <hostname> → domains.outer.now` at your DNS provider.
3. Create the returned `TXT` ownership and certificate records exactly as shown.
4. Poll `list_domains` until hostname and certificate both read `active`.
5. Fix DNS and refresh when status reads `failed` with a reason.

| Status | Meaning |
| --- | --- |
| `pending` | Registered; validation or certificate work is incomplete. |
| `active` | Hostname validated and certificate deployed; serves traffic. |
| `failed` | Stuck; `reason` names the cause. |

## Keep ownership rules straight

1. Manage domains as the app owner only, via `add_domain`, `list_domains`, `remove_domain`.
2. Expect downgrading to Free or deleting the app to remove the domain.
3. Keep the domain across renames; the binding follows the app, not the slug.

For the person-side walkthrough, see [Use a custom domain](/deploy/custom-domains).
