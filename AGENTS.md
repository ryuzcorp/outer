# Outer: agent working agreement

Outer is a personal software forge for AI agents. An agent talks MCP to Outer, Outer builds the user's Ilha + Void app and runs it on [celld](https://celld.dev), and serves it at `https://<slug>.outer.now`.

- **Design record:** the maintainer's spec at `~/Developer/vault/outer/spec.md` (not in this repo). Read it before changing behavior, and say so when a change departs from it.
- **Status:** prototype, milestone M0 (foundations).

## Commands

```bash
cargo fmt --all --check            # `cargo fmt --all` fixes it
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
OUTER_STORE=memory cargo run -- serve   # http://127.0.0.1:8080/health
```

CI (`.github/workflows/ci.yml`) runs exactly the first three. A PR is ready when they pass locally.

## Layout

| Path | What |
| --- | --- |
| `src/main.rs` | Boot: config → store → snapshot restore → db → HTTP, then graceful shutdown |
| `src/config.rs` | The one `Config`, read from env. Every variable has one name, and `validate` lists all problems |
| `src/db/`, `migrations/` | SQLite pool and append-only migrations (never edit a shipped migration) |
| `src/http/` | Axum router: `/health`, `/ready` (MCP, OAuth and the app proxy come later) |
| `src/store/` | `Store` (R2 over S3, or in-memory) and the SQLite snapshot to the bucket |
| `src/git/` | Each app's repo: a bucket WAL (tip bundle + `MANIFEST.json`) and worktree-free commits |
| `src/fleet/` | Sandboxed processes, cgroup v2 leaves, nft egress rules, the celld supervisor |
| `docker/` | The production image |

## Rules

- **Borrowed code.** Outer lifts specific functions from Noite (`github.com/ryuzcorp/noite`, `apps/runner/src/host/`). A lifted file starts with `// Derived from noite apps/runner/src/host/<file>.rs @ <sha> (Apache-2.0).` and one line saying what changed. Lift a function, not a feature: anything Outer's spec doesn't have stays in Noite.
- **No global state.** Modules take explicit dependencies (`&Store`, paths, small config structs). `src/config.rs` is the only place that reads the environment.
- **Errors.** `anyhow` inside modules. Typed `thiserror` enums where a caller must branch (for example `git::commit::CommitError`).
- **Style** (Noite's "ryu" dialect): `let … else` over `?` inside expressions, positional `format!` arguments, early returns.
- **Tests.** Deterministic, no network, no root. Privileged paths (uid drop, real cgroupfs, nft) are `#[ignore]` with a reason. Test behavior a caller would notice, never wiring.
- **Git.** One branch and one PR per item. Never push to `main`. The owner merges.
