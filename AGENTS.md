# Outer: agent working agreement

Outer is a personal software forge for AI agents. An agent talks MCP to Outer, Outer builds the user's Ilha + Void app and runs it on [celld](https://celld.dev), and serves it at `https://<slug>.outer.now`.

- **Design record:** the maintainer's spec at `~/Developer/vault/outer/spec.md` (not in this repo). Read it before changing behavior, and say so when a change departs from it.
- **Status:** live at `outer.now` (M0 done); milestone M1 (owner alpha) in progress.

## Commands

```bash
cargo fmt --all --check            # `cargo fmt --all` fixes it
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
OUTER_STORE=memory cargo run -- serve   # http://127.0.0.1:8080/health
```

Localhost `serve` warns and runs without the per-app nft egress policy
(`table inet outer` needs root + `CAP_NET_ADMIN`); set
`OUTER_EGRESS_FIREWALL=off` to silence it. Production refuses to boot
without the policy.

CI (`.github/workflows/ci.yml`) runs exactly the first three. A PR is ready when they pass locally.

## Hostile suite (spec §12.7 beta gate)

Hostile fixtures live in `e2e/hostile/` (`manifest.json` names every probe;
`worker.js` + `build-probe.mjs` + `r2-probe.mjs` try to escape). The harness
is `crates/outer/tests/hostile.rs`, in two layers:

- **Safe** (`cargo test --locked hostile`, runs in CI on every PR touching
  `fleet/`, `deploy/`, `store/credentials.rs` or the Dockerfile): asserts the
  generated sandbox config for each fixture category — nft ruleset, cgroup
  limits, `fleets/{slug}/` credential scope, reserved-env filter,
  loopback-only fleet command, data-dir layout.
- **Privileged** (`cargo test --locked -- --ignored hostile`, VPS only via
  `sudo scripts/hostile-suite.sh`): runs the real fixtures through
  `run_sandboxed` as an app uid in a real cgroup leaf with the nft table
  installed, plus the R2 scope check with scoped creds (`--r2`). Needs root,
  cgroup v2 with delegation, nft and bun/node; anything missing skips loudly
  instead of passing.

The `worker.js` `/probes` check needs a live celld fleet with a deployed app
and stays a manual VPS step (see the script header). A PR that changes escape
relevant behavior adds a fixture probe first, then the containment.

## Layout

| Path | What |
| --- | --- |
| `crates/outer/src/main.rs` | Boot: config → store → snapshot restore → db → HTTP, then graceful shutdown |
| `crates/outer/src/config.rs` | The one `Config`, read from env. Every variable has one name, and `validate` lists all problems |
| `crates/outer/src/db/`, `crates/outer/migrations/` | SQLite pool and append-only migrations (never edit a shipped migration) |
| `crates/outer/src/http/` | Axum router: operator probes on the platform host, app proxy fallback for app hosts |
| `crates/outer/src/store/` | `Store` (R2 over S3, or in-memory) and the SQLite snapshot to the bucket |
| `crates/outer/src/git/` | Each app's repo: a bucket WAL (tip bundle + `MANIFEST.json`) and worktree-free commits |
| `crates/outer/src/fleet/` | Sandboxed processes, cgroup v2 leaves, nft egress rules, the celld supervisor |
| `e2e/hostile/`, `crates/outer/tests/hostile.rs` | Hostile suite (§12.7): fixtures, manifest, two-layer harness |
| `scripts/hostile-suite.sh` | Integrator entry for the privileged hostile layer (VPS) |
## Rules

- **Borrowed code.** Outer lifts specific functions from Noite (`github.com/ryuzcorp/noite`, `apps/runner/src/host/`). A lifted file starts with `// Derived from noite apps/runner/src/host/<file>.rs @ <sha> (Apache-2.0).` and one line saying what changed. Lift a function, not a feature: anything Outer's spec doesn't have stays in Noite.
- **No global state.** Modules take explicit dependencies (`&Store`, paths, small config structs). `crates/outer/src/config.rs` is the only place that reads the environment.
- **Errors.** `anyhow` inside modules. Typed `thiserror` enums where a caller must branch (for example `git::commit::CommitError`).
- **Style** (Noite's "ryu" dialect): `let … else` over `?` inside expressions, positional `format!` arguments, early returns.
- **Tests.** Deterministic, no network, no root. Privileged paths (uid drop, real cgroupfs, nft) are `#[ignore]` with a reason. Test behavior a caller would notice, never wiring.
- **Git.** One branch and one PR per item. Never push to `main`. The agent squash-merges its own PR once CI is green and review threads are handled (owner, 2026-10-23); an item is done only when it is on `main`.
