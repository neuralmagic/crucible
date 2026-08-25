//! A plan is a versioned DAG of tasks; a deterministic executor runs it.
//!
//! Plans are currently built from engine templates or loaded from human/pack-authored TOML
//! and JSON. `validate` checks the supported version and graph structure before execution.
pub(crate) mod diag;
pub mod exec;
pub mod ir;
pub mod runner;
pub mod starlark;
pub mod term_img;
pub mod worktree;
