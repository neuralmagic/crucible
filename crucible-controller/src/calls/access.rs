//! Who may read what of a call: a calling run's
//! readers see its calls, a playbook target run's own content only when they may read that run
//! too, and a lock names only targets its writer may read.

use crucible_contract::calls::{CallKind, PlaybookRef, ProgressGraph, ProgressNode, TargetHandle};

use crate::api::state::ApiState;
use crate::authz::Caller;
use crate::authz::action::{ResourceType, Verb};
use crate::authz::decision::Resource;
use crate::authz::model::Principal;

pub(crate) async fn run_resource(
    pool: &sqlx::PgPool,
    run: &str,
) -> anyhow::Result<Option<Resource>> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT l.owner, r.attributed_to FROM runs r \
         LEFT JOIN playbook_launches l ON l.key = r.issue WHERE r.run_id = $1",
    )
    .bind(run)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(owner, attributed)| {
        let mut resource = Resource::new(
            ResourceType::Run,
            run,
            Principal::stored(owner.or(attributed).as_deref()),
        );
        resource.run = Some(run.to_string());
        resource
    }))
}

pub(crate) async fn may_read_run(
    state: &ApiState,
    caller: &Caller,
    run: &str,
) -> anyhow::Result<bool> {
    let Some(resource) = run_resource(state.db.pool(), run).await? else {
        return Ok(false);
    };
    let resource =
        crate::authz::granted::attach(state.db.pool(), &caller.principals, resource).await?;
    Ok(crate::authz::owner::may(
        state,
        caller,
        Verb::Read,
        &resource,
    ))
}

pub(crate) fn labels_only(graph: &ProgressGraph) -> ProgressGraph {
    ProgressGraph {
        nodes: graph
            .nodes
            .iter()
            .map(|n| ProgressNode {
                id: n.id.clone(),
                label: n.label.clone(),
                status: n.status,
                group: n.group.clone(),
                started: None,
                ended: None,
                detail: None,
                link: None,
            })
            .collect(),
        edges: graph.edges.clone(),
        omitted: graph.omitted,
    }
}

pub(crate) fn guarded_run(kind: CallKind, handle: &TargetHandle) -> Option<&str> {
    match (kind, handle) {
        (CallKind::Playbook, TargetHandle::Playbook { run }) => Some(run),
        _ => None,
    }
}

pub(crate) async fn may_read_target(
    state: &ApiState,
    caller: &Caller,
    target: &PlaybookRef,
) -> anyhow::Result<bool> {
    let pool = state.db.pool();
    let resource = match target {
        PlaybookRef::Registered(name) => crate::playbooks::registry::get(pool, name)
            .await?
            .map(|row| row.resource()),
        PlaybookRef::Draft(id) => crate::playbooks::drafts::get(pool, id)
            .await?
            .map(|row| Resource::new(ResourceType::PlaybookDraft, id, row.owner.clone())),
    };
    let Some(resource) = resource else {
        return Ok(false);
    };
    let resource = crate::authz::granted::attach(pool, &caller.principals, resource).await?;
    Ok(crate::authz::owner::may(
        state,
        caller,
        Verb::Read,
        &resource,
    ))
}

#[cfg(test)]
mod tests {
    use crate::calls::access::*;
    use crucible_contract::calls::{NodeDetail, NodeLink, NodeStatus, ProgressEdge};

    #[test]
    fn a_reader_without_the_target_run_sees_labels_and_statuses_only() {
        let graph = ProgressGraph {
            nodes: vec![ProgressNode {
                id: "scan".into(),
                label: "scan".into(),
                status: NodeStatus::Running,
                group: Some("g".into()),
                started: Some("2026-11-25T10:00:00Z".into()),
                ended: None,
                detail: Some(NodeDetail::PlaybookTask {
                    task_kind: "agent".into(),
                }),
                link: Some(NodeLink::Run {
                    run: "child".into(),
                    task: Some("scan".into()),
                }),
            }],
            edges: vec![ProgressEdge {
                from: "a".into(),
                to: "scan".into(),
            }],
            omitted: 3,
        };
        let seen = labels_only(&graph);
        let node = &seen.nodes[0];
        assert_eq!(
            (node.id.as_str(), node.label.as_str(), node.status),
            ("scan", "scan", NodeStatus::Running)
        );
        assert_eq!(node.link, None);
        assert_eq!(node.detail, None);
        assert_eq!(node.started, None);
        assert_eq!(seen.edges, graph.edges);
        assert_eq!(seen.omitted, 3);
    }

    #[test]
    fn only_a_playbook_target_guards_its_run() {
        let run = TargetHandle::Playbook { run: "r".into() };
        assert_eq!(guarded_run(CallKind::Playbook, &run), Some("r"));
        let gh = TargetHandle::GithubWorkflow {
            repo: crucible_contract::calls::GithubRepo::parse("acme/app").expect("repo"),
            run_id: 7,
        };
        assert_eq!(guarded_run(CallKind::GithubWorkflow, &gh), None);
    }
}
