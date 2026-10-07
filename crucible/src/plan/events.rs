//! Session-log events a plan run emits: the admitted graph and each terminal task result.

use crate::plan::ir::{TaskName, ValidPlan};

/// `history_record` is the playbook's record task, which lives on the workflow rather than the
/// plan; a scored loop has none.
pub(crate) fn plan_admitted_event(
    plan: &ValidPlan,
    history_record: Option<&TaskName>,
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
            })
            .collect(),
        history_record: history_record.map(|t| t.0.clone()).unwrap_or_default(),
    }
}

/// One terminal task result on the wire. `iter` is the loop round (0 for a standalone
/// `plan run`); fields belonging to other emitters stay at their defaults. `trace_id`/`span_id`
/// carry the emitter's current trace context (the iteration's span) so a RESULTS row links
/// straight to its trace; no active span leaves them empty. `args` carries the run's agent knobs
/// a task's own are resolved against; None reports no agent.
pub(crate) fn task_result_event(
    plan_version: u32,
    iter: u32,
    task: &crate::plan::ir::Task,
    r: &crate::plan::exec::TaskResult,
    args: Option<&crate::args::Args>,
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
        secs: 0.0,
        trace_id,
        span_id,
        agent: args.and_then(|args| crate::plan::harness::resolved_agent(args, task)),
        repairs: r.repairs.clone(),
    }
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
            [[task]]
            name = "pick"
            kind = "command"
            command = "true"
            [[task]]
            name = "build"
            kind = "command"
            command = "true"
            depends_on = ["pick"]
            [[task]]
            name = "confirm"
            kind = "command"
            command = "true"
            depends_on = ["build"]
            revise = { tasks = ["pick", "build"], max_rounds = 2 }
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();
        let SessionEvent::PlanAdmitted { tasks, .. } =
            crate::plan::events::plan_admitted_event(&plan, None)
        else {
            panic!("not a plan_admitted event");
        };
        assert_eq!(
            (tasks[0].revise.as_slice(), tasks[0].max_rounds),
            (&[][..], 0)
        );
        assert_eq!(
            (tasks[1].revise.as_slice(), tasks[1].max_rounds),
            (&["author".to_string()][..], 3)
        );
        let confirm = tasks.iter().find(|t| t.name == "confirm").unwrap();
        assert_eq!(
            (confirm.revise.as_slice(), confirm.max_rounds),
            (&["pick".to_string(), "build".to_string()][..], 2)
        );
    }

    #[test]
    fn the_admitted_plan_carries_each_declared_field_and_its_type() {
        use crucible_contract::decision::Label;
        use crucible_contract::emits::{EmitWire, FieldType};
        let plan = Plan::from_toml_str(
            r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "classify"
            kind = "command"
            command = "true"
            emits = { tier = ["high", "low"], score = "number" }
            [[task]]
            name = "legacy"
            kind = "command"
            command = "true"
            emits = ["lines"]
            [[task]]
            name = "bare"
            kind = "command"
            command = "true"
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();
        let SessionEvent::PlanAdmitted { tasks, .. } =
            crate::plan::events::plan_admitted_event(&plan, None)
        else {
            panic!("not a plan_admitted event");
        };
        let emits = |name: &str| {
            tasks
                .iter()
                .find(|t| t.name == name)
                .map(|t| t.emits.clone())
                .unwrap()
        };
        let label = |l: &str| Label::new(l).unwrap();
        assert_eq!(
            emits("classify"),
            [
                EmitWire {
                    field: "score".into(),
                    ty: Some(FieldType::Number),
                },
                EmitWire {
                    field: "tier".into(),
                    ty: Some(FieldType::OneOf(vec![label("high"), label("low")])),
                },
            ]
        );
        assert_eq!(
            emits("legacy"),
            [EmitWire {
                field: "lines".into(),
                ty: None,
            }]
        );
        assert!(emits("bare").is_empty());
        let line =
            crucible_contract::encode(&crate::plan::events::plan_admitted_event(&plan, None));
        assert!(
            line.contains(r#""emits":[{"field":"score","type":"number"},{"field":"tier","type":["high","low"]}]"#),
            "{line}"
        );
    }

    #[test]
    fn the_admitted_plan_carries_each_schema_and_each_declared_file() {
        use crucible_contract::emits::{DeclaredFile, EmitWire, FieldType, JsonSchema};
        let plan = Plan::from_toml_str(
            r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "plan"
            kind = "command"
            command = "true"
            emits = { lanes = { schema = '{"type":"array","items":{"type":"string"}}' } }
            emits_files = ["REPORT.md", { path = "RESULT.json", schema = '{"type":"object"}' }]
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();
        let event = crate::plan::events::plan_admitted_event(&plan, None);
        let SessionEvent::PlanAdmitted { tasks, .. } = &event else {
            panic!("not a plan_admitted event");
        };
        let lanes = JsonSchema::parse(r#"{"type":"array","items":{"type":"string"}}"#).unwrap();
        assert_eq!(
            tasks[0].emits,
            [EmitWire {
                field: "lanes".into(),
                ty: Some(FieldType::Schema(lanes)),
            }]
        );
        assert_eq!(
            tasks[0].emits_files,
            [
                DeclaredFile::from("REPORT.md"),
                DeclaredFile {
                    path: "RESULT.json".into(),
                    schema: Some(JsonSchema::parse(r#"{"type":"object"}"#).unwrap()),
                },
            ]
        );
        let line = crucible_contract::encode(&event);
        let back = crucible_contract::decode(&line).expect("decodes");
        assert_eq!(crucible_contract::encode(&back), line);
    }

    /// The urls a reader renders come off the declared `link`/`links` fields alone, parsed by the
    /// engine that validated them. A url elsewhere in the output is data.
    #[test]
    fn a_task_result_carries_the_links_of_its_declared_link_fields() {
        use crucible_contract::link::{LinkKind, LinkProvider};
        let plan = Plan::from_toml_str(
            r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "deliver"
            kind = "command"
            command = "true"
            emits = { pr = "link", pushed = "links", homepage = "string" }
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();
        let result = crate::plan::exec::TaskResult {
            status: crate::plan::exec::TaskStatus::Pass,
            attempts: 1,
            cost_usd: 0.0,
            output: Some(serde_json::json!({
                "pr": "https://github.com/neuralmagic/crucible/pull/42",
                "pushed": ["https://github.com/neuralmagic/crucible/tree/topic"],
                "homepage": "https://example.com/not-a-result",
            })),
            note: None,
            fanout: None,
            blocked: None,
            transport: None,
            repairs: Vec::new(),
        };
        let SessionEvent::TaskResult { links, .. } =
            crate::plan::events::task_result_event(1, 0, &plan.plan().tasks[0], &result, None)
        else {
            panic!("not a task_result event");
        };
        assert_eq!(links.len(), 2, "the string field contributed nothing");
        assert_eq!(links[0].provider, LinkProvider::GitHub);
        assert_eq!(links[0].kind, LinkKind::PullRequest);
        assert_eq!(links[0].label, "#42");
        assert_eq!(links[1].kind, LinkKind::Branch);
        assert_eq!(links[1].label, "topic");

        let none = crate::plan::exec::TaskResult {
            output: None,
            ..result
        };
        let SessionEvent::TaskResult { links, .. } =
            crate::plan::events::task_result_event(1, 0, &plan.plan().tasks[0], &none, None)
        else {
            panic!("not a task_result event");
        };
        assert!(links.is_empty(), "no output, no links");
    }

    #[test]
    fn a_task_result_carries_its_repair_turns() {
        let plan = Plan::from_toml_str(
            "version = 1\n[budget]\nusd = 1.0\n[[task]]\nname = \"plan\"\nkind = \"agent\"\nprompt = \"p\"\nrepair = 2\n",
        )
        .unwrap()
        .validate()
        .unwrap();
        let repair = crucible_contract::session::TaskRepair {
            label: "plan repair 1/2".into(),
            round: 1,
            of: 2,
            cost_usd: 0.25,
            notes: vec!["output missing declared field \"lanes\"".into()],
        };
        let result = crate::plan::exec::TaskResult {
            status: crate::plan::exec::TaskStatus::Pass,
            attempts: 1,
            cost_usd: 0.75,
            output: Some(serde_json::json!({"lanes": []})),
            note: None,
            fanout: None,
            blocked: None,
            transport: None,
            repairs: vec![repair.clone()],
        };
        let event =
            crate::plan::events::task_result_event(1, 0, &plan.plan().tasks[0], &result, None);
        let SessionEvent::TaskResult { repairs, .. } = &event else {
            panic!("not a task_result event");
        };
        assert_eq!(repairs, &[repair]);
        let line = crucible_contract::encode(&event);
        assert!(line.contains(r#""label":"plan repair 1/2""#), "{line}");
    }

    #[test]
    fn the_admitted_plan_states_each_tasks_own_time_limit() {
        let plan = Plan::from_toml_str(
            r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "build"
            kind = "command"
            command = "true"
            timeout = "90m"
            [[task]]
            name = "check"
            kind = "command"
            command = "true"
            depends_on = ["build"]
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();
        let SessionEvent::PlanAdmitted { tasks, .. } =
            crate::plan::events::plan_admitted_event(&plan, None)
        else {
            panic!("not a plan_admitted event");
        };
        assert_eq!(tasks[0].timeout, "90m");
        assert_eq!(tasks[1].timeout, "");
    }

    /// What a run writes is what a resume reads back: every row the executor settles, encoded as
    /// the session log carries it, folds to the same result.
    #[test]
    fn every_settled_row_folds_back_to_the_result_it_was_written_from() {
        use crate::plan::exec::{
            Attempt, AttemptOutcome, ExecCfg, Substrate, TaskResult, TaskRunner, TransportFailure,
            execute,
        };
        use crate::plan::ir::{Task, TaskName};
        use std::collections::BTreeMap;

        struct Scripted;
        impl TaskRunner for Scripted {
            fn run(
                &mut self,
                task: &Task,
                attempt: u32,
                _inputs: &BTreeMap<TaskName, serde_json::Value>,
                _deadline: Option<crucible::deadline::Deadline>,
            ) -> Attempt {
                let outcome = match (task.name.0.as_str(), attempt) {
                    ("discover", _) => {
                        AttemptOutcome::Pass(serde_json::json!({"targets": ["a", "b"]}))
                    }
                    ("audit[b]", _) => AttemptOutcome::Fail {
                        note: "b is broken".into(),
                        output: Some(serde_json::json!({"why": "b"})),
                    },
                    ("flaky", _) => AttemptOutcome::Transport(TransportFailure::new(
                        crucible_contract::TransportCause::Gateway,
                        "gateway down",
                    )),
                    _ => AttemptOutcome::Pass(serde_json::json!({"ok": true})),
                };
                Attempt {
                    outcome,
                    cost_usd: 0.25,
                    repairs: Vec::new(),
                }
            }
        }

        let plan = Plan::from_toml_str(
            r#"
            version = 1
            [budget]
            usd = 10.0
            [[task]]
            name = "discover"
            kind = "command"
            command = "true"
            [[task]]
            name = "audit"
            kind = "command"
            command = "true"
            depends_on = ["discover"]
            over = { task = "discover", field = "targets" }
            max_fanout = 4
            required = false
            [[task]]
            name = "after"
            kind = "command"
            command = "true"
            depends_on = ["audit"]
            required = false
            [[task]]
            name = "flaky"
            kind = "command"
            command = "true"
            required = false
            [[task]]
            name = "report"
            kind = "command"
            command = "true"
            stage = "epilogue"
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();
        let mut log = crucible_contract::session::encode(
            &crate::plan::events::plan_admitted_event(&plan, None),
        );
        let mut written: Vec<(TaskName, TaskResult)> = Vec::new();
        execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut Scripted,
            |task, result| {
                log.push('\n');
                log.push_str(&crucible_contract::session::encode(
                    &crate::plan::events::task_result_event(1, 0, task, result, None),
                ));
                written.push((task.name.clone(), result.clone()));
            },
        )
        .unwrap();
        let kinds: Vec<&str> = written.iter().map(|(_, r)| r.status.as_str()).collect();
        for kind in ["pass", "fail", "blocked", "transport"] {
            assert!(
                kinds.contains(&kind),
                "the run settles a {kind} row: {kinds:?}"
            );
        }
        assert!(written.iter().any(|(_, r)| r.fanout.is_some()));

        let prior =
            crate::plan::resume::fold(&log, &plan, std::time::Duration::ZERO).expect("folds");
        assert_eq!(
            prior.order,
            written.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
        );
        for (name, result) in &written {
            assert_eq!(&prior.results[name], result, "{name}");
        }
        assert!(prior.admitted);
        assert!(!prior.shut_down);
        assert!(!prior.recorded_history);
    }
}
