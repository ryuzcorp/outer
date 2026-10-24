//! One deploy: worktree materialization, `bun install`/`bun run build`,
//! wrangler config validation, and `celld deploy`.
//!
//! The integration owner declares `pub mod deploy;` on the crate root; this
//! module owns everything under `src/deploy/`.

pub mod pipeline;

pub use pipeline::{
    fail_interrupted, prune_old, DeployOutcome, DeploySettings, Deployer, PurgeTasks, Step,
};
