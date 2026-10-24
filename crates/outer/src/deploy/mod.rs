//! One deploy: worktree materialization, `bun install`/`bun run build`,
//! wrangler config validation, D1 migrations, and `celld deploy`.
//!
//! The integration owner declares `pub mod deploy;` on the crate root; this
//! module owns everything under `src/deploy/`.

pub mod migrate;
pub mod pipeline;

pub use pipeline::{
    fail_interrupted, prune_old, scrub_interrupted_injections, DeployOutcome, DeploySettings,
    Deployer, PurgeTasks, Step,
};
