//! What a finished run records for its launch series.

use std::collections::BTreeMap;

use crucible_contract::history::{HistoryEntry, RecordedStatus};
use serde_json::Value;

use crate::plan::exec::{TaskResult, TaskStatus};
use crate::plan::ir::{Task, TaskName, ValidPlan};
use crate::plan::machine::BlockedReason;

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
    use crate::plan::ir::{Plan, TaskName, ValidPlan};
    use crate::plan::machine::BlockedReason;
    use crate::plan::record::{declared_output, recorded, run_entry};
    use crucible_contract::history::{HistoryEntry, RecordedStatus};
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
}
