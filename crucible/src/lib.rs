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

/// Support for tests in this crate and the binary that mutate the process environment.
#[doc(hidden)]
pub mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static ENV: Mutex<()> = Mutex::new(());

    /// One lock for every test that sets or reads a process-global environment variable. The
    /// environ is a single global, so per-module locks would not serialize tests in different
    /// modules racing through it. Not reentrant: a fixture that holds it must not take it again.
    pub fn env_lock() -> MutexGuard<'static, ()> {
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }
}
