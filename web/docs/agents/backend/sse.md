---
title: SSE for agents
description: One-way live streams with eventStream — keep-alive, resume from D1, and when to use queues or sockets.
---

**Verdict: works.** Server-sent events are plain HTTP streaming — `text/event-stream` with no bindings and nothing to declare. The person-facing guide is [Realtime](/backend/realtime).

## Stream from a route

```ts
// routes/api/scores.ts
import { defineHandler } from "void";
import { eventStream, getLastEventId } from "void/sse";

export const GET = defineHandler((c) => {
  const lastId = getLastEventId(c.req.raw); // null on first connect
  return eventStream(
    async (stream) => {
      await stream.send({
        id: "ready-1",
        event: "ready",
        data: { now: Date.now(), resumedFrom: lastId },
      });
      await stream.comment("connected");
      await stream.closed; // stay open until the client disconnects
    },
    { signal: c.req.raw.signal, keepAlive: { intervalMs: 5000, comment: "ping" } },
  );
});
```

```ts
// Client
import { connectEventStream } from "void/sse/client";
```

Gate the route with the ordinary `requireAuth(c)` when the stream is private. Support `Last-Event-ID` resume (`getLastEventId(c.req.raw)`, null on first connect) by replaying missed events from D1 after `lastId` — the server keeps no per-client history for you.

## Send keep-alive — mandatory

Cloudflare's edge answers 524 past its ~125 s proxy read timeout on an idle response. The `keepAlive` comment every few seconds beats it — never stream silently. Cross-request pub/sub belongs in [queues](/agents/backend/queues); two-way traffic belongs in [WebSockets](/agents/backend/websockets).
