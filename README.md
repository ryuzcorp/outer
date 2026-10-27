# Outer

<img src="web/public/logo.svg" alt="Outer logo" width="72" />

**Your agent's own app cloud.** Tell your agent what you want, and it builds the app and hands you a live link to share.

Your agent connects to Outer over [MCP](https://modelcontextprotocol.io):
it writes the app with [Ilha](https://github.com/ilhajs/ilha) + Void and
commits it, and Outer builds and serves it at `https://<slug>.outer.now`
within seconds.

```
https://outer.now/mcp
```

[Connect your agent](https://outer.now/get-started/connect) ·
[Quickstart](https://outer.now/get-started/quickstart) ·
[Open Console](https://outer.now/console) · [Docs](https://outer.now)

## How it works

Connect your agent, ask for an app, refine it by chatting, and share the
live link. The full loop takes about ten minutes — start at
[Quickstart](https://outer.now/get-started/quickstart).

## Core concepts

Three terms, used everywhere:

- **App** — a web app or website your agent built, live at
  `https://<slug>.outer.now`. Unlisted by default: anyone with the link
  can visit, nobody can find it by browsing.
- **Agent** — your AI agent (Claude, Codex, ChatGPT, …), connected to
  Outer over MCP. The agent writes the code, ships it, and reads back
  the result.
- **Deploy** — one shipped change. Every change waits for its deploy
  and reports back: `success` with the URL, `failed` with the log tail,
  or `blocked` with the exact lines to fix.

## What you can build

Anything the Ilha + Void template can express, backed by D1, KV, Durable
Objects, R2, queues, cron and realtime: family tools, sign-up sheets,
dashboards, games, chat rooms, scheduled digests. Concrete starters with
copy-paste prompts:
[What you can build](https://outer.now/get-started/what-you-can-build).

## Get help

- Ask your agent first. It reads the docs over MCP (the `docs` tool)
  and relays refusal messages verbatim.
- Deploy stuck? The [FAQ](https://outer.now/help/faq) covers sign-in,
  blocked deploys, and cold starts.
- Prompting tips: [how to ask](https://outer.now/get-started/prompting)
  so your agent gets it right the first time.

## Status

Outer is in closed beta. New accounts need an invite code
(`OUTER-XXXX-XXXX`) — without one, sign-in puts you on the waitlist.
Details: [Sign in](https://outer.now/get-started/sign-in).

[Terms](https://outer.now/terms) · [Privacy](https://outer.now/privacy)

## Development

You need Rust (stable) and `git`.

```bash
cargo test
OUTER_STORE=memory cargo run -- serve
curl http://127.0.0.1:8080/ready
```

`OUTER_STORE=memory` keeps bucket objects (git bundles, state snapshots) in process, so they vanish on restart. Only the SQLite file in `OUTER_DATA` persists. That's fine for local development. Production uses a Cloudflare R2 bucket (`OUTER_R2_ENDPOINT`, `OUTER_R2_BUCKET`, `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`). `outer check-config` prints every configuration problem at once.

## License

[Apache-2.0](LICENSE). Parts of Outer are derived from [Noite](https://github.com/ryuzcorp/noite), also Apache-2.0; see [NOTICE](NOTICE).
