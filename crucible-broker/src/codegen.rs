//! Code-gen GPU tools: `build` / `benchmark` / `lm_eval` / `fetch_log`. The agent reaches GPUs only
//! through these; measurement commands are frozen server-side and jobs run Kueue-admitted.

mod config;

use crate::build::sync_sandbox;
use base64::Engine;
pub(crate) use config::{
    BuildMode, Objective, ProfileCfg, ToolsConfig, ToolsOverlay, resolve_int_kwarg,
};
use forge::kube::{JobResult, KubeTarget, KueueStatus, PodStatus};
use forge::measure_job::{GpuJobRun, GpuJobSpec, PvcMount, run_gpu_job_with};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RESULT_SENTINEL: &str = "___CRUCIBLE_RESULT___";
const JOB_OUT_PATH: &str = "/tmp/codegen-out.json";
/// Where a profile job writes its trace artifact (the configured extension is appended) when NO
/// artifacts PVC is configured. The trace then leaves the pod base64'd after the sentinel, which
/// only works for small traces; see [`FALLBACK_TRACE_MAX_BYTES`].
const JOB_TRACE_BASE: &str = "/tmp/codegen-trace";
/// Ceiling for the base64-through-job-logs fallback. Log collection tails 2000 lines and base64
/// wraps at 76 chars (57 raw bytes/line), so ~114KB is the hard limit before the sentinel scrolls
/// off; 64KiB leaves headroom for the capture's own log lines. A real GPU trace is multi-MB;
/// that's what the artifacts PVC is for.
const FALLBACK_TRACE_MAX_BYTES: u64 = 64 * 1024;
/// Emitted (instead of the base64 tail) by a fallback profile job whose trace is too large to ride
/// the logs; the broker turns it into a readable configure-the-PVC error, never a truncated trace.
const TRACE_TOO_LARGE_MARKER: &str = "___CRUCIBLE_TRACE_TOO_LARGE___";
const REPS_ENV: &str = "CRUCIBLE_BENCH_REPS";
const LIMIT_ENV: &str = "CRUCIBLE_LM_EVAL_LIMIT";
/// Wait budget beyond the Job's own deadline: time spent suspended in the Kueue queue does not
/// tick `activeDeadlineSeconds`.
const QUEUE_SLACK: Duration = Duration::from_secs(1800);

/// How long one benchmark/lm_eval/profile CALL blocks before degrading to a `pending` reply. The
/// job itself keeps running on a detached worker (up to the Job deadline + [`QUEUE_SLACK`]); an
/// identical re-issue re-attaches to it. The default sits well above typical Kueue admission so
/// the common case still resolves inside a single call.
fn call_wait_budget() -> Duration {
    Duration::from_secs(env_u32("BROKER_CODEGEN_CALL_WAIT_SECONDS", 1200) as u64)
}

/// The next-step guidance a `pending` reply carries, so an agent doesn't invent sleep loops.
const PENDING_HINT: &str = "the job keeps running server-side; re-issue this exact call to \
    re-attach and keep waiting (a finished job replays from cache), or poll codegen_jobs / \
    fetch_log meanwhile";

/// GPU jobs remembered for `codegen_jobs`: the in-flight ones plus this many recent terminal ones,
/// so the agent can correlate after a blocking call returns.
const JOB_RING_CAP: usize = 20;

/// Shared across per-request server instances (stateless mode rebuilds `McpServer` per request);
/// constructed once from env at startup.
pub struct CodegenState {
    memo: Mutex<HashMap<String, Cached>>,
    /// Digests this broker's own `build()` produced; measures refuse anything else, since the frozen
    /// command is no protection inside an agent-chosen image.
    built: Mutex<HashSet<String>>,
    /// The durable tier under `memo`/`built`: everything in them dies with the pod, and repaying a
    /// finished 90-minute measure because the pod restarted is the scar this closes.
    steps: forge::steps::StepLedger,
    budget: Budget,
    logs: LogStore,
    /// The GPU jobs THIS broker submitted (a bounded ring), the only jobs `codegen_jobs` reports.
    jobs: Mutex<VecDeque<TrackedJob>>,
    /// Measure/profile jobs a worker thread is still driving, keyed by memo key: an identical
    /// re-issued call attaches here instead of submitting a duplicate Kueue job.
    inflight: Mutex<HashMap<String, Arc<InflightJob>>>,
}

/// One detached measure/profile job between "the call stopped waiting" and "the worker collected
/// the result". Waiters block on `done`; the worker memoizes a success BEFORE filling `reply` and
/// removing the map entry, so an attach miss followed by a memo miss proves nothing is running.
struct InflightJob {
    job: String,
    log: String,
    reply: Mutex<Option<CodegenReply>>,
    done: Condvar,
}

impl InflightJob {
    fn new(job: String, log: String) -> Self {
        Self {
            job,
            log,
            reply: Mutex::new(None),
            done: Condvar::new(),
        }
    }
    fn finish(&self, reply: CodegenReply) {
        if let Ok(mut slot) = self.reply.lock() {
            *slot = Some(reply);
        }
        self.done.notify_all();
    }
    /// Wait up to `budget` for the worker's reply; a timeout degrades to `pending` naming the
    /// live job and its log handle (the job keeps running).
    fn wait(&self, budget: Duration) -> CodegenReply {
        let poisoned = || CodegenReply::Error {
            error: "in-flight job state poisoned".into(),
        };
        let Ok(guard) = self.reply.lock() else {
            return poisoned();
        };
        let Ok((slot, _)) = self.done.wait_timeout_while(guard, budget, |r| r.is_none()) else {
            return poisoned();
        };
        match &*slot {
            Some(reply) => reply.clone(),
            None => CodegenReply::Pending {
                job: self.job.clone(),
                log: self.log.clone(),
                waited_secs: budget.as_secs(),
                hint: PENDING_HINT,
            },
        }
    }
}

/// One submitted GPU job as `codegen_jobs` remembers it. `result` flips from `None` (in flight)
/// when the blocking call collects the terminal state.
#[derive(Clone)]
struct TrackedJob {
    name: String,
    kind: &'static str,
    digest: String,
    namespace: String,
    queue_name: String,
    started_at: u64,
    log: String,
    result: Option<JobResult>,
}

/// A settled result, memoized in-process and recorded in the step ledger. Only facts of the key
/// live here: a crashed or timed-out job is never cached, so it re-runs.
#[derive(Clone, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Cached {
    Build {
        digest: String,
        log: String,
    },
    Measure {
        metrics: BTreeMap<String, f64>,
        logs: Vec<String>,
    },
    Profile {
        trace: String,
        logs: Vec<String>,
    },
}

impl CodegenState {
    pub fn new() -> Self {
        Self::with_steps(crate::steps::ledger())
    }

    fn with_steps(steps: forge::steps::StepLedger) -> Self {
        Self {
            memo: Mutex::new(HashMap::new()),
            built: Mutex::new(HashSet::new()),
            steps,
            budget: Budget::from_env(),
            logs: LogStore::from_env(),
            jobs: Mutex::new(VecDeque::new()),
            inflight: Mutex::new(HashMap::new()),
        }
    }
    fn inflight_get(&self, key: &str) -> Option<Arc<InflightJob>> {
        self.inflight.lock().ok().and_then(|m| m.get(key).cloned())
    }
    fn inflight_insert(&self, key: String, job: Arc<InflightJob>) {
        if let Ok(mut m) = self.inflight.lock() {
            m.insert(key, job);
        }
    }
    fn inflight_remove(&self, key: &str) {
        if let Ok(mut m) = self.inflight.lock() {
            m.remove(key);
        }
    }
    /// Memo first, then the ledger: a hit there is work a previous broker process finished, so it
    /// is rehydrated into the memo (and into the provenance set, for a build) before it is served.
    fn get(&self, key: &str) -> Option<Cached> {
        if let Some(hit) = self.memo.lock().ok().and_then(|m| m.get(key).cloned()) {
            return Some(hit);
        }
        let recorded: Cached = self.steps.lookup(&crate::steps::key(key))?;
        if let Cached::Build { digest, .. } = &recorded {
            self.mark_built(digest);
        }
        if let Ok(mut m) = self.memo.lock() {
            m.insert(key.to_string(), recorded.clone());
        }
        Some(recorded)
    }
    /// Record before the memo: the caller consumes the value the moment this returns, and a
    /// recorded step is what makes that consumption survive the pod.
    fn put(&self, key: String, val: Cached) {
        if let Err(e) = self.steps.record(&crate::steps::key(key.clone()), &val) {
            eprintln!("warning: recording step {key:?} failed: {e:#}");
        }
        if let Ok(mut m) = self.memo.lock() {
            m.insert(key, val);
        }
    }
    fn mark_built(&self, digest: &str) {
        if let Ok(mut s) = self.built.lock() {
            s.insert(digest.to_string());
        }
    }
    /// A digest is ours if this process built it or if the ledger says a broker did. Without the
    /// ledger half, a restarted broker refuses to measure a digest that is already in the registry
    /// and forces a rebuild before any measure can run.
    fn is_built(&self, digest: &str) -> bool {
        if self.built.lock().is_ok_and(|s| s.contains(digest)) {
            return true;
        }
        let recorded = self
            .steps
            .scan::<Cached>(crate::steps::SCOPE)
            .iter()
            .any(|c| matches!(c, Cached::Build { digest: d, .. } if d == digest));
        if recorded {
            self.mark_built(digest);
        }
        recorded
    }
    fn record_job(&self, job: TrackedJob) {
        if let Ok(mut ring) = self.jobs.lock() {
            ring.push_back(job);
            while ring.len() > JOB_RING_CAP {
                ring.pop_front();
            }
        }
    }
    fn finish_job(&self, name: &str, result: JobResult) {
        if let Ok(mut ring) = self.jobs.lock()
            && let Some(job) = ring.iter_mut().find(|j| j.name == name)
        {
            job.result = Some(result);
        }
    }
    /// Newest first (the job the agent is waiting on tops the list).
    fn jobs_snapshot(&self) -> Vec<TrackedJob> {
        self.jobs
            .lock()
            .map(|ring| ring.iter().rev().cloned().collect())
            .unwrap_or_default()
    }
}

impl Default for CodegenState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CodegenReply {
    Built {
        tree_hash: String,
        /// Which tree the hash came from ("sandbox" | "local" | "legacy"), so a stale-source
        /// confusion is diagnosable from the reply instead of git archaeology.
        source: &'static str,
        digest: String,
        mode: BuildMode,
        cached: bool,
        /// The build-log handle (readable mid-build via fetch_log from a concurrent session).
        log: String,
    },
    Measured {
        metrics: BTreeMap<String, f64>,
        objective: Objective,
        logs: Vec<String>,
        cached: bool,
    },
    Profiled {
        trace: String,
        logs: Vec<String>,
        cached: bool,
    },
    /// The call's wait budget ran out before the Kueue job resolved; the job is STILL running
    /// server-side and an identical re-issue attaches to it rather than resubmitting.
    Pending {
        job: String,
        log: String,
        waited_secs: u64,
        hint: &'static str,
    },
    Unconfigured {
        reason: String,
    },
    JobFailed {
        reason: String,
        logs: Vec<String>,
    },
    RejectedKwarg {
        reason: String,
    },
    BudgetExhausted {
        reason: String,
    },
    Disabled {
        reason: String,
    },
    /// A delegated spoke could not be reached at submit. Serialized with the plain `error` status
    /// tag (the agent's contract), but naming the spoke and its reachability tier.
    #[serde(rename = "error")]
    SpokeUnreachable {
        spoke: String,
        tier: String,
        error: String,
    },
    Error {
        error: String,
    },
}

/// GPU-job substrate read from env on the loop pod (the deploy-side facts rather than the tool contract).
struct JobEnv {
    namespace: String,
    queue_name: String,
    pull_secret: Option<String>,
    cpu: String,
    mem_request: String,
    mem_limit: String,
    shm_size_gi: u32,
    active_deadline_seconds: i64,
    ttl_seconds: i32,
    pvc_mounts: Vec<PvcMount>,
    /// Output-only trace transport for profile jobs; NOT mounted on benchmark/lm_eval jobs (the
    /// measured jobs' spec stays minimal, and measurement inputs stay digest-only).
    artifacts: Option<ArtifactsPvc>,
    /// The submitting pod as Job owner (GC of orphans); None for spokes/cross-namespace.
    owner: Option<forge::measure_job::JobOwner>,
    /// The delegated spoke cluster, when BROKER_CODEGEN_KUBECONFIG is set. `None` = the ambient
    /// client, exactly as before.
    spoke: Option<SpokeEnv>,
}

/// A delegated spoke as the broker env names it: the kube target plus the name/tier quoted in
/// unreachable-spoke tool errors.
#[derive(Clone)]
struct SpokeEnv {
    name: String,
    tier: String,
    target: KubeTarget,
}

/// The spoke env the renderer projects next to the substrate: BROKER_CODEGEN_KUBECONFIG gates the
/// feature (the mounted spoke-kubeconfig Secret in a pod, a kubeconfig file path on a laptop);
/// BROKER_CODEGEN_KUBE_CONTEXT is the laptop-parity context selector.
fn spoke_from_env() -> Option<SpokeEnv> {
    let path = env_nonempty("BROKER_CODEGEN_KUBECONFIG")?;
    Some(SpokeEnv {
        name: env_or("BROKER_CODEGEN_CLUSTER", "spoke"),
        tier: env_or("BROKER_CODEGEN_CLUSTER_TIER", "public"),
        target: KubeTarget::Kubeconfig {
            path: Some(PathBuf::from(path)),
            context: env_nonempty("BROKER_CODEGEN_KUBE_CONTEXT"),
            proxy_url: env_nonempty("BROKER_CODEGEN_PROXY_URL"),
        },
    })
}

/// A PVC shared by the profile job and the loop pod: the job writes the trace under `mount_path`,
/// the broker collects it from `local_dir` (its own mount of the SAME claim).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtifactsPvc {
    claim_name: String,
    /// Where the PVC is mounted inside the profile job.
    mount_path: String,
    /// Where the SAME PVC is mounted on the loop pod (defaults to `mount_path`).
    local_dir: PathBuf,
}

impl JobEnv {
    fn from_env() -> Result<Self, String> {
        let namespace = env_req("BROKER_CODEGEN_NAMESPACE")?;
        // Optional prewarmed model-weights PVC: unset means the GPU jobs get NO weights mount
        // (kernel domains measure source, not a served model). Never a fallback claim name.
        let mut pvc_mounts = Vec::new();
        if let Some(model) = env_nonempty("BROKER_CODEGEN_MODEL_PVC") {
            pvc_mounts.push(PvcMount {
                claim_name: model,
                mount_path: env_or("BROKER_CODEGEN_MODEL_MOUNT", "/models"),
                read_only: true,
            });
        }
        if let Some(ws) = env_nonempty("BROKER_CODEGEN_WORKSPACE_PVC") {
            pvc_mounts.push(PvcMount {
                claim_name: ws,
                mount_path: env_or("BROKER_CODEGEN_WORKSPACE_MOUNT", "/workspace"),
                read_only: false,
            });
        }
        let owner = owner_from_env(&namespace, spoke_from_env().is_some());
        Ok(Self {
            namespace,
            queue_name: env_or("BROKER_CODEGEN_QUEUE", "crucible-measure"),
            pull_secret: env_nonempty("BROKER_CODEGEN_PULL_SECRET"),
            cpu: env_or("BROKER_CODEGEN_CPU", "16"),
            mem_request: env_or("BROKER_CODEGEN_MEM_REQUEST", "128Gi"),
            mem_limit: env_or("BROKER_CODEGEN_MEM_LIMIT", "200Gi"),
            shm_size_gi: env_u32("BROKER_CODEGEN_SHM_GI", 16),
            active_deadline_seconds: env_u32("BROKER_CODEGEN_DEADLINE_SECONDS", 5400) as i64,
            ttl_seconds: env_u32("BROKER_CODEGEN_TTL_SECONDS", 86400) as i32,
            pvc_mounts,
            artifacts: artifacts_pvc(
                env_nonempty("BROKER_CODEGEN_ARTIFACTS_PVC"),
                env_nonempty("BROKER_CODEGEN_ARTIFACTS_MOUNT"),
                env_nonempty("BROKER_CODEGEN_ARTIFACTS_DIR"),
            ),
            spoke: spoke_from_env(),
            owner,
        })
    }

    /// The kube target the GPU jobs go to: the spoke's, else ambient.
    fn target(&self) -> KubeTarget {
        self.spoke
            .as_ref()
            .map(|s| s.target.clone())
            .unwrap_or_default()
    }
}

/// The submitting pod as a Job owner, so orphaned GPU jobs garbage-collect with the loop pod
/// (a dead consumer must not keep a GPU busy). Only for hub-local jobs in the pod's OWN
/// namespace: an ownerReference cannot cross clusters (spokes) or namespaces. Identity rides
/// the downward-API env the renderer projects; absent env (older render, local run) = no owner,
/// exactly the old behavior.
fn owner_from_env(job_namespace: &str, spoke: bool) -> Option<forge::measure_job::JobOwner> {
    if spoke {
        return None;
    }
    let name = env_nonempty("CRUCIBLE_POD_NAME")?;
    let uid = env_nonempty("CRUCIBLE_POD_UID")?;
    let pod_namespace = env_nonempty("CRUCIBLE_POD_NAMESPACE")?;
    (pod_namespace == job_namespace).then_some(forge::measure_job::JobOwner { name, uid })
}

/// A reachability-class submission failure on a delegated spoke becomes the typed
/// unreachable-spoke reply naming the spoke and its reachability tier. Any other spoke failure
/// (a job failing, bad RBAC, a rejected spec) names the spoke but claims nothing about
/// reachability; hub-local failures stay plain errors.
fn submit_failure_reply(spoke: Option<&SpokeEnv>, error: String) -> CodegenReply {
    match spoke {
        Some(s) if is_reachability_failure(&error) => CodegenReply::SpokeUnreachable {
            spoke: s.name.clone(),
            tier: s.tier.clone(),
            error,
        },
        Some(s) => CodegenReply::Error {
            error: format!("spoke {}: {error}", s.name),
        },
        None => CodegenReply::Error { error },
    }
}

/// Whether a submit/poll failure looks like the spoke could not be REACHED (connect, timeout, DNS,
/// TLS, proxy), as opposed to a reachable spoke rejecting or failing the work. String-matched
/// because the error already crossed the stringly tool boundary by the time it gets here.
fn is_reachability_failure(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "connect",
        "connection refused",
        "connection reset",
        "timed out",
        "timeout",
        "dns",
        "failed to lookup",
        "tls",
        "certificate",
        "handshake",
        "proxy",
    ]
    .iter()
    .any(|n| e.contains(n))
}

/// `pvc` gates the whole feature; `mount` defaults to `/artifacts` and `dir` to the mount path
/// (the common case: the loop pod mounts the claim at the same place the job does).
fn artifacts_pvc(
    pvc: Option<String>,
    mount: Option<String>,
    dir: Option<String>,
) -> Option<ArtifactsPvc> {
    let claim_name = pvc?;
    let mount_path = mount.unwrap_or_else(|| "/artifacts".to_string());
    let local_dir = PathBuf::from(dir.unwrap_or_else(|| mount_path.clone()));
    Some(ArtifactsPvc {
        claim_name,
        mount_path,
        local_dir,
    })
}

pub(crate) fn build(state: &CodegenState, mode: &str) -> String {
    guard(|cfg| do_build(state, cfg, mode))
}

pub(crate) fn benchmark(
    state: &Arc<CodegenState>,
    digest: &str,
    toggles: &[(String, String)],
    reps: Option<u32>,
) -> String {
    guard(|cfg| run_benchmark(state, cfg, digest, toggles, reps))
}

pub(crate) fn lm_eval(state: &Arc<CodegenState>, digest: &str, limit: Option<u32>) -> String {
    guard(|cfg| run_lm_eval(state, cfg, digest, limit))
}

pub(crate) fn profile(state: &Arc<CodegenState>, digest: &str) -> String {
    guard(|cfg| run_profile(state, cfg, digest))
}

pub(crate) fn fetch_log(state: &CodegenState, handle: &str, offset: usize) -> String {
    match state.logs.read(handle, offset) {
        Ok((text, next_offset, total)) => json!({
            "status": "log",
            "text": text,
            "next_offset": next_offset,
            "total_bytes": total,
        })
        .to_string(),
        Err(e) => json!({"status": "error", "error": e}).to_string(),
    }
}

/// Binary-safe reader for a profile `trace` handle: `fetch_log` runs its bytes through
/// `from_utf8_lossy`, which corrupts a gzip artifact, so a trace is pulled base64'd in byte windows.
pub(crate) fn fetch_trace(state: &CodegenState, handle: &str, offset: usize) -> String {
    match state.logs.read_bytes(handle, offset) {
        Ok((bytes, next_offset, total)) => json!({
            "status": "trace",
            "base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "next_offset": next_offset,
            "total_bytes": total,
        })
        .to_string(),
        Err(e) => json!({"status": "error", "error": e}).to_string(),
    }
}

/// How long `codegen_jobs` waits on the whole live-lookup batch before degrading the in-flight
/// entries to `lifecycle: unknown`; a wedged apiserver must not hang the tool worker.
const LIVE_LOOKUP_DEADLINE: Duration = Duration::from_secs(5);

/// The jobs THIS broker submitted (bounded ring), each with a derived lifecycle, the Kueue
/// admission view, pod progress, and the live log handle. In-flight jobs get a BATCHED live
/// lookup (one Workload list per namespace, one label-filtered pod list per job) under a short
/// deadline; terminal ones answer from the recorded result (the Job may already be TTL-reaped).
pub(crate) fn jobs(state: &CodegenState) -> String {
    if !codegen_enabled() {
        return json_reply(&CodegenReply::Disabled {
            reason: "code-gen tools not enabled for this run (BROKER_CODEGEN unset)".into(),
        });
    }
    let snapshot = state.jobs_snapshot();
    let live = live_views(&snapshot);
    jobs_reply(&snapshot, |job| {
        live.get(&job.name).cloned().unwrap_or_default()
    })
    .to_string()
}

/// One batched cluster lookup for every in-flight job, bounded by [`LIVE_LOOKUP_DEADLINE`]. On
/// timeout the map comes back empty and the entries degrade to `unknown`. The lookup runs on a
/// detached thread so a hung apiserver call can't wedge the tool worker.
fn live_views(jobs: &[TrackedJob]) -> HashMap<String, LiveJobStatus> {
    let inflight: Vec<(String, String)> = jobs
        .iter()
        .filter(|j| j.result.is_none())
        .map(|j| (j.name.clone(), j.namespace.clone()))
        .collect();
    if inflight.is_empty() {
        return HashMap::new();
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The jobs this broker submits all go to one target (spoke or ambient), so one lookup
        // target serves the whole batch.
        let target = spoke_from_env().map(|s| s.target).unwrap_or_default();
        let mut kueue_by_ns: HashMap<String, HashMap<String, KueueStatus>> = HashMap::new();
        for (_, ns) in &inflight {
            if !kueue_by_ns.contains_key(ns) {
                kueue_by_ns.insert(
                    ns.clone(),
                    forge::kube::workload_statuses(&target, ns).unwrap_or_default(),
                );
            }
        }
        let mut out = HashMap::new();
        for (name, ns) in inflight {
            let kueue = kueue_by_ns
                .get(&ns)
                .and_then(|by_job| by_job.get(&name))
                .cloned();
            let pod = forge::kube::job_pod_status(&target, &ns, &name).unwrap_or_default();
            out.insert(name, LiveJobStatus { kueue, pod });
        }
        let _ = tx.send(out);
    });
    rx.recv_timeout(LIVE_LOOKUP_DEADLINE).unwrap_or_default()
}

/// What a live cluster lookup knows about an in-flight job. The default (nothing known) derives
/// `lifecycle: unknown`, the honest label for a vanished job or an unavailable cluster view.
#[derive(Clone, Default)]
struct LiveJobStatus {
    kueue: Option<KueueStatus>,
    pod: PodStatus,
}

/// Assemble the `codegen_jobs` reply (pure; `live` is only invoked for in-flight jobs, so tests
/// drive it with synthetic statuses and terminal entries never touch the cluster).
fn jobs_reply(jobs: &[TrackedJob], live: impl Fn(&TrackedJob) -> LiveJobStatus) -> Value {
    let entries: Vec<Value> = jobs
        .iter()
        .map(|job| {
            let (lifecycle, kueue, pod) = match job.result {
                Some(result) => (
                    derive_lifecycle(Some(result), None, None),
                    // A job that reached a terminal Job condition was admitted; a TimedOut one may
                    // have spent its whole life queued, so `admitted` is omitted (unknown), not
                    // asserted false.
                    if result == JobResult::TimedOut {
                        json!({"queue_name": job.queue_name})
                    } else {
                        json!({"queue_name": job.queue_name, "admitted": true})
                    },
                    Value::Null,
                ),
                None => {
                    let status = live(job);
                    let lifecycle =
                        derive_lifecycle(None, status.kueue.as_ref(), status.pod.phase.as_deref());
                    // No workload in sight ⇒ `admitted` is unknown, omitted rather than asserted.
                    let mut kueue = match status.kueue.as_ref() {
                        Some(k) => json!({
                            "queue_name": job.queue_name,
                            "admitted": k.admitted,
                        }),
                        None => json!({"queue_name": job.queue_name}),
                    };
                    if let Some(reason) =
                        status.kueue.as_ref().and_then(|k| k.pending_reason.clone())
                        && let Some(obj) = kueue.as_object_mut()
                    {
                        obj.insert("pending_reason".into(), Value::String(reason));
                    }
                    let pod = match status.pod.phase {
                        Some(phase) => json!({
                            "phase": phase,
                            "started_at": status.pod.started_at,
                        }),
                        None => Value::Null,
                    };
                    (lifecycle, kueue, pod)
                }
            };
            json!({
                "name": job.name,
                "kind": job.kind,
                "digest": job.digest,
                "started_at": job.started_at,
                "log": job.log,
                "lifecycle": lifecycle,
                "kueue": kueue,
                "pod": pod,
            })
        })
        .collect();
    json!({"status": "jobs", "jobs": entries})
}

/// One lifecycle label from what's known (pure). A recorded terminal result wins; then the pod
/// phase; then the Kueue admission view. A pod in ANY phase means Kueue unsuspended the Job, so a
/// pre-Running phase still reads `admitted`. Neither a workload nor a pod in sight is `unknown`
/// (a vanished/reaped job, or an unavailable cluster view), never claimed `queued`.
fn derive_lifecycle(
    result: Option<JobResult>,
    kueue: Option<&KueueStatus>,
    pod_phase: Option<&str>,
) -> &'static str {
    match result {
        Some(JobResult::Succeeded) => "succeeded",
        Some(JobResult::Failed) | Some(JobResult::TimedOut) => "failed",
        None => match pod_phase {
            Some("Succeeded") => "succeeded",
            Some("Failed") => "failed",
            Some("Running") => "running",
            Some(_) => "admitted",
            None => match kueue {
                Some(k) if k.admitted => "admitted",
                Some(_) => "queued",
                None => "unknown",
            },
        },
    }
}

fn codegen_enabled() -> bool {
    matches!(
        std::env::var("BROKER_CODEGEN").as_deref(),
        Ok("1") | Ok("true")
    )
}

fn guard(f: impl FnOnce(&ToolsConfig) -> CodegenReply) -> String {
    if !codegen_enabled() {
        return json_reply(&CodegenReply::Disabled {
            reason: "code-gen tools not enabled for this run (BROKER_CODEGEN unset)".into(),
        });
    }
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return json_reply(&CodegenReply::Disabled { reason: e }),
    };
    json_reply(&f(&cfg))
}

fn do_build(state: &CodegenState, cfg: &ToolsConfig, mode: &str) -> CodegenReply {
    let mode = match cfg.build.parse_mode(mode) {
        Ok(m) => m,
        Err(reason) => return CodegenReply::RejectedKwarg { reason },
    };
    match build_inner(state, cfg, mode) {
        Ok(r) => r,
        Err(e) => CodegenReply::Error { error: e },
    }
}

fn build_inner(
    state: &CodegenState,
    cfg: &ToolsConfig,
    mode: BuildMode,
) -> Result<CodegenReply, String> {
    let source = TreeSource::resolve()?;
    let tree_hash = source.tree_hash().to_string();
    let key = build_key(&tree_hash, mode, &cfg.build);
    if let Some(Cached::Build { digest, log }) = state.get(&key) {
        return Ok(CodegenReply::Built {
            tree_hash,
            source: source.label(),
            digest,
            mode,
            cached: true,
            log,
        });
    }

    let mut build_cfg = forge::BuildConfig::from_env()
        .map_err(|e| format!("build config (FORGE_REGISTRY / FORGE_AUTHFILE): {e:#}"))?;
    // The build context must come from the same tree the hash did.
    let ctx = source.context_dir()?;
    let tag = format!("codegen-{}", nonce());
    // The Dockerfile is staged OUTSIDE the context (buildah takes an absolute --file): writing it
    // into a local workspace would change the next tree hash.
    let dockerfile = forge::storage_root().join(format!("Dockerfile.{tag}"));
    write_derive_dockerfile(&dockerfile, &cfg.build, mode)?;
    build_cfg.dockerfile = dockerfile.to_string_lossy().into_owned();
    // The handle exists (empty) BEFORE buildah starts and the child writes through into its file,
    // so a concurrent fetch_log tails the build live instead of waiting for the end.
    let (log_handle, log_path) = state.logs.allocate("build", &tag)?;
    let outcome = forge::build_and_push_streaming(&build_cfg, &ctx, &tag, &log_path);
    let _ = std::fs::remove_file(&dockerfile);
    // Prune the isolated buildah storage on EVERY outcome (pushed, compile error, infra error): the
    // vfs layers are dead weight either way; a build leaves the full base+derive set behind, and a
    // few builds fill the volume. The registry holds the pushed artifact and the memo holds the
    // digest, so nothing local is needed again; re-pulling the base per build is the accepted cost.
    if let Err(e) = forge::prune_storage(&build_cfg) {
        eprintln!("warning: pruning codegen build storage failed: {e:#}");
    }
    match outcome.map_err(|e| format!("build/push: {e:#}"))? {
        forge::BuildOutcome::CompileError { .. } => Ok(CodegenReply::JobFailed {
            reason: "the derive-layer build failed (see log)".into(),
            logs: vec![log_handle],
        }),
        forge::BuildOutcome::Built { image_ref } => {
            let digest = forge::oci::pin_digest(&image_ref, Some(&build_cfg.authfile))
                .map_err(|e| format!("resolving pushed digest for {image_ref}: {e:#}"))?;
            state.put(
                key,
                Cached::Build {
                    digest: digest.clone(),
                    log: log_handle.clone(),
                },
            );
            state.mark_built(&digest);
            Ok(CodegenReply::Built {
                tree_hash,
                source: source.label(),
                digest,
                mode,
                cached: false,
                log: log_handle,
            })
        }
    }
}

fn write_derive_dockerfile(
    path: &PathBuf,
    build: &config::BuildCfg,
    mode: BuildMode,
) -> Result<(), String> {
    let install = build.install_for(mode)?;
    // Without --chown the copied source is root-owned while RUN executes as the base image's
    // non-root USER, so an editable install can't write into the tree.
    let chown = build
        .copy_chown
        .as_deref()
        .map(|o| format!(" --chown={o}"))
        .unwrap_or_default();
    let body = format!(
        "FROM {}\n\
         WORKDIR {}\n\
         COPY{chown} . {}\n\
         RUN {install}\n",
        build.base_image, build.src_dir, build.src_dir
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Where a candidate's source tree (and its hash) came from. The hash is the provenance half of
/// the tree↔digest contract, so the source is typed rather than inferred from flags.
enum TreeSource {
    /// The live sibling sandbox, hashed by in-sandbox `git write-tree` (the agent's mid-turn calls).
    LiveSandbox { path: String, tree_hash: String },
    /// The loop-pod checkout (the gate's post-turn call; the sandbox is deleted by then).
    LocalCheckout { dir: PathBuf, tree_hash: String },
    /// A non-git workdir hashed by sha256-of-find (the legacy no-git topology).
    LegacySandbox { path: String, tree_hash: String },
}

impl TreeSource {
    /// Liveness-first precedence: live sandbox > local checkout > legacy. A LIVE sandbox outranks
    /// the local checkout because mid-turn the agent's edits exist only in the sibling sandbox
    /// (the loop-pod checkout is frozen until the turn-end download-back). "No sandbox answered"
    /// is the gate's normal post-turn case, so that failure falls back loudly instead of erroring;
    /// an unhashable tree on every path is still a hard error.
    fn resolve() -> Result<Self, String> {
        match crate::build::sandbox_workdir() {
            Ok(sandbox_path) => match crate::build::sandbox_git_tree_hash(&sandbox_path) {
                Ok(tree_hash) => {
                    return Ok(Self::LiveSandbox {
                        path: sandbox_path,
                        tree_hash,
                    });
                }
                Err(e) => eprintln!(
                    "==> codegen_build: no live sandbox at {sandbox_path} ({e:#}); falling back"
                ),
            },
            Err(e) => eprintln!("==> codegen_build: {e:#}; falling back"),
        }
        let workdir = match env_nonempty("BROKER_CODEGEN_SANDBOX_WORKDIR") {
            Some(w) => w,
            None => crate::build::sandbox_workdir().map_err(|e| {
                format!(
                    "no sandbox workdir configured: {e:#} (or set BROKER_CODEGEN_SANDBOX_WORKDIR)"
                )
            })?,
        };
        let wrap = |e: String| {
            format!(
                "hashing the sandbox workspace (a candidate must trace to an exact source state): {e}"
            )
        };
        match local_workdir(&workdir) {
            Some(dir) => {
                let tree_hash = local_tree_hash(&dir).map_err(wrap)?;
                Ok(Self::LocalCheckout { dir, tree_hash })
            }
            None => {
                let tree_hash = crate::build::sandbox_tree_hash(&workdir)
                    .map(|h| hex_token(&h))
                    .map_err(|e| wrap(format!("{e:#}")))?;
                Ok(Self::LegacySandbox {
                    path: workdir,
                    tree_hash,
                })
            }
        }
    }

    fn tree_hash(&self) -> &str {
        match self {
            Self::LiveSandbox { tree_hash, .. }
            | Self::LocalCheckout { tree_hash, .. }
            | Self::LegacySandbox { tree_hash, .. } => tree_hash,
        }
    }

    /// The provenance tag the Built reply carries.
    fn label(&self) -> &'static str {
        match self {
            Self::LiveSandbox { .. } => "sandbox",
            Self::LocalCheckout { .. } => "local",
            Self::LegacySandbox { .. } => "legacy",
        }
    }

    /// The build context for this source. Git-hashed sources export the EXACT hashed tree
    /// (`git archive <tree>`), so the image content always equals the tree_hash: a file
    /// `.gitignore`d-but-not-`.dockerignore`d can't sneak into the COPY, and a sandbox edit
    /// racing the sync can't ship under the pre-race hash.
    fn context_dir(&self) -> Result<PathBuf, String> {
        match self {
            Self::LocalCheckout { dir, tree_hash } => export_tree(dir, tree_hash),
            Self::LiveSandbox { path, tree_hash } => {
                // The sync pulls .git too, so the write-tree's objects come along for the export.
                let synced = staging_dir("codegen-ctx-sync");
                sync_sandbox(path, &synced).map_err(|e| format!("syncing sandbox tree: {e:#}"))?;
                export_tree(&synced, tree_hash)
            }
            Self::LegacySandbox { path, .. } => {
                let ctx = staging_dir("codegen-ctx");
                sync_sandbox(path, &ctx).map_err(|e| format!("syncing sandbox tree: {e:#}"))?;
                Ok(ctx)
            }
        }
    }
}

/// A cleared staging dir under the build volume (BROKER_CODEGEN_CTX overrides the root for tests).
fn staging_dir(name: &str) -> PathBuf {
    env_nonempty("BROKER_CODEGEN_CTX")
        .map(|c| PathBuf::from(c).with_file_name(name))
        .unwrap_or_else(|| forge::storage_root().join(name))
}

/// Extract tree `tree_hash` from `repo`'s object db into a fresh staging dir (`git archive | tar`).
fn export_tree(repo: &Path, tree_hash: &str) -> Result<PathBuf, String> {
    let ctx = staging_dir("codegen-tree");
    let _ = std::fs::remove_dir_all(&ctx);
    std::fs::create_dir_all(&ctx).map_err(|e| format!("creating {}: {e}", ctx.display()))?;
    let tar = ctx.with_extension("tar");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", "--format=tar", "-o"])
        .arg(&tar)
        .arg(tree_hash)
        .output()
        .map_err(|e| format!("running git archive: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git archive {tree_hash}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let out = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&tar)
        .arg("-C")
        .arg(&ctx)
        .output()
        .map_err(|e| format!("running tar: {e}"))?;
    let _ = std::fs::remove_file(&tar);
    if !out.status.success() {
        return Err(format!(
            "extracting the hashed tree: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(ctx)
}

/// The workdir as a local git checkout (the shared-PVC topology), or `None` for the sandbox path.
fn local_workdir(workdir: &str) -> Option<PathBuf> {
    let p = PathBuf::from(workdir);
    (p.is_dir() && p.join(".git").exists()).then_some(p)
}

/// Hash the WORKING TREE (tracked + untracked + uncommitted edits) of a local checkout: `git add -A`
/// into a throwaway `GIT_INDEX_FILE`, then `git write-tree`. The repo's real index is never touched.
fn local_tree_hash(dir: &Path) -> Result<String, String> {
    let index = std::env::temp_dir().join(format!(
        "codegen-tree-index-{}-{}",
        std::process::id(),
        nonce()
    ));
    let result =
        run_git(dir, &index, &["add", "-A"]).and_then(|_| run_git(dir, &index, &["write-tree"]));
    let _ = std::fs::remove_file(&index);
    let hash = result?;
    if hash.is_empty() {
        return Err("git write-tree produced no tree hash".into());
    }
    Ok(hash)
}

fn run_git(dir: &Path, index: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .env("GIT_INDEX_FILE", index)
        .args(args)
        .output()
        .map_err(|e| format!("running git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_benchmark(
    state: &Arc<CodegenState>,
    cfg: &ToolsConfig,
    digest: &str,
    toggles: &[(String, String)],
    reps: Option<u32>,
) -> CodegenReply {
    let toggles = match cfg.benchmark.mutable_kwargs.validate_toggles(toggles) {
        Ok(t) => t,
        Err(reason) => return CodegenReply::RejectedKwarg { reason },
    };
    let reps = match resolve_int_kwarg("reps", reps, cfg.benchmark.mutable_kwargs.reps.as_ref()) {
        Ok(r) => r,
        Err(reason) => return CodegenReply::RejectedKwarg { reason },
    };
    match measure(state, cfg, digest, MeasureKind::Benchmark { toggles, reps }) {
        Ok(r) => r,
        Err(e) => CodegenReply::Error { error: e },
    }
}

fn run_lm_eval(
    state: &Arc<CodegenState>,
    cfg: &ToolsConfig,
    digest: &str,
    limit: Option<u32>,
) -> CodegenReply {
    let limit = match resolve_int_kwarg("limit", limit, cfg.lm_eval.mutable_kwargs.limit.as_ref()) {
        Ok(l) => l,
        Err(reason) => return CodegenReply::RejectedKwarg { reason },
    };
    match measure(state, cfg, digest, MeasureKind::LmEval { limit }) {
        Ok(r) => r,
        Err(e) => CodegenReply::Error { error: e },
    }
}

fn run_profile(state: &Arc<CodegenState>, cfg: &ToolsConfig, digest: &str) -> CodegenReply {
    let Some(pcfg) = &cfg.profile else {
        return CodegenReply::Unconfigured {
            reason: "profile is not configured for this rig/scenario".into(),
        };
    };
    match do_profile(state, cfg, pcfg, digest) {
        Ok(r) => r,
        Err(e) => CodegenReply::Error { error: e },
    }
}

/// Capture a GPU trace of a built candidate: same provenance gate, budget accounting, and Kueue job
/// shape as a benchmark (profiling holds GPUs; it counts). Memoized on (digest, "profile").
fn do_profile(
    state: &Arc<CodegenState>,
    cfg: &ToolsConfig,
    pcfg: &ProfileCfg,
    digest: &str,
) -> Result<CodegenReply, String> {
    if !state.is_built(digest) {
        return Err(format!(
            "digest {digest:?} was not produced by codegen_build in this broker's lifetime — run \
             codegen_build first and pass the digest it returns"
        ));
    }
    let key = measure_key(digest, "profile", &[]);
    // Attach before the memo check; same ordering contract as `measure`.
    if let Some(entry) = state.inflight_get(&key) {
        return Ok(entry.wait(call_wait_budget()));
    }
    if let Some(Cached::Profile { trace, logs }) = state.get(&key) {
        return Ok(CodegenReply::Profiled {
            trace,
            logs,
            cached: true,
        });
    }
    let token = crate::turn::current_token();
    if let Some(reason) = state.budget.precheck(&token)? {
        return Ok(CodegenReply::BudgetExhausted { reason });
    }

    let env = JobEnv::from_env()?;
    let name = format!("crucible-profile-{}", nonce());
    // Pre-allocated so a concurrent fetch_log tails the capture while the Job runs. Snapshots are
    // monotonic (full pod log, emitted only when longer), so the replace below never shrinks the
    // file under a tailer's offset; the final full log overwrites it once more below. On the
    // fallback transport the post-completion trim to the pre-sentinel part is the ONE deliberate
    // shrink (the base64 blob becomes the trace artifact rather than log text).
    let (log_handle, log_path) = state.logs.allocate("profile", digest)?;
    state.record_job(TrackedJob {
        name: name.clone(),
        kind: "profile",
        digest: digest.to_string(),
        namespace: env.namespace.clone(),
        queue_name: env.queue_name.clone(),
        started_at: epoch_secs(),
        log: log_handle.clone(),
        result: None,
    });
    let worker_state = Arc::clone(state);
    let worker_key = key.clone();
    let pcfg = pcfg.clone();
    let gpus = cfg.gpus;
    let digest = digest.to_string();
    let job_name = name.clone();
    let handle = log_handle.clone();
    let work = move || -> CodegenReply {
        let live = |snapshot: &str| {
            let _ = std::fs::write(&log_path, snapshot);
        };
        // Two trace transports: the artifacts PVC (the job writes $OUT onto a volume the broker
        // also mounts, the multi-MB path) or, unconfigured, base64 through the job log (small
        // traces only, guarded by FALLBACK_TRACE_MAX_BYTES).
        let run = match &env.artifacts {
            Some(a) => {
                let out_path = format!("{}/{}.{}", a.mount_path, job_name, pcfg.trace_ext);
                let mounts = vec![PvcMount {
                    claim_name: a.claim_name.clone(),
                    mount_path: a.mount_path.clone(),
                    read_only: false,
                }];
                submit(
                    &env.target(),
                    &job_spec(
                        &env,
                        gpus,
                        &digest,
                        &job_name,
                        &wrap_command_profile_pvc(&pcfg.command, &out_path),
                        &[],
                        &mounts,
                    ),
                    live,
                )
            }
            None => submit(
                &env.target(),
                &job_spec(
                    &env,
                    gpus,
                    &digest,
                    &job_name,
                    &wrap_command_profile(&pcfg.command, &pcfg.trace_ext),
                    &[],
                    &[],
                ),
                live,
            ),
        };
        let run = match run {
            Ok(run) => run,
            // A failed submission must not leave the tracked job "in flight" forever.
            Err(e) => {
                worker_state.finish_job(&job_name, JobResult::Failed);
                return submit_failure_reply(env.spoke.as_ref(), e);
            }
        };
        worker_state.finish_job(&job_name, run.result);
        collect_profile(
            &worker_state,
            run,
            &env,
            &pcfg,
            worker_key,
            handle,
            &job_name,
            &digest,
            gpus,
            &token,
        )
        .unwrap_or_else(|error| CodegenReply::Error { error })
    };
    let entry = Arc::new(InflightJob::new(name, log_handle));
    Ok(detach_and_wait(state, key, entry, call_wait_budget(), work))
}

/// The post-terminal tail of a profile job: budget accounting, trace collection over whichever
/// transport, memoization. Memo-before-map-removal, same as [`collect_measure`].
#[allow(clippy::too_many_arguments)]
fn collect_profile(
    state: &CodegenState,
    run: GpuJobRun,
    env: &JobEnv,
    pcfg: &ProfileCfg,
    key: String,
    log_handle: String,
    name: &str,
    digest: &str,
    gpus: u32,
    token: &str,
) -> Result<CodegenReply, String> {
    state.budget.record(token, gpu_minutes(run.elapsed, gpus))?;
    state.logs.overwrite(&log_handle, run.logs.as_bytes())?;

    if run.result != JobResult::Succeeded {
        if let Some(reason) = trace_too_large_reason(&run.logs) {
            return Err(reason);
        }
        return Ok(CodegenReply::JobFailed {
            reason: job_failure_reason(run.result),
            logs: vec![log_handle],
        });
    }
    let trace_handle = match &env.artifacts {
        Some(a) => {
            let src = a.local_dir.join(format!("{name}.{}", pcfg.trace_ext));
            collect_trace_artifact(&state.logs, &src, digest, &pcfg.trace_ext)?
        }
        None => {
            let (run_log, trace_bytes) = split_profile_output(&run.logs)?;
            // Keep the human-readable part only: the base64 payload is now the trace artifact.
            state.logs.overwrite(&log_handle, run_log.as_bytes())?;
            let (trace_handle, _) =
                state
                    .logs
                    .store_artifact("trace", digest, &pcfg.trace_ext, &trace_bytes)?;
            trace_handle
        }
    };
    state.put(
        key,
        Cached::Profile {
            trace: trace_handle.clone(),
            logs: vec![log_handle.clone()],
        },
    );
    Ok(CodegenReply::Profiled {
        trace: trace_handle,
        logs: vec![log_handle],
        cached: false,
    })
}

enum MeasureKind {
    Benchmark {
        toggles: Vec<(String, String)>,
        reps: Option<u32>,
    },
    LmEval {
        limit: Option<u32>,
    },
}

impl MeasureKind {
    fn tag(&self) -> &'static str {
        match self {
            MeasureKind::Benchmark { .. } => "bench",
            MeasureKind::LmEval { .. } => "lmeval",
        }
    }
}

/// Run `work` (a submitted GPU job driven to a terminal state) on a detached worker thread,
/// waiting up to the call budget for its reply. On budget exhaustion the caller gets `pending`
/// and the worker keeps going: a success lands in the memo (written inside `work`, before the
/// entry leaves the map) so a later identical call replays it; an identical call that arrives
/// while the worker is still driving attaches to `entry` instead of resubmitting.
fn detach_and_wait(
    state: &Arc<CodegenState>,
    key: String,
    entry: Arc<InflightJob>,
    wait: Duration,
    work: impl FnOnce() -> CodegenReply + Send + 'static,
) -> CodegenReply {
    state.inflight_insert(key.clone(), entry.clone());
    let worker_state = Arc::clone(state);
    let worker_entry = Arc::clone(&entry);
    let worker_key = key.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("codegen-{}", entry.job))
        .spawn(move || {
            let reply = work();
            worker_entry.finish(reply);
            worker_state.inflight_remove(&worker_key);
        });
    match spawned {
        Ok(_) => entry.wait(wait),
        // Spawn failure (resource exhaustion) drops `work` unsubmitted: unwind the tracking so
        // nothing looks in flight forever.
        Err(e) => {
            state.finish_job(&entry.job, JobResult::Failed);
            state.inflight_remove(&key);
            CodegenReply::Error {
                error: format!("spawning the measure worker thread: {e}"),
            }
        }
    }
}

fn measure(
    state: &Arc<CodegenState>,
    cfg: &ToolsConfig,
    digest: &str,
    kind: MeasureKind,
) -> Result<CodegenReply, String> {
    if !state.is_built(digest) {
        return Err(format!(
            "digest {digest:?} was not produced by codegen_build in this broker's lifetime — run \
             codegen_build first and pass the digest it returns"
        ));
    }
    let (command, objective, key, kwarg_env) = match &kind {
        MeasureKind::Benchmark { toggles, reps } => {
            let mut env: Vec<(String, String)> = toggles.clone();
            if let Some(r) = reps {
                env.push((REPS_ENV.to_string(), r.to_string()));
            }
            (
                cfg.benchmark.command.clone(),
                cfg.benchmark.objective.clone(),
                measure_key(digest, "benchmark", &env),
                env,
            )
        }
        MeasureKind::LmEval { limit } => {
            let mut env = Vec::new();
            if let Some(l) = limit {
                env.push((LIMIT_ENV.to_string(), l.to_string()));
            }
            (
                cfg.lm_eval.command.clone(),
                cfg.lm_eval.objective.clone(),
                measure_key(digest, "lm_eval", &env),
                env,
            )
        }
    };

    // Attach BEFORE the memo check: the worker memoizes before its entry leaves the map, so this
    // lookup order can't lose a result that lands between the two.
    if let Some(entry) = state.inflight_get(&key) {
        return Ok(entry.wait(call_wait_budget()));
    }
    if let Some(Cached::Measure { metrics, logs }) = state.get(&key) {
        return Ok(CodegenReply::Measured {
            metrics,
            objective,
            logs,
            cached: true,
        });
    }
    let token = crate::turn::current_token();
    if let Some(reason) = state.budget.precheck(&token)? {
        return Ok(CodegenReply::BudgetExhausted { reason });
    }

    let env = JobEnv::from_env()?;
    let name = format!("crucible-{}-{}", kind.tag(), nonce());
    // Pre-allocated so a concurrent fetch_log tails the run live. Snapshots are monotonic (full
    // pod log, emitted only when longer), so the replace below never shrinks the file under a
    // tailer's offset; the final full collection overwrites it once more (no poll gaps).
    let (handle, log_path) = state.logs.allocate(kind.tag(), digest)?;
    state.record_job(TrackedJob {
        name: name.clone(),
        kind: kind.tag(),
        digest: digest.to_string(),
        namespace: env.namespace.clone(),
        queue_name: env.queue_name.clone(),
        started_at: epoch_secs(),
        log: handle.clone(),
        result: None,
    });
    let tail = MeasureTail {
        key: key.clone(),
        log_handle: handle.clone(),
        objective,
        output_len: cfg.benchmark.output_len,
        num_prompts: cfg.benchmark.num_prompts,
        gpus: cfg.gpus,
        token,
    };
    let worker_state = Arc::clone(state);
    let digest = digest.to_string();
    let job_name = name.clone();
    let work = move || -> CodegenReply {
        let run = match submit(
            &env.target(),
            &job_spec(
                &env,
                tail.gpus,
                &digest,
                &job_name,
                &wrap_command(&command),
                &kwarg_env,
                &[],
            ),
            |snapshot| {
                let _ = std::fs::write(&log_path, snapshot);
            },
        ) {
            Ok(run) => run,
            // A failed submission must not leave the tracked job "in flight" forever.
            Err(e) => {
                worker_state.finish_job(&job_name, JobResult::Failed);
                return submit_failure_reply(env.spoke.as_ref(), e);
            }
        };
        worker_state.finish_job(&job_name, run.result);
        collect_measure(&worker_state, run, &kind, tail)
            .unwrap_or_else(|error| CodegenReply::Error { error })
    };
    let entry = Arc::new(InflightJob::new(name, handle));
    Ok(detach_and_wait(state, key, entry, call_wait_budget(), work))
}

/// What the detached worker needs beyond the run itself, owned: the tool call that submitted the
/// job may have returned `pending` long before this tail runs.
struct MeasureTail {
    key: String,
    log_handle: String,
    objective: Objective,
    output_len: u64,
    num_prompts: u64,
    gpus: u32,
    token: String,
}

/// The post-terminal tail of a measure job: budget accounting, final log capture, result parsing,
/// memoization. A memo write here happens before the worker's inflight entry is removed (see
/// [`detach_and_wait`]).
fn collect_measure(
    state: &CodegenState,
    run: GpuJobRun,
    kind: &MeasureKind,
    tail: MeasureTail,
) -> Result<CodegenReply, String> {
    state
        .budget
        .record(&tail.token, gpu_minutes(run.elapsed, tail.gpus))?;
    state
        .logs
        .overwrite(&tail.log_handle, run.logs.as_bytes())?;

    if run.result != JobResult::Succeeded {
        return Ok(CodegenReply::JobFailed {
            reason: job_failure_reason(run.result),
            logs: vec![tail.log_handle],
        });
    }
    let value = parse_result_json(&run.logs).map_err(|e| format!("parsing result JSON: {e}"))?;
    let metrics = match kind {
        MeasureKind::Benchmark { .. } => bench_metrics(&value, tail.output_len, tail.num_prompts)?,
        MeasureKind::LmEval { .. } => {
            let score = extract_score(&value, &tail.objective.key)?;
            BTreeMap::from([(tail.objective.key.clone(), score)])
        }
    };

    state.put(
        tail.key,
        Cached::Measure {
            metrics: metrics.clone(),
            logs: vec![tail.log_handle.clone()],
        },
    );
    Ok(CodegenReply::Measured {
        metrics,
        objective: tail.objective,
        logs: vec![tail.log_handle],
        cached: false,
    })
}

/// Render the job spec (pure). `extra_mounts` is empty for benchmark/lm_eval; a profile job adds
/// its writable artifacts mount here.
#[allow(clippy::too_many_arguments)]
fn job_spec(
    env: &JobEnv,
    gpus: u32,
    digest: &str,
    name: &str,
    wrapped_cmd: &str,
    kwarg_env: &[(String, String)],
    extra_mounts: &[PvcMount],
) -> GpuJobSpec {
    let mut container_env = vec![("HF_HUB_OFFLINE".to_string(), "1".to_string())];
    container_env.extend(kwarg_env.iter().cloned());
    let mut pvc_mounts = env.pvc_mounts.clone();
    pvc_mounts.extend(extra_mounts.iter().cloned());
    GpuJobSpec {
        name: name.to_string(),
        namespace: env.namespace.clone(),
        image: digest.to_string(),
        queue_name: env.queue_name.clone(),
        pull_secret: env.pull_secret.clone(),
        command: wrapped_cmd.to_string(),
        env: container_env,
        gpus,
        cpu: env.cpu.clone(),
        mem_request: env.mem_request.clone(),
        mem_limit: env.mem_limit.clone(),
        shm_size_gi: env.shm_size_gi,
        active_deadline_seconds: env.active_deadline_seconds,
        ttl_seconds: env.ttl_seconds,
        pvc_mounts,
        owner: env.owner.clone(),
    }
}

fn submit(
    target: &KubeTarget,
    spec: &GpuJobSpec,
    on_logs: impl FnMut(&str),
) -> Result<GpuJobRun, String> {
    // Ride the MCP tool span (entered on this blocking thread by telemetry::spawn_blocking): the
    // job name + queue identify the Kueue job on the trace, gpu_minutes is the recorded cost.
    let span = tracing::Span::current();
    span.record("job", spec.name.as_str());
    span.record("queue", spec.queue_name.as_str());
    let run = run_gpu_job_with(target, spec, QUEUE_SLACK, on_logs)
        .map_err(|e| format!("running measure job: {e:#}"))?;
    span.record("gpu_minutes", gpu_minutes(run.elapsed, spec.gpus));
    Ok(run)
}

/// The spoke smoketest's inputs: a CPU-only sentinel job (gpus 0, EMPTY pvc_mounts, busybox-class
/// image) submitted through the exact production submit/stream/parse path. The standing acceptance
/// probe for hub-spoke delegation; costs cents, finishes in seconds.
pub struct SpokeSmoke {
    pub cluster: String,
    pub target: KubeTarget,
    pub namespace: String,
    pub queue: String,
    pub image: String,
    pub deadline_secs: i64,
}

/// Extra wait beyond the smoke job's deadline for time spent suspended in the Kueue queue.
const SMOKE_QUEUE_SLACK: Duration = Duration::from_secs(600);

/// A cluster name safe to interpolate into the sentinel shell command and its JSON: the fleet-file
/// name character class, validated at entry instead of escaped ad hoc downstream.
fn valid_cluster_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Run the spoke smoketest and return the typed result (the parsed sentinel JSON plus job
/// metadata). Errors name the spoke; the caller decides how to print.
pub fn spoke_smoke(opts: &SpokeSmoke) -> Result<Value, String> {
    if !valid_cluster_name(&opts.cluster) {
        return Err(format!(
            "invalid cluster name {:?}: must match [A-Za-z0-9_-]+",
            opts.cluster
        ));
    }
    let name = format!("crucible-smoke-{}", nonce());
    let sentinel = format!(
        "printf '%s' '{{\"pass\": true, \"cluster\": \"{}\"}}' > \"$OUT\"",
        opts.cluster
    );
    let spec = GpuJobSpec {
        name: name.clone(),
        namespace: opts.namespace.clone(),
        image: opts.image.clone(),
        queue_name: opts.queue.clone(),
        pull_secret: None,
        command: wrap_command(&sentinel),
        env: Vec::new(),
        gpus: 0,
        cpu: "500m".to_string(),
        mem_request: "128Mi".to_string(),
        mem_limit: "256Mi".to_string(),
        shm_size_gi: 1,
        active_deadline_seconds: opts.deadline_secs.max(1),
        ttl_seconds: 600,
        pvc_mounts: Vec::new(),
        // A spoke job: the hub pod's UID means nothing on the target cluster.
        owner: None,
    };
    eprintln!(
        "submitting spoke smoke Job {name} to {} (namespace {}, queue {})",
        opts.cluster, opts.namespace, opts.queue
    );
    let run = run_gpu_job_with(&opts.target, &spec, SMOKE_QUEUE_SLACK, |snapshot| {
        eprintln!("--- {name} log snapshot ---\n{}", snapshot.trim_end());
    })
    .map_err(|e| format!("spoke {}: submit/poll failed: {e:#}", opts.cluster))?;
    if run.result != JobResult::Succeeded {
        return Err(format!(
            "spoke {} smoke Job {name}: {}\nlogs:\n{}",
            opts.cluster,
            job_failure_reason(run.result),
            run.logs.trim_end()
        ));
    }
    let result = parse_result_json(&run.logs)?;
    Ok(json!({
        "status": "smoke",
        "cluster": opts.cluster,
        "job": name,
        "elapsed_s": run.elapsed.as_secs(),
        "result": result,
    }))
}

/// `set -eu` fails the Job on any step; the sentinel lets the broker find the `$OUT` JSON in the logs.
fn wrap_command(frozen_cmd: &str) -> String {
    format!(
        "set -eu\n\
         OUT={JOB_OUT_PATH}\n\
         export OUT\n\
         {frozen_cmd}\n\
         echo {RESULT_SENTINEL}\n\
         cat \"$OUT\"\n"
    )
}

/// Artifacts-PVC transport: the frozen command writes `$OUT` straight onto the shared volume; the
/// broker collects the file after the job; nothing binary rides the log, any trace size works.
fn wrap_command_profile_pvc(frozen_cmd: &str, out_path: &str) -> String {
    format!(
        "set -eu\n\
         OUT={out_path}\n\
         export OUT\n\
         {frozen_cmd}\n"
    )
}

/// Fallback (no artifacts PVC): the trace leaves the pod base64'd after the sentinel. Guarded; an
/// oversize trace would scroll the sentinel off the log-collection tail and hand back a silently
/// truncated artifact, so the job refuses it with the marker instead.
fn wrap_command_profile(frozen_cmd: &str, trace_ext: &str) -> String {
    format!(
        "set -eu\n\
         OUT={JOB_TRACE_BASE}.{trace_ext}\n\
         export OUT\n\
         {frozen_cmd}\n\
         SIZE=$(wc -c < \"$OUT\")\n\
         if [ \"$SIZE\" -gt {FALLBACK_TRACE_MAX_BYTES} ]; then\n\
         \techo {TRACE_TOO_LARGE_MARKER} \"$SIZE\"\n\
         \texit 3\n\
         fi\n\
         echo {RESULT_SENTINEL}\n\
         base64 \"$OUT\"\n"
    )
}

/// The readable operator error when a fallback profile job refused an oversize trace.
fn trace_too_large_reason(logs: &str) -> Option<String> {
    let line = logs
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with(TRACE_TOO_LARGE_MARKER))?;
    let size = line
        .split_whitespace()
        .nth(1)
        .unwrap_or("unknown")
        .to_string();
    Some(format!(
        "the trace artifact ({size} bytes) exceeds the base64-through-logs fallback limit \
         ({FALLBACK_TRACE_MAX_BYTES} bytes) — configure BROKER_CODEGEN_ARTIFACTS_PVC so profile \
         jobs write the trace to a shared artifacts volume instead"
    ))
}

/// Move a PVC-written trace into the LogStore: read, store under a handle, delete the source (the
/// PVC is transport rather than storage). An unreadable or empty file is a hard error, never a fake handle.
fn collect_trace_artifact(
    logs: &LogStore,
    src: &Path,
    digest: &str,
    trace_ext: &str,
) -> Result<String, String> {
    let bytes = std::fs::read(src).map_err(|e| {
        format!(
            "reading the trace artifact {} from the artifacts volume (did the profile command \
             write $OUT?): {e}",
            src.display()
        )
    })?;
    if bytes.is_empty() {
        return Err(format!(
            "the profile job wrote an empty trace artifact at {}",
            src.display()
        ));
    }
    let (handle, _) = logs.store_artifact("trace", digest, trace_ext, &bytes)?;
    if let Err(e) = std::fs::remove_file(src) {
        eprintln!(
            "warning: failed to remove collected trace {}: {e}",
            src.display()
        );
    }
    Ok(handle)
}

/// Split a successful fallback profile job log into (human-readable capture log before the
/// sentinel, decoded trace bytes from the base64 after it).
fn split_profile_output(logs: &str) -> Result<(&str, Vec<u8>), String> {
    let (before, after) = logs.rsplit_once(RESULT_SENTINEL).ok_or_else(|| {
        format!(
            "result sentinel {RESULT_SENTINEL:?} not found in profile job log (the log may have \
             been truncated; for large traces configure BROKER_CODEGEN_ARTIFACTS_PVC)"
        )
    })?;
    let b64: String = after.split_whitespace().collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| format!("decoding the base64 trace artifact from the job log: {e}"))?;
    if bytes.is_empty() {
        return Err("the profile job produced an empty trace artifact".into());
    }
    Ok((before, bytes))
}

/// The JSON payload after the LAST sentinel line.
fn parse_result_json(logs: &str) -> Result<Value, String> {
    let tail = match logs.rsplit_once(RESULT_SENTINEL) {
        Some((_, after)) => after.trim(),
        None => {
            return Err(format!(
                "result sentinel {RESULT_SENTINEL:?} not found in job log"
            ));
        }
    };
    if let Ok(v) = serde_json::from_str::<Value>(tail) {
        return Ok(v);
    }
    let start = tail.find('{').ok_or("no JSON object after the sentinel")?;
    let end = tail.rfind('}').ok_or("no JSON object after the sentinel")?;
    if end < start {
        return Err("malformed JSON span after the sentinel".into());
    }
    serde_json::from_str::<Value>(&tail[start..=end]).map_err(|e| format!("{e}"))
}

/// Benchmark metrics are whatever scalars the frozen command reported: every top-level numeric or
/// boolean field of the result JSON (bools as 1/0, so a correctness rung's `pass` survives the
/// f64 metrics map). When the JSON carries `elapsed_time` the TPOT pair is derived on top;
/// a token-generation bench keeps its `tpot_ms`/`tokens_per_s` contract, a rung that reports
/// `{"pass": true, "refcheck": 3e-8}` is not rejected for not being one.
fn bench_metrics(
    v: &Value,
    output_len: u64,
    fallback_num_requests: u64,
) -> Result<BTreeMap<String, f64>, String> {
    let mut metrics = BTreeMap::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            let num = match val {
                Value::Bool(b) => Some(f64::from(u8::from(*b))),
                _ => val.as_f64(),
            };
            if let Some(n) = num {
                metrics.insert(k.clone(), n);
            }
        }
    }
    if v.get("elapsed_time").is_some() {
        let (tpot_ms, tokens_per_s) = derive_bench(v, output_len, fallback_num_requests)?;
        metrics.insert("tpot_ms".to_string(), tpot_ms);
        metrics.insert("tokens_per_s".to_string(), tokens_per_s);
    }
    if metrics.is_empty() {
        return Err("bench JSON has no numeric or boolean fields to report".into());
    }
    Ok(metrics)
}

/// TPOT ms = elapsed / (num_requests * output_len) * 1000; `num_requests` prefers the JSON, else the
/// configured prompt count.
fn derive_bench(
    v: &Value,
    output_len: u64,
    fallback_num_requests: u64,
) -> Result<(f64, f64), String> {
    let elapsed = v
        .get("elapsed_time")
        .and_then(Value::as_f64)
        .ok_or("bench JSON has no numeric `elapsed_time`")?;
    if elapsed <= 0.0 {
        return Err(format!(
            "bench `elapsed_time` must be positive, got {elapsed}"
        ));
    }
    let num_requests = v
        .get("num_requests")
        .and_then(Value::as_u64)
        .unwrap_or(fallback_num_requests);
    let out_tokens = num_requests.saturating_mul(output_len);
    if out_tokens == 0 {
        return Err("num_requests * output_len is zero — can't derive TPOT".into());
    }
    let tpot_ms = elapsed / out_tokens as f64 * 1000.0;
    let tokens_per_s = v
        .get("tokens_per_second")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| out_tokens as f64 / elapsed);
    Ok((tpot_ms, tokens_per_s))
}

/// The score under `key`: a top-level number, or one nested a single level down.
fn extract_score(v: &Value, key: &str) -> Result<f64, String> {
    if let Some(n) = v.get(key).and_then(Value::as_f64) {
        return Ok(n);
    }
    if let Some(obj) = v.as_object() {
        for (_, sub) in obj {
            if let Some(n) = sub.get(key).and_then(Value::as_f64) {
                return Ok(n);
            }
        }
    }
    Err(format!("lm_eval JSON has no numeric score under {key:?}"))
}

/// The whole finalized build config is fingerprinted (not just the base image): the artifact also
/// depends on install_cmd/src_dir/etc., and a config change must never replay a stale digest.
fn build_key(tree_hash: &str, mode: BuildMode, build: &config::BuildCfg) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    build.hash(&mut h);
    format!("build|{tree_hash}|{}|{:016x}", mode.as_str(), h.finish())
}

/// Sorted kwarg pairs so argument order doesn't split the memo.
fn measure_key(digest: &str, kind: &str, kwarg_env: &[(String, String)]) -> String {
    let mut pairs: Vec<String> = kwarg_env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    format!("{kind}|{digest}|{}", pairs.join(","))
}

fn job_failure_reason(result: JobResult) -> String {
    match result {
        JobResult::Failed => "the measure job crashed / exited non-zero (see log)".into(),
        JobResult::TimedOut => {
            "the measure job exceeded its deadline and was reaped (see log)".into()
        }
        JobResult::Succeeded => "the measure job succeeded".into(),
    }
}

/// Per-turn GPU budget (call count + GPU-minutes), persisted on the shared storage volume so it
/// survives per-request server instances. An empty turn token degrades to uncapped.
struct Budget {
    path: PathBuf,
    max_calls: u32,
    max_gpu_minutes: f64,
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct BudgetState {
    token: String,
    calls: u32,
    gpu_minutes: f64,
}

impl BudgetState {
    /// Reset the counters when the turn token changes.
    fn for_turn(prev: BudgetState, token: &str) -> BudgetState {
        if prev.token == token {
            prev
        } else {
            BudgetState {
                token: token.to_string(),
                ..Default::default()
            }
        }
    }
}

impl Budget {
    fn from_env() -> Self {
        Self {
            path: forge::storage_root().join("codegen-budget.json"),
            max_calls: env_u32("BROKER_CODEGEN_MAX_CALLS_PER_TURN", 0),
            max_gpu_minutes: env_nonempty("BROKER_CODEGEN_MAX_GPU_MINUTES_PER_TURN")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
        }
    }

    fn precheck(&self, token: &str) -> Result<Option<String>, String> {
        if token.is_empty() {
            return Ok(None);
        }
        let (st, reason) =
            decide_precheck(self.load(), token, self.max_calls, self.max_gpu_minutes);
        self.save(&st)?;
        Ok(reason)
    }

    fn record(&self, token: &str, gpu_min: f64) -> Result<(), String> {
        if token.is_empty() {
            return Ok(());
        }
        self.save(&decide_record(self.load(), token, gpu_min))
    }

    fn load(&self) -> BudgetState {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// A failed write MUST surface: silently dropping it would make the budget infinite.
    fn save(&self, st: &BudgetState) -> Result<(), String> {
        let s = serde_json::to_string(st).map_err(|e| format!("serializing budget state: {e}"))?;
        if let Some(p) = self.path.parent() {
            std::fs::create_dir_all(p)
                .map_err(|e| format!("creating budget dir {}: {e}", p.display()))?;
        }
        std::fs::write(&self.path, s)
            .map_err(|e| format!("persisting budget state {}: {e}", self.path.display()))
    }
}

fn decide_precheck(
    st: BudgetState,
    token: &str,
    max_calls: u32,
    max_gpu_min: f64,
) -> (BudgetState, Option<String>) {
    let st = BudgetState::for_turn(st, token);
    if max_calls > 0 && st.calls >= max_calls {
        return (
            st,
            Some(format!(
                "this turn's GPU call budget is spent ({max_calls} calls). Commit your best candidate \
                 and END your turn — the loop measures it and gives you a fresh turn."
            )),
        );
    }
    if max_gpu_min > 0.0 && st.gpu_minutes >= max_gpu_min {
        let used = st.gpu_minutes;
        return (
            st,
            Some(format!(
                "this turn's GPU-minutes budget is spent ({used:.1}/{max_gpu_min:.1} min). Commit your \
                 best candidate and END your turn."
            )),
        );
    }
    (st, None)
}

fn decide_record(st: BudgetState, token: &str, gpu_min: f64) -> BudgetState {
    let mut st = BudgetState::for_turn(st, token);
    st.calls += 1;
    st.gpu_minutes += gpu_min;
    st
}

fn gpu_minutes(elapsed: Duration, gpus: u32) -> f64 {
    elapsed.as_secs_f64() / 60.0 * gpus as f64
}

/// Captured job/build logs, stored on the shared volume, read back by bare-name handle.
struct LogStore {
    dir: PathBuf,
}

impl LogStore {
    fn from_env() -> Self {
        Self {
            dir: forge::storage_root().join("codegen-logs"),
        }
    }

    /// Mint a log handle BEFORE the work starts: the file exists (empty) from the first moment, so
    /// a concurrent fetch_log can tail it while the producer streams into `path`.
    fn allocate(&self, kind: &str, tag: &str) -> Result<(String, PathBuf), String> {
        let (handle, _) = self.store_artifact(kind, tag, "log", b"")?;
        let path = self.dir.join(&handle);
        Ok((handle, path))
    }

    /// Replace a handle's content with the final authoritative bytes (closes any gap the live
    /// streaming left behind).
    fn overwrite(&self, handle: &str, bytes: &[u8]) -> Result<(), String> {
        std::fs::write(self.path_for(handle)?, bytes)
            .map_err(|e| format!("writing job artifact: {e}"))
    }

    /// Store bytes under a handle whose extension is preserved (a profile trace keeps its `json.gz`
    /// so a downstream analyzer keys on it; a plain log is `.log`).
    fn store_artifact(
        &self,
        kind: &str,
        tag: &str,
        ext: &str,
        bytes: &[u8],
    ) -> Result<(String, u64), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("creating log dir: {e}"))?;
        let handle = format!("{kind}-{}-{}.{ext}", short_hint(tag), nonce());
        std::fs::write(self.dir.join(&handle), bytes)
            .map_err(|e| format!("writing job artifact: {e}"))?;
        Ok((handle, bytes.len() as u64))
    }

    /// Resolve a bare handle to its on-disk path, refusing anything path-like.
    fn path_for(&self, handle: &str) -> Result<PathBuf, String> {
        if handle.is_empty() || handle.contains('/') || handle.contains("..") {
            return Err(format!(
                "handle must be a bare artifact name, not a path: {handle:?}"
            ));
        }
        Ok(self.dir.join(handle))
    }

    fn read(&self, handle: &str, offset: usize) -> Result<(String, usize, usize), String> {
        const WINDOW: usize = 64 * 1024;
        let bytes = std::fs::read(self.path_for(handle)?)
            .map_err(|_| format!("no log named {handle:?}"))?;
        let total = bytes.len();
        let start = offset.min(total);
        let end = (start + WINDOW).min(total);
        Ok((
            String::from_utf8_lossy(&bytes[start..end]).into_owned(),
            end,
            total,
        ))
    }

    /// Raw byte window for a binary artifact (a profile trace). The window is a multiple of 3 so
    /// consecutive base64-encoded windows concatenate to the exact artifact with no padding at
    /// window boundaries.
    fn read_bytes(&self, handle: &str, offset: usize) -> Result<(Vec<u8>, usize, usize), String> {
        const WINDOW: usize = 48 * 1024;
        let bytes = std::fs::read(self.path_for(handle)?)
            .map_err(|_| format!("no artifact named {handle:?}"))?;
        let total = bytes.len();
        let start = offset.min(total);
        let end = (start + WINDOW).min(total);
        Ok((bytes[start..end].to_vec(), end, total))
    }
}

fn short_hint(s: &str) -> String {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let start = cleaned.len().saturating_sub(12);
    let hint = cleaned[start..].to_string();
    if hint.is_empty() {
        "x".to_string()
    } else {
        hint
    }
}

/// The leading hex token of a `sha256sum` line.
fn hex_token(s: &str) -> String {
    s.split_whitespace().next().unwrap_or("").to_string()
}

/// Manifest defaults ∘ scenario overlay, finalized against the substrate GPU ceiling.
fn load_config() -> Result<ToolsConfig, String> {
    let defaults = parse_overlay_env("BROKER_CODEGEN_TOOLS_DEFAULTS")?;
    let overlay = parse_overlay_env("BROKER_CODEGEN_TOOLS_OVERLAY")?;
    overlay
        .merge(defaults)
        .finalize(env_u32("BROKER_CODEGEN_MAX_GPUS", 2))
}
fn parse_overlay_env(k: &str) -> Result<ToolsOverlay, String> {
    match env_nonempty(k) {
        Some(s) => {
            serde_json::from_str(&s).map_err(|e| format!("parsing {k} as tools config JSON: {e}"))
        }
        None => Ok(ToolsOverlay::default()),
    }
}

fn env_req(k: &str) -> Result<String, String> {
    env_nonempty(k).ok_or_else(|| format!("{k} is not set on the loop pod"))
}
fn env_or(k: &str, default: &str) -> String {
    env_nonempty(k).unwrap_or_else(|| default.to_string())
}
fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
fn env_u32(k: &str, default: u32) -> u32 {
    env_nonempty(k)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
fn nonce() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
fn json_reply(reply: &CodegenReply) -> String {
    serde_json::to_string(reply)
        .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"serializing reply: {e}"}}"#))
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::Direction;

    fn bench_cfg() -> ToolsConfig {
        let json = serde_json::json!({
            "gpus": 2,
            "build": {"base_image": "ghcr.io/x/base@sha256:abc", "src_dir": "/workspace/vllm", "install_cmd": "VLLM_USE_PRECOMPILED=1 pip install -e ."},
            "benchmark": {
                "command": "bench --output-json \"$OUT\"",
                "mutable_kwargs": {
                    "toggles": {"DISABLE_FUSION": ["0", "1"]},
                    "reps": {}
                }
            },
            "lm_eval": {"command": "lm_eval"}
        });
        let overlay: ToolsOverlay = serde_json::from_value(json).unwrap();
        overlay.finalize(2).unwrap()
    }

    /// A state whose step ledger is a fresh temp dir, so a unit test never reads or writes the
    /// shared build volume (and two tests can never replay each other's steps).
    fn test_state() -> Arc<CodegenState> {
        Arc::new(CodegenState::with_steps(forge::steps::StepLedger::new(
            &steps_dir(),
        )))
    }

    fn steps_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("codegen-steps-{}-{}", std::process::id(), nonce()))
    }

    fn temp_budget(max_calls: u32, max_gpu_minutes: f64) -> (Budget, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "codegen-budget-test-{}-{}",
            std::process::id(),
            nonce()
        ));
        (
            Budget {
                path: dir.join("budget.json"),
                max_calls,
                max_gpu_minutes,
            },
            dir,
        )
    }

    #[test]
    fn replies_serialize_with_a_status_tag() {
        let built = json_reply(&CodegenReply::Built {
            tree_hash: "abc".into(),
            source: "sandbox",
            digest: "repo@sha256:abc".into(),
            mode: BuildMode::Derive,
            cached: false,
            log: "build-abc-1.log".into(),
        });
        assert!(built.contains(r#""status":"built""#));
        assert!(built.contains(r#""log":"build-abc-1.log""#));
        assert!(built.contains(r#""source":"sandbox""#));
        assert!(
            json_reply(&CodegenReply::Measured {
                metrics: BTreeMap::from([("tpot_ms".into(), 9.15)]),
                objective: Objective {
                    key: "tpot_ms".into(),
                    direction: Direction::Lower
                },
                logs: vec!["bench-abc-1.log".into()],
                cached: false,
            })
            .contains(r#""status":"measured""#)
        );
        assert!(
            json_reply(&CodegenReply::RejectedKwarg {
                reason: "no".into()
            })
            .contains(r#""status":"rejected_kwarg""#)
        );
    }

    #[test]
    fn spoke_failures_split_unreachable_from_other_errors() {
        let spoke = SpokeEnv {
            name: "gpu-east".into(),
            tier: "public".into(),
            target: KubeTarget::Ambient,
        };
        // Reachability-class failures get the typed unreachable reply.
        for msg in [
            "running measure job: error trying to connect: tcp connect error",
            "running measure job: operation timed out",
            "running measure job: dns error: failed to lookup address",
            "running measure job: invalid peer certificate: UnknownIssuer",
            "running measure job: proxy CONNECT failed",
        ] {
            match submit_failure_reply(Some(&spoke), msg.into()) {
                CodegenReply::SpokeUnreachable { spoke: s, tier, .. } => {
                    assert_eq!(s, "gpu-east");
                    assert_eq!(tier, "public");
                }
                other => panic!("{msg:?} must map to SpokeUnreachable, got {other:?}"),
            }
        }
        // A reachable spoke rejecting the work names the spoke but claims nothing about reachability.
        match submit_failure_reply(
            Some(&spoke),
            "running measure job: jobs.batch is forbidden: cannot create resource".into(),
        ) {
            CodegenReply::Error { error } => assert!(error.starts_with("spoke gpu-east: ")),
            other => panic!("expected a plain spoke-named error, got {other:?}"),
        }
        // Hub-local failures stay plain.
        match submit_failure_reply(None, "boom".into()) {
            CodegenReply::Error { error } => assert_eq!(error, "boom"),
            other => panic!("expected a plain error, got {other:?}"),
        }
    }

    #[test]
    fn cluster_names_validate_at_entry() {
        assert!(valid_cluster_name("gpu-east"));
        assert!(valid_cluster_name("b200_lab-2"));
        for bad in ["", "wal dorf", "x\"y", "a;rm -rf", "cl$us", "wald'orf"] {
            assert!(!valid_cluster_name(bad), "{bad:?} must be rejected");
        }
        let opts = SpokeSmoke {
            cluster: "bad\"name".into(),
            target: KubeTarget::Ambient,
            namespace: "ns".into(),
            queue: "q".into(),
            image: "busybox".into(),
            deadline_secs: 1,
        };
        let err = spoke_smoke(&opts).expect_err("invalid name must fail before any submit");
        assert!(err.contains("invalid cluster name"), "{err}");
    }

    #[test]
    fn disabled_without_the_gate() {
        if std::env::var("BROKER_CODEGEN").is_ok() {
            return;
        }
        let st = test_state();
        assert!(build(&st, "derive").contains(r#""status":"disabled""#));
        assert!(benchmark(&st, "repo@sha256:x", &[], None).contains(r#""status":"disabled""#));
        assert!(lm_eval(&st, "repo@sha256:x", None).contains(r#""status":"disabled""#));
        assert!(profile(&st, "repo@sha256:x").contains(r#""status":"disabled""#));
    }

    #[test]
    fn parse_result_json_takes_the_payload_after_the_sentinel() {
        let logs = format!(
            "loading weights...\nrunning bench\n{RESULT_SENTINEL}\n{{\"elapsed_time\": 37.4, \"num_requests\": 4}}\n"
        );
        let v = parse_result_json(&logs).unwrap();
        assert_eq!(v.get("num_requests").unwrap().as_u64(), Some(4));
    }

    #[test]
    fn parse_result_json_splits_on_the_last_sentinel_and_errors_without_one() {
        let logs = format!(
            "prose mentioning {RESULT_SENTINEL} early\n{RESULT_SENTINEL}\n{{\"elapsed_time\": 1.0}}"
        );
        assert!(parse_result_json(&logs).is_ok());
        assert!(parse_result_json("no marker {\"x\":1}").is_err());
    }

    #[test]
    fn tpot_derivation_matches_the_reference_workload() {
        // 37.5s / (4 * 1024) * 1000 = ~9.16 ms.
        let v = json!({"elapsed_time": 37.5, "num_requests": 4, "tokens_per_second": 109.7});
        let (tpot, tps) = derive_bench(&v, 1024, 4).unwrap();
        assert!((tpot - 9.155).abs() < 0.01, "tpot={tpot}");
        assert!((tps - 109.7).abs() < 0.001);
    }

    #[test]
    fn tpot_falls_back_to_config_prompt_count_and_computes_throughput() {
        let v = json!({"elapsed_time": 10.0});
        let (tpot, tps) = derive_bench(&v, 100, 5).unwrap();
        assert!((tpot - 20.0).abs() < 1e-9, "tpot={tpot}");
        assert!((tps - 50.0).abs() < 1e-9, "tps={tps}");
    }

    #[test]
    fn tpot_rejects_nonpositive_elapsed() {
        assert!(derive_bench(&json!({"elapsed_time": 0.0}), 1024, 4).is_err());
        assert!(derive_bench(&json!({"foo": 1}), 1024, 4).is_err());
    }

    #[test]
    fn bench_metrics_pass_through_a_rung_report() {
        // A correctness rung's report: no elapsed_time, scalars + a pass bool + non-scalar extras.
        let v =
            json!({"metric": 3e-8, "refcheck": 3e-8, "pass": true, "checks": {"a": 1}, "log": "x"});
        let m = bench_metrics(&v, 1024, 4).unwrap();
        assert_eq!(m["pass"], 1.0);
        assert!((m["refcheck"] - 3e-8).abs() < 1e-12);
        assert!(!m.contains_key("checks"), "non-scalars must be dropped");
        assert!(!m.contains_key("tpot_ms"), "no elapsed_time => no TPOT");
    }

    #[test]
    fn bench_metrics_still_derive_tpot_when_elapsed_time_is_present() {
        let v = json!({"elapsed_time": 10.0, "num_requests": 5});
        let m = bench_metrics(&v, 100, 5).unwrap();
        assert!((m["tpot_ms"] - 20.0).abs() < 1e-9);
        assert!((m["tokens_per_s"] - 50.0).abs() < 1e-9);
        assert_eq!(m["num_requests"], 5.0);
    }

    #[test]
    fn bench_metrics_reject_a_report_with_nothing_numeric() {
        assert!(bench_metrics(&json!({"log": "words"}), 1024, 4).is_err());
        // A malformed elapsed_time still fails loudly rather than falling through.
        assert!(bench_metrics(&json!({"elapsed_time": 0.0}), 1024, 4).is_err());
    }

    #[test]
    fn score_reads_top_level_or_one_level_nested() {
        assert_eq!(
            extract_score(&json!({"score": 0.87}), "score").unwrap(),
            0.87
        );
        assert_eq!(
            extract_score(&json!({"results": {"acc": 0.91}}), "acc").unwrap(),
            0.91
        );
        assert!(extract_score(&json!({"nope": 1}), "score").is_err());
    }

    #[test]
    fn measure_key_is_kwarg_order_independent() {
        let a = measure_key(
            "d",
            "benchmark",
            &[("A".into(), "1".into()), ("B".into(), "2".into())],
        );
        let b = measure_key(
            "d",
            "benchmark",
            &[("B".into(), "2".into()), ("A".into(), "1".into())],
        );
        assert_eq!(a, b);
        assert_ne!(a, measure_key("d2", "benchmark", &[]));
    }

    #[test]
    fn build_errors_when_the_tree_cannot_be_hashed() {
        // No live sandbox and no workdir env in the test process, so the build must error out
        // naming the missing key, never proceed with fabricated provenance.
        let st = test_state();
        let err = build_inner(&st, &bench_cfg(), BuildMode::Derive).unwrap_err();
        assert!(err.contains("BROKER_SANDBOX_WORKDIR"), "{err}");
        assert!(err.contains("BROKER_CODEGEN_SANDBOX_WORKDIR"), "{err}");
    }

    #[test]
    fn measure_rejects_a_digest_this_broker_did_not_build() {
        let st = test_state();
        let cfg = bench_cfg();
        let reply = run_benchmark(&st, &cfg, "ghcr.io/evil/img@sha256:beef", &[], None);
        match reply {
            CodegenReply::Error { error } => {
                assert!(error.contains("not produced by codegen_build"), "{error}");
            }
            other => panic!("expected an error reply, got {other:?}"),
        }
        let reply = run_lm_eval(&st, &cfg, "ghcr.io/evil/img@sha256:beef", None);
        assert!(matches!(reply, CodegenReply::Error { .. }), "{reply:?}");
        st.mark_built("ghcr.io/ok/img@sha256:cafe");
        assert!(st.is_built("ghcr.io/ok/img@sha256:cafe"));
        assert!(!st.is_built("ghcr.io/evil/img@sha256:beef"));
    }

    #[test]
    fn undeclared_kwarg_is_rejected_before_any_side_effect() {
        let st = test_state();
        let cfg = bench_cfg();
        // lm_eval's `limit` is not declared in the fixture.
        let reply = run_lm_eval(&st, &cfg, "repo@sha256:x", Some(100));
        match reply {
            CodegenReply::RejectedKwarg { reason } => {
                assert!(reason.contains("limit"), "{reason}");
            }
            other => panic!("expected rejected_kwarg, got {other:?}"),
        }
        // `reps` IS declared ({} = default bounds 1..=3): out-of-bounds rejects, in-bounds proceeds
        // to the provenance gate (an unknown digest, so an Error rather than a kwarg rejection).
        let reply = run_benchmark(&st, &cfg, "repo@sha256:x", &[], Some(4));
        assert!(
            matches!(reply, CodegenReply::RejectedKwarg { .. }),
            "{reply:?}"
        );
        let reply = run_benchmark(&st, &cfg, "repo@sha256:x", &[], Some(2));
        assert!(matches!(reply, CodegenReply::Error { .. }), "{reply:?}");
    }

    #[test]
    fn memo_replays_a_measure_and_a_build() {
        let st = test_state();
        let mkey = measure_key("d", "benchmark", &[]);
        st.put(
            mkey.clone(),
            Cached::Measure {
                metrics: BTreeMap::from([("tpot_ms".into(), 9.1)]),
                logs: vec!["bench-d-1.log".into()],
            },
        );
        match st.get(&mkey) {
            Some(Cached::Measure { metrics, .. }) => assert_eq!(metrics["tpot_ms"], 9.1),
            _ => panic!("expected a cached measure"),
        }
        let bkey = build_key("treehash", BuildMode::Derive, &bench_cfg().build);
        st.put(
            bkey.clone(),
            Cached::Build {
                digest: "repo@sha256:x".into(),
                log: "build-x-1.log".into(),
            },
        );
        // A cached-build replay carries the original build-log handle with it.
        match st.get(&bkey) {
            Some(Cached::Build { digest, log }) => {
                assert_eq!(digest, "repo@sha256:x");
                assert_eq!(log, "build-x-1.log");
            }
            _ => panic!("expected a cached build"),
        }
    }

    /// The pod died between the build and the measure. A new process shares nothing in memory
    /// with the old one, so the ledger is the only thing that can replay the digest and the
    /// completed measure instead of repaying both.
    #[test]
    fn a_restarted_broker_replays_recorded_steps() {
        let dir = steps_dir();
        let bkey = build_key("treehash", BuildMode::Derive, &bench_cfg().build);
        let mkey = measure_key("repo@sha256:x", "benchmark", &[]);
        {
            let dead = CodegenState::with_steps(forge::steps::StepLedger::new(&dir));
            dead.put(
                bkey.clone(),
                Cached::Build {
                    digest: "repo@sha256:x".into(),
                    log: "build-x-1.log".into(),
                },
            );
            dead.mark_built("repo@sha256:x");
            dead.put(
                mkey.clone(),
                Cached::Measure {
                    metrics: BTreeMap::from([("tpot_ms".into(), 9.1)]),
                    logs: vec!["bench-x-1.log".into()],
                },
            );
        }
        let fresh = CodegenState::with_steps(forge::steps::StepLedger::new(&dir));
        match fresh.get(&bkey) {
            Some(Cached::Build { digest, log }) => {
                assert_eq!(digest, "repo@sha256:x");
                assert_eq!(log, "build-x-1.log");
            }
            _ => panic!("expected the recorded build"),
        }
        match fresh.get(&mkey) {
            Some(Cached::Measure { metrics, logs }) => {
                assert_eq!(metrics["tpot_ms"], 9.1);
                assert_eq!(logs, vec!["bench-x-1.log".to_string()]);
            }
            _ => panic!("expected the recorded measure"),
        }
        // Serving a recorded build also restores its provenance, so the memo hit and the ledger
        // hit leave the state in the same place.
        assert!(fresh.is_built("repo@sha256:x"));
    }

    /// The provenance gate reads the ledger directly: an agent that kept a digest across a broker
    /// restart can measure it without a pointless rebuild.
    #[test]
    fn is_built_accepts_a_digest_only_the_ledger_knows() {
        let dir = steps_dir();
        CodegenState::with_steps(forge::steps::StepLedger::new(&dir)).put(
            build_key("treehash", BuildMode::Derive, &bench_cfg().build),
            Cached::Build {
                digest: "repo@sha256:x".into(),
                log: "build-x-1.log".into(),
            },
        );
        let fresh = CodegenState::with_steps(forge::steps::StepLedger::new(&dir));
        assert!(fresh.is_built("repo@sha256:x"));
        assert!(!fresh.is_built("ghcr.io/evil/img@sha256:beef"));
    }

    /// Two different runs' records coexist under content keys; neither can serve the other.
    #[test]
    fn a_changed_content_key_is_a_ledger_miss() {
        let st = test_state();
        st.put(
            build_key("tree-a", BuildMode::Derive, &bench_cfg().build),
            Cached::Build {
                digest: "repo@sha256:a".into(),
                log: "build-a-1.log".into(),
            },
        );
        assert!(
            st.get(&build_key("tree-b", BuildMode::Derive, &bench_cfg().build))
                .is_none()
        );
    }

    #[test]
    fn build_key_fingerprints_the_whole_build_config() {
        let build = bench_cfg().build;
        // Deterministic for the same tree + config.
        assert_eq!(
            build_key("tree", BuildMode::Derive, &build),
            build_key("tree", BuildMode::Derive, &build)
        );
        // A config change (same tree) must produce a NEW key; replaying the old digest would hand
        // back an artifact the current config never built.
        let mut changed = build.clone();
        changed.install_cmd = "pip install -e .[dev]".into();
        assert_ne!(
            build_key("tree", BuildMode::Derive, &build),
            build_key("tree", BuildMode::Derive, &changed)
        );
        let mut changed = build.clone();
        changed.src_dir = "/workspace/other".into();
        assert_ne!(
            build_key("tree", BuildMode::Derive, &build),
            build_key("tree", BuildMode::Derive, &changed)
        );
        let mut changed = build.clone();
        changed.full_install_cmd = Some("pip install -e . --no-build-isolation".into());
        assert_ne!(
            build_key("tree", BuildMode::Derive, &build),
            build_key("tree", BuildMode::Derive, &changed)
        );
        // Tree and mode still split the key as before.
        assert_ne!(
            build_key("tree-a", BuildMode::Derive, &build),
            build_key("tree-b", BuildMode::Derive, &build)
        );
        assert_ne!(
            build_key("tree", BuildMode::Derive, &build),
            build_key("tree", BuildMode::Full, &build)
        );
    }

    #[test]
    fn toggles_validate_by_name_and_value() {
        let cfg = bench_cfg();
        let kw = &cfg.benchmark.mutable_kwargs;
        assert!(
            kw.validate_toggles(&[("DISABLE_FUSION".into(), "1".into())])
                .is_ok()
        );
        assert!(
            kw.validate_toggles(&[("PATH".into(), "/x".into())])
                .is_err()
        );
        assert!(
            kw.validate_toggles(&[("DISABLE_FUSION".into(), "9".into())])
                .is_err()
        );
    }

    #[test]
    fn mode_domain_enforced() {
        let cfg = bench_cfg();
        assert_eq!(cfg.build.parse_mode("derive").unwrap(), BuildMode::Derive);
        assert!(cfg.build.parse_mode("full").is_err());
        assert!(cfg.build.parse_mode("banana").is_err());
    }

    #[test]
    fn budget_precheck_resets_on_new_turn_and_blocks_at_the_call_cap() {
        let (st, over) = decide_precheck(BudgetState::default(), "turn-A", 2, 0.0);
        assert!(over.is_none());
        let st = decide_record(st, "turn-A", 1.0);
        let (st, over) = decide_precheck(st, "turn-A", 2, 0.0);
        assert!(over.is_none(), "1 of 2 used");
        let st = decide_record(st, "turn-A", 1.0);
        let (st, over) = decide_precheck(st, "turn-A", 2, 0.0);
        assert!(over.is_some(), "2 of 2 used ⇒ blocked");
        let (_st, over) = decide_precheck(st, "turn-B", 2, 0.0);
        assert!(over.is_none(), "new turn resets the budget");
    }

    #[test]
    fn budget_precheck_blocks_on_gpu_minutes_and_uncaps_at_zero() {
        let st = BudgetState {
            token: "t".into(),
            calls: 1,
            gpu_minutes: 61.0,
        };
        let (_st, over) = decide_precheck(st, "t", 0, 60.0);
        assert!(over.unwrap().contains("GPU-minutes"));
        let st = BudgetState {
            token: "t".into(),
            calls: 999,
            gpu_minutes: 9999.0,
        };
        assert!(
            decide_precheck(st, "t", 0, 0.0).1.is_none(),
            "zero caps ⇒ never block"
        );
    }

    #[test]
    fn budget_persists_across_instances_and_surfaces_write_failures() {
        let (budget, dir) = temp_budget(2, 0.0);
        assert!(budget.precheck("turn-A").unwrap().is_none());
        budget.record("turn-A", 1.5).unwrap();
        budget.record("turn-A", 1.5).unwrap();
        // A second Budget over the same path (a fresh per-request instance) sees the spent budget.
        let again = Budget {
            path: budget.path.clone(),
            max_calls: 2,
            max_gpu_minutes: 0.0,
        };
        assert!(again.precheck("turn-A").unwrap().is_some(), "2 of 2 spent");
        assert!(
            again.precheck("turn-B").unwrap().is_none(),
            "new turn resets"
        );
        std::fs::remove_dir_all(&dir).ok();

        // An unwritable state path is an error rather than a silent infinite budget.
        let broken = Budget {
            path: PathBuf::from("/dev/null/budget.json"),
            max_calls: 2,
            max_gpu_minutes: 0.0,
        };
        assert!(broken.precheck("turn-A").is_err());
        assert!(broken.record("turn-A", 1.0).is_err());
        // Empty token (no turn signal) degrades to uncapped without touching the path.
        assert!(broken.precheck("").unwrap().is_none());
        assert!(broken.record("", 1.0).is_ok());
    }

    #[test]
    fn gpu_minutes_scale_with_gpu_count() {
        assert!((gpu_minutes(Duration::from_secs(30), 2) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn wrap_command_echoes_the_sentinel_and_out() {
        let w = wrap_command("bench --output-json \"$OUT\"");
        assert!(w.contains("set -eu"));
        assert!(w.contains(RESULT_SENTINEL));
        assert!(w.contains(&format!("OUT={JOB_OUT_PATH}")));
        assert!(w.contains("cat \"$OUT\""));
    }

    fn profile_cfg() -> ToolsConfig {
        let overlay: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "gpus": 2,
            "build": {"base_image": "ghcr.io/x/base@sha256:abc", "src_dir": "/workspace/vllm", "install_cmd": "VLLM_USE_PRECOMPILED=1 pip install -e ."},
            "benchmark": {"command": "bench --output-json \"$OUT\""},
            "lm_eval": {"command": "lm_eval"},
            "profile": {"command": "capture --out \"$OUT\"", "trace_ext": "json.gz"}
        }))
        .unwrap();
        overlay.finalize(2).unwrap()
    }

    #[test]
    fn profile_without_a_configured_section_is_a_readable_rejection() {
        // bench_cfg has no [tools.profile]: the call is UNCONFIGURED rather than an error/config failure.
        let st = test_state();
        let reply = run_profile(&st, &bench_cfg(), "repo@sha256:x");
        match reply {
            CodegenReply::Unconfigured { reason } => {
                assert!(reason.contains("not configured"), "{reason}");
            }
            other => panic!("expected unconfigured, got {other:?}"),
        }
    }

    #[test]
    fn profile_rejects_a_digest_this_broker_did_not_build() {
        let st = test_state();
        let reply = run_profile(&st, &profile_cfg(), "ghcr.io/evil/img@sha256:beef");
        match reply {
            CodegenReply::Error { error } => {
                assert!(error.contains("not produced by codegen_build"), "{error}");
            }
            other => panic!("expected an error reply, got {other:?}"),
        }
    }

    #[test]
    fn profiled_reply_serializes_with_a_status_tag() {
        assert!(
            json_reply(&CodegenReply::Profiled {
                trace: "trace-abc-1.json.gz".into(),
                logs: vec!["profile-abc-1.log".into()],
                cached: false,
            })
            .contains(r#""status":"profiled""#)
        );
        assert!(
            json_reply(&CodegenReply::Unconfigured {
                reason: "profile is not configured for this rig/scenario".into()
            })
            .contains(r#""status":"unconfigured""#)
        );
    }

    #[test]
    fn wrap_command_profile_writes_out_with_the_ext_and_base64s_it() {
        let w = wrap_command_profile("capture --out \"$OUT\"", "json.gz");
        assert!(w.contains("set -eu"));
        assert!(w.contains(&format!("OUT={JOB_TRACE_BASE}.json.gz")));
        assert!(w.contains(RESULT_SENTINEL));
        assert!(w.contains("base64 \"$OUT\""));
        // The size guard fires BEFORE the sentinel: an oversize trace becomes the marker + a failed
        // job, never a truncated base64 tail that decodes to a plausible-looking artifact.
        assert!(
            w.contains(&format!("-gt {FALLBACK_TRACE_MAX_BYTES}")),
            "{w}"
        );
        assert!(w.contains(TRACE_TOO_LARGE_MARKER), "{w}");
        let guard_at = w.find(TRACE_TOO_LARGE_MARKER).unwrap();
        let sentinel_at = w.find(RESULT_SENTINEL).unwrap();
        assert!(guard_at < sentinel_at, "guard must precede the sentinel");
    }

    #[test]
    fn wrap_command_profile_pvc_writes_out_onto_the_volume_without_base64() {
        let w = wrap_command_profile_pvc(
            "capture --out \"$OUT\"",
            "/artifacts/crucible-profile-1.json.gz",
        );
        assert!(w.contains("set -eu"));
        assert!(w.contains("OUT=/artifacts/crucible-profile-1.json.gz"));
        assert!(!w.contains("base64"), "{w}");
        assert!(!w.contains(RESULT_SENTINEL), "{w}");
    }

    #[test]
    fn oversize_fallback_trace_yields_a_configure_the_pvc_error() {
        let logs = format!("capturing...\n{TRACE_TOO_LARGE_MARKER} 72351744\n");
        let reason = trace_too_large_reason(&logs).unwrap();
        assert!(reason.contains("72351744"), "{reason}");
        assert!(reason.contains("BROKER_CODEGEN_ARTIFACTS_PVC"), "{reason}");
        // Ordinary failure logs don't trip the guard.
        assert!(trace_too_large_reason("CUDA error: out of memory").is_none());
    }

    #[test]
    fn artifacts_pvc_parses_with_defaults_and_is_off_without_the_claim() {
        assert_eq!(artifacts_pvc(None, None, None), None);
        // Mount and dir are unused without the claim.
        assert_eq!(
            artifacts_pvc(None, Some("/x".into()), Some("/y".into())),
            None
        );
        // Claim alone: default mount, local dir = the mount path.
        let a = artifacts_pvc(Some("crucible-artifacts".into()), None, None).unwrap();
        assert_eq!(a.claim_name, "crucible-artifacts");
        assert_eq!(a.mount_path, "/artifacts");
        assert_eq!(a.local_dir, PathBuf::from("/artifacts"));
        // Explicit mount + a differing loop-pod dir.
        let a = artifacts_pvc(
            Some("c".into()),
            Some("/traces".into()),
            Some("/mnt/traces".into()),
        )
        .unwrap();
        assert_eq!(a.mount_path, "/traces");
        assert_eq!(a.local_dir, PathBuf::from("/mnt/traces"));
    }

    #[test]
    fn model_pvc_unset_means_no_weights_mount() {
        // SAFETY: no other test reads these vars; the mutation scopes this assertion.
        unsafe {
            std::env::set_var("BROKER_CODEGEN_NAMESPACE", "ns");
            std::env::remove_var("BROKER_CODEGEN_MODEL_PVC");
            std::env::remove_var("BROKER_CODEGEN_WORKSPACE_PVC");
        }
        let env = JobEnv::from_env().expect("model_pvc is optional");
        assert!(
            env.pvc_mounts.is_empty(),
            "unset model_pvc must skip the weights mount entirely: {:?}",
            env.pvc_mounts
        );
        // Set → exactly one RO mount at the default path (no domain-flavored fallback name).
        unsafe { std::env::set_var("BROKER_CODEGEN_MODEL_PVC", "weights") };
        let env = JobEnv::from_env().unwrap();
        unsafe {
            std::env::remove_var("BROKER_CODEGEN_MODEL_PVC");
            std::env::remove_var("BROKER_CODEGEN_NAMESPACE");
        }
        assert_eq!(
            env.pvc_mounts,
            vec![PvcMount {
                claim_name: "weights".into(),
                mount_path: "/models".into(),
                read_only: true,
            }]
        );
    }

    fn test_job_env(artifacts: Option<ArtifactsPvc>) -> JobEnv {
        JobEnv {
            namespace: "test-ns".into(),
            queue_name: "q".into(),
            pull_secret: None,
            cpu: "16".into(),
            mem_request: "128Gi".into(),
            mem_limit: "200Gi".into(),
            shm_size_gi: 16,
            active_deadline_seconds: 5400,
            ttl_seconds: 86400,
            owner: None,
            pvc_mounts: vec![PvcMount {
                claim_name: "model-cache".into(),
                mount_path: "/models".into(),
                read_only: true,
            }],
            artifacts,
            spoke: None,
        }
    }

    #[test]
    fn artifacts_mount_renders_only_on_profile_jobs() {
        let a = artifacts_pvc(Some("crucible-artifacts".into()), None, None).unwrap();
        let env = test_job_env(Some(a.clone()));
        // A measure-shaped spec (no extra mounts): the artifacts claim must NOT appear even when
        // the substrate has it configured.
        let bench = job_spec(
            &env,
            2,
            "repo@sha256:x",
            "crucible-bench-1",
            "cmd",
            &[],
            &[],
        );
        assert!(
            bench
                .pvc_mounts
                .iter()
                .all(|m| m.claim_name != "crucible-artifacts"),
            "{:?}",
            bench.pvc_mounts
        );
        // A profile-shaped spec mounts it WRITABLE at the configured path.
        let extra = vec![PvcMount {
            claim_name: a.claim_name.clone(),
            mount_path: a.mount_path.clone(),
            read_only: false,
        }];
        let prof = job_spec(
            &env,
            2,
            "repo@sha256:x",
            "crucible-profile-1",
            "cmd",
            &[],
            &extra,
        );
        let m = prof
            .pvc_mounts
            .iter()
            .find(|m| m.claim_name == "crucible-artifacts")
            .expect("profile job must mount the artifacts PVC");
        assert_eq!(m.mount_path, "/artifacts");
        assert!(!m.read_only, "the trace is written there");
        // The base mounts survive alongside.
        assert!(
            prof.pvc_mounts
                .iter()
                .any(|m| m.claim_name == "model-cache")
        );
    }

    #[test]
    fn collect_trace_artifact_moves_the_file_into_the_store() {
        let dir = std::env::temp_dir().join(format!("codegen-artifacts-test-{}", nonce()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = LogStore {
            dir: dir.join("store"),
        };
        let src = dir.join("crucible-profile-1.json.gz");
        std::fs::write(&src, b"\x1f\x8b-gzip-ish-bytes").unwrap();
        let handle = collect_trace_artifact(&store, &src, "repo@sha256:abc", "json.gz").unwrap();
        assert!(handle.starts_with("trace-"), "{handle}");
        assert!(handle.ends_with(".json.gz"), "{handle}");
        // The source is consumed (the PVC is transport rather than storage) and the bytes round-trip.
        assert!(!src.exists());
        let (bytes, _, total) = store.read_bytes(&handle, 0).unwrap();
        assert_eq!(bytes, b"\x1f\x8b-gzip-ish-bytes");
        assert_eq!(total, 17);
        // A missing $OUT and an empty artifact are hard errors rather than fake handles.
        assert!(
            collect_trace_artifact(&store, &dir.join("nope.json.gz"), "d", "json.gz")
                .unwrap_err()
                .contains("write $OUT")
        );
        std::fs::write(dir.join("empty.json.gz"), b"").unwrap();
        assert!(
            collect_trace_artifact(&store, &dir.join("empty.json.gz"), "d", "json.gz")
                .unwrap_err()
                .contains("empty")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn split_profile_output_decodes_the_base64_tail() {
        let raw = b"gzip-trace-bytes-\x00\x01\x02";
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let logs = format!("serving...\ncapturing...\n{RESULT_SENTINEL}\n{b64}\n");
        let (before, bytes) = split_profile_output(&logs).unwrap();
        assert!(before.contains("capturing"));
        assert!(!before.contains(RESULT_SENTINEL));
        assert_eq!(bytes, raw);
        // No sentinel, or non-base64 / empty after it, are hard errors.
        assert!(split_profile_output("no marker here").is_err());
        assert!(split_profile_output(&format!("{RESULT_SENTINEL}\n")).is_err());
        assert!(split_profile_output(&format!("{RESULT_SENTINEL}\n!!!not-b64!!!")).is_err());
    }

    #[test]
    fn profile_memo_replays_a_captured_trace() {
        let st = test_state();
        let key = measure_key("d", "profile", &[]);
        st.put(
            key.clone(),
            Cached::Profile {
                trace: "trace-d-1.json.gz".into(),
                logs: vec!["profile-d-1.log".into()],
            },
        );
        match st.get(&key) {
            Some(Cached::Profile { trace, .. }) => assert_eq!(trace, "trace-d-1.json.gz"),
            _ => panic!("expected a cached profile"),
        }
        // A profile key is distinct from a benchmark key on the same digest.
        assert_ne!(key, measure_key("d", "benchmark", &[]));
    }

    #[test]
    fn store_artifact_preserves_the_extension_and_read_bytes_roundtrips() {
        let store = LogStore {
            dir: std::env::temp_dir().join(format!("codegen-trace-test-{}", nonce())),
        };
        let blob: Vec<u8> = (0..=255u8).cycle().take(70 * 1024).collect();
        let (handle, bytes) = store
            .store_artifact("trace", "repo@sha256:abc", "json.gz", &blob)
            .unwrap();
        assert!(handle.ends_with(".json.gz"), "{handle}");
        assert!(handle.starts_with("trace-"), "{handle}");
        assert_eq!(bytes as usize, blob.len());
        // Read the whole artifact back across windows and confirm byte-exactness.
        let mut out = Vec::new();
        let mut offset = 0usize;
        loop {
            let (chunk, next, total) = store.read_bytes(&handle, offset).unwrap();
            assert_eq!(total, blob.len());
            out.extend_from_slice(&chunk);
            if next >= total {
                break;
            }
            offset = next;
        }
        assert_eq!(out, blob);
        assert!(store.read_bytes("../etc/passwd", 0).is_err());
        std::fs::remove_dir_all(&store.dir).ok();
    }

    fn tracked(name: &str, kind: &'static str, result: Option<JobResult>) -> TrackedJob {
        TrackedJob {
            name: name.to_string(),
            kind,
            digest: "ghcr.io/example/img@sha256:abc".to_string(),
            namespace: "test-ns".to_string(),
            queue_name: "crucible-measure".to_string(),
            started_at: 1_700_000_000,
            log: format!("{kind}-abc-1.log"),
            result,
        }
    }

    #[test]
    fn job_ring_records_at_submit_bounds_and_marks_terminal() {
        let st = test_state();
        for i in 0..JOB_RING_CAP + 5 {
            st.record_job(tracked(&format!("crucible-bench-{i}"), "bench", None));
        }
        let jobs = st.jobs_snapshot();
        assert_eq!(jobs.len(), JOB_RING_CAP, "ring is bounded");
        // Newest first; the oldest 5 fell off the front.
        assert_eq!(jobs[0].name, format!("crucible-bench-{}", JOB_RING_CAP + 4));
        assert!(jobs.iter().all(|j| j.name != "crucible-bench-0"));

        // Terminal transition sticks to the right entry.
        st.finish_job("crucible-bench-10", JobResult::Succeeded);
        let jobs = st.jobs_snapshot();
        let done = jobs.iter().find(|j| j.name == "crucible-bench-10").unwrap();
        assert_eq!(done.result, Some(JobResult::Succeeded));
        assert!(
            jobs.iter()
                .filter(|j| j.name != "crucible-bench-10")
                .all(|j| j.result.is_none())
        );
        // Finishing an unknown (already-evicted) job is a no-op rather than a panic.
        st.finish_job("crucible-bench-0", JobResult::Failed);
    }

    #[test]
    fn lifecycle_derivation_covers_the_ladder() {
        use crate::codegen::derive_lifecycle as d;
        // A recorded terminal result wins outright.
        assert_eq!(d(Some(JobResult::Succeeded), None, None), "succeeded");
        assert_eq!(d(Some(JobResult::Failed), None, None), "failed");
        assert_eq!(d(Some(JobResult::TimedOut), None, None), "failed");
        // Then the pod phase.
        let admitted = KueueStatus {
            quota_reserved: true,
            admitted: true,
            pending_reason: None,
        };
        assert_eq!(d(None, Some(&admitted), Some("Running")), "running");
        assert_eq!(d(None, Some(&admitted), Some("Succeeded")), "succeeded");
        assert_eq!(d(None, Some(&admitted), Some("Failed")), "failed");
        // Admitted but no pod yet.
        assert_eq!(d(None, Some(&admitted), None), "admitted");
        assert_eq!(d(None, Some(&admitted), Some("Pending")), "admitted");
        // A pod in ANY phase means Kueue unsuspended the Job, workload lookup or not.
        assert_eq!(d(None, None, Some("Pending")), "admitted");
        // A live workload without admission = queued.
        let waiting = KueueStatus {
            quota_reserved: false,
            admitted: false,
            pending_reason: Some("insufficient quota".into()),
        };
        assert_eq!(d(None, Some(&waiting), None), "queued");
        // Neither workload nor pod in sight: honest `unknown`, never a claimed `queued`.
        assert_eq!(d(None, None, None), "unknown");
    }

    #[test]
    fn jobs_reply_reports_terminal_from_state_and_live_from_the_lookup() {
        let jobs = vec![
            tracked("crucible-bench-live", "bench", None),
            tracked(
                "crucible-profile-done",
                "profile",
                Some(JobResult::Succeeded),
            ),
            tracked("crucible-bench-late", "bench", Some(JobResult::TimedOut)),
        ];
        let reply = jobs_reply(&jobs, |job| {
            // The lookup must never fire for terminal entries (the Job may be TTL-reaped).
            assert_eq!(
                job.name, "crucible-bench-live",
                "live lookup on a terminal job"
            );
            LiveJobStatus {
                kueue: Some(KueueStatus {
                    quota_reserved: false,
                    admitted: false,
                    pending_reason: Some("insufficient quota for nvidia.com/gpu".into()),
                }),
                pod: PodStatus::default(),
            }
        });
        assert_eq!(reply["status"], "jobs");
        let entries = reply["jobs"].as_array().unwrap();
        assert_eq!(entries.len(), 3);

        let live = &entries[0];
        assert_eq!(live["lifecycle"], "queued");
        assert_eq!(live["kueue"]["admitted"], false);
        assert_eq!(
            live["kueue"]["pending_reason"],
            "insufficient quota for nvidia.com/gpu"
        );
        assert_eq!(live["pod"], Value::Null, "no pod until scheduled");
        assert_eq!(live["log"], "bench-abc-1.log");
        assert_eq!(live["digest"], "ghcr.io/example/img@sha256:abc");

        let done = &entries[1];
        assert_eq!(done["lifecycle"], "succeeded");
        assert_eq!(done["kueue"]["admitted"], true);
        assert_eq!(done["kueue"].get("pending_reason"), None);

        // TimedOut may have spent its whole life queued: failed, and `admitted` is OMITTED (an
        // honest unknown), never asserted false.
        let late = &entries[2];
        assert_eq!(late["lifecycle"], "failed");
        assert_eq!(late["kueue"].get("admitted"), None);
        assert_eq!(late["kueue"]["queue_name"], "crucible-measure");
    }

    #[test]
    fn jobs_reply_degrades_an_unavailable_live_view_to_unknown() {
        // A vanished job / timed-out lookup: nothing known beyond the recorded submission.
        let jobs = vec![tracked("crucible-bench-live", "bench", None)];
        let reply = jobs_reply(&jobs, |_| LiveJobStatus::default());
        let entry = &reply["jobs"][0];
        assert_eq!(entry["lifecycle"], "unknown");
        assert_eq!(entry["kueue"].get("admitted"), None, "not asserted");
        assert_eq!(entry["pod"], Value::Null);
        // The state-derived facts still ride along for correlation.
        assert_eq!(entry["log"], "bench-abc-1.log");
        assert_eq!(entry["kueue"]["queue_name"], "crucible-measure");
    }

    #[test]
    fn jobs_reply_reports_a_running_pod() {
        let jobs = vec![tracked("crucible-bench-live", "bench", None)];
        let reply = jobs_reply(&jobs, |_| LiveJobStatus {
            kueue: Some(KueueStatus {
                quota_reserved: true,
                admitted: true,
                pending_reason: None,
            }),
            pod: PodStatus {
                phase: Some("Running".into()),
                started_at: Some("2026-07-14T12:00:00Z".into()),
            },
        });
        let entry = &reply["jobs"][0];
        assert_eq!(entry["lifecycle"], "running");
        assert_eq!(entry["pod"]["phase"], "Running");
        assert_eq!(entry["pod"]["started_at"], "2026-07-14T12:00:00Z");
        assert_eq!(entry["kueue"]["admitted"], true);
    }

    #[test]
    fn allocated_handle_is_live_tailable_and_final_overwrite_wins() {
        let store = LogStore {
            dir: std::env::temp_dir().join(format!("codegen-live-log-test-{}", nonce())),
        };
        let (handle, path) = store.allocate("bench", "repo@sha256:abc").unwrap();
        assert!(handle.starts_with("bench-"), "{handle}");
        assert!(handle.ends_with(".log"), "{handle}");
        // The handle is readable (empty) from the moment it's minted, before any work ran.
        assert_eq!(store.read(&handle, 0).unwrap(), (String::new(), 0, 0));

        // A producer appends mid-run (standing in for buildah's write-through fd); a concurrent
        // reader tails the new bytes via the previous next_offset.
        use std::io::Write;
        let mut producer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        producer.write_all(b"step 1\n").unwrap();
        let (text, next, total) = store.read(&handle, 0).unwrap();
        assert_eq!(text, "step 1\n");
        assert_eq!((next, total), (7, 7));
        producer.write_all(b"step 2\n").unwrap();
        let (text, next, total) = store.read(&handle, next).unwrap();
        assert_eq!(text, "step 2\n");
        assert_eq!((next, total), (14, 14));

        // The final authoritative collection replaces the streamed content wholesale (no poll gaps).
        store
            .overwrite(&handle, b"the complete authoritative log\n")
            .unwrap();
        let (text, _, total) = store.read(&handle, 0).unwrap();
        assert_eq!(text, "the complete authoritative log\n");
        assert_eq!(total, 31);
        // Overwrite refuses path-like handles just like read.
        assert!(store.overwrite("../etc/passwd", b"x").is_err());
        std::fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn log_store_roundtrips_and_rejects_path_traversal() {
        let store = LogStore {
            dir: std::env::temp_dir().join(format!("codegen-logs-test-{}", nonce())),
        };
        let (handle, bytes) = store
            .store_artifact("bench", "repo@sha256:abc", "log", b"hello logs")
            .unwrap();
        assert_eq!(bytes, 10);
        let (text, next, total) = store.read(&handle, 0).unwrap();
        assert_eq!(text, "hello logs");
        assert_eq!((next, total), (10, 10));
        // Offset past the end is a clean empty tail.
        assert_eq!(store.read(&handle, 99).unwrap().0, "");
        assert!(store.read("../etc/passwd", 0).is_err());
        assert!(store.read("sub/dir.log", 0).is_err());
        assert!(store.read("", 0).is_err());
        std::fs::remove_dir_all(&store.dir).ok();
    }

    #[test]
    fn derive_dockerfile_emits_copy_chown_only_when_configured() {
        let dir =
            std::env::temp_dir().join(format!("codegen-df-{}-{}", std::process::id(), nonce()));
        let read = |cfg: &ToolsConfig| {
            let path = dir.join(format!("Dockerfile.{}", nonce()));
            write_derive_dockerfile(&path, &cfg.build, BuildMode::Derive).unwrap();
            std::fs::read_to_string(&path).unwrap()
        };

        // Unset: the plain COPY, byte-compatible with the pre-chown behavior.
        let plain = bench_cfg();
        let body = read(&plain);
        assert!(body.starts_with("FROM ghcr.io/x/base@sha256:abc"), "{body}");
        assert!(body.contains("\nCOPY . /workspace/vllm\n"), "{body}");
        assert!(!body.contains("--chown"), "{body}");
        assert!(
            body.contains("RUN VLLM_USE_PRECOMPILED=1 pip install -e ."),
            "{body}"
        );

        // Set: COPY --chown so the base image's non-root USER owns the tree.
        let overlay: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "gpus": 2,
            "build": {"base_image": "ghcr.io/x/base@sha256:abc", "src_dir": "/workspace/vllm", "install_cmd": "install", "copy_chown": "1000:1000"},
            "benchmark": {"command": "c"},
            "lm_eval": {"command": "l"}
        }))
        .unwrap();
        let owned = overlay.finalize(2).unwrap();
        let body = read(&owned);
        assert!(
            body.contains("\nCOPY --chown=1000:1000 . /workspace/vllm\n"),
            "{body}"
        );

        // An empty configured value is treated as unset, never emitted as `--chown=`.
        let overlay: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "gpus": 2,
            "build": {"base_image": "b", "src_dir": "/src", "install_cmd": "install", "copy_chown": "  "},
            "benchmark": {"command": "c"},
            "lm_eval": {"command": "l"}
        }))
        .unwrap();
        assert_eq!(overlay.finalize(2).unwrap().build.copy_chown, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    fn init_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codegen-local-repo-{}-{}",
            std::process::id(),
            nonce()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(ok.success());
        std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        dir
    }

    fn porcelain(dir: &Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn local_workdir_detects_a_git_checkout() {
        let repo = init_repo();
        assert_eq!(local_workdir(&repo.to_string_lossy()), Some(repo.clone()));
        // A dir without .git, and a missing path, are not local workdirs.
        let plain = std::env::temp_dir().join(format!("codegen-plain-{}", nonce()));
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(local_workdir(&plain.to_string_lossy()), None);
        assert_eq!(local_workdir("/nonexistent/codegen-path"), None);
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&plain).ok();
    }

    #[test]
    fn local_tree_hash_tracks_the_working_tree_without_touching_the_index() {
        let repo = init_repo();
        let before = porcelain(&repo);

        // Deterministic: the same tree hashes the same.
        let h1 = local_tree_hash(&repo).unwrap();
        let h2 = local_tree_hash(&repo).unwrap();
        assert_eq!(h1, h2);

        // An uncommitted edit to a file changes the hash (the agent edits without committing).
        std::fs::write(repo.join("a.txt"), "alpha changed\n").unwrap();
        let h3 = local_tree_hash(&repo).unwrap();
        assert_ne!(h1, h3);

        // A new untracked file changes the hash.
        std::fs::write(repo.join("b.txt"), "beta\n").unwrap();
        let h4 = local_tree_hash(&repo).unwrap();
        assert_ne!(h3, h4);

        // The repo's real index was never staged into: status is byte-identical (files still
        // untracked, nothing added).
        assert_eq!(porcelain(&repo), format!("{before}?? b.txt\n"));
        assert!(before.contains("?? a.txt"), "{before}");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn export_tree_reproduces_exactly_the_hashed_tree() {
        let repo = std::env::temp_dir().join(format!(
            "codegen-export-test-{}-{}",
            std::process::id(),
            nonce()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("kernel.cuh"), "tracked\n").unwrap();
        std::fs::write(repo.join("untracked.py"), "also hashed\n").unwrap();
        // Ignored files are outside the hash; the export must not ship them either (the
        // .dockerignore-divergence class).
        std::fs::write(repo.join(".gitignore"), "*.pyc\n").unwrap();
        std::fs::write(repo.join("stale.pyc"), "ignored\n").unwrap();

        let hash = local_tree_hash(&repo).unwrap();
        // SAFETY: tests run single-threaded per process env; the var scopes the staging dir.
        unsafe { std::env::set_var("BROKER_CODEGEN_CTX", repo.join("ctx").to_str().unwrap()) };
        let ctx = export_tree(&repo, &hash).unwrap();
        unsafe { std::env::remove_var("BROKER_CODEGEN_CTX") };

        assert!(ctx.join("kernel.cuh").exists());
        assert!(
            ctx.join("untracked.py").exists(),
            "add -A includes untracked"
        );
        assert!(ctx.join(".gitignore").exists());
        assert!(!ctx.join("stale.pyc").exists(), "ignored files never ship");
        assert!(
            !ctx.join(".git").exists(),
            "archive exports content, not the repo"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn hex_token_and_short_hint() {
        assert_eq!(hex_token("deadbeef  -"), "deadbeef");
        assert_eq!(short_hint("repo@sha256:abcdef123456"), "abcdef123456");
        assert_eq!(short_hint(":::"), "x");
    }

    fn measured_fixture() -> CodegenReply {
        CodegenReply::Measured {
            metrics: BTreeMap::from([("tpot_ms".into(), 9.1)]),
            objective: Objective {
                key: "tpot_ms".into(),
                direction: Direction::Lower,
            },
            logs: vec!["bench-d-1.log".into()],
            cached: false,
        }
    }

    #[test]
    fn pending_reply_serializes_with_a_status_tag() {
        let pending = json_reply(&CodegenReply::Pending {
            job: "crucible-bench-42".into(),
            log: "bench-d-1.log".into(),
            waited_secs: 1200,
            hint: PENDING_HINT,
        });
        assert!(pending.contains(r#""status":"pending""#), "{pending}");
        assert!(pending.contains(r#""job":"crucible-bench-42""#));
        assert!(pending.contains(r#""log":"bench-d-1.log""#));
        assert!(pending.contains("re-issue"), "{pending}");
    }

    #[test]
    fn inflight_wait_degrades_to_pending_then_hands_over_the_reply() {
        let entry = Arc::new(InflightJob::new(
            "crucible-bench-1".into(),
            "bench-d-1.log".into(),
        ));
        // Budget exhausted before the worker finishes: pending, naming the live job + log.
        match entry.wait(Duration::from_millis(1)) {
            CodegenReply::Pending { job, log, .. } => {
                assert_eq!(job, "crucible-bench-1");
                assert_eq!(log, "bench-d-1.log");
            }
            other => panic!("expected pending, got {other:?}"),
        }
        // The worker resolves; a waiter already blocked on the condvar gets the terminal reply.
        let worker_entry = entry.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            worker_entry.finish(measured_fixture());
        });
        assert_eq!(entry.wait(Duration::from_secs(10)), measured_fixture());
        t.join().unwrap();
        // A late attach finds the reply already in the slot without waiting.
        assert_eq!(entry.wait(Duration::ZERO), measured_fixture());
    }

    #[test]
    fn detach_and_wait_returns_pending_and_the_reissue_attaches() {
        let st = Arc::new(CodegenState::new());
        let key = measure_key("repo@sha256:d", "benchmark", &[]);
        let entry = Arc::new(InflightJob::new(
            "crucible-bench-7".into(),
            "bench-d-1.log".into(),
        ));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let worker_st = st.clone();
        let worker_key = key.clone();
        let reply = detach_and_wait(&st, key.clone(), entry, Duration::ZERO, move || {
            // Blocks until the test releases it, standing in for a queued Kueue job; memoize
            // before returning, like the real workers do.
            rx.recv().ok();
            worker_st.put(
                worker_key,
                Cached::Measure {
                    metrics: BTreeMap::from([("tpot_ms".into(), 9.1)]),
                    logs: vec!["bench-d-1.log".into()],
                },
            );
            measured_fixture()
        });
        assert!(
            matches!(reply, CodegenReply::Pending { .. }),
            "zero budget must degrade to pending, got {reply:?}"
        );
        // A re-issue while the job runs attaches to the same worker instead of resubmitting.
        let attached = st.inflight_get(&key).expect("entry stays in the map");
        tx.send(()).unwrap();
        assert_eq!(attached.wait(Duration::from_secs(10)), measured_fixture());
        // Memo-before-removal: once the entry leaves the map the memo must already hold the
        // result, so an attach miss + memo miss can't lose a finished job.
        while st.inflight_get(&key).is_some() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(st.get(&key), Some(Cached::Measure { .. })),
            "memo must be written before the inflight entry is removed"
        );
    }
}
