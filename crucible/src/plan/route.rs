//! The deciders that leave the engine: a model through one System One call, and a person
//! through the controller's elicitation endpoint.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crucible_broker::elicit::{ElicitError, Waited};
use crucible_broker::systemone::{DecideError, Endpoint, decide};
use crucible_contract::TransportCause;
use crucible_contract::decision::{Question, QuestionId};
use serde_json::Value;

use crate::plan::exec::{Attempt, AttemptOutcome, Elicited, TransportFailure};
use crate::plan::ir::{Decider, Task, TaskKind, TaskName};

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

/// How often a waiting route reads its question back.
const POLL: Duration = Duration::from_secs(5);

/// Ask a person `task`'s questions through the elicitation endpoint the controller configured.
pub fn human_elicit(task: &Task, ceiling: Option<Instant>) -> Elicited {
    let TaskKind::Route {
        questions,
        decider: Decider::Human { deadline_secs, .. },
    } = &task.task
    else {
        return Elicited::Failed(format!("task {} is not a human-decided route", task.name));
    };
    let Some(endpoint) =
        crucible_broker::elicit::Endpoint::from_env(|name| std::env::var(name).ok())
    else {
        return Elicited::Failed(format!(
            "{} is unset, so no person can be asked",
            crucible_contract::elicit::ENV_ELICIT_URL
        ));
    };
    let request = crucible_contract::elicit::ElicitRequest {
        questions: questions.clone(),
        deadline_secs: *deadline_secs,
    };
    let bounds = crucible_broker::elicit::Bounds {
        poll: POLL,
        ceiling,
    };
    match crucible_broker::elicit::wait(&endpoint, &task.name.0, &request, bounds) {
        Ok(Waited::Answered(answers)) => Elicited::Answered(answers),
        Ok(Waited::Expired(answers)) => Elicited::Expired(answers),
        Ok(Waited::Cut(answers)) => Elicited::Cut(answers),
        Err(ElicitError::Invalid(note)) => Elicited::Failed(note),
        Err(ElicitError::Transport(note)) => {
            Elicited::Transport(TransportFailure::new(TransportCause::Provider, note))
        }
    }
}
