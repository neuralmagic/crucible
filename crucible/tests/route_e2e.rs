//! `crucible plan run` over a routed plan: the real binary, real shell tasks, and a decision
//! endpoint served over a real socket, in the wire shape vLLM's System One server or OpenAI's
//! Decisions API answers in.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const MODEL_PLAN: &str = r#"
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
needs = "decision"
depends_on = ["scan"]
decider = { kind = "model", min_confidence = 0.8 }
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
command = "touch fix.ran && echo '{\"fixed\": true}'"
depends_on = ["gate"]
when = { task = "gate", question = "area", is = ["scheduler"] }

[[task]]
name = "verify"
kind = "command"
command = "touch verify.ran && echo '{}'"
depends_on = ["fix"]

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
depends_on = ["verify", "punt"]
join = "passed"
"#;

fn answers(scheduler: f64, urgent: f64) -> String {
    format!(
        r#"{{"model":"dgemma","answers":{{"area":{{"type":"choice","choice":"scheduler","probabilities":{{"scheduler":{scheduler},"frontend":{}}},"confidence":{scheduler}}},"urgent":{{"type":"noul","noul":{urgent}}}}},"usage":{{"input_tokens":90}}}}"#,
        1.0 - scheduler
    )
}

/// Answer every connection with `status` and `body` until the test ends; keep each request.
fn serve(status: &'static str, body: String) -> (String, Arc<Mutex<Vec<String>>>) {
    serve_at("/v1/systemone", status, body)
}

fn serve_at(
    path: &'static str,
    status: &'static str,
    body: String,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}{path}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
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

fn workdir(tag: &str, plan: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crucible-route-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plan.toml"), plan).unwrap();
    dir
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn inference(url: &str, key_env: Option<&str>) -> String {
    let mut binding = serde_json::json!({
        "role": "decision", "protocol": "system_one", "url": url, "model": "dgemma",
    });
    if let Some(name) = key_env {
        binding["key_env"] = name.into();
    }
    serde_json::json!({"version": 1, "bindings": [binding]}).to_string()
}

fn run(dir: &Path, url: Option<&str>, key: Option<&str>) -> Run {
    let document = url.map(|url| inference(url, key.map(|_| "ROUTE_E2E_KEY")));
    run_with(dir, document.as_deref(), key)
}

fn run_with(dir: &Path, document: Option<&str>, key: Option<&str>) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_crucible"));
    cmd.args(["plan", "run", "--file", "plan.toml"])
        .current_dir(dir)
        .env_remove("CRUCIBLE_INFERENCE")
        .env_remove("ROUTE_E2E_KEY");
    if let Some(document) = document {
        cmd.env("CRUCIBLE_INFERENCE", document);
    }
    if let Some(key) = key {
        cmd.env("ROUTE_E2E_KEY", key);
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
fn a_confident_answer_runs_its_branch_and_leaves_the_other_untaken() {
    let (url, seen) = serve("200 OK", answers(0.93, 0.02));
    let dir = workdir("confident", MODEL_PLAN);
    let run = run(&dir, Some(&url), Some("sekret"));
    assert!(run.ok, "{}\n{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("verdict: valid"), "{}", run.stdout);
    assert_eq!(ran(&dir), ["fix", "verify", "wrap"]);
    assert_eq!(status_of(&run.stdout, "gate"), "pass");
    assert_eq!(status_of(&run.stdout, "punt"), "not_taken");
    assert_eq!(status_of(&run.stdout, "page"), "not_taken");
    assert!(
        run.stdout.contains("gate.area resolved to scheduler"),
        "{}",
        run.stdout
    );

    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1, "one decision, one call");
    let request = &requests[0];
    assert!(
        request.starts_with("POST /v1/systemone HTTP/1.1"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer sekret"),
        "{request}"
    );
    let body: serde_json::Value =
        serde_json::from_str(request.rsplit('\n').next().unwrap()).unwrap();
    assert_eq!(body["model"], "dgemma");
    assert_eq!(
        body["state"]["scan"]["ticket"],
        "decode stalls when the batch is full"
    );
    assert_eq!(body["questions"]["urgent"]["type"], "noul");
    assert_eq!(
        body["questions"]["area"]["criteria"],
        serde_json::json!({"scheduler": "batching and queueing", "frontend": null})
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn openai_answers(scheduler: f64, urgent: f64) -> String {
    format!(
        r#"{{"answers":[{{"type":"predicate","name":"urgent","probability":{urgent}}},{{"type":"choice","name":"area","choice":"frontend","probabilities":[{{"value":"scheduler","probability":{scheduler}}},{{"value":"frontend","probability":{}}}],"confidence":0.1}}],"model":"gpt-6-luna","usage":{{"input_tokens":90,"input_tokens_details":{{"cache_write_tokens":0,"cached_tokens":0}},"output_tokens":0,"output_tokens_details":{{"reasoning_tokens":0}},"total_tokens":90}}}}"#,
        1.0 - scheduler
    )
}

#[test]
fn an_openai_decisions_binding_routes_the_same_plan_the_same_way() {
    let (url, seen) = serve_at("/v1/decisions", "200 OK", openai_answers(0.93, 0.02));
    let dir = workdir("openai", MODEL_PLAN);
    let document = serde_json::json!({"version": 1, "bindings": [{
        "role": "decision", "protocol": "decisions", "url": url, "model": "gpt-6-luna",
        "key_env": "ROUTE_E2E_KEY",
    }]})
    .to_string();
    let run = run_with(&dir, Some(&document), Some("sk-test"));
    assert!(run.ok, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(ran(&dir), ["fix", "verify", "wrap"]);
    assert_eq!(status_of(&run.stdout, "punt"), "not_taken");
    assert_eq!(status_of(&run.stdout, "page"), "not_taken");
    assert!(
        run.stdout.contains("gate.area resolved to scheduler"),
        "{}",
        run.stdout
    );

    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1, "one decision, one call");
    let request = &requests[0];
    assert!(
        request.starts_with("POST /v1/decisions HTTP/1.1"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer sk-test"),
        "{request}"
    );
    let body: serde_json::Value =
        serde_json::from_str(request.rsplit('\n').next().unwrap()).unwrap();
    assert_eq!(body["model"], "gpt-6-luna");
    let input: serde_json::Value = serde_json::from_str(body["input"].as_str().unwrap()).unwrap();
    assert_eq!(
        input["scan"]["ticket"],
        "decode stalls when the batch is full"
    );
    assert_eq!(
        body["questions"],
        serde_json::json!([
            {"type": "choice", "name": "area", "instructions": "Which component?",
             "choices": [{"value": "scheduler", "description": "batching and queueing"},
                         {"value": "frontend"}]},
            {"type": "predicate", "name": "urgent", "instructions": "Reply within the hour?"}
        ])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

const MAPPED_PLAN: &str = r#"
version = 1
[budget]
usd = 5.0

[[task]]
name = "scan"
kind = "command"
command = "echo '{\"items\": [\"a\", \"b\"], \"facts\": {\"a\": {\"lane\": \"needs_reach\"}, \"b\": {\"lane\": \"review\"}}}'"
emits = ["items", "facts"]

[[task]]
name = "analyze"
kind = "command"
depends_on = ["scan"]
over = { task = "scan", field = "items" }
max_fanout = 4
command = "echo '{}'"

[[task]]
name = "judge"
kind = "route"
needs = "decision"
depends_on = ["scan", "analyze"]
over = { task = "scan", field = "items" }
max_fanout = 4
keyed = [{ task = "scan", field = "facts" }]
decider = { kind = "model", min_confidence = 0.5 }
[task.questions.urgent]
instructions = "Does the evidence support the claim?"
type = "noul"
drop = ["no", "uncertain"]

[[task]]
name = "fix"
kind = "command"
depends_on = ["scan", "judge"]
over = { task = "scan", field = "items" }
max_fanout = 4
when = { task = "judge", question = "urgent", is = ["yes"] }
command = "if [ -n \"$ROUTE_E2E_KEY$CRUCIBLE_INFERENCE\" ]; then echo '{\"saw\": true}'; else echo '{\"saw\": false}'; fi"
"#;

/// A route mapped over a list asks the model once per item, with that item and its keyed entry
/// in its state, and the work mapped over the same list runs per item on its answer. No
/// task process sees the decision binding or its key.
#[test]
fn a_mapped_route_asks_once_per_item_with_that_items_state() {
    let (url, seen) = serve_at(
        "/v1/decisions",
        "200 OK",
        r#"{"answers":[{"type":"predicate","name":"urgent","probability":0.97}]}"#.to_string(),
    );
    let dir = workdir("mapped", MAPPED_PLAN);
    let document = serde_json::json!({"version": 1, "bindings": [{
        "role": "decision", "protocol": "decisions", "url": url, "model": "gpt-6-luna",
        "key_env": "ROUTE_E2E_KEY",
    }]})
    .to_string();
    let run = run_with(&dir, Some(&document), Some("sk-test"));
    assert!(run.ok, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(status_of(&run.stdout, "judge"), "pass");
    assert_eq!(status_of(&run.stdout, "fix"), "pass");
    assert_eq!(
        run.stdout.matches("{\"saw\":false}").count(),
        2,
        "every item ran, and no task process saw the decision binding: {}",
        run.stdout
    );

    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2, "one decision per item");
    let mut states: Vec<serde_json::Value> = requests
        .iter()
        .map(|request| {
            let body: serde_json::Value =
                serde_json::from_str(request.rsplit('\n').next().unwrap()).unwrap();
            serde_json::from_str(body["input"].as_str().unwrap()).unwrap()
        })
        .collect();
    states.sort_by_key(|state| state["item"].as_str().map(str::to_owned));
    for (state, (item, lane)) in states.iter().zip([("a", "needs_reach"), ("b", "review")]) {
        assert_eq!(state["item"], item);
        assert_eq!(state["scan"]["facts"], serde_json::json!({"lane": lane}));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_answer_below_min_confidence_takes_the_uncertain_branch() {
    let (url, _seen) = serve("200 OK", answers(0.55, 0.97));
    let dir = workdir("uncertain", MODEL_PLAN);
    let run = run(&dir, Some(&url), None);
    assert!(run.ok, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(ran(&dir), ["page", "punt", "wrap"]);
    assert_eq!(status_of(&run.stdout, "fix"), "not_taken");
    assert_eq!(status_of(&run.stdout, "verify"), "not_taken");
    assert!(
        run.stdout.contains("\"label\":\"uncertain\""),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("\"confidence\":0.55"), "{}", run.stdout);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_model_route_with_no_endpoint_configured_truncates_before_any_dispatch() {
    let dir = workdir("unconfigured", MODEL_PLAN);
    let run = run(&dir, None, None);
    assert!(!run.ok, "{}", run.stdout);
    assert!(run.stdout.contains("truncated at gate"), "{}", run.stdout);
    assert_eq!(ran(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_binding_whose_named_key_is_unset_fails_the_route_without_calling_the_endpoint() {
    let (url, seen) = serve("200 OK", answers(0.93, 0.02));
    let dir = workdir("keyless", MODEL_PLAN);
    let run = run_with(&dir, Some(&inference(&url, Some("ROUTE_E2E_KEY"))), None);
    assert!(!run.ok, "{}", run.stdout);
    assert_eq!(status_of(&run.stdout, "gate"), "fail");
    assert!(
        run.stdout.contains("\"ROUTE_E2E_KEY\" is unset"),
        "{}",
        run.stdout
    );
    assert_eq!(seen.lock().unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_document_stops_the_run_before_any_task() {
    let dir = workdir("malformed", MODEL_PLAN);
    for (document, needle) in [
        (r#"{"version":9,"bindings":[]}"#, "version 9"),
        (
            r#"{"version":1,"bindings":[{"role":"decision","protocol":"system_one","url":"http://h/x","model":"m","api_key":"sk"}]}"#,
            "api_key",
        ),
        (
            r#"{"version":1,"bindings":[{"role":"decision","protocol":"messages","url":"http://h/x","model":"m"}]}"#,
            "does not serve that role",
        ),
    ] {
        let run = run_with(&dir, Some(document), None);
        assert!(!run.ok, "{document}: {}", run.stdout);
        assert!(run.stderr.contains(needle), "{needle}: {}", run.stderr);
        assert_eq!(ran(&dir), Vec::<String>::new(), "{document}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_endpoint_that_keeps_failing_is_retried_as_transport_and_invalidates_the_run() {
    let (url, seen) = serve("500 Internal Server Error", "{}".to_owned());
    let dir = workdir("transport", MODEL_PLAN);
    let run = run(&dir, Some(&url), None);
    assert!(!run.ok, "{}", run.stdout);
    assert_eq!(status_of(&run.stdout, "gate"), "transport");
    assert_eq!(seen.lock().unwrap().len(), 3, "one attempt and two retries");
    assert_eq!(ran(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_answer_outside_the_declared_labels_fails_the_route_and_is_not_retried() {
    let body = answers(0.9, 0.1).replace("\"frontend\":", "\"kv_cache\":");
    let (url, seen) = serve("200 OK", body);
    let dir = workdir("undeclared", MODEL_PLAN);
    let run = run(&dir, Some(&url), None);
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
fn an_output_decided_route_branches_on_a_commands_answer_with_no_endpoint() {
    let plan = MODEL_PLAN
        .replace(
            "echo '{\\\"ticket\\\": \\\"decode stalls when the batch is full\\\"}'",
            "echo '{\\\"area\\\": \\\"frontend\\\", \\\"urgent\\\": true}'",
        )
        .replace("needs = \"decision\"\n", "")
        .replace(
            "decider = { kind = \"model\", min_confidence = 0.8 }",
            "decider = { kind = \"output\", task = \"scan\" }",
        );
    let dir = workdir("deterministic", &plan);
    let run = run(&dir, None, None);
    assert!(run.ok, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(ran(&dir), ["page", "punt", "wrap"]);
    assert_eq!(status_of(&run.stdout, "fix"), "not_taken");
    assert!(run.stdout.contains("verdict: valid"), "{}", run.stdout);
    let _ = std::fs::remove_dir_all(&dir);
}

const DYNAMIC_PLAN: &str = r#"
version = 1
[budget]
usd = 5.0

[[task]]
name = "scan"
kind = "command"
command = "echo '{\"items\": [\"a\", \"b\", \"c\"], \"packages\": {\"a\": [{\"value\": \"p1\", \"description\": \"golang.org/x/net 0.17.0\"}, {\"value\": \"none\", \"description\": \"No shipped package matches\"}], \"b\": [\"p1\", \"none\", \"p1\"], \"c\": [\"none\"]}}'"
emits = ["items", "packages"]

[[task]]
name = "match"
kind = "route"
needs = "decision"
required = false
depends_on = ["scan"]
over = { task = "scan", field = "items" }
max_fanout = 4
decider = { kind = "model", min_confidence = 0.5 }
[task.questions.package]
instructions = "Which shipped package does the flaw affect?"
type = "dynamic_choice"
options_from = { task = "scan", field = "packages" }

[[task]]
name = "analyze"
kind = "command"
depends_on = ["scan", "match"]
join = "settled"
over = { task = "scan", field = "items" }
max_fanout = 4
when = { task = "match", question = "package", is = ["uncertain"], any_option = true }
command = "echo '{}'"
"#;

/// Each element of a mapped route is asked over its own option list, read from the producer's
/// object keyed by element; a repeated option collapses, the answer records what was offered, and
/// an element whose list is too short fails without a request while the others decide.
#[test]
fn a_dynamic_choice_asks_each_item_over_its_own_options() {
    let (url, seen) = serve_at(
        "/v1/decisions",
        "200 OK",
        r#"{"answers":[{"type":"choice","name":"package","probabilities":[{"value":"p1","probability":0.9},{"value":"none","probability":0.1}]}]}"#.to_string(),
    );
    let dir = workdir("dynamic", DYNAMIC_PLAN);
    let document = serde_json::json!({"version": 1, "bindings": [{
        "role": "decision", "protocol": "decisions", "url": url, "model": "gpt-6-luna",
    }]})
    .to_string();
    let run = run_with(&dir, Some(&document), None);
    assert!(run.ok, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(status_of(&run.stdout, "match"), "fail");
    assert!(
        run.stdout.contains("(1 of 3 instances failed: c)"),
        "only the element whose list is too short fails: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains(
            r#""a":{"package":{"confidence":0.9,"label":"p1","options":[{"description":"golang.org/x/net 0.17.0","label":"p1"},{"description":"No shipped package matches","label":"none"}]"#
        ),
        "the answer records the options it chose among: {}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains(r#""b":{"package":{"confidence":0.9,"label":"p1","options":[{"label":"p1"},{"label":"none"}]"#),
        "a repeated option collapses to its first entry: {}",
        run.stdout
    );
    assert_eq!(status_of(&run.stdout, "analyze"), "pass");
    assert!(
        run.stdout.contains(
            r#"out={"failed":0,"instances":3,"not_taken":1,"outputs":{"a":{},"b":{}},"passed":2}"#
        ),
        "any option runs the analysis, the failed element's is not taken: {}",
        run.stdout
    );

    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2, "no request for the element that failed");
    let mut asked: Vec<(String, serde_json::Value)> = requests
        .iter()
        .map(|request| {
            let body: serde_json::Value =
                serde_json::from_str(request.rsplit('\n').next().unwrap()).unwrap();
            let input: serde_json::Value =
                serde_json::from_str(body["input"].as_str().unwrap()).unwrap();
            let item = input["item"].as_str().unwrap().to_owned();
            (item, body["questions"].clone())
        })
        .collect();
    asked.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(
        asked,
        [
            (
                "a".to_owned(),
                serde_json::json!([{
                    "type": "choice", "name": "package",
                    "instructions": "Which shipped package does the flaw affect?",
                    "choices": [
                        {"value": "p1", "description": "golang.org/x/net 0.17.0"},
                        {"value": "none", "description": "No shipped package matches"},
                    ],
                }])
            ),
            (
                "b".to_owned(),
                serde_json::json!([{
                    "type": "choice", "name": "package",
                    "instructions": "Which shipped package does the flaw affect?",
                    "choices": [{"value": "p1"}, {"value": "none"}],
                }])
            ),
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An unmapped route reads its options as the dependency's list, and System One is sent them as
/// the choice's criteria.
#[test]
fn a_dynamic_choice_on_an_unmapped_route_asks_system_one_over_the_list() {
    let plan = r#"
version = 1
[budget]
usd = 5.0

[[task]]
name = "scan"
kind = "command"
command = "echo '{\"packages\": [{\"value\": \"openssl\", \"description\": \"openssl-libs 3.0.7\"}, \"none\"]}'"
emits = { packages = "list" }

[[task]]
name = "match"
kind = "route"
needs = "decision"
depends_on = ["scan"]
decider = { kind = "model", min_confidence = 0.8 }
[task.questions.package]
instructions = "Which shipped package does the flaw affect?"
type = "dynamic_choice"
options_from = { task = "scan", field = "packages" }

[[task]]
name = "review"
kind = "command"
depends_on = ["match"]
when = { task = "match", question = "package", is = ["uncertain"] }
command = "touch review.ran && echo '{}'"

[[task]]
name = "analyze"
kind = "command"
depends_on = ["match"]
when = { task = "match", question = "package", is = [], any_option = true }
command = "touch analyze.ran && echo '{}'"
"#;
    let (url, seen) = serve(
        "200 OK",
        r#"{"model":"dgemma","answers":{"package":{"type":"choice","probabilities":{"openssl":0.6,"none":0.4}}}}"#.to_string(),
    );
    let dir = workdir("dynamic-unmapped", plan);
    let run = run(&dir, Some(&url), None);
    assert!(run.ok, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(ran(&dir), ["review"], "0.6 is below min_confidence");
    assert_eq!(status_of(&run.stdout, "analyze"), "not_taken");
    assert!(
        run.stdout.contains(r#""label":"uncertain""#),
        "{}",
        run.stdout
    );

    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value =
        serde_json::from_str(requests[0].rsplit('\n').next().unwrap()).unwrap();
    assert_eq!(body["questions"]["package"]["type"], "choice");
    assert_eq!(
        body["questions"]["package"]["criteria"],
        serde_json::json!({"openssl": "openssl-libs 3.0.7", "none": null})
    );
    let _ = std::fs::remove_dir_all(&dir);
}
