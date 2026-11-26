//! A workflow run's progress and settlement: GitHub statuses as task statuses, jobs as nodes.

use std::collections::BTreeMap;

use crucible_contract::calls::{
    JobStep, NodeDetail, NodeLink, NodeStatus, ProgressEdge, ProgressGraph, ProgressNode,
};
use crucible_contract::link::ExternalUrl;
use crucible_contract::session::TaskStatus;
use forge::github::{Job, RunConclusion, RunStatus};

/// The task status a job or step with this conclusion is drawn with.
pub fn task_status(conclusion: RunConclusion) -> TaskStatus {
    match conclusion {
        RunConclusion::Success | RunConclusion::Neutral => TaskStatus::Pass,
        RunConclusion::Skipped => TaskStatus::Skipped,
        RunConclusion::ActionRequired => TaskStatus::Blocked,
        RunConclusion::StartupFailure => TaskStatus::Transport,
        RunConclusion::Failure
        | RunConclusion::Cancelled
        | RunConclusion::TimedOut
        | RunConclusion::Stale
        | RunConclusion::Unknown => TaskStatus::Fail,
    }
}

/// How a completed run settles its call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settle {
    /// Passing; `read_result` is false for a run that passes with an empty result.
    Pass {
        read_result: bool,
    },
    Fail(String),
    Cancelled,
}

/// The conclusion mapping of RFC-0008:C-GITHUB-WORKFLOW.
pub fn settle(conclusion: Option<RunConclusion>, caller_cancelled: bool) -> Settle {
    match conclusion {
        Some(RunConclusion::Success) => Settle::Pass { read_result: true },
        Some(RunConclusion::Skipped | RunConclusion::Neutral) => {
            Settle::Pass { read_result: false }
        }
        Some(RunConclusion::Cancelled) if caller_cancelled => Settle::Cancelled,
        Some(RunConclusion::Cancelled) => {
            Settle::Fail("the workflow run was cancelled outside this call".to_owned())
        }
        Some(other) => Settle::Fail(format!("the workflow run concluded {}", other.as_str())),
        None => Settle::Fail("the workflow run completed with no conclusion".to_owned()),
    }
}

/// A node's status from a job's or step's status and conclusion.
pub fn node_status(status: RunStatus, conclusion: Option<RunConclusion>) -> NodeStatus {
    match status {
        RunStatus::Completed => {
            NodeStatus::Settled(conclusion.map_or(TaskStatus::Fail, task_status))
        }
        RunStatus::InProgress => NodeStatus::Running,
        RunStatus::Requested
        | RunStatus::Queued
        | RunStatus::Pending
        | RunStatus::Waiting
        | RunStatus::Unknown => NodeStatus::Pending,
    }
}

fn job_node(job: &Job, id: String, group: Option<String>) -> ProgressNode {
    let status = node_status(job.status, job.conclusion);
    let steps = job
        .steps
        .iter()
        .map(|step| {
            let status = node_status(step.status, step.conclusion);
            JobStep {
                name: step.name.clone(),
                status,
                started: step
                    .started_at
                    .clone()
                    .filter(|_| status != NodeStatus::Pending),
                ended: step.completed_at.clone().filter(|_| status.ended()),
            }
        })
        .collect();
    ProgressNode {
        id,
        label: job.name.clone(),
        status,
        group,
        started: job
            .started_at
            .clone()
            .filter(|_| status != NodeStatus::Pending),
        ended: job.completed_at.clone().filter(|_| status.ended()),
        detail: Some(NodeDetail::GithubJob { steps }),
        link: job
            .html_url
            .as_deref()
            .and_then(|url| ExternalUrl::new(url).ok())
            .map(|url| NodeLink::External { url }),
    }
}

/// The names a job of the run may be known by in the workflow: its own, without a matrix
/// suffix `(a, b)`, and the caller job of a reusable workflow's `caller / callee`.
fn job_names(name: &str) -> Vec<&str> {
    let bare = match name.rfind(" (") {
        Some(at) if name.ends_with(')') => name.get(..at).unwrap_or(name),
        _ => name,
    };
    let mut names = vec![name, bare];
    for candidate in [name, bare] {
        if let Some((caller, _)) = candidate.split_once(" / ") {
            names.push(caller);
        }
    }
    names
}

/// Where one node of a run's progress comes from: a job of the run, or the static node of a
/// workflow job the run has no job for yet.
enum Placed<'a> {
    Job {
        id: String,
        group: Option<String>,
        job: &'a Job,
    },
    Static(&'a ProgressNode),
}

/// The locked static graph with each workflow job replaced by the run's jobs for it, and the
/// node ids each workflow job became. A job with matrix legs becomes one node per leg in a group
/// named by the workflow job; a run job no workflow job names is placed on its own.
fn place<'a>(
    workflow: &'a ProgressGraph,
    jobs: &'a [Job],
) -> (Vec<Placed<'a>>, BTreeMap<&'a str, Vec<String>>) {
    let mut matched: BTreeMap<&str, Vec<&Job>> = BTreeMap::new();
    let mut unmatched = Vec::new();
    for job in jobs {
        let names = job_names(&job.name);
        let owner = names.iter().find_map(|name| {
            workflow
                .nodes
                .iter()
                .find(|n| n.label == *name || n.id == *name)
        });
        match owner {
            Some(node) => matched.entry(node.id.as_str()).or_default().push(job),
            None => unmatched.push(job),
        }
    }
    let mut ids: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut placed = Vec::new();
    for static_node in &workflow.nodes {
        let key = static_node.id.as_str();
        match matched.get(key).map(Vec::as_slice) {
            None | Some([]) => {
                ids.entry(key).or_default().push(key.to_owned());
                placed.push(Placed::Static(static_node));
            }
            Some([only]) if only.name == static_node.label => {
                ids.entry(key).or_default().push(key.to_owned());
                placed.push(Placed::Job {
                    id: key.to_owned(),
                    group: None,
                    job: only,
                });
            }
            Some(legs) => {
                for leg in legs {
                    let id = format!("{key}/{}", leg.id);
                    ids.entry(key).or_default().push(id.clone());
                    placed.push(Placed::Job {
                        id,
                        group: Some(key.to_owned()),
                        job: leg,
                    });
                }
            }
        }
    }
    placed.extend(unmatched.into_iter().map(|job| Placed::Job {
        id: format!("job-{}", job.id),
        group: None,
        job,
    }));
    (placed, ids)
}

/// A run's progress over the locked static graph of its workflow.
pub fn run_graph(workflow: &ProgressGraph, jobs: &[Job]) -> ProgressGraph {
    let (placed, ids) = place(workflow, jobs);
    let nodes = placed
        .into_iter()
        .map(|placed| match placed {
            Placed::Job { id, group, job } => job_node(job, id, group),
            Placed::Static(node) => node.clone(),
        })
        .collect();
    let mut edges = Vec::new();
    for edge in &workflow.edges {
        let (Some(froms), Some(tos)) = (ids.get(edge.from.as_str()), ids.get(edge.to.as_str()))
        else {
            continue;
        };
        for from in froms {
            for to in tos {
                edges.push(ProgressEdge {
                    from: from.clone(),
                    to: to.clone(),
                });
            }
        }
    }
    ProgressGraph::bounded(nodes, edges)
}

/// The id of the job [`run_graph`] draws as node `node`; `None` for a node no job is drawn as.
pub fn job_of(workflow: &ProgressGraph, jobs: &[Job], node: &str) -> Option<u64> {
    place(workflow, jobs)
        .0
        .into_iter()
        .find_map(|placed| match placed {
            Placed::Job { id, job, .. } if id == node => Some(job.id),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use crucible_contract::calls::{NodeDetail, NodeLink, NodeStatus, ProgressEdge};
    use crucible_contract::session::TaskStatus;

    use forge::github::workflow::Workflow;
    use forge::github::{Job, RunConclusion, RunStatus, Step};

    use crate::calls::github::progress::{Settle, job_of, node_status, run_graph, settle};
    use crate::calls::github::workflow::graph;

    const DEMO: &str = r#"
on: workflow_dispatch
jobs:
  prepare:
    runs-on: ubuntu-latest
  build:
    name: Build it
    needs: prepare
    strategy:
      matrix:
        leg: [left, right]
    runs-on: ubuntu-latest
  report:
    name: report ${{ inputs.flavor }}
    needs: [prepare, build]
    runs-on: ubuntu-latest
"#;

    #[test]
    fn conclusions_settle_per_the_rfc() {
        use RunConclusion as C;
        let pass = Settle::Pass { read_result: true };
        let empty = Settle::Pass { read_result: false };
        assert_eq!(settle(Some(C::Success), false), pass);
        assert_eq!(settle(Some(C::Skipped), false), empty);
        assert_eq!(settle(Some(C::Neutral), false), empty);
        assert_eq!(settle(Some(C::Cancelled), true), Settle::Cancelled);
        assert!(
            matches!(settle(Some(C::Cancelled), false), Settle::Fail(n) if n.contains("cancelled outside"))
        );
        for (failing, word) in [
            (C::Failure, "failure"),
            (C::TimedOut, "timed_out"),
            (C::StartupFailure, "startup_failure"),
            (C::ActionRequired, "action_required"),
            (C::Stale, "stale"),
            (C::Unknown, "unknown"),
        ] {
            for caller_cancelled in [false, true] {
                assert!(
                    matches!(settle(Some(failing), caller_cancelled), Settle::Fail(n) if n.contains(word)),
                    "{failing:?}"
                );
            }
        }
        assert!(matches!(settle(None, false), Settle::Fail(_)));
    }

    #[test]
    fn statuses_map_to_task_statuses() {
        use RunConclusion as C;
        for pending in [
            RunStatus::Requested,
            RunStatus::Queued,
            RunStatus::Pending,
            RunStatus::Waiting,
            RunStatus::Unknown,
        ] {
            assert_eq!(node_status(pending, None), NodeStatus::Pending);
        }
        assert_eq!(
            node_status(RunStatus::InProgress, None),
            NodeStatus::Running
        );
        let settled = |c| node_status(RunStatus::Completed, Some(c));
        assert_eq!(settled(C::Success), NodeStatus::Settled(TaskStatus::Pass));
        assert_eq!(settled(C::Neutral), NodeStatus::Settled(TaskStatus::Pass));
        assert_eq!(
            settled(C::Skipped),
            NodeStatus::Settled(TaskStatus::Skipped)
        );
        assert_eq!(
            settled(C::ActionRequired),
            NodeStatus::Settled(TaskStatus::Blocked)
        );
        assert_eq!(
            settled(C::StartupFailure),
            NodeStatus::Settled(TaskStatus::Transport)
        );
        for failing in [C::Failure, C::Cancelled, C::TimedOut, C::Stale, C::Unknown] {
            assert_eq!(settled(failing), NodeStatus::Settled(TaskStatus::Fail));
        }
        assert_eq!(
            node_status(RunStatus::Completed, None),
            NodeStatus::Settled(TaskStatus::Fail)
        );
    }

    fn edge(from: &str, to: &str) -> ProgressEdge {
        ProgressEdge {
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    fn api_job(id: u64, name: &str, status: RunStatus, conclusion: Option<RunConclusion>) -> Job {
        Job {
            id,
            name: name.to_owned(),
            status,
            conclusion,
            started_at: Some("2026-11-25T10:00:00Z".to_owned()),
            completed_at: conclusion.map(|_| "2026-11-25T10:01:00Z".to_owned()),
            html_url: Some(format!("https://github.com/o/r/actions/runs/1/job/{id}")),
            steps: vec![Step {
                name: "Run true".to_owned(),
                status,
                conclusion,
                started_at: Some("2026-11-25T10:00:01Z".to_owned()),
                completed_at: conclusion.map(|_| "2026-11-25T10:00:59Z".to_owned()),
            }],
        }
    }

    #[test]
    fn jobs_map_to_nodes_with_matrix_legs_grouped() {
        use RunConclusion as C;
        let graph = graph(&Workflow::parse(DEMO).unwrap());
        let jobs = [
            api_job(11, "prepare", RunStatus::Completed, Some(C::Success)),
            api_job(
                12,
                "Build it (left)",
                RunStatus::Completed,
                Some(C::Failure),
            ),
            api_job(13, "Build it (right)", RunStatus::InProgress, None),
            api_job(14, "extra / inner", RunStatus::Queued, None),
        ];
        let progress = run_graph(&graph, &jobs);
        let nodes: Vec<(&str, &str, NodeStatus, Option<&str>)> = progress
            .nodes
            .iter()
            .map(|n| {
                (
                    n.id.as_str(),
                    n.label.as_str(),
                    n.status,
                    n.group.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            nodes,
            [
                (
                    "prepare",
                    "prepare",
                    NodeStatus::Settled(TaskStatus::Pass),
                    None
                ),
                (
                    "build/12",
                    "Build it (left)",
                    NodeStatus::Settled(TaskStatus::Fail),
                    Some("build")
                ),
                (
                    "build/13",
                    "Build it (right)",
                    NodeStatus::Running,
                    Some("build")
                ),
                ("report", "report", NodeStatus::Pending, None),
                ("job-14", "extra / inner", NodeStatus::Pending, None),
            ]
        );
        assert_eq!(
            progress.edges,
            [
                edge("prepare", "build/12"),
                edge("prepare", "build/13"),
                edge("prepare", "report"),
                edge("build/12", "report"),
                edge("build/13", "report"),
            ]
        );
        let prepare = &progress.nodes[0];
        assert_eq!(prepare.started.as_deref(), Some("2026-11-25T10:00:00Z"));
        assert_eq!(prepare.ended.as_deref(), Some("2026-11-25T10:01:00Z"));
        let Some(NodeDetail::GithubJob { steps }) = &prepare.detail else {
            panic!("no job detail");
        };
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, NodeStatus::Settled(TaskStatus::Pass));
        assert_eq!(steps[0].ended.as_deref(), Some("2026-11-25T10:00:59Z"));
        assert!(matches!(
            &prepare.link,
            Some(NodeLink::External { url }) if url.to_string().ends_with("/job/11")
        ));
        let running = &progress.nodes[2];
        assert_eq!(running.ended, None);
        let queued = &progress.nodes[4];
        assert_eq!(queued.started, None, "a queued job has not started");

        assert_eq!(job_of(&graph, &jobs, "prepare"), Some(11));
        assert_eq!(job_of(&graph, &jobs, "build/13"), Some(13));
        assert_eq!(job_of(&graph, &jobs, "job-14"), Some(14));
        assert_eq!(job_of(&graph, &jobs, "build"), None, "a group is no job");
        assert_eq!(job_of(&graph, &jobs, "report"), None, "not started yet");
        assert_eq!(job_of(&graph, &jobs, "ghost"), None);
    }

    #[test]
    fn reusable_workflow_jobs_and_name_only_matches_find_their_workflow_job() {
        let graph = graph(&Workflow::parse(
            "on: workflow_dispatch\njobs:\n  call:\n    uses: o/r/.github/workflows/x.yml@main\n  after:\n    needs: call\n    runs-on: x\n",
        ).unwrap());
        let jobs = [
            api_job(1, "call / inner (a)", RunStatus::InProgress, None),
            api_job(2, "call / inner (b)", RunStatus::InProgress, None),
            api_job(3, "after", RunStatus::Queued, None),
        ];
        let progress = run_graph(&graph, &jobs);
        let ids: Vec<&str> = progress.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["call/1", "call/2", "after"]);
        assert_eq!(
            progress.edges,
            [edge("call/1", "after"), edge("call/2", "after")]
        );
    }

    #[test]
    fn a_run_graph_is_bounded() {
        let graph = graph(
            &Workflow::parse("on: workflow_dispatch\njobs:\n  big:\n    runs-on: x\n").unwrap(),
        );
        let jobs: Vec<Job> = (0..600)
            .map(|i| api_job(i, &format!("big ({i})"), RunStatus::Queued, None))
            .collect();
        let progress = run_graph(&graph, &jobs);
        assert_eq!(progress.nodes.len(), 512);
        assert_eq!(progress.omitted, 88);
    }
}
