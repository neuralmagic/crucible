//! Human-decided routes (RFC-0002 C-HUMAN-DECISION, C-DECISION-EVIDENCE). The executor opens a
//! route's decision request through a [`DecisionDesk`], parks the route while other work runs,
//! and settles it from the request's terminal state.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;

use crate::plan::exec::{TaskResult, TaskRunner, TaskStatus};
use crate::plan::ir::{Task, TaskName, ValidPlan};
use crucible_contract::decision::{Answer, Decision, Label, Question, QuestionId};
use crucible_contract::decision_request::{
    AnswerRecord, DECISION_KEY, Evidence, FileEvidence, GatedTask, InputEvidence, OpenRequest,
    RequestState, RequestStatus, RunEvidence,
};

/// The orchestrator that holds decision requests, as the run reaches it.
pub trait DecisionDesk: std::fmt::Debug {
    /// Open the route's request, or return the one already open for this run and task.
    fn open(&self, request: &OpenRequest) -> Result<RequestState, String>;
    fn poll(&self, id: &str) -> Result<RequestState, String>;
    /// How long to wait between polls while every runnable task waits on a request.
    fn interval(&self) -> Duration;
}

/// Read by [`evidence_limit`]; the default is 16 MiB.
pub const EVIDENCE_MAX_ENV: &str = "CRUCIBLE_DECISION_EVIDENCE_MAX_BYTES";
const DEFAULT_EVIDENCE_MAX: usize = 16 * 1024 * 1024;

pub fn evidence_limit() -> usize {
    std::env::var(EVIDENCE_MAX_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_EVIDENCE_MAX)
}

/// What a route's request carries beside its questions.
pub struct EvidenceInputs<'a> {
    pub plan: &'a ValidPlan,
    pub route: &'a Task,
    pub results: &'a BTreeMap<TaskName, TaskResult>,
    pub runner: &'a dyn TaskRunner,
    pub run: RunEvidence,
    pub review: Option<&'a str>,
    pub max_bytes: usize,
}

pub fn build_evidence(inputs: EvidenceInputs<'_>) -> Result<Evidence, String> {
    let EvidenceInputs {
        plan,
        route,
        results,
        runner,
        run,
        review,
        max_bytes,
    } = inputs;
    let mut deps = BTreeMap::new();
    for dep in &route.depends_on {
        let Some(result) = results.get(dep) else {
            continue;
        };
        let passed = result.status == TaskStatus::Pass;
        let files = match plan.get(dep).filter(|_| passed) {
            Some(producer) => producer
                .emits_files
                .iter()
                .filter_map(|declared| {
                    runner
                        .captured_file(producer, &declared.path)
                        .map(|bytes| FileEvidence {
                            path: declared.path.clone(),
                            media_type: media_type(&declared.path).to_owned(),
                            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                        })
                })
                .collect(),
            None => Vec::new(),
        };
        deps.insert(
            dep.0.clone(),
            InputEvidence {
                status: result.status.as_str().to_owned(),
                output: result.output.clone().filter(|_| passed),
                files,
            },
        );
    }
    let gated = plan
        .tasks_topo()
        .filter_map(|t| {
            let when = t.when.as_ref().filter(|w| w.task == route.name)?;
            Some(GatedTask {
                name: t.name.0.clone(),
                kind: t.task.label().to_owned(),
                needs: t.needs.clone(),
                question: when.question.clone(),
                labels: when.is.clone(),
            })
        })
        .collect();
    let mut evidence = Evidence {
        inputs: deps,
        run,
        gated,
        review: None,
    };
    if let Some(template) = review {
        let context = serde_json::to_value(&evidence).map_err(|e| e.to_string())?;
        evidence.review = Some(render_review(template, &context)?);
    }
    let size = serde_json::to_vec(&evidence)
        .map_err(|e| e.to_string())?
        .len();
    if size > max_bytes {
        return Err(format!(
            "the decision evidence is {size} bytes, over the {max_bytes} byte bound ({EVIDENCE_MAX_ENV})"
        ));
    }
    Ok(evidence)
}

/// Render a review template over the evidence. Every inserted value is escaped for CommonMark,
/// so a value cannot open a link, an image, or raw HTML.
pub fn render_review(template: &str, context: &Value) -> Result<String, String> {
    let mut env = minijinja::Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    env.set_formatter(|out, _state, value| {
        out.write_str(&markdown_escape(&value.to_string()))
            .map_err(minijinja::Error::from)
    });
    env.add_template("review", template)
        .map_err(|e| format!("review template: {e}"))?;
    let template = env
        .get_template("review")
        .map_err(|e| format!("review template: {e}"))?;
    template
        .render(crucible_broker::report::template_value(context))
        .map_err(|e| format!("rendering the review: {e}"))
}

/// Backslash-escape every ASCII punctuation character, which CommonMark treats as literal.
fn markdown_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn media_type(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "html" | "htm" => "text/html",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "txt" | "log" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// The route's settled result for a terminal request state; `None` while it is open.
pub fn settle(
    questions: &BTreeMap<QuestionId, Question>,
    state: &RequestState,
) -> Option<TaskResult> {
    match &state.status {
        RequestStatus::Open => None,
        RequestStatus::Answered { answer } => Some(answered(questions, answer)),
        RequestStatus::Expired => {
            let decision = questions
                .keys()
                .map(|id| {
                    (
                        id.clone(),
                        Answer {
                            label: Label::uncertain(),
                            confidence: 0.0,
                            probabilities: BTreeMap::new(),
                        },
                    )
                })
                .collect();
            Some(passing(
                Decision(decision),
                serde_json::json!({
                    "outcome": "expired",
                    "decided_by": null,
                    "evidence_digest": state.evidence_digest,
                }),
            ))
        }
        RequestStatus::Withdrawn => Some(failing("the decision request was withdrawn".to_owned())),
    }
}

fn answered(questions: &BTreeMap<QuestionId, Question>, answer: &AnswerRecord) -> TaskResult {
    if let Some(extra) = answer.labels.keys().find(|id| !questions.contains_key(*id)) {
        return failing(format!(
            "the answer names {extra:?}, which the route does not ask"
        ));
    }
    let mut decision = BTreeMap::new();
    for (id, question) in questions {
        let Some(label) = answer.labels.get(id) else {
            return failing(format!("the answer gives no label for {id:?}"));
        };
        if label.is_uncertain() || !question.resolves_to(label) {
            return failing(format!(
                "the answer gives {label:?} for {id:?}, which the question does not declare"
            ));
        }
        decision.insert(
            id.clone(),
            Answer {
                label: label.clone(),
                confidence: 1.0,
                probabilities: BTreeMap::from([(label.clone(), 1.0)]),
            },
        );
    }
    match serde_json::to_value(answer) {
        Ok(record) => passing(Decision(decision), record),
        Err(e) => failing(e.to_string()),
    }
}

fn passing(decision: Decision, record: Value) -> TaskResult {
    let mut output = match serde_json::to_value(decision) {
        Ok(output) => output,
        Err(e) => return failing(e.to_string()),
    };
    if let Some(object) = output.as_object_mut() {
        object.insert(DECISION_KEY.to_owned(), record);
    }
    TaskResult {
        status: TaskStatus::Pass,
        attempts: 1,
        cost_usd: 0.0,
        output: Some(output),
        note: None,
        fanout: None,
        blocked: None,
        transport: None,
        repairs: Vec::new(),
    }
}

pub(crate) fn failing(note: String) -> TaskResult {
    TaskResult {
        status: TaskStatus::Fail,
        attempts: 1,
        cost_usd: 0.0,
        output: None,
        note: Some(note),
        fanout: None,
        blocked: None,
        transport: None,
        repairs: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::decide::*;

    fn choice(labels: &[&str]) -> Question {
        use crucible_contract::decision::{ChoiceOption, QuestionKind};
        Question {
            instructions: "launch it?".into(),
            kind: QuestionKind::Choice {
                options: labels
                    .iter()
                    .map(|l| ChoiceOption {
                        label: Label::new(*l).unwrap(),
                        description: None,
                    })
                    .collect(),
            },
            drop: Vec::new(),
        }
    }

    fn questions() -> BTreeMap<QuestionId, Question> {
        BTreeMap::from([(QuestionId::new("go").unwrap(), choice(&["approve", "deny"]))])
    }

    fn state(status: RequestStatus) -> RequestState {
        RequestState {
            id: "r1".into(),
            evidence_digest: "sha256:ab".into(),
            expires_at: "2026-10-06T18:00:00Z".into(),
            status,
        }
    }

    fn answer(labels: &[(&str, &str)]) -> RequestStatus {
        RequestStatus::Answered {
            answer: AnswerRecord {
                labels: labels
                    .iter()
                    .map(|(q, l)| (QuestionId::new(*q).unwrap(), Label::new(*l).unwrap()))
                    .collect(),
                decided_by: "user:wseaton".into(),
                decided_at: "2026-10-06T17:00:00Z".into(),
                evidence_digest: "sha256:ab".into(),
                note: Some("ship it".into()),
            },
        }
    }

    #[test]
    fn an_open_request_settles_nothing() {
        assert!(settle(&questions(), &state(RequestStatus::Open)).is_none());
    }

    #[test]
    fn an_answer_settles_the_route_with_its_labels_and_the_record() {
        let r = settle(&questions(), &state(answer(&[("go", "approve")]))).unwrap();
        assert_eq!(r.status, TaskStatus::Pass);
        let out = r.output.unwrap();
        assert_eq!(out["go"]["label"], "approve");
        assert_eq!(out["go"]["confidence"], 1.0);
        assert_eq!(out[DECISION_KEY]["decided_by"], "user:wseaton");
        assert_eq!(out[DECISION_KEY]["note"], "ship it");
    }

    #[test]
    fn an_answer_off_the_declared_labels_fails_the_route() {
        for labels in [
            vec![("go", "maybe")],
            vec![("go", "uncertain")],
            vec![],
            vec![("go", "approve"), ("other", "approve")],
        ] {
            let r = settle(&questions(), &state(answer(&labels))).unwrap();
            assert_eq!(r.status, TaskStatus::Fail, "{labels:?}");
        }
    }

    #[test]
    fn expiry_answers_every_question_uncertain_and_passes() {
        let r = settle(&questions(), &state(RequestStatus::Expired)).unwrap();
        assert_eq!(r.status, TaskStatus::Pass);
        let out = r.output.unwrap();
        assert_eq!(out["go"]["label"], "uncertain");
        assert_eq!(out["go"]["confidence"], 0.0);
        assert_eq!(out[DECISION_KEY]["outcome"], "expired");
        assert!(out[DECISION_KEY]["decided_by"].is_null());
    }

    #[test]
    fn withdrawal_fails_the_route() {
        let r = settle(&questions(), &state(RequestStatus::Withdrawn)).unwrap();
        assert_eq!(r.status, TaskStatus::Fail);
    }

    #[test]
    fn review_values_are_escaped_and_undefined_names_refuse() {
        let context = serde_json::json!({"inputs": {"plan": {"output": {"title": "[click](javascript:x) <b>"}}}});
        let out = render_review("# {{ inputs.plan.output.title }}", &context).unwrap();
        assert_eq!(out, "# \\[click\\]\\(javascript\\:x\\) \\<b\\>");
        assert!(render_review("{{ nope.field }}", &context).is_err());
        assert!(render_review("{% if %}", &context).is_err());
    }

    #[test]
    fn media_types_follow_the_extension() {
        assert_eq!(media_type("out/chart.HTML"), "text/html");
        assert_eq!(media_type("a.svg"), "image/svg+xml");
        assert_eq!(media_type("data.csv"), "text/csv");
        assert_eq!(media_type("noext"), "application/octet-stream");
    }
}
