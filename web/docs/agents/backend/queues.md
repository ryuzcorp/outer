---
title: Queues for agents
description: Background work with queue files — declaration, producing, batch limits, and deploy.
---

**Verdict: works.** Declare a queue file, send from anywhere, consume in batches. Standard Cloudflare Queues under the hood — no extra service. The person-facing guide is [Background jobs](/backend/background-jobs).

## Declare the queue

One file per queue under `queues/`; the path sets the name (`order/notifications.ts` → `order-notifications`):

```ts
// queues/emails.ts
import { defineQueue } from "void";

export default defineQueue<{ to: string; subject: string }>(async (batch, env) => {
  for (const msg of batch.messages) {
    await sendDigest(env, msg.body); // your function
    msg.ack();
  }
});

export const maxBatchSize = 50;
export const maxBatchTimeout = 10;
export const maxRetries = 5;
export const retryDelay = 30;
```

Only one consumer may read a queue. Un-acked messages ack on success and retry on throw (first call wins); over `maxRetries` they are permanently deleted — Void configures no dead-letter queue.

## Produce from anywhere

```ts
import { queues } from "void/queues";

await queues.emails.send({ to, subject }); // typed from defineQueue<T>
```

Batches cap at 100 messages / 256 KiB total, one message at 128 KiB. Delivery is at-least-once and unordered — key work by the stable `msg.id` and keep consumers idempotent. Retention is 4 days.

## Deploy with the app

Bindings come from the built `dist/wrangler.json`: `queues` is an accepted deploy key, so the queue deploys on every account. Rebuild after editing queue files; consumers deploy with the app and scale with it. Trigger local dispatch manually with the dev server's `POST /__void/queue` endpoint — queued messages do not run on their own.
