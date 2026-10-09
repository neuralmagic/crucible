//! Admission of agent-supplied workload manifests. A manifest is accepted only for an allowlisted
//! kind, is pinned to the caller's namespace and queue, and every pod template may set only an
//! allowlisted set of fields: anything that grants host access, privilege, another identity, a
//! secret, or a scheduling escape from Kueue is refused by not being on the list.

use crate::caller::{Caller, QueueName};
use kube::core::ApiResource;
use kube_quantity::ParsedQuantity;
use rmcp::schemars;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const QUEUE_LABEL: &str = "kueue.x-k8s.io/queue-name";
pub const MANAGED_LABEL: &str = "crucible.dev/managed-by";
pub const MANAGED_VALUE: &str = "crucible-kueue";
pub const RUN_LABEL: &str = "crucible.dev/run-id";
/// The human a delegated token acted for. Server-owned: set from the caller, never the manifest.
pub const LAUNCHED_BY_LABEL: &str = "crucible.dev/launched-by";
pub const WORKLOAD_LABEL: &str = "crucible.dev/workload";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
pub enum WorkloadKind {
    Job,
    RayJob,
    LeaderWorkerSet,
    Deployment,
    StatefulSet,
}

/// How Kueue admits the pods of a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    /// One Workload for the whole object; every pod is admitted together or not at all.
    AllOrNothing,
    /// One Workload per pod; the object can run partially admitted and scale freely.
    PerPod,
}

impl WorkloadKind {
    pub const ALL: [WorkloadKind; 5] = [
        Self::Job,
        Self::RayJob,
        Self::LeaderWorkerSet,
        Self::Deployment,
        Self::StatefulSet,
    ];

    pub fn admission(self) -> Admission {
        match self {
            Self::Deployment => Admission::PerPod,
            _ => Admission::AllOrNothing,
        }
    }

    /// Kinds whose stop and resume scale `spec.replicas` to zero and back, since Kueue gives
    /// them no single Workload to deactivate.
    /// Runs to completion (Job, RayJob), so a submission is an MCP task. The other kinds serve
    /// until stopped and report readiness instead.
    pub fn is_batch(self) -> bool {
        matches!(self, Self::Job | Self::RayJob)
    }

    pub fn is_scalable(self) -> bool {
        matches!(self, Self::Deployment | Self::StatefulSet)
    }

    pub fn api_version(self) -> &'static str {
        match self {
            Self::Job => "batch/v1",
            Self::RayJob => "ray.io/v1",
            Self::LeaderWorkerSet => "leaderworkerset.x-k8s.io/v1",
            Self::Deployment | Self::StatefulSet => "apps/v1",
        }
    }

    pub fn kind(self) -> &'static str {
        match self {
            Self::Job => "Job",
            Self::RayJob => "RayJob",
            Self::LeaderWorkerSet => "LeaderWorkerSet",
            Self::Deployment => "Deployment",
            Self::StatefulSet => "StatefulSet",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Self::Job => "jobs",
            Self::RayJob => "rayjobs",
            Self::LeaderWorkerSet => "leaderworkersets",
            Self::Deployment => "deployments",
            Self::StatefulSet => "statefulsets",
        }
    }

    pub fn api_resource(self) -> ApiResource {
        let (group, version) = match self.api_version().split_once('/') {
            Some((g, v)) => (g, v),
            None => ("", self.api_version()),
        };
        ApiResource {
            group: group.to_string(),
            version: version.to_string(),
            api_version: self.api_version().to_string(),
            kind: self.kind().to_string(),
            plural: self.plural().to_string(),
        }
    }

    fn from_type_meta(api_version: &str, kind: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.api_version() == api_version && k.kind() == kind)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest must be a JSON object")]
    NotObject,
    #[error(
        "{api_version}/{kind} is not submittable; allowed: batch/v1 Job, ray.io/v1 RayJob, leaderworkerset.x-k8s.io/v1 LeaderWorkerSet, apps/v1 Deployment, apps/v1 StatefulSet"
    )]
    Kind { api_version: String, kind: String },
    #[error("metadata.name is required and must be a DNS-1123 label, got {0:?}")]
    Name(String),
    #[error("metadata.generateName is not supported; set metadata.name so submit is idempotent")]
    GenerateName,
    #[error("metadata.namespace {got:?} differs from this run's namespace {want:?}")]
    Namespace { got: String, want: String },
    #[error("label {key}={got:?} conflicts with the value this server sets ({want:?})")]
    LabelConflict {
        key: String,
        got: String,
        want: String,
    },
    #[error("metadata.{0} must not be set")]
    Metadata(&'static str),
    #[error("{path}: {what} is not allowed")]
    Forbidden { path: String, what: &'static str },
    #[error("{path}: expected {expected}")]
    Shape {
        path: String,
        expected: &'static str,
    },
    #[error("{path}: resource quantity {value:?} does not parse")]
    Quantity { path: String, value: String },
    #[error("{kind} has no pod template at {path}")]
    NoTemplate { kind: &'static str, path: String },
}

/// One group of identical pods the workload will ask Kueue to admit.
#[derive(Debug, Clone)]
pub struct PodSet {
    pub name: String,
    pub count: u32,
    pub per_pod: BTreeMap<String, ParsedQuantity>,
}

impl PodSet {
    pub fn total(&self) -> BTreeMap<String, ParsedQuantity> {
        self.per_pod
            .iter()
            .map(|(k, v)| (k.clone(), v.clone() * self.count))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct Normalized {
    pub kind: WorkloadKind,
    pub name: String,
    pub object: Value,
    pub pod_sets: Vec<PodSet>,
    pub plan_hash: String,
}

impl Normalized {
    pub fn total_requests(&self) -> BTreeMap<String, ParsedQuantity> {
        let mut total: BTreeMap<String, ParsedQuantity> = BTreeMap::new();
        for set in &self.pod_sets {
            for (k, v) in set.total() {
                crate::quantity::add_into(&mut total, &k, v);
            }
        }
        total
    }
}

pub fn normalize(
    manifest: Value,
    caller: &Caller,
    queue: &QueueName,
) -> Result<Normalized, ManifestError> {
    let Value::Object(mut obj) = manifest else {
        return Err(ManifestError::NotObject);
    };
    let api_version = str_at(&obj, "apiVersion").unwrap_or_default().to_string();
    let kind_str = str_at(&obj, "kind").unwrap_or_default().to_string();
    let kind =
        WorkloadKind::from_type_meta(&api_version, &kind_str).ok_or(ManifestError::Kind {
            api_version,
            kind: kind_str,
        })?;
    obj.remove("status");

    let meta = object_entry(&mut obj, "metadata", "metadata")?;
    if meta.contains_key("generateName") {
        return Err(ManifestError::GenerateName);
    }
    for forbidden in ["ownerReferences", "finalizers"] {
        if meta.contains_key(forbidden) {
            return Err(ManifestError::Metadata(forbidden));
        }
    }
    for server_set in [
        "uid",
        "resourceVersion",
        "creationTimestamp",
        "managedFields",
    ] {
        meta.remove(server_set);
    }
    let name = meta
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if crate::caller::Namespace::parse(&name).is_err() {
        return Err(ManifestError::Name(name));
    }
    let ns = caller.namespace.as_str();
    match meta.get("namespace").and_then(Value::as_str) {
        Some(got) if got != ns => {
            return Err(ManifestError::Namespace {
                got: got.to_string(),
                want: ns.to_string(),
            });
        }
        _ => {
            meta.insert("namespace".into(), Value::String(ns.to_string()));
        }
    }
    check_reserved(meta, "/metadata", OBJECT_LABELS)?;
    let labels = object_entry(meta, "labels", "metadata.labels")?;
    set_label(labels, QUEUE_LABEL, queue.as_str())?;
    set_label(labels, MANAGED_LABEL, MANAGED_VALUE)?;
    set_label(labels, RUN_LABEL, caller.run_id.as_str())?;
    set_label(labels, LAUNCHED_BY_LABEL, caller.launched_by.as_str())?;

    if kind == WorkloadKind::Job {
        object_entry(&mut obj, "spec", "spec")?.insert("suspend".into(), Value::Bool(true));
    }

    let mut root = Value::Object(obj);
    check_object_fields(kind, &root)?;
    let mut pod_sets = Vec::new();
    for slot in template_slots(kind, &root)? {
        let template = root
            .pointer_mut(&slot.pointer)
            .and_then(Value::as_object_mut)
            .ok_or(ManifestError::NoTemplate {
                kind: kind.kind(),
                path: slot.pointer.clone(),
            })?;
        let tmeta = object_entry(template, "metadata", &format!("{}/metadata", slot.pointer))?;
        check_reserved(
            tmeta,
            &format!("{}/metadata", slot.pointer),
            TEMPLATE_LABELS,
        )?;
        let tlabels = object_entry(
            tmeta,
            "labels",
            &format!("{}/metadata/labels", slot.pointer),
        )?;
        set_label(tlabels, WORKLOAD_LABEL, &name)?;
        set_label(tlabels, RUN_LABEL, caller.run_id.as_str())?;
        let spec_path = format!("{}/spec", slot.pointer);
        let spec = object_entry(template, "spec", &spec_path)?;
        check_pod_spec(spec, &spec_path)?;
        spec.insert("automountServiceAccountToken".into(), Value::Bool(false));
        pod_sets.push(PodSet {
            name: slot.name,
            count: slot.count,
            per_pod: pod_requests(spec, &spec_path)?,
        });
    }

    if matches!(kind, WorkloadKind::Deployment | WorkloadKind::StatefulSet) {
        pin_selector(&mut root, &name, caller.run_id.as_str())?;
    }
    let object = root;
    let plan_hash = plan_hash(&object);
    Ok(Normalized {
        kind,
        name,
        object,
        pod_sets,
        plan_hash,
    })
}

/// SHA-256 over the key-sorted JSON of the normalized object, so the hash depends only on content.
pub fn plan_hash(object: &Value) -> String {
    let mut buf = Vec::new();
    write_canonical(object, &mut buf);
    hex::encode(Sha256::digest(&buf))
}

fn write_canonical(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(&Value::String(k.clone()), out);
                out.push(b':');
                if let Some(child) = map.get(k) {
                    write_canonical(child, out);
                }
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        scalar => out.extend_from_slice(scalar.to_string().as_bytes()),
    }
}

struct TemplateSlot {
    name: String,
    pointer: String,
    count: u32,
}

fn template_slots(kind: WorkloadKind, root: &Value) -> Result<Vec<TemplateSlot>, ManifestError> {
    let count_at = |ptr: &str, default: u32| -> Result<u32, ManifestError> {
        match root.pointer(ptr) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(ManifestError::Shape {
                    path: ptr.to_string(),
                    expected: "a non-negative integer",
                }),
        }
    };
    let require = |ptr: &str| -> Result<(), ManifestError> {
        match root.pointer(ptr) {
            Some(Value::Object(_)) => Ok(()),
            _ => Err(ManifestError::NoTemplate {
                kind: kind.kind(),
                path: ptr.to_string(),
            }),
        }
    };
    let mut slots = Vec::new();
    match kind {
        WorkloadKind::Job => {
            require("/spec/template")?;
            slots.push(TemplateSlot {
                name: "main".into(),
                pointer: "/spec/template".into(),
                count: count_at("/spec/parallelism", 1)?,
            });
        }
        WorkloadKind::RayJob => {
            let head = "/spec/rayClusterSpec/headGroupSpec/template";
            require(head)?;
            slots.push(TemplateSlot {
                name: "head".into(),
                pointer: head.into(),
                count: 1,
            });
            let groups = root
                .pointer("/spec/rayClusterSpec/workerGroupSpecs")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            for i in 0..groups {
                let base = format!("/spec/rayClusterSpec/workerGroupSpecs/{i}");
                require(&format!("{base}/template"))?;
                let name = root
                    .pointer(&format!("{base}/groupName"))
                    .and_then(Value::as_str)
                    .unwrap_or("workers")
                    .to_string();
                let replicas = count_at(&format!("{base}/replicas"), 1)?;
                let hosts = count_at(&format!("{base}/numOfHosts"), 1)?;
                slots.push(TemplateSlot {
                    name,
                    pointer: format!("{base}/template"),
                    count: replicas.saturating_mul(hosts),
                });
            }
            if root.pointer("/spec/submitterPodTemplate").is_some() {
                slots.push(TemplateSlot {
                    name: "submitter".into(),
                    pointer: "/spec/submitterPodTemplate".into(),
                    count: 1,
                });
            }
        }
        WorkloadKind::Deployment | WorkloadKind::StatefulSet => {
            require("/spec/template")?;
            slots.push(TemplateSlot {
                name: "main".into(),
                pointer: "/spec/template".into(),
                count: count_at("/spec/replicas", 1)?,
            });
        }
        WorkloadKind::LeaderWorkerSet => {
            let worker = "/spec/leaderWorkerTemplate/workerTemplate";
            require(worker)?;
            let replicas = count_at("/spec/replicas", 1)?;
            let size = count_at("/spec/leaderWorkerTemplate/size", 1)?;
            let leader = "/spec/leaderWorkerTemplate/leaderTemplate";
            if root.pointer(leader).is_some() {
                slots.push(TemplateSlot {
                    name: "leader".into(),
                    pointer: leader.into(),
                    count: replicas,
                });
                slots.push(TemplateSlot {
                    name: "worker".into(),
                    pointer: worker.into(),
                    count: replicas.saturating_mul(size.saturating_sub(1)),
                });
            } else {
                slots.push(TemplateSlot {
                    name: "worker".into(),
                    pointer: worker.into(),
                    count: replicas.saturating_mul(size),
                });
            }
        }
    }
    Ok(slots)
}

/// Label and annotation domains only this server, Kueue, OpenShell, or SPIRE may set.
const RESERVED_DOMAINS: [&str; 4] = [
    "kueue.x-k8s.io",
    "crucible.dev",
    "openshell.ai",
    "spiffe.io",
];
/// Reserved labels the object may carry; each is set by the server and conflicts are refused.
const OBJECT_LABELS: &[&str] = &[QUEUE_LABEL, MANAGED_LABEL, RUN_LABEL, LAUNCHED_BY_LABEL];
const TEMPLATE_LABELS: &[&str] = &[WORKLOAD_LABEL, RUN_LABEL];

const POD_FIELDS: &[&str] = &[
    "containers",
    "initContainers",
    "restartPolicy",
    "terminationGracePeriodSeconds",
    "activeDeadlineSeconds",
    "dnsPolicy",
    "dnsConfig",
    "nodeSelector",
    "affinity",
    "topologySpreadConstraints",
    "securityContext",
    "volumes",
    "imagePullSecrets",
    "hostname",
    "subdomain",
    "setHostnameAsFQDN",
    "enableServiceLinks",
    "shareProcessNamespace",
    "hostAliases",
    "readinessGates",
    "automountServiceAccountToken",
];
const CONTAINER_FIELDS: &[&str] = &[
    "name",
    "image",
    "imagePullPolicy",
    "command",
    "args",
    "workingDir",
    "env",
    "envFrom",
    "ports",
    "resources",
    "volumeMounts",
    "livenessProbe",
    "readinessProbe",
    "startupProbe",
    "lifecycle",
    "terminationMessagePath",
    "terminationMessagePolicy",
    "stdin",
    "stdinOnce",
    "tty",
    "securityContext",
    "restartPolicy",
    "resizePolicy",
];
const POD_SECURITY_FIELDS: &[&str] = &[
    "runAsNonRoot",
    "runAsUser",
    "runAsGroup",
    "fsGroup",
    "fsGroupChangePolicy",
    "supplementalGroups",
    "supplementalGroupsPolicy",
    "seccompProfile",
    "appArmorProfile",
];
const CONTAINER_SECURITY_FIELDS: &[&str] = &[
    "runAsNonRoot",
    "runAsUser",
    "runAsGroup",
    "readOnlyRootFilesystem",
    "allowPrivilegeEscalation",
    "privileged",
    "capabilities",
    "seccompProfile",
    "appArmorProfile",
    "procMount",
];
const VOLUME_SOURCES: &[&str] = &[
    "emptyDir",
    "configMap",
    "downwardAPI",
    "ephemeral",
    "projected",
];
const PROJECTED_SOURCES: &[&str] = &["configMap", "downwardAPI"];
const ENV_FIELDS: &[&str] = &["name", "value", "valueFrom"];
const ENV_SOURCES: &[&str] = &["fieldRef", "resourceFieldRef", "configMapKeyRef"];
const ENV_FROM_FIELDS: &[&str] = &["configMapRef", "prefix"];
const PORT_FIELDS: &[&str] = &["containerPort", "name", "protocol"];
const RESOURCE_FIELDS: &[&str] = &["requests", "limits"];
const MOUNT_FIELDS: &[&str] = &[
    "name",
    "mountPath",
    "subPath",
    "subPathExpr",
    "readOnly",
    "recursiveReadOnly",
];
const DNS_POLICIES: &[&str] = &["ClusterFirst", "Default", "None"];
const SAFE_PROFILES: &[&str] = &["RuntimeDefault"];

fn forbidden(path: String, what: &'static str) -> ManifestError {
    ManifestError::Forbidden { path, what }
}

fn object_at<'a>(v: &'a Value, path: &str) -> Result<&'a Map<String, Value>, ManifestError> {
    v.as_object().ok_or(ManifestError::Shape {
        path: path.to_string(),
        expected: "an object",
    })
}

fn array_of<'a>(v: &'a Value, path: &str) -> Result<&'a [Value], ManifestError> {
    v.as_array().map(Vec::as_slice).ok_or(ManifestError::Shape {
        path: path.to_string(),
        expected: "an array",
    })
}

fn only_fields(
    obj: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), ManifestError> {
    match obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(forbidden(
            format!("{path}/{k}"),
            "a field outside the allowlist for agent manifests",
        )),
        None => Ok(()),
    }
}

fn is_reserved(key: &str) -> bool {
    let Some((domain, _)) = key.split_once('/') else {
        return false;
    };
    RESERVED_DOMAINS
        .iter()
        .any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
}

/// Refuses reserved labels outside `allowed_labels` and every reserved annotation, so an agent
/// can neither pick a Kueue priority or pod group nor forge the server's own bookkeeping.
fn check_reserved(
    meta: &Map<String, Value>,
    path: &str,
    allowed_labels: &[&str],
) -> Result<(), ManifestError> {
    for (field, allowed) in [("labels", allowed_labels), ("annotations", &[][..])] {
        let Some(map) = meta.get(field).filter(|v| !v.is_null()) else {
            continue;
        };
        let map = object_at(map, &format!("{path}/{field}"))?;
        if let Some(key) = map
            .keys()
            .find(|k| is_reserved(k) && !allowed.contains(&k.as_str()))
        {
            return Err(forbidden(
                format!("{path}/{field}/{key}"),
                "a label or annotation reserved for the server",
            ));
        }
    }
    Ok(())
}

const TOP_FIELDS: &[&str] = &["apiVersion", "kind", "metadata", "spec"];
const JOB_SPEC: &[&str] = &[
    "template",
    "parallelism",
    "completions",
    "completionMode",
    "backoffLimit",
    "backoffLimitPerIndex",
    "maxFailedIndexes",
    "activeDeadlineSeconds",
    "ttlSecondsAfterFinished",
    "podFailurePolicy",
    "successPolicy",
    "podReplacementPolicy",
    "suspend",
];
const DEPLOYMENT_SPEC: &[&str] = &[
    "replicas",
    "selector",
    "template",
    "strategy",
    "minReadySeconds",
    "revisionHistoryLimit",
    "progressDeadlineSeconds",
    "paused",
];
const STATEFULSET_SPEC: &[&str] = &[
    "replicas",
    "selector",
    "template",
    "serviceName",
    "podManagementPolicy",
    "updateStrategy",
    "revisionHistoryLimit",
    "minReadySeconds",
    "persistentVolumeClaimRetentionPolicy",
    "ordinals",
    "volumeClaimTemplates",
];
const LWS_SPEC: &[&str] = &[
    "replicas",
    "leaderWorkerTemplate",
    "rolloutStrategy",
    "startupPolicy",
    "networkConfig",
];
const LWS_TEMPLATE: &[&str] = &[
    "size",
    "leaderTemplate",
    "workerTemplate",
    "restartPolicy",
    "subGroupPolicy",
    "volumeClaimTemplates",
];
const RAYJOB_SPEC: &[&str] = &[
    "entrypoint",
    "entrypointNumCpus",
    "entrypointNumGpus",
    "entrypointResources",
    "runtimeEnvYAML",
    "jobId",
    "shutdownAfterJobFinishes",
    "ttlSecondsAfterFinished",
    "activeDeadlineSeconds",
    "backoffLimit",
    "rayClusterSpec",
    "submitterPodTemplate",
    "submitterConfig",
    "submissionMode",
    "suspend",
    "deletionStrategy",
];
const RAYCLUSTER_SPEC: &[&str] = &[
    "rayVersion",
    "headGroupSpec",
    "workerGroupSpecs",
    "enableInTreeAutoscaling",
    "suspend",
];
const RAY_HEAD: &[&str] = &["template", "rayStartParams", "serviceType"];
const RAY_WORKER: &[&str] = &[
    "groupName",
    "replicas",
    "minReplicas",
    "maxReplicas",
    "numOfHosts",
    "rayStartParams",
    "template",
    "scaleStrategy",
    "idleTimeoutSeconds",
    "suspend",
];
const PVC_TEMPLATE: &[&str] = &["metadata", "spec"];
const PVC_SPEC: &[&str] = &["accessModes", "resources", "volumeMode"];

/// Object-level fields outside the pod templates. Each kind's spec is allowlisted, so fields that
/// hand the object to another controller (`managedBy`, `clusterSelector`), adopt foreign pods,
/// or provision storage from another claim or class never reach the cluster.
fn check_object_fields(kind: WorkloadKind, root: &Value) -> Result<(), ManifestError> {
    let top = object_at(root, "")?;
    only_fields(top, TOP_FIELDS, "")?;
    let Some(spec) = top.get("spec") else {
        return Ok(());
    };
    let spec = object_at(spec, "/spec")?;
    match kind {
        WorkloadKind::Job => only_fields(spec, JOB_SPEC, "/spec"),
        WorkloadKind::Deployment => only_fields(spec, DEPLOYMENT_SPEC, "/spec"),
        WorkloadKind::StatefulSet => {
            only_fields(spec, STATEFULSET_SPEC, "/spec")?;
            check_claim_templates(spec, "/spec")
        }
        WorkloadKind::LeaderWorkerSet => {
            only_fields(spec, LWS_SPEC, "/spec")?;
            if let Some(t) = spec.get("leaderWorkerTemplate") {
                let path = "/spec/leaderWorkerTemplate";
                let t = object_at(t, path)?;
                only_fields(t, LWS_TEMPLATE, path)?;
                check_claim_templates(t, path)?;
            }
            Ok(())
        }
        WorkloadKind::RayJob => check_rayjob(spec),
    }
}

fn check_rayjob(spec: &Map<String, Value>) -> Result<(), ManifestError> {
    only_fields(spec, RAYJOB_SPEC, "/spec")?;
    let Some(cluster) = spec.get("rayClusterSpec") else {
        return Ok(());
    };
    let path = "/spec/rayClusterSpec";
    let cluster = object_at(cluster, path)?;
    only_fields(cluster, RAYCLUSTER_SPEC, path)?;
    if cluster.get("enableInTreeAutoscaling") == Some(&Value::Bool(true)) {
        return Err(forbidden(
            format!("{path}/enableInTreeAutoscaling"),
            "the in-tree autoscaler sidecar",
        ));
    }
    if let Some(head) = cluster.get("headGroupSpec") {
        let h_path = format!("{path}/headGroupSpec");
        let head = object_at(head, &h_path)?;
        only_fields(head, RAY_HEAD, &h_path)?;
        if let Some(service) = head.get("serviceType").and_then(Value::as_str)
            && service != "ClusterIP"
        {
            return Err(forbidden(
                format!("{h_path}/serviceType"),
                "exposing the head outside the cluster",
            ));
        }
    }
    if let Some(groups) = cluster.get("workerGroupSpecs") {
        let g_path = format!("{path}/workerGroupSpecs");
        for (i, g) in array_of(groups, &g_path)?.iter().enumerate() {
            let w_path = format!("{g_path}/{i}");
            only_fields(object_at(g, &w_path)?, RAY_WORKER, &w_path)?;
        }
    }
    Ok(())
}

fn check_claim_templates(spec: &Map<String, Value>, path: &str) -> Result<(), ManifestError> {
    let Some(templates) = spec.get("volumeClaimTemplates") else {
        return Ok(());
    };
    let t_path = format!("{path}/volumeClaimTemplates");
    for (i, t) in array_of(templates, &t_path)?.iter().enumerate() {
        check_claim_template(t, &format!("{t_path}/{i}"))?;
    }
    Ok(())
}

/// A claim the workload provisions: default storage class only, empty, never cloned from or
/// bound to an existing volume.
fn check_claim_template(t: &Value, path: &str) -> Result<(), ManifestError> {
    let t = object_at(t, path)?;
    only_fields(t, PVC_TEMPLATE, path)?;
    if let Some(meta) = t.get("metadata") {
        check_reserved(
            object_at(meta, &format!("{path}/metadata"))?,
            &format!("{path}/metadata"),
            &[],
        )?;
    }
    if let Some(spec) = t.get("spec") {
        let s_path = format!("{path}/spec");
        only_fields(object_at(spec, &s_path)?, PVC_SPEC, &s_path)?;
    }
    Ok(())
}

/// Pins a Deployment or StatefulSet selector to the server-set template labels, so its
/// controller can only adopt pods this object created.
fn pin_selector(root: &mut Value, name: &str, run: &str) -> Result<(), ManifestError> {
    let Some(spec) = root.get_mut("spec").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let selector = object_entry(spec, "selector", "/spec/selector")?;
    only_fields(
        selector,
        &["matchLabels", "matchExpressions"],
        "/spec/selector",
    )?;
    let labels = object_entry(selector, "matchLabels", "/spec/selector/matchLabels")?;
    set_label(labels, WORKLOAD_LABEL, name)?;
    set_label(labels, RUN_LABEL, run)
}

fn check_pod_spec(spec: &Map<String, Value>, path: &str) -> Result<(), ManifestError> {
    only_fields(spec, POD_FIELDS, path)?;
    if spec.get("automountServiceAccountToken") == Some(&Value::Bool(true)) {
        return Err(forbidden(
            format!("{path}/automountServiceAccountToken"),
            "mounting the service account token",
        ));
    }
    if let Some(policy) = spec.get("dnsPolicy").and_then(Value::as_str)
        && !DNS_POLICIES.contains(&policy)
    {
        return Err(forbidden(format!("{path}/dnsPolicy"), "this DNS policy"));
    }
    if let Some(sc) = spec.get("securityContext") {
        let sc_path = format!("{path}/securityContext");
        check_security_context(object_at(sc, &sc_path)?, POD_SECURITY_FIELDS, &sc_path)?;
    }
    if let Some(volumes) = spec.get("volumes") {
        for (i, vol) in array_of(volumes, &format!("{path}/volumes"))?
            .iter()
            .enumerate()
        {
            check_volume(vol, &format!("{path}/volumes/{i}"))?;
        }
    }
    for list in ["containers", "initContainers"] {
        let Some(containers) = spec.get(list) else {
            continue;
        };
        for (i, c) in array_of(containers, &format!("{path}/{list}"))?
            .iter()
            .enumerate()
        {
            check_container(c, &format!("{path}/{list}/{i}"))?;
        }
    }
    Ok(())
}

fn check_volume(vol: &Value, path: &str) -> Result<(), ManifestError> {
    let vol = object_at(vol, path)?;
    let source = vol.keys().find(|k| k.as_str() != "name");
    match source {
        Some(k) if VOLUME_SOURCES.contains(&k.as_str()) => {}
        Some(k) => {
            return Err(forbidden(format!("{path}/{k}"), "this volume type"));
        }
        None => return Ok(()),
    }
    if vol.len() > 2 {
        return Err(ManifestError::Shape {
            path: path.to_string(),
            expected: "a name and exactly one volume source",
        });
    }
    if let Some(ephemeral) = vol.get("ephemeral") {
        let e_path = format!("{path}/ephemeral");
        let ephemeral = object_at(ephemeral, &e_path)?;
        only_fields(ephemeral, &["volumeClaimTemplate"], &e_path)?;
        if let Some(template) = ephemeral.get("volumeClaimTemplate") {
            check_claim_template(template, &format!("{e_path}/volumeClaimTemplate"))?;
        }
    }
    if let Some(projected) = vol.get("projected") {
        let p_path = format!("{path}/projected");
        let projected = object_at(projected, &p_path)?;
        if let Some(sources) = projected.get("sources") {
            for (i, src) in array_of(sources, &format!("{p_path}/sources"))?
                .iter()
                .enumerate()
            {
                only_fields(
                    object_at(src, &format!("{p_path}/sources/{i}"))?,
                    PROJECTED_SOURCES,
                    &format!("{p_path}/sources/{i}"),
                )?;
            }
        }
    }
    Ok(())
}

fn check_container(c: &Value, path: &str) -> Result<(), ManifestError> {
    let c = object_at(c, path)?;
    only_fields(c, CONTAINER_FIELDS, path)?;
    if let Some(env) = c.get("env") {
        for (i, e) in array_of(env, &format!("{path}/env"))?.iter().enumerate() {
            let e_path = format!("{path}/env/{i}");
            let e = object_at(e, &e_path)?;
            only_fields(e, ENV_FIELDS, &e_path)?;
            if let Some(from) = e.get("valueFrom") {
                let f_path = format!("{e_path}/valueFrom");
                only_fields(object_at(from, &f_path)?, ENV_SOURCES, &f_path)?;
            }
        }
    }
    if let Some(env_from) = c.get("envFrom") {
        for (i, e) in array_of(env_from, &format!("{path}/envFrom"))?
            .iter()
            .enumerate()
        {
            let e_path = format!("{path}/envFrom/{i}");
            only_fields(object_at(e, &e_path)?, ENV_FROM_FIELDS, &e_path)?;
        }
    }
    if let Some(ports) = c.get("ports") {
        for (i, p) in array_of(ports, &format!("{path}/ports"))?
            .iter()
            .enumerate()
        {
            let p_path = format!("{path}/ports/{i}");
            only_fields(object_at(p, &p_path)?, PORT_FIELDS, &p_path)?;
        }
    }
    if let Some(res) = c.get("resources") {
        let r_path = format!("{path}/resources");
        only_fields(object_at(res, &r_path)?, RESOURCE_FIELDS, &r_path)?;
    }
    if let Some(mounts) = c.get("volumeMounts") {
        for (i, m) in array_of(mounts, &format!("{path}/volumeMounts"))?
            .iter()
            .enumerate()
        {
            let m_path = format!("{path}/volumeMounts/{i}");
            only_fields(object_at(m, &m_path)?, MOUNT_FIELDS, &m_path)?;
        }
    }
    for handler_owner in [
        "livenessProbe",
        "readinessProbe",
        "startupProbe",
        "lifecycle",
    ] {
        if let Some(v) = c.get(handler_owner) {
            check_no_remote_host(v, &format!("{path}/{handler_owner}"))?;
        }
    }
    if let Some(sc) = c.get("securityContext") {
        let sc_path = format!("{path}/securityContext");
        check_security_context(
            object_at(sc, &sc_path)?,
            CONTAINER_SECURITY_FIELDS,
            &sc_path,
        )?;
    }
    Ok(())
}

/// Probe and lifecycle handlers run from the kubelet; a `host` would aim it at another address.
fn check_no_remote_host(v: &Value, path: &str) -> Result<(), ManifestError> {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                let child_path = format!("{path}/{k}");
                if k == "host" {
                    return Err(forbidden(
                        child_path,
                        "a probe or hook aimed at another host",
                    ));
                }
                check_no_remote_host(child, &child_path)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                check_no_remote_host(item, &format!("{path}/{i}"))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn check_security_context(
    sc: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), ManifestError> {
    only_fields(sc, allowed, path)?;
    let field = |k: &str| format!("{path}/{k}");
    for (key, what) in [
        ("privileged", "privileged containers"),
        ("allowPrivilegeEscalation", "privilege escalation"),
    ] {
        if sc.get(key) == Some(&Value::Bool(true)) {
            return Err(forbidden(field(key), what));
        }
    }
    if sc.get("runAsNonRoot") == Some(&Value::Bool(false)) {
        return Err(forbidden(field("runAsNonRoot"), "running as root"));
    }
    if sc.get("runAsUser").and_then(Value::as_i64) == Some(0) {
        return Err(forbidden(field("runAsUser"), "running as root"));
    }
    if let Some(mount) = sc.get("procMount").and_then(Value::as_str)
        && mount != "Default"
    {
        return Err(forbidden(field("procMount"), "an unmasked /proc"));
    }
    for profile in ["seccompProfile", "appArmorProfile"] {
        if let Some(p) = sc.get(profile) {
            let kind = p.get("type").and_then(Value::as_str).unwrap_or_default();
            if !SAFE_PROFILES.contains(&kind) {
                return Err(forbidden(
                    field(profile),
                    "a profile other than RuntimeDefault",
                ));
            }
        }
    }
    if let Some(caps) = sc.get("capabilities") {
        let c_path = field("capabilities");
        let caps = object_at(caps, &c_path)?;
        only_fields(caps, &["add", "drop"], &c_path)?;
        if caps
            .get("add")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        {
            return Err(forbidden(format!("{c_path}/add"), "added capabilities"));
        }
    }
    Ok(())
}

/// Effective per-pod requests: the sum over containers, raised to the largest init container,
/// with limits standing in for requests a container leaves unset.
fn pod_requests(
    spec: &Map<String, Value>,
    path: &str,
) -> Result<BTreeMap<String, ParsedQuantity>, ManifestError> {
    let mut sum: BTreeMap<String, ParsedQuantity> = BTreeMap::new();
    for (i, c) in array_at(spec, "containers").iter().enumerate() {
        for (k, v) in container_requests(c, &format!("{path}/containers/{i}"))? {
            crate::quantity::add_into(&mut sum, &k, v);
        }
    }
    for (i, c) in array_at(spec, "initContainers").iter().enumerate() {
        for (k, v) in container_requests(c, &format!("{path}/initContainers/{i}"))? {
            if sum.get(&k).is_none_or(|cur| v > *cur) {
                sum.insert(k, v);
            }
        }
    }
    Ok(sum)
}

fn container_requests(
    c: &Value,
    path: &str,
) -> Result<BTreeMap<String, ParsedQuantity>, ManifestError> {
    let mut out = BTreeMap::new();
    for source in ["limits", "requests"] {
        let Some(Value::Object(map)) = c.pointer(&format!("/resources/{source}")) else {
            continue;
        };
        for (k, v) in map {
            let text = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => {
                    return Err(ManifestError::Shape {
                        path: format!("{path}/resources/{source}/{k}"),
                        expected: "a quantity string",
                    });
                }
            };
            let q =
                ParsedQuantity::try_from(text.as_str()).map_err(|_| ManifestError::Quantity {
                    path: format!("{path}/resources/{source}/{k}"),
                    value: text.clone(),
                })?;
            out.insert(k.clone(), q);
        }
    }
    Ok(out)
}

fn set_label(labels: &mut Map<String, Value>, key: &str, value: &str) -> Result<(), ManifestError> {
    match labels.get(key).and_then(Value::as_str) {
        Some(got) if got != value => Err(ManifestError::LabelConflict {
            key: key.to_string(),
            got: got.to_string(),
            want: value.to_string(),
        }),
        _ => {
            labels.insert(key.to_string(), Value::String(value.to_string()));
            Ok(())
        }
    }
}

fn str_at<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

fn array_at<'a>(obj: &'a Map<String, Value>, key: &str) -> &'a [Value] {
    obj.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn object_entry<'a>(
    obj: &'a mut Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<&'a mut Map<String, Value>, ManifestError> {
    let slot = obj
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if slot.is_null() {
        *slot = Value::Object(Map::new());
    }
    slot.as_object_mut().ok_or(ManifestError::Shape {
        path: path.to_string(),
        expected: "an object",
    })
}

#[cfg(test)]
mod tests {
    use crate::caller::{Launcher, Namespace, QueueName, RunId};
    use crate::manifest::*;
    use serde_json::json;

    fn caller() -> Caller {
        Caller {
            run_id: RunId::parse("run-7").unwrap(),
            namespace: Namespace::parse("team-a").unwrap(),
            launched_by: Launcher::parse("alice").unwrap(),
        }
    }

    fn queue() -> QueueName {
        QueueName::parse("gpu").unwrap()
    }

    fn job() -> Value {
        json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": {"name": "train"},
            "spec": {
                "parallelism": 2,
                "template": {"spec": {
                    "restartPolicy": "Never",
                    "containers": [{
                        "name": "main",
                        "image": "ubi9",
                        "resources": {"requests": {"cpu": "500m", "memory": "1Gi"}, "limits": {"nvidia.com/gpu": "1"}}
                    }]
                }}
            }
        })
    }

    #[test]
    fn a_job_is_pinned_to_the_callers_namespace_queue_and_run() {
        let n = normalize(job(), &caller(), &queue()).unwrap();
        let o = &n.object;
        assert_eq!(o.pointer("/metadata/namespace"), Some(&json!("team-a")));
        assert_eq!(
            o.pointer("/metadata/labels/kueue.x-k8s.io~1queue-name"),
            Some(&json!("gpu"))
        );
        assert_eq!(
            o.pointer("/metadata/labels/crucible.dev~1run-id"),
            Some(&json!("run-7"))
        );
        assert_eq!(o.pointer("/spec/suspend"), Some(&json!(true)));
        assert_eq!(
            o.pointer("/spec/template/metadata/labels/crucible.dev~1workload"),
            Some(&json!("train"))
        );
        assert_eq!(
            o.pointer("/spec/template/spec/automountServiceAccountToken"),
            Some(&json!(false))
        );
    }

    #[test]
    fn job_requests_multiply_by_parallelism_and_limits_fill_missing_requests() {
        let n = normalize(job(), &caller(), &queue()).unwrap();
        let total = n.total_requests();
        assert_eq!(total["cpu"], ParsedQuantity::try_from("1").unwrap());
        assert_eq!(total["memory"], ParsedQuantity::try_from("2Gi").unwrap());
        assert_eq!(
            total["nvidia.com/gpu"],
            ParsedQuantity::try_from("2").unwrap()
        );
    }

    #[test]
    fn the_plan_hash_ignores_key_order_but_tracks_content() {
        let a = normalize(job(), &caller(), &queue()).unwrap();
        let reordered: Value = serde_json::from_str(
            r#"{"spec":{"template":{"spec":{"containers":[{"resources":{"limits":{"nvidia.com/gpu":"1"},"requests":{"memory":"1Gi","cpu":"500m"}},"image":"ubi9","name":"main"}],"restartPolicy":"Never"}},"parallelism":2},"metadata":{"name":"train"},"kind":"Job","apiVersion":"batch/v1"}"#,
        )
        .unwrap();
        let b = normalize(reordered, &caller(), &queue()).unwrap();
        assert_eq!(a.plan_hash, b.plan_hash);
        let mut changed = job();
        changed["spec"]["parallelism"] = json!(3);
        let c = normalize(changed, &caller(), &queue()).unwrap();
        assert_ne!(a.plan_hash, c.plan_hash);
        let other_queue = normalize(job(), &caller(), &QueueName::parse("cpu").unwrap()).unwrap();
        assert_ne!(a.plan_hash, other_queue.plan_hash);
    }

    #[test]
    fn kinds_outside_the_allowlist_are_refused() {
        let pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}});
        assert!(matches!(
            normalize(pod, &caller(), &queue()),
            Err(ManifestError::Kind { .. })
        ));
        let old_job = json!({"apiVersion": "batch/v2", "kind": "Job", "metadata": {"name": "j"}});
        assert!(matches!(
            normalize(old_job, &caller(), &queue()),
            Err(ManifestError::Kind { .. })
        ));
    }

    #[test]
    fn another_namespace_or_a_conflicting_queue_label_is_refused() {
        let mut m = job();
        m["metadata"]["namespace"] = json!("team-b");
        assert!(matches!(
            normalize(m, &caller(), &queue()),
            Err(ManifestError::Namespace { .. })
        ));
        let mut m = job();
        m["metadata"]["labels"] = json!({"kueue.x-k8s.io/queue-name": "other"});
        assert!(matches!(
            normalize(m, &caller(), &queue()),
            Err(ManifestError::LabelConflict { .. })
        ));
        let mut m = job();
        m["metadata"]["labels"] = json!({"crucible.dev/run-id": "someone-else"});
        assert!(matches!(
            normalize(m, &caller(), &queue()),
            Err(ManifestError::LabelConflict { .. })
        ));
    }

    #[test]
    fn privileged_or_host_level_pod_specs_are_refused_with_the_offending_path() {
        let cases: Vec<(&str, Value)> = vec![
            ("serviceAccountName", json!({"serviceAccountName": "admin"})),
            (
                "automountServiceAccountToken",
                json!({"automountServiceAccountToken": true}),
            ),
            ("hostNetwork", json!({"hostNetwork": true})),
            ("hostPID", json!({"hostPID": true})),
            ("nodeName", json!({"nodeName": "a100-01"})),
            (
                "hostPath",
                json!({"volumes": [{"name": "h", "hostPath": {"path": "/"}}]}),
            ),
        ];
        for (needle, patch) in cases {
            let mut m = job();
            for (k, v) in patch.as_object().unwrap() {
                m["spec"]["template"]["spec"][k] = v.clone();
            }
            let err = normalize(m, &caller(), &queue()).unwrap_err();
            assert!(err.to_string().contains(needle), "{needle}: {err}");
        }
        for (needle, sc) in [
            ("privileged", json!({"privileged": true})),
            (
                "allowPrivilegeEscalation",
                json!({"allowPrivilegeEscalation": true}),
            ),
            (
                "capabilities/add",
                json!({"capabilities": {"add": ["SYS_ADMIN"]}}),
            ),
        ] {
            let mut m = job();
            m["spec"]["template"]["spec"]["containers"][0]["securityContext"] = sc;
            let err = normalize(m, &caller(), &queue()).unwrap_err();
            assert!(err.to_string().contains(needle), "{needle}: {err}");
        }
    }

    #[test]
    fn metadata_that_would_hijack_ownership_is_refused() {
        for (field, v) in [
            ("ownerReferences", json!([{"kind": "X"}])),
            ("finalizers", json!(["x"])),
        ] {
            let mut m = job();
            m["metadata"][field] = v;
            assert_eq!(
                normalize(m, &caller(), &queue()).unwrap_err(),
                ManifestError::Metadata(field)
            );
        }
        let mut m = job();
        m["metadata"] = json!({"generateName": "x-"});
        assert_eq!(
            normalize(m, &caller(), &queue()).unwrap_err(),
            ManifestError::GenerateName
        );
    }

    #[test]
    fn a_rayjob_counts_head_and_every_worker_host() {
        let ray = json!({
            "apiVersion": "ray.io/v1",
            "kind": "RayJob",
            "metadata": {"name": "r"},
            "spec": {"rayClusterSpec": {
                "headGroupSpec": {"template": {"spec": {"containers": [{"name": "h", "resources": {"requests": {"cpu": "1"}}}]}}},
                "workerGroupSpecs": [{
                    "groupName": "gpu", "replicas": 2, "numOfHosts": 2,
                    "template": {"spec": {"containers": [{"name": "w", "resources": {"limits": {"nvidia.com/gpu": "8"}}}]}}
                }]
            }}
        });
        let n = normalize(ray, &caller(), &queue()).unwrap();
        let names: Vec<_> = n
            .pod_sets
            .iter()
            .map(|p| (p.name.as_str(), p.count))
            .collect();
        assert_eq!(names, vec![("head", 1), ("gpu", 4)]);
        assert_eq!(
            n.total_requests()["nvidia.com/gpu"],
            ParsedQuantity::try_from("32").unwrap()
        );
        assert!(n.object.pointer("/spec/suspend").is_none());
    }

    #[test]
    fn a_leaderworkerset_splits_leader_and_workers_by_size() {
        let lws = json!({
            "apiVersion": "leaderworkerset.x-k8s.io/v1",
            "kind": "LeaderWorkerSet",
            "metadata": {"name": "l"},
            "spec": {"replicas": 2, "leaderWorkerTemplate": {
                "size": 4,
                "leaderTemplate": {"spec": {"containers": [{"name": "l"}]}},
                "workerTemplate": {"spec": {"containers": [{"name": "w", "resources": {"limits": {"nvidia.com/gpu": "8"}}}]}}
            }}
        });
        let n = normalize(lws, &caller(), &queue()).unwrap();
        let names: Vec<_> = n
            .pod_sets
            .iter()
            .map(|p| (p.name.as_str(), p.count))
            .collect();
        assert_eq!(names, vec![("leader", 2), ("worker", 6)]);
    }

    #[test]
    fn init_containers_raise_but_do_not_add_to_requests() {
        let mut m = job();
        m["spec"]["parallelism"] = json!(1);
        m["spec"]["template"]["spec"]["initContainers"] =
            json!([{"name": "i", "resources": {"requests": {"cpu": "2", "memory": "1Mi"}}}]);
        let total = normalize(m, &caller(), &queue()).unwrap().total_requests();
        assert_eq!(total["cpu"], ParsedQuantity::try_from("2").unwrap());
        assert_eq!(total["memory"], ParsedQuantity::try_from("1Gi").unwrap());
    }

    #[test]
    fn unparseable_quantities_are_refused_with_their_path() {
        let mut m = job();
        m["spec"]["template"]["spec"]["containers"][0]["resources"]["requests"]["cpu"] =
            json!("lots");
        let err = normalize(m, &caller(), &queue()).unwrap_err();
        assert!(
            err.to_string()
                .contains("containers/0/resources/requests/cpu"),
            "{err}"
        );
    }

    #[test]
    fn a_deployment_counts_replicas_and_is_admitted_per_pod() {
        let d = json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": "svc"},
            "spec": {
                "replicas": 3,
                "selector": {"matchLabels": {"app": "svc"}},
                "template": {"metadata": {"labels": {"app": "svc"}}, "spec": {"containers": [
                    {"name": "s", "resources": {"requests": {"cpu": "250m"}}}
                ]}}
            }
        });
        let n = normalize(d, &caller(), &queue()).unwrap();
        assert_eq!(n.kind.admission(), Admission::PerPod);
        assert_eq!(
            n.total_requests()["cpu"],
            ParsedQuantity::try_from("750m").unwrap()
        );
        assert!(n.object.pointer("/spec/suspend").is_none());
        assert_eq!(
            n.object.pointer("/spec/template/metadata/labels/app"),
            Some(&json!("svc")),
            "selector labels survive"
        );
        assert_eq!(
            n.object
                .pointer("/spec/template/metadata/labels/crucible.dev~1workload"),
            Some(&json!("svc"))
        );
    }

    #[test]
    fn a_statefulset_is_one_all_or_nothing_group_defaulting_to_one_replica() {
        let s = json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "metadata": {"name": "db"},
            "spec": {"template": {"spec": {"containers": [{"name": "d", "resources": {"requests": {"memory": "1Gi"}}}]}}}
        });
        let n = normalize(s, &caller(), &queue()).unwrap();
        assert_eq!(n.kind.admission(), Admission::AllOrNothing);
        assert_eq!(n.pod_sets[0].count, 1);
        assert!(n.kind.is_scalable());
    }

    #[test]
    fn a_job_without_a_pod_template_is_refused() {
        let m =
            json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "j"}, "spec": {}});
        assert!(matches!(
            normalize(m, &caller(), &queue()),
            Err(ManifestError::NoTemplate { .. })
        ));
    }

    #[test]
    fn a_delegated_callers_launcher_is_stamped_and_cannot_be_overridden() {
        let delegated = caller();
        let n = normalize(job(), &delegated, &queue()).unwrap();
        assert_eq!(
            n.object
                .pointer("/metadata/labels/crucible.dev~1launched-by"),
            Some(&json!("alice"))
        );
        let mut forged = job();
        forged["metadata"]["labels"] = json!({"crucible.dev/launched-by": "someone-else"});
        assert!(matches!(
            normalize(forged, &delegated, &queue()),
            Err(ManifestError::LabelConflict { key, .. }) if key == LAUNCHED_BY_LABEL
        ));
    }

    #[test]
    fn the_launcher_is_part_of_the_plan_hash() {
        let mut delegated = caller();
        delegated.launched_by = Launcher::parse("bob").unwrap();
        let a = normalize(job(), &caller(), &queue()).unwrap();
        let b = normalize(job(), &delegated, &queue()).unwrap();
        assert_ne!(a.plan_hash, b.plan_hash);
    }

    fn refused_with(m: Value, needle: &str) {
        let err = normalize(m, &caller(), &queue()).unwrap_err();
        assert!(err.to_string().contains(needle), "{needle}: {err}");
    }

    #[test]
    fn pod_fields_outside_the_allowlist_are_refused_with_their_path() {
        let pod: Vec<(&str, Value)> = vec![
            (
                "serviceAccountToken",
                json!({"volumes": [{"name": "t", "projected": {"sources": [{"serviceAccountToken": {"path": "t"}}]}}]}),
            ),
            (
                "nfs",
                json!({"volumes": [{"name": "n", "nfs": {"server": "s", "path": "/"}}]}),
            ),
            (
                "csi",
                json!({"volumes": [{"name": "c", "csi": {"driver": "d"}}]}),
            ),
            (
                "secret",
                json!({"volumes": [{"name": "s", "secret": {"secretName": "x"}}]}),
            ),
            ("runAsUser", json!({"securityContext": {"runAsUser": 0}})),
            (
                "runAsNonRoot",
                json!({"securityContext": {"runAsNonRoot": false}}),
            ),
            (
                "seccompProfile",
                json!({"securityContext": {"seccompProfile": {"type": "Unconfined"}}}),
            ),
            (
                "seLinuxOptions",
                json!({"securityContext": {"seLinuxOptions": {"type": "spc_t"}}}),
            ),
            (
                "sysctls",
                json!({"securityContext": {"sysctls": [{"name": "kernel.x", "value": "1"}]}}),
            ),
            ("hostUsers", json!({"hostUsers": false})),
            ("runtimeClassName", json!({"runtimeClassName": "kata"})),
            (
                "tolerations",
                json!({"tolerations": [{"operator": "Exists"}]}),
            ),
            (
                "priorityClassName",
                json!({"priorityClassName": "system-node-critical"}),
            ),
            (
                "resourceClaims",
                json!({"resourceClaims": [{"name": "gpu"}]}),
            ),
            (
                "schedulingGates",
                json!({"schedulingGates": [{"name": "g"}]}),
            ),
            (
                "ephemeralContainers",
                json!({"ephemeralContainers": [{"name": "e"}]}),
            ),
            ("dnsPolicy", json!({"dnsPolicy": "ClusterFirstWithHostNet"})),
        ];
        for (needle, patch) in pod {
            let mut m = job();
            for (k, v) in patch.as_object().unwrap() {
                m["spec"]["template"]["spec"][k] = v.clone();
            }
            refused_with(m, needle);
        }
        let container: Vec<(&str, Value)> = vec![
            (
                "hostPort",
                json!({"ports": [{"containerPort": 80, "hostPort": 80}]}),
            ),
            (
                "secretKeyRef",
                json!({"env": [{"name": "T", "valueFrom": {"secretKeyRef": {"name": "s", "key": "k"}}}]}),
            ),
            (
                "secretRef",
                json!({"envFrom": [{"secretRef": {"name": "s"}}]}),
            ),
            (
                "claims",
                json!({"resources": {"claims": [{"name": "gpu"}]}}),
            ),
            (
                "volumeDevices",
                json!({"volumeDevices": [{"name": "d", "devicePath": "/dev/x"}]}),
            ),
            (
                "mountPropagation",
                json!({"volumeMounts": [{"name": "v", "mountPath": "/m", "mountPropagation": "Bidirectional"}]}),
            ),
            (
                "procMount",
                json!({"securityContext": {"procMount": "Unmasked"}}),
            ),
            (
                "windowsOptions",
                json!({"securityContext": {"windowsOptions": {"hostProcess": true}}}),
            ),
            (
                "host",
                json!({"livenessProbe": {"httpGet": {"host": "169.254.169.254", "path": "/", "port": 80}}}),
            ),
            (
                "host",
                json!({"lifecycle": {"preStop": {"tcpSocket": {"host": "10.0.0.1", "port": 22}}}}),
            ),
        ];
        for (needle, patch) in container {
            let mut m = job();
            for (k, v) in patch.as_object().unwrap() {
                m["spec"]["template"]["spec"]["containers"][0][k] = v.clone();
            }
            refused_with(m, needle);
        }
    }

    #[test]
    fn a_hardened_pod_spec_is_accepted() {
        let mut m = job();
        m["spec"]["template"]["spec"]["securityContext"] = json!({
            "runAsNonRoot": true, "runAsUser": 1001, "fsGroup": 1001,
            "seccompProfile": {"type": "RuntimeDefault"}
        });
        m["spec"]["template"]["spec"]["volumes"] = json!([
            {"name": "scratch", "emptyDir": {}},
            {"name": "cfg", "configMap": {"name": "settings"}},
            {"name": "info", "projected": {"sources": [{"downwardAPI": {"items": [{"path": "labels", "fieldRef": {"fieldPath": "metadata.labels"}}]}}]}}
        ]);
        let c = &mut m["spec"]["template"]["spec"]["containers"][0];
        c["securityContext"] = json!({
            "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true,
            "capabilities": {"drop": ["ALL"]}
        });
        c["env"] = json!([
            {"name": "POD", "valueFrom": {"fieldRef": {"fieldPath": "metadata.name"}}},
            {"name": "MODE", "valueFrom": {"configMapKeyRef": {"name": "settings", "key": "mode"}}}
        ]);
        c["ports"] = json!([{"containerPort": 8080, "name": "http"}]);
        c["readinessProbe"] = json!({"httpGet": {"path": "/healthz", "port": 8080}});
        c["volumeMounts"] = json!([{"name": "scratch", "mountPath": "/tmp"}]);
        assert!(normalize(m, &caller(), &queue()).is_ok());
    }

    #[test]
    fn reserved_labels_and_annotations_are_refused_on_the_object_and_its_templates() {
        let cases: Vec<(&str, &str, Value)> = vec![
            (
                "/metadata/labels",
                "kueue.x-k8s.io/priority-class",
                json!("high"),
            ),
            ("/metadata/labels", "crucible.dev/workload", json!("x")),
            (
                "/metadata/annotations",
                "crucible.dev/task-completed",
                json!("done"),
            ),
            (
                "/metadata/annotations",
                "crucible.dev/plan-hash",
                json!("abc"),
            ),
            (
                "/metadata/annotations",
                "kueue.x-k8s.io/podset-required-topology",
                json!("rack"),
            ),
            (
                "/spec/template/metadata/labels",
                "kueue.x-k8s.io/queue-name",
                json!("gpu"),
            ),
            (
                "/spec/template/metadata/labels",
                "kueue.x-k8s.io/pod-group-name",
                json!("g"),
            ),
            (
                "/spec/template/metadata/annotations",
                "openshell.ai/sandbox-id",
                json!("s"),
            ),
            (
                "/spec/template/metadata/labels",
                "workload.spiffe.io/x",
                json!("y"),
            ),
            (
                "/spec/template/metadata/labels",
                "evil.crucible.dev/run-id",
                json!("run-7"),
            ),
        ];
        for (at, key, value) in cases {
            let mut m = job();
            m["spec"]["template"]["metadata"] = json!({});
            let (parent, field) = at.rsplit_once('/').unwrap();
            m.pointer_mut(parent).unwrap()[field] = json!({ key: value });
            refused_with(m, key);
        }
        let mut ok = job();
        ok["metadata"]["labels"] = json!({"app.kubernetes.io/name": "train", "team": "a"});
        ok["spec"]["template"]["metadata"] = json!({"annotations": {"example.com/note": "x"}});
        assert!(normalize(ok, &caller(), &queue()).is_ok());
    }

    #[test]
    fn rayjob_paths_outside_its_pod_templates_cannot_escape_admission() {
        let ray = || {
            json!({
                "apiVersion": "ray.io/v1",
                "kind": "RayJob",
                "metadata": {"name": "r"},
                "spec": {"rayClusterSpec": {
                    "headGroupSpec": {"template": {"spec": {"containers": [{"name": "h"}]}}}
                }}
            })
        };
        let mut m = ray();
        m["spec"]["clusterSelector"] = json!({"ray.io/cluster": "someone-elses"});
        refused_with(m, "clusterSelector");
        let mut m = ray();
        m["spec"]["rayClusterSpec"]["autoscalerOptions"] =
            json!({"securityContext": {"privileged": true}});
        refused_with(m, "autoscalerOptions");
        let mut m = ray();
        m["spec"]["rayClusterSpec"]["enableInTreeAutoscaling"] = json!(true);
        refused_with(m, "enableInTreeAutoscaling");
        let mut m = ray();
        m["spec"]["rayClusterSpec"]["headGroupSpec"]["serviceType"] = json!("LoadBalancer");
        refused_with(m, "serviceType");
        let mut m = ray();
        m["spec"]["submitterPodTemplate"] =
            json!({"spec": {"containers": [{"name": "s"}], "hostNetwork": true}});
        refused_with(m, "hostNetwork");
        assert!(normalize(ray(), &caller(), &queue()).is_ok());
    }

    fn statefulset() -> Value {
        json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "metadata": {"name": "db"},
            "spec": {
                "selector": {"matchLabels": {"app": "db"}},
                "template": {"metadata": {"labels": {"app": "db"}}, "spec": {"containers": [{"name": "d"}]}},
                "volumeClaimTemplates": [{
                    "metadata": {"name": "data"},
                    "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}}
                }]
            }
        })
    }

    #[test]
    fn object_fields_outside_each_kinds_allowlist_are_refused() {
        let mut m = job();
        m["spec"]["managedBy"] = json!("kueue.x-k8s.io/multikueue");
        refused_with(m, "managedBy");
        let mut m = job();
        m["spec"]["manualSelector"] = json!(true);
        refused_with(m, "manualSelector");
        let mut m = job();
        m["extra"] = json!({});
        refused_with(m, "/extra");
        for (needle, field, value) in [
            ("storageClassName", "storageClassName", json!("fast-local")),
            (
                "dataSource",
                "dataSource",
                json!({"kind": "PersistentVolumeClaim", "name": "victim"}),
            ),
            (
                "dataSourceRef",
                "dataSourceRef",
                json!({"kind": "PersistentVolumeClaim", "name": "victim"}),
            ),
            ("volumeName", "volumeName", json!("pv-victim")),
            ("selector", "selector", json!({"matchLabels": {"app": "x"}})),
        ] {
            let mut m = statefulset();
            m["spec"]["volumeClaimTemplates"][0]["spec"][field] = value;
            refused_with(m, needle);
        }
        let mut m = statefulset();
        m["spec"]["volumeClaimTemplates"][0]["metadata"]["labels"] =
            json!({"crucible.dev/run-id": "other"});
        refused_with(m, "crucible.dev/run-id");
        let lws = json!({
            "apiVersion": "leaderworkerset.x-k8s.io/v1",
            "kind": "LeaderWorkerSet",
            "metadata": {"name": "l"},
            "spec": {"leaderWorkerTemplate": {
                "workerTemplate": {"spec": {"containers": [{"name": "w"}]}},
                "volumeClaimTemplates": [{"metadata": {"name": "c"}, "spec": {"dataSource": {"kind": "VolumeSnapshot", "name": "s"}}}]
            }}
        });
        refused_with(lws, "dataSource");
        let ray = |patch: Value| {
            let mut m = json!({
                "apiVersion": "ray.io/v1",
                "kind": "RayJob",
                "metadata": {"name": "r"},
                "spec": {"rayClusterSpec": {"headGroupSpec": {"template": {"spec": {"containers": [{"name": "h"}]}}}}}
            });
            for (ptr, v) in patch.as_object().unwrap() {
                let (parent, field) = ptr.rsplit_once('/').unwrap();
                m.pointer_mut(parent).unwrap()[field] = v.clone();
            }
            m
        };
        refused_with(ray(json!({"/spec/managedBy": "x"})), "managedBy");
        refused_with(
            ray(
                json!({"/spec/rayClusterSpec/headGroupSpec/headService": {"spec": {"type": "NodePort"}}}),
            ),
            "headService",
        );
        refused_with(
            ray(json!({"/spec/rayClusterSpec/headGroupSpec/enableIngress": true})),
            "enableIngress",
        );
        refused_with(
            ray(json!({"/spec/rayClusterSpec/gcsFaultToleranceOptions": {"redisAddress": "r"}})),
            "gcsFaultToleranceOptions",
        );
    }

    #[test]
    fn pod_volumes_cannot_mount_or_clone_existing_claims() {
        let mut m = job();
        m["spec"]["template"]["spec"]["volumes"] =
            json!([{"name": "v", "persistentVolumeClaim": {"claimName": "another-runs-data"}}]);
        refused_with(m, "persistentVolumeClaim");
        let mut m = job();
        m["spec"]["template"]["spec"]["volumes"] = json!([{"name": "v", "ephemeral": {"volumeClaimTemplate": {
            "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": "x", "resources": {"requests": {"storage": "1Gi"}}}
        }}}]);
        refused_with(m, "storageClassName");
        let mut m = job();
        m["spec"]["template"]["spec"]["volumes"] = json!([{"name": "v", "ephemeral": {"volumeClaimTemplate": {
            "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}}
        }}}]);
        assert!(normalize(m, &caller(), &queue()).is_ok());
    }

    #[test]
    fn deployment_and_statefulset_selectors_are_pinned_to_the_server_labels() {
        let n = normalize(statefulset(), &caller(), &queue()).unwrap();
        let sel = n.object.pointer("/spec/selector/matchLabels").unwrap();
        assert_eq!(sel["app"], "db");
        assert_eq!(sel["crucible.dev/workload"], "db");
        assert_eq!(sel["crucible.dev/run-id"], "run-7");
        let mut m = statefulset();
        m["spec"]["selector"]["matchLabels"]["crucible.dev/run-id"] = json!("another-run");
        assert!(matches!(
            normalize(m, &caller(), &queue()),
            Err(ManifestError::LabelConflict { .. })
        ));
        let mut m = statefulset();
        m["spec"]["selector"]["matchFields"] = json!([]);
        refused_with(m, "matchFields");
    }
}
