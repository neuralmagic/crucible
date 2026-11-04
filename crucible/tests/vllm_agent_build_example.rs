//! The vllm-agent-build example pack compiles to an agent build and a trace step, and carries the
//! bench's trace renderer unchanged.

use crucible::plan::ir::TaskKind;
use crucible::plan::starlark::compile_file_with;
use std::collections::BTreeMap;
use std::path::Path;

#[test]
fn the_vllm_agent_build_example_compiles_to_a_build_and_its_traces() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let pack = root.join("examples/vllm-agent-build");
    let compiled = compile_file_with(&pack.join("workflow.star"), &pack, &BTreeMap::new())
        .unwrap_or_else(|error| panic!("{}", crucible::errors::report(&error)));
    let tasks = &compiled.workflow.tasks;
    let names: Vec<&str> = tasks.iter().map(|t| t.name.0.as_str()).collect();
    assert_eq!(names, ["build", "trace"]);
    assert!(matches!(tasks[0].task, TaskKind::Agent { .. }));
    let TaskKind::Command { command } = &tasks[1].task else {
        panic!("trace is a command")
    };
    assert_eq!(command, "python3 trace/render.py inputs/build");
    let deps: Vec<&str> = tasks[1].depends_on.iter().map(|d| d.0.as_str()).collect();
    assert_eq!(deps, ["build"]);
    assert_eq!(
        compiled.workflow.result.as_ref().map(|r| r.0.as_str()),
        Some("trace")
    );
}

#[test]
fn the_vllm_agent_build_example_renders_traces_with_the_bench_s_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for file in ["buildtrace.py", "trace.html"] {
        let bench =
            std::fs::read_to_string(root.join("examples/vllm-build-bench/bench").join(file))
                .unwrap();
        let agent =
            std::fs::read_to_string(root.join("examples/vllm-agent-build/trace").join(file))
                .unwrap();
        assert!(
            bench == agent,
            "examples/vllm-agent-build/trace/{file} must be a copy of examples/vllm-build-bench/bench/{file}"
        );
    }
}
