//! The MCP tool surface. Every tool resolves the [`Caller`] the auth layer attached to the HTTP
//! request and acts only inside that caller's namespace; mutations additionally require the
//! submitter role and, past submit, an object this server created for the same run.

use crate::hardware;
use crate::kueue::{
    self, ClusterQueue, KubeError, KueueVersion, LocalQueue, ResourceFlavor, Workload,
};
use crate::manifest::{
    self, MANAGED_LABEL, MANAGED_VALUE, ManifestError, Normalized, RUN_LABEL, WORKLOAD_LABEL,
    WorkloadKind,
};
use crate::status::{self, Rollup};
use crate::task::{self, Outcome, TaskRef};
use crate::tenant::{Caller, Identity, NameError, Namespace, QueueName, Refusal};
use crucible_contract::kueue::TenantRole;
use crucible_contract::kueue::{SCOPE_READ, SCOPE_SUBMIT};
use crucible_mcp_auth::guard::{AccessReview, MethodScopes};
use crucible_mcp_auth::kube::USER_CLAIM;
use crucible_mcp_auth::policy::AccessCheck;
use crucible_mcp_auth::principal::Principal;
use k8s_openapi::api::core::v1::{Event, Node, Pod};
use kube::api::{
    Api, DeleteParams, DynamicObject, ListParams, LogParams, Patch, PatchParams, PostParams,
};
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

pub const FIELD_MANAGER: &str = "crucible-kueue";
pub const PLAN_HASH_ANNOTATION: &str = "crucible.dev/plan-hash";
pub const STOPPED_REPLICAS_ANNOTATION: &str = "crucible.dev/stopped-replicas";
/// The sandbox that presented the run's token when the workload was planned and submitted.
pub const ACTOR_ANNOTATION: &str = "crucible.dev/actor";
/// The tasks extension's methods and the scopes each needs. The guard admits no other method.
pub const METHODS: &[MethodScopes] = &[
    MethodScopes {
        method: "tasks/get",
        scopes: &[SCOPE_READ],
    },
    MethodScopes {
        method: "tasks/cancel",
        scopes: &[SCOPE_SUBMIT],
    },
];
const MAX_WORKLOADS_SHOWN: usize = 20;
const MAX_REPLICAS: i64 = 1000;
const MAX_LOG_PODS: usize = 4;
const DEFAULT_TAIL: i64 = 200;
const MAX_TAIL: i64 = 2000;
const MAX_LOG_BYTES: i64 = 64 * 1024;

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
    #[error("{kind} {name} not found in namespace {namespace}")]
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
    Rejected { kind: &'static str, message: String },
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
            Self::NotFound { .. } => "not_found",
            Self::Kube(e) if e.is_not_found() => "not_found",
            Self::Kube(_) => "error",
        }
    }
}

fn reply(result: Result<Value, ToolError>) -> String {
    match result {
        Ok(Value::Object(mut map)) => {
            map.entry("status").or_insert_with(|| json!("ok"));
            Value::Object(map).to_string()
        }
        Ok(other) => json!({"status": "ok", "result": other}).to_string(),
        Err(e) => {
            tracing::info!(error = %e, status = e.status(), "tool call refused or failed");
            json!({"status": e.status(), "error": e.to_string()}).to_string()
        }
    }
}

struct StatusReport {
    body: Value,
    object: DynamicObject,
    rollup: Rollup,
    last_updated: Option<String>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskAccess {
    Read,
    Cancel,
}

impl TaskAccess {
    fn method(self) -> &'static str {
        match self {
            Self::Read => "tasks/get",
            Self::Cancel => "tasks/cancel",
        }
    }
}

/// The Kubernetes access a task request needs on the task's object: `get` to read it, `update` to
/// cancel it.
fn task_check(t: &TaskRef, c: &Caller, access: TaskAccess) -> AccessCheck {
    let (verb, rule, message) = match access {
        TaskAccess::Read => ("get", "task-get", "you may not read this workload"),
        TaskAccess::Cancel => ("update", "task-cancel", "you may not cancel this workload"),
    };
    let group = t
        .kind
        .api_version()
        .rsplit_once('/')
        .map_or("", |(group, _)| group);
    AccessCheck {
        tool: access.method().into(),
        rule: rule.into(),
        message: message.into(),
        group: group.into(),
        resource: t.kind.plural().into(),
        subresource: String::new(),
        verb: verb.into(),
        namespace: c.namespace.to_string(),
        name: t.name.clone(),
    }
}

fn refused(error: impl std::fmt::Display) -> McpError {
    McpError::invalid_request(error.to_string(), None)
}

fn request_parts(ctx: &RequestContext<RoleServer>) -> Result<&http::request::Parts, ToolError> {
    ctx.extensions
        .get::<http::request::Parts>()
        .ok_or(ToolError::NoCaller)
}

fn principal(parts: &http::request::Parts) -> Result<&Principal, ToolError> {
    parts
        .extensions
        .get::<Principal>()
        .ok_or(ToolError::NoCaller)
}

/// The annotations only the server writes: the plan hash and, for a run, the sandbox that acted.
fn stamp(object: &mut DynamicObject, plan_hash: &str, actor: Option<&str>) {
    let annotations = object
        .metadata
        .annotations
        .get_or_insert_with(BTreeMap::new);
    annotations.insert(PLAN_HASH_ANNOTATION.into(), plan_hash.into());
    if let Some(actor) = actor {
        annotations.insert(ACTOR_ANNOTATION.into(), actor.into());
    }
}

/// Whether `caller` created `obj`: crucible-kueue's managed label and the caller's run label.
fn owns(caller: &Caller, obj: &DynamicObject) -> bool {
    let labels = obj.metadata.labels.as_ref();
    let label = |key: &str| labels.and_then(|l| l.get(key)).map(String::as_str);
    label(MANAGED_LABEL) == Some(MANAGED_VALUE) && label(RUN_LABEL) == Some(caller.run_id.as_str())
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
    /// LocalQueue name in this run's namespace.
    pub queue: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlanArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    /// LocalQueue name in this run's namespace.
    pub queue: String,
    /// The workload as a JSON object: batch/v1 Job, ray.io/v1 RayJob, or leaderworkerset.x-k8s.io/v1
    /// LeaderWorkerSet. metadata.name is required; namespace and queue are filled in.
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
    pub kind: WorkloadKind,
    /// metadata.name of the submitted object.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LogArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    pub kind: WorkloadKind,
    pub name: String,
    /// One pod's name; omit for up to four of the workload's pods.
    #[serde(default)]
    pub pod: Option<String>,
    #[serde(default)]
    pub container: Option<String>,
    /// Lines from the end of each log (default 200, max 2000).
    #[serde(default)]
    pub tail_lines: Option<i64>,
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
    pub name: String,
    /// Target replica count. Kueue admits a StatefulSet only when scaling to or from zero.
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
    /// Include workloads other runs created in this namespace.
    #[serde(default)]
    pub all_runs: bool,
}

#[derive(Clone)]
pub struct KueueMcp {
    client: kube::Client,
    /// Decides task requests from tokens carrying a Kubernetes user, as the tool policy's
    /// authorize rules decide tool calls.
    reviewer: Option<Arc<dyn AccessReview>>,
    kueue: KueueVersion,
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
        reviewer: Option<Arc<dyn AccessReview>>,
        kueue: KueueVersion,
    ) -> Self {
        Self {
            client,
            reviewer,
            kueue,
            tool_router: Self::tool_router(),
        }
    }

    /// The caller a task request acts as, once its token, its role and Kubernetes RBAC on the
    /// task's object allow `access`. Cancelling also needs the submit scope.
    async fn task_caller(
        &self,
        ctx: &RequestContext<RoleServer>,
        t: &TaskRef,
        access: TaskAccess,
    ) -> Result<(Caller, bool), McpError> {
        let parts = request_parts(ctx).map_err(refused)?;
        let identity = identity(parts).map_err(refused)?;
        let principal = principal(parts).map_err(refused)?;
        let c = identity
            .in_namespace(t.namespace.as_deref())
            .map_err(refused)?;
        if access == TaskAccess::Cancel && c.role != TenantRole::Submitter {
            return Err(refused("tasks/cancel needs the submitter role"));
        }
        let may_submit = c.role == TenantRole::Submitter && principal.scopes.contains(SCOPE_SUBMIT);
        let reviewed = principal.claims.contains_key(USER_CLAIM);
        if reviewed || !identity.pinned {
            let Some(reviewer) = &self.reviewer else {
                return Err(refused(format!(
                    "{} needs an access reviewer this server does not have",
                    access.method()
                )));
            };
            let check = task_check(t, &c, access);
            match reviewer.review(principal, &check).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::info!(sub = %principal.subject, rule = %check.rule, namespace = %check.namespace, name = %check.name, "task request denied");
                    return Err(refused(format!(
                        "{} refused by {}: {}",
                        check.tool, check.rule, check.message
                    )));
                }
                Err(error) => {
                    tracing::warn!(sub = %principal.subject, rule = %check.rule, %error, "access review failed");
                    return Err(refused(format!(
                        "{} refused by {}: the access review failed",
                        check.tool, check.rule
                    )));
                }
            }
        }
        Ok((c, may_submit))
    }

    fn dynamic(&self, kind: WorkloadKind, ns: &str) -> Api<DynamicObject> {
        Api::namespaced_with(self.client.clone(), ns, &kind.api_resource())
    }

    async fn local_queue(&self, caller: &Caller, queue: &str) -> Result<LocalQueue, ToolError> {
        let queue = QueueName::parse(queue)?;
        caller.authorize_queue(&queue)?;
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
        if !owns(caller, &obj) {
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

    async fn pods_of(&self, ns: &str, name: &str) -> Result<Vec<Pod>, ToolError> {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let lp = ListParams::default().labels(&format!("{WORKLOAD_LABEL}={name}"));
        Ok(api
            .list(&lp)
            .await
            .map_err(|source| KubeError::api("list", format!("pods of {name}"), source))?
            .items)
    }

    async fn events_for(&self, ns: &str, names: &[&str]) -> Result<Vec<Value>, ToolError> {
        let api: Api<Event> = Api::namespaced(self.client.clone(), ns);
        let mut out: Vec<(String, Value)> = Vec::new();
        for name in names {
            let lp = ListParams::default().fields(&format!("involvedObject.name={name}"));
            let events = api
                .list(&lp)
                .await
                .map_err(|source| KubeError::api("list", format!("events of {name}"), source))?;
            for e in events.items {
                let when = e
                    .last_timestamp
                    .as_ref()
                    .map(|t| t.0.to_string())
                    .or_else(|| e.event_time.as_ref().map(|t| t.0.to_string()))
                    .unwrap_or_default();
                out.push((
                    when.clone(),
                    json!({
                        "time": when,
                        "object": format!("{}/{}", e.involved_object.kind.clone().unwrap_or_default(), name),
                        "type": e.type_,
                        "reason": e.reason,
                        "message": e.message,
                        "count": e.count,
                    }),
                ));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        let skip = out.len().saturating_sub(15);
        Ok(out.into_iter().skip(skip).map(|(_, v)| v).collect())
    }

    async fn plan(
        &self,
        caller: &Caller,
        queue: &str,
        manifest: Value,
    ) -> Result<(Normalized, ClusterQueue), ToolError> {
        let lq = self.local_queue(caller, queue).await?;
        let queue = QueueName::parse(queue)?;
        let normalized = manifest::normalize(manifest, caller, &queue)?;
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
        stamp(&mut object, &n.plan_hash, actor);
        let pp = PostParams {
            dry_run,
            field_manager: Some(FIELD_MANAGER.into()),
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
            patch["metadata"] = json!({"annotations": {STOPPED_REPLICAS_ANNOTATION: value}});
        }
        self.dynamic(r.kind, caller.namespace.as_str())
            .patch(
                &r.name,
                &PatchParams {
                    field_manager: Some(FIELD_MANAGER.into()),
                    ..PatchParams::default()
                },
                &Patch::Merge(patch),
            )
            .await
            .map_err(|e| match e {
                kube::Error::Api(s) if (400..500).contains(&s.code) && s.code != 404 => {
                    ToolError::Rejected {
                        kind: r.kind.kind(),
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
            .and_then(|a| a.get(STOPPED_REPLICAS_ANNOTATION))
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
        caller.require_submitter(if active {
            "workload_resume"
        } else {
            "workload_stop"
        })?;
        let obj = self.owned_object(caller, r.kind, &r.name).await?;
        if r.kind.is_scalable() {
            return self.scale_active(caller, r, &obj, active).await;
        }
        let ns = caller.namespace.as_str();
        let pods = self.pods_of(ns, &r.name).await?;
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

    async fn status_report(
        &self,
        c: &Caller,
        kind: WorkloadKind,
        name: &str,
    ) -> Result<StatusReport, ToolError> {
        let ns = c.namespace.as_str();
        let obj = self.get_object(c, kind, name).await?;
        let pods = self.pods_of(ns, name).await?;
        let wls = self.workloads_of(ns, &obj, &pods).await?;
        let refs: Vec<&Workload> = wls.iter().collect();
        let rollup = status::rollup(&refs);
        let pending: Vec<&Workload> = wls
            .iter()
            .filter(|w| status::phase(Some(w)).phase == status::Phase::Pending)
            .collect();
        let mut positions = Vec::new();
        if let Some(first) = pending.first() {
            let line = kueue::pending_in_local_queue(
                &self.client,
                self.kueue,
                ns,
                &first.spec.queue_name,
                1000,
            )
            .await?;
            for w in &pending {
                if let Some(p) = line.iter().find(|p| p.metadata.name == w.metadata.name) {
                    positions.push(json!({"workload": w.metadata.name, "in_local_queue": p.position_in_local_queue, "in_cluster_queue": p.position_in_cluster_queue}));
                }
            }
        }
        let mut names = vec![name];
        names.extend(wls.iter().take(3).map(|w| w.metadata.name.as_str()));
        let events = self.events_for(ns, &names).await?;
        let last_updated = wls
            .iter()
            .flat_map(|w| w.status.conditions.iter())
            .filter_map(|c| c.last_transition_time.clone())
            .max();
        let body = json!({
            "kind": kind,
            "name": name,
            "admission": kind.admission(),
            "phase": rollup,
            "queue_position": positions,
            "workloads": wls.iter().take(MAX_WORKLOADS_SHOWN).map(|w| json!({
                "name": w.metadata.name,
                "phase": status::phase(Some(w)).phase,
                "queue": w.spec.queue_name,
                "active": w.spec.active.unwrap_or(true),
                "admission": w.status.admission,
                "admission_checks": w.status.admission_checks,
                "requeue_state": w.status.requeue_state,
                "conditions": w.status.conditions,
            })).collect::<Vec<_>>(),
            "workloads_total": wls.len(),
            "object_status": obj.data.get("status"),
            "pods": pods.iter().map(pod_summary).collect::<Vec<_>>(),
            "events": events,
        });
        Ok(StatusReport {
            body,
            object: obj,
            rollup,
            last_updated,
        })
    }

    /// Resolves a task id to its object in the caller's namespace. A malformed id, a missing
    /// object, or a UID mismatch (the name was reused) are all an unknown task.
    async fn task_object(
        &self,
        c: &Caller,
        id: &str,
    ) -> Result<(TaskRef, DynamicObject), McpError> {
        let unknown = || McpError::invalid_params(format!("unknown task: {id}"), None);
        let t = TaskRef::parse(id).ok_or_else(unknown)?;
        let obj = match self.get_object(c, t.kind, &t.name).await {
            Ok(o) => o,
            Err(ToolError::NotFound { .. }) => return Err(unknown()),
            Err(e) => return Err(internal(e)),
        };
        if obj.metadata.uid.as_deref() != Some(t.uid.as_str()) {
            return Err(unknown());
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
                    field_manager: Some(FIELD_MANAGER.into()),
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

    /// The task's state. Only a `writable` request records a completion it observes.
    async fn detailed_task(
        &self,
        c: &Caller,
        t: &TaskRef,
        writable: bool,
    ) -> Result<DetailedTask, ToolError> {
        let report = self.status_report(c, t.kind, &t.name).await?;
        let full = serde_json::to_value(&report.object).map_err(|source| KubeError::Decode {
            what: format!("{} {}", t.kind.kind(), t.name),
            source,
        })?;
        let mut outcome = task::outcome(t.kind, &full, &report.rollup);
        if let Outcome::Completed {
            failed: false,
            message,
        } = &outcome
            && writable
            && t.kind.is_scalable()
            && report
                .object
                .metadata
                .annotations
                .as_ref()
                .is_none_or(|a| !a.contains_key(task::COMPLETED_ANNOTATION))
        {
            self.annotate(c, t.kind, &t.name, task::COMPLETED_ANNOTATION, message)
                .await?;
            outcome = Outcome::Completed {
                failed: false,
                message: message.clone(),
            };
        }
        let created = report
            .object
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|ts| ts.0.to_string())
            .unwrap_or_default();
        let updated = report
            .last_updated
            .clone()
            .unwrap_or_else(|| created.clone());
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
                let text = json!({"status": "ok", "outcome": if failed { "failed" } else { "succeeded" }, "message": message, "report": report.body});
                let result = if failed {
                    CallToolResult::error(vec![ContentBlock::text(text.to_string())])
                } else {
                    CallToolResult::success(vec![ContentBlock::text(text.to_string())])
                };
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
    #[tool(description = "The namespace, role, and queue allowlist this token is bound to.")]
    async fn whoami(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<NamespaceArgs>,
    ) -> String {
        reply(caller(&parts, args.namespace.as_deref()).map(|c| {
            json!({
                "run_id": c.run_id.as_str(),
                "namespace": c.namespace.as_str(),
                "role": c.role,
                "queues": c.queues.iter().map(QueueName::as_str).collect::<Vec<_>>(),
                "launched_by": c.launched_by.as_ref().map(|l| l.as_str()),
                "submittable_kinds": WorkloadKind::ALL.iter().map(|k| format!("{} {}", k.api_version(), k.kind())).collect::<Vec<_>>(),
            })
        }))
    }

    #[tool(
        description = "LocalQueues this run may use, with their ClusterQueue and pending/admitted counts."
    )]
    async fn queues_list(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<NamespaceArgs>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, args.namespace.as_deref())?;
                let all: Vec<LocalQueue> = kueue::list(
                    &self.client,
                    &self.kueue.local_queues(),
                    Some(c.namespace.as_str()),
                    &ListParams::default(),
                )
                .await?;
                let rows: Vec<Value> = all
                    .into_iter()
                    .filter(|q| {
                        QueueName::parse(&q.metadata.name)
                            .is_ok_and(|n| c.authorize_queue(&n).is_ok())
                    })
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
    }

    #[tool(
        description = "Quota, usage, borrowing, preemption, and the pending line for one LocalQueue's ClusterQueue."
    )]
    async fn queue_describe(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<QueueArgs>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, args.namespace.as_deref())?;
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
            .await,
        )
    }

    #[tool(
        description = "Hardware behind a queue: each ResourceFlavor's node labels, taints, and topology joined with the GPU nodes it can land on."
    )]
    async fn hardware_describe(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<QueueArgs>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, args.namespace.as_deref())?;
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
                        flavors.push(hardware::describe(&rf, &group.covered_resources, &nodes));
                    }
                }
                Ok(json!({"queue": lq.metadata.name, "cluster_queue": cq.metadata.name, "flavors": flavors}))
            }
            .await,
        )
    }

    #[tool(
        description = "Validate a workload without creating it: pins it to this run's namespace and queue, runs a server-side dry run, and estimates whether the queue's quota can admit it. Returns the plan_hash workload_submit requires."
    )]
    async fn workload_plan(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<PlanArgs>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, args.namespace.as_deref())?;
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
                    "dry_run": "accepted",
                    "admission": n.kind.admission(),
                    "pod_sets": pod_sets,
                    "total_requests": totals,
                    "fit": kueue::fit(&n.total_requests(), &cq),
                    "manifest": n.object,
                }))
            }
            .await,
        )
    }

    #[tool(
        description = "Create a planned workload. The manifest and queue must be the ones passed to workload_plan and plan_hash must match. Resubmitting an identical plan is a no-op."
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
        let submitted: Result<(Value, Task), ToolError> = async {
                let parts = parts.ok_or(ToolError::NoCaller)?;
                let c = caller(parts, args.namespace.as_deref())?;
                c.require_submitter("workload_submit")?;
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
                        let task = new_task(&c.namespace, n.kind, &n.name, &obj);
                        Ok((json!({"kind": n.kind, "name": n.name, "uid": obj.metadata.uid, "task_id": task.task_id, "submitted": true}), task))
                    }
                    Err(ToolError::Rejected { .. }) if self.get_object(&c, n.kind, &n.name).await.is_ok() => {
                        let existing = self.owned_object(&c, n.kind, &n.name).await?;
                        let hash = existing
                            .metadata
                            .annotations
                            .as_ref()
                            .and_then(|a| a.get(PLAN_HASH_ANNOTATION))
                            .cloned()
                            .unwrap_or_default();
                        if hash != n.plan_hash {
                            return Err(ToolError::Conflict {
                                kind: n.kind.kind(),
                                name: n.name,
                                existing: hash,
                            });
                        }
                        let task = new_task(&c.namespace, n.kind, &n.name, &existing);
                        Ok((json!({"kind": n.kind, "name": n.name, "uid": existing.metadata.uid, "task_id": task.task_id, "submitted": false, "note": "already submitted with this plan"}), task))
                    }
                    Err(e) => Err(e),
                }
            }
            .await;
        match submitted {
            Ok((_, task)) if wants_task => CallToolResponse::Task(CreateTaskResult::new(task)),
            Ok((body, _)) => {
                CallToolResult::success(vec![ContentBlock::text(reply(Ok(body)))]).into()
            }
            Err(e) => CallToolResult::success(vec![ContentBlock::text(reply(Err(e)))]).into(),
        }
    }

    #[tool(
        description = "Where a workload is: Kueue phase and the reason it is not admitted, queue position, flavor assignment, the object's own status, its pods, and recent events."
    )]
    async fn workload_status(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(r): Parameters<WorkloadRef>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, r.namespace.as_deref())?;
                Ok(self.status_report(&c, r.kind, &r.name).await?.body)
            }
            .await,
        )
    }

    #[tool(
        description = "Workloads crucible-kueue created in this namespace, with their Kueue phase."
    )]
    async fn workload_list(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListArgs>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, args.namespace.as_deref())?;
                let ns = c.namespace.as_str();
                let mut selector = format!("{MANAGED_LABEL}={MANAGED_VALUE}");
                if !args.all_runs {
                    selector.push_str(&format!(",{RUN_LABEL}={}", c.run_id));
                }
                let workloads = self.namespace_workloads(ns).await?;
                let pods: Vec<Pod> = Api::<Pod>::namespaced(self.client.clone(), ns)
                    .list(&ListParams::default().labels(WORKLOAD_LABEL))
                    .await
                    .map_err(|source| KubeError::api("list", "pods", source))?
                    .items;
                let mut rows = Vec::new();
                for kind in WorkloadKind::ALL {
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
                        let its_pods: Vec<Pod> = pods
                            .iter()
                            .filter(|p| p.metadata.labels.as_ref().and_then(|l| l.get(WORKLOAD_LABEL)) == Some(&name))
                            .cloned()
                            .collect();
                        let owners = owner_uids(&o, &its_pods);
                        let p = status::rollup(&owned_by(&workloads, &owners));
                        rows.push(json!({
                            "kind": kind,
                            "name": name,
                            "run": o.metadata.labels.as_ref().and_then(|l| l.get(RUN_LABEL)),
                            "queue": o.metadata.labels.as_ref().and_then(|l| l.get(manifest::QUEUE_LABEL)),
                            "phase": p.headline.phase,
                            "counts": p.counts,
                            "reason": p.headline.reason,
                            "created": o.metadata.creation_timestamp.map(|t| t.0.to_string()),
                        }));
                    }
                }
                Ok(json!({"namespace": ns, "workloads": rows}))
            }
            .await,
        )
    }

    #[tool(description = "Tail container logs of a workload's pods.")]
    async fn workload_logs(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<LogArgs>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, args.namespace.as_deref())?;
                let ns = c.namespace.as_str();
                self.get_object(&c, args.kind, &args.name).await?;
                let mut pods = self.pods_of(ns, &args.name).await?;
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
                let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
                let lp = LogParams {
                    container: args.container.clone(),
                    tail_lines: Some(args.tail_lines.unwrap_or(DEFAULT_TAIL).clamp(1, MAX_TAIL)),
                    limit_bytes: Some(MAX_LOG_BYTES),
                    previous: args.previous,
                    ..LogParams::default()
                };
                let mut logs = Vec::new();
                for p in pods.iter().take(MAX_LOG_PODS) {
                    let name = p.metadata.name.clone().unwrap_or_default();
                    let text = match api.logs(&name, &lp).await {
                        Ok(t) => json!(t),
                        Err(kube::Error::Api(s)) => json!(format!("unavailable: {}", s.message)),
                        Err(source) => {
                            return Err(KubeError::api("read logs of", name, source).into());
                        }
                    };
                    logs.push(json!({"pod": name, "log": text}));
                }
                Ok(json!({"pods_total": total, "logs": logs}))
            }
            .await,
        )
    }

    #[tool(
        description = "Set the replica count of a Deployment or StatefulSet this run submitted. New Deployment pods queue individually; a StatefulSet scales only to or from zero."
    )]
    async fn workload_scale(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ScaleArgs>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, args.namespace.as_deref())?;
                c.require_submitter("workload_scale")?;
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
            .await,
        )
    }

    #[tool(
        description = "Hold a submitted workload out of the queue until workload_resume. Jobs, RayJobs, and LeaderWorkerSets are deactivated in Kueue (evicted if admitted); Deployments and StatefulSets scale to zero and remember their replica count."
    )]
    async fn workload_stop(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(r): Parameters<WorkloadRef>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, r.namespace.as_deref())?;
                self.set_active(&c, &r, false).await
            }
            .await,
        )
    }

    #[tool(description = "Return a stopped workload to its queue.")]
    async fn workload_resume(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(r): Parameters<WorkloadRef>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, r.namespace.as_deref())?;
                self.set_active(&c, &r, true).await
            }
            .await,
        )
    }

    #[tool(description = "Delete a workload this run submitted, along with its pods.")]
    async fn workload_delete(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(r): Parameters<WorkloadRef>,
    ) -> String {
        reply(
            async {
                let c = caller(&parts, r.namespace.as_deref())?;
                c.require_submitter("workload_delete")?;
                self.owned_object(&c, r.kind, &r.name).await?;
                self.dynamic(r.kind, c.namespace.as_str())
                    .delete(&r.name, &DeleteParams::background())
                    .await
                    .map_err(|source| KubeError::api("delete", format!("{} {}", r.kind.kind(), r.name), source))?;
                tracing::info!(kind = r.kind.kind(), name = %r.name, namespace = %c.namespace, "workload deleted");
                Ok(json!({"kind": r.kind, "name": r.name, "deleted": true}))
            }
            .await,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KueueMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_tasks().build()).with_instructions(
            "Submit and watch Kueue workloads in this run's namespace. Flow: queues_list -> hardware_describe/queue_describe -> workload_plan -> workload_submit -> poll workload_status (or tasks/get when the client speaks the tasks extension) -> workload_logs. Kueue chooses the node; pin hardware with a nodeSelector only when hardware_describe shows the flavor spans several GPU products.",
        )
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        let t = parse_task(&request.task_id)?;
        let (c, may_submit) = self.task_caller(&ctx, &t, TaskAccess::Read).await?;
        let (t, obj) = self.task_object(&c, &request.task_id).await?;
        let writable = may_submit && owns(&c, &obj);
        let detailed = self
            .detailed_task(&c, &t, writable)
            .await
            .map_err(internal)?;
        Ok(GetTaskResult::new(detailed))
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let t = parse_task(&request.task_id)?;
        let (c, _) = self.task_caller(&ctx, &t, TaskAccess::Cancel).await?;
        let (t, _) = self.task_object(&c, &request.task_id).await?;
        let r = WorkloadRef {
            namespace: t.namespace.clone(),
            kind: t.kind,
            name: t.name.clone(),
        };
        self.owned_object(&c, t.kind, &t.name)
            .await
            .map_err(refused)?;
        let current = self.detailed_task(&c, &t, true).await.map_err(internal)?;
        if current.status().is_terminal() {
            return Ok(());
        }
        self.annotate(&c, t.kind, &t.name, task::CANCELLED_ANNOTATION, "true")
            .await
            .map_err(internal)?;
        self.set_active(&c, &r, false).await.map_err(internal)?;
        tracing::info!(task = %request.task_id, namespace = %c.namespace, "task cancelled");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::server::*;

    #[test]
    fn the_server_stamps_the_plan_hash_and_a_runs_actor() {
        let object = || -> DynamicObject {
            serde_json::from_value(json!({
                "apiVersion": "batch/v1", "kind": "Job",
                "metadata": {"name": "j", "annotations": {"keep": "x"}},
            }))
            .unwrap()
        };
        let mut run = object();
        stamp(&mut run, "h1", Some("spiffe://example.crucible/s1"));
        assert_eq!(
            run.metadata.annotations.unwrap(),
            BTreeMap::from([
                ("keep".to_string(), "x".to_string()),
                (PLAN_HASH_ANNOTATION.to_string(), "h1".to_string()),
                (
                    ACTOR_ANNOTATION.to_string(),
                    "spiffe://example.crucible/s1".to_string()
                ),
            ])
        );
        let mut person = object();
        stamp(&mut person, "h1", None);
        assert!(
            !person
                .metadata
                .annotations
                .unwrap()
                .contains_key(ACTOR_ANNOTATION)
        );
    }

    #[test]
    fn task_checks_name_the_tasks_object_and_the_verb_its_method_needs() {
        let caller = Identity::pinned(Caller {
            run_id: crate::tenant::RunId::parse("r").unwrap(),
            namespace: Namespace::parse("team-a").unwrap(),
            role: TenantRole::Submitter,
            queues: vec![],
            launched_by: None,
        })
        .in_namespace(None)
        .unwrap();
        let shown = |c: AccessCheck| {
            (
                c.tool,
                c.rule,
                c.group,
                c.resource,
                c.verb,
                c.namespace,
                c.name,
            )
        };
        let job = TaskRef::parse("team-a/Job/train/u1").unwrap();
        assert_eq!(
            shown(task_check(&job, &caller, TaskAccess::Read)),
            (
                "tasks/get".into(),
                "task-get".into(),
                "batch".into(),
                "jobs".into(),
                "get".into(),
                "team-a".into(),
                "train".into()
            )
        );
        let set = TaskRef::parse("team-a/LeaderWorkerSet/serve/u2").unwrap();
        assert_eq!(
            shown(task_check(&set, &caller, TaskAccess::Cancel)),
            (
                "tasks/cancel".into(),
                "task-cancel".into(),
                "leaderworkerset.x-k8s.io".into(),
                "leaderworkersets".into(),
                "update".into(),
                "team-a".into(),
                "serve".into()
            )
        );
    }

    #[test]
    fn replies_carry_a_status_tag_and_policy_failures_are_refusals() {
        let ok: Value = serde_json::from_str(&reply(Ok(json!({"x": 1})))).unwrap();
        assert_eq!(ok["status"], "ok");
        let refused: Value = serde_json::from_str(&reply(Err(ToolError::HashMismatch {
            got: "a".into(),
            want: "b".into(),
        })))
        .unwrap();
        assert_eq!(refused["status"], "refused");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("re-run workload_plan")
        );
        let missing: Value = serde_json::from_str(&reply(Err(ToolError::NotFound {
            kind: "LocalQueue",
            name: "q".into(),
            namespace: "n".into(),
        })))
        .unwrap();
        assert_eq!(missing["status"], "not_found");
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
    fn the_shipped_tool_policy_covers_every_tool_and_injects_nothing_into_schemas() {
        let policy = crucible_mcp_auth::policy::ToolPolicy::from_toml(include_str!(
            "../deploy/kueue-policy.toml"
        ))
        .unwrap();
        for tool in KueueMcp::tool_router().list_all() {
            let scopes = policy.scopes(&tool.name).unwrap();
            assert_eq!(
                scopes.len(),
                1,
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
    fn the_shipped_policy_reviews_every_persons_call_but_whoami() {
        let policy = crucible_mcp_auth::policy::ToolPolicy::from_toml(include_str!(
            "../deploy/kueue-policy.toml"
        ))
        .unwrap();
        let person = match json!({"sub": "p", "kube_user": "alice", "scope": "kueue.read kueue.submit"})
        {
            Value::Object(m) => m,
            _ => unreachable!(),
        };
        let scopes = ["kueue.read", "kueue.submit"].map(String::from).into();
        for tool in KueueMcp::tool_router().list_all() {
            let mut args = sample_args(&tool.name);
            let checks = policy
                .apply(&tool.name, &mut args, &person, &scopes)
                .unwrap();
            if tool.name == "whoami" {
                assert!(checks.is_empty());
                continue;
            }
            assert!(
                !checks.is_empty(),
                "{} is not reviewed for people",
                tool.name
            );
            assert!(
                checks.iter().all(|c| c.namespace == "team-b"),
                "{}: {checks:?}",
                tool.name
            );
        }
        let mut plan = sample_args("workload_plan");
        let checks = policy
            .apply("workload_plan", &mut plan, &person, &scopes)
            .unwrap();
        let summary: Vec<_> = checks
            .iter()
            .map(|c| {
                (
                    c.verb.as_str(),
                    c.group.as_str(),
                    c.resource.as_str(),
                    c.name.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("use", "kueue.x-k8s.io", "localqueues", "gpu"),
                ("create", "batch", "jobs", "")
            ]
        );
    }

    #[test]
    fn the_shipped_policy_leaves_tenant_tokens_to_their_record() {
        let policy = crucible_mcp_auth::policy::ToolPolicy::from_toml(include_str!(
            "../deploy/kueue-policy.toml"
        ))
        .unwrap();
        let tenant = match json!({"sub": "tenant:r", "namespace": "team-b", "queues": [], "scope": "kueue.read kueue.submit"})
        {
            Value::Object(m) => m,
            _ => unreachable!(),
        };
        let scopes = ["kueue.read", "kueue.submit"].map(String::from).into();
        for tool in KueueMcp::tool_router().list_all() {
            let mut args = sample_args(&tool.name);
            let checks = policy
                .apply(&tool.name, &mut args, &tenant, &scopes)
                .unwrap();
            assert!(checks.is_empty(), "{}", tool.name);
        }
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
