---
title: Stay inside quotas and caps
description: Track counted quotas, abuse limits, fleet caps, and overage behavior.
---

## Track counted quotas

| Quota | Free | Pro |
| --- | --- | --- |
| Apps owned | 2 | 20 |
| Code changes / day (UTC) | 40 | 1000 (fair use) |
| Build minutes / day | 30 | 300 |
| Collaborators / app | 1 | 10 |
| D1 database | 100 MB (writes refused at cap, nothing deleted) | 5 GB (writes refused at cap, nothing deleted) |
| R2 files | 250 MB (writes refused at cap, nothing deleted) | 25 GB (writes refused at cap, nothing deleted) |
| Requests / month | Soft 100k (email + upgrade ask, keeps serving) | Soft 5M (never pause a live app) |
| Logs and errors kept | 24 hours | 30 days |
| Backups | Daily, keep 3 | Daily, keep 30 + any-day restore |
| D1 databases / app | 1 | 1 |
| R2 buckets / app | 1 | 1 |

1. Charge each `commit` that creates a commit to the app owner, whoever makes it.
2. Exclude the template commit from `create_app` and every `rollback` from counting.
3. Check usage and reset times with `whoami`.
4. Relay `quota_exceeded` plus its verbatim `user_message` to the person. Refusals name the limit, the reset time, and the upgrade link.
5. Merge extra databases or buckets; a second `d1_databases` or `r2_buckets` entry fails validation.
6. Export is available on every plan (owner only). Collaborators on Free are limited to 1 per app.
7. Downgrade keeps the 2 oldest apps live and stops the rest for 30 days; collaborators beyond 1 per app become read-only.

## Respect abuse and runtime caps

1. Expect sign-in throttles: 5 OTP emails/hour/address, 20/hour/IP, 3 new accounts/day/IP.
2. Run every fleet under the host's memory and CPU caps; there is no per-plan sizing.
3. Expect idle sleep after 15 minutes on Free (cold start on wake); on Pro, keep-warm holds selected fleets awake, otherwise sleep after 30 minutes.
4. Track requests and storage against the soft caps above; request overage only triggers an email and an upgrade ask — never tell the person an app paused over quota.
5. Inactive Free apps are deleted after 90 days with reminders; Pro apps are never deleted while subscribed.

## Keep Free apps alive past 90 days

1. Free apps are deleted after 90 days without owner activity, never while Pro. `whoami`, `list_apps`, and `get_app` show the deadline as `deletes_at`.
2. Count as activity: every authenticated request by the owner (sign-ins, any tool call, Console visits) and tool calls on that app by a collaborator. Never visitor traffic.
3. The person gets reminder emails at 30, 7, and 1 day before the deadline. After the deadline the host serves an inactivity page for 7 days; any owner activity in that week restores the app on its next request. Then it is purged and the slug is claimable again.
4. When the person asks why an app is gone or when it goes away, read `deletes_at` and relay the countdown; to keep it, any tool call counts.

For the person-side walkthrough, see [Limits and quotas](/deploy/limits).
