---
title: Stay inside quotas and caps
description: Track counted quotas, abuse limits, fleet caps, and overage behavior.
---

## Track counted quotas

| Quota | Free | Pro |
| --- | --- | --- |
| Apps owned | 1 | 10 |
| Code changes / day (UTC) | 50 | 500 |
| D1 databases / app | 1 | 1 |
| R2 buckets / app | 1 | 1 |

1. Charge each `commit` that creates a commit to the app owner, whoever makes it.
2. Exclude the template commit from `create_app` and every `rollback` from counting.
3. Check usage and reset times with `whoami`.
4. Relay `quota_exceeded` plus its verbatim `user_message` to the person.
5. Merge extra databases or buckets; a second `d1_databases` or `r2_buckets` entry fails validation.

## Respect abuse and runtime caps

1. Expect sign-in throttles: 5 OTP emails/hour/address, 20/hour/IP, 3 new accounts/day/IP.
2. Run fleets at 512 MiB (Free) or 1 GiB (Pro), CPU 0.5/1.
3. Expect idle sleep after 30 minutes (Free) or 24 hours; Pro can hold warmth with keep-warm.
4. Expect Free apps at 100% requests, storage, or egress to pause until reset; stored data is kept.

For the person-side walkthrough, see [Limits](/deploy/limits).
