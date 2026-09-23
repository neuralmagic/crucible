//! A plan is a versioned DAG of tasks; a deterministic executor runs it.
//!
//! Plans are currently built from engine templates or loaded from human/pack-authored TOML
//! and JSON. `validate` checks the supported version and graph structure before execution.
pub(crate) mod diag;
pub mod exec;
pub mod ir;
pub mod machine;
pub mod route;
pub mod runner;
pub mod starlark;
pub mod term_img;
pub mod workflow;
pub mod worktree;

/// The environment variable naming the task a turn runs, set by both the command runner and the
/// agent harness. Engine-provisioned, so [`crate::exposure`] carries it as standing disclosed
/// reach.
pub const TASK_NAME_ENV: &str = "CRUCIBLE_TASK";

/// The environment variable naming the file an agent turn writes its result to, relative to the
/// root it runs in. Engine-provisioned alongside [`TASK_NAME_ENV`].
pub const TASK_RESULT_ENV: &str = "CRUCIBLE_TASK_RESULT";

/// Where a consumer finds what its ancestors declared. Under the workspace so a task reaches it
/// with a relative path, and named so it is obviously not the task's own work.
pub const STAGED_INPUTS: &str = "inputs";
