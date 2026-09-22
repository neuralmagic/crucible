//! The model-decided half of a route task: one System One call, recorded as the task's output.

use std::collections::BTreeMap;

use crucible_broker::systemone::{DecideError, Endpoint, decide};
use crucible_contract::TransportCause;
use crucible_contract::decision::{Question, QuestionId};
use serde_json::Value;

use crate::plan::exec::{Attempt, AttemptOutcome};
use crate::plan::ir::TaskName;

pub fn model_attempt(
    endpoint: &Endpoint,
    questions: &BTreeMap<QuestionId, Question>,
    min_confidence: f64,
    inputs: &BTreeMap<TaskName, Value>,
) -> Attempt {
    let state = match serde_json::to_value(inputs) {
        Ok(state) => state,
        Err(e) => return Attempt::failed(0.0, format!("inputs not serializable: {e}")),
    };
    match decide(endpoint, questions, &state, min_confidence) {
        Ok(decision) => match serde_json::to_value(decision) {
            Ok(output) => Attempt {
                outcome: AttemptOutcome::Pass(output),
                cost_usd: 0.0,
            },
            Err(e) => Attempt::failed(0.0, format!("encoding the decision: {e}")),
        },
        Err(DecideError::Transport(note)) => Attempt::transport(TransportCause::Provider, note),
        Err(DecideError::Invalid(note)) => Attempt::failed(0.0, note),
    }
}
