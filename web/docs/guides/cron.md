---
title: Cron guide
description: Scheduled work with crons/*.ts — Outer wakes the app and runs the handler. Works today.
---

**Verdict: works.** Write a schedule file, commit it, and Outer registers
the schedule on deploy, wakes the app at each occurrence, and runs the
Worker's `scheduled` handler.

## 1. Write the job

One file per job under `crons/` (`_`-prefixed files are ignored). The path
is just a name; the `cron` export is the schedule (Cloudflare 5-field
syntax, UTC, 1-minute resolution):

```ts
// crons/hourly-heartbeat.ts
import { defineScheduled } from "void";

export const cron = "0 * * * *";

export default defineScheduled(async (controller, env) => {
  await env.KV.put("jobs:last-heartbeat", controller.scheduledTime.toString());
});
```

An array of expressions runs the job once per matching occurrence. Jobs
sharing one expression each run once per invocation.

## 2. Deploy — Outer registers it

The build carries `triggers.crons` into `dist/wrangler.json`; on deploy
Outer replaces the app's schedules with the built config in one
transaction (`crates/outer/src/cron.rs`). Missing `triggers`/`crons` means
no schedules; a misshapen expression fails the deploy naming it.

Every 15 s Outer finds schedules due within the next 90 s and wakes those
fleets, so the running fleet fires the `scheduled` handler on time. A
sleeping app fires nothing until woken — waking is Outer's job, not yours.

## 3. Rules worth knowing

- Handlers for one app run **one at a time**; a slow handler delays the next
  occurrence. Keep jobs short or fan out to a [queue](/guides/queues).
- A throw retries with backoff (give up after 6 failures or at the next
  occurrence); `controller.noRetry()` stops retries.
- After downtime Outer runs **one** missed occurrence and skips the rest —
  no catch-up storm. Design jobs to be idempotent anyway.
- Plan floors (Free: hourly or slower; Pro: down to 1 min) are spec, not
  yet enforced in code — stay inside your plan's floor regardless.

Test locally with `POST /__void/scheduled` and Void's dev trigger token;
schedules never fire from the dev server on their own.

## Sources

- Void cron guide: <https://void.cloud/guide/jobs>
- Outer wake loop: `crates/outer/src/cron.rs`
- celld cron triggers: <https://celld.dev/docs/services/cron-triggers>
