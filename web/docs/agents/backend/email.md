---
title: Send email with Resend
description: Send mail through Resend over HTTP — Void's native sender is not available on Outer.
---

**Verdict: not available natively — use Resend.** Void's `sendEmail` needs Cloudflare's `send_email` binding, which is not an accepted deploy key on Outer: a config declaring it fails deploy validation naming the key. SMTP ports 25/465/587 are blocked at the fleet edge. Mail through an HTTP email API such as Resend with your own key instead. The person-facing guide is [Send email from your app](/backend/sending-email).

## Store the key

Never commit it. Set it outside the code and read it as a Worker env var on the next deploy ([Secrets](/agents/security/secrets)):

- `set_secret {app, key: "RESEND_API_KEY", value: "re_…"}` (MCP), or Console → Secrets.

## Send over HTTP

```ts
// routes/api/invite.ts
import { defineHandler } from "void";
import { env } from "cloudflare:workers";

export const action = defineHandler(async (c) => {
  const body = await c.req.json<{ to: string }>();
  if (typeof body.to !== "string" || !body.to.includes("@")) {
    return c.json({ errors: { to: "Enter a valid email address." } }, 422);
  }
  const res = await fetch("https://api.resend.com/emails", {
    method: "POST",
    headers: {
      Authorization: `Bearer ${env.RESEND_API_KEY}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      from: "Grocery <list@updates.example.com>",
      to: body.to,
      subject: "You're invited",
      text: "Join our grocery list: https://grocery.outer.now",
    }),
  });
  if (!res.ok) return c.json({ errors: { to: "Send failed — check the address." } }, 422);
  return {};
});
```

Use a sender domain you control (verified in Resend), and validate the recipient before sending. Bounced spam burns the domain.

## Plan for inbound

Inbound email needs a receiving service in front (Resend inbound, Cloudflare Email Routing on your own zone) forwarding to an app route as HTTP. There is no MX path into `<slug>.outer.now`.
