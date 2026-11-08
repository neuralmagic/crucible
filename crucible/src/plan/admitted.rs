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
    let definitions = crate::plan::rerun::definition_digests(plan);
    crucible_contract::session::SessionEvent::PlanAdmitted {
        plan_version: p.version,
        reason: p.reason.clone().unwrap_or_default(),
        budget_usd: p.budget.usd,
        tasks: plan
            .tasks_topo()
            .map(|t| crucible_contract::session::PlanTaskWire {
                name: t.name.0.clone(),
                kind: t.task.label().to_string(),
                depends_on: t.depends_on.iter().map(|d| d.0.clone()).collect(),
                optional: t.optional.iter().map(|d| d.0.clone()).collect(),
                session: t.session.clone().unwrap_or_default(),
                needs: t.needs.clone(),
                required: t.required,
                join: t.join.as_str().to_string(),
                stage: t.stage.as_str().to_string(),
                over: t
                    .over
                    .as_ref()
                    .map(crate::plan::ir::OutputRef::to_string)
                    .unwrap_or_default(),
                max_fanout: t.max_fanout.unwrap_or_default(),
                when: t.when.as_ref().map(ToString::to_string).unwrap_or_default(),
                keyed: t.keyed.iter().map(ToString::to_string).collect(),
                revise: t
                    .revise
                    .as_ref()
                    .map(|r| r.tasks.iter().map(|task| task.0.clone()).collect())
                    .unwrap_or_default(),
                max_rounds: t.revise.as_ref().map_or(0, |r| r.max_rounds),
                emits: t
                    .emits
                    .fields()
                    .into_iter()
                    .map(|(field, ty)| crucible_contract::emits::EmitWire {
                        field: field.0.clone(),
                        ty: ty.cloned(),
                    })
                    .collect(),
                emits_files: t.emits_files.clone(),
                timeout: t
                    .timeout
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                history_depth: t.history.unwrap_or_default(),
                definition: definitions.get(&t.name).cloned().unwrap_or_default(),
            })
            .collect(),
        history_record: history_record.map(|t| t.0.clone()).unwrap_or_default(),
    }
}
