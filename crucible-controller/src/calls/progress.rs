//! A playbook target run's progress (RFC-0008 C-PROGRESS), folded from its session log: one node
//! per task and per mapped instance, with the run's own statuses.

use std::collections::BTreeMap;

use crucible_contract::calls::{
    NodeDetail, NodeLink, NodeStatus, ProgressEdge, ProgressGraph, ProgressNode,
};
use crucible_contract::session::{PlanTaskWire, SessionEvent, TaskStatus};

#[derive(Default)]
struct Seen {
    started: Option<String>,
    running: bool,
    settled: Option<(TaskStatus, Option<String>)>,
}

/// The graph of run `run` as its session log has it. `planned` stands in for the plan until the
/// log admits one.
pub(crate) fn fold(session: &str, run: &str, planned: &ProgressGraph) -> ProgressGraph {
    let mut plan: Option<Vec<PlanTaskWire>> = None;
    let mut seen: BTreeMap<String, Seen> = BTreeMap::new();
    for event in session
        .lines()
        .filter_map(crucible_contract::session::decode)
    {
        match event {
            SessionEvent::PlanAdmitted { tasks, .. } => plan = Some(tasks),
            SessionEvent::TaskStarted { task, at, .. } => {
                let entry = seen.entry(task).or_default();
                entry.running = true;
                entry.settled = None;
                if entry.started.is_none() {
                    entry.started = at;
                }
            }
            SessionEvent::TaskResult {
                task, status, at, ..
            } => {
                let entry = seen.entry(task).or_default();
                entry.running = false;
                entry.settled = Some((status, at));
            }
            _ => {}
        }
    }

    let (mut nodes, edges) = match plan {
        Some(tasks) => {
            let edges = tasks
                .iter()
                .flat_map(|t| {
                    t.depends_on.iter().map(|dep| ProgressEdge {
                        from: dep.clone(),
                        to: t.name.clone(),
                    })
                })
                .collect();
            let nodes = tasks
                .into_iter()
                .map(|t| ProgressNode {
                    id: t.name.clone(),
                    label: t.name,
                    status: NodeStatus::Pending,
                    group: None,
                    started: None,
                    ended: None,
                    detail: Some(NodeDetail::PlaybookTask { task_kind: t.kind }),
                    link: None,
                })
                .collect::<Vec<_>>();
            (nodes, edges)
        }
        None => {
            let pending = planned.pending();
            (pending.nodes, pending.edges)
        }
    };

    let known: std::collections::BTreeSet<String> = nodes.iter().map(|n| n.id.clone()).collect();
    for name in seen.keys() {
        if known.contains(name) {
            continue;
        }
        let task = crucible::plan::ir::TaskName(name.clone());
        let Some((group, _)) = task.instance_of() else {
            continue;
        };
        if !known.contains(group) {
            continue;
        }
        nodes.push(ProgressNode {
            id: name.clone(),
            label: name.clone(),
            status: NodeStatus::Pending,
            group: Some(group.to_string()),
            started: None,
            ended: None,
            detail: None,
            link: None,
        });
    }

    for node in &mut nodes {
        node.link = Some(NodeLink::Run {
            run: run.to_string(),
            task: Some(node.id.clone()),
        });
        let Some(state) = seen.get(&node.id) else {
            continue;
        };
        node.started = state.started.clone();
        node.status = match &state.settled {
            Some((status, at)) => {
                node.ended = at.clone();
                NodeStatus::Settled(*status)
            }
            None if state.running => NodeStatus::Running,
            None => NodeStatus::Pending,
        };
    }
    ProgressGraph::bounded(nodes, edges)
}

#[cfg(test)]
mod tests {
    use crate::calls::progress::fold;
    use crucible_contract::calls::{NodeLink, NodeStatus, ProgressGraph};
    use crucible_contract::session::TaskStatus;

    const SESSION: &str = r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{"name":"scan","kind":"command","depends_on":[]},{"name":"fix","kind":"agent","depends_on":["scan"],"over":"scan.items","max_fanout":4},{"name":"report","kind":"command","depends_on":["fix"]}]}
{"v":1,"kind":"task_started","task":"scan","attempt":1,"estimate_usd":0.0,"at":"2026-11-25T10:00:00Z"}
{"v":1,"kind":"task_result","task":"scan","status":"pass","at":"2026-11-25T10:00:05Z"}
{"v":1,"kind":"task_started","task":"fix[a]","attempt":1,"estimate_usd":0.0,"at":"2026-11-25T10:00:06Z"}
{"v":1,"kind":"task_started","task":"fix[b]","attempt":1,"estimate_usd":0.0}
{"v":1,"kind":"task_result","task":"fix[b]","status":"fail"}
not json
"#;

    #[test]
    fn a_session_folds_into_tasks_instances_and_their_statuses() {
        let graph = fold(SESSION, "run-2", &ProgressGraph::default());
        let status = |id: &str| {
            graph
                .nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.status)
                .unwrap_or_else(|| panic!("no node {id}"))
        };
        assert_eq!(status("scan"), NodeStatus::Settled(TaskStatus::Pass));
        assert_eq!(status("fix[a]"), NodeStatus::Running);
        assert_eq!(status("fix[b]"), NodeStatus::Settled(TaskStatus::Fail));
        assert_eq!(status("report"), NodeStatus::Pending);
        let instance = graph
            .nodes
            .iter()
            .find(|n| n.id == "fix[a]")
            .expect("instance");
        assert_eq!(instance.group.as_deref(), Some("fix"));
        assert_eq!(instance.started.as_deref(), Some("2026-11-25T10:00:06Z"));
        assert_eq!(
            instance.link,
            Some(NodeLink::Run {
                run: "run-2".into(),
                task: Some("fix[a]".into())
            })
        );
        let scan = graph.nodes.iter().find(|n| n.id == "scan").expect("scan");
        assert_eq!(scan.ended.as_deref(), Some("2026-11-25T10:00:05Z"));
        assert_eq!(graph.edges.len(), 2);
        assert_eq!(graph.omitted, 0);
    }

    #[test]
    fn before_the_plan_is_admitted_the_locked_graph_stands_in_pending() {
        let planned: ProgressGraph = serde_json::from_value(serde_json::json!({
            "nodes": [{"id": "only", "label": "only", "status": "pass"}],
            "edges": []
        }))
        .expect("graph");
        let graph = fold("", "run-2", &planned);
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].status, NodeStatus::Pending);
        assert!(graph.nodes[0].link.is_some());
    }
}
