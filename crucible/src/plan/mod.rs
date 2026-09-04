//! A plan is a versioned DAG of tasks; a deterministic executor runs it.
//!
//! Plans are currently built from engine templates or loaded from human/pack-authored TOML
//! and JSON. `validate` checks the supported version and graph structure before execution.
pub(crate) mod diag;
pub mod exec;
pub mod gate;
pub mod ir;
pub mod runner;
pub mod starlark;
pub mod term_img;
pub mod worktree;

/// What happens when a gate has parked as long as it may. Lives here rather than beside the CLI
/// because the pod renderer emits it as `--park-policy` and the engine parses it back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ParkPolicy {
    /// Idle in place until `--max-park`, then fail the gate as timed out.
    #[default]
    ParkThenDeny,
    /// Idle in place until `--max-park`, then snapshot and exit so a later run can resume.
    ParkThenSuspend,
}

/// The environment variable naming the task a turn runs, set by both the command runner and the
/// agent harness. Engine-provisioned, so [`crate::exposure`] carries it as standing disclosed
/// reach.
pub const TASK_NAME_ENV: &str = "CRUCIBLE_TASK";
