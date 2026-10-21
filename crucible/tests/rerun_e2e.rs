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
goal = "rerun from a failed task"
[workflow]
type = "playbook"
file = "workflow.star"
"#;

/// once -> produce -> edit -> check -> consume, and a report in the epilogue. Every task appends
/// to a ledger outside the workspace, so a task that ran in both runs shows up twice. `check`
/// fails until it is edited.
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
    run = "echo edit >> ../ran.log && echo kept > edited.txt && printf '{}\n'",
    depends_on = [produce],
)
check = command(
    name = "check",
    run = "echo check >> ../ran.log && exit 1",
    depends_on = [edit],
)
consume = command(
    name = "consume",
    run = "echo consume >> ../ran.log && cp inputs/produce/out.txt ../consumed.txt && printf '{}\n'",
    depends_on = [check],
)
report = command(name = "report", run = "echo report >> ../ran.log && printf '{}\n'", stage = "epilogue")
workflow(type = "playbook", tasks = [once, produce, edit, check, consume, report])
"#;

const FIXED_CHECK: &str =
    r#"run = "echo check >> ../ran.log && cat edited.txt > ../seen.log && printf '{}\n'","#;

fn pack(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crucible-rerun-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("crucible.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("workflow.star"), WORKFLOW).unwrap();
    dir
}

fn run(dir: &Path, name: &str, rerun: &[&str]) -> Output {
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
    args.extend_from_slice(rerun);
    Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(args)
        .current_dir(dir)
        .env("FORGE_STORAGE_ROOT", dir.join("storage"))
        .env("CRUCIBLE_RUN_NAME", name)
        .output()
        .expect("run crucible")
}

fn edit_workflow(dir: &Path, from: &str, to: &str) {
    let path = dir.join("workflow.star");
    let src = std::fs::read_to_string(&path).unwrap();
    assert!(src.contains(from), "{from}");
    std::fs::write(path, src.replace(from, to)).unwrap();
}

/// Set the parent's state aside, as a launcher hands a rerun its parent's evidence.
fn keep_parent(dir: &Path) {
    std::fs::rename(dir.join("state"), dir.join("parent")).unwrap();
}

fn events(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("state/session.jsonl"))
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
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

const RERUN_CHECK: &[&str] = &["--rerun-of", "parent", "--from", "check"];

#[test]
fn a_rerun_from_an_edited_task_reuses_upstream_work_and_its_commits() {
    let dir = pack("edited");
    let first = run(&dir, "crucible-run-parent", &[]);
    assert!(!first.status.success(), "{}", describe(&first));
    assert_eq!(ran(&dir), ["once", "produce", "edit", "check", "report"]);
    let parent_memory: Vec<String> =
        serde_json::from_slice(&std::fs::read(dir.join("state/git-memory/tasks.json")).unwrap())
            .unwrap();
    assert_eq!(parent_memory, ["produce", "edit"]);

    keep_parent(&dir);
    edit_workflow(
        &dir,
        r#"run = "echo check >> ../ran.log && exit 1","#,
        FIXED_CHECK,
    );
    // A fresh checkout of the same base: the baseline commit differs, its tree does not.
    std::fs::remove_dir_all(dir.join("workspace")).unwrap();

    let rerun = run(&dir, "crucible-run-child", RERUN_CHECK);
    assert!(rerun.status.success(), "{}", describe(&rerun));
    assert_eq!(
        &ran(&dir)[5..],
        ["check", "consume", "report"],
        "upstream tasks are not dispatched again"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("seen.log")).unwrap(),
        "kept\n",
        "the reused edit's commit is in the rerun's workspace"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("consumed.txt")).unwrap(),
        "carried\n",
        "the reused producer's captured file is staged downstream"
    );

    let log = events(&dir);
    let reused: Vec<(&str, &str)> = log
        .iter()
        .filter(|e| e["kind"] == "task_result")
        .filter_map(|e| Some((e["task"].as_str()?, e["reused_from"].as_str()?)))
        .collect();
    assert_eq!(
        reused,
        [
            ("once", "crucible-run-parent"),
            ("produce", "crucible-run-parent"),
            ("edit", "crucible-run-parent"),
        ]
    );
    let admitted = log.iter().find(|e| e["kind"] == "plan_admitted").unwrap();
    assert!(
        admitted["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["definition"]
                .as_str()
                .is_some_and(|d| d.starts_with("sha256:"))),
        "{admitted}"
    );
    let memory: Vec<String> =
        serde_json::from_slice(&std::fs::read(dir.join("state/git-memory/tasks.json")).unwrap())
            .unwrap();
    assert_eq!(
        memory,
        ["produce", "edit"],
        "replayed commits are the rerun's own git memory"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rerun_refuses_an_edit_upstream_of_its_selection() {
    let dir = pack("upstream");
    let first = run(&dir, "crucible-run-parent", &[]);
    assert!(!first.status.success(), "{}", describe(&first));
    keep_parent(&dir);
    edit_workflow(&dir, "echo kept > edited.txt", "echo changed > edited.txt");

    let refused = run(&dir, "crucible-run-child", RERUN_CHECK);
    assert!(!refused.status.success(), "{}", describe(&refused));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("edit changed since the parent run"),
        "{}",
        describe(&refused)
    );
    assert_eq!(ran(&dir).len(), 5, "nothing dispatched");

    let from_edit = run(
        &dir,
        "crucible-run-child",
        &["--rerun-of", "parent", "--from", "edit"],
    );
    assert!(
        !from_edit.status.success(),
        "check still fails: {}",
        describe(&from_edit)
    );
    assert_eq!(&ran(&dir)[5..], ["edit", "check", "report"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rerun_needs_a_finished_parent() {
    let dir = pack("unfinished");
    let first = run(&dir, "crucible-run-parent", &[]);
    assert!(!first.status.success(), "{}", describe(&first));
    keep_parent(&dir);
    let log = std::fs::read_to_string(dir.join("parent/session.jsonl")).unwrap();
    let open: Vec<&str> = log
        .lines()
        .filter(|line| !line.contains(r#""kind":"shutdown""#))
        .collect();
    std::fs::write(dir.join("parent/session.jsonl"), open.join("\n")).unwrap();

    let refused = run(&dir, "crucible-run-child", RERUN_CHECK);
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("has not shut down"),
        "{}",
        describe(&refused)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
