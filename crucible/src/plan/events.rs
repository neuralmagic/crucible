//! Session-log events a plan run emits: the admitted graph and each terminal task result.

use crate::plan::ir::ValidPlan;

pub(crate) fn plan_admitted_event(plan: &ValidPlan) -> crate::report::session::SessionEvent {
    let p = plan.plan();
    crate::report::session::SessionEvent::PlanAdmitted {
        plan_version: p.version,
        reason: p.reason.clone().unwrap_or_default(),
        budget_usd: p.budget.usd,
        tasks: plan
            .tasks_topo()
            .map(|t| crate::report::session::PlanTaskWire {
                name: t.name.0.clone(),
                kind: t.task.label().to_string(),
                depends_on: t.depends_on.iter().map(|d| d.0.clone()).collect(),
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
            })
            .collect(),
    }
}

/// One terminal task result on the wire. `iter` is the loop round (0 for a standalone
/// `plan run`); fields belonging to other emitters stay at their defaults. `trace_id`/`span_id`
/// carry the emitter's current trace context (the iteration's span) so a RESULTS row links
/// straight to its trace; no active span leaves them empty.
pub(crate) fn task_result_event(
    plan_version: u32,
    iter: u32,
    task: &crate::plan::ir::Task,
    r: &crate::plan::exec::TaskResult,
) -> crate::report::session::SessionEvent {
    let (trace_id, span_id) = crate::agent::engine::current_trace_env()
        .and_then(|(tp, _)| {
            let f: Vec<&str> = tp.split('-').collect();
            match f.as_slice() {
                [_, tid, sid, ..] => Some((tid.to_string(), sid.to_string())),
                _ => None,
            }
        })
        .unwrap_or_default();
    crate::report::session::SessionEvent::TaskResult {
        task: task.name.0.clone(),
        status: r.status.as_str().to_string(),
        plan_version,
        task_kind: task.task.label().to_string(),
        iter,
        digest: String::new(),
        job: String::new(),
        attempts: r.attempts,
        cost_usd: r.cost_usd,
        metric: None,
        output: r.output.clone(),
        note: r.note.clone().unwrap_or_default(),
        secs: 0.0,
        trace_id,
        span_id,
    }
}
