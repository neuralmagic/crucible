//! The model-decided half of a route task: one decision API call, recorded as the task's output.

use std::collections::BTreeMap;

use crucible_broker::decide::{DecideError, Endpoint, decide};
use crucible_contract::TransportCause;
use crucible_contract::decision::{
    ChoiceOption, OptionSource, Question, QuestionId, dynamic_options,
};
use serde_json::Value;

use crate::plan::exec::{Attempt, AttemptOutcome};
use crate::plan::ir::{ITEM_INPUT, Join, Task, TaskName};

pub fn model_attempt(
    endpoint: &Endpoint,
    route: &Task,
    questions: &BTreeMap<QuestionId, Question>,
    min_confidence: f64,
    inputs: &BTreeMap<TaskName, Value>,
) -> Attempt {
    let (asked, resolved) = match asked_questions(route, questions, inputs) {
        Ok(asked) => asked,
        Err(note) => return Attempt::failed(0.0, note),
    };
    let state = match serde_json::to_value(inputs) {
        Ok(state) => state,
        Err(e) => return Attempt::failed(0.0, format!("inputs not serializable: {e}")),
    };
    match decide(endpoint, &asked, &state, min_confidence) {
        Ok(mut decision) => {
            for (id, options) in resolved {
                if let Some(answer) = decision.0.get_mut(&id) {
                    answer.options = options;
                }
            }
            match serde_json::to_value(decision) {
                Ok(output) => Attempt {
                    outcome: AttemptOutcome::Pass(output),
                    cost_usd: 0.0,
                    repairs: Vec::new(),
                },
                Err(e) => Attempt::failed(0.0, format!("encoding the decision: {e}")),
            }
        }
        Err(DecideError::Transport(note)) => Attempt::transport(TransportCause::Provider, note),
        Err(DecideError::Invalid(note)) => Attempt::failed(0.0, note),
    }
}

type Resolved = BTreeMap<QuestionId, Vec<ChoiceOption>>;

/// The questions as this decision asks them, each dynamic choice turned into a choice over the
/// options its source supplies, and the options each dynamic choice resolved to.
fn asked_questions(
    route: &Task,
    questions: &BTreeMap<QuestionId, Question>,
    inputs: &BTreeMap<TaskName, Value>,
) -> Result<(BTreeMap<QuestionId, Question>, Resolved), String> {
    let mut asked = BTreeMap::new();
    let mut resolved = BTreeMap::new();
    for (id, question) in questions {
        let Some(source) = question.options_from() else {
            asked.insert(id.clone(), question.clone());
            continue;
        };
        let (list, key) = option_list(route, source, inputs)?;
        let options = dynamic_options(list).map_err(|e| match key {
            Some(key) => format!("options {source} for {key:?} {e}"),
            None => format!("options {source} {e}"),
        })?;
        asked.insert(id.clone(), question.with_options(options.clone()));
        resolved.insert(id.clone(), options);
    }
    Ok((asked, resolved))
}

/// The value `source` holds for this decision, and the element key it was read under. An instance
/// of a mapped route, which receives its key under [`ITEM_INPUT`], reads the entry under that key,
/// unless `keyed` already narrowed the field to it.
fn option_list<'a>(
    route: &Task,
    source: &OptionSource,
    inputs: &'a BTreeMap<TaskName, Value>,
) -> Result<(&'a Value, Option<&'a str>), String> {
    let entry = inputs
        .get(&TaskName(source.task.clone()))
        .ok_or_else(|| format!("options {source}: {} contributed no output", source.task))?;
    let output = match route.join {
        Join::Settled => entry.get("output").unwrap_or(&Value::Null),
        Join::All | Join::Passed => entry,
    };
    let field = output.get(&source.field).ok_or_else(|| {
        format!(
            "options {source} is absent from what {} emitted",
            source.task
        )
    })?;
    let Some(key) = inputs.get(&TaskName(ITEM_INPUT.to_owned())) else {
        return Ok((field, None));
    };
    let key = key
        .as_str()
        .ok_or_else(|| format!("options {source}: the element key is {key}, not a string"))?;
    let narrowed = route
        .keyed
        .iter()
        .any(|k| k.task.0 == source.task && k.field.0 == source.field);
    if narrowed {
        return Ok((field, Some(key)));
    }
    let Value::Object(by_key) = field else {
        return Err(format!(
            "options {source} is not an object keyed by element"
        ));
    };
    by_key
        .get(key)
        .map(|list| (list, Some(key)))
        .ok_or_else(|| format!("options {source} has no entry for {key:?}"))
}

#[cfg(test)]
mod tests {
    use crate::plan::ir::Plan;
    use crate::plan::route::*;
    use crucible_contract::decision::{Label, QuestionKind};
    use serde_json::json;

    fn route(extra: &str) -> (Task, BTreeMap<QuestionId, Question>) {
        let text = format!(
            r#"version = 1
[budget]
usd = 1.0

[[task]]
name = "match"
kind = "route"
needs = "decision"
depends_on = ["prepare"]
{extra}
decider = {{ kind = "model", min_confidence = 0.5 }}
[task.questions.package]
instructions = "Which package?"
type = "dynamic_choice"
options_from = {{ task = "prepare", field = "packages" }}
[task.questions.urgent]
instructions = "Now?"
type = "noul"
"#
        );
        let task = Plan::from_toml_str(&text).unwrap().tasks.remove(0);
        let crate::plan::ir::TaskKind::Route { questions, .. } = &task.task else {
            panic!("match is a route");
        };
        let questions = questions.clone();
        (task, questions)
    }

    fn inputs(pairs: &[(&str, Value)]) -> BTreeMap<TaskName, Value> {
        pairs
            .iter()
            .map(|(name, value)| (TaskName((*name).to_owned()), value.clone()))
            .collect()
    }

    fn package() -> QuestionId {
        QuestionId::new("package").unwrap()
    }

    fn labels(question: &Question) -> Vec<String> {
        question.labels().iter().map(Label::to_string).collect()
    }

    #[test]
    fn an_unmapped_route_asks_over_the_fields_list_and_leaves_other_questions_alone() {
        let (task, questions) = route("");
        let state = inputs(&[(
            "prepare",
            json!({"packages": [{"value": "p1", "description": "golang.org/x/net"}, "none"]}),
        )]);
        let (asked, resolved) = asked_questions(&task, &questions, &state).unwrap();
        let QuestionKind::Choice { options } = &asked[&package()].kind else {
            panic!("the dynamic choice is asked as a choice");
        };
        assert_eq!(labels(&asked[&package()]), ["p1", "none"]);
        assert_eq!(options[0].description.as_deref(), Some("golang.org/x/net"));
        assert_eq!(asked[&package()].instructions, "Which package?");
        let urgent = QuestionId::new("urgent").unwrap();
        assert_eq!(asked[&urgent], questions[&urgent]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(&resolved[&package()], options);
    }

    #[test]
    fn an_instance_of_a_mapped_route_reads_the_entry_under_its_key() {
        let (task, questions) = route("");
        let state = inputs(&[
            (
                "prepare",
                json!({"packages": {"a": ["p1", "none"], "b": ["p2", "p3", "none"]}}),
            ),
            (ITEM_INPUT, json!("b")),
        ]);
        let (asked, _) = asked_questions(&task, &questions, &state).unwrap();
        assert_eq!(labels(&asked[&package()]), ["p2", "p3", "none"]);
    }

    #[test]
    fn a_field_keyed_already_narrowed_is_read_as_the_list() {
        let (task, questions) = route(r#"keyed = [{ task = "prepare", field = "packages" }]"#);
        let state = inputs(&[
            ("prepare", json!({"packages": ["p1", "none"]})),
            (ITEM_INPUT, json!("a")),
        ]);
        let (asked, _) = asked_questions(&task, &questions, &state).unwrap();
        assert_eq!(labels(&asked[&package()]), ["p1", "none"]);
    }

    #[test]
    fn a_settled_join_reads_the_producers_output_entry() {
        let (task, questions) = route(r#"join = "settled""#);
        let state = inputs(&[(
            "prepare",
            json!({"status": "pass", "output": {"packages": ["p1", "none"]}}),
        )]);
        let (asked, _) = asked_questions(&task, &questions, &state).unwrap();
        assert_eq!(labels(&asked[&package()]), ["p1", "none"]);
    }

    #[test]
    fn options_that_cannot_be_read_fail_naming_the_field_and_the_key() {
        let (task, questions) = route("");
        let item = (ITEM_INPUT, json!("b"));
        let cases = [
            (
                inputs(&[]),
                "options prepare.packages: prepare contributed no output",
            ),
            (
                inputs(&[("prepare", json!({"other": 1}))]),
                "options prepare.packages is absent from what prepare emitted",
            ),
            (
                inputs(&[
                    ("prepare", json!({"packages": ["p1", "none"]})),
                    item.clone(),
                ]),
                "options prepare.packages is not an object keyed by element",
            ),
            (
                inputs(&[
                    ("prepare", json!({"packages": {"a": ["p1", "none"]}})),
                    item.clone(),
                ]),
                "options prepare.packages has no entry for \"b\"",
            ),
            (
                inputs(&[
                    ("prepare", json!({"packages": {"b": ["none"]}})),
                    item.clone(),
                ]),
                "options prepare.packages for \"b\" holds 1 distinct option(s); a choice needs at \
                 least two",
            ),
            (
                inputs(&[("prepare", json!({"packages": "p1"}))]),
                "options prepare.packages is a string, not a list of options",
            ),
        ];
        for (state, expected) in cases {
            assert_eq!(
                asked_questions(&task, &questions, &state).unwrap_err(),
                expected
            );
        }
    }
}
