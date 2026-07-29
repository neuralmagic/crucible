//! `crucible scope`: the pipeline runner over a domain pack, hand-written or agent-drafted.
//! Stages: [ingest] (resolve the goal) -> [propose] (an agent turn drafts the pack, `--propose`
//! only) -> [validate] (`crucible check`) -> [freeze] (write `SCOPE.md` with the pack's `RunIdentity`
//! digest). No isolation preflight (S3), no draft-PR approval (S4), the freeze report names them
//! pending.
//!
//! Split into [`pipeline`] (the `Stage` trait + `Ingest`/`Propose`/`Validate`/`Freeze` + the
//! propose/refine/gaming-review machinery), [`pack`] (the pack-directory filesystem + freeze
//! rewrite helpers), [`transcript`] (the preserved session NDJSON), [`progress`] (the `--marker`
//! progress/activity beats), and [`cli`] (`ScopeArgs`/`execute`/`run`, the command's entry point).

mod cli;
mod pack;
mod pipeline;
mod progress;
mod transcript;

pub use cli::{ScopeArgs, run};
pub use pipeline::ProposeTier;
