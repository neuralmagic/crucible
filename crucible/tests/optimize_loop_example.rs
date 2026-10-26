//! The optimize-loop example pack compiles in both variants through the public compiler.

use crucible::plan::ir::TaskKind;
use crucible::plan::starlark::compile_file_with;
use std::collections::BTreeMap;
use std::path::Path;

/// Both variants of the optimize-loop pack compile, each emitting its own harness for the
/// verifier to compare with the frozen copy, and the evidence path keeps the optimizer's
/// output away from the collector: it hears only from the gate.
#[test]
fn the_optimize_loop_example_compiles_both_variants() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let pack = root.join("examples/optimize-loop");
    for (gpu, harness, prompt_head) in [
        (
            "false",
            "app/main.go",
            "You are optimizing a slow Go program",
        ),
        (
            "true",
            "gpu/bench.py",
            "You are optimizing a slow PyTorch attention kernel",
        ),
    ] {
        let supplied = BTreeMap::from([
            ("gpu".to_string(), gpu.to_string()),
            ("rounds".to_string(), "2".to_string()),
        ]);
        let compiled = compile_file_with(&pack.join("workflow.star"), &pack, &supplied)
            .unwrap_or_else(|error| panic!("gpu={gpu}: {}", crucible::errors::report(&error)));
        let task = |name: &str| {
            compiled
                .workflow
                .tasks
                .iter()
                .find(|t| t.name.0 == name)
                .unwrap_or_else(|| panic!("no task {name}"))
        };
        let optimize = task("optimize");
        let TaskKind::Agent { prompt, .. } = &optimize.task else {
            panic!("optimize is an agent")
        };
        assert!(prompt.starts_with(prompt_head), "gpu={gpu}: {prompt}");
        assert!(prompt.contains("Round limit N: 2\n"), "{prompt}");
        let mut emitted: Vec<&str> = optimize
            .emits_files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        emitted.sort_unstable();
        assert!(emitted.contains(&harness), "gpu={gpu}: {emitted:?}");
        assert!(emitted.contains(&"report.json"), "{emitted:?}");
        let deps = |name: &str| {
            task(name)
                .depends_on
                .iter()
                .map(|d| d.0.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(deps("gate"), ["optimize"]);
        assert_eq!(deps("collect"), ["gate"]);
        assert_eq!(deps("verify"), ["optimize", "collect"]);
        assert!(matches!(task("verify").task, TaskKind::Evaluate { .. }));
        assert_eq!(
            compiled.workflow.result.as_ref().map(|r| r.0.as_str()),
            Some("verify")
        );
    }
}
