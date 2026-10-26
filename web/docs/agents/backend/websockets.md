---
title: Add realtime rooms
description: Realtime rooms with room route files — schemas, auth, reconnect, and route identity.
---

**Verdict: works.** Define a room route, connect from the client, and Outer passes WebSocket upgrades straight through the app proxy to the fleet. The person-facing guide is [Show updates as they happen](/backend/realtime).

## Define a room

One route file per room (`index/[id]/[...slug]` nesting allowed; `.dev`/`.prod` env suffixes). Each room instance is backed by a Durable Object, a standard binding declared for you:

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

Events are JSON with a validated shape: no raw string/binary frames. Valibot schemas are required. Auth freezes at connect (`ctx.user`); re-check bans inside `onMessage`. Cross-origin sockets stay anonymous unless the origin is trusted.

## Reconnect with backoff — required

Sockets die on deploy and on fleet sleep: every deploy moves the fleet, and an idle fleet stops after ~15 minutes on Free (~30 on Pro without keep-warm). Your client must reconnect with backoff and re-fetch missed messages from D1. Hibernated Durable Object sockets survive short idles but never a move. Code for the disconnect.

## Keep route identity on moves

Nothing to declare: the route file is the config. Rebuild and commit; the room serves at the same host as the app. One room per file keeps failure domains small. Moving a route keeps its state only when you carry its identity: run `void info` first for the `name:` to add to `defineRoom`, keep the parameter names, and use `key:` to preserve the old key when a parameter is renamed. Otherwise the moved route selects a new Durable Object and looks empty.
