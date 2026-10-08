//! Client for the decision APIs a route task asks: System One (`POST /v1/systemone`) and OpenAI
//! Decisions (`POST /v1/decisions`).
//!
//! Both answer the same typed questions with probabilities and differ only on the wire, so each
//! API is a codec into [`Reply`] over one transport and one validation path:
//!
//! ```text
//!   questions, state ──► DecisionApi::request_body ──► POST ──► DecisionApi::answers
//!                                                                     │ id -> Reply
//!                                    Decision ◄── Question::resolve ◄─┘
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use crucible_contract::decision::{
    Answer, AskedAs, Decision, Label, NOUL_NO, NOUL_YES, Question, QuestionId, QuestionKind,
};
use crucible_contract::inference::{InferenceBinding, InferenceProtocol};
use serde::Deserialize;
use serde_json::{Number, Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionApi {
    SystemOne,
    OpenAi,
}

impl DecisionApi {
    pub fn name(self) -> &'static str {
        match self {
            DecisionApi::SystemOne => "System One",
            DecisionApi::OpenAi => "OpenAI Decisions",
        }
    }

    pub fn request_body(
        self,
        model: &str,
        questions: &BTreeMap<QuestionId, Question>,
        state: &Value,
    ) -> Value {
        match self {
            DecisionApi::SystemOne => system_one_body(model, questions, state),
            DecisionApi::OpenAi => openai_body(model, questions, state),
        }
    }

    /// The form this API is asked a score question in: OpenAI's own score question, or, where the
    /// API has no ordered question, a choice over the levels. `None` for any other question.
    pub fn asked_as(self, question: &Question) -> Option<AskedAs> {
        match (&question.kind, self) {
            (QuestionKind::Score { .. }, DecisionApi::OpenAi) => Some(AskedAs::Score),
            (QuestionKind::Score { .. }, DecisionApi::SystemOne) => Some(AskedAs::Choice),
            (QuestionKind::Noul | QuestionKind::Choice { .. } | QuestionKind::Pick { .. }, _) => {
                None
            }
        }
    }

    fn answers(self, body: &str) -> Result<BTreeMap<String, Reply>, DecideError> {
        match self {
            DecisionApi::SystemOne => system_one_answers(body),
            DecisionApi::OpenAi => openai_answers(body),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub api: DecisionApi,
    pub url: String,
    pub api_key: Option<String>,
    pub model: String,
}

impl Endpoint {
    /// `lookup` reads an environment variable by name.
    pub fn from_binding(
        binding: &InferenceBinding,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, DecideError> {
        let api = match binding.protocol {
            InferenceProtocol::SystemOne => DecisionApi::SystemOne,
            InferenceProtocol::Decisions => DecisionApi::OpenAi,
            other @ (InferenceProtocol::Messages
            | InferenceProtocol::ChatCompletions
            | InferenceProtocol::Responses) => {
                return Err(DecideError::Invalid(format!(
                    "the decision binding speaks {other}, which is not a decision API"
                )));
            }
        };
        let api_key = match &binding.key_env {
            None => None,
            Some(name) => Some(
                lookup(name.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        DecideError::Invalid(format!(
                            "the decision binding's credential variable {name:?} is unset"
                        ))
                    })?,
            ),
        };
        let url = binding
            .url
            .clone()
            .or_else(|| binding.protocol.decision_url().map(str::to_owned))
            .ok_or_else(|| DecideError::Invalid("the decision binding names no url".to_owned()))?;
        Ok(Endpoint {
            api,
            url,
            api_key,
            model: binding.model.clone(),
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

/// What an API said about one question, before it is checked against the question.
enum Reply {
    Answered(WireAnswer),
    /// The API declined to answer. It is recorded as `uncertain`, which every route already has to
    /// route somewhere.
    Refused,
}

enum WireAnswer {
    Noul(Number),
    Choice(BTreeMap<String, Number>),
    Score(BTreeMap<String, Number>),
    Unanswered,
}

fn system_one_body(
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
                QuestionKind::Pick { .. } => body["type"] = json!("pick"),
                QuestionKind::Choice { options, .. } | QuestionKind::Score { levels: options } => {
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

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SystemOneAnswer {
    Noul {
        noul: Number,
    },
    Choice {
        probabilities: BTreeMap<String, Number>,
    },
}

#[derive(Deserialize)]
struct SystemOneResponse {
    answers: BTreeMap<String, Option<SystemOneAnswer>>,
}

fn system_one_answers(body: &str) -> Result<BTreeMap<String, Reply>, DecideError> {
    let wire: SystemOneResponse = serde_json::from_str(body)
        .map_err(|e| DecideError::Invalid(format!("decoding System One response: {e}")))?;
    Ok(wire
        .answers
        .into_iter()
        .map(|(id, answer)| {
            let answer = match answer {
                None => WireAnswer::Unanswered,
                Some(SystemOneAnswer::Noul { noul }) => WireAnswer::Noul(noul),
                Some(SystemOneAnswer::Choice { probabilities }) => {
                    WireAnswer::Choice(probabilities)
                }
            };
            (id, Reply::Answered(answer))
        })
        .collect())
}

fn openai_body(model: &str, questions: &BTreeMap<QuestionId, Question>, state: &Value) -> Value {
    let questions: Vec<Value> = questions
        .iter()
        .map(|(id, q)| match &q.kind {
            QuestionKind::Noul => json!({
                "type": "predicate",
                "name": id.as_str(),
                "instructions": q.instructions,
            }),
            QuestionKind::Pick { .. } => json!({
                "type": "pick",
                "name": id.as_str(),
                "instructions": q.instructions,
            }),
            QuestionKind::Choice { options, .. } => {
                let choices: Vec<Value> = options
                    .iter()
                    .map(|o| {
                        let mut choice = json!({ "value": o.label.as_str() });
                        if let Some(description) = &o.description {
                            choice["description"] = json!(description);
                        }
                        choice
                    })
                    .collect();
                json!({
                    "type": "choice",
                    "name": id.as_str(),
                    "instructions": q.instructions,
                    "choices": choices,
                })
            }
            QuestionKind::Score { levels } => {
                let levels: Vec<Value> = levels
                    .iter()
                    .map(|l| {
                        let mut level = json!({ "label": l.label.as_str() });
                        if let Some(description) = &l.description {
                            level["description"] = json!(description);
                        }
                        level
                    })
                    .collect();
                json!({
                    "type": "score",
                    "name": id.as_str(),
                    "instructions": q.instructions,
                    "levels": levels,
                })
            }
        })
        .collect();
    json!({ "model": model, "input": state.to_string(), "questions": questions })
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiAnswer {
    Predicate {
        name: Option<String>,
        probability: Number,
    },
    Choice {
        name: Option<String>,
        probabilities: Vec<OpenAiProbability>,
    },
    Score {
        name: Option<String>,
        probabilities: Vec<OpenAiLevelProbability>,
    },
    Refusal {
        name: Option<String>,
    },
}

#[derive(Deserialize)]
struct OpenAiProbability {
    value: Value,
    probability: Number,
}

#[derive(Deserialize)]
struct OpenAiLevelProbability {
    label: String,
    probability: Number,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    answers: Vec<OpenAiAnswer>,
}

fn openai_answers(body: &str) -> Result<BTreeMap<String, Reply>, DecideError> {
    let invalid = |m: String| DecideError::Invalid(m);
    let wire: OpenAiResponse = serde_json::from_str(body)
        .map_err(|e| invalid(format!("decoding OpenAI Decisions response: {e}")))?;
    let mut answers = BTreeMap::new();
    for answer in wire.answers {
        let (name, answer) = match answer {
            OpenAiAnswer::Predicate { name, probability } => {
                (name, Reply::Answered(WireAnswer::Noul(probability)))
            }
            OpenAiAnswer::Choice {
                name,
                probabilities,
            } => {
                let mut distribution = BTreeMap::new();
                for p in probabilities {
                    let Value::String(value) = p.value else {
                        return Err(invalid(format!(
                            "question {name:?}: choice value {} is not a declared label",
                            p.value
                        )));
                    };
                    if distribution.insert(value.clone(), p.probability).is_some() {
                        return Err(invalid(format!(
                            "question {name:?}: option {value:?} has two probabilities"
                        )));
                    }
                }
                (name, Reply::Answered(WireAnswer::Choice(distribution)))
            }
            OpenAiAnswer::Score {
                name,
                probabilities,
            } => {
                let mut distribution = BTreeMap::new();
                for p in probabilities {
                    if distribution
                        .insert(p.label.clone(), p.probability)
                        .is_some()
                    {
                        return Err(invalid(format!(
                            "question {name:?}: level {:?} has two probabilities",
                            p.label
                        )));
                    }
                }
                (name, Reply::Answered(WireAnswer::Score(distribution)))
            }
            OpenAiAnswer::Refusal { name } => (name, Reply::Refused),
        };
        let name = name.ok_or_else(|| invalid("an answer names no question".to_owned()))?;
        if answers.insert(name.clone(), answer).is_some() {
            return Err(invalid(format!(
                "the response answers question {name:?} twice"
            )));
        }
    }
    Ok(answers)
}

pub fn parse_response(
    api: DecisionApi,
    body: &str,
    questions: &BTreeMap<QuestionId, Question>,
    min_confidence: f64,
) -> Result<Decision, DecideError> {
    let invalid = |m: String| DecideError::Invalid(m);
    let mut answers = api.answers(body)?;
    let mut decision = BTreeMap::new();
    for (id, question) in questions {
        let answer = answers
            .remove(id.as_str())
            .ok_or_else(|| invalid(format!("the response omits question {id:?}")))?;
        let asked_as = api.asked_as(question);
        let answer = match answer {
            Reply::Refused => Answer {
                label: Label::uncertain(),
                confidence: 0.0,
                probabilities: BTreeMap::new(),
                score: None,
                asked_as,
                labels: Vec::new(),
            },
            Reply::Answered(answer) => Answer {
                asked_as,
                ..question
                    .resolve(
                        probabilities(id, question, asked_as, answer)?,
                        min_confidence,
                    )
                    .map_err(|e| invalid(format!("question {id:?}: {e}")))?
            },
        };
        decision.insert(id.clone(), answer);
    }
    if let Some(extra) = answers.keys().next() {
        return Err(invalid(format!(
            "the response answers undeclared question {extra:?}"
        )));
    }
    Ok(Decision(decision))
}

fn probabilities(
    id: &QuestionId,
    question: &Question,
    asked_as: Option<AskedAs>,
    answer: WireAnswer,
) -> Result<BTreeMap<Label, f64>, DecideError> {
    let invalid = |m: String| DecideError::Invalid(m);
    let float = |n: Number| {
        n.as_f64()
            .ok_or_else(|| invalid(format!("question {id:?}: {n} is not a probability")))
    };
    let label = |s: &str| Label::new(s).map_err(|e| invalid(format!("question {id:?}: {e}")));
    let distribution = |probabilities: BTreeMap<String, Number>| {
        probabilities
            .into_iter()
            .map(|(name, p)| Ok((label(&name)?, float(p)?)))
            .collect()
    };
    match (&question.kind, answer) {
        (_, WireAnswer::Unanswered) => Err(invalid(format!(
            "the model left question {id:?} unanswered"
        ))),
        (QuestionKind::Noul, WireAnswer::Noul(noul)) => {
            let noul = float(noul)?;
            Ok(BTreeMap::from([
                (label(NOUL_YES)?, noul),
                (label(NOUL_NO)?, 1.0 - noul),
            ]))
        }
        (QuestionKind::Choice { .. }, WireAnswer::Choice(probabilities)) => {
            distribution(probabilities)
        }
        (QuestionKind::Score { .. }, WireAnswer::Choice(probabilities))
            if asked_as == Some(AskedAs::Choice) =>
        {
            distribution(probabilities)
        }
        (QuestionKind::Score { .. }, WireAnswer::Score(probabilities))
            if asked_as == Some(AskedAs::Score) =>
        {
            distribution(probabilities)
        }
        (QuestionKind::Pick { .. }, _) => Err(invalid(format!(
            "question {id:?} is a pick, which only a person answers"
        ))),
        (kind, answer) => {
            let asked = match (kind, asked_as) {
                (QuestionKind::Noul, _) => "a noul",
                (QuestionKind::Choice { .. }, _) => "a choice",
                (QuestionKind::Score { .. }, Some(AskedAs::Choice)) => "a score asked as a choice",
                (QuestionKind::Score { .. }, _) => "a score",
                (QuestionKind::Pick { .. }, _) => "a pick",
            };
            let answered = match answer {
                WireAnswer::Noul(_) => "a noul",
                WireAnswer::Choice(_) => "a choice",
                WireAnswer::Score(_) => "a score",
                WireAnswer::Unanswered => "nothing",
            };
            Err(invalid(format!(
                "question {id:?} is {asked} but was answered as {answered}"
            )))
        }
    }
}

pub fn decide(
    endpoint: &Endpoint,
    questions: &BTreeMap<QuestionId, Question>,
    state: &Value,
    min_confidence: f64,
) -> Result<Decision, DecideError> {
    let api = endpoint.api.name();
    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| DecideError::Invalid(format!("building {api} client: {e}")))?;
    let mut request = client.post(&endpoint.url).json(&endpoint.api.request_body(
        &endpoint.model,
        questions,
        state,
    ));
    if let Some(key) = &endpoint.api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .map_err(|e| DecideError::Transport(format!("reaching {api}: {e}")))?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|e| DecideError::Transport(format!("reading {api} response: {e}")))?;
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(DecideError::Transport(format!(
            "{api} answered {status}: {}",
            truncate(&body)
        )));
    }
    if !status.is_success() {
        return Err(DecideError::Invalid(format!(
            "{api} rejected the request with {status}: {}",
            truncate(&body)
        )));
    }
    parse_response(endpoint.api, &body, questions, min_confidence)
}

fn truncate(body: &str) -> &str {
    let end = body.char_indices().nth(500).map_or(body.len(), |(i, _)| i);
    &body[..end]
}

#[cfg(test)]
mod tests {
    use crate::decide::*;
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
                        multiple: false,
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
    fn serve_once(
        path: &'static str,
        status: &'static str,
        body: &'static str,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}{path}", listener.local_addr().unwrap());
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
            api: DecisionApi::SystemOne,
            url,
            api_key: api_key.map(str::to_owned),
            model: "dgemma".into(),
        }
    }

    fn binding(key_env: Option<&str>) -> InferenceBinding {
        use crucible_contract::inference::{EnvName, InferenceProtocol, InferenceRole};
        InferenceBinding {
            role: InferenceRole::Decision,
            protocol: InferenceProtocol::SystemOne,
            url: Some("http://dgemma:8011/v1/systemone".into()),
            model: "dgemma".into(),
            key_env: key_env.map(|name| EnvName::new(name).unwrap()),
        }
    }

    #[test]
    fn an_endpoint_takes_its_url_and_model_from_the_binding() {
        let got = Endpoint::from_binding(&binding(None), |_| panic!("no key to look up")).unwrap();
        assert_eq!(
            got,
            Endpoint {
                api: DecisionApi::SystemOne,
                url: "http://dgemma:8011/v1/systemone".into(),
                api_key: None,
                model: "dgemma".into(),
            }
        );
    }

    #[test]
    fn an_endpoint_reads_its_key_from_the_variable_the_binding_names() {
        let got = Endpoint::from_binding(&binding(Some("DECISION_KEY")), |name| {
            (name == "DECISION_KEY").then(|| "sekret".to_owned())
        })
        .unwrap();
        assert_eq!(got.api_key.as_deref(), Some("sekret"));
    }

    #[test]
    fn a_named_key_that_is_unset_or_blank_is_an_error_not_an_open_endpoint() {
        for value in [None, Some(String::new()), Some("  ".to_owned())] {
            let err = Endpoint::from_binding(&binding(Some("DECISION_KEY")), |_| value.clone())
                .unwrap_err();
            match err {
                DecideError::Invalid(m) => assert!(m.contains("\"DECISION_KEY\" is unset"), "{m}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn the_request_carries_state_and_each_question_in_the_api_shape() {
        let body =
            DecisionApi::SystemOne.request_body("dgemma", &questions(), &json!({"ticket": "down"}));
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
        let d = parse_response(DecisionApi::SystemOne, GOOD, &questions(), 0.5).unwrap();
        let urgent = &d.0[&qid("urgent")];
        assert_eq!(urgent.label, label("yes"));
        assert_eq!(urgent.probabilities[&label("yes")], 0.92);
        assert!((urgent.probabilities[&label("no")] - 0.08).abs() < 1e-9);
    }

    #[test]
    fn a_low_noul_probability_is_a_confident_no() {
        let body = GOOD.replace("0.92", "0.03");
        let d = parse_response(DecisionApi::SystemOne, &body, &questions(), 0.9).unwrap();
        assert_eq!(d.0[&qid("urgent")].label, label("no"));
    }

    #[test]
    fn an_answer_below_the_threshold_is_uncertain() {
        let d = parse_response(DecisionApi::SystemOne, GOOD, &questions(), 0.8).unwrap();
        assert_eq!(d.0[&qid("urgent")].label, label("yes"));
        assert!(d.0[&qid("area")].label.is_uncertain());
        assert_eq!(d.0[&qid("area")].confidence, 0.6);
    }

    #[test]
    fn the_engine_derives_the_label_and_ignores_the_models_own_pick() {
        let body = GOOD.replace(r#""choice":"scheduler""#, r#""choice":"frontend""#);
        let d = parse_response(DecisionApi::SystemOne, &body, &questions(), 0.5).unwrap();
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
            match parse_response(DecisionApi::SystemOne, body, &questions(), 0.5) {
                Err(DecideError::Invalid(m)) => assert!(m.contains(needle), "{m} lacks {needle}"),
                other => panic!("{body} gave {other:?}"),
            }
        }
    }

    #[test]
    fn decide_posts_the_request_with_bearer_auth_and_returns_the_decision() {
        let (url, received) = serve_once("/v1/systemone", "200 OK", GOOD);
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
            DecisionApi::SystemOne.request_body("dgemma", &questions(), &json!({"ticket": "down"}))
        );
    }

    #[test]
    fn decide_sends_no_authorization_header_without_a_key() {
        let (url, received) = serve_once("/v1/systemone", "200 OK", GOOD);
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
            "/v1/systemone",
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
            let (url, _rx) = serve_once("/v1/systemone", status, "{}");
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

    fn openai_endpoint(url: String) -> Endpoint {
        Endpoint {
            api: DecisionApi::OpenAi,
            url,
            api_key: Some("sk-test".into()),
            model: "gpt-6-luna".into(),
        }
    }

    const OPENAI_GOOD: &str = r#"{"answers":[
        {"type":"predicate","name":"urgent","probability":0.92},
        {"type":"choice","name":"area","choice":"scheduler","probabilities":[
            {"value":"scheduler","probability":0.6},{"value":"frontend","probability":0.4}
        ],"confidence":0.5}
    ],"model":"gpt-6-luna","usage":{"input_tokens":120,"input_tokens_details":{"cache_write_tokens":0,"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":120}}"#;

    #[test]
    fn a_decisions_binding_without_a_url_reaches_openai() {
        use crucible_contract::inference::EnvName;
        let mut b = binding(Some("OPENAI_API_KEY"));
        b.protocol = InferenceProtocol::Decisions;
        b.url = None;
        b.model = "gpt-6-luna".into();
        let got = Endpoint::from_binding(&b, |name| {
            (name == "OPENAI_API_KEY").then(|| "sk-test".to_owned())
        })
        .unwrap();
        assert_eq!(
            got,
            Endpoint {
                api: DecisionApi::OpenAi,
                url: "https://api.openai.com/v1/decisions".into(),
                api_key: Some("sk-test".into()),
                model: "gpt-6-luna".into(),
            }
        );
        b.url = Some("https://gateway.corp/v1/decisions".into());
        b.key_env = Some(EnvName::new("GATEWAY_KEY").unwrap());
        let got = Endpoint::from_binding(&b, |_| Some("k".to_owned())).unwrap();
        assert_eq!(got.url, "https://gateway.corp/v1/decisions");
    }

    #[test]
    fn an_agent_protocol_is_not_a_decision_api() {
        for protocol in [
            InferenceProtocol::Messages,
            InferenceProtocol::ChatCompletions,
            InferenceProtocol::Responses,
        ] {
            let mut b = binding(None);
            b.protocol = protocol;
            match Endpoint::from_binding(&b, |_| None) {
                Err(DecideError::Invalid(m)) => {
                    assert!(m.contains("not a decision API"), "{m}")
                }
                other => panic!("{protocol}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_openai_request_names_each_question_and_carries_state_as_text() {
        let state = json!({"ticket": "down"});
        let body = DecisionApi::OpenAi.request_body("gpt-6-luna", &questions(), &state);
        assert_eq!(
            body,
            json!({
                "model": "gpt-6-luna",
                "input": r#"{"ticket":"down"}"#,
                "questions": [
                    {
                        "type": "choice",
                        "name": "area",
                        "instructions": "Which component?",
                        "choices": [
                            {"value": "scheduler", "description": "batching and queueing"},
                            {"value": "frontend"}
                        ]
                    },
                    {"type": "predicate", "name": "urgent", "instructions": "Needs a reply within the hour?"}
                ]
            })
        );
    }

    #[test]
    fn openai_answers_resolve_like_system_one_answers() {
        let openai = parse_response(DecisionApi::OpenAi, OPENAI_GOOD, &questions(), 0.5).unwrap();
        let system_one = parse_response(DecisionApi::SystemOne, GOOD, &questions(), 0.5).unwrap();
        assert_eq!(openai, system_one);
        let uncertain =
            parse_response(DecisionApi::OpenAi, OPENAI_GOOD, &questions(), 0.8).unwrap();
        assert!(uncertain.0[&qid("area")].label.is_uncertain());
        assert_eq!(uncertain.0[&qid("urgent")].label, label("yes"));
    }

    #[test]
    fn an_openai_refusal_is_uncertain_with_no_distribution() {
        let body = r#"{"answers":[{"type":"predicate","name":"urgent","probability":0.9},{"type":"refusal","name":"area"}]}"#;
        let d = parse_response(DecisionApi::OpenAi, body, &questions(), 0.5).unwrap();
        let area = &d.0[&qid("area")];
        assert!(area.label.is_uncertain());
        assert_eq!(area.confidence, 0.0);
        assert!(area.probabilities.is_empty());
        assert_eq!(d.0[&qid("urgent")].label, label("yes"));
    }

    #[test]
    fn the_engine_ignores_openais_own_choice_and_confidence() {
        let body = OPENAI_GOOD
            .replace(r#""choice":"scheduler""#, r#""choice":"frontend""#)
            .replace(r#""confidence":0.5"#, r#""confidence":0.99"#);
        let d = parse_response(DecisionApi::OpenAi, &body, &questions(), 0.55).unwrap();
        assert_eq!(d.0[&qid("area")].label, label("scheduler"));
        assert_eq!(d.0[&qid("area")].confidence, 0.6);
    }

    #[test]
    fn malformed_openai_responses_are_invalid() {
        let area = r#"{"type":"choice","name":"area","choice":"scheduler","probabilities":[{"value":"scheduler","probability":0.6},{"value":"frontend","probability":0.4}],"confidence":0.5}"#;
        let urgent = r#"{"type":"predicate","name":"urgent","probability":0.9}"#;
        let answers = |list: &[&str]| format!(r#"{{"answers":[{}]}}"#, list.join(","));
        let cases = [
            ("{}".to_owned(), "decoding"),
            (answers(&[urgent]), "omits"),
            (
                answers(&[
                    urgent,
                    r#"{"type":"score","name":"area","score":1.0,"probabilities":[],"confidence":0.5}"#,
                ]),
                "answered as a score",
            ),
            (
                answers(&[
                    r#"{"type":"predicate","name":"urgent","probability":1.4}"#,
                    area,
                ]),
                "outside [0, 1]",
            ),
            (
                answers(&[
                    urgent,
                    r#"{"type":"choice","name":"area","choice":true,"probabilities":[{"value":true,"probability":0.6},{"value":"frontend","probability":0.4}],"confidence":0.5}"#,
                ]),
                "is not a declared label",
            ),
            (
                answers(&[
                    urgent,
                    r#"{"type":"choice","name":"area","choice":"scheduler","probabilities":[{"value":"scheduler","probability":0.6},{"value":"scheduler","probability":0.4}],"confidence":0.5}"#,
                ]),
                "two probabilities",
            ),
            (
                answers(&[
                    urgent,
                    r#"{"type":"choice","name":"area","choice":"scheduler","probabilities":[{"value":"scheduler","probability":0.6},{"value":"kv_cache","probability":0.4}],"confidence":0.5}"#,
                ]),
                "undeclared label",
            ),
            (
                answers(&[
                    r#"{"type":"choice","name":"urgent","choice":"yes","probabilities":[{"value":"yes","probability":1.0}],"confidence":1.0}"#,
                    area,
                ]),
                "is a noul",
            ),
            (
                answers(&[
                    r#"{"type":"predicate","name":"area","probability":0.9}"#,
                    urgent,
                ]),
                "is a choice",
            ),
            (answers(&[urgent, urgent, area]), "twice"),
            (
                answers(&[r#"{"type":"predicate","probability":0.9}"#, area]),
                "names no question",
            ),
            (
                answers(&[
                    urgent,
                    area,
                    r#"{"type":"predicate","name":"extra","probability":0.5}"#,
                ]),
                "undeclared question",
            ),
        ];
        for (body, needle) in cases {
            match parse_response(DecisionApi::OpenAi, &body, &questions(), 0.5) {
                Err(DecideError::Invalid(m)) => assert!(m.contains(needle), "{m} lacks {needle}"),
                other => panic!("{body} gave {other:?}"),
            }
        }
    }

    #[test]
    fn decide_posts_to_openai_with_bearer_auth() {
        let (url, received) = serve_once("/v1/decisions", "200 OK", OPENAI_GOOD);
        let d = decide(
            &openai_endpoint(url),
            &questions(),
            &json!({"ticket": "down"}),
            0.5,
        )
        .unwrap();
        assert_eq!(d.0[&qid("urgent")].label, label("yes"));
        let request = received.recv().unwrap();
        assert!(
            request.starts_with("POST /v1/decisions HTTP/1.1"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-test")
        );
        let sent: Value = serde_json::from_str(request.rsplit('\n').next().unwrap()).unwrap();
        assert_eq!(
            sent,
            DecisionApi::OpenAi.request_body(
                "gpt-6-luna",
                &questions(),
                &json!({"ticket": "down"})
            )
        );
    }

    #[test]
    fn openai_errors_name_the_api_and_classify_like_system_one() {
        let (url, _rx) = serve_once(
            "/v1/decisions",
            "400 Bad Request",
            r#"{"error":{"message":"Unknown parameter: 'state'.","type":"invalid_request_error"}}"#,
        );
        match decide(&openai_endpoint(url), &questions(), &json!({}), 0.5) {
            Err(DecideError::Invalid(m)) => {
                assert!(m.starts_with("OpenAI Decisions rejected"), "{m}");
                assert!(m.contains("Unknown parameter"), "{m}");
            }
            other => panic!("{other:?}"),
        }
        let (url, _rx) = serve_once("/v1/decisions", "429 Too Many Requests", "{}");
        match decide(&openai_endpoint(url), &questions(), &json!({}), 0.5) {
            Err(DecideError::Transport(m)) => {
                assert!(m.starts_with("OpenAI Decisions answered"), "{m}")
            }
            other => panic!("{other:?}"),
        }
    }

    fn risk() -> BTreeMap<QuestionId, Question> {
        BTreeMap::from([(
            qid("risk"),
            Question {
                instructions: "How risky is the change?".into(),
                kind: QuestionKind::Score {
                    levels: ["low", "medium", "high"]
                        .into_iter()
                        .map(|l| ChoiceOption {
                            label: label(l),
                            description: Some(format!("{l} blast radius")),
                        })
                        .collect(),
                },
                drop: vec![],
            },
        )])
    }

    #[test]
    fn a_score_is_asked_natively_of_openai_and_as_a_choice_of_system_one() {
        let openai = DecisionApi::OpenAi.request_body("gpt-6-luna", &risk(), &json!({}));
        assert_eq!(
            openai["questions"][0],
            json!({
                "type": "score",
                "name": "risk",
                "instructions": "How risky is the change?",
                "levels": [
                    {"label": "low", "description": "low blast radius"},
                    {"label": "medium", "description": "medium blast radius"},
                    {"label": "high", "description": "high blast radius"},
                ],
            })
        );
        let system_one = DecisionApi::SystemOne.request_body("dgemma", &risk(), &json!({}));
        assert_eq!(
            system_one["questions"]["risk"],
            json!({
                "instructions": "How risky is the change?",
                "type": "choice",
                "criteria": {
                    "low": "low blast radius",
                    "medium": "medium blast radius",
                    "high": "high blast radius",
                },
            })
        );
    }

    /// Either way the engine computes the score from the distribution and records the form it
    /// asked in, ignoring the score and confidence the API reports.
    #[test]
    fn a_score_answer_records_the_engines_score_and_the_form_it_was_asked_in() {
        let openai = r#"{"answers":[{"type":"score","name":"risk","score":0.1,"probabilities":[{"value":0,"label":"low","probability":0.1},{"value":1,"label":"medium","probability":0.3},{"value":2,"label":"high","probability":0.6}],"confidence":0.9}]}"#;
        let system_one = r#"{"answers":{"risk":{"type":"choice","choice":"low","probabilities":{"low":0.1,"medium":0.3,"high":0.6},"confidence":0.9}}}"#;
        for (api, body, form) in [
            (DecisionApi::OpenAi, openai, AskedAs::Score),
            (DecisionApi::SystemOne, system_one, AskedAs::Choice),
        ] {
            let d = parse_response(api, body, &risk(), 0.5).unwrap();
            let risk = &d.0[&qid("risk")];
            assert_eq!(risk.label, label("high"));
            assert!((risk.confidence - 0.6).abs() < 1e-12);
            assert!((risk.score.unwrap() - 0.75).abs() < 1e-12);
            assert_eq!(risk.asked_as, Some(form));
        }
    }

    #[test]
    fn a_score_answered_in_another_form_or_with_no_mass_is_invalid() {
        for (api, body, needle) in [
            (
                DecisionApi::SystemOne,
                r#"{"answers":{"risk":{"type":"noul","noul":0.9}}}"#,
                "is a score asked as a choice but was answered as a noul",
            ),
            (
                DecisionApi::OpenAi,
                r#"{"answers":[{"type":"choice","name":"risk","choice":"low","probabilities":[{"value":"low","probability":1.0}],"confidence":1.0}]}"#,
                "is a score but was answered as a choice",
            ),
            (
                DecisionApi::OpenAi,
                r#"{"answers":[{"type":"score","name":"risk","score":0,"probabilities":[{"value":0,"label":"low","probability":0},{"value":1,"label":"medium","probability":0},{"value":2,"label":"high","probability":0}],"confidence":0}]}"#,
                "every level probability 0",
            ),
            (
                DecisionApi::OpenAi,
                r#"{"answers":[{"type":"score","name":"risk","score":0,"probabilities":[{"value":0,"label":"low","probability":0.5},{"value":0,"label":"low","probability":0.5}],"confidence":0}]}"#,
                "has two probabilities",
            ),
        ] {
            match parse_response(api, body, &risk(), 0.5) {
                Err(DecideError::Invalid(m)) => assert!(m.contains(needle), "{m} lacks {needle}"),
                other => panic!("{body} gave {other:?}"),
            }
        }
    }

    #[test]
    fn a_refused_score_is_uncertain_with_no_score_and_still_names_its_form() {
        let d = parse_response(
            DecisionApi::OpenAi,
            r#"{"answers":[{"type":"refusal","name":"risk"}]}"#,
            &risk(),
            0.5,
        )
        .unwrap();
        let risk = &d.0[&qid("risk")];
        assert!(risk.label.is_uncertain());
        assert_eq!(risk.score, None);
        assert_eq!(risk.asked_as, Some(AskedAs::Score));
    }

    #[test]
    fn noul_and_choice_answers_record_no_score_or_form() {
        for (api, body) in [
            (DecisionApi::SystemOne, GOOD),
            (DecisionApi::OpenAi, OPENAI_GOOD),
        ] {
            let d = parse_response(api, body, &questions(), 0.5).unwrap();
            for answer in d.0.values() {
                assert_eq!(answer.score, None);
                assert_eq!(answer.asked_as, None);
            }
        }
    }
}
