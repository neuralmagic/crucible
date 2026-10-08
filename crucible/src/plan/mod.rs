//! A plan is a versioned DAG of tasks; a deterministic executor runs it.
//!
//! Plans are currently built from engine templates or loaded from human/pack-authored TOML
//! and JSON. `validate` checks the supported version and graph structure before execution.
pub mod decide;
pub(crate) mod diag;
pub mod exec;
pub mod history;
pub mod ir;
pub mod machine;
pub mod param;
pub mod record;
pub mod resume;
pub mod route;
pub mod runner;
pub mod starlark;
pub mod term_img;
pub mod workflow;
pub mod worktree;

/// The exit code of a run that finished with an invalid verdict, or ended at setup. It is not a
/// crash: the session log already holds the outcome, so a pod wrapper does not restart the run on it.
pub const INVALID_VERDICT_EXIT: u8 = 3;

/// The environment variable naming the task a turn runs, set by both the command runner and the
/// agent harness. Engine-provisioned, so [`crate::exposure`] carries it as standing disclosed
/// reach.
pub const TASK_NAME_ENV: &str = "CRUCIBLE_TASK";

/// Where a consumer finds what its ancestors declared. Under the workspace so a task reaches it
/// with a relative path, and named so it is obviously not the task's own work.
pub const STAGED_INPUTS: &str = "inputs";
