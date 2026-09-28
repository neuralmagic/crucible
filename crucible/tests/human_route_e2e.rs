//! `crucible plan run` over a plan whose route a person decides: the real binary, real shell
//! tasks, and the controller's elicitation endpoint stood in for by a real socket that answers in
//! the `crucible_contract::elicit` shape.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PLAN: &str = r#"
version = 1
[budget]
usd = 5.0

[[task]]
name = "scan"
kind = "command"
command = "echo '{\"ticket\": \"decode stalls when the batch is full\"}'"

[[task]]
name = "gate"
kind = "route"
needs = "human"
depends_on = ["scan"]
decider = { kind = "human", via = "slack", deadline_secs = 3600 }
[task.questions.area]
instructions = "Which component?"
type = "choice"
options = [{ label = "scheduler", description = "batching and queueing" }, { label = "frontend" }]
[task.questions.urgent]
instructions = "Reply within the hour?"
type = "noul"
drop = ["no", "uncertain"]

[[task]]
name = "fix"
kind = "command"
command = "touch fix.ran && echo '{}'"
depends_on = ["gate"]
when = { task = "gate", question = "area", is = ["scheduler"] }

[[task]]
name = "punt"
kind = "command"
command = "touch punt.ran && echo '{}'"
depends_on = ["gate"]
when = { task = "gate", question = "area", is = ["frontend", "uncertain"] }

[[task]]
name = "page"
kind = "command"
command = "touch page.ran && echo '{}'"
depends_on = ["gate"]
when = { task = "gate", question = "urgent", is = ["yes"] }

[[task]]
name = "wrap"
kind = "command"
command = "touch wrap.ran && echo '{}'"
depends_on = ["fix", "punt"]
join = "passed"
"#;

fn status(expires_in_secs: u64, answers: &[(&str, &str)]) -> (&'static str, String) {
    let answers: serde_json::Map<String, serde_json::Value> = answers
        .iter()
        .map(|(q, l)| ((*q).to_owned(), serde_json::Value::from(*l)))
        .collect();
    (
        "200 OK",
        serde_json::json!({"expires_in_secs": expires_in_secs, "answers": answers}).to_string(),
    )
}

/// Answer each request with the next scripted response, repeating the last; keep each request.
fn serve(script: Vec<(&'static str, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/api/pods/loop-1/elicitations",
        listener.local_addr().unwrap()
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for (served, stream) in listener.incoming().enumerate() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut payload = vec![0u8; length];
            reader.read_exact(&mut payload).unwrap();
            log.lock()
                .unwrap()
                .push(format!("{head}\n{}", String::from_utf8(payload).unwrap()));
            let (status, body) = &script[served.min(script.len() - 1)];
            write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (url, seen)
}

fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crucible-human-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plan.toml"), PLAN).unwrap();
    std::fs::write(dir.join("token"), "pod-bound-token\n").unwrap();
    dir
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn run(dir: &Path, url: Option<&str>, extra: &[&str]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_crucible"));
    cmd.args(["plan", "run", "--file", "plan.toml"])
        .args(extra)
        .current_dir(dir)
        .env_remove("CRUCIBLE_INFERENCE")
        .env_remove(crucible_contract::elicit::ENV_ELICIT_URL)
        .env_remove(crucible_contract::elicit::ENV_ELICIT_TOKEN_PATH);
    if let Some(url) = url {
        cmd.env(crucible_contract::elicit::ENV_ELICIT_URL, url).env(
            crucible_contract::elicit::ENV_ELICIT_TOKEN_PATH,
            dir.join("token"),
        );
    }
    let out = cmd.output().expect("run crucible");
    Run {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn status_of<'a>(stdout: &'a str, task: &str) -> &'a str {
    stdout
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some(task)).then(|| words.next().unwrap_or(""))
        })
        .unwrap_or_else(|| panic!("no row for {task} in:\n{stdout}"))
}

fn ran(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.unwrap().file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".ran").map(str::to_owned))
        .collect();
    names.sort();
    names
}

#[test]
fn a_persons_answers_run_their_branches_and_are_recorded_as_certain() {
    let (url, seen) = serve(vec![status(
        3500,
        &[("area", "scheduler"), ("urgent", "yes")],
    )]);
    let dir = workdir("answered");
    let run = run(&dir, Some(&url), &[]);
    assert!(run.ok, "{}\n{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("verdict: valid"), "{}", run.stdout);
    assert_eq!(ran(&dir), ["fix", "page", "wrap"]);
    assert_eq!(status_of(&run.stdout, "gate"), "pass");
    assert_eq!(status_of(&run.stdout, "punt"), "not_taken");
    assert!(
        run.stdout.contains(
            r#""area":{"confidence":1.0,"label":"scheduler","probabilities":{"scheduler":1.0}}"#
        ),
        "{}",
        run.stdout
    );

    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1, "an answered question is not polled");
    let request = &requests[0];
    assert!(
        request.starts_with("PUT /api/pods/loop-1/elicitations/gate HTTP/1.1"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer pod-bound-token\r\n"),
        "{request}"
    );
    let body: crucible_contract::elicit::ElicitRequest =
        serde_json::from_str(request.rsplit('\n').next().unwrap()).unwrap();
    assert_eq!(body.deadline_secs, 3600);
    assert_eq!(
        body.questions
            .keys()
            .map(|q| q.as_str())
            .collect::<Vec<_>>(),
        ["area", "urgent"]
    );
    assert!(
        !request.contains("decode stalls"),
        "no task output reaches the person: {request}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_answer_given_while_the_run_waits_is_read_on_the_next_poll() {
    let (url, seen) = serve(vec![
        status(3600, &[("urgent", "no")]),
        status(3595, &[("urgent", "no"), ("area", "frontend")]),
    ]);
    let dir = workdir("polled");
    let run = run(&dir, Some(&url), &[]);
    assert!(run.ok, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(ran(&dir), ["punt", "wrap"]);
    assert_eq!(status_of(&run.stdout, "fix"), "not_taken");
    assert_eq!(status_of(&run.stdout, "page"), "not_taken");
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].starts_with("GET /api/pods/loop-1/elicitations/gate HTTP/1.1"),
        "{}",
        requests[1]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_deadline_nobody_met_takes_the_uncertain_branch_and_keeps_the_valid_verdict() {
    let (url, _seen) = serve(vec![status(0, &[("urgent", "yes")])]);
    let dir = workdir("expired");
    let run = run(&dir, Some(&url), &[]);
    assert!(run.ok, "{}\n{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("verdict: valid"), "{}", run.stdout);
    assert_eq!(ran(&dir), ["page", "punt", "wrap"]);
    assert!(
        run.stdout
            .contains(r#""area":{"confidence":0.0,"label":"uncertain","probabilities":{}}"#),
        "{}",
        run.stdout
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_runs_wall_clock_ceiling_ends_the_wait_and_the_run() {
    let (url, _seen) = serve(vec![status(3600, &[])]);
    let dir = workdir("ceiling");
    let started = Instant::now();
    let run = run(&dir, Some(&url), &["--max-time", "2s"]);
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(!run.ok, "{}", run.stdout);
    assert!(
        run.stdout.contains("wall-clock ceiling reached"),
        "{}",
        run.stdout
    );
    assert_eq!(status_of(&run.stdout, "gate"), "fail");
    assert_eq!(status_of(&run.stdout, "fix"), "blocked");
    assert_eq!(status_of(&run.stdout, "punt"), "blocked");
    assert_eq!(ran(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_answer_outside_the_declared_labels_fails_the_route_and_is_not_asked_again() {
    let (url, seen) = serve(vec![status(3600, &[("area", "kv_cache")])]);
    let dir = workdir("undeclared");
    let run = run(&dir, Some(&url), &[]);
    assert!(!run.ok, "{}", run.stdout);
    assert_eq!(status_of(&run.stdout, "gate"), "fail");
    assert!(
        run.stdout.contains("undeclared label \"kv_cache\""),
        "{}",
        run.stdout
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(ran(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_endpoint_that_keeps_failing_is_retried_as_transport() {
    let (url, seen) = serve(vec![("503 Service Unavailable", "{}".to_owned())]);
    let dir = workdir("transport");
    let run = run(&dir, Some(&url), &[]);
    assert!(!run.ok, "{}", run.stdout);
    assert_eq!(status_of(&run.stdout, "gate"), "transport");
    assert_eq!(seen.lock().unwrap().len(), 3, "one ask and two retries");
    assert_eq!(ran(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_human_route_with_no_endpoint_configured_truncates_before_any_dispatch() {
    let dir = workdir("unconfigured");
    let run = run(&dir, None, &[]);
    assert!(!run.ok, "{}", run.stdout);
    assert!(run.stdout.contains("truncated at gate"), "{}", run.stdout);
    assert_eq!(ran(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}
