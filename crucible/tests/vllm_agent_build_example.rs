//! The vllm-agent-build example pack compiles to a seed command, an agent fan-out over the kernels
//! the seed emits, and a trace join; carries the bench's trace renderer unchanged; and its trace
//! step renders the seed and every instance it is given.

use crucible::plan::ir::{Isolation, TaskKind};
use crucible::plan::starlark::compile_file_with;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

#[test]
fn the_vllm_agent_build_example_compiles_to_a_seed_a_fanout_and_a_join() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let pack = root.join("examples/vllm-agent-build");
    let compiled = compile_file_with(&pack.join("workflow.star"), &pack, &BTreeMap::new())
        .unwrap_or_else(|error| panic!("{}", crucible::errors::report(&error)));
    let tasks = &compiled.workflow.tasks;
    let names: Vec<&str> = tasks.iter().map(|t| t.name.0.as_str()).collect();
    assert_eq!(names, ["seed", "edit", "trace"]);

    let TaskKind::Command { command } = &tasks[0].task else {
        panic!("seed is a command")
    };
    assert_eq!(command, "python3 seed.py");
    assert!(tasks[0].depends_on.is_empty());

    assert!(matches!(tasks[1].task, TaskKind::Agent { .. }));
    let over = tasks[1].over.as_ref().expect("edit fans out");
    assert_eq!(over.task.0, "seed");
    assert_eq!(over.field.0, "kernels");
    assert_eq!(tasks[1].isolation, Some(Isolation::Worktree));

    let TaskKind::Command { command } = &tasks[2].task else {
        panic!("trace is a command")
    };
    assert_eq!(command, "python3 trace/render.py inputs");
    let deps: Vec<&str> = tasks[2].depends_on.iter().map(|d| d.0.as_str()).collect();
    assert_eq!(deps, ["seed", "edit"]);
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

/// The trace step as the run runs it, on slices of a real run's logs laid out as the join's
/// inputs: the seed under inputs/seed, one instance under inputs/edit[layernorm], and a failed
/// instance that captured nothing and so has no directory.
#[test]
fn the_vllm_agent_build_trace_step_renders_the_seed_and_every_instance() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let trace = root.join("examples/vllm-agent-build/trace");
    let fixtures = trace.join("testdata/build/traces");
    let work = tempfile::tempdir().unwrap();
    let dir = work.path();
    std::fs::create_dir_all(dir.join("trace")).unwrap();
    for file in ["render.py", "buildtrace.py", "trace.html"] {
        std::fs::copy(trace.join(file), dir.join("trace").join(file)).unwrap();
    }
    let seed = dir.join("inputs/seed/traces");
    let instance = dir.join("inputs/edit[layernorm]");
    std::fs::create_dir_all(&seed).unwrap();
    std::fs::create_dir_all(instance.join("traces")).unwrap();
    for log in ["ninja_log", "ccache-stats.log", "steps.log"] {
        std::fs::copy(
            fixtures.join(format!("checkout.{log}")),
            seed.join(format!("seed.{log}")),
        )
        .unwrap();
        std::fs::copy(
            fixtures.join(format!("checkout.{log}")),
            instance.join(format!("traces/checkout.{log}")),
        )
        .unwrap();
    }
    for log in ["ninja_log", "ccache-stats.log"] {
        std::fs::copy(
            fixtures.join(format!("edit1.{log}")),
            instance.join(format!("traces/edit.{log}")),
        )
        .unwrap();
    }
    std::fs::write(
        instance.join("report.json"),
        serde_json::json!({
            "kernel": "layernorm",
            "builds": [
                {"name": "checkout", "build_id": "buildit-a", "status": "succeeded", "seconds": 352},
                {"name": "edit", "build_id": "buildit-a-1", "status": "succeeded", "seconds": 206},
            ],
        })
        .to_string(),
    )
    .unwrap();
    let inputs_file = dir.join("inputs.json");
    std::fs::write(
        &inputs_file,
        serde_json::json!({
            "params": {},
            "seed": {"kernels": ["layernorm", "activation"], "seconds": 1252},
            "edit": {"instances": 2, "passed": 1, "failed": 1, "not_taken": 0, "outputs": {"layernorm": {}}},
        })
        .to_string(),
    )
    .unwrap();

    let out = Command::new("python3")
        .args(["trace/render.py", "inputs"])
        .env("CRUCIBLE_INPUTS_FILE", &inputs_file)
        .current_dir(dir)
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
    let rows: Vec<(&str, &str)> = builds
        .iter()
        .map(|b| (b["lane"].as_str().unwrap(), b["build"].as_str().unwrap()))
        .collect();
    assert_eq!(
        rows,
        [
            ("seed", "seed"),
            ("layernorm", "checkout"),
            ("layernorm", "edit")
        ]
    );
    assert_eq!(builds[0]["seconds"], 1252);
    assert_eq!(builds[2]["seconds"], 206);
    assert!(builds[2]["jobs"]["hit"].as_u64().unwrap() > 0, "{output}");

    let index = std::fs::read_to_string(dir.join("traces/index.html")).unwrap();
    assert!(index.starts_with("<!doctype html>"));
    assert_eq!(index.matches("<details data-page=").count(), 3);
    let (_, embedded) = index
        .split_once("<script type='application/json' id='pages'>")
        .unwrap();
    let (embedded, _) = embedded.split_once("</script>").unwrap();
    let pages: Vec<String> = serde_json::from_str(embedded).unwrap();
    assert_eq!(pages.len(), 3);
    assert!(pages.iter().all(|p| p.starts_with("<!doctype html>")));
    assert!(
        pages[0].contains("\"name\": \"11/11 RUN TORCH_CUDA_ARCH_LIST")
            && pages[0].contains("\"name\": \"commit\""),
        "the seed's Dockerfile steps are its phases"
    );
}
