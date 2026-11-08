//! `crucible plan run --manifest` over a playbook whose agent tasks talk: the real binary, the
//! deterministic agent stand-in printing native events, and the session log the run leaves.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

const WORKFLOW: &str = r#"
seed = command(name = "seed", run = "printf '{\"keys\": [\"a\", \"b\"]}\n'", emits = ["keys"])
edit = agent(name = "edit", prompt = "Edit one kernel.", over = seed.keys, max_fanout = 4, depends_on = [seed])
workflow(type = "playbook", tasks = [seed, edit])
"#;

fn pack(tag: &str, agents: &Value) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "crucible-transcript-e2e-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    std::fs::write(dir.join("workflow.star"), WORKFLOW).unwrap();
    std::fs::write(dir.join("agents.json"), agents.to_string()).unwrap();
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
agent_cmd = "python3 {}"
goal = "say what you do"
[agent.env]
FAKE_AGENT_SCRIPT = "{}"
[workflow]
type = "playbook"
file = "workflow.star"
"#,
            root.join("tools/fake-agent.py").display(),
            dir.join("agents.json").display(),
        ),
    )
    .unwrap();
    dir
}

fn session(dir: &Path) -> (bool, String, Vec<Value>) {
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
        .env("CRUCIBLE_TURN_TRACE_REDACT", "1")
        .env_remove("CRUCIBLE_HISTORY")
        .env_remove("CRUCIBLE_INFERENCE")
        .output()
        .expect("run crucible");
    let log = std::fs::read_to_string(dir.join("state/session.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        log,
    )
}

#[test]
fn each_fanout_instance_records_its_own_whole_transcript() {
    let long = "x".repeat(400);
    let speaks = json!({
            "events": [
                {"kind": "thinking", "delta": "Which kernel "},
                {"kind": "thinking", "delta": "is mine?"},
                {"kind": "text", "delta": "I am editing {ENV:CRUCIBLE_TASK}.\n"},
                {"kind": "text", "delta": format!("A long second line kept whole: {long}")},
                {"kind": "tool", "name": "Bash", "summary": "$ curl -H 'Authorization: Bearer s3cr3t' https://x"},
                {"kind": "raw", "text": "undecoded noise", "stream": "stderr"},
            ],
            "cost_usd": 0.01,
            "result": {"ok": true},
    });
    let agents = json!({"edit[a]": speaks, "edit[b]": speaks});
    let dir = pack("fanout", &agents);
    let (ok, stderr, log) = session(&dir);
    assert!(ok, "{stderr}");

    let transcript: Vec<&Value> = log.iter().filter(|e| e["kind"] == "task_agent").collect();
    for instance in ["edit[a]", "edit[b]"] {
        let mine: Vec<&Value> = transcript
            .iter()
            .copied()
            .filter(|e| e["task"] == instance)
            .collect();
        assert!(mine.iter().all(|e| e["attempt"] == 1), "{mine:?}");
        let spoken: Vec<&Value> = mine
            .iter()
            .map(|e| &e["event"])
            .filter(|e| e["kind"] != "log")
            .collect();
        let kinds: Vec<&str> = spoken.iter().filter_map(|e| e["kind"].as_str()).collect();
        assert_eq!(
            kinds,
            ["thinking", "text", "tool", "result"],
            "{instance}: {mine:?}"
        );
        assert_eq!(spoken[0]["delta"], "Which kernel is mine?");
        let said = spoken[1]["delta"].as_str().unwrap();
        assert!(
            said.starts_with(&format!("I am editing {instance}.\n")),
            "{said}"
        );
        assert!(said.ends_with(&long), "a long line is kept whole: {said}");
        let summary = spoken[2]["summary"].as_str().unwrap();
        assert!(!summary.contains("s3cr3t"), "redaction applies: {summary}");
    }
    assert!(
        transcript.iter().all(|e| e["event"]["kind"] != "raw"),
        "undecoded lines stay on the console"
    );
    assert!(
        log.iter()
            .all(|e| e["kind"] != "task_agent" || e["task"] != "seed"),
        "a command task has no agent transcript"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
