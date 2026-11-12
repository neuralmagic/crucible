//! The demo task run as a real process, with the environment and files the engine gives a task.

use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, inputs: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_crucible-task-demo"));
    command.current_dir(dir).env_remove("CRUCIBLE_INPUTS_FILE");
    if let Some(inputs) = inputs {
        let file = dir.join("task-inputs.json");
        std::fs::write(&file, inputs).unwrap();
        command
            .env("CRUCIBLE_INPUTS_FILE", &file)
            .env("CRUCIBLE_TASK", "report");
    }
    command.output().unwrap()
}

fn last_line(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap();
    serde_json::from_str(line).unwrap()
}

#[test]
fn a_task_reads_its_inputs_writes_its_file_and_ends_on_its_result_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("inputs/stage")).unwrap();
    std::fs::write(dir.path().join("inputs/stage/manifest.txt"), "run 7\n").unwrap();

    let output = run(
        dir.path(),
        Some(r#"{"params":{"profile":"llm-d"},"stage":{"members":["A-1","A-2"]}}"#),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        last_line(&output),
        serde_json::json!({"task": "report", "tickets": 2})
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("working on llm-d\n"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("REPORT.md")).unwrap(),
        "llm-d: A-1, A-2 (run 7)\n"
    );
}

#[test]
fn a_failure_exits_nonzero_with_its_reason_as_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(
        dir.path(),
        Some(r#"{"params":{"profile":"llm-d"},"stage":{"members":[]}}"#),
    );
    assert!(!output.status.success());
    let result = last_line(&output);
    assert_eq!(result["pass"], false);
    let note = result["note"].as_str().unwrap();
    assert!(note.contains("declared no file manifest.txt"), "{note}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("declared no file manifest.txt"));

    let outside = run(dir.path(), None);
    assert!(!outside.status.success());
    assert!(
        last_line(&outside)["note"]
            .as_str()
            .unwrap()
            .contains("CRUCIBLE_INPUTS_FILE is unset")
    );
}
