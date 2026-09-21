//! Client for the System One decision API (`POST /v1/systemone`).

use std::collections::BTreeMap;
use std::time::Duration;

use crucible_contract::decision::{
    Answer, Decision, Label, NOUL_NO, NOUL_YES, Question, QuestionId, QuestionKind,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub const ENV_URL: &str = "CRUCIBLE_SYSTEMONE_URL";
pub const ENV_API_KEY: &str = "CRUCIBLE_SYSTEMONE_API_KEY";
pub const ENV_MODEL: &str = "CRUCIBLE_SYSTEMONE_MODEL";

const DEFAULT_MODEL: &str = "jev-latest";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub api_key: Option<String>,
    pub model: String,
}

impl Endpoint {
    pub fn from_env() -> Result<Self, DecideError> {
        let url = std::env::var(ENV_URL)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| DecideError::Invalid(format!("{ENV_URL} is unset")))?;
        Ok(Endpoint {
            url,
            api_key: std::env::var(ENV_API_KEY).ok().filter(|v| !v.is_empty()),
            model: std::env::var(ENV_MODEL)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecideError {
    /// The endpoint could not be reached or is temporarily failing; worth a retry.
    Transport(String),
    /// The request or the response is wrong; a retry would repeat it.
    Invalid(String),
}

impl std::fmt::Display for DecideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecideError::Transport(m) | DecideError::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for DecideError {}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireAnswer {
    Noul {
        noul: serde_json::Number,
    },
    Choice {
        probabilities: BTreeMap<String, serde_json::Number>,
    },
}

#[derive(Deserialize)]
struct WireResponse {
    answers: BTreeMap<String, Option<WireAnswer>>,
}

pub fn request_body(
    model: &str,
    questions: &BTreeMap<QuestionId, Question>,
    state: &Value,
) -> Value {
    let questions: serde_json::Map<String, Value> = questions
        .iter()
        .map(|(id, q)| {
            let mut body = json!({ "instructions": q.instructions });
            match &q.kind {
                QuestionKind::Noul => body["type"] = json!("noul"),
                QuestionKind::Choice { options } => {
                    body["type"] = json!("choice");
                    body["criteria"] = Value::Object(
                        options
                            .iter()
                            .map(|o| (o.label.as_str().to_owned(), json!(o.description)))
                            .collect(),
                    );
                }
            }
            (id.as_str().to_owned(), body)
        })
        .collect();
    json!({ "model": model, "state": state, "questions": questions })
}

pub fn parse_response(
    body: &str,
    questions: &BTreeMap<QuestionId, Question>,
    min_confidence: f64,
) -> Result<Decision, DecideError> {
    let invalid = |m: String| DecideError::Invalid(m);
    let mut wire: WireResponse = serde_json::from_str(body)
        .map_err(|e| invalid(format!("decoding System One response: {e}")))?;
    let mut decision = BTreeMap::new();
    for (id, question) in questions {
        let answer = wire
            .answers
            .remove(id.as_str())
            .ok_or_else(|| invalid(format!("the response omits question {id:?}")))?
            .ok_or_else(|| invalid(format!("the model left question {id:?} unanswered")))?;
        let probabilities = probabilities(id, question, answer)?;
        let answer: Answer = question
            .resolve(probabilities, min_confidence)
            .map_err(|e| invalid(format!("question {id:?}: {e}")))?;
        decision.insert(id.clone(), answer);
    }
    if let Some(extra) = wire.answers.keys().next() {
        return Err(invalid(format!(
            "the response answers undeclared question {extra:?}"
        )));
    }
    Ok(Decision(decision))
}

fn probabilities(
    id: &QuestionId,
    question: &Question,
    answer: WireAnswer,
) -> Result<BTreeMap<Label, f64>, DecideError> {
    let invalid = |m: String| DecideError::Invalid(m);
    let float = |n: serde_json::Number| {
        n.as_f64()
            .ok_or_else(|| invalid(format!("question {id:?}: {n} is not a probability")))
    };
    match (&question.kind, answer) {
        (QuestionKind::Noul, WireAnswer::Noul { noul }) => {
            let noul = float(noul)?;
            let label =
                |s: &str| Label::new(s).map_err(|e| invalid(format!("question {id:?}: {e}")));
            Ok(BTreeMap::from([
                (label(NOUL_YES)?, noul),
                (label(NOUL_NO)?, 1.0 - noul),
            ]))
        }
        (QuestionKind::Choice { .. }, WireAnswer::Choice { probabilities }) => probabilities
            .into_iter()
            .map(|(name, p)| {
                let label =
                    Label::new(name).map_err(|e| invalid(format!("question {id:?}: {e}")))?;
                Ok((label, float(p)?))
            })
            .collect(),
        (QuestionKind::Noul, WireAnswer::Choice { .. }) => Err(invalid(format!(
            "question {id:?} is a noul but was answered as a choice"
        ))),
        (QuestionKind::Choice { .. }, WireAnswer::Noul { .. }) => Err(invalid(format!(
            "question {id:?} is a choice but was answered as a noul"
        ))),
    }
}

pub fn decide(
    endpoint: &Endpoint,
    questions: &BTreeMap<QuestionId, Question>,
    state: &Value,
    min_confidence: f64,
) -> Result<Decision, DecideError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| DecideError::Invalid(format!("building System One client: {e}")))?;
    let mut request =
        client
            .post(&endpoint.url)
            .json(&request_body(&endpoint.model, questions, state));
    if let Some(key) = &endpoint.api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .map_err(|e| DecideError::Transport(format!("reaching System One: {e}")))?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|e| DecideError::Transport(format!("reading System One response: {e}")))?;
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(DecideError::Transport(format!(
            "System One answered {status}: {}",
            truncate(&body)
        )));
    }
    if !status.is_success() {
        return Err(DecideError::Invalid(format!(
            "System One rejected the request with {status}: {}",
            truncate(&body)
        )));
    }
    parse_response(&body, questions, min_confidence)
}

fn truncate(body: &str) -> &str {
    let end = body.char_indices().nth(500).map_or(body.len(), |(i, _)| i);
    &body[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_contract::decision::ChoiceOption;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn qid(s: &str) -> QuestionId {
        QuestionId::new(s).unwrap()
    }

    fn label(s: &str) -> Label {
        Label::new(s).unwrap()
    }

    fn questions() -> BTreeMap<QuestionId, Question> {
        BTreeMap::from([
            (
                qid("urgent"),
                Question {
                    instructions: "Needs a reply within the hour?".into(),
                    kind: QuestionKind::Noul,
                    drop: vec![],
                },
            ),
            (
                qid("area"),
                Question {
                    instructions: "Which component?".into(),
                    kind: QuestionKind::Choice {
                        options: vec![
                            ChoiceOption {
                                label: label("scheduler"),
                                description: Some("batching and queueing".into()),
                            },
                            ChoiceOption {
                                label: label("frontend"),
                                description: None,
                            },
                        ],
                    },
                    drop: vec![],
                },
            ),
        ])
    }

    const GOOD: &str = r#"{"model":"dgemma","answers":{
        "urgent":{"type":"noul","noul":0.92},
        "area":{"type":"choice","choice":"scheduler","probabilities":{"scheduler":0.6,"frontend":0.4},"confidence":0.6}
    },"usage":{"input_tokens":120}}"#;

    /// Serve one HTTP response on a real socket and hand back the request it received.
    fn serve_once(status: &'static str, body: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
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
            tx.send(format!("{head}\n{}", String::from_utf8(payload).unwrap()))
                .unwrap();
            write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        (url, rx)
    }

    fn endpoint(url: String, api_key: Option<&str>) -> Endpoint {
        Endpoint {
            url,
            api_key: api_key.map(str::to_owned),
            model: "dgemma".into(),
        }
    }

    #[test]
    fn the_request_carries_state_and_each_question_in_the_api_shape() {
        let body = request_body("dgemma", &questions(), &json!({"ticket": "down"}));
        assert_eq!(body["model"], "dgemma");
        assert_eq!(body["state"]["ticket"], "down");
        assert_eq!(body["questions"]["urgent"]["type"], "noul");
        assert!(body["questions"]["urgent"].get("criteria").is_none());
        assert_eq!(body["questions"]["area"]["type"], "choice");
        assert_eq!(
            body["questions"]["area"]["criteria"],
            json!({"scheduler": "batching and queueing", "frontend": null})
        );
    }

    #[test]
    fn a_noul_probability_becomes_a_yes_no_distribution() {
        let d = parse_response(GOOD, &questions(), 0.5).unwrap();
        let urgent = &d.0[&qid("urgent")];
        assert_eq!(urgent.label, label("yes"));
        assert_eq!(urgent.probabilities[&label("yes")], 0.92);
        assert!((urgent.probabilities[&label("no")] - 0.08).abs() < 1e-9);
    }

    #[test]
    fn a_low_noul_probability_is_a_confident_no() {
        let body = GOOD.replace("0.92", "0.03");
        let d = parse_response(&body, &questions(), 0.9).unwrap();
        assert_eq!(d.0[&qid("urgent")].label, label("no"));
    }

    #[test]
    fn an_answer_below_the_threshold_is_uncertain() {
        let d = parse_response(GOOD, &questions(), 0.8).unwrap();
        assert_eq!(d.0[&qid("urgent")].label, label("yes"));
        assert!(d.0[&qid("area")].label.is_uncertain());
        assert_eq!(d.0[&qid("area")].confidence, 0.6);
    }

    #[test]
    fn the_engine_derives_the_label_and_ignores_the_models_own_pick() {
        let body = GOOD.replace(r#""choice":"scheduler""#, r#""choice":"frontend""#);
        let d = parse_response(&body, &questions(), 0.5).unwrap();
        assert_eq!(d.0[&qid("area")].label, label("scheduler"));
    }

    #[test]
    fn malformed_responses_are_invalid() {
        let cases = [
            ("not json", "decoding"),
            (
                r#"{"answers":{"urgent":{"type":"noul","noul":0.9}}}"#,
                "omits",
            ),
            (
                r#"{"answers":{"urgent":null,"area":{"type":"choice","probabilities":{"scheduler":0.6,"frontend":0.4}}}}"#,
                "unanswered",
            ),
            (
                r#"{"answers":{"urgent":{"type":"noul","noul":1.4},"area":{"type":"choice","probabilities":{"scheduler":0.6,"frontend":0.4}}}}"#,
                "outside [0, 1]",
            ),
            (
                r#"{"answers":{"urgent":{"type":"noul","noul":0.9},"area":{"type":"choice","probabilities":{"scheduler":0.6,"kv_cache":0.4}}}}"#,
                "undeclared label",
            ),
            (
                r#"{"answers":{"urgent":{"type":"noul","noul":0.9},"area":{"type":"choice","probabilities":{"scheduler":0.6}}}}"#,
                "no probability",
            ),
            (
                r#"{"answers":{"urgent":{"type":"choice","probabilities":{"yes":1.0,"no":0.0}},"area":{"type":"choice","probabilities":{"scheduler":0.6,"frontend":0.4}}}}"#,
                "is a noul",
            ),
            (
                r#"{"answers":{"urgent":{"type":"noul","noul":0.9},"area":{"type":"choice","probabilities":{"scheduler":0.6,"frontend":0.4}},"extra":{"type":"noul","noul":0.5}}}"#,
                "undeclared question",
            ),
        ];
        for (body, needle) in cases {
            match parse_response(body, &questions(), 0.5) {
                Err(DecideError::Invalid(m)) => assert!(m.contains(needle), "{m} lacks {needle}"),
                other => panic!("{body} gave {other:?}"),
            }
        }
    }

    #[test]
    fn decide_posts_the_request_with_bearer_auth_and_returns_the_decision() {
        let (url, received) = serve_once("200 OK", GOOD);
        let d = decide(
            &endpoint(url, Some("sekret")),
            &questions(),
            &json!({"ticket": "down"}),
            0.5,
        )
        .unwrap();
        assert_eq!(d.0[&qid("area")].label, label("scheduler"));
        let request = received.recv().unwrap();
        assert!(request.starts_with("POST /v1/systemone HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer sekret")
        );
        let sent: Value = serde_json::from_str(request.rsplit('\n').next().unwrap()).unwrap();
        assert_eq!(
            sent,
            request_body("dgemma", &questions(), &json!({"ticket": "down"}))
        );
    }

    #[test]
    fn decide_sends_no_authorization_header_without_a_key() {
        let (url, received) = serve_once("200 OK", GOOD);
        decide(&endpoint(url, None), &questions(), &json!({}), 0.5).unwrap();
        assert!(
            !received
                .recv()
                .unwrap()
                .to_ascii_lowercase()
                .contains("authorization:")
        );
    }

    #[test]
    fn a_validation_rejection_is_invalid_and_quotes_the_server() {
        let (url, _rx) = serve_once(
            "422 Unprocessable Entity",
            r#"{"error":{"message":"label 'kv_cache' is 2 tokens","type":"validation_error"}}"#,
        );
        match decide(&endpoint(url, None), &questions(), &json!({}), 0.5) {
            Err(DecideError::Invalid(m)) => assert!(m.contains("2 tokens"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_server_error_and_a_rate_limit_are_transport() {
        for status in ["500 Internal Server Error", "429 Too Many Requests"] {
            let (url, _rx) = serve_once(status, "{}");
            assert!(matches!(
                decide(&endpoint(url, None), &questions(), &json!({}), 0.5),
                Err(DecideError::Transport(_))
            ));
        }
    }

    #[test]
    fn an_unreachable_endpoint_is_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        drop(listener);
        assert!(matches!(
            decide(&endpoint(url, None), &questions(), &json!({}), 0.5),
            Err(DecideError::Transport(_))
        ));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let body = "é".repeat(600);
        assert_eq!(truncate(&body).chars().count(), 500);
        assert_eq!(truncate("short"), "short");
    }
}
