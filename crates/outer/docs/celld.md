# celld guide

celld runs your built app: one fleet (isolated worker pool) per app, one
uid per app, with its own memory, CPU and egress accounting.

## What runs

- The deploy builds with `bun install` (skipped when the lockfile is
  unchanged) and `bun run build`, then validates `dist/wrangler.json`.
- The fleet serves `dist/` and wakes on first request after sleeping
  (30 min idle on Free, 24 h on Pro). A sleeping app adopts the newest
  deploy when it wakes.
- Health is polled after every deploy; a deploy that fails its health
  check never takes over — the previous version keeps serving.

## Config (`dist/wrangler.json`)

Only keys celld accepts pass validation. In v1:

- `bindings`: D1, KV and Durable Objects on all plans; R2 (file uploads),
  Queues and Workflows need Pro. Anything else fails the deploy with a
  clear error.
- `crons`: minimum interval 1 h on Free, 1 min on Pro.
- No Workers AI, Vectorize or Cache bindings anywhere.

## Runtime rules

- No module-scope state. `fetch` handlers only.
- Outbound traffic: 5 GB/month on Free, 100 GB/month on Pro; new
  connections capped per second. Loopback, private ranges, link-local,
  metadata addresses and SMTP ports (25/465/587) are blocked for every app.
- Memory: 512 MiB (Free) / 1 GiB (Pro), no swap. CPU: 0.5 / 1.0.
- Build time: 60 min/day (Free) / 600 min/day (Pro); concurrent builds per
  account: 1 / 2.
