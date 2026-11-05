//! The vllm-agent-build example pack compiles to an agent build and a trace step, carries the
//! bench's trace renderer unchanged, and its trace step renders what the agent fetched.

use crucible::plan::ir::TaskKind;
use crucible::plan::starlark::compile_file_with;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

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

/// The trace step as the run runs it, on slices of a real run's ninja and ccache logs: a page per
/// build, the index, and a last stdout line that is the task's JSON output.
#[test]
fn the_vllm_agent_build_trace_step_writes_every_page_and_ends_with_its_json() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let trace = root.join("examples/vllm-agent-build/trace");
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(work.path().join("trace")).unwrap();
    for file in ["render.py", "buildtrace.py", "trace.html"] {
        std::fs::copy(trace.join(file), work.path().join("trace").join(file)).unwrap();
    }
    let inputs = work.path().join("inputs/build");
    std::fs::create_dir_all(inputs.join("traces")).unwrap();
    for file in std::fs::read_dir(trace.join("testdata/build/traces")).unwrap() {
        let file = file.unwrap();
        std::fs::copy(file.path(), inputs.join("traces").join(file.file_name())).unwrap();
    }
    std::fs::copy(
        trace.join("testdata/build/report.json"),
        inputs.join("report.json"),
    )
    .unwrap();

    let out = Command::new("python3")
        .args(["trace/render.py", "inputs/build"])
        .current_dir(work.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let last = stdout.lines().last().unwrap_or_default();
    let output: serde_json::Value = serde_json::from_str(last)
        .unwrap_or_else(|e| panic!("the last stdout line is not JSON ({e}): {stdout}"));
    let builds = output["builds"].as_array().unwrap();
    let names: Vec<&str> = builds.iter().map(|b| b["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["checkout", "edit1"]);
    assert_eq!(builds[1]["seconds"], 206);
    assert!(builds[1]["jobs"]["hit"].as_u64().unwrap() > 0, "{output}");
    for page in ["checkout.html", "edit1.html", "index.html"] {
        let html = std::fs::read_to_string(work.path().join("traces").join(page)).unwrap();
        assert!(html.starts_with("<!doctype html>"), "{page}");
    }
    let checkout = std::fs::read_to_string(work.path().join("traces/checkout.html")).unwrap();
    assert!(
        checkout.contains("\"name\": \"11/11 RUN TORCH_CUDA_ARCH_LIST")
            && checkout.contains("\"name\": \"commit\""),
        "the checkout's Dockerfile steps are its phases"
    );
}
