---
title: Custom domains
description: Serve your app from your own domain (Pro, one per app) via Cloudflare for SaaS.
---

Custom domains are a **Pro** feature, **one per app**. `add_domain`
registers your hostname as a Cloudflare for SaaS custom hostname on
Outer’s zone; the proxy then routes that hostname to your app with the
same visibility headers, gates and moderation as your
`<slug>.outer.now` host.

Only the app **owner** can manage domains. Console: Settings →
Custom domain. MCP: `add_domain`, `list_domains`, `remove_domain`.

## Setup

1. Add the domain: `add_domain {app, hostname}` (or the console card).
   The tool returns the DNS records below.
2. At your DNS provider, create the records:
   - `CNAME <hostname> → domains.outer.now` (your hostname’s whole
     name pointing at Outer’s target). Subdomains like
     `groceries.silva.family` work anywhere; an apex domain needs a
     provider with CNAME flattening.
   - `TXT` ownership + certificate records exactly as returned (names
     and values). These prove ownership and let Cloudflare issue the
     certificate; they work before the CNAME cutover.
3. Wait for validation: `list_domains` (or Refresh in the console) shows
   `pending → active`. Both the hostname and the certificate must be
   `active` before the domain serves traffic.

The console’s status badges are live reads from Cloudflare (the GET
endpoint is the source of truth). `failed` carries a reason — usually
the DNS records are missing or wrong; fix them, then refresh.

## Verification states

| Status | Meaning |
| --- | --- |
| `pending` | Registered; hostname validation and/or the certificate are not done yet. Create the DNS records and refresh. |
| `active` | Hostname validated and certificate deployed. The domain serves your app. |
| `failed` | Stuck (e.g. certificate validation timed out). `reason` says why; fix DNS and refresh, or remove and re-add. |

## Removing

`remove_domain` (or Remove in the console) deletes the Cloudflare
hostname, drops the row and purges the host’s cached pages. Deleting
the app removes its domain the same way.

Downgrading to Free removes the domain too — re-add it after
re-upgrading. Renaming an app keeps its domain: the binding follows the
app, not the slug.

## MCP tools

- `add_domain {app, hostname}` → `{domains: [{hostname, status,
  cname_target}]}` plus the human-readable DNS setup in the summary.
  Refusals: `payment_required` (Free plan), `invalid_argument` (bad
  name, a platform/app host, the app already has one, or the name is
  taken).
- `list_domains {app}` → `{domains: [{hostname, status, ssl_status,
  cname_target, txt_records, reason}]}`. Refreshes from Cloudflare
  when reachable, else the stored status.
- `remove_domain {app}` → `{removed, user_message}`.
