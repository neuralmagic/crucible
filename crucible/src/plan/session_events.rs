//! Session-log events a plan run emits: the admitted graph and each terminal task result.

pub use crate::plan::admitted::plan_admitted;
use crucible_contract::session::{SessionEvent, TaskAgent};

/// One terminal task result on the wire. `iter` is the loop round (0 for a standalone
/// `plan run`); fields belonging to other emitters stay at their defaults. `trace` is the
/// emitter's current trace and span, so a RESULTS row links straight to its trace. `agent` is
/// what the task's attempts ran on; None reports no agent. `secs` is the task's wall time.
pub fn task_result(
    plan_version: u32,
    iter: u32,
    task: &crate::plan::ir::Task,
    r: &crate::plan::exec::TaskResult,
    agent: Option<TaskAgent>,
    trace: Option<(String, String)>,
    secs: f64,
) -> SessionEvent {
    let (trace_id, span_id) = trace.unwrap_or_default();
    SessionEvent::TaskResult {
        task: task.name.0.clone(),
        status: r.status,
        plan_version,
        task_kind: task.task.label().to_string(),
        iter,
        digest: String::new(),
        job: String::new(),
        attempts: r.attempts,
        cost_usd: r.cost_usd,
        metric: None,
        output: r.output.clone(),
        links: r
            .output
            .as_ref()
            .map(|output| reported_links(&task.emits, output))
            .unwrap_or_default(),
        note: r.note.clone().unwrap_or_default(),
        blocked: r
            .blocked
            .as_ref()
            .map(crate::plan::machine::BlockedReason::wire),
        transport: r.transport,
        secs,
        at: Some(crate::plan::admission::stamp()),
        trace_id,
        span_id,
        agent: agent.map(Box::new),
        repairs: r.repairs.clone(),
        reused_from: None,
    }
}

/// A row a rerun reused rather than dispatched, naming the run that settled it.
pub fn reused_result(
    plan_version: u32,
    task: &crate::plan::ir::Task,
    r: &crate::plan::exec::TaskResult,
    from: &str,
) -> SessionEvent {
    let mut event = task_result(plan_version, 0, task, r, None, None, 0.0);
    if let SessionEvent::TaskResult { reused_from, .. } = &mut event {
        *reused_from = Some(from.to_string());
    }
    event
}

/// The external results an output carries, read only out of the fields declared `link`/`links`.
/// A url anywhere else in the output is data, not a reported result.
fn reported_links(
    emits: &crate::plan::ir::Emits,
    output: &serde_json::Value,
) -> Vec<crucible_contract::link::ExternalLink> {
    emits
        .fields()
        .into_iter()
        .filter_map(|(field, ty)| Some((output.get(&field.0)?, ty?)))
        .flat_map(|(value, ty)| ty.links(value))
        .collect()
}
