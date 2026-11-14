//! `crucible plan run` over a playbook whose tasks take known time: every `task_started` and
//! `task_result` carries when it happened, each result says how long its task ran, and the run's
//! summary prints that time beside each task.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const WORKFLOW: &str = r#"
slow = command(name = "slow", run = "sleep 1.2; printf '{\"ok\": true}'", emits = ["ok"])
quick = command(name = "quick", run = "printf '{\"ok\": true}'", emits = ["ok"], depends_on = [slow])
workflow(type = "playbook", tasks = [slow, quick], result = quick)
"#;

fn pack() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crucible-timing-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("workflow.star"), WORKFLOW).unwrap();
    std::fs::write(
        dir.join("crucible.toml"),
        r#"
[agent]
backend = "command"
agent_cmd = "true"
goal = "time two tasks"
[workflow]
type = "playbook"
file = "workflow.star"
"#,
    )
    .unwrap();
    dir
}

fn run(dir: &Path) -> (Vec<Value>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args([
            "plan",
            "run",
            "--manifest",
            "crucible.toml",
            "--max-cost",
            "1",
            "--max-time",
            "60s",
        ])
        .current_dir(dir)
        .env("FORGE_STORAGE_ROOT", dir.join("storage"))
        .env_remove("CRUCIBLE_HISTORY")
        .env_remove("CRUCIBLE_INFERENCE")
        .output()
        .expect("run crucible");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(dir.join("state/session.jsonl"))
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    (log, String::from_utf8(out.stdout).unwrap())
}

fn summary_secs(stdout: &str, task: &str) -> f64 {
    let line = stdout
        .lines()
        .find(|l| l.split_whitespace().next() == Some(task))
        .unwrap_or_else(|| panic!("no summary line for {task}: {stdout}"));
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("elapsed="))
        .and_then(|v| v.strip_suffix('s'))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no elapsed seconds on {line:?}"))
}

fn event<'a>(log: &'a [Value], kind: &str, task: &str) -> &'a Value {
    log.iter()
        .find(|e| e["kind"] == kind && e["task"] == task)
        .unwrap_or_else(|| panic!("no {kind} for {task}: {log:#?}"))
}

fn at(event: &Value) -> jiff::Timestamp {
    event["at"]
        .as_str()
        .unwrap_or_else(|| panic!("no at: {event}"))
        .parse()
        .unwrap_or_else(|e| panic!("at is not RFC 3339 ({e}): {event}"))
}

#[test]
fn task_events_carry_when_they_happened_and_results_how_long_their_task_ran() {
    let dir = pack();
    let (log, stdout) = run(&dir);

    let slow_started = at(event(&log, "task_started", "slow"));
    let slow = event(&log, "task_result", "slow");
    let quick_started = at(event(&log, "task_started", "quick"));
    let quick = event(&log, "task_result", "quick");

    let secs = slow["secs"].as_f64().unwrap();
    assert!((1.2..10.0).contains(&secs), "slow ran {secs}s: {slow}");
    let span = at(slow).duration_since(slow_started).as_secs_f64();
    assert!(
        (secs - span).abs() < 0.5,
        "secs {secs} disagrees with the stamps' {span}"
    );
    assert!(
        quick_started >= at(slow),
        "quick started before the task it depends on settled"
    );
    assert!(quick["secs"].as_f64().unwrap() < 1.0, "{quick}");
    assert!(at(quick) >= quick_started);

    let printed = summary_secs(&stdout, "slow");
    assert!(
        (printed - secs).abs() < 0.2,
        "summary says {printed}s, result {secs}s"
    );
    assert!(summary_secs(&stdout, "quick") < 1.0, "{stdout}");

    let _ = std::fs::remove_dir_all(&dir);
}
