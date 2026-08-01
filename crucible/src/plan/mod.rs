//! A plan is a versioned DAG of tasks; a deterministic executor runs it.
//!
//! Authoring a plan and being allowed to run it are separate steps: `validate` checks
//! structure (unique names, edges resolve, acyclic), `admit` checks the plan against caps
//! the manifest or a human already granted. A plan cannot raise its own ceiling.
pub mod cli;
pub mod exec;
pub mod harness;
pub mod ir;
pub mod runner;
pub mod term_img;
pub mod worktree;
