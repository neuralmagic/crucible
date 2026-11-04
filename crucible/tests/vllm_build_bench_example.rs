//! The vllm-build-bench example pack compiles to its step chain.

use crucible::plan::ir::TaskKind;
use crucible::plan::starlark::compile_file_with;
use std::collections::BTreeMap;
use std::path::Path;

const STEPS: [&str; 7] = [
    "clean",
    "leaf",
    "leaf_py",
    "revert",
    "leaf_again",
    "header",
    "cmake",
];

#[test]
fn the_vllm_build_bench_example_compiles_to_a_chain_of_steps_and_a_report() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let pack = root.join("examples/vllm-build-bench");
    let compiled = compile_file_with(&pack.join("workflow.star"), &pack, &BTreeMap::new())
        .unwrap_or_else(|error| panic!("{}", crucible::errors::report(&error)));
    let tasks = &compiled.workflow.tasks;
    let names: Vec<&str> = tasks.iter().map(|t| t.name.0.as_str()).collect();
    assert_eq!(names[..STEPS.len()], STEPS);
    assert_eq!(names[STEPS.len()..], ["report"]);
    for (i, step) in STEPS.iter().enumerate() {
        let task = &tasks[i];
        let TaskKind::Command { command } = &task.task else {
            panic!("{step} is a command")
        };
        assert_eq!(command, &format!("python3 bench/replay.py step {step}"));
        let deps: Vec<&str> = task.depends_on.iter().map(|d| d.0.as_str()).collect();
        assert_eq!(deps, STEPS[i.saturating_sub(1)..i]);
    }
    let report = &tasks[STEPS.len()];
    assert!(matches!(report.task, TaskKind::Evaluate { .. }));
    let deps: Vec<&str> = report.depends_on.iter().map(|d| d.0.as_str()).collect();
    assert_eq!(deps, STEPS);
    assert_eq!(
        compiled.workflow.result.as_ref().map(|r| r.0.as_str()),
        Some("report")
    );
}
