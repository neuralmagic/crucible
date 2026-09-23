//! Client for the controller's elicitation endpoint: open a human-decided route's question, then
//! poll it until every question is answered, its deadline passes, or the run's ceiling arrives.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crucible_contract::decision::{Label, QuestionId};
use crucible_contract::elicit::{
    ENV_ELICIT_TOKEN_PATH, ENV_ELICIT_URL, ElicitRequest, ElicitStatus,
};

const TIMEOUT: Duration = Duration::from_secs(30);

/// The least time one request gets, however close the run's ceiling is.
const MIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub token_path: Option<PathBuf>,
}

impl Endpoint {
    /// The endpoint the controller configured, or `None` when it configured none. `lookup` reads
    /// an environment variable by name.
    pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let present = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        Some(Endpoint {
            url: present(ENV_ELICIT_URL)?.trim_end_matches('/').to_owned(),
            token_path: present(ENV_ELICIT_TOKEN_PATH).map(PathBuf::from),
        })
    }

    fn question_url(&self, task: &str) -> String {
        format!("{}/{}", self.url, path_segment(task))
    }

    fn token(&self) -> Result<Option<String>, ElicitError> {
        let Some(path) = &self.token_path else {
            return Ok(None);
        };
        let token = std::fs::read_to_string(path).map_err(|e| {
            ElicitError::Invalid(format!(
                "reading the elicitation token {}: {e}",
                path.display()
            ))
        })?;
        Ok(Some(token.trim().to_owned()))
    }
}

fn path_segment(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitError {
    /// The endpoint could not be reached or is temporarily failing; worth a retry.
    Transport(String),
    /// The request, the configuration, or the answer is wrong; a retry would repeat it.
    Invalid(String),
}

impl std::fmt::Display for ElicitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ElicitError::Transport(m) | ElicitError::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ElicitError {}

/// How a wait ended, with the checked answers read by then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waited {
    /// Every question has an answer.
    Answered(BTreeMap<QuestionId, Label>),
    /// The question's deadline passed first.
    Expired(BTreeMap<QuestionId, Label>),
    /// The run's wall-clock ceiling arrived before the deadline.
    Cut(BTreeMap<QuestionId, Label>),
}

/// The bounds on one wait besides the question's own deadline.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub poll: Duration,
    pub ceiling: Option<Instant>,
}

enum Method {
    Put,
    Get,
}

fn exchange(
    client: &reqwest::blocking::Client,
    endpoint: &Endpoint,
    task: &str,
    method: Method,
    request: &ElicitRequest,
    ceiling: Option<Instant>,
) -> Result<ElicitStatus, ElicitError> {
    let url = endpoint.question_url(task);
    let timeout = ceiling.map_or(TIMEOUT, |c| {
        c.saturating_duration_since(Instant::now())
            .clamp(MIN_TIMEOUT, TIMEOUT)
    });
    let mut call = match method {
        Method::Put => client.put(&url).json(request),
        Method::Get => client.get(&url),
    }
    .timeout(timeout);
    if let Some(token) = endpoint.token()? {
        call = call.bearer_auth(token);
    }
    let response = call
        .send()
        .map_err(|e| ElicitError::Transport(format!("reaching the elicitation endpoint: {e}")))?;
    let status = response.status();
    let body = response.text().map_err(|e| {
        ElicitError::Transport(format!("reading the elicitation endpoint's answer: {e}"))
    })?;
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(ElicitError::Transport(format!(
            "the elicitation endpoint answered {status}: {}",
            truncate(&body)
        )));
    }
    if !status.is_success() {
        return Err(ElicitError::Invalid(format!(
            "the elicitation endpoint refused the question with {status}: {}",
            truncate(&body)
        )));
    }
    let decoded: ElicitStatus = serde_json::from_str(&body).map_err(|e| {
        ElicitError::Invalid(format!("decoding the elicitation endpoint's answer: {e}"))
    })?;
    decoded
        .check(&request.questions)
        .map_err(|e| ElicitError::Invalid(e.to_string()))?;
    Ok(decoded)
}

/// Open `task`'s question and wait on it. Opening a question that is already open returns its
/// stored answers and the time left, so a retry or a resumed run picks up where it was.
pub fn wait(
    endpoint: &Endpoint,
    task: &str,
    request: &ElicitRequest,
    bounds: Bounds,
) -> Result<Waited, ElicitError> {
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| ElicitError::Invalid(format!("building the elicitation client: {e}")))?;
    let mut status = exchange(
        &client,
        endpoint,
        task,
        Method::Put,
        request,
        bounds.ceiling,
    )?;
    let deadline_after = |status: &ElicitStatus| {
        let left = Duration::from_secs(status.expires_in_secs.min(request.deadline_secs));
        let now = Instant::now();
        now.checked_add(left).unwrap_or(now)
    };
    let mut deadline = deadline_after(&status);
    let ceiling_first = |deadline: Instant| bounds.ceiling.is_some_and(|c| c < deadline);
    loop {
        if status.complete(&request.questions) {
            return Ok(Waited::Answered(status.answers));
        }
        let now = Instant::now();
        if bounds.ceiling.is_some_and(|c| c <= now) && ceiling_first(deadline) {
            return Ok(Waited::Cut(status.answers));
        }
        let bound = bounds.ceiling.map_or(deadline, |c| c.min(deadline));
        std::thread::sleep(bound.saturating_duration_since(now).min(bounds.poll));
        match exchange(
            &client,
            endpoint,
            task,
            Method::Get,
            request,
            bounds.ceiling,
        ) {
            Ok(fresh) => {
                deadline = deadline_after(&fresh);
                status = fresh;
                if status.complete(&request.questions) {
                    return Ok(Waited::Answered(status.answers));
                }
                let now = Instant::now();
                if deadline <= now || bounds.ceiling.is_some_and(|c| c <= now) {
                    return Ok(if ceiling_first(deadline) {
                        Waited::Cut(status.answers)
                    } else {
                        Waited::Expired(status.answers)
                    });
                }
            }
            Err(ElicitError::Transport(note)) => {
                let now = Instant::now();
                if bounds.ceiling.is_some_and(|c| c <= now) && ceiling_first(deadline) {
                    return Ok(Waited::Cut(status.answers));
                }
                if deadline <= now {
                    return Err(ElicitError::Transport(note));
                }
                tracing::warn!(task, reason = %note, "elicitation poll failed; polling again");
            }
            Err(invalid) => return Err(invalid),
        }
    }
}

fn truncate(body: &str) -> &str {
    let end = body.char_indices().nth(500).map_or(body.len(), |(i, _)| i);
    &body[..end]
}

#[cfg(test)]
mod tests {
    use crate::elicit::*;
    use crucible_contract::decision::{ChoiceOption, Question, QuestionKind};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn qid(s: &str) -> QuestionId {
        QuestionId::new(s).unwrap()
    }

    fn label(s: &str) -> Label {
        Label::new(s).unwrap()
    }

    fn request() -> ElicitRequest {
        ElicitRequest {
            questions: BTreeMap::from([
                (
                    qid("ship"),
                    Question {
                        instructions: "Ship it?".into(),
                        kind: QuestionKind::Noul,
                        drop: vec![],
                    },
                ),
                (
                    qid("owner"),
                    Question {
                        instructions: "Who owns it?".into(),
                        kind: QuestionKind::Choice {
                            options: vec![
                                ChoiceOption {
                                    label: label("serving"),
                                    description: None,
                                },
                                ChoiceOption {
                                    label: label("kernels"),
                                    description: None,
                                },
                            ],
                        },
                        drop: vec![],
                    },
                ),
            ]),
            deadline_secs: 3600,
        }
    }

    /// A real socket that answers each request with the next scripted response, repeating the
    /// last one, and records every request it received.
    fn serve(script: Vec<(&'static str, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/api/pods/p-1/elicitations",
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

    fn ok(expires_in_secs: u64, answers: &[(&str, &str)]) -> (&'static str, String) {
        let answers: serde_json::Map<String, serde_json::Value> = answers
            .iter()
            .map(|(q, l)| ((*q).to_owned(), serde_json::Value::from(*l)))
            .collect();
        (
            "200 OK",
            serde_json::json!({"expires_in_secs": expires_in_secs, "answers": answers}).to_string(),
        )
    }

    fn endpoint(url: String) -> Endpoint {
        Endpoint {
            url,
            token_path: None,
        }
    }

    fn fast(ceiling: Option<Duration>) -> Bounds {
        Bounds {
            poll: Duration::from_millis(10),
            ceiling: ceiling.map(|d| Instant::now() + d),
        }
    }

    fn answers(pairs: &[(&str, &str)]) -> BTreeMap<QuestionId, Label> {
        pairs.iter().map(|(q, l)| (qid(q), label(l))).collect()
    }

    #[test]
    fn the_endpoint_comes_from_the_environment_or_not_at_all() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(Endpoint::from_env(env(&[])), None);
        assert_eq!(Endpoint::from_env(env(&[(ENV_ELICIT_URL, "  ")])), None);
        assert_eq!(
            Endpoint::from_env(env(&[
                (ENV_ELICIT_URL, "http://c:8080/api/pods/p/elicitations/"),
                (ENV_ELICIT_TOKEN_PATH, "/var/run/token"),
            ])),
            Some(Endpoint {
                url: "http://c:8080/api/pods/p/elicitations".into(),
                token_path: Some(PathBuf::from("/var/run/token")),
            })
        );
    }

    #[test]
    fn a_task_name_is_one_escaped_path_segment() {
        assert_eq!(path_segment("gate-1.a_b~"), "gate-1.a_b~");
        assert_eq!(path_segment("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn opening_an_answered_question_returns_without_polling() {
        let (url, seen) = serve(vec![ok(3600, &[("ship", "yes"), ("owner", "kernels")])]);
        let dir = std::env::temp_dir().join(format!("elicit-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token = dir.join("token");
        std::fs::write(&token, "sekret\n").unwrap();
        let got = wait(
            &Endpoint {
                url,
                token_path: Some(token),
            },
            "gate",
            &request(),
            fast(None),
        )
        .unwrap();
        assert_eq!(
            got,
            Waited::Answered(answers(&[("ship", "yes"), ("owner", "kernels")]))
        );
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with("PUT /api/pods/p-1/elicitations/gate HTTP/1.1"),
            "{}",
            requests[0]
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer sekret\r\n"),
            "{}",
            requests[0]
        );
        let sent: ElicitRequest =
            serde_json::from_str(requests[0].rsplit('\n').next().unwrap()).unwrap();
        assert_eq!(sent, request());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_question_answered_later_is_read_by_polling() {
        let (url, seen) = serve(vec![
            ok(3600, &[]),
            ok(3600, &[("ship", "no")]),
            ok(3600, &[("ship", "no"), ("owner", "serving")]),
        ]);
        let got = wait(&endpoint(url), "gate", &request(), fast(None)).unwrap();
        assert_eq!(
            got,
            Waited::Answered(answers(&[("ship", "no"), ("owner", "serving")]))
        );
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("GET /api/pods/p-1/elicitations/gate"));
        assert!(!requests[1].to_ascii_lowercase().contains("authorization:"));
    }

    #[test]
    fn a_deadline_that_passes_keeps_what_was_answered() {
        let (url, seen) = serve(vec![ok(0, &[("ship", "yes")])]);
        let got = wait(&endpoint(url), "gate", &request(), fast(None)).unwrap();
        assert_eq!(got, Waited::Expired(answers(&[("ship", "yes")])));
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "one last read at the deadline"
        );
    }

    #[test]
    fn the_run_ceiling_before_the_deadline_cuts_the_wait() {
        let (url, _seen) = serve(vec![ok(3600, &[("owner", "kernels")])]);
        let started = Instant::now();
        let got = wait(
            &endpoint(url),
            "gate",
            &request(),
            fast(Some(Duration::from_millis(60))),
        )
        .unwrap();
        assert_eq!(got, Waited::Cut(answers(&[("owner", "kernels")])));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_poll_that_hangs_at_the_ceiling_is_cut_at_the_ceiling() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/e", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for (served, stream) in listener.incoming().enumerate() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
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
                }
                let mut payload = vec![0u8; length];
                reader.read_exact(&mut payload).unwrap();
                if served == 0 {
                    let (_, body) = ok(3600, &[]);
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                } else {
                    held.push(stream);
                }
            }
        });
        let started = Instant::now();
        let got = wait(
            &endpoint(url),
            "gate",
            &request(),
            Bounds {
                poll: Duration::from_millis(10),
                ceiling: Some(started + Duration::from_millis(500)),
            },
        )
        .unwrap();
        assert_eq!(got, Waited::Cut(BTreeMap::new()));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a hung poll held the wait {:?} past a 500ms ceiling",
            started.elapsed()
        );
    }

    #[test]
    fn a_deadline_before_the_ceiling_is_an_expiry_not_a_cut() {
        let (url, _seen) = serve(vec![ok(0, &[])]);
        let got = wait(
            &endpoint(url),
            "gate",
            &request(),
            fast(Some(Duration::from_secs(3600))),
        )
        .unwrap();
        assert_eq!(got, Waited::Expired(BTreeMap::new()));
    }

    #[test]
    fn the_endpoint_cannot_grant_more_time_than_the_route_asked_for() {
        let (url, _seen) = serve(vec![ok(u64::MAX, &[])]);
        let mut short = request();
        short.deadline_secs = 0;
        let got = wait(&endpoint(url), "gate", &short, fast(None)).unwrap();
        assert_eq!(got, Waited::Expired(BTreeMap::new()));
    }

    #[test]
    fn a_ceiling_already_reached_cuts_before_any_poll() {
        let (url, seen) = serve(vec![ok(3600, &[])]);
        let got = wait(
            &endpoint(url),
            "gate",
            &request(),
            Bounds {
                poll: Duration::from_millis(10),
                ceiling: Some(Instant::now()),
            },
        )
        .unwrap();
        assert_eq!(got, Waited::Cut(BTreeMap::new()));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failing_poll_is_retried_until_the_answer_arrives() {
        let (url, seen) = serve(vec![
            ok(3600, &[]),
            ("503 Service Unavailable", "{}".into()),
            ok(3600, &[("ship", "yes"), ("owner", "kernels")]),
        ]);
        let got = wait(&endpoint(url), "gate", &request(), fast(None)).unwrap();
        assert!(matches!(got, Waited::Answered(_)));
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_poll_that_fails_at_the_deadline_is_transport_not_an_expiry() {
        let (url, _seen) = serve(vec![ok(0, &[]), ("502 Bad Gateway", "{}".into())]);
        match wait(&endpoint(url), "gate", &request(), fast(None)) {
            Err(ElicitError::Transport(m)) => assert!(m.contains("502"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_answer_the_question_does_not_declare_is_invalid() {
        for (answer, needle) in [
            (("owner", "frontend"), "undeclared label \"frontend\""),
            (("extra", "yes"), "undeclared question \"extra\""),
            (("ship", "uncertain"), "only the engine records"),
        ] {
            let (url, _seen) = serve(vec![ok(3600, &[answer])]);
            match wait(&endpoint(url), "gate", &request(), fast(None)) {
                Err(ElicitError::Invalid(m)) => assert!(m.contains(needle), "{m} lacks {needle}"),
                other => panic!("{answer:?} gave {other:?}"),
            }
        }
    }

    #[test]
    fn an_invalid_answer_read_by_a_poll_is_invalid_too() {
        let (url, _seen) = serve(vec![ok(3600, &[]), ok(3600, &[("ship", "maybe")])]);
        assert!(matches!(
            wait(&endpoint(url), "gate", &request(), fast(None)),
            Err(ElicitError::Invalid(_))
        ));
    }

    #[test]
    fn a_refused_open_is_invalid_and_a_failing_one_is_transport() {
        let (url, _seen) = serve(vec![(
            "409 Conflict",
            r#"{"error":"gate was opened with other questions"}"#.into(),
        )]);
        match wait(&endpoint(url), "gate", &request(), fast(None)) {
            Err(ElicitError::Invalid(m)) => assert!(m.contains("other questions"), "{m}"),
            other => panic!("{other:?}"),
        }
        for status in ["500 Internal Server Error", "429 Too Many Requests"] {
            let (url, _seen) = serve(vec![(status, "{}".into())]);
            assert!(matches!(
                wait(&endpoint(url), "gate", &request(), fast(None)),
                Err(ElicitError::Transport(_))
            ));
        }
        let (url, _seen) = serve(vec![("200 OK", "not json".into())]);
        assert!(matches!(
            wait(&endpoint(url), "gate", &request(), fast(None)),
            Err(ElicitError::Invalid(_))
        ));
    }

    #[test]
    fn an_unreachable_endpoint_is_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/e", listener.local_addr().unwrap());
        drop(listener);
        assert!(matches!(
            wait(&endpoint(url), "gate", &request(), fast(None)),
            Err(ElicitError::Transport(_))
        ));
    }

    #[test]
    fn a_named_token_file_that_is_missing_is_invalid_before_any_request() {
        let (url, seen) = serve(vec![ok(3600, &[])]);
        let got = wait(
            &Endpoint {
                url,
                token_path: Some(PathBuf::from("/nonexistent/elicit/token")),
            },
            "gate",
            &request(),
            fast(None),
        );
        assert!(matches!(got, Err(ElicitError::Invalid(m)) if m.contains("elicitation token")));
        assert_eq!(seen.lock().unwrap().len(), 0);
    }
}
