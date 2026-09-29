use std::path::{Path, PathBuf};
use std::process::Command;

const MANIFEST: &str = r#"
[repo]
path = "."
[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
[agent]
backend = "command"
agent_cmd = "true"
goal = "sweep the queue"
[workflow]
type = "playbook"
file = "workflow.star"
"#;

fn workflow(check: &str) -> String {
    format!(
        r#"
check = command(name = "check", run = "{check}")
work = command(name = "work", run = "touch work.ran && printf '{{}}\n'", depends_on = [check])
report = command(
    name = "report",
    run = "cp \"$FORGE_STORAGE_ROOT/report.json\" card.json && python3 -c 'import json, os; print(json.dumps({{\"exit\": json.loads(os.environ[\"CRUCIBLE_INPUTS\"])[\"outcome\"][\"exit\"]}}))' | tee exit.json",
    stage = "epilogue",
)
workflow(type = "playbook", tasks = [check, work, report])
"#
    )
}

fn pack(tag: &str, check: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "crucible-early-completion-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("crucible.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("workflow.star"), workflow(check)).unwrap();
    dir
}

fn run(dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args([
            "plan",
            "run",
            "--manifest",
            "crucible.toml",
            "--max-cost",
            "1",
            "--max-time",
            "5m",
        ])
        .current_dir(dir)
        .env("FORGE_STORAGE_ROOT", dir.join("storage"))
        .output()
        .expect("run crucible")
}

fn shutdown(dir: &Path) -> serde_json::Value {
    let log = std::fs::read_to_string(dir.join("state/session.jsonl")).unwrap();
    let lines: Vec<serde_json::Value> = log
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["kind"] == "shutdown")
        .collect();
    assert_eq!(lines.len(), 1, "{log}");
    lines[0].clone()
}

#[test]
fn a_task_that_finds_nothing_to_do_ends_the_run_valid_and_complete() {
    let dir = pack(
        "complete",
        r#"printf '{\"complete\": true, \"reason\": \"no new tickets\"}\n'"#,
    );
    let out = run(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        !dir.join("workspace/work.ran").exists(),
        "a task after the completion ran"
    );
    let shutdown = shutdown(&dir);
    assert_eq!(shutdown["outcome"], "complete");
    assert_eq!(
        shutdown["reason"],
        "completed early by check: no new tickets"
    );
    let exit: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("workspace/exit.json")).unwrap())
            .unwrap();
    assert_eq!(
        exit["exit"], "complete",
        "the epilogue saw how the run ended"
    );
    let card: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("workspace/card.json")).unwrap())
            .unwrap();
    assert_eq!(card["verdict"], "pass");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_task_that_does_not_complete_leaves_the_graph_running() {
    let dir = pack("finished", r#"printf '{\"complete\": false}\n'"#);
    let out = run(&dir);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.join("workspace/work.ran").exists());
    assert_eq!(shutdown(&dir)["outcome"], "finished");
    let _ = std::fs::remove_dir_all(&dir);
}
