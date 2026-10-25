---
title: celld guide
description: How celld runs your app — the deploy pipeline, fleets, and runtime rules.
---

celld runs your built app: one fleet (isolated worker pool) per app, one
uid per app, with its own memory, CPU and egress accounting.

## The deploy pipeline

Every `commit` runs these steps (each times out after 120 s):

1. **Checkout** the commit into the app's persistent work directory.
2. **`bun install`** — skipped when `package.json` and `bun.lock` hash
   the same as the last successful install and `node_modules` exists.
   With a lockfile present the install uses `--frozen-lockfile`.
3. **`bun run build`** — skipped when `package.json` has no
   `scripts.build`.
4. **Validate `dist/wrangler.json`** (else `wrangler.jsonc` /
   `wrangler.json` at the repo root): only keys celld accepts pass —
   `name`, `main`, `compatibility_date`, `durable_objects`, `migrations`,
   `assets`, `services`, `triggers`, `vars`, `d1_databases`,
   `kv_namespaces`, `queues`, `workflows`, `r2_buckets` and a few build
   keys. Anything else fails the deploy naming the key.
5. **Review:** the pre-live gate (see the moderation guide).
6. **Deploy** with celld, then poll the fleet's health. A deploy that
   fails any step or the health check never takes over — the previous
   version keeps serving. Nothing is uploaded before the gate passes.

The template's `vite.config.ts` derives the deploy-root
`dist/wrangler.json` from Void's own `dist/ssr/wrangler.json` — never
hand-write it: set bindings in `void.config.ts` and rebuild.

## Runtime rules

- No module-scope state: every request must read fresh state.
- `fetch` handlers only. No Workers AI, Vectorize or Cache bindings.
- Outbound traffic from an app can never reach loopback, private ranges,
  link-local or metadata addresses, and SMTP ports 25/465/587 are
  blocked — send email through an HTTP API with a secret.
- Responses an app fetches count against its own memory: stream large
  downloads instead of buffering them.
- A fleet stops after about 30 minutes without requests (the deployment's
  idle setting) and wakes on the next request; a sleeping app adopts the
  newest deploy when it wakes.
 ## Secrets

 Apps that call external APIs keep keys outside the code with three MCP
 tools. `set_secret {app, key, value}` stores a value encrypted at rest
 (AES-256-GCM, one key per app derived from the platform secret);
 `list_secrets {app}` shows names and when each was set; `unset_secret
 {app, key}` removes one. Values are never returned, logged, or echoed —
 not even to the app's owner.

 Rules: names match `^[A-Z_][A-Z0-9_]{0,63}$`; platform-reserved names
 (`PORT`, `AWS_*`, `OUTER_*`, `LD_*`, …) are refused; values hold at most
 4 KiB; at most 50 secrets per app.

 Each deploy decrypts the values into the deploy-root config `vars`, so
 the app reads them as Worker env vars (`env.MY_KEY`). They apply on the
 next deploy: setting a secret never touches the running fleet. A
 same-named `vars` entry in code loses to the secret, and the deploy log
 names the overwrite. Builds never see secrets — `bun install` and `bun
 run build` run without them, so build scripts must not require them.

## PWA

Every app is installable by default: `vite-plugin-pwa` (`autoUpdate`,
`generateSW`) owns the manifest, the service worker and all icons, built
into `dist/client/` from the single source `public/favicon.svg` — edit
the SVG and the build regenerates matching PNGs. Never commit icon PNGs
or a hand-written manifest. The SSR app has no `index.html`, so
`pages/_pwa.ts` registers the service worker on the client (manual
`virtual:pwa-register`) and `void.config.ts` `head` carries the
manifest/icon links.

What not to break: never set `navigateFallback` or precache HTML —
navigations go to the network so reloads always show fresh D1 data.
Rename the app via `manifest` `name` in `vite.config.ts` + `head` `title`
in `void.config.ts`.

## Sources

- The template's build config (celld config derivation):
  <https://github.com/ryuzcorp/outer-template/blob/32d413655a4f050485c14d04e9476325619f2b32/vite.config.ts>
- The PWA registration:
  <https://github.com/ryuzcorp/outer-template/blob/32d413655a4f050485c14d04e9476325619f2b32/pages/_pwa.ts>
- celld's runtime docs: <https://celld.dev/docs/>
- Outer's deploy pipeline: `crates/outer/src/deploy/pipeline.rs`
