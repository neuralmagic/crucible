//! Agent-sized views of a workload: pod states and roles, the pods worth a look, whether a wait
//! is over, what to do next, and how a log budget is shared between pods.

use crate::kueue::Verdict;
use crate::manifest::WorkloadKind;
use crate::status::{Phase, Readiness, Rollup};
use k8s_openapi::api::core::v1::{ContainerStatus, Pod};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Waiting reasons a container does not leave without a new manifest or outside help.
const STUCK_REASONS: &[&str] = &[
    "ImagePullBackOff",
    "ErrImagePull",
    "InvalidImageName",
    "CrashLoopBackOff",
    "CreateContainerConfigError",
    "CreateContainerError",
    "RunContainerError",
];
const MAX_TERMINATION_MESSAGE: usize = 512;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Detail {
    /// Phase, reason, queue position, pod counts, the pods with problems, recent events, and the
    /// next action.
    #[default]
    Summary,
    /// The summary plus every Workload's conditions and admission, the object's full status, and
    /// every pod.
    Full,
}

/// A point in a workload's life to wait for. Each is reached once the workload is there or past
/// it, so a Job that already finished has been admitted, running and ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Until {
    /// Kueue admitted every Workload.
    Admitted,
    /// Admitted and at least one pod is running.
    Running,
    /// Serving kinds: every replica ready. Jobs and RayJobs: every pod ready or succeeded.
    Ready,
    /// Kueue marked it finished (Jobs and RayJobs).
    Finished,
}

/// Why a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitEnd {
    Reached,
    Timeout,
    /// Finished or stopped short of the wanted point; waiting longer cannot help.
    Settled,
    /// A pod is stuck (image pull, crash loop, bad config); waiting longer cannot help.
    Stuck,
}

pub fn pod_name(p: &Pod) -> &str {
    p.metadata.name.as_deref().unwrap_or_default()
}

/// The pod's phase, or `Terminating` once it is being deleted.
pub fn pod_state(p: &Pod) -> &str {
    if p.metadata.deletion_timestamp.is_some() {
        return "Terminating";
    }
    p.status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("Unknown")
}

fn pod_ready(p: &Pod) -> bool {
    p.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

fn label<'a>(p: &'a Pod, key: &str) -> Option<&'a str> {
    p.metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(key))
        .map(String::as_str)
}

/// What a pod does in its workload, from the labels its controller sets.
pub fn role(kind: WorkloadKind, p: &Pod) -> &'static str {
    match kind {
        WorkloadKind::Job | WorkloadKind::JobSet => "job",
        WorkloadKind::PyTorchJob => match label(p, "training.kubeflow.org/replica-type") {
            Some(t) if t.eq_ignore_ascii_case("master") => "master",
            _ => "worker",
        },
        WorkloadKind::Deployment | WorkloadKind::StatefulSet => "replica",
        WorkloadKind::RayJob => match label(p, "ray.io/node-type") {
            Some("head") => "head",
            Some("worker") => "worker",
            _ if label(p, "batch.kubernetes.io/job-name").is_some()
                || label(p, "job-name").is_some() =>
            {
                "submitter"
            }
            _ => "pod",
        },
        WorkloadKind::LeaderWorkerSet => {
            match label(p, "leaderworkerset.sigs.k8s.io/worker-index") {
                Some("0") => "leader",
                _ => "worker",
            }
        }
    }
}

pub fn pod_counts(pods: &[Pod]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for p in pods {
        *counts.entry(pod_state(p).to_string()).or_insert(0) += 1;
    }
    counts
}

/// A pod that needs a look, with the evidence: why it is waiting or how its container ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProblemPod {
    pub name: String,
    pub role: &'static str,
    pub phase: String,
    pub reason: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub restarts: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The container's termination message (`lastState.terminated.message` after a restart).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_message: Option<String>,
    /// Lower is worse: stuck, failed, restarted, unschedulable.
    #[serde(skip)]
    pub severity: u8,
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn nonempty(s: Option<&String>) -> Option<String> {
    s.filter(|m| !m.trim().is_empty())
        .map(|m| clip(m.trim(), MAX_TERMINATION_MESSAGE))
}

pub fn problem(kind: WorkloadKind, p: &Pod) -> Option<ProblemPod> {
    let status = p.status.as_ref();
    let containers: Vec<&ContainerStatus> = status
        .into_iter()
        .flat_map(|s| {
            s.init_container_statuses
                .iter()
                .flatten()
                .chain(s.container_statuses.iter().flatten())
        })
        .collect();
    let restarts = containers.iter().map(|c| c.restart_count).sum();
    let base = |severity: u8, reason: String, message: String| ProblemPod {
        name: pod_name(p).to_string(),
        role: role(kind, p),
        phase: pod_state(p).to_string(),
        reason,
        message,
        container: None,
        restarts,
        exit_code: None,
        termination_message: None,
        severity,
    };
    let last_message = |c: &ContainerStatus| {
        c.last_state
            .as_ref()
            .and_then(|s| s.terminated.as_ref())
            .and_then(|t| nonempty(t.message.as_ref()))
    };
    for c in &containers {
        let Some(w) = c.state.as_ref().and_then(|s| s.waiting.as_ref()) else {
            continue;
        };
        let reason = w.reason.clone().unwrap_or_default();
        if STUCK_REASONS.contains(&reason.as_str()) {
            let last = c.last_state.as_ref().and_then(|s| s.terminated.as_ref());
            return Some(ProblemPod {
                container: Some(c.name.clone()),
                exit_code: last.map(|t| t.exit_code),
                termination_message: last_message(c),
                ..base(0, reason, w.message.clone().unwrap_or_default())
            });
        }
    }
    for c in &containers {
        let Some(t) = c.state.as_ref().and_then(|s| s.terminated.as_ref()) else {
            continue;
        };
        if t.exit_code != 0 {
            return Some(ProblemPod {
                container: Some(c.name.clone()),
                exit_code: Some(t.exit_code),
                termination_message: nonempty(t.message.as_ref()).or_else(|| last_message(c)),
                ..base(
                    1,
                    t.reason.clone().unwrap_or_else(|| "Error".into()),
                    String::new(),
                )
            });
        }
    }
    if pod_state(p) == "Failed" {
        return Some(base(
            1,
            status
                .and_then(|s| s.reason.clone())
                .unwrap_or_else(|| "Failed".into()),
            status.and_then(|s| s.message.clone()).unwrap_or_default(),
        ));
    }
    for c in &containers {
        let Some(t) = c.last_state.as_ref().and_then(|s| s.terminated.as_ref()) else {
            continue;
        };
        if c.restart_count > 0 {
            return Some(ProblemPod {
                container: Some(c.name.clone()),
                exit_code: Some(t.exit_code),
                termination_message: last_message(c),
                ..base(
                    2,
                    format!(
                        "Restarted ({})",
                        t.reason.clone().unwrap_or_else(|| "Error".into())
                    ),
                    String::new(),
                )
            });
        }
    }
    let unschedulable = status.and_then(|s| s.conditions.as_ref()).and_then(|c| {
        c.iter()
            .find(|c| c.type_ == "PodScheduled" && c.status == "False")
    });
    if let (Some(c), "Pending") = (unschedulable, pod_state(p)) {
        return Some(base(
            3,
            c.reason.clone().unwrap_or_else(|| "Unschedulable".into()),
            c.message.clone().unwrap_or_default(),
        ));
    }
    None
}

/// Every pod with a problem, worst first.
pub fn problem_pods(kind: WorkloadKind, pods: &[Pod]) -> Vec<ProblemPod> {
    let mut problems: Vec<ProblemPod> = pods.iter().filter_map(|p| problem(kind, p)).collect();
    problems.sort_by(|a, b| (a.severity, &a.name).cmp(&(b.severity, &b.name)));
    problems
}

pub fn stuck(problems: &[ProblemPod]) -> bool {
    problems.iter().any(|p| p.severity == 0)
}

/// Whether the workload is at `until` or past it.
pub fn reached(
    until: Until,
    kind: WorkloadKind,
    rollup: &Rollup,
    readiness: Option<Readiness>,
    pods: &[Pod],
) -> bool {
    let phase = rollup.headline.phase;
    let finished = phase == Phase::Finished;
    let admitted = phase == Phase::Admitted;
    match until {
        Until::Finished => finished,
        Until::Admitted => finished || admitted,
        Until::Running => finished || (admitted && pods.iter().any(|p| pod_state(p) == "Running")),
        Until::Ready if kind.is_batch() => {
            finished
                || (admitted
                    && !pods.is_empty()
                    && pods
                        .iter()
                        .all(|p| pod_ready(p) || pod_state(p) == "Succeeded"))
        }
        Until::Ready => readiness.is_some_and(|r| r.ready),
    }
}

/// Finished or stopped: nothing changes without the caller acting.
pub fn settled(rollup: &Rollup) -> bool {
    matches!(rollup.headline.phase, Phase::Finished | Phase::Stopped)
}

/// What the status says, for choosing the next action.
pub struct Situation<'a> {
    pub kind: WorkloadKind,
    pub rollup: &'a Rollup,
    pub readiness: Option<Readiness>,
    pub fit: Option<Verdict>,
    pub position: Option<i64>,
    pub problems: &'a [ProblemPod],
    pub retention_seconds: Option<i64>,
}

pub fn next_action(s: &Situation<'_>) -> String {
    let head = &s.rollup.headline;
    let position = s
        .position
        .map(|n| format!(" (position {n} in its LocalQueue)"))
        .unwrap_or_default();
    let problem = s.problems.first();
    match head.phase {
        Phase::NoWorkload => "Kueue has not created a Workload for it yet. Check again in a few seconds; if that persists, the LocalQueue is missing or Kueue does not manage this kind (queues_list).".into(),
        Phase::Pending => match s.fit {
            Some(Verdict::NeverFits) => "It can never be admitted: its requests exceed what the ClusterQueue can hold even when idle. Delete it and resubmit smaller or on another queue (workload_plan's fit shows the numbers).".into(),
            Some(Verdict::WaitsForQuota) => format!("Waiting for quota other workloads hold{position}. Wait with workload_status until=admitted, or delete it and resubmit smaller."),
            Some(Verdict::FitsNow) => format!("Quota is free{position}; Kueue should admit it shortly. Wait with workload_status until=admitted."),
            None => format!("Waiting in the queue{position}: {}. Wait with workload_status until=admitted.", head.message),
        },
        Phase::QuotaReserved => "Quota is reserved; waiting on admission checks. Wait with workload_status until=admitted.".into(),
        Phase::Evicted => format!("Evicted ({}): {}. Kueue requeues it by itself; wait with workload_status until=admitted.", head.reason, head.message),
        Phase::Stopped => "Stopped. workload_resume returns it to its queue; workload_delete removes it.".into(),
        Phase::Admitted => match problem {
            Some(p) if p.severity == 0 => format!("Pod {} is stuck ({}); it will not recover by waiting. Fix the manifest and resubmit under a new name; workload_logs shows its output.", p.name, p.reason),
            Some(p) => format!("Pod {} has a problem ({}). Read workload_logs, which puts failed pods first.", p.name, p.reason),
            None if s.kind.is_batch() => "Running. Wait with workload_status until=finished, then read workload_logs.".into(),
            None if s.readiness.is_some_and(|r| r.ready) => "Ready and serving.".into(),
            None => "Admitted; replicas are starting. Wait with workload_status until=ready.".into(),
        },
        Phase::Finished => {
            let gone = s
                .retention_seconds
                .map(|n| format!(" It is deleted {n}s after it finished, so read what you need before then."))
                .unwrap_or_default();
            if head.reason == "Succeeded" {
                format!("Finished successfully. Read workload_logs for its output.{gone}")
            } else {
                format!("Finished and failed ({}): {}. See problem_pods and workload_logs.{gone}", head.reason, head.message)
            }
        }
    }
}

/// Order for spending a log budget: pods with problems, then the pods that coordinate (Ray head
/// and submitter, LWS leader, PyTorch master), then the rest.
pub fn log_priority(kind: WorkloadKind, p: &Pod) -> u8 {
    if problem(kind, p).is_some_and(|x| x.severity <= 2) {
        return 0;
    }
    match role(kind, p) {
        "head" | "submitter" | "leader" | "master" => 1,
        _ => 2,
    }
}

/// The lines of `text` containing `pattern`.
pub fn grep(text: &str, pattern: &str) -> String {
    text.lines()
        .filter(|l| l.contains(pattern))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Shares `total` bytes between logs of `sizes`: a log smaller than its even share keeps all of
/// it, and what it leaves goes to the larger ones.
pub fn budget(sizes: &[usize], total: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by_key(|&i| sizes[i]);
    let mut grants = vec![0; sizes.len()];
    let mut left = total;
    for (n, &i) in order.iter().enumerate() {
        let share = left / (sizes.len() - n);
        grants[i] = sizes[i].min(share);
        left -= grants[i];
    }
    grants
}

/// The last `keep` bytes of `text`, starting on a line boundary when one is near, and how many
/// bytes were dropped from the front.
pub fn tail(text: &str, keep: usize) -> (&str, usize) {
    if text.len() <= keep {
        return (text, 0);
    }
    let mut start = text.len() - keep;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    if let Some(i) = text[start..].find('\n')
        && i < keep / 4
    {
        start += i + 1;
    }
    (&text[start..], start)
}

/// The scalar fields of an object's status (counts, times, states), without its conditions.
pub fn scalar_status(status: Option<&Value>) -> Value {
    let Some(Value::Object(map)) = status else {
        return Value::Null;
    };
    let scalars: Map<String, Value> = map
        .iter()
        .filter(|(_, v)| !v.is_object() && !v.is_array())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Value::Object(scalars)
}

/// The container to read when the caller named none: the pod's default container when it has
/// several, else the API server's choice.
pub fn default_container(p: &Pod) -> Option<String> {
    let containers = &p.spec.as_ref()?.containers;
    if containers.len() < 2 {
        return None;
    }
    p.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("kubectl.kubernetes.io/default-container"))
        .cloned()
        .or_else(|| containers.first().map(|c| c.name.clone()))
}

#[cfg(test)]
mod tests {
    use crate::report::*;
    use crate::status::PhaseReport;
    use serde_json::json;

    fn pod(v: Value) -> Pod {
        serde_json::from_value(v).unwrap()
    }

    fn rollup(phase: Phase, reason: &str) -> Rollup {
        Rollup {
            headline: PhaseReport {
                phase,
                reason: reason.into(),
                message: "msg".into(),
            },
            counts: BTreeMap::new(),
        }
    }

    fn running(name: &str, ready: bool) -> Pod {
        pod(json!({
            "metadata": {"name": name},
            "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}],
                "containerStatuses": [{"name": "main", "image": "i", "imageID": "", "ready": ready, "restartCount": 0, "state": {"running": {}}}]}
        }))
    }

    fn crashlooping() -> Pod {
        pod(json!({
            "metadata": {"name": "crash"},
            "status": {"phase": "Running", "containerStatuses": [{
                "name": "main", "image": "i", "imageID": "", "ready": false, "restartCount": 4,
                "state": {"waiting": {"reason": "CrashLoopBackOff", "message": "back-off 40s"}},
                "lastState": {"terminated": {"exitCode": 2, "reason": "Error", "message": "ModuleNotFoundError: torch\n"}}
            }]}
        }))
    }

    fn failed() -> Pod {
        pod(json!({
            "metadata": {"name": "failed"},
            "status": {"phase": "Failed", "containerStatuses": [{
                "name": "main", "image": "i", "imageID": "", "ready": false, "restartCount": 0,
                "state": {"terminated": {"exitCode": 137, "reason": "OOMKilled"}}
            }]}
        }))
    }

    fn unschedulable() -> Pod {
        pod(json!({
            "metadata": {"name": "pending"},
            "status": {"phase": "Pending", "conditions": [{"type": "PodScheduled", "status": "False",
                "reason": "Unschedulable", "message": "0/3 nodes have nvidia.com/gpu"}]}
        }))
    }

    #[test]
    fn a_crash_loop_is_stuck_and_carries_the_last_termination_message() {
        let p = problem(WorkloadKind::Job, &crashlooping()).unwrap();
        assert_eq!(p.severity, 0);
        assert_eq!(p.reason, "CrashLoopBackOff");
        assert_eq!(p.exit_code, Some(2));
        assert_eq!(p.restarts, 4);
        assert_eq!(
            p.termination_message.as_deref(),
            Some("ModuleNotFoundError: torch")
        );
        assert!(stuck(&[p]));
    }

    #[test]
    fn failed_exits_and_unschedulable_pods_are_problems_in_that_order() {
        let pods = vec![unschedulable(), running("ok", true), failed()];
        let problems = problem_pods(WorkloadKind::Job, &pods);
        let got: Vec<(&str, &str, Option<i32>)> = problems
            .iter()
            .map(|p| (p.name.as_str(), p.reason.as_str(), p.exit_code))
            .collect();
        assert_eq!(
            got,
            vec![
                ("failed", "OOMKilled", Some(137)),
                ("pending", "Unschedulable", None)
            ]
        );
        assert!(!stuck(&problems));
    }

    #[test]
    fn a_pod_that_restarted_and_recovered_is_still_worth_a_look() {
        let p = pod(json!({
            "metadata": {"name": "r"},
            "status": {"phase": "Running", "containerStatuses": [{
                "name": "main", "image": "i", "imageID": "", "ready": true, "restartCount": 1,
                "state": {"running": {}},
                "lastState": {"terminated": {"exitCode": 1, "reason": "Error"}}
            }]}
        }));
        let got = problem(WorkloadKind::Deployment, &p).unwrap();
        assert_eq!(got.severity, 2);
        assert_eq!(got.reason, "Restarted (Error)");
        assert!(problem(WorkloadKind::Job, &running("fine", true)).is_none());
    }

    #[test]
    fn long_termination_messages_are_clipped_on_a_char_boundary() {
        let long = "é".repeat(MAX_TERMINATION_MESSAGE);
        let clipped = clip(&long, MAX_TERMINATION_MESSAGE);
        assert!(clipped.len() <= MAX_TERMINATION_MESSAGE + "…".len());
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn pods_are_counted_by_state_and_deletion_reads_as_terminating() {
        let gone = pod(json!({
            "metadata": {"name": "gone", "deletionTimestamp": "2026-10-23T00:00:00Z"},
            "status": {"phase": "Running"}
        }));
        let counts = pod_counts(&[running("a", true), running("b", false), failed(), gone]);
        assert_eq!(
            counts,
            BTreeMap::from([
                ("Failed".to_string(), 1),
                ("Running".to_string(), 2),
                ("Terminating".to_string(), 1)
            ])
        );
    }

    #[test]
    fn roles_come_from_the_controllers_labels() {
        let labelled = |labels: Value| pod(json!({"metadata": {"name": "p", "labels": labels}}));
        assert_eq!(
            role(
                WorkloadKind::RayJob,
                &labelled(json!({"ray.io/node-type": "head"}))
            ),
            "head"
        );
        assert_eq!(
            role(
                WorkloadKind::RayJob,
                &labelled(json!({"ray.io/node-type": "worker"}))
            ),
            "worker"
        );
        assert_eq!(
            role(
                WorkloadKind::RayJob,
                &labelled(json!({"batch.kubernetes.io/job-name": "r"}))
            ),
            "submitter"
        );
        assert_eq!(
            role(
                WorkloadKind::LeaderWorkerSet,
                &labelled(json!({"leaderworkerset.sigs.k8s.io/worker-index": "0"}))
            ),
            "leader"
        );
        assert_eq!(
            role(
                WorkloadKind::LeaderWorkerSet,
                &labelled(json!({"leaderworkerset.sigs.k8s.io/worker-index": "2"}))
            ),
            "worker"
        );
        assert_eq!(role(WorkloadKind::Job, &labelled(json!({}))), "job");
        assert_eq!(role(WorkloadKind::JobSet, &labelled(json!({}))), "job");
        assert_eq!(
            role(
                WorkloadKind::PyTorchJob,
                &labelled(json!({"training.kubeflow.org/replica-type": "master"}))
            ),
            "master"
        );
        assert_eq!(
            role(
                WorkloadKind::PyTorchJob,
                &labelled(json!({"training.kubeflow.org/replica-type": "worker"}))
            ),
            "worker"
        );
        assert_eq!(
            role(WorkloadKind::StatefulSet, &labelled(json!({}))),
            "replica"
        );
    }

    #[test]
    fn failed_pods_then_coordinators_then_the_rest_get_the_log_budget_first() {
        let head = pod(json!({"metadata": {"name": "h", "labels": {"ray.io/node-type": "head"}}}));
        let worker =
            pod(json!({"metadata": {"name": "w", "labels": {"ray.io/node-type": "worker"}}}));
        assert_eq!(log_priority(WorkloadKind::RayJob, &failed()), 0);
        assert_eq!(log_priority(WorkloadKind::RayJob, &crashlooping()), 0);
        assert_eq!(log_priority(WorkloadKind::RayJob, &head), 1);
        assert_eq!(log_priority(WorkloadKind::RayJob, &worker), 2);
        assert_eq!(
            log_priority(WorkloadKind::RayJob, &unschedulable()),
            2,
            "an unscheduled pod has no log"
        );
    }

    #[test]
    fn waits_are_reached_at_their_point_or_past_it() {
        let job = WorkloadKind::Job;
        let pending = rollup(Phase::Pending, "Pending");
        let admitted = rollup(Phase::Admitted, "Admitted");
        let finished = rollup(Phase::Finished, "Failed");
        for until in [
            Until::Admitted,
            Until::Running,
            Until::Ready,
            Until::Finished,
        ] {
            assert!(!reached(until, job, &pending, None, &[]), "{until:?}");
            assert!(reached(until, job, &finished, None, &[]), "{until:?}");
        }
        assert!(reached(Until::Admitted, job, &admitted, None, &[]));
        assert!(!reached(Until::Running, job, &admitted, None, &[]));
        let starting = [running("a", true), running("b", false)];
        assert!(reached(Until::Running, job, &admitted, None, &starting));
        assert!(!reached(Until::Ready, job, &admitted, None, &starting));
        assert!(reached(
            Until::Ready,
            job,
            &admitted,
            None,
            &[running("a", true), running("b", true)]
        ));
        assert!(!reached(Until::Finished, job, &admitted, None, &starting));
    }

    #[test]
    fn a_serving_workload_is_ready_by_its_readiness() {
        let admitted = rollup(Phase::Admitted, "Admitted");
        let up = Readiness {
            ready: true,
            replicas: 2,
            ready_replicas: 2,
        };
        let down = Readiness {
            ready: false,
            replicas: 2,
            ready_replicas: 1,
        };
        let d = WorkloadKind::Deployment;
        assert!(reached(Until::Ready, d, &admitted, Some(up), &[]));
        assert!(!reached(Until::Ready, d, &admitted, Some(down), &[]));
        assert!(!reached(Until::Finished, d, &admitted, Some(up), &[]));
    }

    #[test]
    fn finished_and_stopped_workloads_are_settled() {
        assert!(settled(&rollup(Phase::Finished, "Succeeded")));
        assert!(settled(&rollup(Phase::Stopped, "Inactive")));
        assert!(!settled(&rollup(Phase::Pending, "Pending")));
        assert!(!settled(&rollup(Phase::Evicted, "Preempted")));
    }

    fn action(phase: Phase, reason: &str, fit: Option<Verdict>, problems: &[ProblemPod]) -> String {
        next_action(&Situation {
            kind: WorkloadKind::Job,
            rollup: &rollup(phase, reason),
            readiness: None,
            fit,
            position: Some(3),
            problems,
            retention_seconds: Some(3600),
        })
    }

    #[test]
    fn the_next_action_follows_the_phase_the_fit_and_the_pods() {
        assert!(
            action(Phase::Pending, "Pending", Some(Verdict::NeverFits), &[])
                .starts_with("It can never be admitted")
        );
        let waits = action(Phase::Pending, "Pending", Some(Verdict::WaitsForQuota), &[]);
        assert!(waits.contains("position 3"), "{waits}");
        assert!(waits.contains("until=admitted"), "{waits}");
        let stuck = problem(WorkloadKind::Job, &crashlooping()).unwrap();
        let crash = action(Phase::Admitted, "Admitted", None, &[stuck]);
        assert!(
            crash.contains("Pod crash is stuck (CrashLoopBackOff)"),
            "{crash}"
        );
        assert!(action(Phase::Admitted, "Admitted", None, &[]).contains("until=finished"));
        let done = action(Phase::Finished, "Succeeded", None, &[]);
        assert!(done.starts_with("Finished successfully"), "{done}");
        assert!(done.contains("deleted 3600s after"), "{done}");
        let failed = action(Phase::Finished, "Failed", None, &[]);
        assert!(
            failed.starts_with("Finished and failed (Failed)"),
            "{failed}"
        );
        assert!(action(Phase::Stopped, "Inactive", None, &[]).contains("workload_resume"));
        assert!(action(Phase::NoWorkload, "", None, &[]).contains("queues_list"));
    }

    #[test]
    fn grep_keeps_matching_lines() {
        assert_eq!(
            grep("a ERROR 1\nok\nERROR 2\n", "ERROR"),
            "a ERROR 1\nERROR 2"
        );
        assert_eq!(grep("nothing", "ERROR"), "");
    }

    #[test]
    fn the_budget_lets_small_logs_keep_everything_and_shares_the_rest() {
        assert_eq!(
            budget(&[100, 10_000, 10_000], 4_100),
            vec![100, 2_000, 2_000]
        );
        assert_eq!(budget(&[10, 20], 1_000), vec![10, 20]);
        assert_eq!(budget(&[], 1_000), Vec::<usize>::new());
        let grants = budget(&[5_000, 7_000, 9_000, 50], 16_384);
        assert!(grants.iter().sum::<usize>() <= 16_384);
        assert_eq!(grants[3], 50);
    }

    #[test]
    fn a_tail_keeps_the_end_on_a_line_boundary() {
        let text = "first line\nsecond line\nthird line\n";
        assert_eq!(tail(text, 1_000), (text, 0));
        let (kept, omitted) = tail(text, 14);
        assert_eq!(kept, "third line\n");
        assert_eq!(omitted, text.len() - kept.len());
        let (kept, omitted) = tail("ééé", 3);
        assert_eq!(kept, "é");
        assert_eq!(omitted, 4);
    }

    #[test]
    fn scalar_status_drops_conditions_and_nested_objects() {
        let status = json!({"succeeded": 1, "startTime": "t", "conditions": [{"type": "Complete"}], "uncountedTerminatedPods": {}});
        assert_eq!(
            scalar_status(Some(&status)),
            json!({"succeeded": 1, "startTime": "t"})
        );
        assert_eq!(scalar_status(None), Value::Null);
    }

    #[test]
    fn the_default_container_is_named_only_when_a_pod_has_several() {
        let one = pod(json!({"metadata": {"name": "p"}, "spec": {"containers": [{"name": "a"}]}}));
        assert_eq!(default_container(&one), None);
        let two = pod(
            json!({"metadata": {"name": "p"}, "spec": {"containers": [{"name": "a"}, {"name": "b"}]}}),
        );
        assert_eq!(default_container(&two).as_deref(), Some("a"));
        let annotated = pod(json!({
            "metadata": {"name": "p", "annotations": {"kubectl.kubernetes.io/default-container": "b"}},
            "spec": {"containers": [{"name": "a"}, {"name": "b"}]}
        }));
        assert_eq!(default_container(&annotated).as_deref(), Some("b"));
    }
}
