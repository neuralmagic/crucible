//! The per-iteration plan an autoresearch workflow runs: the authored graph, or the default
//! propose, apply, measure, decide chain with any legacy splice tasks between propose and apply.

use anyhow::{Context, Result};

use crate::plan::ir::{
    EngineOp, Join, Plan, PlanBudget, Stage, Task, TaskKind, TaskName, ValidPlan, Workspace,
};
use crate::plan::workflow::{WorkflowCaps, WorkflowCfg, WorkflowType};

/// Build and admit the default or authored iteration graph.
pub(crate) fn iteration_template(
    workflow: Option<&WorkflowCfg>,
    caps: &WorkflowCaps,
) -> Result<ValidPlan> {
    if let Some(workflow) = workflow.filter(|workflow| !workflow.is_legacy_splice()) {
        workflow
            .admit(caps)
            .context("admitting authored workflow into the autoresearch loop")?;
        return Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: f64::MAX },
            tasks: workflow.iteration_tasks(),
        }
        .validate()
        .context("building authored iteration workflow");
    }

    let engine =
        |name: &str, op: EngineOp, source: Option<TaskName>, deps: Vec<TaskName>| -> Task {
            Task {
                name: name.into(),
                task: TaskKind::Engine {
                    op,
                    source,
                    tiebreak: None,
                },
                depends_on: deps,
                session: None,
                needs: "any".to_string(),
                required: true,
                workspace: Workspace::Shared,
                join: Join::default(),
                stage: Stage::Iteration,
                emits: Vec::new(),
                emits_files: Vec::new(),
                over: None,
                max_fanout: None,
                when: None,
                revise: None,
            }
        };
    let mut tasks = vec![engine("propose", EngineOp::Propose, None, vec![])];

    // Legacy splice tasks run between `propose` and `apply`; `apply` waits on every sink.
    // Epilogue tasks never splice: they run once post-loop, not per iteration.
    let mut apply_deps = vec![TaskName("propose".to_string())];
    if let Some(w) = workflow.filter(|w| !w.tasks.is_empty()) {
        for mut t in w.iteration_tasks() {
            if t.depends_on.is_empty() {
                t.depends_on = vec![TaskName("propose".to_string())];
            }
            tasks.push(t);
        }
        let sinks = w.sinks();
        if !sinks.is_empty() {
            apply_deps = sinks;
        }
    }

    tasks.push(engine("apply", EngineOp::Apply, None, apply_deps));
    tasks.push(engine(
        "measure",
        EngineOp::Measure,
        None,
        vec![TaskName("apply".to_string())],
    ));
    tasks.push(engine(
        "decide",
        EngineOp::Decide,
        Some(TaskName("measure".to_string())),
        vec![TaskName("measure".to_string())],
    ));
    let workflow = WorkflowCfg {
        workflow_type: WorkflowType::Autoresearch,
        result: Some("decide".into()),
        tasks,
        file: None,
        resolved_from: None,
    };
    workflow
        .admit(caps)
        .context("admitting the default autoresearch workflow")?;
    Plan {
        version: 1,
        reason: None,
        budget: PlanBudget { usd: f64::MAX },
        tasks: workflow.tasks,
    }
    .validate()
    .context("building the iteration template")
}

#[cfg(test)]
mod tests {
    use crate::plan::template::*;

    #[test]
    fn pack_tasks_splice_between_the_turn_and_the_gate() {
        let w: WorkflowCfg = toml::from_str(
            "[[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\n             [[task]]\nname = \"lint\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"review\"]\n",
        )
        .unwrap();
        w.validate().unwrap();
        let plan = iteration_template(Some(&w), &WorkflowCaps::autoresearch_engine()).unwrap();
        let names: Vec<&str> = plan.tasks_topo().map(|t| t.name.0.as_str()).collect();
        assert_eq!(
            names,
            ["propose", "review", "lint", "apply", "measure", "decide"]
        );
        let dep = |n: &str| {
            plan.get(&n.into())
                .unwrap()
                .depends_on
                .iter()
                .map(|d| d.0.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            dep("review"),
            ["propose"],
            "an unattached task hangs off propose"
        );
        assert_eq!(
            dep("apply"),
            ["lint"],
            "apply waits on the sink, not on propose"
        );
        assert_eq!(dep("measure"), ["apply"]);
        assert_eq!(dep("decide"), ["measure"]);
    }

    #[test]
    fn template_is_the_canonical_chain() {
        let plan = iteration_template(None, &WorkflowCaps::autoresearch_engine()).unwrap();
        let names: Vec<&str> = plan.tasks_topo().map(|t| t.name.0.as_str()).collect();
        assert_eq!(names, ["propose", "apply", "measure", "decide"]);
        assert!(plan.tasks_topo().all(|t| t.required));
        let kinds: Vec<&str> = plan.tasks_topo().map(|t| t.task.label()).collect();
        assert_eq!(
            kinds,
            [
                "engine_propose",
                "engine_apply",
                "engine_measure",
                "engine_decide"
            ]
        );
    }

    #[test]
    fn authored_autoresearch_uses_semantics_instead_of_reserved_names() {
        let workflow: WorkflowCfg = toml::from_str(
            "type = \"autoresearch\"\nresult = \"keep-if-better\"\n\
             [[task]]\nname = \"invent\"\nkind = \"engine\"\nop = \"propose\"\n\
             [[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"invent\"]\n\
             [[task]]\nname = \"deploy-preview\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"review\"]\n\
             [[task]]\nname = \"benchmark-a\"\nkind = \"engine\"\nop = \"measure\"\ndepends_on = [\"deploy-preview\"]\n\
             [[task]]\nname = \"explain-score\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"benchmark-a\"]\n\
             [[task]]\nname = \"keep-if-better\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"benchmark-a\"\ndepends_on = [\"benchmark-a\", \"explain-score\"]\n",
        )
        .unwrap();
        let plan =
            iteration_template(Some(&workflow), &WorkflowCaps::autoresearch_engine()).unwrap();
        let names: Vec<&str> = plan.tasks_topo().map(|task| task.name.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "invent",
                "review",
                "deploy-preview",
                "benchmark-a",
                "explain-score",
                "keep-if-better"
            ]
        );
    }
}
