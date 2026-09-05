//! Fixtures the test modules share: a scratch dir, parsed args, a workflow, a manifest file, and
//! the one lock every test that mutates the process environment takes.

use crate::args::Args;
use clap::Parser;
use std::fs;
use std::path::PathBuf;

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
