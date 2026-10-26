---
title: Schedule repeating work
description: Scheduled work with crons files — syntax, deploy registration, wake loop, and handler rules.
---

**Verdict: works.** Write a schedule file, commit it, and Outer registers the schedule on deploy, wakes the app at each occurrence, and runs the Worker's `scheduled` handler. The person-facing guide is [Run work on a schedule](/backend/scheduled-jobs).

## Write the job
One file per job under `crons/` (`_`-prefixed files are ignored). The path serves only as a name; the `cron` export is the schedule (Cloudflare 5-field syntax, UTC, 1-minute resolution):

```ts
// crons/hourly-heartbeat.ts
import { defineScheduled } from "void";

export const cron = "0 * * * *";

export default defineScheduled(async (controller, env) => {
  await env.KV.put("jobs:last-heartbeat", controller.scheduledTime.toString());
});
```

An array of expressions runs the job once per matching occurrence. Jobs sharing one expression each run once per invocation.

## Deploy to register

The build carries `triggers.crons` into `dist/wrangler.json`. On deploy Outer replaces the app's schedules with the built config in one transaction. Missing `triggers`/`crons` means no schedules; a misshapen expression fails the deploy naming it. Every 15 s Outer finds schedules due within the next 90 s and wakes those fleets, so the running fleet fires the `scheduled` handler on time. A sleeping app fires nothing until woken. Waking is Outer's job, not yours.

## Follow the handler rules

| Rule | Detail |
| --- | --- |
| One at a time | Handlers for one app run one at a time; a slow handler delays the next occurrence. Keep jobs short or fan out to a [Queue background work](/agents/backend/queues) |
| Throw retries | A throw retries with backoff; give up after 6 failures or at the next occurrence; `controller.noRetry()` stops retries |
| One missed occurrence | After downtime Outer runs one missed occurrence and skips the rest — no catch-up storm; design jobs idempotent anyway |
| Plan floors | Free: hourly or slower; Pro: down to 1 min (spec, not yet enforced in code). Stay inside your plan's floor regardless |

Test locally with `POST /__void/scheduled` and Void's dev trigger token; schedules never fire from the dev server on their own.
