//! The vllm-hill-climb example pack compiles to a seed command, the climbing agent and a report
//! command; its helper stages, keeps and resets candidates in a real git checkout; and its report
//! computes the speedup, iteration times and build reuse from what the climb recorded.

use crucible::plan::ir::TaskKind;
use crucible::plan::starlark::compile_file_with;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn pack() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("examples/vllm-hill-climb")
}

#[test]
fn the_vllm_hill_climb_example_compiles_to_a_seed_a_climb_and_a_report() {
    let pack = pack();
    let compiled = compile_file_with(&pack.join("workflow.star"), &pack, &BTreeMap::new())
        .unwrap_or_else(|error| panic!("{}", crucible::errors::report(&error)));
    let tasks = &compiled.workflow.tasks;
    let names: Vec<&str> = tasks.iter().map(|t| t.name.0.as_str()).collect();
    assert_eq!(names, ["seed", "climb", "report"]);

    let TaskKind::Command { command } = &tasks[0].task else {
        panic!("seed is a command")
    };
    assert_eq!(command, "python3 seed.py");
    assert!(tasks[0].depends_on.is_empty());

    let TaskKind::Agent { prompt, .. } = &tasks[1].task else {
        panic!("climb is an agent")
    };
    let prompt = format!("{prompt:?}");
    for line in [
        "vllm_ref: 5df63a7203cdb9328d824a27b682e77c7b9928be",
        "CUDA_ARCH_LIST: 8.0",
        "QUEUE: crucible-e2e",
        "N: 3",
    ] {
        assert!(prompt.contains(line), "the climb prompt lacks {line:?}");
    }
    let deps: Vec<&str> = tasks[1].depends_on.iter().map(|d| d.0.as_str()).collect();
    assert_eq!(deps, ["seed"]);

    let TaskKind::Command { command } = &tasks[2].task else {
        panic!("report is a command")
    };
    assert_eq!(command, "python3 report.py inputs/climb");
    let deps: Vec<&str> = tasks[2].depends_on.iter().map(|d| d.0.as_str()).collect();
    assert_eq!(deps, ["seed", "climb"]);
    assert_eq!(
        compiled.workflow.result.as_ref().map(|r| r.0.as_str()),
        Some("report")
    );
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn ok(dir: &Path, program: &str, args: &[&str]) -> String {
    let out = run(dir, program, args);
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A workspace as `hc.py checkout` leaves it, from a local repository instead of GitHub.
fn workspace() -> tempfile::TempDir {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path();
    std::fs::copy(pack().join("hc.py"), dir.join("hc.py")).unwrap();
    let src = dir.join("build/vllm");
    std::fs::create_dir_all(src.join("vllm/model_executor/layers/fused_moe")).unwrap();
    std::fs::create_dir_all(src.join("csrc/moe")).unwrap();
    std::fs::write(
        src.join("vllm/model_executor/layers/fused_moe/fused_moe.py"),
        "BLOCK = 64\n",
    )
    .unwrap();
    std::fs::write(src.join("csrc/moe/topk_softmax_kernels.cu"), "// v0\n").unwrap();
    std::fs::write(src.join("setup.py"), "\n").unwrap();
    let git = |args: &[&str]| {
        let mut all = vec!["-C", "build/vllm", "-c", "user.name=t", "-c", "user.email=t@t"];
        all.extend_from_slice(args);
        ok(dir, "git", &all);
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "seed"]);
    git(&["tag", "seed"]);
    git(&["checkout", "-q", "-b", "best"]);
    for name in ["timeline.jsonl", "results.jsonl", "best.patch"] {
        std::fs::write(dir.join(name), "").unwrap();
    }
    work
}

fn stage(dir: &Path, label: &str) -> serde_json::Value {
    let out = ok(dir, "python3", &["hc.py", "stage", label, "reg/vllm@sha256:seed"]);
    serde_json::from_str(out.trim()).unwrap()
}

const MOE: &str = "build/vllm/vllm/model_executor/layers/fused_moe/fused_moe.py";
const CONFIG: &str = "vllm/model_executor/layers/fused_moe/configs/E=128,N=768,device_name=NVIDIA_A100-SXM4-80GB.json";

#[test]
fn a_python_edit_stages_as_a_layer_of_only_the_edited_files_over_the_seed() {
    let work = workspace();
    let dir = work.path();
    std::fs::write(dir.join(MOE), "BLOCK = 128\n").unwrap();
    std::fs::create_dir_all(dir.join("build/vllm").join(CONFIG).parent().unwrap()).unwrap();
    std::fs::write(dir.join("build/vllm").join(CONFIG), "{}\n").unwrap();

    let staged = stage(dir, "blocks");
    assert_eq!(staged["recipe"], "layer");
    assert_eq!(
        staged["build"],
        serde_json::json!({
            "context": "cand/blocks",
            "base": "reg/vllm@sha256:seed",
            "script": "true",
            "path": "/src/vllm",
        })
    );
    let layer = dir.join("cand/blocks");
    assert_eq!(
        std::fs::read_to_string(layer.join("vllm/model_executor/layers/fused_moe/fused_moe.py"))
            .unwrap(),
        "BLOCK = 128\n"
    );
    assert!(layer.join(CONFIG).is_file());
    assert!(
        !layer.join("setup.py").exists() && !layer.join("csrc").exists(),
        "a layer holds only the edited files"
    );

    ok(dir, "python3", &["hc.py", "reset"]);
    assert_eq!(std::fs::read_to_string(dir.join(MOE)).unwrap(), "BLOCK = 64\n");
    assert!(!dir.join("build/vllm").join(CONFIG).exists());
}

#[test]
fn a_cuda_edit_stages_as_a_dockerfile_build_of_the_whole_tree() {
    let work = workspace();
    let dir = work.path();
    std::fs::write(dir.join(MOE), "BLOCK = 32\n").unwrap();
    std::fs::write(dir.join("build/vllm/csrc/moe/topk_softmax_kernels.cu"), "// v1\n").unwrap();

    let staged = stage(dir, "topk");
    assert_eq!(staged["recipe"], "dockerfile");
    assert_eq!(staged["build"]["context"], "build");
    assert_eq!(staged["build"]["ccache"], true);
    assert!(!dir.join("cand/topk").exists());
    assert!(dir.join("cand/topk.patch").is_file());
}

#[test]
fn keeping_a_candidate_makes_it_the_tree_every_later_candidate_starts_from() {
    let work = workspace();
    let dir = work.path();
    std::fs::write(dir.join(MOE), "BLOCK = 128\n").unwrap();
    stage(dir, "blocks");
    ok(dir, "python3", &["hc.py", "reset"]);
    std::fs::write(dir.join(MOE), "BLOCK = 256\n").unwrap();
    stage(dir, "wider");
    ok(dir, "python3", &["hc.py", "reset"]);

    ok(dir, "python3", &["hc.py", "keep", "blocks"]);
    assert_eq!(std::fs::read_to_string(dir.join(MOE)).unwrap(), "BLOCK = 128\n");
    let best = std::fs::read_to_string(dir.join("best.patch")).unwrap();
    assert!(best.contains("+BLOCK = 128"), "{best}");

    std::fs::write(dir.join(MOE), "BLOCK = 512\n").unwrap();
    ok(dir, "python3", &["hc.py", "reset"]);
    assert_eq!(
        std::fs::read_to_string(dir.join(MOE)).unwrap(),
        "BLOCK = 128\n",
        "reset returns to the kept tree, not the seed"
    );

    std::fs::write(dir.join(MOE), "BLOCK = 128\nSPLIT = 2\n").unwrap();
    let staged = stage(dir, "split");
    assert_eq!(staged["recipe"], "layer");
    let patch = std::fs::read_to_string(dir.join("cand/split.patch")).unwrap();
    assert!(
        patch.contains("+BLOCK = 128") && patch.contains("+SPLIT = 2"),
        "a candidate is staged against the seed, so its layer carries the kept edits too: {patch}"
    );
}

#[test]
fn stage_refuses_an_unchanged_tree_a_deletion_and_a_bad_label() {
    let work = workspace();
    let dir = work.path();
    let out = run(dir, "python3", &["hc.py", "stage", "same", "img"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no edits"));

    std::fs::remove_file(dir.join("build/vllm/setup.py")).unwrap();
    let out = run(dir, "python3", &["hc.py", "stage", "gone", "img"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot delete"));
    ok(dir, "python3", &["hc.py", "reset"]);

    std::fs::write(dir.join(MOE), "BLOCK = 1\n").unwrap();
    let out = run(dir, "python3", &["hc.py", "stage", "Bad_Label", "img"]);
    assert!(!out.status.success());
}

#[test]
fn marking_a_label_measured_records_its_job_log() {
    let work = workspace();
    let dir = work.path();
    let out = run(dir, "python3", &["hc.py", "mark", "baseline", "measured"]);
    assert!(!out.status.success(), "measured needs the log first");

    std::fs::create_dir_all(dir.join("evidence")).unwrap();
    std::fs::write(
        dir.join("evidence/baseline.log"),
        "hill-climb-result {\"ok\": true, \"geomean_us\": 100}\n",
    )
    .unwrap();
    ok(dir, "python3", &["hc.py", "mark", "baseline", "staged"]);
    ok(dir, "python3", &["hc.py", "mark", "baseline", "measured"]);
    let results = std::fs::read_to_string(dir.join("results.jsonl")).unwrap();
    let row: serde_json::Value = serde_json::from_str(results.trim()).unwrap();
    assert_eq!(row["label"], "baseline");
    assert!(row["log"].as_str().unwrap().contains("\"geomean_us\": 100"));
    let events: Vec<String> = std::fs::read_to_string(dir.join("timeline.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["event"].to_string())
        .collect();
    assert_eq!(events, ["\"staged\"", "\"measured\""]);
}

fn result_line(ok: bool, geomean: f64) -> String {
    format!(
        "Defaulted container \"bench\"\nhill-climb-result {}\n",
        serde_json::json!({"ok": ok, "geomean_us": geomean, "us": {"1": geomean}})
    )
}

fn climb_inputs(dir: &Path, best: &str) {
    let climb = dir.join("inputs/climb");
    std::fs::create_dir_all(&climb).unwrap();
    std::fs::copy(pack().join("report.py"), dir.join("report.py")).unwrap();
    std::fs::write(
        climb.join("report.json"),
        serde_json::json!({
            "best": best,
            "candidates": [
                {"label": "baseline", "round": 0, "recipe": "seed", "change": "the seed"},
                {"label": "config", "round": 1, "recipe": "layer", "change": "an A100 config"},
                {"label": "wrong", "round": 1, "recipe": "layer", "change": "skips a block"},
                {"label": "topk", "round": 1, "recipe": "dockerfile", "change": "warp topk"},
            ],
            "summary": "The config helped.",
        })
        .to_string(),
    )
    .unwrap();
    let mut timeline = String::new();
    for (label, staged, pushed, submitted, measured) in [
        ("baseline", 0.0, 0.0, 10.0, 130.0),
        ("config", 200.0, 230.0, 240.0, 330.0),
        ("wrong", 210.0, 250.0, 255.0, 345.0),
        ("topk", 220.0, 620.0, 630.0, 750.0),
    ] {
        for (event, t) in [
            ("staged", staged),
            ("pushed", pushed),
            ("submitted", submitted),
            ("measured", measured),
        ] {
            timeline.push_str(
                &serde_json::json!({"label": label, "event": event, "t": t}).to_string(),
            );
            timeline.push('\n');
        }
    }
    std::fs::write(climb.join("timeline.jsonl"), timeline).unwrap();
    let mut results = String::new();
    for (label, log) in [
        ("baseline", result_line(true, 200.0)),
        ("config", result_line(true, 160.0)),
        ("wrong", result_line(false, 90.0)),
        ("topk", result_line(true, 195.0)),
        (
            "serve-seed",
            "hill-climb-serve {\"ok\": true, \"output_throughput\": 1000.0}\n".to_string(),
        ),
        (
            "serve-best",
            "hill-climb-serve {\"ok\": true, \"output_throughput\": 1100.0}\n".to_string(),
        ),
    ] {
        results.push_str(&serde_json::json!({"label": label, "log": log}).to_string());
        results.push('\n');
    }
    std::fs::write(climb.join("results.jsonl"), results).unwrap();
    std::fs::write(
        dir.join("inputs.json"),
        serde_json::json!({
            "params": {},
            "seed": {"seed_build_seconds": 600, "seed_push_seconds": 90,
                     "seed_ccache": {"remote_hit": 570, "hit": 0, "miss": 7, "uncached": 0}},
        })
        .to_string(),
    )
    .unwrap();
}

fn report(dir: &Path) -> Output {
    Command::new("python3")
        .args(["report.py", "inputs/climb"])
        .env("CRUCIBLE_INPUTS_FILE", dir.join("inputs.json"))
        .current_dir(dir)
        .output()
        .unwrap()
}

#[test]
fn the_report_computes_the_speedup_iteration_times_and_build_reuse() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path();
    climb_inputs(dir, "config");
    let out = report(dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let emitted: serde_json::Value =
        serde_json::from_str(stdout.lines().last().unwrap_or_default()).unwrap();
    assert_eq!(emitted["best"], "config");
    assert_eq!(emitted["speedup"], 1.25);
    assert_eq!(emitted["builds"], 3);
    assert_eq!(emitted["layer_builds"], 2);
    assert_eq!(
        emitted["median_iteration_s"],
        serde_json::json!({"layer": 132.5, "dockerfile": 530.0})
    );
    assert_eq!(emitted["median_queue_run_s"], 105.0);

    let summary = std::fs::read_to_string(dir.join("summary.md")).unwrap();
    assert!(summary.starts_with("# vLLM fused MoE hill climb: 1.25x"), "{summary}");
    assert!(summary.contains("| 1 | wrong | layer | no | 90.0 |  |"), "{summary}");
    assert!(summary.contains("| output_throughput | 1000.0 | 1100.0 |"), "{summary}");
    assert!(summary.contains("'remote_hit': 570"), "{summary}");
}

#[test]
fn the_report_fails_when_the_kept_candidate_measured_wrong() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path();
    climb_inputs(dir, "wrong");
    let out = report(dir);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("'wrong' has no correct measurement"));
}

#[test]
fn the_seed_counts_one_ccache_outcome_per_compiler_call() {
    let log = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("examples/vllm-agent-build/trace/testdata/build/traces/edit1.ccache-stats.log");
    let text = std::fs::read_to_string(&log).unwrap();
    let calls = text.lines().filter(|l| l.starts_with('#')).count();
    let out = Command::new("python3")
        .args([
            "-c",
            "import json, sys; from pathlib import Path; import seed; \
             print(json.dumps(seed.ccache_counts(Path(sys.argv[1]))))",
        ])
        .arg(&log)
        .current_dir(pack())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let counts: BTreeMap<String, usize> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(counts.values().sum::<usize>(), calls);
    assert!(counts["hit"] + counts["remote_hit"] > 0, "{counts:?}");
}
