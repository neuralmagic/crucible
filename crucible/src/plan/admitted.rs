//! The session event a plan run opens with: the admitted graph, task by task. A viewer that folds
//! a run's session can fold this to show a plan before anything runs.

use crate::plan::ir::{TaskName, ValidPlan};

/// `history_record` is the playbook's record task, which lives on the workflow rather than the
/// plan; a scored loop has none.
pub fn plan_admitted(
    plan: &ValidPlan,
    history_record: Option<&TaskName>,
) -> crucible_contract::session::SessionEvent {
    let p = plan.plan();
    crucible_contract::session::SessionEvent::PlanAdmitted {
        plan_version: p.version,
        reason: p.reason.clone().unwrap_or_default(),
        budget_usd: p.budget.usd,
        tasks: crate::plan::record::wire_tasks(plan),
        history_record: history_record.map(|t| t.0.clone()).unwrap_or_default(),
    }
}
