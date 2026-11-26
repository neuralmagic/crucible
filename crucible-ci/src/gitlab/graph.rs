//! A GitLab pipeline's jobs as a progress graph, and how a pipeline ended (RFC-0008
//! C-GITLAB-PIPELINE).

use std::collections::BTreeMap;

use crucible_contract::calls::{NodeLink, NodeStatus, ProgressEdge, ProgressGraph, ProgressNode};
use crucible_contract::link::ExternalUrl;
use crucible_contract::session::TaskStatus;

use crate::gitlab::model::{Bridge, Job, Lint, Pipeline};

/// The static shape of a pipeline: its stages in order, and what each job needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    pub stages: Vec<String>,
    /// Job name to the jobs it needs. A job absent here waits on the stage before its own.
    pub needs: BTreeMap<String, Vec<String>>,
}

impl Shape {
    /// The shape GitLab's lint reports for a ref.
    pub fn from_lint(lint: &Lint) -> Self {
        let mut stages: Vec<String> = Vec::new();
        let mut needs = BTreeMap::new();
        for job in &lint.jobs {
            if !stages.contains(&job.stage) {
                stages.push(job.stage.clone());
            }
            let needed = job.needed();
            if !needed.is_empty() {
                needs.insert(job.name.clone(), needed);
            }
        }
        Shape { stages, needs }
    }
}

/// What a job node carries beyond the graph: the job id its log is read by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobDetail {
    pub job_id: u64,
    /// The downstream pipeline a trigger job started.
    pub downstream: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunGraph {
    pub graph: ProgressGraph,
    pub jobs: BTreeMap<String, JobDetail>,
}

/// A job's status as a node status.
pub fn job_status(status: &str, allow_failure: bool) -> NodeStatus {
    match status {
        "running" => NodeStatus::Running,
        "success" => NodeStatus::Settled(TaskStatus::Pass),
        "failed" if allow_failure => NodeStatus::Settled(TaskStatus::Pass),
        "failed" | "canceled" | "canceling" => NodeStatus::Settled(TaskStatus::Fail),
        "skipped" => NodeStatus::Settled(TaskStatus::Skipped),
        _ => NodeStatus::Pending,
    }
}

/// The graph of a pipeline from its jobs and trigger jobs, with `shape` giving stage order and
/// needs. Stages the shape does not name follow, ordered by their first job's id.
pub fn pipeline_graph(jobs: &[Job], bridges: &[Bridge], shape: &Shape) -> RunGraph {
    struct Row<'a> {
        id: u64,
        name: &'a str,
        stage: &'a str,
        status: NodeStatus,
        started: Option<String>,
        ended: Option<String>,
        url: Option<&'a str>,
        downstream: Option<u64>,
    }
    let mut rows: Vec<Row> = jobs
        .iter()
        .map(|j| Row {
            id: j.id,
            name: &j.name,
            stage: &j.stage,
            status: job_status(&j.status, j.allow_failure),
            started: j.started_at.clone(),
            ended: j.finished_at.clone(),
            url: j.web_url.as_deref(),
            downstream: None,
        })
        .chain(bridges.iter().map(|b| Row {
            id: b.id,
            name: &b.name,
            stage: &b.stage,
            status: job_status(&b.status, b.allow_failure),
            started: b.started_at.clone(),
            ended: b.finished_at.clone(),
            url: b.web_url.as_deref(),
            downstream: b.downstream_pipeline.as_ref().map(|d| d.id),
        }))
        .collect();
    rows.sort_by_key(|r| r.id);

    let mut stages = shape.stages.clone();
    for row in &rows {
        if !stages.iter().any(|s| s == row.stage) {
            stages.push(row.stage.to_owned());
        }
    }
    let rank = |stage: &str| stages.iter().position(|s| s == stage).unwrap_or(usize::MAX);

    let mut nodes = Vec::with_capacity(rows.len());
    let mut details = BTreeMap::new();
    for row in &rows {
        nodes.push(ProgressNode {
            id: row.name.to_owned(),
            label: row.name.to_owned(),
            status: row.status,
            group: Some(row.stage.to_owned()),
            started: row.started.clone(),
            ended: row.ended.clone(),
            detail: None,
            link: row
                .url
                .and_then(|u| ExternalUrl::new(u).ok())
                .map(|url| NodeLink::External { url }),
        });
        details.insert(
            row.name.to_owned(),
            JobDetail {
                job_id: row.id,
                downstream: row.downstream,
            },
        );
    }

    let mut edges = Vec::new();
    for row in &rows {
        let from: Vec<&str> = match shape.needs.get(row.name) {
            Some(needed) => needed.iter().map(String::as_str).collect(),
            None => {
                let mine = rank(row.stage);
                let previous = rows
                    .iter()
                    .map(|r| rank(r.stage))
                    .filter(|r| *r < mine)
                    .max();
                rows.iter()
                    .filter(|r| Some(rank(r.stage)) == previous)
                    .map(|r| r.name)
                    .collect()
            }
        };
        for dep in from {
            if details.contains_key(dep) && dep != row.name {
                edges.push(ProgressEdge {
                    from: dep.to_owned(),
                    to: row.name.to_owned(),
                });
            }
        }
    }
    RunGraph {
        graph: ProgressGraph::bounded(nodes, edges),
        jobs: details,
    }
}

/// How a pipeline stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PipelineState {
    Running,
    Passed,
    /// Skipped: passing with an empty result.
    Skipped,
    Failed(String),
    Cancelled,
    /// Stopped on a manual job, which nothing will start.
    Blocked(String),
}

/// `pipeline`'s state; `jobs` names the manual job a blocked pipeline waits on.
pub fn pipeline_state(pipeline: &Pipeline, jobs: &[Job]) -> PipelineState {
    match pipeline.status.as_str() {
        "success" => PipelineState::Passed,
        "skipped" => PipelineState::Skipped,
        "failed" => {
            let failed: Vec<&str> = jobs
                .iter()
                .filter(|j| j.status == "failed" && !j.allow_failure)
                .map(|j| j.name.as_str())
                .collect();
            PipelineState::Failed(match failed.is_empty() {
                true => format!("pipeline {} failed", pipeline.id),
                false => format!("pipeline {} failed: {}", pipeline.id, failed.join(", ")),
            })
        }
        "canceled" => PipelineState::Cancelled,
        "manual" => {
            let manual: Vec<&str> = jobs
                .iter()
                .filter(|j| j.status == "manual" && !j.allow_failure)
                .map(|j| j.name.as_str())
                .collect();
            PipelineState::Blocked(format!(
                "pipeline {} waits on the manual job(s) {}",
                pipeline.id,
                manual.join(", ")
            ))
        }
        _ => PipelineState::Running,
    }
}

#[cfg(test)]
mod tests {
    use crucible_contract::calls::NodeStatus;
    use crucible_contract::session::TaskStatus;
    use serde_json::json;

    use crate::gitlab::graph::{PipelineState, Shape, job_status, pipeline_graph, pipeline_state};
    use crate::gitlab::model::{Bridge, Job, Lint, Pipeline};

    fn jobs() -> Vec<Job> {
        serde_json::from_str(include_str!("../../testdata/gitlab/jobs.json")).expect("jobs parse")
    }

    fn pipeline() -> Pipeline {
        serde_json::from_str(include_str!("../../testdata/gitlab/pipeline.json"))
            .expect("pipeline parses")
    }

    fn job(id: u64, name: &str, stage: &str, status: &str) -> Job {
        Job {
            id,
            name: name.into(),
            stage: stage.into(),
            status: status.into(),
            allow_failure: false,
            started_at: None,
            finished_at: None,
            web_url: None,
        }
    }

    #[test]
    fn statuses_map_as_the_rfc_lists_them() {
        for pending in [
            "created",
            "waiting_for_resource",
            "preparing",
            "pending",
            "scheduled",
            "manual",
        ] {
            assert_eq!(job_status(pending, false), NodeStatus::Pending, "{pending}");
        }
        assert_eq!(job_status("running", false), NodeStatus::Running);
        assert_eq!(
            job_status("success", false),
            NodeStatus::Settled(TaskStatus::Pass)
        );
        assert_eq!(
            job_status("failed", false),
            NodeStatus::Settled(TaskStatus::Fail)
        );
        assert_eq!(
            job_status("failed", true),
            NodeStatus::Settled(TaskStatus::Pass)
        );
        assert_eq!(
            job_status("canceled", false),
            NodeStatus::Settled(TaskStatus::Fail)
        );
        assert_eq!(
            job_status("skipped", false),
            NodeStatus::Settled(TaskStatus::Skipped)
        );
    }

    #[test]
    fn a_real_pipeline_draws_its_jobs_by_stage() {
        let shape = Shape {
            stages: vec!["test".into(), "deploy".into()],
            needs: Default::default(),
        };
        let g = pipeline_graph(&jobs(), &[], &shape);
        assert_eq!(g.graph.nodes.len(), 11);
        assert_eq!(
            g.graph
                .nodes
                .iter()
                .filter(|n| n.group.as_deref() == Some("test"))
                .count(),
            9
        );
        let deploys: Vec<&str> = g
            .graph
            .nodes
            .iter()
            .filter(|n| n.group.as_deref() == Some("deploy"))
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(deploys.len(), 2);
        assert_eq!(
            g.graph.edges.len(),
            9 * 2,
            "each deploy job waits on every test job"
        );
        assert!(g.graph.nodes.iter().all(|n| n.link.is_some()));
        assert_eq!(g.jobs.len(), 11);
        assert_eq!(pipeline_state(&pipeline(), &jobs()), PipelineState::Passed);
    }

    #[test]
    fn needs_replace_stage_order_and_bridges_are_nodes() {
        let jobs = vec![
            job(1, "build", "build", "success"),
            job(2, "lint", "build", "running"),
            job(3, "test", "test", "pending"),
        ];
        let bridge: Bridge = serde_json::from_value(json!({
            "id": 4, "name": "deploy", "stage": "deploy", "status": "running",
            "downstream_pipeline": {"id": 99}
        }))
        .expect("bridge parses");
        let lint: Lint = serde_json::from_value(json!({
            "valid": true,
            "jobs": [
                {"name": "build", "stage": "build"},
                {"name": "lint", "stage": "build"},
                {"name": "test", "stage": "test", "needs": [{"name": "build"}]},
                {"name": "deploy", "stage": "deploy", "needs": ["test"]}
            ]
        }))
        .expect("lint parses");
        let g = pipeline_graph(&jobs, &[bridge], &Shape::from_lint(&lint));
        let edges: Vec<(&str, &str)> = g
            .graph
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();
        assert_eq!(edges, [("build", "test"), ("test", "deploy")]);
        assert_eq!(g.jobs.get("deploy").and_then(|d| d.downstream), Some(99));
    }

    #[test]
    fn a_pipeline_ends_by_its_status() {
        let mut p = pipeline();
        let jobs = vec![
            job(1, "unit", "test", "failed"),
            Job {
                allow_failure: true,
                ..job(2, "flaky", "test", "failed")
            },
            job(3, "release", "deploy", "manual"),
        ];
        p.status = "failed".into();
        assert_eq!(
            pipeline_state(&p, &jobs),
            PipelineState::Failed(format!("pipeline {} failed: unit", p.id))
        );
        p.status = "manual".into();
        assert!(
            matches!(pipeline_state(&p, &jobs), PipelineState::Blocked(n) if n.ends_with("release"))
        );
        p.status = "canceled".into();
        assert_eq!(pipeline_state(&p, &jobs), PipelineState::Cancelled);
        p.status = "skipped".into();
        assert_eq!(pipeline_state(&p, &jobs), PipelineState::Skipped);
        for running in [
            "created",
            "pending",
            "running",
            "waiting_for_resource",
            "preparing",
            "scheduled",
        ] {
            p.status = running.into();
            assert_eq!(
                pipeline_state(&p, &jobs),
                PipelineState::Running,
                "{running}"
            );
        }
    }
}
