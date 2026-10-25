---
title: Limits
description: The quotas Outer enforces in code, and what happens when one is hit.
---

## Enforced quotas

These numbers are constants in the code (`crates/outer/src/mcp/tools/mod.rs`):

| Limit | Free | Pro |
| --- | --- | --- |
| Apps owned | 1 | 10 |
| Code changes / day (UTC) | 50 | 500 |
| D1 databases / app | 1 | 1 |
| R2 buckets / app | 1 | 1 |

A second `d1_databases` or `r2_buckets` entry fails the deploy at its
validation step (`deploy-root`): "Outer apps can have one D1 database and
one R2 bucket for now; merge the extra ones".
Every account is on Free until billing ships — the Pro column shows what
the code already honors for Pro accounts, not something you can buy yet.

`whoami` shows your usage and when it resets. `create_app`'s template
commit and `rollback` never count as code changes: only `commit` calls
that create a commit do, charged to the app **owner** whoever makes them.

When a quota is hit the tool returns `quota_exceeded` with an upgrade
link and a `user_message` — relay it to your human verbatim.

## Abuse limits

On the sign-in path: 5 OTP emails per hour per address, 20 per hour per
IP, 20 client registrations per hour per IP, 3 new accounts per day per
IP (`crates/outer/src/auth/limits.rs`).

## Runtime settings

One deployment-wide setting each (operator config, not per plan): fleets
stop after 30 minutes idle (`OUTER_SLEEP_AFTER_S`), and each fleet is
capped at 512 MiB (`OUTER_FLEET_MEMORY_MB`) with CPU `50000 100000`.
Build and request metering tables exist, but caps and Pro billing arrive
in M2.

## Sources

- Quota constants and counting:
  `crates/outer/src/mcp/tools/mod.rs`
  (`FREE_APP_MAX`, `FREE_CHANGES_PER_DAY`, `record_change`)
- Sign-in limits: `crates/outer/src/auth/limits.rs`
- Fleet settings: `crates/outer/src/config.rs`
