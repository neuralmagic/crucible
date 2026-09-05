//! Fixtures the test modules share: a scratch dir, parsed args, a workflow, a manifest file, and
//! the one lock every test that mutates the process environment takes.

use crate::args::Args;
use clap::Parser;
use std::fs;
use std::path::PathBuf;

/// One crate-wide lock for tests that mutate a process-global env var (`GITHUB_API_URL`, …). The
/// environ is a single global, so per-module locks wouldn't serialize tests in different modules
/// racing through it (`scope`, `run`, `rank_grounded` all point `GITHUB_API_URL` at a local
/// listener); this is the one guard they share.
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn manifest_toml(effort_line: &str) -> String {
    format!(
        r#"
        [repo]
        path = "."
        [judge]
        measure_cmd = "m"
        direction = "higher"
        objective = "v"
        [agent]
        backend = "command"
        agent_cmd = "a"
        goal = "g"
        {effort_line}
    "#
    )
}

pub(crate) fn args_from(argv: &[&str]) -> Args {
    crate::cli::Cli::parse_from(argv).run
}

pub(crate) fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "crucible-run-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&dir).expect("mkdir tmp");
    dir
}

pub(crate) fn workflow_from(toml_src: &str) -> crate::plan::workflow::WorkflowCfg {
    toml::from_str(toml_src).unwrap()
}
