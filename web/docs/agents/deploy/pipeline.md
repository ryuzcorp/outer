---
title: Follow the deploy pipeline
description: Move commits through queue, build, gate, migrations, and celld deploy.
---

## Move through each pipeline step

1. Queue behind the host's single build semaphore (sized at half the CPUs); there is no per-owner or per-plan priority.
2. Check out the commit into the app's work directory with `checkout --force clean -ffdx -e node_modules`.
3. Run `bun install` (with `--frozen-lockfile` when a lockfile is present), skipped on matching `package.json`/`bun.lock` hashes; each step times out after 120 s.
4. Run `bun run build`; wrangler validation rejects unknown config keys by name (full list: [Accepted config keys](/agents/platform/config-keys)).
5. Pass the moderation gate; nothing uploads before it passes.
6. Apply D1 migrations scoped by key: before upload when a live version exists, otherwise upload, wake, then migrate.
7. Deploy with celld, then poll fleet health; a failed step or health check keeps the previous version serving.
8. Expect wiped worktrees: sleep, failed builds, and deletes remove everything except `node_modules`.

## Watch a deploy to its terminal state

1. Watch `commit` move from `queued` through terminal states.
2. Set `OUTER_TOOL_WAIT_S` (default 50 s) as the status wait; past it expect `building` and poll with `deploy_status wait: true`.
3. Read outcomes as `success`, `failed` (step plus error plus ≤8 KiB tail plus structured `diagnostics` parsed from tsc, oxlint, Vite/esbuild and `bun test` output), or `blocked` (gate findings).

## Verify without deploying

`check {app}` runs install, build, typecheck (`bun x tsc --noEmit` when the template has `tsconfig`), lint (oxlint when present) and `bun test` over the staged draft in the build sandbox. It never deploys, never spends a code change, and charges build minutes. It returns `pass`/`fail` with the same structured `diagnostics` a failed deploy carries.

For the person-side walkthrough, see [How deploys work](/deploy/how-deploys-work).
