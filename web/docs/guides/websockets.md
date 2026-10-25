---
title: WebSockets guide
description: Realtime rooms and sockets with *.ws.ts routes — works on Outer today, with reconnect notes.
---

**Verdict: works.** Define a room route, connect from the client, and Outer
passes WebSocket upgrades straight through the app proxy to the fleet.

## 1. Define a room

One route file per room (`index/[id]/[...slug]` nesting allowed;
`.dev`/`.prod` env suffixes). Each room instance is backed by a Durable
Object — standard binding, declared for you:

```ts
// routes/chat/[room].ws.ts
import * as v from "valibot";
import { defineRoom } from "void/ws";

// Validation schemas — install with `bun add valibot` if not present.
const ClientMessage = v.object({ type: v.literal("chat.message"), text: v.string() });
const ServerMessage = v.object({ type: v.literal("chat.message"), text: v.string() });

export default defineRoom({
  messages: { client: ClientMessage, server: ServerMessage },
  async onBeforeConnect(ctx) {
    if (!ctx.user) return new Response("Unauthorized", { status: 401 });
  },
  async onMessage(ctx, event) {
    await ctx.room.broadcast({ type: "chat.message", text: event.text });
  },
});
```

```tsx
// Client
import { connect } from "void/ws";

const socket = connect("/chat/:room", { params: { room: "general" } });
```

Events are JSON with a validated shape — no raw string/binary frames.
Auth freezes at connect (`ctx.user`); re-check bans inside `onMessage`.
Cross-origin sockets stay anonymous unless the origin is trusted.

## 2. Reconnect — required, not optional

Sockets die on deploy and on fleet sleep: every deploy moves the fleet, and
an idle fleet stops after ~30 minutes. The client **must** reconnect with
backoff and re-fetch missed messages from D1. Hibernated Durable Object
sockets survive short idles but never a move — code for the disconnect.

## 3. Deploy

Nothing to declare: the route file is the config. Rebuild and commit; the
room serves at the same host as the app. One room per file keeps failure
domains small. Moving a route keeps its state only when you carry its
identity: run `void info` first for the `name:` to add to `defineRoom`,
keep the parameter names, and use `key:` to preserve the old key when a
parameter is renamed — otherwise the moved route selects a new Durable
Object and looks empty.

## Sources

- Void WebSockets guide: <https://void.cloud/guide/websockets>
- celld WebSockets / Durable Objects: <https://celld.dev/docs/services/durable-objects>
