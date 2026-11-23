//! What `crucible-runner` refuses of a pack. The runner refuses it before it sets up a
//! workspace, and `crucible check --engine runner` reports the same list as findings.

use crate::plan::ir::{ModelAsk, NEEDS_HUMAN, Task, TaskName};
use crate::plan::workflow::WorkflowCfg;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error(
        "task {task} asks {ask}, and crucible-runner calls no model: it runs command, evaluate \
         and report tasks and routes decided by a task's output. Run this pack on the full engine"
    )]
    AsksModel { task: TaskName, ask: ModelAsk },
    #[error(
        "task {task} is required and needs a person's decision, and crucible-runner opens no \
         decision desk. Run this pack on the full engine"
    )]
    NeedsHuman { task: TaskName },
    #[error(
        "the pack names [workflow].servers ({servers}), which a task reaches with a token \
         crucible-runner does not mint. Run this pack on the full engine"
    )]
    WorkflowServers { servers: String },
}

/// Why the runner will not run `task`, if it will not.
pub fn task_refusal(task: &Task) -> Option<Refusal> {
    if let Some(ask) = task.task.model_ask() {
        return Some(Refusal::AsksModel {
            task: task.name.clone(),
            ask,
        });
    }
    (task.required && task.needs == NEEDS_HUMAN).then(|| Refusal::NeedsHuman {
        task: task.name.clone(),
    })
}

/// Every refusal among `tasks`, in their order.
pub fn task_refusals<'a>(
    tasks: impl IntoIterator<Item = &'a Task> + 'a,
) -> impl Iterator<Item = Refusal> + 'a {
    tasks.into_iter().filter_map(task_refusal)
}

/// Every refusal of `workflow`: its tasks', then its servers.
pub fn workflow_refusals(workflow: &WorkflowCfg) -> Vec<Refusal> {
    let servers = (!workflow.servers.is_empty()).then(|| Refusal::WorkflowServers {
        servers: workflow
            .servers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    });
    task_refusals(&workflow.tasks).chain(servers).collect()
}

#[cfg(test)]
mod tests {
    use crate::plan::refusal::*;
    use crate::plan::starlark::compile_launch_file;
    use std::collections::BTreeMap;

    fn compiled(source: &str) -> WorkflowCfg {
        let dir = tempfile::tempdir().expect("scratch");
        let file = dir.path().join("workflow.star");
        std::fs::write(&file, source).expect("workflow");
        compile_launch_file(&file, dir.path(), &BTreeMap::new())
            .expect("compiles")
            .workflow
    }

    const EVERY_REFUSAL: &str = r#"
read = command(name = "read", run = "echo '{\"ticket\": \"down\"}'", emits = ["ticket"])
think = agent(name = "think", prompt = "decide", depends_on = [read])
gate = route(
    name = "gate",
    depends_on = [read],
    decider = model(min_confidence = 0.8),
    questions = {"urgent": noul(ask = "Is it urgent?", drop = ["no", "uncertain"])},
)
sign = route(
    name = "sign",
    depends_on = [read],
    decider = human(),
    questions = {"ship": noul(ask = "Ship it?", drop = ["no", "uncertain"])},
)
workflow(type = "playbook", tasks = [read, think, gate, sign])
"#;

    #[test]
    fn every_refusal_of_a_workflow_is_named_in_task_order_with_servers_last() {
        let mut workflow = compiled(EVERY_REFUSAL);
        workflow.servers = ["tracker", "wiki"]
            .iter()
            .map(|name| {
                crucible_contract::agent_servers::ServerName::parse(name).expect("a server name")
            })
            .collect();
        assert_eq!(
            workflow_refusals(&workflow),
            vec![
                Refusal::AsksModel {
                    task: TaskName("think".into()),
                    ask: ModelAsk::AgentTurn,
                },
                Refusal::AsksModel {
                    task: TaskName("gate".into()),
                    ask: ModelAsk::DecisionRoute,
                },
                Refusal::NeedsHuman {
                    task: TaskName("sign".into()),
                },
                Refusal::WorkflowServers {
                    servers: "tracker, wiki".into(),
                },
            ]
        );
    }

    #[test]
    fn a_human_decision_the_plan_does_not_require_is_not_refused() {
        let mut workflow = compiled(EVERY_REFUSAL);
        let sign = workflow
            .tasks
            .iter_mut()
            .find(|task| task.name.0 == "sign")
            .expect("the human route");
        assert_eq!(
            task_refusal(sign),
            Some(Refusal::NeedsHuman {
                task: TaskName("sign".into()),
            })
        );
        sign.required = false;
        assert_eq!(task_refusal(sign), None);
    }

    #[test]
    fn a_command_playbook_with_an_output_decided_route_has_no_refusal() {
        let workflow = compiled(
            r#"
read = command(name = "read", run = "echo '{\"urgent\": true}'", emits = {"urgent": "boolean"})
gate = route(
    name = "gate",
    depends_on = [read],
    decider = output(read),
    questions = {"urgent": noul(ask = "Is it urgent?", drop = ["no", "uncertain"])},
)
page = command(name = "page", run = "true", when = gate.urgent)
workflow(type = "playbook", tasks = [read, gate, page])
"#,
        );
        assert_eq!(workflow_refusals(&workflow), Vec::new());
    }

    #[test]
    fn each_refusal_says_what_the_runner_lacks() {
        let task = TaskName("t".into());
        let said = |refusal: Refusal| refusal.to_string();
        assert!(
            said(Refusal::AsksModel {
                task: task.clone(),
                ask: ModelAsk::ProposeTurn,
            })
            .starts_with(
                "task t asks the scored loop's propose turn, and crucible-runner calls no model"
            )
        );
        assert!(
            said(Refusal::NeedsHuman { task })
                .starts_with("task t is required and needs a person's decision")
        );
        assert!(
            said(Refusal::WorkflowServers {
                servers: "tracker".into(),
            })
            .starts_with("the pack names [workflow].servers (tracker)")
        );
    }
}
