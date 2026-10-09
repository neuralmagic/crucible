//! Read models for the `kueue.x-k8s.io` objects the tools report on, at `v1beta2` or `v1beta1`
//! (whichever the cluster serves), and the quota-fit estimate. Only the fields the tools use are modeled; unknown fields are ignored so a newer
//! Kueue does not break deserialization.

use kube::api::{Api, DynamicObject, ListParams};
use kube::core::ApiResource;
use kube_quantity::ParsedQuantity;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const GROUP: &str = "kueue.x-k8s.io";

/// The Kueue API version a cluster serves. Kueue 0.15 introduced `v1beta2`; older releases serve
/// only `v1beta1`, whose read models differ in field names the models alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KueueVersion {
    V1beta1,
    #[default]
    V1beta2,
}

#[derive(Debug, Deserialize)]
struct ApiGroup {
    versions: Vec<GroupVersion>,
}

#[derive(Debug, Deserialize)]
struct GroupVersion {
    version: String,
}

impl KueueVersion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1beta1 => "v1beta1",
            Self::V1beta2 => "v1beta2",
        }
    }

    /// The newest version this server understands among those the cluster serves.
    pub fn pick(served: &[String]) -> Option<Self> {
        [Self::V1beta2, Self::V1beta1]
            .into_iter()
            .find(|v| served.iter().any(|s| s == v.as_str()))
    }

    pub async fn discover(client: &kube::Client) -> Result<Self, KubeError> {
        let what = format!("API group {GROUP}");
        let req = http::Request::get(format!("/apis/{GROUP}"))
            .body(Vec::new())
            .map_err(|e| KubeError::api("get", what.clone(), kube::Error::HttpError(e)))?;
        let group: ApiGroup = client
            .request(req)
            .await
            .map_err(|source| KubeError::api("get", what, source))?;
        let served: Vec<String> = group.versions.into_iter().map(|v| v.version).collect();
        Self::pick(&served).ok_or(KubeError::UnsupportedVersion { served })
    }

    pub fn resource(self, kind: &str, plural: &str) -> ApiResource {
        ApiResource {
            group: GROUP.into(),
            version: self.as_str().into(),
            api_version: format!("{GROUP}/{}", self.as_str()),
            kind: kind.into(),
            plural: plural.into(),
        }
    }

    pub fn local_queues(self) -> ApiResource {
        self.resource("LocalQueue", "localqueues")
    }
    pub fn cluster_queues(self) -> ApiResource {
        self.resource("ClusterQueue", "clusterqueues")
    }
    pub fn resource_flavors(self) -> ApiResource {
        self.resource("ResourceFlavor", "resourceflavors")
    }
    pub fn workloads(self) -> ApiResource {
        self.resource("Workload", "workloads")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KubeError {
    #[error("{verb} {what}: {source}")]
    Api {
        verb: &'static str,
        what: String,
        #[source]
        source: Box<kube::Error>,
    },
    #[error("the cluster serves {GROUP} {served:?}; this server understands v1beta2 and v1beta1")]
    UnsupportedVersion { served: Vec<String> },
    #[error("decoding {what}: {source}")]
    Decode {
        what: String,
        #[source]
        source: serde_json::Error,
    },
}

impl KubeError {
    pub fn api(verb: &'static str, what: impl Into<String>, source: kube::Error) -> Self {
        Self::Api {
            verb,
            what: what.into(),
            source: Box::new(source),
        }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Api { source, .. } if matches!(source.as_ref(), kube::Error::Api(s) if s.code == 404))
    }
}

fn decode<T: DeserializeOwned>(obj: DynamicObject, what: &str) -> Result<T, KubeError> {
    serde_json::to_value(obj)
        .and_then(serde_json::from_value)
        .map_err(|source| KubeError::Decode {
            what: what.to_string(),
            source,
        })
}

pub async fn get<T: DeserializeOwned>(
    client: &kube::Client,
    ar: &ApiResource,
    namespace: Option<&str>,
    name: &str,
) -> Result<T, KubeError> {
    let api: Api<DynamicObject> = match namespace {
        Some(ns) => Api::namespaced_with(client.clone(), ns, ar),
        None => Api::all_with(client.clone(), ar),
    };
    let what = format!("{} {name}", ar.kind);
    let obj = api
        .get(name)
        .await
        .map_err(|source| KubeError::api("get", what.clone(), source))?;
    decode(obj, &what)
}

pub async fn list<T: DeserializeOwned>(
    client: &kube::Client,
    ar: &ApiResource,
    namespace: Option<&str>,
    params: &ListParams,
) -> Result<Vec<T>, KubeError> {
    let api: Api<DynamicObject> = match namespace {
        Some(ns) => Api::namespaced_with(client.clone(), ns, ar),
        None => Api::all_with(client.clone(), ar),
    };
    let what = format!("{} list", ar.kind);
    let objs = api
        .list(params)
        .await
        .map_err(|source| KubeError::api("list", what.clone(), source))?;
    objs.items.into_iter().map(|o| decode(o, &what)).collect()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Meta {
    pub name: String,
    pub namespace: Option<String>,
    pub uid: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub owner_references: Vec<OwnerRef>,
    pub creation_timestamp: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OwnerRef {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub uid: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub last_transition_time: Option<String>,
}

pub fn condition<'a>(conditions: &'a [Condition], type_: &str) -> Option<&'a Condition> {
    conditions.iter().find(|c| c.type_ == type_)
}

pub fn is_true(conditions: &[Condition], type_: &str) -> bool {
    condition(conditions, type_).is_some_and(|c| c.status == "True")
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LocalQueue {
    pub metadata: Meta,
    pub spec: LocalQueueSpec,
    pub status: LocalQueueStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LocalQueueSpec {
    pub cluster_queue: String,
    pub stop_policy: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LocalQueueStatus {
    pub pending_workloads: i64,
    pub reserving_workloads: i64,
    pub admitted_workloads: i64,
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClusterQueue {
    pub metadata: Meta,
    pub spec: ClusterQueueSpec,
    pub status: ClusterQueueStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClusterQueueSpec {
    #[serde(alias = "cohort")]
    pub cohort_name: Option<String>,
    pub queueing_strategy: Option<String>,
    pub stop_policy: Option<String>,
    pub namespace_selector: Value,
    pub preemption: Value,
    pub fair_sharing: Value,
    pub admission_checks_strategy: Value,
    pub resource_groups: Vec<ResourceGroup>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResourceGroup {
    pub covered_resources: Vec<String>,
    pub flavors: Vec<FlavorQuotas>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FlavorQuotas {
    pub name: String,
    pub resources: Vec<ResourceQuota>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResourceQuota {
    pub name: String,
    pub nominal_quota: Value,
    pub borrowing_limit: Option<Value>,
    pub lending_limit: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClusterQueueStatus {
    pub pending_workloads: i64,
    pub reserving_workloads: i64,
    pub admitted_workloads: i64,
    pub flavors_usage: Vec<FlavorUsage>,
    pub flavors_reservation: Vec<FlavorUsage>,
    pub conditions: Vec<Condition>,
    pub fair_sharing: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FlavorUsage {
    pub name: String,
    pub resources: Vec<ResourceUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResourceUsage {
    pub name: String,
    pub total: Value,
    pub borrowed: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResourceFlavor {
    pub metadata: Meta,
    pub spec: ResourceFlavorSpec,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResourceFlavorSpec {
    pub node_labels: BTreeMap<String, String>,
    pub node_taints: Vec<Value>,
    pub tolerations: Vec<Value>,
    pub topology_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Workload {
    pub metadata: Meta,
    pub spec: WorkloadSpec,
    pub status: WorkloadStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkloadSpec {
    pub queue_name: String,
    pub active: Option<bool>,
    pub priority: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkloadStatus {
    pub conditions: Vec<Condition>,
    pub admission: Option<Value>,
    pub admission_checks: Vec<Value>,
    pub requeue_state: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PendingWorkload {
    pub metadata: Meta,
    pub priority: i64,
    pub local_queue_name: String,
    pub position_in_cluster_queue: i64,
    pub position_in_local_queue: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct PendingWorkloadsSummary {
    items: Vec<PendingWorkload>,
}

/// Queue order from Kueue's visibility API for one LocalQueue.
pub async fn pending_in_local_queue(
    client: &kube::Client,
    version: KueueVersion,
    namespace: &str,
    queue: &str,
    limit: u32,
) -> Result<Vec<PendingWorkload>, KubeError> {
    let path = format!(
        "/apis/visibility.{GROUP}/{}/namespaces/{namespace}/localqueues/{queue}/pendingworkloads?limit={limit}",
        version.as_str()
    );
    let what = format!("pending workloads of LocalQueue {namespace}/{queue}");
    let req = http::Request::get(path)
        .body(Vec::new())
        .map_err(|e| KubeError::api("get", what.clone(), kube::Error::HttpError(e)))?;
    let summary: PendingWorkloadsSummary = client
        .request(req)
        .await
        .map_err(|source| KubeError::api("get", what, source))?;
    Ok(summary.items)
}

pub fn quantity(v: &Value) -> Option<ParsedQuantity> {
    match v {
        Value::String(s) => ParsedQuantity::try_from(s.as_str()).ok(),
        Value::Number(n) => ParsedQuantity::try_from(n.to_string().as_str()).ok(),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Some flavor has free nominal quota for every requested resource right now.
    FitsNow,
    /// Within nominal quota, but current usage means it waits in the queue.
    WaitsForQuota,
    /// No flavor's nominal quota (plus borrowing limit) can ever hold it; it would stay pending forever.
    NeverFits,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceFit {
    pub resource: String,
    pub requested: String,
    pub nominal: String,
    pub used: String,
    pub free: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupFit {
    pub covered_resources: Vec<String>,
    pub flavor: Option<String>,
    pub verdict: Verdict,
    pub resources: Vec<ResourceFit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FitReport {
    pub cluster_queue: String,
    pub verdict: Verdict,
    pub uncovered_resources: Vec<String>,
    pub groups: Vec<GroupFit>,
}

/// Estimates admission from the workload's summed requests. Kueue assigns flavors per pod set, so
/// this can call `fits_now` for a workload whose pod sets must split across flavors; it never
/// calls `never_fits` for one Kueue could admit without borrowing from a cohort.
pub fn fit(requests: &BTreeMap<String, ParsedQuantity>, cq: &ClusterQueue) -> FitReport {
    let zero = crate::quantity::zero();
    let requested: BTreeMap<&String, &ParsedQuantity> =
        requests.iter().filter(|(_, q)| **q > zero).collect();
    let usage: BTreeMap<(&str, &str), ParsedQuantity> = cq
        .status
        .flavors_usage
        .iter()
        .flat_map(|f| {
            f.resources.iter().map(move |r| {
                (
                    (f.name.as_str(), r.name.as_str()),
                    quantity(&r.total).unwrap_or_else(crate::quantity::zero),
                )
            })
        })
        .collect();
    let mut covered: Vec<&str> = Vec::new();
    let mut groups = Vec::new();
    for group in &cq.spec.resource_groups {
        covered.extend(group.covered_resources.iter().map(String::as_str));
        let wanted: Vec<(&String, &ParsedQuantity)> = requested
            .iter()
            .filter(|(r, _)| group.covered_resources.contains(r))
            .map(|(r, q)| (*r, *q))
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let mut best: Option<GroupFit> = None;
        for flavor in &group.flavors {
            let mut verdict = Verdict::FitsNow;
            let mut rows = Vec::new();
            for (res, req) in &wanted {
                let quota = flavor.resources.iter().find(|q| &q.name == *res);
                let nominal = quota
                    .and_then(|q| quantity(&q.nominal_quota))
                    .unwrap_or_else(crate::quantity::zero);
                let used = usage
                    .get(&(flavor.name.as_str(), res.as_str()))
                    .cloned()
                    .unwrap_or_else(crate::quantity::zero);
                let free = nominal.clone() - used.clone();
                let cap = match (
                    cq.spec.cohort_name.as_ref(),
                    quota.and_then(|q| q.borrowing_limit.as_ref()),
                ) {
                    (Some(_), Some(limit)) => {
                        nominal.clone() + quantity(limit).unwrap_or_else(crate::quantity::zero)
                    }
                    _ => nominal.clone(),
                };
                let row_verdict = if **req <= free {
                    Verdict::FitsNow
                } else if **req <= cap {
                    Verdict::WaitsForQuota
                } else {
                    Verdict::NeverFits
                };
                verdict = worse(verdict, row_verdict);
                rows.push(ResourceFit {
                    resource: (*res).clone(),
                    requested: req.to_string(),
                    nominal: nominal.to_string(),
                    used: used.to_string(),
                    free: free.to_string(),
                });
            }
            let candidate = GroupFit {
                covered_resources: group.covered_resources.clone(),
                flavor: Some(flavor.name.clone()),
                verdict,
                resources: rows,
            };
            if best
                .as_ref()
                .is_none_or(|b| rank(candidate.verdict) < rank(b.verdict))
            {
                best = Some(candidate);
            }
            if verdict == Verdict::FitsNow {
                break;
            }
        }
        groups.push(best.unwrap_or(GroupFit {
            covered_resources: group.covered_resources.clone(),
            flavor: None,
            verdict: Verdict::NeverFits,
            resources: Vec::new(),
        }));
    }
    let uncovered_resources: Vec<String> = requested
        .keys()
        .filter(|r| !covered.contains(&r.as_str()))
        .map(|r| (*r).clone())
        .collect();
    let mut verdict = groups
        .iter()
        .map(|g| g.verdict)
        .fold(Verdict::FitsNow, worse);
    if !uncovered_resources.is_empty() {
        verdict = Verdict::NeverFits;
    }
    FitReport {
        cluster_queue: cq.metadata.name.clone(),
        verdict,
        uncovered_resources,
        groups,
    }
}

fn rank(v: Verdict) -> u8 {
    match v {
        Verdict::FitsNow => 0,
        Verdict::WaitsForQuota => 1,
        Verdict::NeverFits => 2,
    }
}

fn worse(a: Verdict, b: Verdict) -> Verdict {
    if rank(b) > rank(a) { b } else { a }
}

#[cfg(test)]
mod tests {
    use crate::kueue::*;
    use serde_json::json;

    fn cq() -> ClusterQueue {
        serde_json::from_value(json!({
            "metadata": {"name": "crucible"},
            "spec": {"resourceGroups": [
                {"coveredResources": ["cpu", "memory"], "flavors": [
                    {"name": "default-flavor", "resources": [
                        {"name": "cpu", "nominalQuota": "10"},
                        {"name": "memory", "nominalQuota": "64Gi"}
                    ]}
                ]},
                {"coveredResources": ["nvidia.com/gpu"], "flavors": [
                    {"name": "a100", "resources": [{"name": "nvidia.com/gpu", "nominalQuota": 8}]},
                    {"name": "b200", "resources": [{"name": "nvidia.com/gpu", "nominalQuota": "16"}]}
                ]}
            ]},
            "status": {"flavorsUsage": [
                {"name": "default-flavor", "resources": [{"name": "cpu", "total": "9"}, {"name": "memory", "total": "0"}]},
                {"name": "a100", "resources": [{"name": "nvidia.com/gpu", "total": "6"}]},
                {"name": "b200", "resources": [{"name": "nvidia.com/gpu", "total": "0"}]}
            ]}
        }))
        .unwrap()
    }

    fn req(pairs: &[(&str, &str)]) -> BTreeMap<String, ParsedQuantity> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), ParsedQuantity::try_from(*v).unwrap()))
            .collect()
    }

    #[test]
    fn a_request_inside_free_quota_fits_now() {
        let r = fit(&req(&[("cpu", "500m"), ("memory", "1Gi")]), &cq());
        assert_eq!(r.verdict, Verdict::FitsNow);
        assert_eq!(r.groups[0].flavor.as_deref(), Some("default-flavor"));
    }

    #[test]
    fn a_request_over_free_but_under_nominal_waits() {
        let r = fit(&req(&[("cpu", "2")]), &cq());
        assert_eq!(r.verdict, Verdict::WaitsForQuota);
    }

    #[test]
    fn flavors_are_tried_in_order_and_the_first_with_room_wins() {
        let r = fit(&req(&[("nvidia.com/gpu", "4")]), &cq());
        assert_eq!(r.verdict, Verdict::FitsNow);
        assert_eq!(r.groups[0].flavor.as_deref(), Some("b200"));
        let r = fit(&req(&[("nvidia.com/gpu", "2")]), &cq());
        assert_eq!(r.groups[0].flavor.as_deref(), Some("a100"));
    }

    #[test]
    fn a_request_larger_than_any_flavor_never_fits() {
        let r = fit(&req(&[("nvidia.com/gpu", "17")]), &cq());
        assert_eq!(r.verdict, Verdict::NeverFits);
    }

    #[test]
    fn a_resource_the_cluster_queue_does_not_cover_never_fits() {
        let r = fit(&req(&[("cpu", "1"), ("example.com/fpga", "1")]), &cq());
        assert_eq!(r.verdict, Verdict::NeverFits);
        assert_eq!(r.uncovered_resources, vec!["example.com/fpga".to_string()]);
    }

    #[test]
    fn zero_requests_do_not_count_as_uncovered() {
        let r = fit(&req(&[("cpu", "1"), ("example.com/fpga", "0")]), &cq());
        assert_eq!(r.verdict, Verdict::FitsNow);
    }

    #[test]
    fn borrowing_limits_only_count_inside_a_cohort() {
        let mut q = cq();
        q.spec.resource_groups[0].flavors[0].resources[0].borrowing_limit = Some(json!("10"));
        assert_eq!(fit(&req(&[("cpu", "15")]), &q).verdict, Verdict::NeverFits);
        q.spec.cohort_name = Some("pool".into());
        assert_eq!(
            fit(&req(&[("cpu", "15")]), &q).verdict,
            Verdict::WaitsForQuota
        );
    }

    #[test]
    fn conditions_are_looked_up_by_type() {
        let c = vec![Condition {
            type_: "Admitted".into(),
            status: "True".into(),
            ..Default::default()
        }];
        assert!(is_true(&c, "Admitted"));
        assert!(!is_true(&c, "Finished"));
    }

    #[test]
    fn the_newest_understood_kueue_version_is_picked() {
        let served = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            KueueVersion::pick(&served(&["v1beta2", "v1beta1"])),
            Some(KueueVersion::V1beta2)
        );
        assert_eq!(
            KueueVersion::pick(&served(&["v1beta1", "v1alpha1"])),
            Some(KueueVersion::V1beta1)
        );
        assert_eq!(KueueVersion::pick(&served(&["v1alpha1"])), None);
        assert_eq!(
            KueueVersion::V1beta1.workloads().api_version,
            "kueue.x-k8s.io/v1beta1"
        );
    }

    #[test]
    fn a_v1beta1_cluster_queue_reads_its_cohort() {
        let q: ClusterQueue = serde_json::from_value(json!({
            "metadata": {"name": "q"},
            "spec": {"cohort": "pool", "resourceGroups": []}
        }))
        .unwrap();
        assert_eq!(q.spec.cohort_name.as_deref(), Some("pool"));
    }
}
