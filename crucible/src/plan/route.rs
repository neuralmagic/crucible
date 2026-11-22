//! The model-decided half of a route task: one decision API call, recorded as the task's output.

use std::collections::BTreeMap;

use crucible_broker::decide::{DecideError, Endpoint, decide};
use crucible_contract::TransportCause;
use crucible_contract::decision::{CompiledQuestion, QuestionId};
use crucible_contract::inference::{ENV_INFERENCE, InferenceRole};
use serde_json::Value;

use crate::plan::attempt::{Attempt, AttemptOutcome};
use crate::plan::ir::{Task, TaskName};
use crate::plan::options::asked_questions;

/// What a runner calls for a model-decided route.
pub type ModelRoute =
    fn(&Task, &BTreeMap<QuestionId, CompiledQuestion>, f64, &BTreeMap<TaskName, Value>) -> Attempt;

/// A model-decided route answered by the decision binding in the run's inference document.
pub fn model_attempt_from_env(
    route: &Task,
    questions: &BTreeMap<QuestionId, CompiledQuestion>,
    min_confidence: f64,
    inputs: &BTreeMap<TaskName, Value>,
) -> Attempt {
    let inference = match crate::inference::from_process_env() {
        Ok(inference) => inference,
        Err(error) => return Attempt::failed(0.0, error.to_string()),
    };
    let Some(binding) = inference.binding(InferenceRole::Decision) else {
        return Attempt::failed(0.0, format!("{ENV_INFERENCE} holds no decision binding"));
    };
    let endpoint = match Endpoint::from_binding(binding, |name| std::env::var(name).ok()) {
        Ok(endpoint) => endpoint,
        Err(error) => return Attempt::failed(0.0, error.to_string()),
    };
    model_attempt(&endpoint, route, questions, min_confidence, inputs)
}

pub fn model_attempt(
    endpoint: &Endpoint,
    route: &Task,
    questions: &BTreeMap<QuestionId, CompiledQuestion>,
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
