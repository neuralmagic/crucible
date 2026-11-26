//! Read-only checks against a live Konflux cluster. Ignored by default; run with
//!
//! ```text
//! CRUCIBLE_CI_KONFLUX_API=https://api.stone-prd-rh01.pg1f.p1.openshiftapps.com:6443 \
//! CRUCIBLE_CI_KONFLUX_NAMESPACE=open-data-hub-tenant \
//! CRUCIBLE_CI_KONFLUX_TOKEN=$(oc whoami -t) \
//!   cargo test -p crucible-ci --test konflux_live -- --ignored --nocapture
//! ```
//!
//! The test picks a commit that has Pipelines-as-Code runs on the cluster now, so it does not
//! depend on a run the cluster has since pruned. It only lists and reads.

use std::collections::BTreeSet;

use crucible_ci::commit::CommitId;
use crucible_ci::tekton::find::{
    Event, Filter, PULL_REQUEST_LABEL, REPOSITORY_LABEL, SHA_LABEL, latest_by_name, run_name,
};
use crucible_ci::tekton::graph::run_graph;
use crucible_ci::tekton::outcome::{RunState, run_state};
use crucible_ci::tekton::{Cluster, Namespace};
use crucible_ci::token::BearerToken;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn cluster() -> (Cluster, Namespace, BearerToken) {
    let http = reqwest::Client::builder()
        .build()
        .expect("http client builds");
    let cluster =
        Cluster::new(http, &env("CRUCIBLE_CI_KONFLUX_API")).expect("the API address parses");
    let ns = Namespace::parse(&env("CRUCIBLE_CI_KONFLUX_NAMESPACE")).expect("namespace parses");
    (
        cluster,
        ns,
        BearerToken::new(env("CRUCIBLE_CI_KONFLUX_TOKEN")),
    )
}

#[tokio::test]
#[ignore = "reaches a live Konflux cluster"]
async fn watch_finds_and_follows_the_runs_of_a_live_commit() {
    let (cluster, ns, token) = cluster();
    let every = cluster
        .pipeline_runs(&token, &ns, &format!("{SHA_LABEL},{REPOSITORY_LABEL}"))
        .await
        .expect("listing Pipelines-as-Code runs");
    let Some(sample) = every.iter().max_by_key(|r| {
        let sha = r.metadata.labels.get(SHA_LABEL);
        every
            .iter()
            .filter(|o| o.metadata.labels.get(SHA_LABEL) == sha)
            .count()
    }) else {
        panic!("no Pipelines-as-Code runs in {}", ns.as_str());
    };
    let labels = &sample.metadata.labels;
    let filter = Filter {
        commit: CommitId::parse(&labels[SHA_LABEL]).expect("PaC labels a full commit"),
        repository: labels[REPOSITORY_LABEL].clone(),
        event: match labels.contains_key(PULL_REQUEST_LABEL) {
            true => Some(Event::PullRequest),
            false => Some(Event::Push),
        },
    };
    println!(
        "watching {} of {} ({:?})",
        filter.commit.short(),
        filter.repository,
        filter.event
    );

    let found = cluster
        .find(&token, &ns, &filter)
        .await
        .expect("find answers");
    assert!(!found.is_empty());
    assert!(found.iter().all(|r| filter.admits(r)));
    let expected: BTreeSet<String> = every
        .iter()
        .filter(|r| filter.admits(r))
        .map(|r| r.metadata.name.clone())
        .collect();
    let got: BTreeSet<String> = found.iter().map(|r| r.metadata.name.clone()).collect();
    assert_eq!(
        got, expected,
        "the server-side selector matches what the client admits"
    );

    for (name, run) in latest_by_name(&found) {
        let fresh = cluster
            .pipeline_run(&token, &ns, &run.metadata.name)
            .await
            .expect("reading the run")
            .expect("the run still exists");
        let task_runs = cluster
            .task_runs(&token, &ns, &fresh.metadata.name)
            .await
            .expect("listing its TaskRuns");
        let graph = run_graph(&fresh, &task_runs);
        let state = run_state(&fresh);
        println!(
            "  {name}: {} nodes, {} edges, {} TaskRuns, {}",
            graph.graph.nodes.len(),
            graph.graph.edges.len(),
            task_runs.len(),
            match &state {
                RunState::Running => "running".to_owned(),
                RunState::Ended(e) =>
                    format!("{:?}: {} ({} results)", e.verdict, e.note, e.results.len()),
            }
        );
        assert_eq!(run_name(&fresh), name);
        let started = fresh
            .status
            .as_ref()
            .is_some_and(|s| s.pipeline_spec.is_some());
        assert_eq!(!graph.graph.nodes.is_empty(), started);
        for detail in graph.tasks.values() {
            assert!(
                task_runs.iter().any(|t| t.metadata.name == detail.task_run),
                "every node detail names a TaskRun the run created"
            );
        }
    }
}

#[tokio::test]
#[ignore = "reaches a live Konflux cluster"]
async fn a_step_log_reads_from_the_task_pod() {
    let (cluster, ns, token) = cluster();
    let every = cluster
        .pipeline_runs(&token, &ns, &format!("{SHA_LABEL},{REPOSITORY_LABEL}"))
        .await
        .expect("listing Pipelines-as-Code runs");
    for run in &every {
        let task_runs = cluster
            .task_runs(&token, &ns, &run.metadata.name)
            .await
            .expect("listing TaskRuns");
        let graph = run_graph(run, &task_runs);
        let Some((pod, container)) = graph.tasks.values().find_map(|d| {
            let pod = d.pod.clone()?;
            let step = d.steps.iter().find(|s| s.status.ended())?;
            Some((pod, step.container.clone()?))
        }) else {
            continue;
        };
        let log = cluster
            .step_log(&token, &ns, &pod, &container, Some(20))
            .await
            .expect("reading the step log");
        println!("{pod}/{container}:\n{log}");
        assert!(log.lines().count() <= 20);
        return;
    }
    panic!("no ended step with a pod among the runs in {}", ns.as_str());
}

/// Needs `CRUCIBLE_CI_KONFLUX_RESULTS` (the Tekton Results address) and
/// `CRUCIBLE_CI_KONFLUX_PRUNED_SHA`, a commit whose runs the cluster has pruned, and
/// `CRUCIBLE_CI_KONFLUX_PRUNED_REPOSITORY`, their Pipelines-as-Code Repository.
#[tokio::test]
#[ignore = "reaches a live Konflux cluster"]
async fn results_still_hold_a_pruned_commits_runs() {
    let (cluster, ns, token) = cluster();
    let http = reqwest::Client::builder()
        .build()
        .expect("http client builds");
    let results =
        crucible_ci::tekton::results::Results::new(http, &env("CRUCIBLE_CI_KONFLUX_RESULTS"))
            .expect("the Results address parses");
    let filter = Filter {
        commit: CommitId::parse(&env("CRUCIBLE_CI_KONFLUX_PRUNED_SHA")).expect("full commit"),
        repository: env("CRUCIBLE_CI_KONFLUX_PRUNED_REPOSITORY"),
        event: None,
    };
    let live = cluster
        .find(&token, &ns, &filter)
        .await
        .expect("find answers");
    assert!(live.is_empty(), "the cluster has pruned these runs");
    let recorded = results
        .find(&token, &ns, &filter)
        .await
        .expect("Results answers");
    assert!(!recorded.is_empty());
    for run in &recorded {
        let uid = run
            .metadata
            .uid
            .as_deref()
            .expect("a recorded run has a uid");
        let task_runs = results
            .task_runs(&token, &ns, uid)
            .await
            .expect("its TaskRuns read back");
        let graph = run_graph(run, &task_runs);
        let state = run_state(run);
        println!(
            "  {}: {} nodes, {} TaskRuns, {:?}",
            run_name(run),
            graph.graph.nodes.len(),
            task_runs.len(),
            match &state {
                RunState::Ended(e) =>
                    format!("{:?} {:?}", e.verdict, e.results.get("IMAGE_DIGEST")),
                RunState::Running => "running".to_owned(),
            }
        );
        assert!(matches!(state, RunState::Ended(_)));
        assert!(!graph.graph.nodes.is_empty());
    }
}
