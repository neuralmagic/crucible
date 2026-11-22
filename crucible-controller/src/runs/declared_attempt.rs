//! The attempt a playbook task declared in its output. A pack puts a [`RunnerAttempt`] under the
//! reserved `attempt` field; this merges the records an earlier task and a later one each wrote.

use crate::runs::model::TaskResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The task-output field carrying the attempt. Reserved for the pack's declaration.
pub(crate) const ATTEMPT_FIELD: &str = "attempt";

/// The failure the pack observed for its attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttemptFailure {
    /// The line as the pack wants it shown.
    pub line: String,
    /// What produced the line: a pod, a host, a remote job id, whatever the pack names.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
}

/// The attempt a pack reports. The labels are whatever the pack wants a reader to see; every
/// field is optional so a task can add to what an earlier one reported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RunnerAttempt {
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub failure: Option<AttemptFailure>,
}

impl RunnerAttempt {
    fn of_output(output: &Value) -> Option<Self> {
        serde_json::from_value(output.get(ATTEMPT_FIELD)?.clone()).ok()
    }

    fn merge(&mut self, later: Self) {
        self.labels.extend(later.labels);
        if later.failure.is_some() {
            self.failure = later.failure;
        }
    }
}

/// The attempt the run's results declare, with each later record's fields winning.
pub(crate) fn declared(results: &[TaskResult]) -> Option<RunnerAttempt> {
    let mut merged = RunnerAttempt::default();
    let mut any = false;
    for output in results.iter().filter_map(|result| result.output.as_ref()) {
        let Some(attempt) = RunnerAttempt::of_output(output) else {
            continue;
        };
        merged.merge(attempt);
        any = true;
    }
    any.then_some(merged)
}

/// The failure the run's attempt declared, when it declared one.
pub(crate) fn declared_failure(results: &[TaskResult]) -> Option<AttemptFailure> {
    declared(results)?.failure
}

/// Whether any task attempt ended in a way a reader needs the failure line for.
pub(crate) fn run_failed(results: &[TaskResult]) -> bool {
    results
        .iter()
        .any(|result| matches!(result.status.as_str(), "fail" | "transport" | "truncated"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(iter: i64, output: Option<Value>) -> TaskResult {
        TaskResult {
            iter,
            task: "submit".to_string(),
            status: "fail".to_string(),
            note: String::new(),
            cost_usd: None,
            secs: None,
            blocked: None,
            links: Vec::new(),
            agent: None,
            repairs: Vec::new(),
            output,
        }
    }

    #[test]
    fn a_later_record_adds_to_what_an_earlier_one_declared() {
        let results = [
            result(
                0,
                Some(serde_json::json!({
                    "attempt": {
                        "labels": {"job": "batch/overlay-p1", "pod": "overlay-p1-runner"}
                    }
                })),
            ),
            result(
                1,
                Some(serde_json::json!({
                    "attempt": {
                        "failure": {"line": "FATAL: prebuild failed",
                                    "source": "overlay-p1-runner"}
                    }
                })),
            ),
        ];
        let attempt = declared(&results).expect("declared");
        assert_eq!(
            attempt.labels.get("job").map(String::as_str),
            Some("batch/overlay-p1")
        );
        assert_eq!(
            attempt.labels.get("pod").map(String::as_str),
            Some("overlay-p1-runner")
        );
        assert_eq!(
            attempt
                .failure
                .as_ref()
                .and_then(|failure| failure.source.as_deref()),
            Some("overlay-p1-runner")
        );
        assert_eq!(
            attempt
                .failure
                .as_ref()
                .map(|failure| failure.line.as_str()),
            Some("FATAL: prebuild failed")
        );
        assert!(run_failed(&results));
    }

    #[test]
    fn a_run_that_declared_no_attempt_has_none() {
        let results = [result(0, Some(serde_json::json!({"pass": true})))];
        assert!(declared(&results).is_none());
        assert!(declared_failure(&results).is_none());
        assert!(run_failed(&results));
    }
}
