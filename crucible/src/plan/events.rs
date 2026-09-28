//! Session-log events a plan run emits: the admitted graph and each terminal task result.

use crate::plan::ir::ValidPlan;

/// `max_asks` is the run's bound on asks as the launcher set it; 0 where the lane admits none.
pub(crate) fn plan_admitted_event(
    plan: &ValidPlan,
    max_asks: u32,
) -> crate::report::session::SessionEvent {
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
                when: t.when.as_ref().map(ToString::to_string).unwrap_or_default(),
                revise: t
                    .revise
                    .as_ref()
                    .map(|r| r.task.0.clone())
                    .unwrap_or_default(),
                max_rounds: t.revise.as_ref().map_or(0, |r| r.max_rounds),
                asks: t.asks.clone(),
            })
            .collect(),
        max_asks,
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
        blocked: r
            .blocked
            .as_ref()
            .map(crate::plan::machine::BlockedReason::wire),
        transport: r.transport,
        secs: 0.0,
        trace_id,
        span_id,
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::ir::Plan;
    use crate::report::session::SessionEvent;

    #[test]
    fn the_admitted_plan_names_each_revise_loop_and_its_bound() {
        let plan = Plan::from_toml_str(
            r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "author"
            kind = "command"
            command = "true"
            [[task]]
            name = "repro"
            kind = "command"
            command = "true"
            depends_on = ["author"]
            revise = { task = "author", max_rounds = 3 }
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();
        let SessionEvent::PlanAdmitted { tasks, .. } =
            crate::plan::events::plan_admitted_event(&plan, 0)
        else {
            panic!("not a plan_admitted event");
        };
        assert_eq!((tasks[0].revise.as_str(), tasks[0].max_rounds), ("", 0));
        assert_eq!(
            (tasks[1].revise.as_str(), tasks[1].max_rounds),
            ("author", 3)
        );
    }

    #[test]
    fn the_admitted_plan_carries_each_tasks_asks_and_the_runs_bound() {
        let plan = Plan::from_toml_str(
            r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "scan"
            kind = "command"
            command = "true"
            [[task]]
            name = "roundup"
            kind = "command"
            command = "true"
            depends_on = ["scan"]
            asks = ["issue-fix"]
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();
        let event = crate::plan::events::plan_admitted_event(&plan, 7);
        let SessionEvent::PlanAdmitted {
            tasks, max_asks, ..
        } = &event
        else {
            panic!("not a plan_admitted event");
        };
        assert_eq!(*max_asks, 7);
        assert!(tasks[0].asks.is_empty());
        assert_eq!(
            tasks[1].asks.iter().map(|w| w.as_str()).collect::<Vec<_>>(),
            ["issue-fix"]
        );
        let line = crate::report::session::encode(&event);
        assert!(line.contains("\"max_asks\":7"), "{line}");
        assert!(line.contains("\"asks\":[\"issue-fix\"]"), "{line}");
    }
}
