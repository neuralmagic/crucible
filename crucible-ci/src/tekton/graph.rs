//! A PipelineRun and its TaskRuns as a progress graph (RFC-0008 C-TEKTON, C-PROGRESS).
//!
//! ```text
//!   status.pipelineSpec.tasks/finally ─▶ one node per pipeline task (per TaskRun for a matrix)
//!   runAfter + $(tasks.X.results.Y)   ─▶ edges
//!   childReferences ─▶ TaskRun ─▶ status, times, steps, pod
//! ```

use std::collections::{BTreeMap, BTreeSet};

use crucible_contract::calls::{NodeLink, NodeStatus, ProgressEdge, ProgressGraph, ProgressNode};
use crucible_contract::link::ExternalUrl;
use crucible_contract::session::TaskStatus;
use serde_json::Value;

use crate::tekton::find::LOG_URL_ANNOTATION;
use crate::tekton::model::{PipelineRun, PipelineTask, StepState, TaskRun, succeeded};

/// The group every finally task is drawn in.
pub const FINALLY_GROUP: &str = "finally";

/// One step of a task, with the container its log is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub name: String,
    pub container: Option<String>,
    pub status: NodeStatus,
    pub started: Option<String>,
    pub ended: Option<String>,
}

/// What a node's TaskRun carries beyond the graph: where its logs are and its steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDetail {
    pub task_run: String,
    pub pod: Option<String>,
    pub steps: Vec<Step>,
}

/// A run's graph, and the TaskRun detail of each node that has one, keyed by node id.
#[derive(Clone, Debug, PartialEq)]
pub struct RunGraph {
    pub graph: ProgressGraph,
    pub tasks: BTreeMap<String, TaskDetail>,
}

/// The graph of `run`, with `task_runs` the TaskRuns it created. A run that has not resolved its
/// pipeline yet has no nodes.
pub fn run_graph(run: &PipelineRun, task_runs: &[TaskRun]) -> RunGraph {
    let Some(status) = &run.status else {
        return empty();
    };
    let Some(spec) = &status.pipeline_spec else {
        return empty();
    };
    let by_name: BTreeMap<&str, &TaskRun> = task_runs
        .iter()
        .map(|t| (t.metadata.name.as_str(), t))
        .collect();
    let skipped: BTreeSet<&str> = status
        .skipped_tasks
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let run_ended = succeeded(&status.conditions).is_some_and(|c| c.status != "Unknown");
    let link = run
        .metadata
        .annotations
        .get(LOG_URL_ANNOTATION)
        .and_then(|u| ExternalUrl::new(u).ok())
        .map(|url| NodeLink::External { url });

    let mut nodes = Vec::new();
    let mut tasks = BTreeMap::new();
    let mut ids: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let finally: BTreeSet<&str> = spec.finally.iter().map(|t| t.name.as_str()).collect();
    for task in spec.tasks.iter().chain(&spec.finally) {
        let runs: Vec<&TaskRun> = status
            .child_references
            .iter()
            .filter(|c| c.kind.as_deref().is_none_or(|k| k == "TaskRun"))
            .filter(|c| c.pipeline_task_name.as_deref() == Some(task.name.as_str()))
            .filter_map(|c| by_name.get(c.name.as_str()).copied())
            .collect();
        let group = match (runs.len() > 1, finally.contains(task.name.as_str())) {
            (true, _) => Some(task.name.clone()),
            (false, true) => Some(FINALLY_GROUP.to_owned()),
            (false, false) => None,
        };
        let entry = ids.entry(task.name.as_str()).or_default();
        if runs.is_empty() {
            let status = match (skipped.contains(task.name.as_str()), run_ended) {
                (true, _) => NodeStatus::Settled(TaskStatus::Skipped),
                (false, true) => NodeStatus::Settled(TaskStatus::NotTaken),
                (false, false) => NodeStatus::Pending,
            };
            entry.push(task.name.clone());
            nodes.push(ProgressNode {
                id: task.name.clone(),
                label: task.name.clone(),
                status,
                group,
                started: None,
                ended: None,
                detail: None,
                link: link.clone(),
            });
            continue;
        }
        let legs = runs.len();
        for (i, task_run) in runs.into_iter().enumerate() {
            let (id, label) = match legs {
                1 => (task.name.clone(), task.name.clone()),
                _ => (
                    format!("{}[{i}]", task.name),
                    format!("{} ({})", task.name, i + 1),
                ),
            };
            let st = task_run.status.clone().unwrap_or_default();
            entry.push(id.clone());
            nodes.push(ProgressNode {
                id: id.clone(),
                label,
                status: task_run_status(task_run),
                group: group.clone(),
                started: st.start_time.clone(),
                ended: st.completion_time.clone(),
                detail: None,
                link: link.clone(),
            });
            tasks.insert(
                id,
                TaskDetail {
                    task_run: task_run.metadata.name.clone(),
                    pod: st.pod_name.clone(),
                    steps: st.steps.iter().map(step).collect(),
                },
            );
        }
    }

    let mut edges = Vec::new();
    for task in spec.tasks.iter().chain(&spec.finally) {
        let Some(to_ids) = ids.get(task.name.as_str()) else {
            continue;
        };
        for dep in dependencies(task) {
            let Some(from_ids) = ids.get(dep.as_str()) else {
                continue;
            };
            for from in from_ids {
                for to in to_ids {
                    edges.push(ProgressEdge {
                        from: from.clone(),
                        to: to.clone(),
                    });
                }
            }
        }
    }
    RunGraph {
        graph: ProgressGraph::bounded(nodes, edges),
        tasks,
    }
}

fn empty() -> RunGraph {
    RunGraph {
        graph: ProgressGraph::default(),
        tasks: BTreeMap::new(),
    }
}

/// The tasks `task` waits for: its runAfter, and every task whose results its params or when
/// expressions read.
pub fn dependencies(task: &PipelineTask) -> BTreeSet<String> {
    let mut deps: BTreeSet<String> = task.run_after.iter().cloned().collect();
    for param in &task.params {
        result_refs(&param.value, &mut deps);
    }
    for when in &task.when {
        result_refs(when, &mut deps);
    }
    deps.remove(&task.name);
    deps
}

fn result_refs(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some((_, after)) = rest.split_once("$(tasks.") {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                    .unwrap_or(after.len());
                if let Some((name, tail)) = after.split_at_checked(end)
                    && !name.is_empty()
                    && tail.starts_with(".results.")
                {
                    into.insert(name.to_owned());
                }
                rest = after;
            }
        }
        Value::Array(items) => items.iter().for_each(|v| result_refs(v, into)),
        Value::Object(map) => map.values().for_each(|v| result_refs(v, into)),
        _ => {}
    }
}

/// A TaskRun's state as a node status: not started is pending, an Unknown `Succeeded` running,
/// True pass, False fail.
pub fn task_run_status(task_run: &TaskRun) -> NodeStatus {
    let Some(status) = &task_run.status else {
        return NodeStatus::Pending;
    };
    match succeeded(&status.conditions).map(|c| c.status.as_str()) {
        Some("True") => NodeStatus::Settled(TaskStatus::Pass),
        Some("False") => NodeStatus::Settled(TaskStatus::Fail),
        Some(_) if status.start_time.is_some() => NodeStatus::Running,
        _ => NodeStatus::Pending,
    }
}

fn step(state: &StepState) -> Step {
    let (status, started, ended) = match (&state.terminated, &state.running) {
        (Some(t), _) => {
            let status = match t.exit_code {
                0 => TaskStatus::Pass,
                _ => TaskStatus::Fail,
            };
            (
                NodeStatus::Settled(status),
                t.started_at.clone(),
                t.finished_at.clone(),
            )
        }
        (None, Some(r)) => (NodeStatus::Running, r.started_at.clone(), None),
        (None, None) => (NodeStatus::Pending, None, None),
    };
    Step {
        name: state.name.clone(),
        container: state.container.clone(),
        status,
        started,
        ended,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crucible_contract::calls::{NodeLink, NodeStatus};
    use crucible_contract::session::TaskStatus;
    use serde_json::json;

    use crate::tekton::graph::{FINALLY_GROUP, dependencies, run_graph};
    use crate::tekton::model::{List, PipelineRun, PipelineTask, TaskRun};

    fn pipeline_run(name: &str) -> PipelineRun {
        let raw = include_str!("../../testdata/tekton/pipelineruns.json");
        let list: List<PipelineRun> = serde_json::from_str(raw).expect("fixture parses");
        list.items
            .into_iter()
            .find(|r| r.metadata.name == name)
            .expect("fixture holds the run")
    }

    fn task_runs(raw: &str) -> Vec<TaskRun> {
        let list: List<TaskRun> = serde_json::from_str(raw).expect("fixture parses");
        list.items
    }

    fn status_of(g: &crate::tekton::graph::RunGraph, id: &str) -> Option<NodeStatus> {
        g.graph.nodes.iter().find(|n| n.id == id).map(|n| n.status)
    }

    #[test]
    fn a_finished_run_draws_every_task_with_its_outcome() {
        let run = pipeline_run("odh-openshell-sandbox-openclaw-on-push-wtb9f");
        let trs = task_runs(include_str!(
            "../../testdata/tekton/taskruns-odh-openshell-sandbox-openclaw-on-push-wtb9f.json"
        ));
        let g = run_graph(&run, &trs);
        let spec = run
            .status
            .as_ref()
            .and_then(|s| s.pipeline_spec.as_ref())
            .expect("resolved spec");
        let st = run.status.as_ref().expect("status");
        assert_eq!(
            g.graph.nodes.len(),
            st.child_references.len() + st.skipped_tasks.len(),
            "one node per TaskRun, matrix legs included, and one per skipped task"
        );
        for matrix in ["build-images", "clair-scan", "clamav-scan"] {
            let legs = g
                .graph
                .nodes
                .iter()
                .filter(|n| n.group.as_deref() == Some(matrix))
                .count();
            assert!(legs > 1, "{matrix} is drawn as its legs");
        }
        assert_eq!(g.graph.omitted, 0);
        let passed = g
            .graph
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Settled(TaskStatus::Pass))
            .count();
        let skipped = g
            .graph
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Settled(TaskStatus::Skipped))
            .count();
        assert_eq!((passed, skipped), (24, 4));
        assert_eq!(
            status_of(&g, "clone-repository"),
            Some(NodeStatus::Settled(TaskStatus::Pass))
        );
        assert!(
            g.graph
                .nodes
                .iter()
                .all(|n| matches!(n.link, Some(NodeLink::External { .. })))
        );
        for f in &spec.finally {
            let node = g
                .graph
                .nodes
                .iter()
                .find(|n| n.id == f.name)
                .expect("finally node");
            assert_eq!(node.group.as_deref(), Some(FINALLY_GROUP));
        }
        let ids: BTreeSet<&str> = g.graph.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(
            g.graph
                .edges
                .iter()
                .all(|e| ids.contains(e.from.as_str()) && ids.contains(e.to.as_str()))
        );
        assert!(
            g.graph
                .edges
                .iter()
                .any(|e| e.from == "init" && e.to == "clone-repository")
        );
        let clone = g
            .tasks
            .get("clone-repository")
            .expect("clone has a TaskRun");
        assert!(clone.pod.is_some());
        assert!(!clone.steps.is_empty());
        assert!(clone.steps.iter().all(|s| {
            s.container
                .as_deref()
                .is_some_and(|c| c.starts_with("step-"))
        }));
        assert!(
            clone
                .steps
                .iter()
                .all(|s| s.status == NodeStatus::Settled(TaskStatus::Pass))
        );
    }

    #[test]
    fn a_running_run_draws_matrix_legs_and_unstarted_tasks() {
        let run = pipeline_run("odh-openshell-gateway-on-push-wcsxc");
        let trs = task_runs(include_str!(
            "../../testdata/tekton/taskruns-odh-openshell-gateway-on-push-wcsxc.json"
        ));
        let g = run_graph(&run, &trs);
        let legs: Vec<_> = g
            .graph
            .nodes
            .iter()
            .filter(|n| n.group.as_deref() == Some("build-images"))
            .collect();
        assert_eq!(legs.len(), 2);
        assert!(legs.iter().all(|n| n.id.starts_with("build-images[")));
        assert!(legs.iter().all(|n| n.status == NodeStatus::Running));
        assert_eq!(
            status_of(&g, "init"),
            Some(NodeStatus::Settled(TaskStatus::Pass))
        );
        let pending = g
            .graph
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Pending)
            .count();
        assert!(pending > 0, "tasks after build-images have not started");
        assert!(
            g.graph
                .edges
                .iter()
                .filter(|e| e.from == "prefetch-dependencies")
                .filter(|e| e.to.starts_with("build-images["))
                .count()
                == 2
        );
        let leg = g.tasks.get("build-images[0]").expect("leg detail");
        assert!(leg.steps.iter().any(|s| s.status == NodeStatus::Running));
    }

    #[test]
    fn a_run_without_a_resolved_pipeline_has_no_nodes() {
        let run = pipeline_run("early-gate-ci-build-c2k2h");
        let g = run_graph(&run, &[]);
        assert!(g.graph.nodes.is_empty());
        assert!(g.tasks.is_empty());
    }

    #[test]
    fn result_references_are_dependencies() {
        let task: PipelineTask = serde_json::from_value(json!({
            "name": "build",
            "runAfter": ["init"],
            "params": [
                {"name": "url", "value": "$(tasks.clone.results.url)"},
                {"name": "list", "value": ["x", "$(tasks.prefetch.results.dir) and $(tasks.clone.results.commit)"]},
                {"name": "plain", "value": "$(params.git-url)"},
                {"name": "self", "value": "$(tasks.build.results.x)"},
                {"name": "notresult", "value": "$(tasks.status)"}
            ],
            "when": [{"input": "$(tasks.gate.results.go)", "operator": "in", "values": ["true"]}]
        }))
        .expect("task parses");
        let deps: Vec<String> = dependencies(&task).into_iter().collect();
        assert_eq!(deps, ["clone", "gate", "init", "prefetch"]);
    }
}
