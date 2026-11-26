//! A playbook's call tasks run against a controller on loopback that speaks the real call API:
//! the engine binds its values, uploads its files, dispatches with the run's pod token, records
//! the dispatch, waits, and stages the target's result files for its dependents. A resumed run
//! attaches to the call it already dispatched.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crucible_contract::calls::{
    CallAlias, CallId, CallRefused, CallState, CallStatus, CallsLock, DispatchRequest,
    DispatchResponse, FileRef, LOCK_FILE, LOCK_VERSION, LockedCall, Pin, ProgressGraph,
    TargetHandle, TargetInterface,
};
use crucible_contract::decision::Label;
use crucible_contract::emits::{DeclaredFile, EmitWire, FieldType};

const TOKEN: &str = "crr_test_token";
const RUN: &str = "crucible-run-caller";

const MANIFEST: &str = r#"
[repo]
path = "."
[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
[agent]
backend = "command"
agent_cmd = "true"
goal = "call another playbook"
[workflow]
type = "playbook"
file = "workflow.star"
"#;

const WORKFLOW: &str = r#"
calls = {"release": {"kind": "playbook", "playbook": "release"}}
plan = command(
    name = "plan",
    run = "echo 'release notes' > notes.md && printf '{\"version\": \"1.2.3\", \"repos\": [\"a\", \"b\"]}\n'",
    emits = {"version": "string", "repos": "list"},
    emits_files = ["notes.md"],
)
ship = call(
    name = "ship",
    target = "release",
    params = {"version": plan.version, "notes": plan.files["notes.md"], "mode": "fast"},
    budget = budget(wall_clock = "10m"),
)
fan = call(name = "fan", target = "release", params = {"version": item()}, over = each(plan.repos, max = 4))
consume = command(
    name = "consume",
    run = "cp inputs/ship/report.md ../report.txt && printf '{}\n'",
    depends_on = [ship],
    when = ship.verdict.any_of("ok"),
)
workflow(type = "playbook", tasks = [plan, ship, fan, consume])
"#;

fn interface() -> TargetInterface {
    TargetInterface {
        params: serde_json::json!({
            "type": "object",
            "properties": {
                "version": {"type": "string"},
                "notes": {"type": "object", "x-param-type": "file", "x-max-bytes": 4096},
                "mode": {"type": "string", "enum": ["fast", "slow"]},
            },
            "required": ["version"],
        }),
        result: vec![EmitWire {
            field: "verdict".into(),
            ty: Some(FieldType::OneOf(vec![
                Label::new("ok".to_owned()).unwrap(),
                Label::new("bad".to_owned()).unwrap(),
            ])),
        }],
        result_files: vec![DeclaredFile::from("report.md")],
        graph: ProgressGraph::default(),
    }
}

fn pack(tag: &str, workflow: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crucible-call-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("crucible.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("workflow.star"), workflow).unwrap();
    let lock = CallsLock {
        version: LOCK_VERSION,
        calls: BTreeMap::from([(
            CallAlias::parse("release").unwrap(),
            LockedCall::new(
                Pin::Revision {
                    playbook: "release".into(),
                    digest: "sha256:aa".into(),
                },
                interface(),
            )
            .unwrap(),
        )]),
    };
    std::fs::write(dir.join(LOCK_FILE), lock.render().unwrap()).unwrap();
    dir
}

/// What the controller on loopback saw and holds.
#[derive(Default)]
struct Seen {
    dispatched: Vec<DispatchRequest>,
    calls: BTreeMap<String, DispatchRequest>,
    polls: BTreeMap<String, u32>,
    uploads: BTreeMap<String, Vec<u8>>,
    cancelled: Vec<String>,
    unauthorized: u32,
    param_files: BTreeMap<String, Vec<u8>>,
    fetched: Vec<String>,
}

struct Controller {
    url: String,
    seen: Arc<Mutex<Seen>>,
    hold: Arc<AtomicBool>,
    lose_answers: Arc<AtomicU32>,
}

fn report_for(request: &DispatchRequest) -> Vec<u8> {
    format!(
        "report for {}",
        request
            .values
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    )
    .into_bytes()
}

fn reply(conn: &mut std::net::TcpStream, status: &str, body: &[u8], kind: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body);
}

fn json_reply(conn: &mut std::net::TcpStream, status: &str, value: &impl serde::Serialize) {
    reply(
        conn,
        status,
        &serde_json::to_vec(value).unwrap(),
        "application/json",
    );
}

impl Controller {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let hold = Arc::new(AtomicBool::new(false));
        let lose_answers = Arc::new(AtomicU32::new(0));
        let (state, held, lose) = (seen.clone(), hold.clone(), lose_answers.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut length = 0;
                let mut authorized = false;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if lower.trim() == format!("authorization: bearer {}", TOKEN.to_lowercase()) {
                        authorized = true;
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let path = parts.next().unwrap_or_default().to_owned();
                let mut seen = state.lock().unwrap();
                if !authorized {
                    seen.unauthorized += 1;
                    reply(&mut conn, "401 Unauthorized", b"", "text/plain");
                    continue;
                }
                let base = format!("/api/runs/{RUN}/");
                let Some(rest) = path.strip_prefix(&base) else {
                    reply(&mut conn, "404 Not Found", b"", "text/plain");
                    continue;
                };
                let segments: Vec<&str> = rest.split('/').collect();
                match (method.as_str(), segments.as_slice()) {
                    ("POST", ["calls"]) => {
                        let request: DispatchRequest = serde_json::from_slice(&body).unwrap();
                        seen.dispatched.push(request.clone());
                        if request.values.get("mode") == Some(&serde_json::json!("slow")) {
                            json_reply(
                                &mut conn,
                                "422 Unprocessable Entity",
                                &CallRefused {
                                    note: "mode: slow is not allowed".into(),
                                },
                            );
                            continue;
                        }
                        let id = seen
                            .calls
                            .iter()
                            .find(|(_, r)| {
                                r.task == request.task
                                    && r.instance == request.instance
                                    && r.attempt == request.attempt
                            })
                            .map(|(id, _)| id.clone())
                            .unwrap_or_else(|| format!("c-{}", seen.calls.len() + 1));
                        seen.calls.insert(id.clone(), request);
                        if lose
                            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                            .is_ok()
                        {
                            continue;
                        }
                        json_reply(
                            &mut conn,
                            "200 OK",
                            &DispatchResponse {
                                call: CallId(id.clone()),
                                handle: TargetHandle::Playbook {
                                    run: format!("target-{id}"),
                                },
                                link: None,
                            },
                        );
                    }
                    ("GET", ["calls", id]) => {
                        *seen.polls.entry(id.to_string()).or_default() += 1;
                        let Some(request) = seen.calls.get(*id).cloned() else {
                            reply(&mut conn, "404 Not Found", b"", "text/plain");
                            continue;
                        };
                        let running = held.load(Ordering::SeqCst);
                        let report = report_for(&request);
                        json_reply(
                            &mut conn,
                            "200 OK",
                            &CallState {
                                run_number: None,
                                links: Vec::new(),
                                call: CallId(id.to_string()),
                                status: if running {
                                    CallStatus::Running
                                } else {
                                    CallStatus::Passed
                                },
                                note: String::new(),
                                result: if running {
                                    serde_json::Value::Null
                                } else {
                                    serde_json::json!({"verdict": "ok", "seen": request.values})
                                },
                                result_files: if running {
                                    Vec::new()
                                } else {
                                    vec![FileRef {
                                        digest: crucible_contract::artifact::content_digest(
                                            &report,
                                        ),
                                        size: report.len() as u64,
                                        path: Some("report.md".into()),
                                        media_type: None,
                                    }]
                                },
                                progress: ProgressGraph::default(),
                                handle: TargetHandle::Playbook {
                                    run: format!("target-{id}"),
                                },
                                link: None,
                            },
                        );
                    }
                    ("POST", ["calls", id, "cancel"]) => {
                        seen.cancelled.push(id.to_string());
                        reply(&mut conn, "200 OK", b"{}", "application/json");
                    }
                    ("GET", ["calls", id, "files", "report.md"]) => match seen.calls.get(*id) {
                        Some(request) => {
                            let report = report_for(request);
                            reply(&mut conn, "200 OK", &report, "application/octet-stream")
                        }
                        None => reply(&mut conn, "404 Not Found", b"", "text/plain"),
                    },
                    ("GET", ["param-files", digest]) => {
                        let digest = digest.replace("%3A", ":");
                        seen.fetched.push(digest.clone());
                        match seen.param_files.get(&digest).cloned() {
                            Some(bytes) => {
                                reply(&mut conn, "200 OK", &bytes, "application/octet-stream")
                            }
                            None => reply(&mut conn, "404 Not Found", b"", "text/plain"),
                        }
                    }
                    ("PUT", ["call-files", digest]) => {
                        let digest = digest.replace("%3A", ":");
                        assert_eq!(
                            crucible_contract::artifact::content_digest(&body),
                            digest,
                            "an upload carries the bytes its digest names"
                        );
                        seen.uploads.insert(digest, body);
                        reply(&mut conn, "200 OK", b"", "text/plain");
                    }
                    _ => reply(&mut conn, "404 Not Found", b"", "text/plain"),
                }
            }
        });
        Controller {
            url,
            seen,
            hold,
            lose_answers,
        }
    }
}

fn command(dir: &Path, controller: Option<&Controller>, resume: bool) -> Command {
    command_with(dir, controller, resume, &[])
}

fn command_with(
    dir: &Path,
    controller: Option<&Controller>,
    resume: bool,
    params: &[String],
) -> Command {
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
    for param in params {
        args.push("--param");
        args.push(param);
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_crucible"));
    cmd.args(args)
        .current_dir(dir)
        .env("FORGE_STORAGE_ROOT", dir.join("storage"))
        .env("CRUCIBLE_RUN_NAME", RUN)
        .env("CRUCIBLE_CALL_POLL_SECS", "1")
        .env_remove("CRUCIBLE_CONTROLLER_URL")
        .env_remove("CRUCIBLE_POD_TOKEN");
    if let Some(controller) = controller {
        cmd.env("CRUCIBLE_CONTROLLER_URL", &controller.url)
            .env("CRUCIBLE_POD_TOKEN", TOKEN);
    }
    cmd
}

fn events(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("state/session.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn results(dir: &Path) -> BTreeMap<String, serde_json::Value> {
    events(dir)
        .into_iter()
        .filter(|e| e["kind"] == "task_result")
        .filter_map(|e| Some((e["task"].as_str()?.to_owned(), e)))
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
fn a_call_dispatches_waits_and_hands_its_result_to_dependents() {
    let dir = pack("dispatch", WORKFLOW);
    let controller = Controller::start();
    let out = command(&dir, Some(&controller), false).output().unwrap();
    assert!(out.status.success(), "{}", describe(&out));

    let results = results(&dir);
    for task in ["plan", "ship", "fan", "fan[a]", "fan[b]", "consume"] {
        assert_eq!(
            results[task]["status"], "pass",
            "{task}: {:?}",
            results[task]
        );
    }
    assert_eq!(results["ship"]["output"]["verdict"], "ok");
    assert_eq!(
        std::fs::read_to_string(dir.join("report.txt")).unwrap(),
        "report for 1.2.3",
        "the target's result file is staged for the call's dependents"
    );

    let seen = controller.seen.lock().unwrap();
    assert_eq!(seen.unauthorized, 0);
    assert_eq!(
        seen.dispatched.len(),
        3,
        "one for ship and one per fanned item"
    );
    let ship = seen
        .dispatched
        .iter()
        .find(|r| r.task == "ship")
        .expect("ship dispatched");
    assert_eq!(ship.instance, None);
    assert_eq!(ship.attempt, 1);
    assert_eq!(ship.interface_digest, interface().digest().unwrap());
    assert_eq!(ship.values["version"], "1.2.3");
    assert_eq!(ship.values["mode"], "fast");
    assert_eq!(ship.budget.and_then(|b| b.wall_clock_secs), Some(600));
    let notes = &ship.files["notes"];
    assert_eq!(notes.path.as_deref(), Some("notes.md"));
    assert_eq!(seen.uploads[&notes.digest], b"release notes\n");
    let mut fanned: Vec<(Option<&str>, &serde_json::Value)> = seen
        .dispatched
        .iter()
        .filter(|r| r.task == "fan")
        .map(|r| (r.instance.as_deref(), &r.values["version"]))
        .collect();
    fanned.sort_by_key(|(instance, _)| *instance);
    assert_eq!(
        fanned,
        [
            (Some("a"), &serde_json::json!("a")),
            (Some("b"), &serde_json::json!("b"))
        ]
    );

    let dispatched: Vec<serde_json::Value> = events(&dir)
        .into_iter()
        .filter(|e| e["kind"] == "call_dispatched")
        .collect();
    assert_eq!(dispatched.len(), 3);
    let recorded = dispatched.iter().find(|e| e["task"] == "ship").unwrap();
    assert_eq!(recorded["alias"], "release");
    assert_eq!(recorded["handle"]["kind"], "playbook");
    let log = std::fs::read_to_string(dir.join("state/session.jsonl")).unwrap();
    assert!(
        !log.contains(TOKEN),
        "the pod token never reaches the session log"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dispatch_whose_answer_is_lost_is_resent_under_the_same_identity() {
    let dir = pack("lost-answer", WORKFLOW);
    let controller = Controller::start();
    controller.lose_answers.store(2, Ordering::SeqCst);
    let out = command(&dir, Some(&controller), false).output().unwrap();
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(results(&dir)["ship"]["status"], "pass");

    let seen = controller.seen.lock().unwrap();
    assert_eq!(
        seen.dispatched.len(),
        5,
        "three calls, two of them sent twice after a lost answer"
    );
    assert!(
        seen.dispatched.iter().all(|r| r.attempt == 1),
        "a resend carries the same attempt: {:?}",
        seen.dispatched
            .iter()
            .map(|r| (&r.task, &r.instance, r.attempt))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        seen.calls.len(),
        3,
        "one call for ship and one per fanned item, however many sends"
    );
}

#[test]
fn a_resumed_run_attaches_to_the_call_it_dispatched() {
    let workflow = r#"
calls = {"release": {"kind": "playbook", "playbook": "release"}}
plan = command(
    name = "plan",
    run = "echo notes > notes.md && printf '{\"version\": \"2.0\"}\n'",
    emits = {"version": "string"},
    emits_files = ["notes.md"],
)
ship = call(name = "ship", target = "release", params = {"version": plan.version})
workflow(type = "playbook", tasks = [plan, ship])
"#;
    let dir = pack("resume", workflow);
    let controller = Controller::start();
    controller.hold.store(true, Ordering::SeqCst);
    let mut first = command(&dir, Some(&controller), false).spawn().unwrap();
    let started = std::time::Instant::now();
    while controller.seen.lock().unwrap().polls.is_empty() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "the call was never polled"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    first.kill().unwrap();
    let _ = first.wait();
    assert!(
        events(&dir).iter().any(|e| e["kind"] == "call_dispatched"),
        "the dispatch is recorded before the call waits"
    );

    controller.hold.store(false, Ordering::SeqCst);
    let out = command(&dir, Some(&controller), true).output().unwrap();
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(results(&dir)["ship"]["status"], "pass");
    let seen = controller.seen.lock().unwrap();
    assert_eq!(
        seen.dispatched.len(),
        1,
        "a resumed run attaches to the recorded call and dispatches nothing"
    );
    assert!(seen.cancelled.is_empty(), "an interruption cancels nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_call_settles_failing_when_refused_or_when_no_controller_launched_the_run() {
    let refused = r#"
calls = {"release": {"kind": "playbook", "playbook": "release"}}
plan = command(name = "plan", run = "printf '{\"version\": \"1\"}\n'", emits = {"version": "string"})
ship = call(name = "ship", target = "release", params = {"version": plan.version, "mode": "slow"})
workflow(type = "playbook", tasks = [plan, ship])
"#;
    let dir = pack("refused", refused);
    let controller = Controller::start();
    let out = command(&dir, Some(&controller), false).output().unwrap();
    assert!(!out.status.success(), "{}", describe(&out));
    let ship = &results(&dir)["ship"];
    assert_eq!(ship["status"], "fail");
    assert!(
        ship["note"]
            .as_str()
            .unwrap()
            .contains("mode: slow is not allowed"),
        "{ship}"
    );
    assert!(controller.seen.lock().unwrap().polls.is_empty());
    let _ = std::fs::remove_dir_all(&dir);

    let dir = pack("unset", refused);
    let out = command(&dir, None, false).output().unwrap();
    assert!(!out.status.success(), "{}", describe(&out));
    let ship = &results(&dir)["ship"];
    assert_eq!(ship["status"], "fail");
    assert!(
        ship["note"]
            .as_str()
            .unwrap()
            .contains("CRUCIBLE_CONTROLLER_URL"),
        "{ship}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pack_whose_lock_does_not_match_its_calls_is_refused() {
    let dir = pack(
        "stale",
        &WORKFLOW.replace("\"playbook\": \"release\"", "\"playbook\": \"other\""),
    );
    let out = command(&dir, None, false).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(LOCK_FILE),
        "{}",
        describe(&out)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_call_binds_another_calls_result_field_and_file() {
    let workflow = r#"
calls = {"release": {"kind": "playbook", "playbook": "release"}}
plan = command(name = "plan", run = "printf '{\"version\": \"1.2.3\"}\n'", emits = {"version": "string"})
ship = call(name = "ship", target = "release", params = {"version": plan.version})
again = call(
    name = "again",
    target = "release",
    params = {"version": ship.verdict, "notes": ship.files["report.md"]},
)
workflow(type = "playbook", tasks = [plan, ship, again])
"#;
    let dir = pack("chained", workflow);
    let controller = Controller::start();
    let out = command(&dir, Some(&controller), false).output().unwrap();
    assert!(out.status.success(), "{}", describe(&out));
    let results = results(&dir);
    assert_eq!(results["again"]["status"], "pass", "{:?}", results["again"]);

    let seen = controller.seen.lock().unwrap();
    let again = seen
        .dispatched
        .iter()
        .find(|r| r.task == "again")
        .expect("again dispatched");
    assert_eq!(
        again.values["version"], "ok",
        "the second call reads the first call's result field"
    );
    let notes = &again.files["notes"];
    assert_eq!(notes.path.as_deref(), Some("report.md"));
    assert_eq!(
        seen.uploads[&notes.digest], b"report for 1.2.3",
        "the second call sends the first call's result file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_param_off_the_engines_disk_is_fetched_from_the_controller() {
    let workflow = r#"params = {"notes": {"type": "file", "required": True, "max_bytes": 4096}}
calls = {"release": {"kind": "playbook", "playbook": "release"}}
ship = call(name = "ship", target = "release", params = {"version": "1", "notes": param("notes")})
workflow(type = "playbook", tasks = [ship])
"#;
    let dir = pack("param-file", workflow);
    let controller = Controller::start();
    let bytes = b"notes held by the controller\n".to_vec();
    let digest = crucible_contract::artifact::content_digest(&bytes);
    controller
        .seen
        .lock()
        .unwrap()
        .param_files
        .insert(digest.clone(), bytes.clone());
    let value = serde_json::json!({
        "path": "/controller/scratch/param-files/notes.md",
        "digest": digest,
        "size": bytes.len(),
        "media_type": "text/markdown",
    });
    let params = [format!("notes={value}")];
    let out = command_with(&dir, Some(&controller), false, &params)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(results(&dir)["ship"]["status"], "pass");

    let staged = dir
        .join("state/param-files")
        .join(digest.trim_start_matches("sha256:"))
        .join("notes.md");
    assert_eq!(std::fs::read(&staged).unwrap(), bytes);
    assert!(
        std::fs::metadata(&staged).unwrap().permissions().readonly(),
        "a fetched file parameter is staged read-only"
    );
    {
        let seen = controller.seen.lock().unwrap();
        assert_eq!(seen.fetched, std::slice::from_ref(&digest));
        let ship = seen.dispatched.iter().find(|r| r.task == "ship").unwrap();
        assert_eq!(ship.files["notes"].digest, digest);
        assert_eq!(
            ship.files["notes"].media_type.as_deref(),
            Some("text/markdown")
        );
        assert_eq!(seen.uploads[&digest], bytes);
    }

    let tampered = serde_json::json!({
        "path": "/controller/scratch/param-files/other.md",
        "digest": crucible_contract::artifact::content_digest(b"something else"),
    });
    let dir = pack("param-file-missing", workflow);
    let out = command_with(
        &dir,
        Some(&controller),
        false,
        &[format!("notes={tampered}")],
    )
    .output()
    .unwrap();
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("404"),
        "{}",
        describe(&out)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_mapped_call_dispatches_its_instances_together() {
    let workflow = r#"
calls = {"release": {"kind": "playbook", "playbook": "release"}}
plan = command(name = "plan", run = "printf '{\"repos\": [\"a\", \"b\", \"c\"]}\n'", emits = {"repos": "list"})
fan = call(name = "fan", target = "release", params = {"version": item()}, over = each(plan.repos, max = 4))
workflow(type = "playbook", tasks = [plan, fan])
"#;
    let dir = pack("concurrent", workflow);
    let controller = Controller::start();
    controller.hold.store(true, Ordering::SeqCst);
    let mut run = command(&dir, Some(&controller), false).spawn().unwrap();
    let started = std::time::Instant::now();
    while controller.seen.lock().unwrap().calls.len() < 3 {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "every instance is dispatched while the first is still running"
        );
        if run.try_wait().unwrap().is_some() {
            panic!("the run ended before its calls were released");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    controller.hold.store(false, Ordering::SeqCst);
    let status = run.wait().unwrap();
    assert!(status.success());
    let results = results(&dir);
    for task in ["fan[a]", "fan[b]", "fan[c]"] {
        assert_eq!(results[task]["status"], "pass", "{task}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
