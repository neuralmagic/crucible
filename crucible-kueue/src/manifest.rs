//! Admission of agent-supplied workload manifests. A manifest is accepted only for an allowlisted
//! kind, is pinned to the caller's namespace and queue, and is refused outright when any pod
//! template asks for host access, privilege, or an identity other than the namespace default.

use crate::tenant::{Caller, QueueName};
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
    if crate::tenant::Namespace::parse(&name).is_err() {
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
    let labels = object_entry(meta, "labels", "metadata.labels")?;
    set_label(labels, QUEUE_LABEL, queue.as_str())?;
    set_label(labels, MANAGED_LABEL, MANAGED_VALUE)?;
    set_label(labels, RUN_LABEL, caller.run_id.as_str())?;

    if kind == WorkloadKind::Job {
        object_entry(&mut obj, "spec", "spec")?.insert("suspend".into(), Value::Bool(true));
    }

    let mut root = Value::Object(obj);
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

fn check_pod_spec(spec: &Map<String, Value>, path: &str) -> Result<(), ManifestError> {
    let forbid = |what: &'static str, field: &str| ManifestError::Forbidden {
        path: format!("{path}/{field}"),
        what,
    };
    for field in ["serviceAccountName", "serviceAccount"] {
        if spec.get(field).is_some_and(|v| !v.is_null()) {
            return Err(forbid("a non-default service account", field));
        }
    }
    if spec.get("automountServiceAccountToken") == Some(&Value::Bool(true)) {
        return Err(forbid(
            "mounting the service account token",
            "automountServiceAccountToken",
        ));
    }
    for field in ["hostNetwork", "hostPID", "hostIPC"] {
        if spec.get(field) == Some(&Value::Bool(true)) {
            return Err(forbid("host namespaces", field));
        }
    }
    if spec.get("nodeName").is_some_and(|v| !v.is_null()) {
        return Err(forbid("pinning to a node (it bypasses Kueue)", "nodeName"));
    }
    if let Some(Value::Array(volumes)) = spec.get("volumes") {
        for (i, vol) in volumes.iter().enumerate() {
            if vol.get("hostPath").is_some() {
                return Err(forbid("hostPath volumes", &format!("volumes/{i}/hostPath")));
            }
        }
    }
    for list in ["containers", "initContainers", "ephemeralContainers"] {
        let Some(Value::Array(containers)) = spec.get(list) else {
            continue;
        };
        for (i, c) in containers.iter().enumerate() {
            let sc = format!("{list}/{i}/securityContext");
            if c.pointer("/securityContext/privileged") == Some(&Value::Bool(true)) {
                return Err(forbid("privileged containers", &format!("{sc}/privileged")));
            }
            if c.pointer("/securityContext/allowPrivilegeEscalation") == Some(&Value::Bool(true)) {
                return Err(forbid(
                    "privilege escalation",
                    &format!("{sc}/allowPrivilegeEscalation"),
                ));
            }
            if c.pointer("/securityContext/capabilities/add")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty())
            {
                return Err(forbid(
                    "added capabilities",
                    &format!("{sc}/capabilities/add"),
                ));
            }
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
    use crate::manifest::*;
    use crate::tenant::{Namespace, QueueName, RunId};
    use crucible_contract::kueue::TenantRole;
    use serde_json::json;

    fn caller() -> Caller {
        Caller {
            run_id: RunId::parse("run-7").unwrap(),
            namespace: Namespace::parse("team-a").unwrap(),
            role: TenantRole::Submitter,
            queues: vec![],
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
}
