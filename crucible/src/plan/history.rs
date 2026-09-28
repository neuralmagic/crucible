//! A launch series' history as the engine hands it to a task: the supplied records, ordered and
//! trimmed to the task's depth and the operator's size bound.

use std::collections::BTreeMap;

use crucible_contract::history::{
    DEFAULT_HISTORY_MAX_BYTES, ENV_HISTORY, ENV_HISTORY_MAX_BYTES, HistoryEntry, HistoryRecord,
    RecordedStatus, SuppliedHistory,
};
use serde_json::Value;

use crate::plan::exec::{TaskResult, TaskStatus};
use crate::plan::ir::{Task, TaskName, ValidPlan};
use crate::plan::machine::BlockedReason;

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error(transparent)]
    Document(#[from] crucible_contract::history::HistoryError),
    #[error("{ENV_HISTORY} record {run:?} has {field} {value:?}, which is not an RFC 3339 time")]
    Timestamp {
        run: String,
        field: &'static str,
        value: String,
    },
    #[error("{ENV_HISTORY} record {run:?} ends before it starts")]
    EndsBeforeStart { run: String },
    #[error("{ENV_HISTORY_MAX_BYTES} is {got:?}; it must be a positive whole number of bytes")]
    Limit { got: String },
    #[error("{ENV_HISTORY} record {run:?} does not encode: {detail}")]
    Encode { run: String, detail: String },
}

/// Who set the size bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    Operator,
    EngineDefault,
}

/// The most bytes of history one task receives, in its compact JSON encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLimit {
    pub bytes: u64,
    pub source: LimitSource,
}

impl HistoryLimit {
    /// Read the operator's bound from [`ENV_HISTORY_MAX_BYTES`], or take the engine default.
    pub fn from_env() -> Result<Self, HistoryError> {
        Self::parse(std::env::var(ENV_HISTORY_MAX_BYTES).ok().as_deref())
    }

    pub fn parse(raw: Option<&str>) -> Result<Self, HistoryError> {
        match raw {
            None => Ok(HistoryLimit {
                bytes: DEFAULT_HISTORY_MAX_BYTES,
                source: LimitSource::EngineDefault,
            }),
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(bytes) if bytes > 0 => Ok(HistoryLimit {
                    bytes,
                    source: LimitSource::Operator,
                }),
                _ => Err(HistoryError::Limit {
                    got: raw.to_owned(),
                }),
            },
        }
    }

    /// One line for `crucible check`.
    pub fn describe(&self) -> String {
        let source = match self.source {
            LimitSource::Operator => ENV_HISTORY_MAX_BYTES,
            LimitSource::EngineDefault => "engine default",
        };
        format!(
            "history size limit: {} bytes per task ({source})",
            self.bytes
        )
    }
}

/// One supplied record and the length of its compact encoding.
#[derive(Debug, Clone)]
struct Encoded {
    value: Value,
    bytes: u64,
}

/// The records a launch supplied, oldest first by end time.
#[derive(Debug, Clone)]
pub struct SeriesHistory {
    records: Vec<Encoded>,
    limit: HistoryLimit,
}

impl SeriesHistory {
    /// Read [`ENV_HISTORY`] and the size bound. An absent document is a run in no series.
    pub fn from_env() -> Result<Self, HistoryError> {
        let limit = HistoryLimit::from_env()?;
        match std::env::var(ENV_HISTORY) {
            Ok(text) => Self::parse(&text, limit),
            Err(_) => Ok(SeriesHistory {
                records: Vec::new(),
                limit,
            }),
        }
    }

    pub fn parse(text: &str, limit: HistoryLimit) -> Result<Self, HistoryError> {
        let doc = SuppliedHistory::parse(text)?;
        let mut timed: Vec<(jiff::Timestamp, HistoryRecord)> = doc
            .records
            .into_iter()
            .map(|record| {
                let started = timestamp(&record, "started_at", &record.started_at)?;
                let ended = timestamp(&record, "ended_at", &record.ended_at)?;
                if ended < started {
                    return Err(HistoryError::EndsBeforeStart {
                        run: record.run.clone(),
                    });
                }
                Ok((ended, record))
            })
            .collect::<Result<_, _>>()?;
        timed.sort_by_key(|(ended, _)| *ended);
        let records = timed
            .into_iter()
            .map(|(_, record)| {
                let encode_error = |e: serde_json::Error| HistoryError::Encode {
                    run: record.run.clone(),
                    detail: e.to_string(),
                };
                let value = serde_json::to_value(&record).map_err(encode_error)?;
                let bytes = serde_json::to_vec(&value).map_err(encode_error)?.len() as u64;
                Ok(Encoded { value, bytes })
            })
            .collect::<Result<_, HistoryError>>()?;
        Ok(SeriesHistory { records, limit })
    }

    /// What a task declaring `depth` receives: the most recent `depth` records, oldest first,
    /// less whole records from the oldest end until the encoding fits the bound, and how many
    /// that removed.
    pub fn input(&self, depth: u32) -> Value {
        let window = &self.records[self.records.len().saturating_sub(depth as usize)..];
        let mut dropped = 0;
        while dropped < window.len() && encoded_len(&window[dropped..], dropped) > self.limit.bytes
        {
            dropped += 1;
        }
        envelope(&window[dropped..], dropped)
    }
}

/// What a task in a run with no series receives.
pub fn empty_input() -> Value {
    envelope(&[], 0)
}

fn envelope(records: &[Encoded], dropped: usize) -> Value {
    serde_json::json!({
        "records": records.iter().map(|r| r.value.clone()).collect::<Vec<_>>(),
        "dropped": dropped,
    })
}

/// The compact encoding's length of [`envelope`], without building it: the fixed keys and
/// brackets, each record, the commas between them, and the digits of the count.
fn encoded_len(records: &[Encoded], dropped: usize) -> u64 {
    const FRAME: u64 = r#"{"records":[],"dropped":}"#.len() as u64;
    let commas = records.len().saturating_sub(1) as u64;
    let body: u64 = records.iter().map(|r| r.bytes).sum();
    FRAME + commas + body + dropped.to_string().len() as u64
}

fn timestamp(
    record: &HistoryRecord,
    field: &'static str,
    value: &str,
) -> Result<jiff::Timestamp, HistoryError> {
    value
        .parse::<jiff::Timestamp>()
        .map_err(|_| HistoryError::Timestamp {
            run: record.run.clone(),
            field,
            value: value.to_owned(),
        })
}

/// A passing task's output, cut down to the fields it declares in `emits`.
pub fn declared_output(task: &Task, result: &TaskResult) -> Option<Value> {
    if result.status != TaskStatus::Pass {
        return None;
    }
    let object = result.output.as_ref()?.as_object()?;
    Some(Value::Object(
        task.emits
            .iter()
            .filter_map(|field| {
                object
                    .get(&field.0)
                    .cloned()
                    .map(|value| (field.0.clone(), value))
            })
            .collect(),
    ))
}

/// What this run records for its series. A task left undispatched, by early completion or by a
/// ceiling, never settled, so its status is null.
pub fn run_entry(
    plan: &ValidPlan,
    record: Option<&TaskName>,
    results: &BTreeMap<TaskName, TaskResult>,
) -> HistoryEntry {
    let Some(name) = record else {
        return HistoryEntry {
            task: String::new(),
            status: None,
            output: None,
        };
    };
    let settled = results.get(name).filter(|r| {
        !matches!(
            r.blocked,
            Some(BlockedReason::BudgetCeiling | BlockedReason::WallClockCeiling)
        )
    });
    HistoryEntry {
        task: name.0.clone(),
        status: settled.map(|r| recorded(r.status)),
        output: settled
            .zip(plan.get(name))
            .and_then(|(r, task)| declared_output(task, r)),
    }
}

fn recorded(status: TaskStatus) -> RecordedStatus {
    match status {
        TaskStatus::Pass => RecordedStatus::Pass,
        TaskStatus::Fail => RecordedStatus::Fail,
        TaskStatus::Transport => RecordedStatus::Transport,
        TaskStatus::Skipped => RecordedStatus::Skipped,
        TaskStatus::NotTaken => RecordedStatus::NotTaken,
        TaskStatus::Blocked => RecordedStatus::Blocked,
        TaskStatus::Truncated => RecordedStatus::Truncated,
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::exec::{TaskResult, TaskStatus};
    use crate::plan::history::{
        HistoryError, HistoryLimit, LimitSource, SeriesHistory, empty_input, encoded_len, envelope,
    };
    use crate::plan::history::{declared_output, recorded, run_entry};
    use crate::plan::ir::{Plan, TaskName, ValidPlan};
    use crate::plan::machine::BlockedReason;
    use crucible_contract::history::{DEFAULT_HISTORY_MAX_BYTES, HistoryEntry, RecordedStatus};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    fn result(status: TaskStatus, output: Option<Value>) -> TaskResult {
        TaskResult {
            status,
            attempts: 1,
            cost_usd: 0.0,
            output,
            note: None,
            fanout: None,
            blocked: None,
            transport: None,
        }
    }

    fn card_plan() -> ValidPlan {
        Plan::from_toml_str(
            r#"
version = 1
[budget]
usd = 1
[[task]]
name = "card"
kind = "command"
command = "true"
emits = ["verdict", "dirty"]
"#,
        )
        .unwrap()
        .validate()
        .unwrap()
    }

    #[test]
    fn projection_keeps_only_declared_fields_from_a_passing_task() {
        let plan = card_plan();
        let task = plan.get(&"card".into()).unwrap();
        let passed = result(
            TaskStatus::Pass,
            Some(json!({
                "verdict": "ACTION REQUIRED",
                "dirty": 3,
                "undeclared_secret": "must not cross"
            })),
        );
        assert_eq!(
            declared_output(task, &passed),
            Some(json!({"verdict": "ACTION REQUIRED", "dirty": 3}))
        );
        let failed = TaskResult {
            status: TaskStatus::Fail,
            ..passed
        };
        assert_eq!(declared_output(task, &failed), None);
    }

    #[test]
    fn the_entry_carries_the_record_tasks_status_and_declared_output() {
        let plan = card_plan();
        let card = TaskName::from("card");
        let results = BTreeMap::from([(
            card.clone(),
            result(
                TaskStatus::Pass,
                Some(json!({"verdict": "ok", "dirty": 0, "scratch": "x"})),
            ),
        )]);
        assert_eq!(
            run_entry(&plan, Some(&card), &results),
            HistoryEntry {
                task: "card".into(),
                status: Some(RecordedStatus::Pass),
                output: Some(json!({"verdict": "ok", "dirty": 0})),
            }
        );

        let failed = BTreeMap::from([(
            card.clone(),
            result(TaskStatus::Fail, Some(json!({"verdict": "bad"}))),
        )]);
        assert_eq!(
            run_entry(&plan, Some(&card), &failed),
            HistoryEntry {
                task: "card".into(),
                status: Some(RecordedStatus::Fail),
                output: None,
            }
        );

        let mut blocked = result(TaskStatus::Blocked, None);
        blocked.blocked = Some(BlockedReason::RequiredTaskFailed("other".into()));
        assert_eq!(
            run_entry(
                &plan,
                Some(&card),
                &BTreeMap::from([(card.clone(), blocked)])
            )
            .status,
            Some(RecordedStatus::Blocked)
        );
    }

    #[test]
    fn a_record_task_that_never_settled_records_null_status_and_output() {
        let plan = card_plan();
        let card = TaskName::from("card");
        let never = HistoryEntry {
            task: "card".into(),
            status: None,
            output: None,
        };
        assert_eq!(run_entry(&plan, Some(&card), &BTreeMap::new()), never);
        for reason in [
            BlockedReason::BudgetCeiling,
            BlockedReason::WallClockCeiling,
        ] {
            let mut r = result(TaskStatus::Blocked, None);
            r.blocked = Some(reason);
            assert_eq!(
                run_entry(&plan, Some(&card), &BTreeMap::from([(card.clone(), r)])),
                never
            );
        }
    }

    #[test]
    fn a_playbook_naming_no_record_task_records_an_empty_entry() {
        let plan = card_plan();
        let results = BTreeMap::from([(
            TaskName::from("card"),
            result(TaskStatus::Pass, Some(json!({"verdict": "ok"}))),
        )]);
        assert_eq!(
            run_entry(&plan, None, &results),
            HistoryEntry {
                task: String::new(),
                status: None,
                output: None,
            }
        );
    }

    /// The recorded tokens are the task-result event's.
    #[test]
    fn recorded_statuses_carry_the_task_result_tokens() {
        for status in [
            TaskStatus::Pass,
            TaskStatus::Fail,
            TaskStatus::Transport,
            TaskStatus::Skipped,
            TaskStatus::NotTaken,
            TaskStatus::Blocked,
            TaskStatus::Truncated,
        ] {
            assert_eq!(
                serde_json::to_value(recorded(status)).unwrap(),
                Value::String(status.as_str().to_owned())
            );
        }
    }

    fn record(run: &str, ended: &str, note: &str) -> Value {
        json!({
            "run": run,
            "started_at": "2026-09-01T00:00:00Z",
            "ended_at": ended,
            "outcome": "finished",
            "verdict": "valid",
            "revision": "rev",
            "link": format!("https://controller.test/runs/{run}"),
            "entry": {"task": "triage", "status": "pass", "output": {"note": note}},
        })
    }

    fn doc(records: Vec<Value>) -> String {
        json!({"version": 1, "records": records}).to_string()
    }

    fn unbounded() -> HistoryLimit {
        HistoryLimit {
            bytes: u64::MAX,
            source: LimitSource::Operator,
        }
    }

    fn runs(input: &Value) -> Vec<&str> {
        input["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["run"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn a_task_gets_the_most_recent_records_oldest_first_by_end_time() {
        let history = SeriesHistory::parse(
            &doc(vec![
                record("c", "2026-09-03T00:00:00Z", ""),
                record("a", "2026-09-01T00:00:00Z", ""),
                record("d", "2026-09-04T00:00:00+02:00", ""),
                record("b", "2026-09-02T00:00:00Z", ""),
            ]),
            unbounded(),
        )
        .unwrap();
        assert_eq!(runs(&history.input(30)), ["a", "b", "c", "d"]);
        assert_eq!(runs(&history.input(2)), ["c", "d"]);
        assert_eq!(runs(&history.input(1)), ["d"]);
        assert_eq!(history.input(2)["dropped"], 0);
    }

    #[test]
    fn no_series_is_an_empty_list_and_zero() {
        assert_eq!(empty_input(), json!({"records": [], "dropped": 0}));
        let history = SeriesHistory::parse(&doc(vec![]), unbounded()).unwrap();
        assert_eq!(history.input(5), empty_input());
    }

    /// A record reaches a task exactly as its supplier wrote it: no field added, none lost.
    #[test]
    fn a_record_arrives_with_exactly_the_supplied_fields() {
        let mut supplied = record("a", "2026-09-01T00:10:00Z", "x");
        supplied["outcome"] = Value::Null;
        supplied["entry"]["output"] = json!({"n": 1.25, "items": [1, 2]});
        let history = SeriesHistory::parse(&doc(vec![supplied.clone()]), unbounded()).unwrap();
        assert_eq!(history.input(1)["records"][0], supplied);
    }

    #[test]
    fn the_size_bound_drops_whole_records_from_the_oldest_end_and_counts_them() {
        let records = vec![
            record("a", "2026-09-01T00:00:00Z", &"a".repeat(400)),
            record("b", "2026-09-02T00:00:00Z", &"b".repeat(400)),
            record("c", "2026-09-03T00:00:00Z", &"c".repeat(400)),
        ];
        let full = SeriesHistory::parse(&doc(records.clone()), unbounded()).unwrap();
        let two = serde_json::to_vec(&json!({"records": [records[1], records[2]], "dropped": 1}))
            .unwrap()
            .len() as u64;
        let bounded = SeriesHistory::parse(
            &doc(records.clone()),
            HistoryLimit {
                bytes: two,
                source: LimitSource::Operator,
            },
        )
        .unwrap();
        let input = bounded.input(3);
        assert_eq!(runs(&input), ["b", "c"]);
        assert_eq!(input["dropped"], 1);
        assert_eq!(serde_json::to_vec(&input).unwrap().len() as u64, two);
        for kept in input["records"].as_array().unwrap() {
            assert!(records.contains(kept), "a record was truncated: {kept}");
        }

        // `dropped` counts only records within this task's depth.
        assert_eq!(bounded.input(2)["dropped"], 0);
        assert_eq!(runs(&bounded.input(2)), ["b", "c"]);

        let tiny = SeriesHistory::parse(
            &doc(records),
            HistoryLimit {
                bytes: 1,
                source: LimitSource::Operator,
            },
        )
        .unwrap();
        assert_eq!(tiny.input(3), json!({"records": [], "dropped": 3}));
        assert_eq!(full.input(3)["dropped"], 0);
    }

    /// The bound is checked against the length of what the task receives, not an estimate.
    #[test]
    fn the_computed_length_is_the_encoded_length() {
        let history = SeriesHistory::parse(
            &doc((0..12)
                .map(|i| {
                    record(
                        &format!("r{i}"),
                        &format!("2026-09-{:02}T00:00:00Z", i + 1),
                        "é\"x",
                    )
                })
                .collect()),
            unbounded(),
        )
        .unwrap();
        for dropped in [0, 1, 11, 12] {
            let kept = &history.records[dropped..];
            let actual = serde_json::to_vec(&envelope(kept, dropped)).unwrap().len() as u64;
            assert_eq!(encoded_len(kept, dropped), actual, "dropped {dropped}");
        }
    }

    #[test]
    fn malformed_times_and_limits_are_refused() {
        let bad = record("a", "yesterday", "");
        assert!(matches!(
            SeriesHistory::parse(&doc(vec![bad]), unbounded()),
            Err(HistoryError::Timestamp {
                field: "ended_at",
                ..
            })
        ));
        let backwards = record("a", "2026-08-01T00:00:00Z", "");
        assert!(matches!(
            SeriesHistory::parse(&doc(vec![backwards]), unbounded()),
            Err(HistoryError::EndsBeforeStart { .. })
        ));
        assert!(matches!(
            SeriesHistory::parse("{}", unbounded()),
            Err(HistoryError::Document(_))
        ));
        for raw in ["0", "-1", "lots", ""] {
            assert!(
                matches!(
                    HistoryLimit::parse(Some(raw)),
                    Err(HistoryError::Limit { .. })
                ),
                "{raw:?}"
            );
        }
        assert_eq!(
            HistoryLimit::parse(None).unwrap(),
            HistoryLimit {
                bytes: DEFAULT_HISTORY_MAX_BYTES,
                source: LimitSource::EngineDefault
            }
        );
        assert_eq!(
            HistoryLimit::parse(Some("2048")).unwrap().describe(),
            "history size limit: 2048 bytes per task (CRUCIBLE_HISTORY_MAX_BYTES)"
        );
    }
}
