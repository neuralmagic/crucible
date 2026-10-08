//! Human-decided routes (RFC-0002 C-HUMAN-DECISION, C-DECISION-EVIDENCE). The executor opens a
//! route's decision request through a [`DecisionDesk`], parks the route while other work runs,
//! and settles it from the request's terminal state.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;

use crate::plan::ir::{Task, TaskName, ValidPlan};
use crucible_contract::decision::{Answer, Decision, Label, Question, QuestionId, QuestionKind};
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

/// A settled dependency of the route, as its evidence shows it.
pub struct Dependency<'a> {
    pub status: &'a str,
    pub passed: bool,
    pub output: Option<&'a Value>,
}

/// What a route's request carries beside its questions.
pub struct EvidenceInputs<'a> {
    pub plan: &'a ValidPlan,
    pub route: &'a Task,
    pub questions: &'a BTreeMap<QuestionId, Question>,
    pub dependencies: &'a BTreeMap<TaskName, Dependency<'a>>,
    /// The captured copy of a file a task declared, when the run captured it.
    pub captured: &'a dyn Fn(&Task, &str) -> Option<Vec<u8>>,
    pub run: RunEvidence,
    pub review: Option<&'a str>,
    pub max_bytes: usize,
}

pub fn build_evidence(inputs: EvidenceInputs<'_>) -> Result<Evidence, String> {
    let EvidenceInputs {
        plan,
        route,
        questions,
        dependencies,
        captured,
        run,
        review,
        max_bytes,
    } = inputs;
    let mut deps = BTreeMap::new();
    for dep in &route.depends_on {
        let Some(result) = dependencies.get(dep) else {
            continue;
        };
        let passed = result.passed;
        let files = match plan.get(dep).filter(|_| passed) {
            Some(producer) => producer
                .emits_files
                .iter()
                .filter_map(|declared| {
                    captured(producer, &declared.path).map(|bytes| FileEvidence {
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
                status: result.status.to_owned(),
                output: result.output.filter(|_| passed).cloned(),
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
    let mut choices = BTreeMap::new();
    for (id, question) in questions {
        if let Some(source) = question.pick_source() {
            let output = dependencies
                .get(&TaskName(source.task.clone()))
                .filter(|r| r.passed)
                .and_then(|r| r.output);
            choices.insert(
                id.clone(),
                pick_options(output, &source.task, &source.field)?,
            );
        }
    }
    let mut evidence = Evidence {
        inputs: deps,
        run,
        gated,
        review: None,
        choices,
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

/// A pick question's options: the strings of a list field of its source's output, first
/// occurrence kept.
fn pick_options(output: Option<&Value>, task: &str, field: &str) -> Result<Vec<String>, String> {
    let Some(output) = output else {
        return Err(format!("{task} produced no output to pick {field} from"));
    };
    let Some(items) = output.get(field).and_then(Value::as_array) else {
        return Err(format!("{task}.{field} is not a list"));
    };
    let mut seen = BTreeSet::new();
    let mut options = Vec::new();
    for item in items {
        let Some(item) = item.as_str() else {
            return Err(format!(
                "{task}.{field} holds {item}, which is not a string"
            ));
        };
        if seen.insert(item) {
            options.push(item.to_owned());
        }
    }
    if options.is_empty() {
        return Err(format!(
            "{task}.{field} is empty, so there is nothing to pick"
        ));
    }
    Ok(options)
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

/// How a route's request settled: passing with the route's output, or failing with why.
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    Pass(Value),
    Fail(String),
}

/// The route's settled result for a terminal request state; `None` while it is open. `choices`
/// are the pick options the request's evidence offered.
pub fn settle(
    questions: &BTreeMap<QuestionId, Question>,
    choices: &BTreeMap<QuestionId, Vec<String>>,
    state: &RequestState,
) -> Option<Settled> {
    match &state.status {
        RequestStatus::Open => None,
        RequestStatus::Answered { answer } => Some(answered(questions, choices, answer)),
        RequestStatus::Expired => {
            let mut decision = BTreeMap::new();
            let mut picks = BTreeMap::new();
            for (id, question) in questions {
                if question.pick_source().is_some() {
                    picks.insert(id.clone(), Vec::new());
                } else {
                    decision.insert(
                        id.clone(),
                        Answer {
                            label: Label::uncertain(),
                            confidence: 0.0,
                            probabilities: BTreeMap::new(),
                            score: None,
                            asked_as: None,
                            labels: Vec::new(),
                        },
                    );
                }
            }
            Some(passing(
                Decision(decision),
                picks,
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

fn answered(
    questions: &BTreeMap<QuestionId, Question>,
    choices: &BTreeMap<QuestionId, Vec<String>>,
    answer: &AnswerRecord,
) -> Settled {
    if let Some(extra) = answer.labels.keys().find(|id| !questions.contains_key(*id)) {
        return failing(format!(
            "the answer names {extra:?}, which the route does not ask"
        ));
    }
    let mut decision = BTreeMap::new();
    let mut picks = BTreeMap::new();
    for (id, question) in questions {
        let values = match answer.labels.get(id).map(Vec::as_slice) {
            None | Some([]) => return failing(format!("the answer gives nothing for {id:?}")),
            Some(values) => values,
        };
        if values.len() > 1 && !question.multiple() {
            return failing(format!(
                "the answer gives {} values for {id:?}, which takes one",
                values.len()
            ));
        }
        if values.iter().collect::<BTreeSet<_>>().len() != values.len() {
            return failing(format!("the answer repeats a value for {id:?}"));
        }
        if let QuestionKind::Pick { .. } = question.kind {
            let offered = choices.get(id).map_or(&[][..], Vec::as_slice);
            if let Some(value) = values.iter().find(|v| !offered.contains(v)) {
                return failing(format!(
                    "the answer picks {value:?} for {id:?}, which the request did not offer"
                ));
            }
            picks.insert(id.clone(), values.to_vec());
            continue;
        }
        let mut labels = Vec::with_capacity(values.len());
        for value in values {
            match Label::new(value.as_str()) {
                Ok(label) if !label.is_uncertain() && question.resolves_to(&label) => {
                    labels.push(label);
                }
                _ => {
                    return failing(format!(
                        "the answer gives {value:?} for {id:?}, which the question does not declare"
                    ));
                }
            }
        }
        let Some(first) = labels.first().cloned() else {
            return failing(format!("the answer gives nothing for {id:?}"));
        };
        let score = question.level_score(&first);
        decision.insert(
            id.clone(),
            Answer {
                label: first,
                confidence: 1.0,
                probabilities: labels.iter().map(|l| (l.clone(), 1.0)).collect(),
                score,
                asked_as: None,
                labels: if question.multiple() {
                    labels
                } else {
                    Vec::new()
                },
            },
        );
    }
    match serde_json::to_value(answer) {
        Ok(record) => passing(Decision(decision), picks, record),
        Err(e) => failing(e.to_string()),
    }
}

fn passing(decision: Decision, picks: BTreeMap<QuestionId, Vec<String>>, record: Value) -> Settled {
    let mut output = match serde_json::to_value(decision) {
        Ok(output) => output,
        Err(e) => return failing(e.to_string()),
    };
    if let Some(object) = output.as_object_mut() {
        for (id, values) in picks {
            object.insert(id.to_string(), Value::from(values));
        }
        object.insert(DECISION_KEY.to_owned(), record);
    }
    Settled::Pass(output)
}

fn failing(note: String) -> Settled {
    Settled::Fail(note)
}

#[cfg(test)]
mod tests {
    use crate::plan::decide::*;
    use crucible_contract::decision::{ChoiceOption, PickSource};

    fn choice(labels: &[&str], multiple: bool) -> Question {
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
                multiple,
            },
            drop: Vec::new(),
        }
    }

    fn pick(multiple: bool) -> Question {
        Question {
            instructions: "which nodes?".into(),
            kind: QuestionKind::Pick {
                source: PickSource {
                    task: "plan".into(),
                    field: "nodes".into(),
                },
                multiple,
            },
            drop: Vec::new(),
        }
    }

    fn id(name: &str) -> QuestionId {
        QuestionId::new(name).unwrap()
    }

    fn questions() -> BTreeMap<QuestionId, Question> {
        BTreeMap::from([(id("go"), choice(&["approve", "deny"], false))])
    }

    fn mixed() -> BTreeMap<QuestionId, Question> {
        BTreeMap::from([
            (id("go"), choice(&["approve", "deny"], false)),
            (id("checks"), choice(&["lint", "unit", "e2e"], true)),
            (id("node"), pick(false)),
            (id("nodes"), pick(true)),
        ])
    }

    fn offered() -> BTreeMap<QuestionId, Vec<String>> {
        let nodes = vec!["a1".to_owned(), "b2".to_owned(), "c3".to_owned()];
        BTreeMap::from([(id("node"), nodes.clone()), (id("nodes"), nodes)])
    }

    fn none() -> BTreeMap<QuestionId, Vec<String>> {
        BTreeMap::new()
    }

    fn state(status: RequestStatus) -> RequestState {
        RequestState {
            id: "r1".into(),
            evidence_digest: "sha256:ab".into(),
            expires_at: "2026-10-06T18:00:00Z".into(),
            status,
        }
    }

    fn answer(labels: &[(&str, &[&str])]) -> RequestStatus {
        RequestStatus::Answered {
            answer: AnswerRecord {
                labels: labels
                    .iter()
                    .map(|(q, ls)| (id(q), ls.iter().map(|l| (*l).to_owned()).collect()))
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
        assert!(settle(&questions(), &none(), &state(RequestStatus::Open)).is_none());
    }

    #[test]
    fn an_answer_settles_the_route_with_its_labels_and_the_record() {
        let r = settle(
            &questions(),
            &none(),
            &state(answer(&[("go", &["approve"])])),
        )
        .unwrap();
        let Settled::Pass(out) = r else {
            panic!("{r:?}");
        };
        assert_eq!(out["go"]["label"], "approve");
        assert_eq!(out["go"]["confidence"], 1.0);
        assert!(out["go"].get("labels").is_none());
        assert_eq!(out[DECISION_KEY]["decided_by"], "user:wseaton");
        assert_eq!(out[DECISION_KEY]["note"], "ship it");
        assert_eq!(
            out[DECISION_KEY]["labels"]["go"],
            serde_json::json!(["approve"])
        );
    }

    #[test]
    fn an_answer_off_the_declared_labels_fails_the_route() {
        let cases: [&[(&str, &[&str])]; 7] = [
            &[("go", &["maybe"])],
            &[("go", &["uncertain"])],
            &[("go", &["Not A Label"])],
            &[],
            &[("go", &[])],
            &[("go", &["approve", "deny"])],
            &[("go", &["approve"]), ("other", &["approve"])],
        ];
        for labels in cases {
            let r = settle(&questions(), &none(), &state(answer(labels))).unwrap();
            assert!(matches!(r, Settled::Fail(_)), "{labels:?}");
        }
    }

    #[test]
    fn a_multiple_choice_and_picks_settle_with_every_value() {
        let r = settle(
            &mixed(),
            &offered(),
            &state(answer(&[
                ("go", &["approve"]),
                ("checks", &["unit", "lint"]),
                ("node", &["b2"]),
                ("nodes", &["c3", "a1"]),
            ])),
        )
        .unwrap();
        let Settled::Pass(out) = r else {
            panic!("{r:?}");
        };
        assert_eq!(out["checks"]["label"], "unit");
        assert_eq!(out["checks"]["labels"], serde_json::json!(["unit", "lint"]));
        assert_eq!(out["checks"]["probabilities"]["lint"], 1.0);
        let checks: Answer = serde_json::from_value(out["checks"].clone()).unwrap();
        assert!(checks.chose(&Label::new("lint").unwrap()));
        assert!(!checks.chose(&Label::new("e2e").unwrap()));
        assert_eq!(out["node"], serde_json::json!(["b2"]));
        assert_eq!(out["nodes"], serde_json::json!(["c3", "a1"]));
    }

    #[test]
    fn a_multiple_answer_holding_one_label_still_lists_it() {
        let r = settle(
            &mixed(),
            &offered(),
            &state(answer(&[
                ("go", &["deny"]),
                ("checks", &["e2e"]),
                ("node", &["a1"]),
                ("nodes", &["a1"]),
            ])),
        )
        .unwrap();
        let Settled::Pass(out) = r else {
            panic!("{r:?}");
        };
        assert_eq!(out["checks"]["labels"], serde_json::json!(["e2e"]));
    }

    #[test]
    fn a_pick_or_multiple_answer_off_its_bounds_fails_the_route() {
        let base: [(&str, &[&str]); 4] = [
            ("go", &["approve"]),
            ("checks", &["lint"]),
            ("node", &["a1"]),
            ("nodes", &["a1"]),
        ];
        let cases: [(&str, &[&str]); 7] = [
            ("node", &["zz"]),
            ("node", &["a1", "b2"]),
            ("nodes", &["a1", "a1"]),
            ("nodes", &[]),
            ("checks", &["lint", "lint"]),
            ("checks", &["lint", "nope"]),
            ("checks", &["uncertain"]),
        ];
        for (question, values) in cases {
            let labels: Vec<(&str, &[&str])> = base
                .iter()
                .map(|(q, v)| {
                    if *q == question {
                        (*q, values)
                    } else {
                        (*q, *v)
                    }
                })
                .collect();
            let r = settle(&mixed(), &offered(), &state(answer(&labels))).unwrap();
            assert!(matches!(r, Settled::Fail(_)), "{question} {values:?}");
        }
        let r = settle(&mixed(), &none(), &state(answer(&base))).unwrap();
        assert!(
            matches!(r, Settled::Fail(_)),
            "a pick the request did not offer"
        );
    }

    #[test]
    fn expiry_answers_every_question_uncertain_and_passes() {
        let r = settle(&mixed(), &offered(), &state(RequestStatus::Expired)).unwrap();
        let Settled::Pass(out) = r else {
            panic!("{r:?}");
        };
        assert_eq!(out["go"]["label"], "uncertain");
        assert_eq!(out["go"]["confidence"], 0.0);
        assert_eq!(out["checks"]["label"], "uncertain");
        assert!(out["checks"].get("labels").is_none());
        assert_eq!(out["nodes"], serde_json::json!([]));
        assert_eq!(out["node"], serde_json::json!([]));
        assert_eq!(out[DECISION_KEY]["outcome"], "expired");
        assert!(out[DECISION_KEY]["decided_by"].is_null());
    }

    #[test]
    fn withdrawal_fails_the_route() {
        let r = settle(&questions(), &none(), &state(RequestStatus::Withdrawn)).unwrap();
        assert!(matches!(r, Settled::Fail(_)));
    }

    #[test]
    fn pick_options_are_the_source_strings_in_order_without_repeats() {
        let out =
            serde_json::json!({"nodes": ["b", "a", "b"], "n": 3, "mixed": ["a", 1], "none": []});
        assert_eq!(
            pick_options(Some(&out), "plan", "nodes").unwrap(),
            ["b", "a"]
        );
        for field in ["n", "mixed", "none", "missing"] {
            assert!(pick_options(Some(&out), "plan", field).is_err(), "{field}");
        }
        assert!(pick_options(None, "plan", "nodes").is_err());
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
