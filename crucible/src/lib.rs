//! The crucible library: the manifest vocabulary, the workflow compiler, and the deployment
//! renderers, everything a controller links to produce a render in its own process (RFC-0004).
//! The loop runtime stays in the `crucible` binary.

pub mod command_judge;
pub mod command_world;
pub mod crucible;
pub mod deploy;
pub mod diagram;
pub mod duration;
pub mod errors;
pub mod exposure;
pub mod flow;
pub mod manifest;
pub mod openshell;
pub mod outputs;
pub mod plan;
pub mod task_judge;
pub mod turn_trace;

/// One crate-wide lock for tests that mutate a process-global env var. The environ is a single
/// global, so per-module locks wouldn't serialize tests in different modules racing through it.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
