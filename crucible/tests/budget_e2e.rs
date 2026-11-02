use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The agent stand-in reports a running cost as soon as it starts, works for a second, then
/// settles at the same cost: every lane in flight has said what it is spending before the first
/// one finishes and frees a slot.
const AGENT: &str = r#"#!/bin/sh
echo "$CRUCIBLE_TASK" >> "$LEDGER"
printf '{"v":1,"kind":"tokens","input":1,"output":1,"total":2,"rate":null,"cost_usd":0.1}\n'
sleep 2
printf '{"lane": "%s"}' "$CRUCIBLE_TASK" > PLAN_TASK_RESULT.json
printf '{"v":1,"kind":"result","subtype":"success","turns":1,"cost_usd":0.1}\n'
"#;

const WORKFLOW: &str = r#"
discover = command(
    name = "discover",
    run = "echo discover >> $LEDGER && printf '{\"targets\": [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\", \"k\", \"l\"]}\n'",
    emits = ["targets"],
)
analyze = agent(
    name = "analyze",
    prompt = "audit the target",
    depends_on = [discover],
    over = discover.targets,
    max_fanout = 12,
    isolated = True,
    required = False,
)
apply = command(
    name = "apply",
    run = "echo apply >> $LEDGER && python3 -c 'import json, os; print(json.dumps({\"applied\": sorted(json.load(open(os.environ[\"CRUCIBLE_INPUTS_FILE\"]))[\"analyze\"][\"outputs\"])}))'",
    depends_on = [discover, analyze],
    join = "passed",
)
fix = agent(name = "fix", prompt = "fix it", depends_on = [apply], required = False)
report = command(
    name = "report",
    run = "echo report >> $LEDGER && printf '{}\n'",
    depends_on = [apply, fix],
    join = "settled",
)
workflow(type = "playbook", tasks = [discover, analyze, apply, fix, report])
"#;

fn pack(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crucible-budget-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let agent = dir.join("agent.sh");
    std::fs::write(&agent, AGENT).unwrap();
    std::fs::write(
        dir.join("crucible.toml"),
        format!(
            r#"
[repo]
path = "."
[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
[agent]
backend = "command"
agent_cmd = "sh {}"
goal = "audit every target"
[workflow]
type = "playbook"
file = "workflow.star"
"#,
            agent.display()
        ),
    )
    .unwrap();
    std::fs::write(dir.join("workflow.star"), WORKFLOW).unwrap();
    dir
}

fn run(dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args([
            "plan",
            "run",
            "--manifest",
            "crucible.toml",
            "--max-cost",
            "0.25",
            "--max-time",
            "5m",
        ])
        .current_dir(dir)
        .env("LEDGER", dir.join("ran.log"))
        .env("FORGE_STORAGE_ROOT", dir.join("storage"))
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

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\n{}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A fan-out wider than the lanes that may run at once stops admitting lanes at the cost
/// ceiling, counting what the lanes in flight report before any of them settles. The command
/// after it still runs over the lanes that finished, the agent after it does not, and the run
/// ends invalid on the budget.
#[test]
fn a_fanout_stops_at_the_cost_ceiling_and_the_commands_after_it_still_run() {
    let dir = pack("fanout");
    let out = run(&dir);
    assert_eq!(out.status.code(), Some(3), "{}", describe(&out));

    let ran: Vec<String> = std::fs::read_to_string(dir.join("ran.log"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let lanes: Vec<&String> = ran.iter().filter(|n| n.starts_with("analyze[")).collect();
    assert_eq!(lanes.len(), 8, "only the first width of lanes ran: {ran:?}");
    assert!(ran.contains(&"apply".to_string()), "{ran:?}");
    assert!(ran.contains(&"report".to_string()), "{ran:?}");
    assert!(!ran.contains(&"fix".to_string()), "{ran:?}");

    let events = events(&dir);
    let result = |task: &str| {
        events
            .iter()
            .find(|e| e["kind"] == "task_result" && e["task"] == task)
            .unwrap_or_else(|| panic!("no row for {task}"))
            .clone()
    };
    let refused: Vec<&str> = events
        .iter()
        .filter(|e| e["kind"] == "task_result")
        .filter(|e| e["blocked"]["reason"] == "budget_ceiling")
        .filter_map(|e| e["task"].as_str())
        .collect();
    assert_eq!(refused.len(), 5, "four lanes and fix: {refused:?}");
    assert!(refused.contains(&"fix"), "{refused:?}");
    let applied = &result("apply")["output"]["applied"];
    assert_eq!(applied.as_array().map(Vec::len), Some(8), "{applied}");
    assert_eq!(result("report")["status"], "pass");
    let shutdown = events
        .iter()
        .find(|e| e["kind"] == "shutdown")
        .expect("a shutdown event");
    assert_eq!(shutdown["outcome"], "budget", "{shutdown}");
    let _ = std::fs::remove_dir_all(&dir);
}
