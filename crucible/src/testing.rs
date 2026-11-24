//! Fixtures the test modules share: a scratch dir, parsed args, a workflow, a manifest file, and
//! the one lock every test that mutates the process environment takes.

use crate::args::Args;
use clap::Parser;

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
    let mut args = crate::cli::Cli::parse_from(argv).run;
    args.secret_rules = secret_rules();
    args
}

/// One compiled rule set for the whole test binary, as for one engine process, so no turn under
/// test spends its timeout compiling.
pub(crate) fn secret_rules() -> crucible_harness::redact::SecretRules {
    static RULES: std::sync::OnceLock<crucible_harness::redact::SecretRules> =
        std::sync::OnceLock::new();
    let rules = RULES.get_or_init(Default::default).clone();
    assert!(rules.ready(), "the builtin secret rules compile");
    rules
}

pub(crate) use crate::test_fixtures::{checked_out, git_origin, read_one_request, tempdir};
