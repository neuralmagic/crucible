//! `crucible-runner plan run` end to end: the built binary against real packs, real subprocesses
//! and a real git workspace.

use std::io::{Read, Write};
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
goal = "run the commands"
[workflow]
type = "playbook"
file = "workflow.star"
"#;

/// A pack in a scratch directory: the manifest above, with `workflow_extra` appended to its
/// `[workflow]` table, and `workflow` as its graph.
fn pack(workflow: &str, workflow_extra: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("scratch");
    std::fs::write(
        dir.path().join("crucible.toml"),
        format!("{MANIFEST}{workflow_extra}"),
    )
    .expect("manifest");
    std::fs::write(dir.path().join("workflow.star"), workflow).expect("workflow");
    dir
}

/// `crucible-runner` with none of this process's environment but what a shell needs.
fn runner() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_crucible-runner"));
    cmd.env_clear();
    for name in ["PATH", "HOME", "TMPDIR"] {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
    cmd
}

fn plan_run(dir: &Path, extra: &[&str]) -> Command {
    let mut cmd = runner();
    cmd.args(["plan", "run", "--manifest"])
        .arg(dir.join("crucible.toml"))
        .args(["--max-cost", "1", "--max-time", "60s"])
        .args(extra);
    cmd
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn session(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("state/session.jsonl"))
        .expect("a session log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a session event"))
        .collect()
}

/// The status each task settled with, off the session log.
fn statuses(dir: &Path) -> Vec<(String, String)> {
    session(dir)
        .iter()
        .filter(|e| e["kind"] == "task_result")
        .map(|e| {
            (
                e["task"].as_str().expect("task").to_string(),
                e["status"].as_str().expect("status").to_string(),
            )
        })
        .collect()
}

const PIPELINE: &str = r#"
build = command(name = "build", run = "echo built > artifact.txt; echo '{\"artifact\": \"app-1.2.3\"}'", emits_files = ["artifact.txt"])
test = command(name = "test", run = "echo \"{\\\"read\\\": \\\"$(cat inputs/build/artifact.txt)\\\"}\"", depends_on = [build], isolated = True)
workflow(type = "playbook", tasks = [build, test])
"#;

/// The whole playbook contract with no agent anywhere: the workspace is set up, a declared file
/// reaches an isolated consumer, a passing task is committed, and the session log ends finished.
#[test]
fn a_command_playbook_runs_to_a_valid_verdict() {
    let dir = pack(PIPELINE, "");
    let out = plan_run(dir.path(), &["--compute-driver=kubernetes"])
        .output()
        .expect("runs");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        statuses(dir.path()),
        [
            ("build".to_string(), "pass".to_string()),
            ("test".to_string(), "pass".to_string()),
        ]
    );
    let events = session(dir.path());
    let test = events
        .iter()
        .find(|e| e["kind"] == "task_result" && e["task"] == "test")
        .expect("test's result");
    assert_eq!(
        test["output"]["read"], "built",
        "the isolated task read its ancestor's file"
    );
    let last = events.last().expect("a shutdown");
    assert_eq!(last["kind"], "shutdown", "{last}");
    assert_eq!(last["outcome"], "finished", "{last}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("state/files/build/artifact.txt")).expect("file"),
        "built\n"
    );
    let log = Command::new("git")
        .arg("-C")
        .arg(dir.path().join("workspace"))
        .args(["log", "--format=%s"])
        .output()
        .expect("git log");
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).lines().next(),
        Some("task build"),
        "the passing task was committed"
    );
}

/// A required task that fails ends the run with the exit a pod wrapper does not restart on, and
/// the session log still records the outcome.
#[test]
fn an_invalid_verdict_exits_without_asking_for_a_restart() {
    let dir = pack(
        r#"
broken = command(name = "broken", run = "echo boom >&2; exit 3")
after = command(name = "after", run = "true", depends_on = [broken])
workflow(type = "playbook", tasks = [broken, after])
"#,
        "",
    );
    let out = plan_run(dir.path(), &[]).output().expect("runs");
    assert_eq!(
        out.status.code(),
        Some(i32::from(crucible::plan::INVALID_VERDICT_EXIT)),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        statuses(dir.path()),
        [
            ("broken".to_string(), "fail".to_string()),
            ("after".to_string(), "blocked".to_string()),
        ]
    );
    assert_eq!(
        session(dir.path()).last().expect("shutdown")["kind"],
        "shutdown"
    );
}

/// A route decided by a dependency's output needs no model, so it runs and its branches follow.
#[test]
fn a_route_decided_by_a_tasks_output_runs() {
    let dir = pack(
        r#"
check = command(name = "check", run = "echo '{\"verdict\": \"ship\"}'", emits = {"verdict": ["ship", "hold"]})
gate = route(
    name = "gate",
    depends_on = [check],
    decider = output(check),
    questions = {"verdict": choice(ask = "Ship it?", options = {"ship": "release now", "hold": "wait"})},
)
ship = command(name = "ship", run = "echo '{\"shipped\": true}'", when = gate.verdict.any_of("ship"))
hold = command(name = "hold", run = "echo '{\"held\": true}'", when = gate.verdict.otherwise())
workflow(type = "playbook", tasks = [check, gate, ship, hold])
"#,
        "",
    );
    let out = plan_run(dir.path(), &[]).output().expect("runs");
    assert!(out.status.success(), "{}", stderr(&out));
    let settled = statuses(dir.path());
    assert!(
        settled.contains(&("ship".to_string(), "pass".to_string())),
        "{settled:?}"
    );
    assert!(
        !settled.contains(&("hold".to_string(), "pass".to_string())),
        "{settled:?}"
    );
}

/// The desktop app's Run button passes a pack's precompiled plan beside its manifest.
#[test]
fn a_precompiled_plan_runs_in_the_manifests_workspace() {
    let dir = pack(PIPELINE, "");
    let plan = dir.path().join("plan.toml");
    std::fs::write(
        &plan,
        r#"
version = 1
[budget]
usd = 1.0
[[task]]
name = "only"
kind = "command"
command = "echo '{\"where\": \"'$(basename $PWD)'\"}'"
"#,
    )
    .expect("plan");
    let out = plan_run(dir.path(), &["--file"])
        .arg(&plan)
        .output()
        .expect("runs");
    assert!(out.status.success(), "{}", stderr(&out));
    let events = session(dir.path());
    let only = events
        .iter()
        .find(|e| e["kind"] == "task_result" && e["task"] == "only")
        .expect("the plan's task ran, not the manifest's graph");
    assert_eq!(only["status"], "pass", "{only}");
    assert_eq!(only["output"]["where"], "workspace", "{only}");
    assert!(
        !statuses(dir.path()).iter().any(|(task, _)| task == "build"),
        "the manifest's own graph ran beside the plan"
    );
}

#[test]
fn a_precompiled_plan_that_asks_a_model_is_refused_before_any_task_runs() {
    let dir = pack(PIPELINE, "");
    let plan = dir.path().join("plan.toml");
    std::fs::write(
        &plan,
        r#"
version = 1
[budget]
usd = 1.0
[[task]]
name = "first"
kind = "command"
command = "touch ran"
[[task]]
name = "think"
kind = "agent"
prompt = "decide"
depends_on = ["first"]
"#,
    )
    .expect("plan");
    let out = plan_run(dir.path(), &["--file"])
        .arg(&plan)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("task think asks an agent turn"),
        "{}",
        stderr(&out)
    );
    assert!(
        !dir.path().join("workspace/ran").exists(),
        "a task ran before the plan was refused"
    );
}

/// What the refusal of `workflow` says, with the workspace it must not have set up.
fn refused(
    workflow: &str,
    workflow_extra: &str,
    flags: &[&str],
) -> (String, PathBuf, tempfile::TempDir) {
    let dir = pack(workflow, workflow_extra);
    let out = plan_run(dir.path(), flags).output().expect("runs");
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let workspace = dir.path().join("workspace");
    (stderr(&out), workspace, dir)
}

#[test]
fn an_agent_task_is_refused_before_the_workspace_is_set_up() {
    let (why, workspace, _dir) = refused(
        r#"
a = agent(name = "a", prompt = "do it")
workflow(type = "playbook", tasks = [a])
"#,
        "",
        &[],
    );
    assert!(why.contains("task a asks an agent turn"), "{why}");
    assert!(
        !workspace.exists(),
        "the workspace was set up for a refused pack"
    );
}

#[test]
fn a_model_decided_route_is_refused_before_the_workspace_is_set_up() {
    let (why, workspace, _dir) = refused(
        r#"
read = command(name = "read", run = "echo '{\"ticket\": \"down\"}'", emits = ["ticket"])
gate = route(
    name = "gate",
    depends_on = [read],
    decider = model(min_confidence = 0.8),
    questions = {"urgent": noul(ask = "Is it urgent?", drop = ["no", "uncertain"])},
)
page = command(name = "page", run = "true", when = gate.urgent)
workflow(type = "playbook", tasks = [read, gate, page])
"#,
        "",
        &[],
    );
    assert!(why.contains("task gate asks a decision model"), "{why}");
    assert!(
        !workspace.exists(),
        "the workspace was set up for a refused pack"
    );
}

#[test]
fn a_pack_naming_workflow_servers_is_refused() {
    let (why, workspace, _dir) = refused(
        r#"
t = command(name = "t", run = "true")
workflow(type = "playbook", tasks = [t])
"#,
        "servers = [\"tracker\"]\n",
        &[],
    );
    assert!(why.contains("[workflow].servers (tracker)"), "{why}");
    assert!(
        !workspace.exists(),
        "the workspace was set up for a refused pack"
    );
}

#[test]
fn the_full_engines_agent_flags_are_refused() {
    for flag in ["--harness=claude", "--model=some-model"] {
        let (why, workspace, _dir) = refused(PIPELINE, "", &[flag]);
        assert!(why.contains("runs no agent task"), "{flag}: {why}");
        assert!(!workspace.exists(), "{flag}: the workspace was set up");
    }
}

/// An image that installs this binary as its engine checks the contract version the same way.
#[test]
fn it_reports_the_engines_contract_version() {
    let out = runner().arg("--contract-version").output().expect("runs");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        crucible_contract::CONTRACT_VERSION.to_string()
    );
}

/// One HTTP request off `stream`, answered `200`: its request line, its headers lowercased, and
/// how many body bytes it carried.
fn answer(stream: &mut std::net::TcpStream) -> (String, String, usize) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head_end, len) = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= i + 4 + len {
                break (i, len);
            }
        }
        let n = stream.read(&mut chunk).expect("read");
        assert!(n > 0, "the client hung up mid-request");
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let (line, headers) = head.split_once("\r\n").unwrap_or((head.as_str(), ""));
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .expect("respond");
    (line.to_string(), headers.to_ascii_lowercase(), len)
}

/// A run the controller dispatched uploads its record to its own run under its own token, and
/// leaves no mark asking the wrapper to print the session instead.
#[test]
fn the_run_uploads_its_record_under_its_own_token() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let controller = std::thread::spawn(move || {
        let mut got = Vec::new();
        for stream in listener.incoming() {
            let request = answer(&mut stream.expect("accept"));
            if request.0.starts_with("GET /done ") {
                break;
            }
            got.push(request);
        }
        got
    });
    let dir = pack(PIPELINE, "");
    let out = plan_run(dir.path(), &[])
        .env(
            crucible_contract::ENV_CONTROLLER_URL,
            format!("http://127.0.0.1:{port}/"),
        )
        .env(crucible_contract::ENV_RUN_NAME, "run_1")
        .env(crucible_contract::ENV_POD_TOKEN, "crr_run_1_secret")
        .output()
        .expect("runs");
    assert!(out.status.success(), "{}", stderr(&out));
    let mut done = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    done.write_all(b"GET /done HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("stop");
    let requests = controller.join().expect("controller");
    let uploaded: Vec<&str> = requests.iter().map(|(line, _, _)| line.as_str()).collect();
    for kind in ["run-files", "run-commits", "run-session"] {
        assert!(
            uploaded
                .iter()
                .any(|line| line.starts_with(&format!("PUT /api/evidence/runs/run_1/{kind} "))),
            "no {kind} among {uploaded:?}"
        );
    }
    for (line, headers, bytes) in &requests {
        assert!(
            headers.contains("authorization: bearer crr_run_1_secret"),
            "{line}: {headers}"
        );
        assert!(*bytes > 0, "{line} carried no body");
    }
    assert!(
        !dir.path()
            .join("state")
            .join(crucible_contract::RUN_SESSION_UNDELIVERED)
            .exists(),
        "a delivered session was marked undelivered"
    );
}
