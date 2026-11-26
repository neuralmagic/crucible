//! Compiling a pack for what calls need of it: a target's interface, the
//! call tasks of a calling pack, and a finished playbook run's verdict (RFC-0002
//! C-PLAYBOOK-LANE).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context as _;
use crucible_contract::calls::{
    CallAlias, NodeDetail, NodeStatus, ProgressEdge, ProgressGraph, ProgressNode, TargetInterface,
};
use crucible_contract::emits::{DeclaredFile, EmitWire};
use crucible_contract::session::{SessionEvent, TaskStatus};
use serde::Deserialize;
use serde_json::Value;

use crate::calls::CallError;

/// A compiled pack: its interface, the task whose output is its result, and its call tasks.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Compiled {
    pub interface: TargetInterface,
    pub result: Option<String>,
    pub calls: BTreeMap<String, CallTask>,
}

/// A call task as the compiled plan declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallTask {
    pub alias: CallAlias,
    pub mapped: bool,
}

#[derive(Deserialize)]
struct Plan {
    #[serde(default)]
    result: Option<String>,
    #[serde(default, rename = "task")]
    tasks: Vec<PlanTask>,
}

#[derive(Deserialize)]
struct PlanTask {
    name: String,
    kind: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    emits: crucible::plan::ir::Emits,
    #[serde(default)]
    emits_files: Vec<DeclaredFile>,
    #[serde(default)]
    alias: Option<CallAlias>,
    #[serde(default)]
    over: Option<Value>,
}

pub(crate) fn compile(
    root: &Path,
    variant: Option<&crucible::manifest::variant::VariantName>,
) -> Result<Compiled, String> {
    let source = crate::playbooks::registry::workflow_source_for(
        root,
        crate::playbooks::registry::PackWorkflowKind::Playbook,
        variant,
    )
    .map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(&source)
        .map_err(|e| format!("reading {}: {e}", source.display()))?;
    let schema = crucible::plan::starlark::declared_params(&text, &source)
        .map_err(|e| crucible::errors::report(&e))?;
    let values = crate::playbooks::preview::effective(
        Some(&schema),
        &BTreeMap::new(),
        crate::playbooks::preview::Unvalued::Placeholder,
    );
    let compiled = crucible::plan::starlark::compile_file_with(
        &source,
        crucible::plan::starlark::parent_or_cwd(&source),
        &values,
    )
    .map_err(|e| crucible::errors::report(&e))?;
    let plan: Plan = serde_json::from_str(&compiled.canonical_json)
        .map_err(|e| format!("the engine's compiled plan did not parse: {e}"))?;
    Ok(compiled_of(schema, plan))
}

fn compiled_of(params: Value, plan: Plan) -> Compiled {
    let result_task = plan
        .result
        .as_deref()
        .and_then(|r| plan.tasks.iter().find(|t| t.name == r));
    let result = result_task
        .map(|t| {
            t.emits
                .fields()
                .into_iter()
                .map(|(field, ty)| EmitWire {
                    field: field.0.clone(),
                    ty: ty.cloned(),
                })
                .collect()
        })
        .unwrap_or_default();
    let result_files = result_task
        .map(|t| t.emits_files.clone())
        .unwrap_or_default();
    let edges = plan
        .tasks
        .iter()
        .flat_map(|t| {
            t.depends_on.iter().map(|dep| ProgressEdge {
                from: dep.clone(),
                to: t.name.clone(),
            })
        })
        .collect();
    let nodes = plan
        .tasks
        .iter()
        .map(|t| ProgressNode {
            id: t.name.clone(),
            label: t.name.clone(),
            status: NodeStatus::Pending,
            group: None,
            started: None,
            ended: None,
            detail: Some(NodeDetail::PlaybookTask {
                task_kind: t.kind.clone(),
            }),
            link: None,
        })
        .collect();
    let calls = plan
        .tasks
        .iter()
        .filter(|t| t.kind == "call")
        .filter_map(|t| {
            Some((
                t.name.clone(),
                CallTask {
                    alias: t.alias.clone()?,
                    mapped: t.over.as_ref().is_some_and(|o| !o.is_null()),
                },
            ))
        })
        .collect();
    Compiled {
        interface: TargetInterface {
            params,
            result,
            result_files,
            graph: ProgressGraph::bounded(nodes, edges),
        },
        result: plan.result,
        calls,
    }
}

pub(crate) async fn compile_tree(
    tree: crucible_contract::pack_tree::PackTree,
    variant: Option<crucible::manifest::variant::VariantName>,
) -> Result<Compiled, CallError> {
    tokio::task::spawn_blocking(move || {
        let scratch = tempfile::tempdir().context("a scratch dir for the pack")?;
        let root = scratch.path().join("pack");
        crate::playbooks::packs::write_checked_tree(&tree, &root)?;
        Ok::<_, anyhow::Error>(compile(&root, variant.as_ref()))
    })
    .await
    .context("joining the pack compile")??
    .map_err(|e| CallError::Refused(format!("the pack does not compile: {e}")))
}

/// A finished playbook run's verdict (RFC-0002 C-PLAYBOOK-LANE), read from its session log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Valid,
    Invalid(String),
}

pub(crate) fn verdict(session: &str) -> Verdict {
    let mut required: Vec<String> = Vec::new();
    let mut settled: BTreeMap<String, TaskStatus> = BTreeMap::new();
    let mut outcome: Option<String> = None;
    for event in session
        .lines()
        .filter_map(crucible_contract::session::decode)
    {
        match event {
            SessionEvent::PlanAdmitted { tasks, .. } => {
                required = tasks
                    .into_iter()
                    .filter(|t| t.required && t.stage != "epilogue")
                    .map(|t| t.name)
                    .collect();
            }
            SessionEvent::TaskResult { task, status, .. } => {
                settled.insert(task, status);
            }
            SessionEvent::Shutdown { outcome: o, .. } => outcome = Some(o),
            _ => {}
        }
    }
    let Some(outcome) = outcome else {
        return Verdict::Invalid("the run recorded no shutdown".to_string());
    };
    if outcome == "budget" {
        return Verdict::Invalid("the run exhausted a ceiling".to_string());
    }
    let complete = outcome == "complete";
    for task in &required {
        match settled.get(task) {
            Some(TaskStatus::Pass | TaskStatus::NotTaken | TaskStatus::Skipped) => {}
            Some(status) => {
                return Verdict::Invalid(format!(
                    "required task {task} settled {}",
                    status.as_str()
                ));
            }
            None if complete => {}
            None => {
                return Verdict::Invalid(format!(
                    "required task {task} never settled (the run ended {outcome})"
                ));
            }
        }
    }
    Verdict::Valid
}

#[cfg(test)]
mod tests {
    use crate::calls::compile::*;
    use crate::testing::fixtures::write_playbook_pack;

    #[test]
    fn a_pack_compiles_to_its_params_result_and_graph() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = write_playbook_pack(
            dir.path(),
            "params = {\"pr\": {\"type\": \"int\", \"required\": True}}\n\
             scan = command(name = \"scan\", run = \"true\")\n\
             report = command(name = \"report\", run = \"true\", depends_on = [scan], \
             emits = {\"verdict\": \"string\"}, emits_files = [\"report.md\"])\n\
             workflow(type = \"playbook\", tasks = [scan, report], result = report)\n",
        );
        let compiled = compile(&root, None).expect("compiles");
        assert_eq!(compiled.result.as_deref(), Some("report"));
        assert!(compiled.calls.is_empty());
        let interface = &compiled.interface;
        assert!(
            interface.params["properties"]["pr"].is_object(),
            "{}",
            interface.params
        );
        assert_eq!(interface.result.len(), 1);
        assert_eq!(interface.result[0].field, "verdict");
        assert!(interface.result[0].ty.is_some());
        assert_eq!(interface.result_files[0].path, "report.md");
        let ids: Vec<&str> = interface
            .graph
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(ids, ["scan", "report"]);
        assert!(
            interface
                .graph
                .nodes
                .iter()
                .all(|n| n.status == NodeStatus::Pending)
        );
        assert_eq!(interface.graph.edges.len(), 1);
        assert_eq!(
            compile(&root, None).expect("again").interface.digest().ok(),
            interface.digest().ok(),
            "the same pack compiles to the same interface"
        );
    }

    #[test]
    fn a_pack_with_no_result_has_an_empty_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = write_playbook_pack(
            dir.path(),
            "a = command(name = \"a\", run = \"true\")\n\
             workflow(type = \"playbook\", tasks = [a])\n",
        );
        let compiled = compile(&root, None).expect("compiles");
        assert_eq!(compiled.result, None);
        assert!(compiled.interface.result.is_empty());
        assert!(compiled.interface.result_files.is_empty());
    }

    #[test]
    fn a_plans_call_tasks_carry_their_alias_and_whether_they_map() {
        let plan: Plan = serde_json::from_value(serde_json::json!({
            "task": [
                {"name": "prep", "kind": "command", "run": "true"},
                {"name": "ask", "kind": "call", "alias": "child", "target": "playbook"},
                {"name": "each", "kind": "call", "alias": "child", "target": "playbook",
                 "over": {"task": "prep", "field": "items"}, "max_fanout": 4},
            ]
        }))
        .expect("plan");
        let compiled = compiled_of(serde_json::json!({}), plan);
        let child = CallAlias::parse("child").expect("alias");
        assert_eq!(
            compiled.calls.get("ask"),
            Some(&CallTask {
                alias: child.clone(),
                mapped: false
            })
        );
        assert_eq!(
            compiled.calls.get("each"),
            Some(&CallTask {
                alias: child,
                mapped: true
            })
        );
        assert!(!compiled.calls.contains_key("prep"));
    }

    fn session(lines: &[&str]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    const PLAN: &str = r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{"name":"a","kind":"command","required":true},{"name":"b","kind":"command","required":true},{"name":"tip","kind":"command","required":false},{"name":"wrap","kind":"command","required":true,"stage":"epilogue"}]}"#;

    #[test]
    fn a_run_is_valid_only_when_its_required_main_graph_passed() {
        let pass =
            |t: &str| format!(r#"{{"v":1,"kind":"task_result","task":"{t}","status":"pass"}}"#);
        let fail =
            |t: &str| format!(r#"{{"v":1,"kind":"task_result","task":"{t}","status":"fail"}}"#);
        let shutdown =
            |o: &str| format!(r#"{{"v":1,"kind":"shutdown","outcome":"{o}","reason":""}}"#);
        assert_eq!(
            verdict(&session(&[
                PLAN,
                &pass("a"),
                &pass("b"),
                &fail("tip"),
                &fail("wrap"),
                &shutdown("finished")
            ])),
            Verdict::Valid,
            "advisory and epilogue failures leave the verdict alone"
        );
        assert_eq!(
            verdict(&session(&[
                PLAN,
                &pass("a"),
                &fail("b"),
                &shutdown("error")
            ])),
            Verdict::Invalid("required task b settled fail".into())
        );
        assert_eq!(
            verdict(&session(&[PLAN, &pass("a"), &shutdown("complete")])),
            Verdict::Valid,
            "early completion leaves undispatched tasks out"
        );
        assert_eq!(
            verdict(&session(&[PLAN, &fail("a"), &shutdown("complete")])),
            Verdict::Invalid("required task a settled fail".into()),
            "early completion does not launder an earlier failure"
        );
        assert!(matches!(
            verdict(&session(&[PLAN, &pass("a"), &shutdown("stopped")])),
            Verdict::Invalid(why) if why.contains("never settled")
        ));
        assert!(matches!(
            verdict(&session(&[PLAN, &pass("a"), &pass("b"), &shutdown("budget")])),
            Verdict::Invalid(why) if why.contains("ceiling")
        ));
        assert!(matches!(
            verdict(&session(&[PLAN, &pass("a"), &pass("b")])),
            Verdict::Invalid(why) if why.contains("no shutdown")
        ));
    }
}
