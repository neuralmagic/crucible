use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MANIFEST: &str = r#"
[repo]
path = "."
[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
[agent]
backend = "command"
agent_cmd = "true"
goal = "resume after a crash"
[workflow]
type = "playbook"
file = "workflow.star"
"#;

/// once -> produce -> edit -> crash -> consume, and a report in the epilogue. Every task appends
/// to a ledger outside the workspace, so a task that ran twice shows up twice. `crash` kills the
/// engine the first time it runs.
const WORKFLOW: &str = r#"
once = command(name = "once", run = "echo once >> ../ran.log && printf '{}\n'")
produce = command(
    name = "produce",
    run = "echo produce >> ../ran.log && echo carried > out.txt && printf '{}\n'",
    depends_on = [once],
    emits_files = ["out.txt"],
)
edit = command(
    name = "edit",
    run = "echo edit >> ../ran.log && echo scratch > edited.txt && printf '{}\n'",
    depends_on = [produce],
)
crash = command(
    name = "crash",
    run = "echo crash >> ../ran.log; if [ -e ../crashed ]; then if [ -e edited.txt ]; then echo present > ../seen.log; else echo absent > ../seen.log; fi; printf '{}\n'; else touch ../crashed; kill -9 $PPID; sleep 5; fi",
    depends_on = [edit],
)
consume = command(
    name = "consume",
    run = "echo consume >> ../ran.log && cp inputs/produce/out.txt ../consumed.txt && printf '{}\n'",
    depends_on = [crash],
)
report = command(name = "report", run = "echo report >> ../ran.log && printf '{}\n'", stage = "epilogue")
workflow(type = "playbook", tasks = [once, produce, edit, crash, consume, report])
"#;

fn pack(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crucible-resume-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("crucible.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("workflow.star"), WORKFLOW).unwrap();
    dir
}

fn run(dir: &Path, resume: bool) -> Output {
    let mut args = vec![
        "plan",
        "run",
        "--manifest",
        "crucible.toml",
        "--max-cost",
        "1",
        "--max-time",
        "5m",
    ];
    if resume {
        args.push("--resume");
    }
    Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(args)
        .current_dir(dir)
        .env("FORGE_STORAGE_ROOT", dir.join("storage"))
        .env("CRUCIBLE_RUN_NAME", "crucible-run-resume")
        .output()
        .expect("run crucible")
}

fn events(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("state/session.jsonl"))
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn of_kind<'a>(events: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
    events.iter().filter(|e| e["kind"] == kind).collect()
}

fn ran(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("ran.log"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\n{}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_killed_playbook_resumes_without_repeating_a_settled_task() {
    let dir = pack("killed");

    let killed = run(&dir, false);
    assert!(!killed.status.success(), "{}", describe(&killed));
    assert_eq!(ran(&dir), ["once", "produce", "edit", "crash"]);
    let before = events(&dir);
    assert!(of_kind(&before, "shutdown").is_empty(), "the engine died");
    let settled: Vec<&str> = of_kind(&before, "task_result")
        .iter()
        .filter_map(|e| e["task"].as_str())
        .collect();
    assert_eq!(settled, ["once", "produce", "edit"]);

    let resumed = run(&dir, true);
    assert!(resumed.status.success(), "{}", describe(&resumed));
    assert_eq!(
        ran(&dir),
        [
            "once", "produce", "edit", "crash", "crash", "consume", "report"
        ],
        "only the task that was in flight runs again"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("seen.log"))
            .unwrap()
            .trim(),
        "absent",
        "the workspace was rebuilt from the pristine checkout, not from where edit left it"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("consumed.txt"))
            .unwrap()
            .trim(),
        "carried",
        "a file captured before the crash is staged after it"
    );
    let after = events(&dir);
    assert_eq!(of_kind(&after, "plan_admitted").len(), 1);
    let recovery = of_kind(&after, "recovery");
    assert_eq!(recovery.len(), 1);
    assert_eq!(recovery[0]["class"], "died_in_plan_task");
    let shutdown = of_kind(&after, "shutdown");
    assert_eq!(shutdown.len(), 1);
    assert_eq!(shutdown[0]["outcome"], "finished");
    assert_eq!(after.last().unwrap()["kind"], "shutdown");
    assert_eq!(of_kind(&after, "history_entry").len(), 1);
    let rows: Vec<&str> = of_kind(&after, "task_result")
        .iter()
        .filter_map(|e| e["task"].as_str())
        .collect();
    assert_eq!(
        rows,
        ["once", "produce", "edit", "crash", "consume", "report"]
    );
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("storage/report.json")).unwrap())
            .unwrap();
    assert_eq!(report["verdict"], "pass");
    assert_eq!(
        report["tasks"].as_array().unwrap().len(),
        6,
        "the report lists the folded tasks too"
    );

    let log = std::fs::read_to_string(dir.join("state/session.jsonl")).unwrap();
    let again = run(&dir, true);
    assert!(again.status.success(), "{}", describe(&again));
    assert_eq!(
        std::fs::read_to_string(dir.join("state/session.jsonl")).unwrap(),
        log,
        "a finished run's log is closed"
    );
    assert_eq!(ran(&dir).len(), 7, "a finished run dispatches nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_resume_refuses_a_run_it_did_not_start() {
    let dir = pack("refused");
    let never = run(&dir, true);
    assert!(!never.status.success());
    assert!(
        String::from_utf8_lossy(&never.stderr).contains("never started"),
        "{}",
        describe(&never)
    );

    let killed = run(&dir, false);
    assert!(!killed.status.success(), "{}", describe(&killed));
    let log = std::fs::read_to_string(dir.join("state/session.jsonl")).unwrap();
    std::fs::write(
        dir.join("workflow.star"),
        WORKFLOW.replace("echo carried", "echo changed"),
    )
    .unwrap();
    let changed = run(&dir, true);
    assert!(!changed.status.success());
    assert!(
        String::from_utf8_lossy(&changed.stderr).contains("is not the one the interrupted run"),
        "{}",
        describe(&changed)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("state/session.jsonl")).unwrap(),
        log,
        "a refused resume writes nothing"
    );
    assert_eq!(ran(&dir), ["once", "produce", "edit", "crash"]);
    assert!(
        dir.join("workspace/edited.txt").exists(),
        "a refused resume leaves the workspace as the crash left it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An invalid verdict is a finished run, not a crash: it exits with its own code, and resuming it
/// reports the same verdict without running or writing anything.
#[test]
fn a_finished_invalid_run_exits_with_its_own_code_every_time() {
    let dir = pack("invalid");
    std::fs::write(
        dir.join("workflow.star"),
        r#"
broken = command(name = "broken", run = "echo broken >> ../ran.log && exit 1")
workflow(type = "playbook", tasks = [broken])
"#,
    )
    .unwrap();
    let first = run(&dir, false);
    assert_eq!(first.status.code(), Some(3), "{}", describe(&first));
    let log = std::fs::read_to_string(dir.join("state/session.jsonl")).unwrap();

    let again = run(&dir, true);
    assert_eq!(again.status.code(), Some(3), "{}", describe(&again));
    assert_eq!(
        std::fs::read_to_string(dir.join("state/session.jsonl")).unwrap(),
        log
    );
    assert_eq!(ran(&dir), ["broken"]);
    let _ = std::fs::remove_dir_all(&dir);
}
