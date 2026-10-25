---
title: SSE guide
description: One-way live streams with eventStream — works on Outer today with keep-alive.
---

**Verdict: works.** Server-sent events are plain HTTP streaming —
`text/event-stream` with no bindings and nothing to declare.

## 1. Stream from a route

```ts
// routes/api/scores.ts
import { eventStream } from "void/sse";

export const GET = defineHandler((c) =>
  eventStream(
    async (stream) => {
      await stream.send({ event: "ready", data: { now: Date.now() } });
      await stream.comment("connected");
      await stream.closed; // stay open until the client disconnects
    },
    { signal: c.req.raw.signal, keepAlive: { intervalMs: 5000, comment: "ping" } },
  ),
);
```

```ts
// Client
import { connectEventStream } from "void/sse/client";
```

Gate the route with the ordinary `requireAuth(c)` when the stream is
private, and support `Last-Event-ID` resume (`getLastEventId(req)`) by
replaying from D1 — the server keeps no per-client history for you.

## 2. Keep-alive is mandatory

The app proxy times out after 60 s with no progress, and Cloudflare's edge
caps a response start at 100 s. The `keepAlive` comment every few seconds
beats both — never stream silently. Cross-request pub/sub belongs in
[queues](/guides/queues); two-way traffic belongs in
[WebSockets](/guides/websockets).

## Sources

- Void SSE guide: <https://void.cloud/guide/sse>
