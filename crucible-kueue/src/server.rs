//! The MCP tool surface. Every tool resolves the [`Caller`] the auth layer attached to the HTTP
//! request and acts in the namespace the call names; past submit, mutations need an object this
//! server created for the same run, unless the server runs with [`Ownership::Rbac`].

use crate::caller::{Caller, Identity, NameError, Namespace, QueueName, Refusal};
use crate::config::{Config, Labels, Ownership, QUEUE_LABEL};
use crate::hardware;
use crate::kinds;
use crate::kueue::{
    self, ClusterQueue, KubeError, KueueVersion, LocalQueue, ResourceFlavor, Workload,
};
use crate::manifest::{self, ManifestError, Normalized, WorkloadKind};
use crate::namespaces::{self, Namespaces};
use crate::report::{self, Detail, Situation, Until, WaitEnd};
use crate::status::{self, Phase, Readiness, Rollup};
use crate::task::{self, Outcome, TaskRef};
use crucible_mcp_auth::principal::Principal;
use k8s_openapi::api::core::v1::{Event, Node, Pod};
use kube::api::{
    Api, DeleteParams, DynamicObject, ListParams, LogParams, Patch, PatchParams, PostParams,
};
use kube::core::SelectorExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResponse, CallToolResult, CancelTaskParams, ContentBlock, CreateTaskResult,
    DetailedTask, GetTaskParams, GetTaskResult, ServerCapabilities, ServerConfig, Task,
    TaskPayload, TaskStatus,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_WORKLOADS_SHOWN: usize = 20;
const MAX_REPLICAS: i64 = 1000;
const MAX_LOG_PODS: usize = 4;
const DEFAULT_TAIL: i64 = 200;
const MAX_TAIL: i64 = 2000;
const MAX_LOG_BYTES: i64 = 64 * 1024;
const DEFAULT_LOG_BUDGET: usize = 16 * 1024;
const MIN_LOG_BUDGET: usize = 1024;
const MAX_PROBLEM_PODS: usize = 5;
const SUMMARY_EVENTS: usize = 10;
const FULL_EVENTS: usize = 15;
const MAX_WAIT_SECONDS: u64 = 50;
const WAIT_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("request carried no authenticated caller")]
    NoCaller,
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Name(#[from] NameError),
    #[error(transparent)]
    Kube(#[from] KubeError),
    #[error("{kind} {name} not found in namespace {namespace}{}", gone_hint(.kind))]
    NotFound {
        kind: &'static str,
        name: String,
        namespace: String,
    },
    #[error("{kind} {name} was not created by crucible-kueue for run {run}")]
    NotOwned {
        kind: &'static str,
        name: String,
        run: String,
    },
    #[error(
        "plan_hash {got} does not match the manifest (expected {want}); re-run workload_plan and review it"
    )]
    HashMismatch { got: String, want: String },
    #[error(
        "{kind} {name} already exists with different content (plan {existing}); delete it or pick another name"
    )]
    Conflict {
        kind: &'static str,
        name: String,
        existing: String,
    },
    #[error("{kind} {name} cannot be resumed: {detail}")]
    NotStopped {
        kind: &'static str,
        name: String,
        detail: &'static str,
    },
    #[error("{kind} has no replicas to scale; workload_scale takes Deployment or StatefulSet")]
    NotScalable { kind: &'static str },
    #[error("replicas must be between 0 and {max}, got {got}")]
    Replicas { got: i64, max: i64 },
    #[error("the API server rejected the {kind}: {message}")]
    Rejected {
        kind: &'static str,
        code: u16,
        message: String,
    },
    #[error("namespace {namespace} does not exist or this server is not installed there; {}", namespaces::describe(.usable))]
    NotServed {
        namespace: String,
        usable: Vec<String>,
    },
    #[error("name a namespace: this token has no default one; {}", namespaces::describe(.usable))]
    NamespaceRequired { usable: Vec<String> },
}

/// Why a batch workload may be gone: a finished one is deleted after its TTL.
fn gone_hint(kind: &str) -> String {
    let batch = WorkloadKind::ALL
        .iter()
        .any(|k| k.is_batch() && k.kind() == kind);
    match batch {
        true => format!("; a finished {kind} is deleted ttlSecondsAfterFinished after it finishes"),
        false => String::new(),
    }
}

impl ToolError {
    fn status(&self) -> &'static str {
        match self {
            Self::Refused(_)
            | Self::Manifest(_)
            | Self::Name(_)
            | Self::NotOwned { .. }
            | Self::HashMismatch { .. }
            | Self::Conflict { .. }
            | Self::NotStopped { .. }
            | Self::NotScalable { .. }
            | Self::Replicas { .. }
            | Self::NoCaller => "refused",
            Self::Rejected { .. } => "rejected",
            Self::NotFound { .. } | Self::NotServed { .. } => "not_found",
            Self::NamespaceRequired { .. } => "refused",
            Self::Kube(e) if e.is_not_found() => "not_found",
            Self::Kube(_) => "error",
        }
    }

    /// The server's own credential was refused: it is not installed in the namespace, so the
    /// API server's message (which names its service account) must not reach the caller.
    fn server_forbidden(&self) -> bool {
        match self {
            Self::Kube(e) => e.code() == Some(403),
            Self::Rejected { code, message, .. } => {
                *code == 403 && !message.contains("admission webhook")
            }
            _ => false,
        }
    }

    /// Whether the same call may succeed later: an admission webhook that refused it may accept
    /// it once the workload settles, and the API server may come back.
    fn retryable(&self) -> bool {
        match self {
            Self::Rejected { code, message, .. } => {
                message.contains("admission webhook") || *code == 409 || *code == 429
            }
            Self::Kube(e) => e.retryable(),
            _ => false,
        }
    }
}

fn text_result(body: &Value, is_error: bool) -> CallToolResult {
    let content = vec![ContentBlock::text(body.to_string())];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// Every tool answers JSON text with a `status`. Anything but `ok` is a tool error result.
fn reply(result: Result<Value, ToolError>) -> CallToolResult {
    match result {
        Ok(Value::Object(mut map)) => {
            map.entry("status").or_insert_with(|| json!("ok"));
            text_result(&Value::Object(map), false)
        }
        Ok(other) => text_result(&json!({"status": "ok", "result": other}), false),
        Err(e) => {
            tracing::info!(error = %e, status = e.status(), "tool call refused or failed");
            text_result(
                &json!({"status": e.status(), "error": e.to_string(), "retryable": e.retryable()}),
                true,
            )
        }
    }
}

/// One read of a workload: the object, its pods, and the Workloads Kueue tracks it as.
struct Snapshot {
    object: DynamicObject,
    pods: Vec<Pod>,
    workloads: Vec<Workload>,
    rollup: Rollup,
}

impl Snapshot {
    fn readiness(&self, kind: WorkloadKind) -> Option<Readiness> {
        (!kind.is_batch()).then(|| status::readiness(&self.object.data, &self.rollup))
    }

    fn last_updated(&self) -> Option<String> {
        self.workloads
            .iter()
            .flat_map(|w| w.status.conditions.iter())
            .filter_map(|c| c.last_transition_time.clone())
            .max()
    }
}

#[derive(Serialize)]
struct EventLine {
    time: String,
    object: String,
    #[serde(rename = "type")]
    type_: Option<String>,
    reason: Option<String>,
    message: Option<String>,
    count: Option<i32>,
}

impl EventLine {
    fn line(&self) -> String {
        let mut out = format!(
            "{} {} {} {}: {}",
            self.time,
            self.object,
            self.type_.as_deref().unwrap_or_default(),
            self.reason.as_deref().unwrap_or_default(),
            self.message.as_deref().unwrap_or_default().trim()
        );
        if let Some(n) = self.count.filter(|n| *n > 1) {
            out.push_str(&format!(" (x{n})"));
        }
        out
    }
}

fn internal(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn new_task(namespace: &Namespace, kind: WorkloadKind, name: &str, obj: &DynamicObject) -> Task {
    let id = TaskRef {
        namespace: Some(namespace.to_string()),
        kind,
        name: name.to_string(),
        uid: obj.metadata.uid.clone().unwrap_or_default(),
    }
    .encode();
    let created = obj
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| t.0.to_string())
        .unwrap_or_default();
    Task::new(id, TaskStatus::Working, created.clone(), created)
        .with_poll_interval_ms(task::POLL_INTERVAL_MS)
        .with_status_message("submitted; waiting for Kueue admission")
}

fn parse_task(id: &str) -> Result<TaskRef, McpError> {
    TaskRef::parse(id).ok_or_else(|| McpError::invalid_params(format!("unknown task: {id}"), None))
}

fn refused(error: impl std::fmt::Display) -> McpError {
    McpError::invalid_request(error.to_string(), None)
}

fn request_parts(ctx: &RequestContext<RoleServer>) -> Result<&http::request::Parts, ToolError> {
    ctx.extensions
        .get::<http::request::Parts>()
        .ok_or(ToolError::NoCaller)
}

/// The annotations only the server writes: the plan hash and, for a run, the sandbox that acted.
fn stamp(object: &mut DynamicObject, labels: &Labels, plan_hash: &str, actor: Option<&str>) {
    let annotations = object
        .metadata
        .annotations
        .get_or_insert_with(BTreeMap::new);
    annotations.insert(labels.plan_hash.clone(), plan_hash.into());
    if let Some(actor) = actor {
        annotations.insert(labels.actor.clone(), actor.into());
    }
}

/// Whether `caller` created `obj`: this server's managed label, and the caller's run label and
/// kind of owner.
fn created_by(caller: &Caller, obj: &DynamicObject, labels: &Labels) -> bool {
    let set = obj.metadata.labels.as_ref();
    let label = |key: &str| set.and_then(|l| l.get(key)).map(String::as_str);
    label(&labels.managed_by) == Some(labels.manager.as_str())
        && label(&labels.run_id) == Some(caller.run_id.as_str())
        && label(&labels.owner_kind) == Some(caller.owner.as_str())
}

/// Whether `caller` may stop, resume, scale or delete `obj`. Under [`Ownership::Rbac`] the access
/// checks the guard ran already decided.
fn controls(caller: &Caller, obj: &DynamicObject, cfg: &Config) -> bool {
    match cfg.ownership {
        Ownership::Managed => created_by(caller, obj, &cfg.labels),
        Ownership::Rbac => true,
    }
}

fn owner_uids(obj: &DynamicObject, pods: &[Pod]) -> Vec<String> {
    obj.metadata
        .uid
        .iter()
        .chain(pods.iter().filter_map(|p| p.metadata.uid.as_ref()))
        .cloned()
        .collect()
}

fn owned_by<'a>(workloads: &'a [Workload], owners: &[String]) -> Vec<&'a Workload> {
    workloads
        .iter()
        .filter(|w| {
            w.metadata
                .owner_references
                .iter()
                .any(|o| owners.contains(&o.uid))
        })
        .collect()
}

fn replicas_of(obj: &DynamicObject) -> i64 {
    obj.data
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .unwrap_or(1)
}

fn identity(parts: &http::request::Parts) -> Result<&Identity, ToolError> {
    parts
        .extensions
        .get::<Identity>()
        .ok_or(ToolError::NoCaller)
}

/// The request's identity acting in `namespace` (its own when `None`).
fn caller(parts: &http::request::Parts, namespace: Option<&str>) -> Result<Caller, ToolError> {
    Ok(identity(parts)?.in_namespace(namespace)?)
}

fn api_rejection(kind: WorkloadKind, err: kube::Error) -> ToolError {
    match err {
        kube::Error::Api(s) if (400..500).contains(&s.code) && s.code != 404 => {
            ToolError::Rejected {
                kind: kind.kind(),
                code: s.code,
                message: s.message,
            }
        }
        source => ToolError::Kube(KubeError::api("create", kind.kind().to_string(), source)),
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueueArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    /// LocalQueue name in the namespace acted in (queues_list lists them).
    pub queue: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlanArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    /// LocalQueue name in the namespace acted in (queues_list lists them).
    pub queue: String,
    /// The workload as a JSON object: batch/v1 Job, ray.io/v1 RayJob, leaderworkerset.x-k8s.io/v1
    /// LeaderWorkerSet, apps/v1 Deployment, apps/v1 StatefulSet, jobset.x-k8s.io/v1alpha2 JobSet,
    /// or kubeflow.org/v1 PyTorchJob (whoami lists those installed). metadata.name is required;
    /// the namespace and queue label are filled in. Pod templates may not use host access,
    /// privilege, another service account, or scheduling fields that bypass Kueue; the server's
    /// allowlist profile (whoami) decides Secrets, volume claims and tolerations.
    pub manifest: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SubmitArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    /// LocalQueue name, the same one passed to workload_plan.
    pub queue: String,
    /// The manifest exactly as passed to workload_plan.
    pub manifest: Value,
    /// plan_hash returned by workload_plan for this queue and manifest.
    pub plan_hash: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkloadRef {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    /// The submitted object's kind.
    pub kind: WorkloadKind,
    /// metadata.name of the submitted object.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StatusArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    /// The submitted object's kind.
    pub kind: WorkloadKind,
    /// metadata.name of the submitted object.
    pub name: String,
    /// summary (default) or full, which adds every Workload's conditions, the object's status,
    /// and every pod.
    #[serde(default)]
    pub detail: Option<Detail>,
    /// Wait on the server until the workload reaches this point (or passes it), it settles
    /// short of it, a pod is stuck, or timeout_seconds passes; then answer as usual, with `wait`
    /// saying which.
    #[serde(default)]
    pub until: Option<Until>,
    /// How long `until` waits, in seconds (default and max 50).
    #[serde(default)]
    #[schemars(range(min = 1, max = 50))]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LogArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    /// The submitted object's kind.
    pub kind: WorkloadKind,
    /// metadata.name of the submitted object.
    pub name: String,
    /// One pod's name; omit for up to four of the workload's pods, failed ones first, then a
    /// RayJob's head and submitter or a LeaderWorkerSet's leader.
    #[serde(default)]
    pub pod: Option<String>,
    /// Container to read; omit for the pod's default container.
    #[serde(default)]
    pub container: Option<String>,
    /// Lines from the end of each log (default 200, or 2000 with grep).
    #[serde(default)]
    #[schemars(range(min = 1, max = 2000))]
    pub tail_lines: Option<i64>,
    /// Only lines written in the last this many seconds.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub since_seconds: Option<i64>,
    /// Keep only lines containing this text (case-sensitive), applied before the byte budget.
    #[serde(default)]
    pub grep: Option<String>,
    /// Total bytes of log text across all pods (default 16384, max 65536). Each pod keeps the
    /// end of its log; `bytes_omitted` counts what was cut from the front.
    #[serde(default)]
    #[schemars(range(min = 1024, max = 65536))]
    pub max_bytes: Option<usize>,
    /// Logs of the previous container instance, after a restart.
    #[serde(default)]
    pub previous: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScaleArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Deployment or StatefulSet.
    pub kind: WorkloadKind,
    /// metadata.name of the submitted object.
    pub name: String,
    /// Target replica count. Kueue admits a StatefulSet only when scaling to or from zero.
    #[schemars(range(min = 0, max = 1000))]
    pub replicas: i64,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct NamespaceArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ListArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    /// mine (default): what this run created; managed: what this server created for any run;
    /// all: every Kueue-queued workload of a served kind.
    #[serde(default)]
    pub filter: Option<ListFilter>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListFilter {
    #[default]
    Mine,
    Managed,
    All,
}

/// The label selector `filter` lists objects by.
fn list_selector(filter: ListFilter, labels: &Labels, caller: &Caller) -> String {
    match filter {
        ListFilter::Mine => format!(
            "{}={},{}={},{}={}",
            labels.managed_by,
            labels.manager,
            labels.run_id,
            caller.run_id.as_str(),
            labels.owner_kind,
            caller.owner.as_str()
        ),
        ListFilter::Managed => format!("{}={}", labels.managed_by, labels.manager),
        ListFilter::All => QUEUE_LABEL.to_string(),
    }
}

#[derive(Clone)]
pub struct KueueMcp {
    client: kube::Client,
    kueue: KueueVersion,
    config: Arc<Config>,
    /// The served kinds the cluster has installed.
    kinds: Arc<Vec<WorkloadKind>>,
    namespaces: Arc<Namespaces>,
    tool_router: ToolRouter<Self>,
}

#[derive(Serialize)]
struct PodSummary {
    name: String,
    phase: Option<String>,
    node: Option<String>,
    restarts: i32,
    waiting: Vec<String>,
    terminated: Vec<String>,
}

fn pod_summary(p: &Pod) -> PodSummary {
    let statuses = p
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.clone())
        .unwrap_or_default();
    PodSummary {
        name: p.metadata.name.clone().unwrap_or_default(),
        phase: p.status.as_ref().and_then(|s| s.phase.clone()),
        node: p.spec.as_ref().and_then(|s| s.node_name.clone()),
        restarts: statuses.iter().map(|c| c.restart_count).sum(),
        waiting: statuses
            .iter()
            .filter_map(|c| {
                let w = c.state.as_ref()?.waiting.as_ref()?;
                Some(format!(
                    "{}: {} {}",
                    c.name,
                    w.reason.clone().unwrap_or_default(),
                    w.message.clone().unwrap_or_default()
                ))
            })
            .collect(),
        terminated: statuses
            .iter()
            .filter_map(|c| {
                let t = c.state.as_ref()?.terminated.as_ref()?;
                Some(format!(
                    "{}: exit {} {}",
                    c.name,
                    t.exit_code,
                    t.reason.clone().unwrap_or_default()
                ))
            })
            .collect(),
    }
}

impl KueueMcp {
    pub fn new(
        client: kube::Client,
        kueue: KueueVersion,
        config: Arc<Config>,
        kinds: Arc<Vec<WorkloadKind>>,
        namespaces: Arc<Namespaces>,
    ) -> Self {
        Self {
            client,
            kueue,
            config,
            kinds,
            namespaces,
            tool_router: Self::tool_router(),
        }
    }

    /// Rewrites a failure that means the call named a namespace this server does not serve (its
    /// own credential refused there, a missing namespace default, or a missing LocalQueue outside
    /// the caller's namespaces) into one that lists the namespaces the caller can use.
    async fn explain(
        &self,
        parts: &http::request::Parts,
        requested: Option<&str>,
        result: Result<Value, ToolError>,
    ) -> Result<Value, ToolError> {
        let Err(e) = result else {
            return result;
        };
        let Some(principal) = parts.extensions.get::<Principal>() else {
            return Err(e);
        };
        let usable = || self.namespaces.usable(principal);
        match e {
            ToolError::Refused(Refusal::NamespaceRequired) => Err(ToolError::NamespaceRequired {
                usable: usable().await,
            }),
            e if e.server_forbidden() => {
                let namespace = requested
                    .or(principal.namespace.as_deref())
                    .unwrap_or_default()
                    .to_string();
                tracing::warn!(error = %e, %namespace, "this server's credential was refused");
                Err(ToolError::NotServed {
                    namespace,
                    usable: usable().await,
                })
            }
            ToolError::NotFound {
                kind: "LocalQueue",
                name,
                namespace,
            } => {
                let usable = usable().await;
                match usable.contains(&namespace) {
                    true => Err(ToolError::NotFound {
                        kind: "LocalQueue",
                        name,
                        namespace,
                    }),
                    false => Err(ToolError::NotServed { namespace, usable }),
                }
            }
            e => Err(e),
        }
    }

    /// The request's identity acting in `namespace` (its own when `None`), when this server serves
    /// that namespace; otherwise `not_found`, naming the namespaces the caller can use.
    async fn served_caller(
        &self,
        parts: &http::request::Parts,
        namespace: Option<&str>,
    ) -> Result<Caller, ToolError> {
        let c = caller(parts, namespace)?;
        if self.namespaces.serves(c.namespace.as_str()).await {
            return Ok(c);
        }
        let usable = match parts.extensions.get::<Principal>() {
            Some(principal) => self.namespaces.usable(principal).await,
            None => Vec::new(),
        };
        Err(ToolError::NotServed {
            namespace: c.namespace.as_str().to_string(),
            usable,
        })
    }

    /// The caller a task request acts as, in the namespace its task id names. The guard has
    /// already checked the method's scope and RBAC on the task's object.
    async fn task_caller(
        &self,
        ctx: &RequestContext<RoleServer>,
        t: &TaskRef,
    ) -> Result<Caller, McpError> {
        let parts = request_parts(ctx).map_err(refused)?;
        self.served_caller(parts, t.namespace.as_deref())
            .await
            .map_err(refused)
    }

    fn dynamic(&self, kind: WorkloadKind, ns: &str) -> Api<DynamicObject> {
        Api::namespaced_with(self.client.clone(), ns, &kind.api_resource())
    }

    async fn local_queue(&self, caller: &Caller, queue: &str) -> Result<LocalQueue, ToolError> {
        let queue = QueueName::parse(queue)?;
        let ns = caller.namespace.as_str();
        kueue::get::<LocalQueue>(
            &self.client,
            &self.kueue.local_queues(),
            Some(ns),
            queue.as_str(),
        )
        .await
        .map_err(|e| {
            if e.is_not_found() {
                ToolError::NotFound {
                    kind: "LocalQueue",
                    name: queue.to_string(),
                    namespace: ns.to_string(),
                }
            } else {
                e.into()
            }
        })
    }

    async fn cluster_queue(&self, lq: &LocalQueue) -> Result<ClusterQueue, ToolError> {
        Ok(kueue::get(
            &self.client,
            &self.kueue.cluster_queues(),
            None,
            &lq.spec.cluster_queue,
        )
        .await?)
    }

    async fn get_object(
        &self,
        caller: &Caller,
        kind: WorkloadKind,
        name: &str,
    ) -> Result<DynamicObject, ToolError> {
        let ns = caller.namespace.as_str();
        self.dynamic(kind, ns)
            .get(name)
            .await
            .map_err(|source| match source {
                kube::Error::Api(s) if s.code == 404 => ToolError::NotFound {
                    kind: kind.kind(),
                    name: name.to_string(),
                    namespace: ns.to_string(),
                },
                source => ToolError::Kube(KubeError::api(
                    "get",
                    format!("{} {name}", kind.kind()),
                    source,
                )),
            })
    }

    async fn owned_object(
        &self,
        caller: &Caller,
        kind: WorkloadKind,
        name: &str,
    ) -> Result<DynamicObject, ToolError> {
        let obj = self.get_object(caller, kind, name).await?;
        if !controls(caller, &obj, &self.config) {
            return Err(ToolError::NotOwned {
                kind: kind.kind(),
                name: name.to_string(),
                run: caller.run_id.to_string(),
            });
        }
        Ok(obj)
    }

    async fn namespace_workloads(&self, ns: &str) -> Result<Vec<Workload>, ToolError> {
        Ok(kueue::list(
            &self.client,
            &self.kueue.workloads(),
            Some(ns),
            &ListParams::default(),
        )
        .await?)
    }

    /// The object's Workloads: those it owns directly, plus those owned by its pods (Kueue makes
    /// one per Deployment pod, and a StatefulSet's group Workload is owned by its pods while
    /// the set is scaled to zero).
    async fn workloads_of(
        &self,
        ns: &str,
        obj: &DynamicObject,
        pods: &[Pod],
    ) -> Result<Vec<Workload>, ToolError> {
        let all = self.namespace_workloads(ns).await?;
        let owners = owner_uids(obj, pods);
        Ok(owned_by(&all, &owners).into_iter().cloned().collect())
    }

    /// The object's pods, found through its selector or its controller's labels.
    async fn pods_of(
        &self,
        ns: &str,
        kind: WorkloadKind,
        obj: &DynamicObject,
    ) -> Result<Vec<Pod>, ToolError> {
        let name = obj.metadata.name.as_deref().unwrap_or_default();
        let object = serde_json::to_value(obj).map_err(|source| KubeError::Decode {
            what: format!("{} {name}", kind.kind()),
            source,
        })?;
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let mut pods: Vec<Pod> = Vec::new();
        for selector in kinds::pod_selectors(kind, &object) {
            let lp = ListParams::default().labels(&selector.to_string());
            let found = api
                .list(&lp)
                .await
                .map_err(|source| KubeError::api("list", format!("pods of {name}"), source))?;
            for p in found.items {
                if !pods.iter().any(|q| q.metadata.uid == p.metadata.uid) {
                    pods.push(p);
                }
            }
        }
        Ok(pods)
    }

    /// The newest events about the named objects, oldest first.
    async fn events_for(&self, ns: &str, names: &[&str]) -> Result<Vec<EventLine>, ToolError> {
        let api: Api<Event> = Api::namespaced(self.client.clone(), ns);
        let mut out = Vec::new();
        for name in names {
            let lp = ListParams::default().fields(&format!("involvedObject.name={name}"));
            let events = api
                .list(&lp)
                .await
                .map_err(|source| KubeError::api("list", format!("events of {name}"), source))?;
            for e in events.items {
                let time = e
                    .last_timestamp
                    .as_ref()
                    .map(|t| t.0.to_string())
                    .or_else(|| e.event_time.as_ref().map(|t| t.0.to_string()))
                    .unwrap_or_default();
                out.push(EventLine {
                    time,
                    object: format!(
                        "{}/{}",
                        e.involved_object.kind.clone().unwrap_or_default(),
                        name
                    ),
                    type_: e.type_,
                    reason: e.reason,
                    message: e.message,
                    count: e.count,
                });
            }
        }
        out.sort_by(|a, b| a.time.cmp(&b.time));
        Ok(out)
    }

    async fn plan(
        &self,
        caller: &Caller,
        queue: &str,
        manifest: Value,
    ) -> Result<(Normalized, ClusterQueue), ToolError> {
        let lq = self.local_queue(caller, queue).await?;
        let queue = QueueName::parse(queue)?;
        let normalized = manifest::normalize(manifest, caller, &queue, &self.config)?;
        let cq = self.cluster_queue(&lq).await?;
        Ok((normalized, cq))
    }

    async fn create(
        &self,
        caller: &Caller,
        n: &Normalized,
        actor: Option<&str>,
        dry_run: bool,
    ) -> Result<DynamicObject, ToolError> {
        let mut object: DynamicObject =
            serde_json::from_value(n.object.clone()).map_err(|source| KubeError::Decode {
                what: format!("normalized {}", n.kind.kind()),
                source,
            })?;
        stamp(&mut object, &self.config.labels, &n.plan_hash, actor);
        let pp = PostParams {
            dry_run,
            field_manager: Some(self.config.labels.manager.to_string()),
        };
        self.dynamic(n.kind, caller.namespace.as_str())
            .create(&pp, &object)
            .await
            .map_err(|e| api_rejection(n.kind, e))
    }

    async fn set_replicas(
        &self,
        caller: &Caller,
        r: &WorkloadRef,
        replicas: Option<i64>,
        stopped_annotation: Option<Option<String>>,
    ) -> Result<(), ToolError> {
        let mut patch = json!({});
        if let Some(n) = replicas {
            patch["spec"] = json!({"replicas": n});
        }
        if let Some(value) = stopped_annotation {
            patch["metadata"] =
                json!({"annotations": {self.config.labels.stopped_replicas.as_str(): value}});
        }
        self.dynamic(r.kind, caller.namespace.as_str())
            .patch(
                &r.name,
                &PatchParams {
                    field_manager: Some(self.config.labels.manager.to_string()),
                    ..PatchParams::default()
                },
                &Patch::Merge(patch),
            )
            .await
            .map_err(|e| match e {
                kube::Error::Api(s) if (400..500).contains(&s.code) && s.code != 404 => {
                    ToolError::Rejected {
                        kind: r.kind.kind(),
                        code: s.code,
                        message: s.message,
                    }
                }
                source => ToolError::Kube(KubeError::api(
                    "patch",
                    format!("{} {}", r.kind.kind(), r.name),
                    source,
                )),
            })?;
        Ok(())
    }

    /// Stop/resume for Deployments and StatefulSets: scale to zero, remembering the replica
    /// count in an annotation, and scale back from it.
    async fn scale_active(
        &self,
        caller: &Caller,
        r: &WorkloadRef,
        obj: &DynamicObject,
        active: bool,
    ) -> Result<Value, ToolError> {
        let current = replicas_of(obj);
        let remembered = obj
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(&self.config.labels.stopped_replicas))
            .and_then(|v| v.parse::<i64>().ok());
        match (active, current, remembered) {
            (false, 0, _) => Ok(json!({"replicas": 0, "note": "already stopped"})),
            (false, n, _) => {
                self.set_replicas(caller, r, Some(0), Some(Some(n.to_string())))
                    .await?;
                Ok(json!({"replicas": 0, "stopped_from": n}))
            }
            (true, 0, Some(n)) => {
                self.set_replicas(caller, r, Some(n), Some(None)).await?;
                Ok(json!({"replicas": n}))
            }
            (true, 0, None) => Err(ToolError::NotStopped {
                kind: r.kind.kind(),
                name: r.name.clone(),
                detail: "it is at 0 replicas but was not stopped by workload_stop; use workload_scale",
            }),
            (true, n, _) => Ok(json!({"replicas": n, "note": "already running"})),
        }
    }

    async fn set_active(
        &self,
        caller: &Caller,
        r: &WorkloadRef,
        active: bool,
    ) -> Result<Value, ToolError> {
        let obj = self.owned_object(caller, r.kind, &r.name).await?;
        if r.kind.is_scalable() {
            return self.scale_active(caller, r, &obj, active).await;
        }
        let ns = caller.namespace.as_str();
        let pods = self.pods_of(ns, r.kind, &obj).await?;
        let wl = self
            .workloads_of(ns, &obj, &pods)
            .await?
            .into_iter()
            .next()
            .ok_or(ToolError::NotFound {
                kind: "Workload",
                name: format!("owned by {} {}", r.kind.kind(), r.name),
                namespace: ns.to_string(),
            })?;
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), ns, &self.kueue.workloads());
        api.patch(
            &wl.metadata.name,
            &PatchParams::default(),
            &Patch::Merge(json!({"spec": {"active": active}})),
        )
        .await
        .map_err(|source| {
            KubeError::api("patch", format!("Workload {}", wl.metadata.name), source)
        })?;
        Ok(json!({"workload": wl.metadata.name, "active": active}))
    }

    async fn snapshot(
        &self,
        c: &Caller,
        kind: WorkloadKind,
        name: &str,
    ) -> Result<Snapshot, ToolError> {
        let ns = c.namespace.as_str();
        let object = self.get_object(c, kind, name).await?;
        let pods = self.pods_of(ns, kind, &object).await?;
        let workloads = self.workloads_of(ns, &object, &pods).await?;
        let rollup = status::rollup(&workloads.iter().collect::<Vec<_>>());
        Ok(Snapshot {
            object,
            pods,
            workloads,
            rollup,
        })
    }

    /// Where the pending Workloads stand in their LocalQueue, and the first one's position, or
    /// why the visibility API could not say.
    async fn queue_positions(&self, ns: &str, pending: &[&Workload]) -> (Value, Option<i64>) {
        let Some(first) = pending.first() else {
            return (Value::Null, None);
        };
        let line = match kueue::pending_in_local_queue(
            &self.client,
            self.kueue,
            ns,
            &first.spec.queue_name,
            1000,
        )
        .await
        {
            Ok(line) => line,
            Err(e) => return (json!(format!("unavailable: {e}")), None),
        };
        let mut positions = Vec::new();
        let mut head = None;
        for w in pending {
            if let Some(p) = line.iter().find(|p| p.metadata.name == w.metadata.name) {
                head.get_or_insert(p.position_in_local_queue);
                positions.push(json!({"workload": w.metadata.name, "in_local_queue": p.position_in_local_queue, "in_cluster_queue": p.position_in_cluster_queue}));
            }
        }
        (json!(positions), head)
    }

    /// The plan's fit estimate against the queue as it is now; for a Deployment, one pod's.
    async fn refit(
        &self,
        c: &Caller,
        kind: WorkloadKind,
        snap: &Snapshot,
    ) -> Result<kueue::FitReport, String> {
        let queue = snap
            .workloads
            .first()
            .map(|w| w.spec.queue_name.clone())
            .unwrap_or_default();
        let lq = self
            .local_queue(c, &queue)
            .await
            .map_err(|e| e.to_string())?;
        let cq = self.cluster_queue(&lq).await.map_err(|e| e.to_string())?;
        let object = serde_json::to_value(&snap.object).map_err(|e| e.to_string())?;
        let sets = manifest::pod_sets_of(kind, &object).map_err(|e| e.to_string())?;
        let requests = match kind.admission() {
            manifest::Admission::PerPod => {
                sets.first().map(|s| s.per_pod.clone()).unwrap_or_default()
            }
            manifest::Admission::AllOrNothing => manifest::total_requests(&sets),
        };
        Ok(kueue::fit(&requests, &cq))
    }

    async fn report(
        &self,
        c: &Caller,
        kind: WorkloadKind,
        name: &str,
        snap: &Snapshot,
        detail: Detail,
    ) -> Value {
        let ns = c.namespace.as_str();
        let head = &snap.rollup.headline;
        let readiness = snap.readiness(kind);
        let pending: Vec<&Workload> = snap
            .workloads
            .iter()
            .filter(|w| status::phase(Some(w)).phase == Phase::Pending)
            .collect();
        let (queue_position, position) = self.queue_positions(ns, &pending).await;
        let fit = match head.phase {
            Phase::Pending => Some(self.refit(c, kind, snap).await),
            _ => None,
        };
        let problems = report::problem_pods(kind, &snap.pods);
        let mut names = vec![name];
        names.extend(
            snap.workloads
                .iter()
                .take(3)
                .map(|w| w.metadata.name.as_str()),
        );
        match problems.is_empty() {
            true => names.extend(snap.pods.iter().take(2).map(report::pod_name)),
            false => names.extend(
                problems
                    .iter()
                    .take(MAX_PROBLEM_PODS)
                    .map(|p| p.name.as_str()),
            ),
        }
        let events = self.events_for(ns, &names).await;
        let retention_seconds = snap
            .object
            .data
            .pointer("/spec/ttlSecondsAfterFinished")
            .and_then(Value::as_i64);
        let next_action = report::next_action(&Situation {
            kind,
            rollup: &snap.rollup,
            readiness,
            fit: fit
                .as_ref()
                .and_then(|f| f.as_ref().ok())
                .map(|f| f.verdict),
            position,
            problems: &problems,
            retention_seconds,
        });
        let admitted_to = snap
            .workloads
            .iter()
            .find_map(|w| w.status.admission.as_ref())
            .map(|a| {
                json!({
                    "cluster_queue": a.get("clusterQueue"),
                    "flavors": a.get("podSetAssignments").and_then(Value::as_array).map(|sets| {
                        sets.iter()
                            .map(|p| (p.get("name").and_then(Value::as_str).unwrap_or_default().to_string(), p.get("flavors").cloned().unwrap_or(Value::Null)))
                            .collect::<serde_json::Map<_, _>>()
                    }),
                })
            });
        let mut body = json!({
            "kind": kind,
            "name": name,
            "phase": head.phase,
            "reason": head.reason,
            "message": head.message,
            "workload_counts": snap.rollup.counts,
            "next_action": next_action,
            "pods": {"total": snap.pods.len(), "by_state": report::pod_counts(&snap.pods)},
            "problem_pods": problems.iter().take(MAX_PROBLEM_PODS).collect::<Vec<_>>(),
        });
        let mut set = |key: &str, value: Value| {
            if !value.is_null() {
                body[key] = value;
            }
        };
        set("readiness", json!(readiness));
        set("queue_position", queue_position);
        set("admitted_to", json!(admitted_to));
        if problems.len() > MAX_PROBLEM_PODS {
            set("problem_pods_total", json!(problems.len()));
        }
        set("retention_seconds_after_finish", json!(retention_seconds));
        match (&fit, detail) {
            (None, _) => {}
            (Some(Err(e)), _) => set("fit", json!(format!("unavailable: {e}"))),
            (Some(Ok(f)), Detail::Summary) => set(
                "fit",
                json!({"verdict": f.verdict, "cluster_queue": f.cluster_queue}),
            ),
            (Some(Ok(f)), Detail::Full) => set("fit", json!(f)),
        }
        let keep = match detail {
            Detail::Summary => SUMMARY_EVENTS,
            Detail::Full => FULL_EVENTS,
        };
        set(
            "events",
            match (events, detail) {
                (Err(e), _) => json!(format!("unavailable: {e}")),
                (Ok(ev), Detail::Summary) => json!(
                    ev[ev.len().saturating_sub(keep)..]
                        .iter()
                        .map(EventLine::line)
                        .collect::<Vec<_>>()
                ),
                (Ok(ev), Detail::Full) => json!(ev[ev.len().saturating_sub(keep)..]),
            },
        );
        match detail {
            Detail::Summary => set(
                "object_status",
                report::scalar_status(snap.object.data.get("status")),
            ),
            Detail::Full => {
                set("admission", json!(kind.admission()));
                set(
                    "object_status",
                    snap.object
                        .data
                        .get("status")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                set("workloads_total", json!(snap.workloads.len()));
                set(
                    "workloads",
                    json!(
                        snap.workloads
                            .iter()
                            .take(MAX_WORKLOADS_SHOWN)
                            .map(|w| json!({
                                "name": w.metadata.name,
                                "phase": status::phase(Some(w)).phase,
                                "queue": w.spec.queue_name,
                                "active": w.spec.active.unwrap_or(true),
                                "admission": w.status.admission,
                                "admission_checks": w.status.admission_checks,
                                "requeue_state": w.status.requeue_state,
                                "conditions": w.status.conditions,
                            }))
                            .collect::<Vec<_>>()
                    ),
                );
                set(
                    "pod_list",
                    json!(snap.pods.iter().map(pod_summary).collect::<Vec<_>>()),
                );
            }
        }
        body
    }

    /// workload_status: one report, or with `until` the report once the wait ends.
    async fn status(&self, c: &Caller, args: StatusArgs) -> Result<Value, ToolError> {
        let started = Instant::now();
        let timeout = Duration::from_secs(
            args.timeout_seconds
                .unwrap_or(MAX_WAIT_SECONDS)
                .clamp(1, MAX_WAIT_SECONDS),
        );
        let mut snap = self.snapshot(c, args.kind, &args.name).await?;
        let mut wait = Value::Null;
        if let Some(until) = args.until {
            let ended = loop {
                if report::reached(
                    until,
                    args.kind,
                    &snap.rollup,
                    snap.readiness(args.kind),
                    &snap.pods,
                ) {
                    break WaitEnd::Reached;
                }
                if report::settled(&snap.rollup) {
                    break WaitEnd::Settled;
                }
                if report::stuck(&report::problem_pods(args.kind, &snap.pods)) {
                    break WaitEnd::Stuck;
                }
                if started.elapsed() + WAIT_POLL > timeout {
                    break WaitEnd::Timeout;
                }
                tokio::time::sleep(WAIT_POLL).await;
                let left = timeout.saturating_sub(started.elapsed());
                match tokio::time::timeout(left, self.snapshot(c, args.kind, &args.name)).await {
                    Ok(next) => snap = next?,
                    Err(_) => break WaitEnd::Timeout,
                }
            };
            wait = json!({
                "until": until,
                "reached": ended == WaitEnd::Reached,
                "ended": ended,
                "waited_seconds": started.elapsed().as_secs(),
            });
        }
        let mut body = self
            .report(
                c,
                args.kind,
                &args.name,
                &snap,
                args.detail.unwrap_or_default(),
            )
            .await;
        if !wait.is_null() {
            body["wait"] = wait;
        }
        Ok(body)
    }

    /// Resolves a task id to its object in the caller's namespace. A malformed id is an unknown
    /// task; a well-formed one whose object is gone, or was recreated under the same name, says so.
    async fn task_object(
        &self,
        c: &Caller,
        id: &str,
    ) -> Result<(TaskRef, DynamicObject), McpError> {
        let t = TaskRef::parse(id)
            .ok_or_else(|| McpError::invalid_params(format!("unknown task: {id}"), None))?;
        let obj = match self.get_object(c, t.kind, &t.name).await {
            Ok(o) => o,
            Err(ToolError::NotFound { .. }) => {
                return Err(McpError::invalid_params(
                    task::gone(&t, self.config.lifetimes.ttl_seconds_after_finished),
                    None,
                ));
            }
            Err(e) => return Err(internal(e)),
        };
        if obj.metadata.uid.as_deref() != Some(t.uid.as_str()) {
            return Err(McpError::invalid_params(task::replaced(&t), None));
        }
        Ok((t, obj))
    }

    async fn annotate(
        &self,
        c: &Caller,
        kind: WorkloadKind,
        name: &str,
        key: &str,
        value: &str,
    ) -> Result<(), ToolError> {
        self.dynamic(kind, c.namespace.as_str())
            .patch(
                name,
                &PatchParams {
                    field_manager: Some(self.config.labels.manager.to_string()),
                    ..PatchParams::default()
                },
                &Patch::Merge(json!({"metadata": {"annotations": {key: value}}})),
            )
            .await
            .map_err(|source| {
                KubeError::api("annotate", format!("{} {name}", kind.kind()), source)
            })?;
        Ok(())
    }

    /// A batch workload's task state.
    async fn detailed_task(&self, c: &Caller, t: &TaskRef) -> Result<DetailedTask, ToolError> {
        let snap = self.snapshot(c, t.kind, &t.name).await?;
        let full = serde_json::to_value(&snap.object).map_err(|source| KubeError::Decode {
            what: format!("{} {}", t.kind.kind(), t.name),
            source,
        })?;
        let outcome = task::outcome(&full, &snap.rollup, &self.config.labels.task_cancelled);
        let created = snap
            .object
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|ts| ts.0.to_string())
            .unwrap_or_default();
        let updated = snap.last_updated().unwrap_or_else(|| created.clone());
        let base = |status: TaskStatus, message: &str| {
            Task::new(t.encode(), status, created.clone(), updated.clone())
                .with_poll_interval_ms(task::POLL_INTERVAL_MS)
                .with_status_message(message.to_string())
        };
        Ok(match outcome {
            Outcome::Working { message } => {
                DetailedTask::new(base(TaskStatus::Working, &message), TaskPayload::Working)
            }
            Outcome::Cancelled => DetailedTask::new(
                base(TaskStatus::Cancelled, "cancelled"),
                TaskPayload::Cancelled,
            ),
            Outcome::Completed { failed, message } => {
                let report = self
                    .report(c, t.kind, &t.name, &snap, Detail::Summary)
                    .await;
                let result = text_result(&task::completed_body(failed, &message, report), failed);
                let result = match serde_json::to_value(result) {
                    Ok(Value::Object(map)) => map,
                    Ok(_) | Err(_) => serde_json::Map::new(),
                };
                DetailedTask::new(
                    base(TaskStatus::Completed, &message),
                    TaskPayload::Completed { result },
                )
            }
        })
    }
}

#[tool_router]
impl KueueMcp {
    #[tool(
        description = "Who this token acts as: its run, launcher, Kubernetes user, and default namespace.",
        annotations(title = "Who am I", read_only_hint = true, open_world_hint = false)
    )]
    async fn whoami(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<NamespaceArgs>,
    ) -> CallToolResult {
        let requested = args.namespace.clone();
        reply(
            self.explain(&parts, requested.as_deref(), async {
                let id = identity(&parts)?;
                let namespace = match id.in_namespace(args.namespace.as_deref()) {
                    Ok(c) => Some(c.namespace.to_string()),
                    Err(Refusal::NamespaceRequired) => None,
                    Err(e) => return Err(e.into()),
                };
                let usable = match parts.extensions.get::<Principal>() {
                    Some(p) => self.namespaces.usable(p).await,
                    None => Vec::new(),
                };
                let mut queues = serde_json::Map::new();
                for ns in &usable {
                    let names: Vec<String> = kueue::list::<LocalQueue>(
                        &self.client,
                        &self.kueue.local_queues(),
                        Some(ns),
                        &ListParams::default(),
                    )
                    .await
                    .map(|qs| qs.into_iter().map(|q| q.metadata.name).collect())
                    .unwrap_or_default();
                    queues.insert(ns.clone(), json!(names));
                }
                Ok(json!({
                    "run_id": id.run_id.as_str(),
                    "namespace": namespace,
                    "namespaces": usable,
                    "queues": queues,
                    "launched_by": id.launched_by.as_str(),
                    "kube_user": id.kube_user,
                    "submittable_kinds": self.kinds.iter().map(|k| format!("{} {}", k.api_version(), k.kind())).collect::<Vec<_>>(),
                    "ownership": self.config.ownership,
                    "allowlist_profile": self.config.profile.name,
                }))
            }
            .await)
            .await,
        )
    }

    #[tool(
        description = "LocalQueues in the namespace, with their ClusterQueue and pending/admitted counts.",
        annotations(title = "List queues", read_only_hint = true, open_world_hint = false)
    )]
    async fn queues_list(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<NamespaceArgs>,
    ) -> CallToolResult {
        let requested = args.namespace.clone();
        reply(
            self.explain(
                &parts,
                requested.as_deref(),
                async {
                    let c = self
                        .served_caller(&parts, args.namespace.as_deref())
                        .await?;
                    let all: Vec<LocalQueue> = kueue::list(
                        &self.client,
                        &self.kueue.local_queues(),
                        Some(c.namespace.as_str()),
                        &ListParams::default(),
                    )
                    .await?;
                    let rows: Vec<Value> = all
                        .into_iter()
                        .map(|q| {
                            json!({
                                "queue": q.metadata.name,
                                "cluster_queue": q.spec.cluster_queue,
                                "stop_policy": q.spec.stop_policy,
                                "pending": q.status.pending_workloads,
                                "reserving": q.status.reserving_workloads,
                                "admitted": q.status.admitted_workloads,
                                "active": kueue::is_true(&q.status.conditions, "Active"),
                            })
                        })
                        .collect();
                    Ok(json!({"namespace": c.namespace.as_str(), "queues": rows}))
                }
                .await,
            )
            .await,
        )
    }

    #[tool(
        description = "Quota, usage, borrowing, preemption, and the pending line for one LocalQueue's ClusterQueue.",
        annotations(
            title = "Describe a queue",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn queue_describe(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<QueueArgs>,
    ) -> CallToolResult {
        let requested = args.namespace.clone();
        reply(
            self.explain(&parts, requested.as_deref(), async {
                let c = self.served_caller(&parts, args.namespace.as_deref()).await?;
                let lq = self.local_queue(&c, &args.queue).await?;
                let cq = self.cluster_queue(&lq).await?;
                let usage: BTreeMap<(String, String), (Value, Value)> = cq
                    .status
                    .flavors_usage
                    .iter()
                    .flat_map(|f| {
                        f.resources
                            .iter()
                            .map(|r| ((f.name.clone(), r.name.clone()), (r.total.clone(), r.borrowed.clone())))
                    })
                    .collect();
                let quotas: Vec<Value> = cq
                    .spec
                    .resource_groups
                    .iter()
                    .flat_map(|g| g.flavors.iter())
                    .flat_map(|f| {
                        let usage = &usage;
                        f.resources.iter().map(move |r| {
                            let (used, borrowed) = usage
                                .get(&(f.name.clone(), r.name.clone()))
                                .cloned()
                                .unwrap_or((json!("0"), json!("0")));
                            json!({
                                "flavor": f.name,
                                "resource": r.name,
                                "nominal": r.nominal_quota,
                                "borrowing_limit": r.borrowing_limit,
                                "lending_limit": r.lending_limit,
                                "used": used,
                                "borrowed": borrowed,
                            })
                        })
                    })
                    .collect();
                let pending = kueue::pending_in_local_queue(&self.client, self.kueue, c.namespace.as_str(), &lq.metadata.name, 50).await?;
                Ok(json!({
                    "queue": lq.metadata.name,
                    "cluster_queue": cq.metadata.name,
                    "cohort": cq.spec.cohort_name,
                    "queueing_strategy": cq.spec.queueing_strategy,
                    "stop_policy": cq.spec.stop_policy,
                    "preemption": cq.spec.preemption,
                    "fair_sharing": cq.status.fair_sharing,
                    "admission_checks": cq.spec.admission_checks_strategy,
                    "conditions": cq.status.conditions,
                    "counts": {
                        "cluster_queue": {"pending": cq.status.pending_workloads, "reserving": cq.status.reserving_workloads, "admitted": cq.status.admitted_workloads},
                        "local_queue": {"pending": lq.status.pending_workloads, "reserving": lq.status.reserving_workloads, "admitted": lq.status.admitted_workloads},
                    },
                    "quotas": quotas,
                    "pending_in_local_queue": pending,
                }))
            }
            .await)
            .await,
        )
    }

    #[tool(
        description = "Hardware behind a queue: each ResourceFlavor's node labels, taints, and topology joined with the GPU nodes it can land on.",
        annotations(
            title = "Describe a queue's hardware",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn hardware_describe(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<QueueArgs>,
    ) -> CallToolResult {
        let requested = args.namespace.clone();
        reply(
            self.explain(&parts, requested.as_deref(), async {
                let c = self.served_caller(&parts, args.namespace.as_deref()).await?;
                let lq = self.local_queue(&c, &args.queue).await?;
                let cq = self.cluster_queue(&lq).await?;
                let nodes: Vec<Node> = Api::<Node>::all(self.client.clone())
                    .list(&ListParams::default())
                    .await
                    .map_err(|source| KubeError::api("list", "nodes", source))?
                    .items;
                let mut flavors = Vec::new();
                for group in &cq.spec.resource_groups {
                    for f in &group.flavors {
                        let rf: ResourceFlavor = kueue::get(&self.client, &self.kueue.resource_flavors(), None, &f.name).await?;
                        flavors.push(hardware::describe(&rf, &group.covered_resources, &nodes, &self.config.hardware));
                    }
                }
                Ok(json!({"queue": lq.metadata.name, "cluster_queue": cq.metadata.name, "flavors": flavors}))
            }
            .await)
            .await,
        )
    }

    #[tool(
        description = "Validate a workload without creating it: pins it to the namespace and queue, runs a server-side dry run, and estimates whether the queue's quota can admit it (fit: fits_now, waits_for_quota, never_fits). Returns the plan_hash workload_submit requires, how long a finished Job or RayJob is kept (finished_retention_seconds), and server_changes: what the server added, such as the image pull secret an image's registry needs.",
        annotations(
            title = "Plan a workload",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn workload_plan(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<PlanArgs>,
    ) -> CallToolResult {
        let requested = args.namespace.clone();
        reply(
            self.explain(&parts, requested.as_deref(), async {
                let c = self.served_caller(&parts, args.namespace.as_deref()).await?;
                let actor = identity(&parts)?.actor.as_deref();
                let (n, cq) = self.plan(&c, &args.queue, args.manifest).await?;
                self.create(&c, &n, actor, true).await?;
                let totals: BTreeMap<String, String> =
                    n.total_requests().into_iter().map(|(k, v)| (k, v.to_string())).collect();
                let pod_sets: Vec<Value> = n
                    .pod_sets
                    .iter()
                    .map(|p| {
                        json!({
                            "name": p.name,
                            "count": p.count,
                            "per_pod": p.per_pod.iter().map(|(k, v)| (k.clone(), v.to_string())).collect::<BTreeMap<_, _>>(),
                        })
                    })
                    .collect();
                Ok(json!({
                    "kind": n.kind,
                    "name": n.name,
                    "plan_hash": n.plan_hash,
                    "finished_retention_seconds": n.retention_seconds(),
                    "server_changes": n.server_changes,
                    "dry_run": "accepted",
                    "admission": n.kind.admission(),
                    "pod_sets": pod_sets,
                    "total_requests": totals,
                    "fit": kueue::fit(&n.total_requests(), &cq),
                    "manifest": n.object,
                }))
            }
            .await)
            .await,
        )
    }

    #[tool(
        description = "Create a planned workload. The manifest and queue must be the ones passed to workload_plan and plan_hash must match. Resubmitting an identical plan is a no-op.",
        annotations(
            title = "Submit a workload",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workload_submit(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<SubmitArgs>,
    ) -> CallToolResponse {
        let wants_task = ctx
            .client_capabilities()
            .is_some_and(|c| c.supports_tasks());
        let parts = ctx.extensions.get::<http::request::Parts>();
        let requested = args.namespace.clone();
        let submitted: Result<(Value, Option<Task>), ToolError> = async {
                let parts = parts.ok_or(ToolError::NoCaller)?;
                let c = self.served_caller(parts, args.namespace.as_deref()).await?;
                let actor = identity(parts)?.actor.as_deref();
                let (n, _) = self.plan(&c, &args.queue, args.manifest).await?;
                if n.plan_hash != args.plan_hash {
                    return Err(ToolError::HashMismatch {
                        got: args.plan_hash,
                        want: n.plan_hash,
                    });
                }
                match self.create(&c, &n, actor, false).await {
                    Ok(obj) => {
                        tracing::info!(kind = n.kind.kind(), name = %n.name, namespace = %c.namespace, run = %c.run_id, "workload submitted");
                        let task = n.kind.is_batch().then(|| new_task(&c.namespace, n.kind, &n.name, &obj));
                        Ok((json!({"kind": n.kind, "name": n.name, "uid": obj.metadata.uid, "task_id": task.as_ref().map(|t| t.task_id.clone()), "finished_retention_seconds": n.retention_seconds(), "submitted": true}), task))
                    }
                    Err(ToolError::Rejected { .. }) if self.get_object(&c, n.kind, &n.name).await.is_ok() => {
                        let existing = self.owned_object(&c, n.kind, &n.name).await?;
                        let hash = existing
                            .metadata
                            .annotations
                            .as_ref()
                            .and_then(|a| a.get(&self.config.labels.plan_hash))
                            .cloned()
                            .unwrap_or_default();
                        if hash != n.plan_hash {
                            return Err(ToolError::Conflict {
                                kind: n.kind.kind(),
                                name: n.name,
                                existing: hash,
                            });
                        }
                        let task = n.kind.is_batch().then(|| new_task(&c.namespace, n.kind, &n.name, &existing));
                        Ok((json!({"kind": n.kind, "name": n.name, "uid": existing.metadata.uid, "task_id": task.as_ref().map(|t| t.task_id.clone()), "finished_retention_seconds": n.retention_seconds(), "submitted": false, "note": "already submitted with this plan"}), task))
                    }
                    Err(e) => Err(e),
                }
            }
            .await;
        match submitted {
            Ok((_, Some(task))) if wants_task => {
                CallToolResponse::Task(CreateTaskResult::new(task))
            }
            Ok((body, _)) => reply(Ok(body)).into(),
            Err(e) => match parts {
                Some(p) => reply(self.explain(p, requested.as_deref(), Err(e)).await).into(),
                None => reply(Err(e)).into(),
            },
        }
    }

    #[tool(
        description = "Where a workload is: Kueue phase and why it is not admitted, queue position and fit while pending, pod counts, the pods with problems and their exit messages, recent events, and next_action. With until, waits on the server (up to 50s) for admitted, running, ready or finished instead of polling.",
        annotations(
            title = "Workload status",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn workload_status(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<StatusArgs>,
    ) -> CallToolResult {
        let requested = args.namespace.clone();
        reply(
            self.explain(
                &parts,
                requested.as_deref(),
                async {
                    let c = self
                        .served_caller(&parts, args.namespace.as_deref())
                        .await?;
                    self.status(&c, args).await
                }
                .await,
            )
            .await,
        )
    }

    #[tool(
        description = "Workloads in the namespace with their Kueue phase: by default those this run created; filter=managed adds other runs', filter=all every Kueue-queued workload of a served kind, however it was created.",
        annotations(
            title = "List workloads",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn workload_list(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListArgs>,
    ) -> CallToolResult {
        let requested = args.namespace.clone();
        reply(
            self.explain(&parts, requested.as_deref(), async {
                let c = self.served_caller(&parts, args.namespace.as_deref()).await?;
                let ns = c.namespace.as_str();
                let labels = &self.config.labels;
                let selector = list_selector(args.filter.unwrap_or_default(), labels, &c);
                let workloads = self.namespace_workloads(ns).await?;
                let pods: Vec<Pod> = Api::<Pod>::namespaced(self.client.clone(), ns)
                    .list(&ListParams::default())
                    .await
                    .map_err(|source| KubeError::api("list", "pods", source))?
                    .items;
                let mut rows = Vec::new();
                for &kind in self.kinds.iter() {
                    let objs = match self.dynamic(kind, ns).list(&ListParams::default().labels(&selector)).await {
                        Ok(list) => list.items,
                        Err(kube::Error::Api(s)) if s.code == 404 => continue,
                        Err(source) => {
                            return Err(KubeError::api("list", kind.kind(), source)
                            .into());
                        }
                    };
                    for o in objs {
                        let name = o.metadata.name.clone().unwrap_or_default();
                        let selectors = serde_json::to_value(&o)
                            .map(|v| kinds::pod_selectors(kind, &v))
                            .unwrap_or_default();
                        let none = BTreeMap::new();
                        let its_pods: Vec<Pod> = pods
                            .iter()
                            .filter(|p| {
                                let l = p.metadata.labels.as_ref().unwrap_or(&none);
                                selectors.iter().any(|s| s.matches(l))
                            })
                            .cloned()
                            .collect();
                        let owners = owner_uids(&o, &its_pods);
                        let p = status::rollup(&owned_by(&workloads, &owners));
                        rows.push(json!({
                            "kind": kind,
                            "name": name,
                            "run": o.metadata.labels.as_ref().and_then(|l| l.get(&labels.run_id)),
                            "managed": o.metadata.labels.as_ref().and_then(|l| l.get(&labels.managed_by)).is_some_and(|v| v == labels.manager.as_str()),
                            "queue": o.metadata.labels.as_ref().and_then(|l| l.get(QUEUE_LABEL)),
                            "phase": p.headline.phase,
                            "counts": p.counts,
                            "reason": p.headline.reason,
                            "created": o.metadata.creation_timestamp.map(|t| t.0.to_string()),
                        }));
                    }
                }
                Ok(json!({"namespace": ns, "workloads": rows}))
            }
            .await)
            .await,
        )
    }

    #[tool(
        description = "Tail container logs of a workload's pods within a byte budget (16 KiB by default): failed pods first, then a RayJob's head and submitter or a LeaderWorkerSet's leader. Filter with grep and since_seconds.",
        annotations(
            title = "Workload logs",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn workload_logs(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<LogArgs>,
    ) -> CallToolResult {
        let requested = args.namespace.clone();
        reply(
            self.explain(
                &parts,
                requested.as_deref(),
                async {
                    let c = self
                        .served_caller(&parts, args.namespace.as_deref())
                        .await?;
                    let ns = c.namespace.as_str();
                    let obj = self.get_object(&c, args.kind, &args.name).await?;
                    let mut pods = self.pods_of(ns, args.kind, &obj).await?;
                    if let Some(want) = &args.pod {
                        pods.retain(|p| p.metadata.name.as_deref() == Some(want.as_str()));
                        if pods.is_empty() {
                            return Err(ToolError::NotFound {
                                kind: "Pod",
                                name: want.clone(),
                                namespace: ns.to_string(),
                            });
                        }
                    }
                    let total = pods.len();
                    pods.sort_by(|a, b| {
                        (report::log_priority(args.kind, a), report::pod_name(a))
                            .cmp(&(report::log_priority(args.kind, b), report::pod_name(b)))
                    });
                    pods.truncate(MAX_LOG_PODS);
                    let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
                    let grep = args.grep.as_deref().filter(|g| !g.is_empty());
                    let default_tail = if grep.is_some() {
                        MAX_TAIL
                    } else {
                        DEFAULT_TAIL
                    };
                    let budget = args
                        .max_bytes
                        .unwrap_or(DEFAULT_LOG_BUDGET)
                        .clamp(MIN_LOG_BUDGET, MAX_LOG_BYTES as usize);
                    let mut fetched = Vec::new();
                    for p in &pods {
                        let name = report::pod_name(p).to_string();
                        let container = args
                            .container
                            .clone()
                            .or_else(|| report::default_container(p));
                        let lp = LogParams {
                            container: container.clone(),
                            tail_lines: Some(
                                args.tail_lines.unwrap_or(default_tail).clamp(1, MAX_TAIL),
                            ),
                            since_seconds: args.since_seconds.filter(|s| *s > 0),
                            limit_bytes: Some(MAX_LOG_BYTES),
                            previous: args.previous,
                            ..LogParams::default()
                        };
                        let text = match api.logs(&name, &lp).await {
                            Ok(t) => Ok(match grep {
                                Some(g) => report::grep(&t, g),
                                None => t,
                            }),
                            Err(kube::Error::Api(s)) => Err(s.message),
                            Err(source) => {
                                return Err(KubeError::api("read logs of", name, source).into());
                            }
                        };
                        fetched.push((p, container, text));
                    }
                    let sizes: Vec<usize> = fetched
                        .iter()
                        .map(|(_, _, t)| t.as_ref().map_or(0, String::len))
                        .collect();
                    let grants = report::budget(&sizes, budget);
                    let mut omitted_total = 0;
                    let mut logs = Vec::new();
                    for ((p, container, text), grant) in fetched.iter().zip(grants) {
                        let mut entry = json!({
                            "pod": report::pod_name(p),
                            "role": report::role(args.kind, p),
                            "phase": report::pod_state(p),
                        });
                        if let Some(c) = container {
                            entry["container"] = json!(c);
                        }
                        match text {
                            Ok(t) => {
                                let (kept, omitted) = report::tail(t, grant);
                                entry["log"] = json!(kept);
                                if omitted > 0 {
                                    entry["bytes_omitted"] = json!(omitted);
                                    omitted_total += omitted;
                                }
                            }
                            Err(message) => {
                                entry["error"] = json!(format!("unavailable: {message}"))
                            }
                        }
                        logs.push(entry);
                    }
                    Ok(json!({
                        "pods_total": total,
                        "pods_shown": logs.len(),
                        "budget_bytes": budget,
                        "truncated": omitted_total > 0,
                        "bytes_omitted": omitted_total,
                        "logs": logs,
                    }))
                }
                .await,
            )
            .await,
        )
    }

    #[tool(
        description = "Set the replica count of a Deployment or StatefulSet this run submitted. New Deployment pods queue individually; a StatefulSet scales only to or from zero.",
        annotations(
            title = "Scale a workload",
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn workload_scale(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ScaleArgs>,
    ) -> CallToolResult {
        let requested = args.namespace.clone();
        reply(
            self.explain(&parts, requested.as_deref(), async {
                let c = self.served_caller(&parts, args.namespace.as_deref()).await?;
                if !args.kind.is_scalable() {
                    return Err(ToolError::NotScalable {
                        kind: args.kind.kind(),
                    });
                }
                if !(0..=MAX_REPLICAS).contains(&args.replicas) {
                    return Err(ToolError::Replicas {
                        got: args.replicas,
                        max: MAX_REPLICAS,
                    });
                }
                let r = WorkloadRef {
                    namespace: args.namespace,
                    kind: args.kind,
                    name: args.name,
                };
                let obj = self.owned_object(&c, r.kind, &r.name).await?;
                let from = replicas_of(&obj);
                self.set_replicas(&c, &r, Some(args.replicas), Some(None))
                    .await?;
                Ok(json!({"kind": r.kind, "name": r.name, "replicas": args.replicas, "from": from}))
            }
            .await)
            .await,
        )
    }

    #[tool(
        description = "Hold a submitted workload out of the queue until workload_resume. Jobs, RayJobs, and LeaderWorkerSets are deactivated in Kueue (evicted if admitted); Deployments and StatefulSets scale to zero and remember their replica count.",
        annotations(
            title = "Stop a workload",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workload_stop(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(r): Parameters<WorkloadRef>,
    ) -> CallToolResult {
        let requested = r.namespace.clone();
        reply(
            self.explain(
                &parts,
                requested.as_deref(),
                async {
                    let c = self.served_caller(&parts, r.namespace.as_deref()).await?;
                    self.set_active(&c, &r, false).await
                }
                .await,
            )
            .await,
        )
    }

    #[tool(
        description = "Return a stopped workload to its queue.",
        annotations(
            title = "Resume a workload",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workload_resume(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(r): Parameters<WorkloadRef>,
    ) -> CallToolResult {
        let requested = r.namespace.clone();
        reply(
            self.explain(
                &parts,
                requested.as_deref(),
                async {
                    let c = self.served_caller(&parts, r.namespace.as_deref()).await?;
                    self.set_active(&c, &r, true).await
                }
                .await,
            )
            .await,
        )
    }

    #[tool(
        description = "Delete a workload this run submitted, along with its pods.",
        annotations(
            title = "Delete a workload",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workload_delete(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(r): Parameters<WorkloadRef>,
    ) -> CallToolResult {
        let requested = r.namespace.clone();
        reply(
            self.explain(&parts, requested.as_deref(), async {
                let c = self.served_caller(&parts, r.namespace.as_deref()).await?;
                self.owned_object(&c, r.kind, &r.name).await?;
                self.dynamic(r.kind, c.namespace.as_str())
                    .delete(&r.name, &DeleteParams::background())
                    .await
                    .map_err(|source| KubeError::api("delete", format!("{} {}", r.kind.kind(), r.name), source))?;
                tracing::info!(kind = r.kind.kind(), name = %r.name, namespace = %c.namespace, "workload deleted");
                Ok(json!({"kind": r.kind, "name": r.name, "deleted": true}))
            }
            .await)
            .await,
        )
    }
}

fn duration(seconds: i64) -> String {
    match seconds {
        s if s > 0 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s > 0 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// The server instructions for this deployment's kinds, lifetimes, ownership and allowlist.
pub fn instructions(cfg: &Config, kinds: &[WorkloadKind]) -> String {
    let names: Vec<&str> = kinds.iter().map(|k| k.kind()).collect();
    let lists = &cfg.profile.lists;
    let lifetime = match (
        cfg.lifetimes.active_deadline_seconds,
        cfg.lifetimes.ttl_seconds_after_finished,
    ) {
        (None, None) => "A batch workload keeps its own activeDeadlineSeconds and ttlSecondsAfterFinished; plan and submit report its retention as finished_retention_seconds.".to_string(),
        (deadline, ttl) => {
            let runs = deadline
                .map(|d| format!("runs at most {} (activeDeadlineSeconds)", duration(d)))
                .unwrap_or_else(|| "runs until it ends".into());
            let kept = ttl
                .map(|t| format!(" and is deleted {} after it finishes (ttlSecondsAfterFinished), with its pods, logs and task,", duration(t)))
                .unwrap_or_default();
            format!("A batch workload {runs}{kept} unless its manifest sets its own; plan and submit report this as finished_retention_seconds. Read the outcome and logs before then.")
        }
    };
    let control = match cfg.ownership {
        Ownership::Managed => {
            "workload_stop, workload_resume, workload_scale and workload_delete act only on workloads this server created for the caller's run."
        }
        Ownership::Rbac => {
            "workload_stop, workload_resume, workload_scale and workload_delete act on any workload the caller's Kubernetes RBAC permits, including ones created outside this server; workload_list with filter=all lists them."
        }
    };
    let mut refused = vec![
        "host namespaces and paths",
        "privileged containers and added capabilities",
        "service accounts and their tokens",
        "nodeName",
    ];
    if !lists.volume_sources.contains("secret") && !lists.env_sources.contains("secretKeyRef") {
        refused.push("Secrets (volumes and env)");
    }
    if !lists.pod.contains("tolerations") {
        refused.push("tolerations");
    }
    refused.push(match lists.pod.contains("priorityClassName") {
        true => "scheduler and runtime class fields",
        false => "priority, scheduler and runtime class fields",
    });
    let mut domains: Vec<&str> = vec![cfg.labels.domain.as_str()];
    domains.extend(lists.reserved_domains.iter().map(String::as_str));
    format!(
        "Submit and watch Kueue workloads ({}) in a namespace: a run's token acts in its own, a person names one they may use. \
Flow: whoami (the namespaces you can use and their queues) -> queues_list -> hardware_describe/queue_describe -> workload_plan -> workload_submit -> workload_status with until=admitted|running|ready|finished, which waits on the server up to 50s per call (or, for a batch workload, tasks/get when the client speaks the tasks extension) -> workload_logs. \
Every tool answers JSON text with a status: ok; refused (the request is not allowed or not valid: fix it, do not retry it as is); rejected (the API server or an admission webhook refused it); not_found; error (the server could not complete it); failed (a finished task whose workload failed). Anything but ok comes back with isError and a retryable flag: retry a retryable call a few times with a pause, change a non-retryable one. \
workload_status answers a summary by default with next_action, the pods with problems and their exit messages; detail=full adds every Workload's conditions and every pod. Logs come within a 16 KiB budget, failed pods first; narrow them with grep and since_seconds. \
{lifetime} {control} \
Manifests are allowlisted (profile {}): {}, and labels or annotations reserved for {} are refused. \
Kueue chooses the node; pin hardware with a nodeSelector only when hardware_describe shows the flavor spans several GPU products.",
        names.join(", "),
        cfg.profile.name,
        refused.join(", "),
        domains.join(", "),
    )
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KueueMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tasks()
                .build(),
        )
        .with_instructions(instructions(&self.config, &self.kinds))
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        let t = parse_task(&request.task_id)?;
        let c = self.task_caller(&ctx, &t).await?;
        let (t, _) = self.task_object(&c, &request.task_id).await?;
        let detailed = self.detailed_task(&c, &t).await.map_err(internal)?;
        Ok(GetTaskResult::new(detailed))
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let t = parse_task(&request.task_id)?;
        let c = self.task_caller(&ctx, &t).await?;
        let (t, _) = self.task_object(&c, &request.task_id).await?;
        let r = WorkloadRef {
            namespace: t.namespace.clone(),
            kind: t.kind,
            name: t.name.clone(),
        };
        self.owned_object(&c, t.kind, &t.name)
            .await
            .map_err(refused)?;
        let current = self.detailed_task(&c, &t).await.map_err(internal)?;
        if current.status().is_terminal() {
            return Ok(());
        }
        self.annotate(
            &c,
            t.kind,
            &t.name,
            &self.config.labels.task_cancelled,
            "true",
        )
        .await
        .map_err(internal)?;
        self.set_active(&c, &r, false).await.map_err(internal)?;
        tracing::info!(task = %request.task_id, namespace = %c.namespace, "task cancelled");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::access::KueueRules;
    use crate::server::*;
    use crucible_mcp_auth::access::{AccessRules, Call};
    use crucible_mcp_auth::principal::{Callers, Principal};

    #[test]
    fn the_server_stamps_the_plan_hash_and_a_runs_actor() {
        let object = || -> DynamicObject {
            serde_json::from_value(json!({
                "apiVersion": "batch/v1", "kind": "Job",
                "metadata": {"name": "j", "annotations": {"keep": "x"}},
            }))
            .unwrap()
        };
        let labels = Labels::new("crucible.dev");
        let mut run = object();
        stamp(
            &mut run,
            &labels,
            "h1",
            Some("spiffe://example.crucible/s1"),
        );
        assert_eq!(
            run.metadata.annotations.unwrap(),
            BTreeMap::from([
                ("keep".to_string(), "x".to_string()),
                ("crucible.dev/plan-hash".to_string(), "h1".to_string()),
                (
                    "crucible.dev/actor".to_string(),
                    "spiffe://example.crucible/s1".to_string()
                ),
            ])
        );
        let mut person = object();
        stamp(&mut person, &labels, "h1", None);
        assert!(
            !person
                .metadata
                .annotations
                .unwrap()
                .contains_key("crucible.dev/actor")
        );
    }

    /// A reply's `isError` and its JSON text.
    fn body(r: &CallToolResult) -> (bool, Value) {
        let text = r.content[0].as_text().unwrap().text.clone();
        (
            r.is_error.unwrap_or(false),
            serde_json::from_str(&text).unwrap(),
        )
    }

    #[test]
    fn replies_carry_a_status_tag_and_every_failure_is_a_tool_error() {
        let (is_error, ok) = body(&reply(Ok(json!({"x": 1}))));
        assert!(!is_error);
        assert_eq!(ok["status"], "ok");
        assert!(ok.get("retryable").is_none());
        let (is_error, refused) = body(&reply(Err(ToolError::HashMismatch {
            got: "a".into(),
            want: "b".into(),
        })));
        assert!(is_error);
        assert_eq!(refused["status"], "refused");
        assert_eq!(refused["retryable"], false);
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("re-run workload_plan")
        );
        let (is_error, missing) = body(&reply(Err(ToolError::NotFound {
            kind: "LocalQueue",
            name: "q".into(),
            namespace: "n".into(),
        })));
        assert!(is_error);
        assert_eq!(missing["status"], "not_found");
        assert_eq!(missing["error"], "LocalQueue q not found in namespace n");
    }

    fn rejected(code: u16, message: &str) -> ToolError {
        ToolError::Rejected {
            kind: "StatefulSet",
            code,
            message: message.into(),
        }
    }

    fn kube(code: u16) -> ToolError {
        ToolError::Kube(KubeError::api(
            "get",
            "Job j",
            kube::Error::Api(
                kube::core::Status::failure("m", "r")
                    .with_code(code)
                    .boxed(),
            ),
        ))
    }

    fn server_in(served: &[&str]) -> KueueMcp {
        crate::install_tls_provider();
        let client =
            kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().unwrap()))
                .unwrap();
        let namespaces = Namespaces::new(
            client.clone(),
            KueueVersion::default(),
            served.iter().map(|n| n.to_string()).collect(),
            None,
        );
        KueueMcp::new(
            client,
            KueueVersion::default(),
            Arc::new(Config::default()),
            Arc::new(WorkloadKind::ALL.to_vec()),
            Arc::new(namespaces),
        )
    }

    fn parts_of(claims: Value) -> http::request::Parts {
        let (mut parts, _) = http::Request::new(()).into_parts();
        let Value::Object(m) = claims else {
            unreachable!()
        };
        parts.extensions.insert(
            Principal::from_claims(m, crucible_mcp_auth::principal::Callers::People).unwrap(),
        );
        parts
    }

    const SA_403: &str = "localqueues.kueue.x-k8s.io is forbidden: User \"system:serviceaccount:crucible-system:crucible-kueue-people\" cannot list resource \"localqueues\" in API group \"kueue.x-k8s.io\" in the namespace \"crucible-e2e\"";

    #[tokio::test]
    async fn the_servers_own_refusal_names_the_usable_namespaces_and_never_its_account() {
        let server = server_in(&["crucible-kueue-e2e"]);
        let parts = parts_of(json!({"sub": "w", "kube_user": "casey"}));
        let forbidden = ToolError::Kube(KubeError::api(
            "list",
            "LocalQueue list",
            kube::Error::Api(
                kube::core::Status::failure(SA_403, "Forbidden")
                    .with_code(403)
                    .boxed(),
            ),
        ));
        let (is_error, out) = body(&reply(
            server
                .explain(&parts, Some("crucible-e2e"), Err(forbidden))
                .await,
        ));
        assert!(is_error);
        assert_eq!(out["status"], "not_found", "{out}");
        assert_eq!(
            out["error"],
            "namespace crucible-e2e does not exist or this server is not installed there; namespaces you can use: crucible-kueue-e2e"
        );
        assert!(!out.to_string().contains("system:serviceaccount"), "{out}");

        let create = rejected(403, SA_403);
        let (_, out) = body(&reply(
            server
                .explain(&parts, Some("crucible-e2e"), Err(create))
                .await,
        ));
        assert_eq!(out["status"], "not_found", "{out}");
        assert!(!out.to_string().contains("system:serviceaccount"), "{out}");

        let webhook = rejected(
            403,
            "admission webhook \"vstatefulset.kb.io\" denied the request",
        );
        let (_, out) = body(&reply(
            server
                .explain(&parts, Some("crucible-kueue-e2e"), Err(webhook))
                .await,
        ));
        assert_eq!(
            out["status"], "rejected",
            "a webhook's refusal is the caller's to read: {out}"
        );
    }

    #[tokio::test]
    async fn a_missing_queue_outside_the_callers_namespaces_points_at_them() {
        let server = server_in(&["ml-a", "ml-b"]);
        let parts = parts_of(json!({"sub": "w", "kube_user": "casey"}));
        let missing = |namespace: &str| ToolError::NotFound {
            kind: "LocalQueue",
            name: "gpu".into(),
            namespace: namespace.into(),
        };
        let (_, outside) = body(&reply(
            server
                .explain(&parts, Some("ml-c"), Err(missing("ml-c")))
                .await,
        ));
        assert_eq!(
            outside["error"],
            "namespace ml-c does not exist or this server is not installed there; namespaces you can use: ml-a, ml-b"
        );
        let (_, inside) = body(&reply(
            server
                .explain(&parts, Some("ml-a"), Err(missing("ml-a")))
                .await,
        ));
        assert_eq!(
            inside["error"],
            "LocalQueue gpu not found in namespace ml-a"
        );
    }

    #[tokio::test]
    async fn a_token_without_a_namespace_is_told_which_it_can_name() {
        let server = server_in(&["ml-a", "ml-b"]);
        let parts = parts_of(json!({"sub": "w", "kube_user": "casey"}));
        let (_, out) = body(&reply(
            server
                .explain(
                    &parts,
                    None,
                    Err(ToolError::Refused(Refusal::NamespaceRequired)),
                )
                .await,
        ));
        assert_eq!(out["status"], "refused");
        assert_eq!(
            out["error"],
            "name a namespace: this token has no default one; namespaces you can use: ml-a, ml-b"
        );
    }

    #[test]
    fn webhook_rejections_and_server_errors_are_retryable() {
        let webhook = rejected(
            403,
            "admission webhook \"vstatefulset.kb.io\" denied the request: spec.replicas: Forbidden",
        );
        let (_, b) = body(&reply(Err(webhook)));
        assert_eq!(
            (b["status"].as_str(), b["retryable"].as_bool()),
            (Some("rejected"), Some(true))
        );
        assert!(rejected(409, "the object has been modified").retryable());
        assert!(!rejected(422, "spec.template: Invalid value").retryable());
        let (_, b) = body(&reply(Err(kube(503))));
        assert_eq!(
            (b["status"].as_str(), b["retryable"].as_bool()),
            (Some("error"), Some(true))
        );
        assert!(!kube(403).retryable());
        assert_eq!(kube(404).status(), "not_found");
        assert!(!ToolError::NoCaller.retryable());
    }

    #[test]
    fn a_missing_job_or_rayjob_points_at_its_ttl() {
        let missing = |kind| ToolError::NotFound {
            kind,
            name: "train".into(),
            namespace: "team-a".into(),
        };
        for kind in ["Job", "RayJob", "JobSet", "PyTorchJob"] {
            let msg = missing(kind).to_string();
            assert!(
                msg.contains("is deleted ttlSecondsAfterFinished after it finishes"),
                "{msg}"
            );
        }
        assert_eq!(
            missing("Deployment").to_string(),
            "Deployment train not found in namespace team-a"
        );
    }

    #[test]
    fn an_event_reads_as_one_line() {
        let e = EventLine {
            time: "2026-10-23T10:00:00Z".into(),
            object: "Pod/train-0".into(),
            type_: Some("Warning".into()),
            reason: Some("BackOff".into()),
            message: Some("Back-off restarting failed container\n".into()),
            count: Some(4),
        };
        assert_eq!(
            e.line(),
            "2026-10-23T10:00:00Z Pod/train-0 Warning BackOff: Back-off restarting failed container (x4)"
        );
    }

    fn obj(uid: &str, replicas: Option<i64>) -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "d", "uid": uid},
            "spec": replicas.map_or(json!({}), |n| json!({"replicas": n})),
        }))
        .unwrap()
    }

    fn pod(uid: &str) -> Pod {
        serde_json::from_value(json!({"metadata": {"name": uid, "uid": uid}})).unwrap()
    }

    fn workload(name: &str, owner: &str) -> Workload {
        serde_json::from_value(json!({
            "metadata": {"name": name, "ownerReferences": [{"apiVersion": "v1", "kind": "Pod", "name": owner, "uid": owner}]}
        }))
        .unwrap()
    }

    #[test]
    fn workloads_owned_by_the_object_or_its_pods_belong_to_it() {
        let all = vec![
            workload("direct", "obj-uid"),
            workload("pod-a", "pod-a"),
            workload("pod-b", "pod-b"),
            workload("other", "someone-else"),
        ];
        let owners = owner_uids(&obj("obj-uid", None), &[pod("pod-a"), pod("pod-b")]);
        let names: Vec<&str> = owned_by(&all, &owners)
            .iter()
            .map(|w| w.metadata.name.as_str())
            .collect();
        assert_eq!(names, vec!["direct", "pod-a", "pod-b"]);
    }

    #[test]
    fn replicas_default_to_one_like_the_apps_api() {
        assert_eq!(replicas_of(&obj("u", None)), 1);
        assert_eq!(replicas_of(&obj("u", Some(0))), 0);
        assert_eq!(replicas_of(&obj("u", Some(4))), 4);
    }

    #[test]
    fn a_request_without_an_authenticated_caller_is_refused() {
        let (parts, _) = http::Request::new(()).into_parts();
        assert!(matches!(caller(&parts, None), Err(ToolError::NoCaller)));
    }

    #[test]
    fn every_tool_has_a_scope_and_the_shipped_policy_injects_nothing_into_schemas() {
        let policy = crucible_mcp_auth::policy::ToolPolicy::from_toml(include_str!(
            "../deploy/kueue-policy.toml"
        ))
        .unwrap();
        for tool in KueueMcp::tool_router().list_all() {
            let args = sample_args(&tool.name);
            let call = Call::Tool {
                name: &tool.name,
                args: &args,
            };
            assert_eq!(
                KueueRules::default().scopes(&call).map(<[_]>::len),
                Ok(1),
                "{} needs exactly one kueue scope",
                tool.name
            );
            let mut schema = (*tool.input_schema).clone();
            policy.hide_injected(&tool.name, &mut schema);
            assert_eq!(
                schema, *tool.input_schema,
                "{}: its injected paths sit inside free-form fields",
                tool.name
            );
        }
    }

    fn sample_args(tool: &str) -> serde_json::Map<String, Value> {
        let job = json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "j"}});
        let args = match tool {
            "whoami" | "queues_list" | "workload_list" => json!({"namespace": "team-b"}),
            "queue_describe" | "hardware_describe" => {
                json!({"namespace": "team-b", "queue": "gpu"})
            }
            "workload_plan" => json!({"namespace": "team-b", "queue": "gpu", "manifest": job}),
            "workload_submit" => {
                json!({"namespace": "team-b", "queue": "gpu", "manifest": job, "plan_hash": "h"})
            }
            "workload_scale" => {
                json!({"namespace": "team-b", "kind": "Deployment", "name": "d", "replicas": 2})
            }
            _ => json!({"namespace": "team-b", "kind": "Job", "name": "j"}),
        };
        match args {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    #[test]
    fn every_persons_call_but_whoami_is_checked_in_the_named_namespace() {
        let person = match json!({"sub": "p", "kube_user": "alice", "scope": "kueue.read kueue.submit"})
        {
            Value::Object(m) => Principal::from_claims(m, Callers::People).unwrap(),
            _ => unreachable!(),
        };
        for tool in KueueMcp::tool_router().list_all() {
            let args = sample_args(&tool.name);
            let call = Call::Tool {
                name: &tool.name,
                args: &args,
            };
            let checks = KueueRules::default().checks(&call, &person).unwrap();
            if tool.name == "whoami" {
                assert!(checks.is_empty());
                continue;
            }
            assert!(
                !checks.is_empty(),
                "{} is not checked for people",
                tool.name
            );
            assert!(
                checks.iter().all(|c| c.namespace == "team-b"),
                "{}: {checks:?}",
                tool.name
            );
        }
    }

    #[test]
    fn every_tool_is_annotated_for_what_it_changes() {
        let read_only = [
            "whoami",
            "queues_list",
            "queue_describe",
            "hardware_describe",
            "workload_status",
            "workload_list",
            "workload_logs",
            "workload_plan",
        ];
        let destructive = ["workload_delete", "workload_stop", "workload_scale"];
        let idempotent = [
            "workload_submit",
            "workload_stop",
            "workload_resume",
            "workload_delete",
        ];
        let tools = KueueMcp::tool_router().list_all();
        assert_eq!(tools.len(), 13);
        for tool in tools {
            let name = tool.name.as_ref();
            let a = tool
                .annotations
                .clone()
                .unwrap_or_else(|| panic!("{name} has no annotations"));
            assert!(
                a.title.as_deref().is_some_and(|t| !t.is_empty()),
                "{name} has no title"
            );
            assert_eq!(a.open_world_hint, Some(false), "{name}");
            assert_eq!(
                a.read_only_hint,
                Some(read_only.contains(&name)),
                "{name} readOnlyHint"
            );
            if read_only.contains(&name) {
                continue;
            }
            assert_eq!(
                a.destructive_hint,
                Some(destructive.contains(&name)),
                "{name} destructiveHint"
            );
            assert_eq!(
                a.idempotent_hint.unwrap_or(false),
                idempotent.contains(&name),
                "{name} idempotentHint"
            );
        }
    }

    fn property(tool: &str, field: &str) -> Value {
        let tool = KueueMcp::tool_router()
            .list_all()
            .into_iter()
            .find(|t| t.name == tool)
            .unwrap();
        Value::Object((*tool.input_schema).clone())["properties"][field].clone()
    }

    #[test]
    fn bounded_arguments_advertise_their_bounds() {
        let bounds = |tool, field| {
            let p = property(tool, field);
            (p["minimum"].as_i64(), p["maximum"].as_i64())
        };
        assert_eq!(bounds("workload_logs", "tail_lines"), (Some(1), Some(2000)));
        assert_eq!(
            bounds("workload_logs", "max_bytes"),
            (Some(1024), Some(65536))
        );
        assert_eq!(bounds("workload_scale", "replicas"), (Some(0), Some(1000)));
        assert_eq!(
            bounds("workload_status", "timeout_seconds"),
            (Some(1), Some(50))
        );
    }

    #[test]
    fn every_argument_is_described() {
        for tool in KueueMcp::tool_router().list_all() {
            let schema = Value::Object((*tool.input_schema).clone());
            let Some(props) = schema["properties"].as_object() else {
                continue;
            };
            for (field, p) in props {
                let described = p["description"].as_str().is_some_and(|d| !d.is_empty())
                    || p["$ref"].is_string()
                    || p["allOf"].is_array()
                    || p["anyOf"].is_array();
                assert!(described, "{}.{field} has no description: {p}", tool.name);
            }
        }
    }

    #[test]
    fn the_plan_names_every_submittable_kind() {
        let doc = property("workload_plan", "manifest")["description"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for kind in WorkloadKind::ALL {
            assert!(
                doc.contains(&format!("{} {}", kind.api_version(), kind.kind())),
                "{doc}"
            );
        }
        let queue = property("workload_plan", "queue")["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!queue.contains("this run's namespace"), "{queue}");
    }

    #[test]
    fn the_instructions_cover_status_retries_lifetimes_and_refused_fields() {
        let crucible = instructions(&Config::default(), &WorkloadKind::ALL);
        for want in [
            "until=admitted|running|ready|finished",
            "refused",
            "rejected",
            "retryable",
            "runs at most 24h",
            "deleted 1h after it finishes",
            "finished_retention_seconds",
            "Secrets",
            "privileged",
            "tolerations",
            "priority, scheduler",
            "only on workloads this server created",
            "profile strict",
            "crucible.dev, kueue.x-k8s.io",
            "PyTorchJob",
        ] {
            assert!(
                crucible.contains(want),
                "instructions lack {want:?}: {crucible}"
            );
        }
    }

    #[test]
    fn a_tenant_deployments_instructions_follow_its_config() {
        let cfg = Config {
            labels: Labels::new("kueue-mcp.example.com"),
            ownership: Ownership::Rbac,
            profile: crate::profile::Profile::tenant(),
            lifetimes: crate::config::Lifetimes {
                active_deadline_seconds: None,
                ttl_seconds_after_finished: Some(600),
                suspend: true,
            },
            ..Config::default()
        };
        let text = instructions(&cfg, &[WorkloadKind::Job, WorkloadKind::Deployment]);
        assert!(text.contains("(Job, Deployment)"), "{text}");
        assert!(
            text.contains("runs until it ends and is deleted 10m after"),
            "{text}"
        );
        assert!(
            text.contains("any workload the caller's Kubernetes RBAC permits"),
            "{text}"
        );
        assert!(text.contains("profile tenant"), "{text}");
        assert!(
            text.contains("reserved for kueue-mcp.example.com are refused"),
            "{text}"
        );
        for absent in ["Secrets", "tolerations", "priority,", "crucible.dev"] {
            assert!(!text.contains(absent), "{absent}: {text}");
        }
        let none = instructions(
            &Config {
                lifetimes: crate::config::Lifetimes {
                    active_deadline_seconds: None,
                    ttl_seconds_after_finished: None,
                    suspend: true,
                },
                ..Config::default()
            },
            &[WorkloadKind::Job],
        );
        assert!(
            none.contains("keeps its own activeDeadlineSeconds"),
            "{none}"
        );
    }

    #[test]
    fn list_filters_select_this_run_this_server_or_everything_queued() {
        let l = Labels::new("crucible.dev");
        let r1 = Caller {
            owner: crate::caller::OwnerKind::Run,
            run_id: crate::caller::RunId::parse("r1").unwrap(),
            namespace: Namespace::parse("team-a").unwrap(),
            launched_by: crate::caller::Launcher::parse("alice").unwrap(),
            kube_user: "alice".into(),
        };
        assert_eq!(
            list_selector(ListFilter::Mine, &l, &r1),
            "crucible.dev/managed-by=crucible-kueue,crucible.dev/run-id=r1,crucible.dev/owner-kind=run"
        );
        assert_eq!(
            list_selector(ListFilter::Managed, &l, &r1),
            "crucible.dev/managed-by=crucible-kueue"
        );
        assert_eq!(
            list_selector(ListFilter::All, &l, &r1),
            "kueue.x-k8s.io/queue-name"
        );
        let other = Labels::new("crucible.dev")
            .managed_as(crate::caller::Manager::parse("team-b-kueue").unwrap());
        assert_eq!(
            list_selector(ListFilter::Managed, &other, &r1),
            "crucible.dev/managed-by=team-b-kueue"
        );
        let theirs: DynamicObject = serde_json::from_value(json!({"apiVersion": "batch/v1",
            "kind": "Job", "metadata": {"name": "j", "labels": {
                "crucible.dev/managed-by": "crucible-kueue", "crucible.dev/run-id": "r1",
                "crucible.dev/owner-kind": "run"}}}))
        .unwrap();
        assert!(created_by(&r1, &theirs, &l));
        assert!(
            !created_by(&r1, &theirs, &other),
            "another manager's object is not this server's"
        );
    }

    #[test]
    fn only_rbac_ownership_lets_a_caller_control_what_it_did_not_create() {
        let caller = Caller {
            owner: crate::caller::OwnerKind::Run,
            run_id: crate::caller::RunId::parse("r1").unwrap(),
            namespace: Namespace::parse("team-a").unwrap(),
            launched_by: crate::caller::Launcher::parse("alice").unwrap(),
            kube_user: "alice".into(),
        };
        let with = |labels: Value| -> DynamicObject {
            serde_json::from_value(json!({"apiVersion": "batch/v1", "kind": "Job",
                "metadata": {"name": "j", "labels": labels}}))
            .unwrap()
        };
        let mine = with(json!({"crucible.dev/managed-by": "crucible-kueue",
            "crucible.dev/run-id": "r1", "crucible.dev/owner-kind": "run"}));
        let other_run = with(json!({"crucible.dev/managed-by": "crucible-kueue",
            "crucible.dev/run-id": "r2", "crucible.dev/owner-kind": "run"}));
        let person_named_r1 = with(json!({"crucible.dev/managed-by": "crucible-kueue",
            "crucible.dev/run-id": "r1", "crucible.dev/owner-kind": "person"}));
        let kubectl = with(json!({}));
        let managed = Config::default();
        assert!(controls(&caller, &mine, &managed));
        assert!(!controls(&caller, &other_run, &managed));
        assert!(
            !controls(&caller, &person_named_r1, &managed),
            "a person whose subject reads r1 owns nothing of run r1"
        );
        assert!(!controls(&caller, &kubectl, &managed));
        let rbac = Config {
            ownership: Ownership::Rbac,
            ..Config::default()
        };
        assert!(controls(&caller, &kubectl, &rbac));
        assert!(controls(&caller, &other_run, &rbac));
        let elsewhere = Config {
            labels: Labels::new("kueue-mcp.example.com"),
            ..Config::default()
        };
        assert!(
            !controls(&caller, &mine, &elsewhere),
            "labels follow the domain"
        );
    }

    #[test]
    fn every_tool_is_listed() {
        let names: Vec<String> = KueueMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for want in [
            "whoami",
            "queues_list",
            "queue_describe",
            "hardware_describe",
            "workload_plan",
            "workload_submit",
            "workload_status",
            "workload_list",
            "workload_logs",
            "workload_stop",
            "workload_resume",
            "workload_scale",
            "workload_delete",
        ] {
            assert!(
                names.iter().any(|n| n == want),
                "missing {want} in {names:?}"
            );
        }
    }
}
