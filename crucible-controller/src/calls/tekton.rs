//! The `tekton` call target (RFC-0008 C-TEKTON, C-WATCH): a watch of the Pipelines-as-Code
//! PipelineRuns a commit started. Dispatch only opens the watch; each poll finds the runs, live
//! or recorded in Tekton Results, follows them, and settles once they have all ended.
//!
//! ```text
//!   dispatch ─▶ detail {window_closes_at, matched: {}}          (no cluster call)
//!   poll     ─▶ find (live ∪ Results) while the window is open ─▶ matched by name
//!            ─▶ each matched run + its TaskRuns ─▶ one group per run in the graph
//!            ─▶ settle: a run failed · window closed and all passed · past the wall clock
//!   cancel   ─▶ nothing: a watch started none of the runs
//! ```

mod links;

use std::collections::BTreeMap;
use std::path::PathBuf;

use crucible_ci::commit::CommitId;
use crucible_ci::tekton::find::{Event, Filter, latest_by_name};
use crucible_ci::tekton::graph::{RunGraph, TaskDetail, fold_legs, run_graph};
use crucible_ci::tekton::model::{PipelineRun, TaskRun};
use crucible_ci::tekton::outcome::{RunState, Verdict, declared_results, run_state};
use crucible_ci::tekton::results::Results;
use crucible_ci::tekton::{Cluster, Namespace, TektonError};
use crucible_ci::token::BearerToken;
use crucible_contract::calls::{
    CallStatus, CallTarget, NodeDetail, NodeStatus, PacEvent, Pin, ProgressEdge, ProgressGraph,
    ProgressNode, TargetHandle, TargetInterface, TektonExpected, TektonStep, TektonTaskRun,
    WATCH_APPEAR_PARAM, WATCH_COMMIT_PARAM, WATCH_DEFAULT_APPEAR,
};
use crucible_contract::emits::{EmitWire, FieldType};
use serde_json::{Map, Value, json};

use crate::calls::{
    CallCtx, CallError, CallRecord, DispatchSpec, Dispatched, Outcome, Polled, Resolved,
    TargetBackend,
};

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_APPEAR: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

/// One cluster a Tekton target may name: the kubeconfig whose current context reaches it, read at
/// each operation and held in memory only, and its Tekton Results address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TektonCluster {
    pub kubeconfig: PathBuf,
    pub results: Option<String>,
}

pub(crate) fn clusters(
    kubeconfigs: &[String],
    results: &[String],
) -> Result<BTreeMap<String, TektonCluster>, String> {
    let pair = |entry: &str, what: &str| -> Result<(String, String), String> {
        match entry.split_once('=') {
            Some((name, value)) if !name.trim().is_empty() && !value.trim().is_empty() => {
                Ok((name.trim().to_owned(), value.trim().to_owned()))
            }
            _ => Err(format!("{entry:?} is not <cluster>={what}")),
        }
    };
    let mut out = BTreeMap::new();
    for entry in kubeconfigs.iter().filter(|e| !e.trim().is_empty()) {
        let (name, path) = pair(entry, "<kubeconfig path>")?;
        out.insert(
            name,
            TektonCluster {
                kubeconfig: PathBuf::from(path),
                results: None,
            },
        );
    }
    for entry in results.iter().filter(|e| !e.trim().is_empty()) {
        let (name, url) = pair(entry, "<results url>")?;
        let cluster = out.get_mut(&name).ok_or_else(|| {
            format!("Tekton Results names cluster {name:?}, which has no kubeconfig")
        })?;
        cluster.results = Some(url);
    }
    Ok(out)
}

pub(crate) struct TektonBackend {
    http: reqwest::Client,
    clusters: BTreeMap<String, TektonCluster>,
}

/// The fields of a `tekton` target.
struct Target<'a> {
    cluster: &'a str,
    namespace: Namespace,
    repository: &'a str,
    event: Option<PacEvent>,
    expected: &'a [TektonExpected],
}

fn target(target: &CallTarget) -> Result<Target<'_>, CallError> {
    match target {
        CallTarget::Tekton {
            cluster,
            namespace,
            repository,
            event,
            expected,
        } => Ok(Target {
            cluster,
            namespace: Namespace::parse(namespace)
                .map_err(|e| CallError::Refused(e.to_string()))?,
            repository,
            event: *event,
            expected,
        }),
        other => Err(CallError::Refused(format!(
            "a {} target is not a tekton target",
            other.kind()
        ))),
    }
}

fn refused(e: TektonError) -> CallError {
    match e.is_transient() {
        true => CallError::Internal(anyhow::Error::new(e)),
        false => CallError::Refused(e.to_string()),
    }
}

impl TektonBackend {
    pub(crate) fn new(clusters: BTreeMap<String, TektonCluster>) -> Result<Self, String> {
        crate::install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("building the Tekton client: {e}"))?;
        Ok(TektonBackend { http, clusters })
    }

    async fn reach(
        &self,
        name: &str,
    ) -> Result<(Cluster, Option<Results>, BearerToken), CallError> {
        let cfg = self.clusters.get(name).ok_or_else(|| {
            CallError::Refused(format!(
                "this controller reaches no Tekton cluster named {name:?} \
                 (CONTROLLER_CALLS_TEKTON_CLUSTERS)"
            ))
        })?;
        let text = tokio::fs::read_to_string(&cfg.kubeconfig)
            .await
            .map_err(|e| {
                CallError::Refused(format!(
                    "reading the kubeconfig of cluster {name:?} at {}: {e}",
                    cfg.kubeconfig.display()
                ))
            })?;
        let (api, token) = kube_endpoint(name, &text).await?;
        let cluster = Cluster::new(self.http.clone(), &api).map_err(refused)?;
        let results = match &cfg.results {
            Some(url) => Some(Results::new(self.http.clone(), url).map_err(refused)?),
            None => None,
        };
        Ok((cluster, results, token))
    }
}

async fn kube_endpoint(name: &str, text: &str) -> Result<(String, BearerToken), CallError> {
    use secrecy::ExposeSecret;
    let refused = |e: &dyn std::fmt::Display| CallError::Refused(format!("cluster {name:?}: {e}"));
    let kubeconfig = kube::config::Kubeconfig::from_yaml(text).map_err(|e| refused(&e))?;
    let config = kube::Config::from_custom_kubeconfig(
        kubeconfig,
        &kube::config::KubeConfigOptions::default(),
    )
    .await
    .map_err(|e| refused(&e))?;
    let token = match (&config.auth_info.token, &config.auth_info.token_file) {
        (Some(token), _) => token.expose_secret().to_owned(),
        (None, Some(file)) => tokio::fs::read_to_string(file)
            .await
            .map_err(|e| refused(&e))?
            .trim()
            .to_owned(),
        (None, None) => {
            return Err(refused(
                &"the current context holds no bearer token; sign in again",
            ));
        }
    };
    Ok((
        config
            .cluster_url
            .to_string()
            .trim_end_matches('/')
            .to_owned(),
        BearerToken::new(token),
    ))
}

/// What a watch keeps between polls in its call's detail.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct WatchDetail {
    window_closes_at: String,
    #[serde(default)]
    matched: BTreeMap<String, Matched>,
    #[serde(default)]
    results_read_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Matched {
    pipeline_run: String,
    uid: String,
    #[serde(default)]
    ended: Option<EndedRun>,
}

/// A matched run that ended, kept so later polls never read it again.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct EndedRun {
    verdict: EndedAs,
    note: String,
    #[serde(default)]
    results: BTreeMap<String, Value>,
    view: RunView,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum EndedAs {
    Passed,
    Failed,
    Cancelled,
}

impl EndedRun {
    fn state(&self) -> RunState {
        RunState::Ended(crucible_ci::tekton::outcome::Ended {
            verdict: match self.verdict {
                EndedAs::Passed => Verdict::Passed,
                EndedAs::Failed => Verdict::Failed,
                EndedAs::Cancelled => Verdict::Cancelled,
            },
            note: self.note.clone(),
            results: self.results.clone(),
        })
    }
}

const RESULTS_EVERY: jiff::SignedDuration = jiff::SignedDuration::from_secs(60);

/// One matched run as a poll saw it.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    pipeline_run: String,
    state: RunState,
}

fn appear_window(appear: &str) -> Result<jiff::SignedDuration, CallError> {
    appear
        .parse::<jiff::SignedDuration>()
        .ok()
        .filter(|d| d.is_positive() && *d <= MAX_APPEAR)
        .ok_or_else(|| {
            CallError::Refused(format!(
                "appear {appear:?} is not a duration such as \"10m\" of at most 24h"
            ))
        })
}

fn results_due(window_open: bool, read_at: Option<&str>, now: jiff::Timestamp) -> bool {
    window_open
        && read_at
            .and_then(|at| at.parse::<jiff::Timestamp>().ok())
            .is_none_or(|at| now.duration_since(at) >= RESULTS_EVERY)
}

fn settle(
    now: jiff::Timestamp,
    window_closes_at: jiff::Timestamp,
    deadline: Option<jiff::Timestamp>,
    expected: &[TektonExpected],
    seen: &BTreeMap<String, Seen>,
) -> Option<Outcome> {
    if let Some((name, failed)) = seen.iter().find_map(|(name, s)| match &s.state {
        RunState::Ended(e) if e.verdict != Verdict::Passed => Some((name, e)),
        _ => None,
    }) {
        let how = match failed.verdict {
            Verdict::Cancelled => "was cancelled",
            _ => "failed",
        };
        return Some(Outcome::failed(format!("{name} {how}: {}", failed.note)));
    }
    let unended: Vec<&str> = seen
        .iter()
        .filter(|(_, s)| s.state == RunState::Running)
        .map(|(name, _)| name.as_str())
        .collect();
    if deadline.is_some_and(|d| now >= d) {
        return Some(Outcome::failed(match unended.is_empty() {
            true => "the watch timed out before every run appeared".to_owned(),
            false => format!("the watch timed out; still running: {}", unended.join(", ")),
        }));
    }
    if now < window_closes_at {
        return None;
    }
    if seen.is_empty() {
        return Some(Outcome::failed(
            "no PipelineRun appeared for the commit within the appearance window",
        ));
    }
    let missing: Vec<&str> = expected
        .iter()
        .map(|e| e.name.as_str())
        .filter(|name| !seen.contains_key(*name))
        .collect();
    if !missing.is_empty() {
        return Some(Outcome::failed(format!(
            "no run appeared for {} within the appearance window",
            missing.join(", ")
        )));
    }
    if !unended.is_empty() {
        return None;
    }
    let mut result = Map::new();
    let mut runs = Vec::new();
    for (name, s) in seen {
        let RunState::Ended(ended) = &s.state else {
            continue;
        };
        runs.push(json!({"name": name, "pipeline_run": s.pipeline_run, "status": "passed"}));
        if let Some(want) = expected.iter().find(|e| &e.name == name) {
            match declared_results(name, ended, &want.results) {
                Ok(values) => {
                    result.insert(want.field(), json!(values));
                }
                Err(missing) => return Some(Outcome::failed(missing.to_string())),
            }
        }
    }
    result.insert("runs".to_owned(), Value::Array(runs));
    Some(Outcome {
        status: CallStatus::Passed,
        note: format!("{} run(s) passed", seen.len()),
        result: Value::Object(result),
        result_files: Vec::new(),
    })
}

/// One matched run's part of the watch graph: its nodes, ids prefixed with the run's name and
/// matrix legs folded, and their edges.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct RunView {
    nodes: Vec<ProgressNode>,
    edges: Vec<ProgressEdge>,
}

fn run_view(name: &str, run: &RunGraph) -> RunView {
    let run = fold_legs(run);
    let id = |node: &str| format!("{name}/{node}");
    RunView {
        nodes: run
            .graph
            .nodes
            .iter()
            .map(|node| ProgressNode {
                id: id(&node.id),
                group: Some(name.to_owned()),
                detail: run.tasks.get(&node.id).map(|legs| NodeDetail::TektonTask {
                    task_runs: legs.iter().map(task_run).collect(),
                }),
                ..node.clone()
            })
            .collect(),
        edges: run
            .graph
            .edges
            .iter()
            .map(|e| ProgressEdge {
                from: id(&e.from),
                to: id(&e.to),
            })
            .collect(),
    }
}

fn watch_graph(expected: &[TektonExpected], runs: &BTreeMap<String, RunView>) -> ProgressGraph {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for want in expected {
        if !runs.contains_key(&want.name) {
            nodes.push(ProgressNode {
                id: want.name.clone(),
                label: want.name.clone(),
                status: NodeStatus::Pending,
                group: Some(want.name.clone()),
                started: None,
                ended: None,
                detail: None,
                link: None,
            });
        }
    }
    for view in runs.values() {
        nodes.extend(view.nodes.iter().cloned());
        edges.extend(view.edges.iter().cloned());
    }
    ProgressGraph::bounded(nodes, edges)
}

fn task_run(t: &TaskDetail) -> TektonTaskRun {
    TektonTaskRun {
        name: t.task_run.clone(),
        pod: t.pod.clone(),
        steps: t
            .steps
            .iter()
            .map(|s| TektonStep {
                name: s.name.clone(),
                container: s.container.clone(),
                status: s.status,
                started: s.started.clone(),
                ended: s.ended.clone(),
            })
            .collect(),
    }
}

fn interface_of(expected: &[TektonExpected]) -> TargetInterface {
    let mut result = vec![EmitWire {
        field: "runs".to_owned(),
        ty: Some(FieldType::List),
    }];
    result.extend(expected.iter().map(|e| EmitWire {
        field: e.field(),
        ty: Some(FieldType::Object),
    }));
    TargetInterface {
        params: json!({
            "type": "object",
            "properties": {
                WATCH_COMMIT_PARAM: {
                    "type": "string",
                    "description": "The commit whose PipelineRuns the watch follows",
                    "pattern": "^([0-9a-f]{40}|[0-9a-f]{64})$",
                },
                WATCH_APPEAR_PARAM: {
                    "type": "string",
                    "description": "How long runs may take to appear",
                    "default": WATCH_DEFAULT_APPEAR,
                },
            },
            "required": [WATCH_COMMIT_PARAM],
            "additionalProperties": false,
        }),
        result,
        result_files: Vec::new(),
        graph: watch_graph(expected, &BTreeMap::new()),
    }
}

fn watch_of(call: &CallRecord) -> Result<(String, CommitId, WatchDetail), CallError> {
    let Some(TargetHandle::TektonWatch {
        cluster, commit, ..
    }) = &call.handle
    else {
        return Err(CallError::Refused(format!(
            "call {} is no Tekton watch",
            call.id
        )));
    };
    let commit = CommitId::parse(commit).map_err(|e| CallError::Refused(e.to_string()))?;
    let detail: WatchDetail =
        serde_json::from_value(call.detail.clone()).map_err(|_| CallError::Unrecorded {
            call: call.id.clone(),
            what: "watch window",
        })?;
    Ok((cluster.clone(), commit, detail))
}

#[async_trait::async_trait]
impl TargetBackend for TektonBackend {
    async fn interface(&self, _ctx: &CallCtx, t: &CallTarget) -> Result<Resolved, CallError> {
        let t = target(t)?;
        if !self.clusters.contains_key(t.cluster) {
            return Err(CallError::Refused(format!(
                "this controller reaches no Tekton cluster named {:?}",
                t.cluster
            )));
        }
        Ok(Resolved {
            pin: Pin::TektonWatch,
            interface: interface_of(t.expected),
        })
    }

    async fn authorize(
        &self,
        _ctx: &CallCtx,
        _who: &crate::authz::model::Principal,
        t: &CallTarget,
    ) -> Result<(), CallError> {
        let t = target(t)?;
        let (cluster, _, token) = self.reach(t.cluster).await?;
        match cluster
            .may(&token, &t.namespace, "list")
            .await
            .map_err(refused)?
        {
            true => Ok(()),
            false => Err(CallError::Refused(format!(
                "the token for cluster {:?} may not list PipelineRuns in {}",
                t.cluster,
                t.namespace.as_str()
            ))),
        }
    }

    async fn dispatch(
        &self,
        _ctx: &CallCtx,
        spec: &DispatchSpec<'_>,
    ) -> Result<Dispatched, CallError> {
        let t = target(spec.target)?;
        let commit = spec
            .values
            .get(WATCH_COMMIT_PARAM)
            .and_then(Value::as_str)
            .ok_or_else(|| CallError::Refused("the watch binds no commit".to_owned()))?;
        let commit = CommitId::parse(commit).map_err(|e| CallError::Refused(e.to_string()))?;
        let appear = spec
            .values
            .get(WATCH_APPEAR_PARAM)
            .and_then(Value::as_str)
            .unwrap_or(WATCH_DEFAULT_APPEAR);
        let closes = jiff::Timestamp::now()
            .checked_add(appear_window(appear)?)
            .map_err(|e| CallError::Internal(e.into()))?;
        let detail = WatchDetail {
            window_closes_at: closes.to_string(),
            matched: BTreeMap::new(),
            results_read_at: None,
        };
        Ok(Dispatched {
            handle: TargetHandle::TektonWatch {
                cluster: t.cluster.to_owned(),
                namespace: t.namespace.as_str().to_owned(),
                commit: commit.to_string(),
            },
            link: None,
            detail: serde_json::to_value(detail).map_err(|e| CallError::Internal(e.into()))?,
        })
    }

    async fn poll(&self, _ctx: &CallCtx, call: &CallRecord) -> Result<Polled, CallError> {
        let t = target(&call.target)?;
        let (cluster_name, commit, mut detail) = watch_of(call)?;
        let (cluster, results, token) = self.reach(&cluster_name).await?;
        let now = jiff::Timestamp::now();
        let closes: jiff::Timestamp = detail
            .window_closes_at
            .parse()
            .map_err(|e: jiff::Error| CallError::Internal(e.into()))?;
        let filter = Filter {
            commit,
            repository: t.repository.to_owned(),
            event: t.event.map(|e| match e {
                PacEvent::Push => Event::Push,
                PacEvent::PullRequest => Event::PullRequest,
            }),
        };

        let window_open = now < closes;
        let unended: Vec<String> = detail
            .matched
            .iter()
            .filter(|(_, m)| m.ended.is_none())
            .map(|(name, _)| name.clone())
            .collect();
        let live = match window_open || !unended.is_empty() {
            true => cluster
                .find(&token, &t.namespace, &filter)
                .await
                .map_err(refused)?,
            false => Vec::new(),
        };
        let missing_live = unended.iter().any(|name| {
            detail
                .matched
                .get(name)
                .is_some_and(|m| !live.iter().any(|r| r.metadata.name == m.pipeline_run))
        });
        let results_due = results_due(window_open, detail.results_read_at.as_deref(), now);
        let mut candidates: Vec<PipelineRun> = live.clone();
        if let Some(r) = results.as_ref().filter(|_| results_due || missing_live) {
            for run in r
                .find(&token, &t.namespace, &filter)
                .await
                .map_err(refused)?
            {
                if !candidates
                    .iter()
                    .any(|c| c.metadata.uid == run.metadata.uid)
                {
                    candidates.push(run);
                }
            }
            detail.results_read_at = Some(now.to_string());
        }
        for (name, run) in latest_by_name(&candidates) {
            let known = detail.matched.get(&name);
            if !window_open && known.is_none() {
                continue;
            }
            if known.is_some_and(|m| m.pipeline_run == run.metadata.name) {
                continue;
            }
            detail.matched.insert(
                name,
                Matched {
                    pipeline_run: run.metadata.name.clone(),
                    uid: run.metadata.uid.clone().unwrap_or_default(),
                    ended: None,
                },
            );
        }

        let mut views = BTreeMap::new();
        let mut seen = BTreeMap::new();
        for (name, m) in detail.matched.iter_mut() {
            if let Some(ended) = &m.ended {
                views.insert(name.clone(), ended.view.clone());
                seen.insert(
                    name.clone(),
                    Seen {
                        pipeline_run: m.pipeline_run.clone(),
                        state: ended.state(),
                    },
                );
                continue;
            }
            let is_live = live.iter().any(|r| r.metadata.name == m.pipeline_run);
            let run = candidates
                .iter()
                .find(|r| r.metadata.name == m.pipeline_run);
            let task_runs: Vec<TaskRun> = match (run, is_live, &results) {
                (Some(run), true, _) => cluster
                    .task_runs(&token, &t.namespace, &run.metadata.name)
                    .await
                    .map_err(refused)?,
                (Some(_), false, Some(r)) => r
                    .task_runs(&token, &t.namespace, &m.uid)
                    .await
                    .map_err(refused)?,
                _ => Vec::new(),
            };
            let (state, view) = match run {
                Some(run) => (run_state(run), run_view(name, &run_graph(run, &task_runs))),
                None => (
                    RunState::Ended(crucible_ci::tekton::outcome::Ended {
                        verdict: Verdict::Failed,
                        note: "the run was pruned before it ended, and no Tekton Results \
                               record holds it"
                            .to_owned(),
                        results: BTreeMap::new(),
                    }),
                    RunView::default(),
                ),
            };
            if let RunState::Ended(e) = &state {
                m.ended = Some(EndedRun {
                    verdict: match e.verdict {
                        Verdict::Passed => EndedAs::Passed,
                        Verdict::Failed => EndedAs::Failed,
                        Verdict::Cancelled => EndedAs::Cancelled,
                    },
                    note: e.note.clone(),
                    results: e.results.clone(),
                    view: view.clone(),
                });
            }
            views.insert(name.clone(), view);
            seen.insert(
                name.clone(),
                Seen {
                    pipeline_run: m.pipeline_run.clone(),
                    state,
                },
            );
        }

        let deadline = call
            .budget
            .and_then(|b| b.wall_clock_secs)
            .and_then(|secs| {
                call.created_at
                    .parse::<jiff::Timestamp>()
                    .ok()?
                    .checked_add(jiff::SignedDuration::from_secs(i64::try_from(secs).ok()?))
                    .ok()
            });
        let outcome = settle(now, closes, deadline, t.expected, &seen);
        let mut recorded = Map::new();
        recorded.insert(
            "matched".to_owned(),
            serde_json::to_value(&detail.matched).map_err(|e| CallError::Internal(e.into()))?,
        );
        recorded.insert(
            "results_read_at".to_owned(),
            serde_json::to_value(&detail.results_read_at)
                .map_err(|e| CallError::Internal(e.into()))?,
        );
        let known: Vec<crucible_contract::calls::NamedLink> = call
            .detail
            .get(crucible_contract::calls::LINKS_DETAIL_KEY)
            .and_then(|l| serde_json::from_value(l.clone()).ok())
            .unwrap_or_default();
        let runs: BTreeMap<String, String> = detail
            .matched
            .iter()
            .map(|(name, m)| (name.clone(), m.pipeline_run.clone()))
            .collect();
        let (named, link) = links::watch_links(&runs, &candidates, &known);
        crate::calls::record_links(&mut recorded, &named);
        Ok(Polled {
            progress: watch_graph(t.expected, &views),
            recorded,
            outcome,
            link,
        })
    }

    async fn result_file(
        &self,
        _ctx: &CallCtx,
        _call: &CallRecord,
        _path: &str,
    ) -> Result<Option<Vec<u8>>, CallError> {
        Ok(None)
    }

    async fn cancel(&self, _ctx: &CallCtx, _call: &CallRecord) -> Result<(), CallError> {
        Ok(())
    }

    async fn node_log(
        &self,
        _ctx: &CallCtx,
        call: &CallRecord,
        node: &str,
    ) -> Result<String, CallError> {
        let t = target(&call.target)?;
        let (cluster_name, _, detail) = watch_of(call)?;
        let (name, task) = node
            .split_once('/')
            .ok_or_else(|| CallError::Refused(format!("{node:?} is no task of a matched run")))?;
        let m = detail
            .matched
            .get(name)
            .ok_or_else(|| CallError::Refused(format!("the watch matched no run {name:?}")))?;
        let (cluster, _, token) = self.reach(&cluster_name).await?;
        let Some(run) = cluster
            .pipeline_run(&token, &t.namespace, &m.pipeline_run)
            .await
            .map_err(refused)?
        else {
            return Err(CallError::Refused(format!(
                "{} has been pruned; its logs are gone from the cluster",
                m.pipeline_run
            )));
        };
        let task_runs = cluster
            .task_runs(&token, &t.namespace, &run.metadata.name)
            .await
            .map_err(refused)?;
        let graph = fold_legs(&run_graph(&run, &task_runs));
        let legs = graph
            .tasks
            .get(task)
            .ok_or_else(|| CallError::Refused(format!("{task:?} has no TaskRun yet")))?;
        let mut out = String::new();
        for leg in legs {
            let Some(pod) = leg.pod.as_deref() else {
                continue;
            };
            for step in &leg.steps {
                let Some(container) = step.container.as_deref() else {
                    continue;
                };
                if step.status == NodeStatus::Pending {
                    continue;
                }
                let log = cluster
                    .step_log(&token, &t.namespace, pod, container, None)
                    .await
                    .map_err(refused)?;
                let head = match legs.len() {
                    1 => step.name.clone(),
                    _ => format!("{} · {}", leg.task_run, step.name),
                };
                out.push_str(&format!("── {head} ──\n{log}\n"));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crucible_ci::tekton::outcome::{Ended, RunState, Verdict};
    use crucible_contract::calls::{CallStatus, TektonExpected};
    use serde_json::{Value, json};

    use crate::calls::tekton::{
        EndedAs, EndedRun, Matched, RunView, Seen, WatchDetail, appear_window, clusters,
        interface_of, results_due, settle,
    };

    fn at(secs: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_second(1_800_000_000 + secs).expect("in range")
    }

    fn expected(names: &[&str]) -> Vec<TektonExpected> {
        names
            .iter()
            .map(|n| TektonExpected {
                name: (*n).to_owned(),
                results: vec!["IMAGE_DIGEST".to_owned()],
            })
            .collect()
    }

    fn ended(verdict: Verdict, digest: Option<&str>) -> Seen {
        let mut results = BTreeMap::new();
        if let Some(d) = digest {
            results.insert("IMAGE_DIGEST".to_owned(), Value::from(d));
        }
        Seen {
            pipeline_run: "pr-x".to_owned(),
            state: RunState::Ended(Ended {
                verdict,
                note: "Tasks Completed: 3".to_owned(),
                results,
            }),
        }
    }

    fn running() -> Seen {
        Seen {
            pipeline_run: "pr-y".to_owned(),
            state: RunState::Running,
        }
    }

    fn seen(entries: Vec<(&str, Seen)>) -> BTreeMap<String, Seen> {
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
    }

    #[test]
    fn nothing_settles_while_the_window_is_open_and_runs_pass() {
        let exp = expected(&["server", "operator"]);
        let s = seen(vec![("server", ended(Verdict::Passed, Some("sha256:a")))]);
        assert_eq!(settle(at(0), at(600), None, &exp, &s), None);
        let s = seen(vec![
            ("server", ended(Verdict::Passed, Some("sha256:a"))),
            ("operator", running()),
        ]);
        assert_eq!(settle(at(700), at(600), None, &exp, &s), None);
    }

    #[test]
    fn a_failed_run_settles_at_once() {
        let exp = expected(&["server", "operator"]);
        let s = seen(vec![
            ("server", ended(Verdict::Failed, None)),
            ("operator", running()),
        ]);
        let out = settle(at(0), at(600), None, &exp, &s).expect("settled");
        assert_eq!(out.status, CallStatus::Failed);
        assert!(out.note.starts_with("server failed: "), "{}", out.note);
        let s = seen(vec![("server", ended(Verdict::Cancelled, None))]);
        let out = settle(at(0), at(600), None, &exp, &s).expect("settled");
        assert!(out.note.contains("was cancelled"));
    }

    #[test]
    fn the_window_closing_settles_on_missing_or_passing_runs() {
        let exp = expected(&["server", "operator"]);
        let none = settle(at(601), at(600), None, &exp, &BTreeMap::new()).expect("settled");
        assert!(none.note.starts_with("no PipelineRun appeared"));
        let one = seen(vec![("server", ended(Verdict::Passed, Some("sha256:a")))]);
        let missing = settle(at(601), at(600), None, &exp, &one).expect("settled");
        assert_eq!(
            missing.note,
            "no run appeared for operator within the appearance window"
        );
        let both = seen(vec![
            ("server", ended(Verdict::Passed, Some("sha256:a"))),
            ("operator", ended(Verdict::Passed, Some("sha256:b"))),
        ]);
        let passed = settle(at(601), at(600), None, &exp, &both).expect("settled");
        assert_eq!(passed.status, CallStatus::Passed);
        assert_eq!(passed.result["server"], json!({"IMAGE_DIGEST": "sha256:a"}));
        let dashed = expected(&["odh-server"]);
        let one = seen(vec![(
            "odh-server",
            ended(Verdict::Passed, Some("sha256:c")),
        )]);
        let out = settle(at(601), at(600), None, &dashed, &one).expect("settled");
        assert_eq!(
            out.result["odh_server"],
            json!({"IMAGE_DIGEST": "sha256:c"})
        );
        assert_eq!(
            passed.result["operator"],
            json!({"IMAGE_DIGEST": "sha256:b"})
        );
        assert_eq!(passed.result["runs"].as_array().map(Vec::len), Some(2));
        let no_digest = seen(vec![
            ("server", ended(Verdict::Passed, None)),
            ("operator", ended(Verdict::Passed, Some("sha256:b"))),
        ]);
        let lacking = settle(at(601), at(600), None, &exp, &no_digest).expect("settled");
        assert_eq!(lacking.status, CallStatus::Failed);
        assert!(lacking.note.contains("IMAGE_DIGEST"));
    }

    #[test]
    fn unexpected_runs_count_but_return_no_results() {
        let s = seen(vec![("lint", ended(Verdict::Passed, None))]);
        let out = settle(at(601), at(600), None, &[], &s).expect("settled");
        assert_eq!(out.status, CallStatus::Passed);
        assert_eq!(
            out.result,
            json!({"runs": [{"name": "lint", "pipeline_run": "pr-x", "status": "passed"}]})
        );
    }

    #[test]
    fn the_wall_clock_ends_a_watch_naming_what_still_runs() {
        let exp = expected(&["server"]);
        let s = seen(vec![("server", running())]);
        let out = settle(at(4000), at(600), Some(at(3600)), &exp, &s).expect("settled");
        assert_eq!(out.note, "the watch timed out; still running: server");
    }

    #[test]
    fn the_interface_takes_a_commit_and_returns_each_expected_run() {
        let i = interface_of(&expected(&["server", "operator"]));
        assert_eq!(i.params["required"], json!(["commit"]));
        assert_eq!(i.params["properties"]["appear"]["default"], json!("10m"));
        let fields: Vec<&str> = i.result.iter().map(|w| w.field.as_str()).collect();
        assert_eq!(fields, ["runs", "server", "operator"]);
        assert_eq!(i.graph.nodes.len(), 2);
    }

    #[test]
    fn windows_and_cluster_entries_parse() {
        let secs = |raw: &str| appear_window(raw).ok().map(|d| d.as_secs());
        assert_eq!(secs("90s"), Some(90));
        assert_eq!(secs("10m"), Some(600));
        assert_eq!(secs("2h"), Some(7200));
        assert_eq!(secs("24h"), Some(86_400));
        for bad in ["", "m", "0s", "-1m", "25h", "soon"] {
            assert_eq!(secs(bad), None, "{bad:?}");
        }
        let c = clusters(
            &["konflux=/tmp/kubeconfig".to_owned()],
            &["konflux=https://results.example.com".to_owned()],
        )
        .expect("parses");
        assert_eq!(
            c["konflux"].results.as_deref(),
            Some("https://results.example.com")
        );
        assert!(clusters(&["bad".to_owned()], &[]).is_err());
        assert!(clusters(&[], &["other=https://r".to_owned()]).is_err());
    }

    #[test]
    fn results_are_read_once_a_minute_while_the_window_is_open() {
        assert!(results_due(true, None, at(0)));
        let read = at(0).to_string();
        assert!(!results_due(true, Some(&read), at(30)));
        assert!(results_due(true, Some(&read), at(60)));
        assert!(!results_due(false, None, at(0)));
        assert!(results_due(true, Some("garbage"), at(0)));
    }

    #[test]
    fn an_ended_run_is_kept_and_read_back_as_it_ended() {
        let ended = EndedRun {
            verdict: EndedAs::Passed,
            note: "Tasks Completed: 3".to_owned(),
            results: BTreeMap::from([("IMAGE_DIGEST".to_owned(), json!("sha256:a"))]),
            view: RunView::default(),
        };
        let detail = WatchDetail {
            window_closes_at: at(600).to_string(),
            matched: BTreeMap::from([(
                "server".to_owned(),
                Matched {
                    pipeline_run: "server-x".to_owned(),
                    uid: "u".to_owned(),
                    ended: Some(ended.clone()),
                },
            )]),
            results_read_at: Some(at(0).to_string()),
        };
        let back: WatchDetail =
            serde_json::from_value(serde_json::to_value(&detail).expect("serializes"))
                .expect("parses");
        assert_eq!(back, detail);
        let RunState::Ended(e) = ended.state() else {
            panic!("ended");
        };
        assert_eq!(e.verdict, Verdict::Passed);
        let old: WatchDetail = serde_json::from_value(json!({
            "window_closes_at": at(600).to_string(),
            "matched": {"server": {"pipeline_run": "server-x", "uid": "u"}}
        }))
        .expect("a detail from before the cache parses");
        assert!(old.matched["server"].ended.is_none());
    }
}
