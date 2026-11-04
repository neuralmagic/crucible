//! What one attempt of a task produced, as a runner reports it to the executor.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::deadline::Deadline;
use crate::plan::ir::{Task, TaskName};
use crucible_contract::TransportCause;
use crucible_contract::session::TaskRepair;

/// One attempt's result, as reported by the runner.
#[derive(Debug)]
pub enum AttemptOutcome {
    /// Measured success with the task's structured output (the edge payload).
    Pass(Value),
    /// Measured failure. Never retried: a task that failed, failed. `output` is the object this
    /// attempt itself produced (a graded evaluation record, a nonzero exit's JSON line), and
    /// `None` when it produced nothing readable.
    Fail { note: String, output: Option<Value> },
    /// The task ran and found its check inapplicable: no evidence either way, and nobody accused.
    /// Declared by the task itself via `"status": "skipped"` in its output — distinct from the
    /// walker's own skip (a task filtered out by substrate caps, which never ran at all).
    Skipped(Value, String),
    /// Transport failure (infra, not the work). Retried, bounded, every attempt visible.
    Transport(TransportFailure),
    /// The runner killed the attempt at its deadline. Settles as a measured failure naming the
    /// limit and is never retried: a rerun would spend the same time again.
    TimedOut(Deadline),
}

/// What an attempt died on before the task could be judged, and the engine's account of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportFailure {
    pub cause: TransportCause,
    pub note: String,
}

impl TransportFailure {
    pub fn new(cause: TransportCause, note: impl Into<String>) -> Self {
        Self {
            cause,
            note: note.into(),
        }
    }
}

impl std::fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.note)
    }
}

impl AttemptOutcome {
    /// A measured failure that produced no readable output.
    pub fn fail(note: impl Into<String>) -> Self {
        AttemptOutcome::Fail {
            note: note.into(),
            output: None,
        }
    }

    /// The terminal outcome a task's own reserved `status` field declares, or this outcome
    /// unchanged. A runner that acts on an attempt before returning it — capturing declared
    /// files, discarding a workspace — settles the declared status first, or it acts on a
    /// veto as if it were a pass.
    pub fn settle_declared(self) -> Self {
        let value = match self {
            AttemptOutcome::Pass(value) => value,
            other => return other,
        };
        match DeclaredStatus::of(&value) {
            Some(DeclaredStatus::Skipped) => {
                let note = declared_note(&value, DeclaredStatus::Skipped);
                AttemptOutcome::Skipped(value, note)
            }
            Some(DeclaredStatus::Fail) => {
                let note = declared_note(&value, DeclaredStatus::Fail);
                AttemptOutcome::Fail {
                    note,
                    output: Some(value),
                }
            }
            Some(DeclaredStatus::Pass) | None => AttemptOutcome::Pass(value),
        }
    }

    /// A failure the runner measured on the task's own result, as that result's declared `status`
    /// settles it: `"skipped"` skips it, and no value passes it.
    pub fn failed_as_declared(note: String, output: Option<Value>) -> Self {
        match output {
            Some(value) if DeclaredStatus::of(&value) == Some(DeclaredStatus::Skipped) => {
                AttemptOutcome::Skipped(value, note)
            }
            output => AttemptOutcome::Fail { note, output },
        }
    }
}

pub struct Attempt {
    pub outcome: AttemptOutcome,
    /// What the attempt cost, its repair turns included.
    pub cost_usd: f64,
    /// The repair turns this attempt took, in order.
    pub repairs: Vec<TaskRepair>,
}

impl Attempt {
    pub fn failed(cost_usd: f64, note: String) -> Self {
        Self {
            outcome: AttemptOutcome::fail(note),
            cost_usd,
            repairs: Vec::new(),
        }
    }

    pub fn transport(cause: TransportCause, note: impl Into<String>) -> Self {
        Self {
            outcome: AttemptOutcome::Transport(TransportFailure::new(cause, note)),
            cost_usd: 0.0,
            repairs: Vec::new(),
        }
    }

    pub fn timed_out(cost_usd: f64, deadline: Deadline) -> Self {
        Self {
            outcome: AttemptOutcome::TimedOut(deadline),
            cost_usd,
            repairs: Vec::new(),
        }
    }
}

/// One task of a concurrent dispatch batch (see [`TaskRunner::run_many`]). Its deadline is its
/// lane's, stamped when the lane is admitted.
pub struct BatchItem<'a> {
    pub task: &'a Task,
    pub attempt: u32,
    pub inputs: BTreeMap<TaskName, Value>,
}

/// A task's self-declared `status`, which wins over the boolean `pass` when present.
///
/// Without this a task that RAN could only report pass or fail, so a rung whose check turned out to
/// be inapplicable had to pick between claiming success and accusing the candidate. The GLM A/B rung
/// picked success, and six hours of GPU reported a green attribution rung with no attribution behind
/// it. `skipped` is the honest third answer: ran, measured nothing, blames nobody. `fail` is how a
/// task with no exit code and no `pass` grading vetoes itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclaredStatus {
    Pass,
    Fail,
    Skipped,
}

impl DeclaredStatus {
    /// Every value the engine acts on. Any other value of the field is ignored.
    pub const ALL: [DeclaredStatus; 3] = [
        DeclaredStatus::Pass,
        DeclaredStatus::Fail,
        DeclaredStatus::Skipped,
    ];

    /// The token a task writes into its output's reserved `status` field.
    pub fn as_str(self) -> &'static str {
        match self {
            DeclaredStatus::Pass => "pass",
            DeclaredStatus::Fail => "fail",
            DeclaredStatus::Skipped => "skipped",
        }
    }

    fn of(value: &Value) -> Option<Self> {
        match value.get("status").and_then(Value::as_str)? {
            "pass" => Some(DeclaredStatus::Pass),
            "fail" => Some(DeclaredStatus::Fail),
            "skipped" => Some(DeclaredStatus::Skipped),
            _ => None,
        }
    }
}

/// The reason a task's result states in its `note` field, when that is a non-blank string.
pub fn own_note(value: &Value) -> Option<&str> {
    value
        .get(crate::plan::ir::NOTE_FIELD)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|note| !note.is_empty())
}

/// The note a self-declaring task gave, or a stand-in naming what it declared.
fn declared_note(value: &Value, declared: DeclaredStatus) -> String {
    own_note(value)
        .map(str::to_string)
        .unwrap_or_else(|| format!("task declared status={}", declared.as_str()))
}
