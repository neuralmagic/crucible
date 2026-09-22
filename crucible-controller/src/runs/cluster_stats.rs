//! Per-cluster GPU utilization snapshots for the dispatch-visibility surface.
//!
//! Base utilization is allocation math over the kube API, not DCGM: node-allocatable
//! `nvidia.com/gpu` versus the summed requests of scheduled, non-terminal pods, grouped into
//! pools by the node's GPU product label. Kueue only sees queue-admitted work; serving rigs and
//! dev pods hold most of the GPUs without ever passing through a queue, so the node+pod sweep is
//! the primary busy signal.
//!
//! Snapshots are fetched on demand behind a TTL cache and degrade per cluster: a fetch failure
//! serves the last good snapshot marked unreachable (with its age) rather than erroring the
//! whole endpoint, and one slow spoke never blocks the others (concurrent fan-out, bounded
//! per-cluster timeout).

use crate::runs::clusters::ClusterClients;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ApiResource, DynamicObject, ListParams};
use kube::core::GroupVersionKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The extended resource name GPUs are scheduled under.
const GPU_RESOURCE: &str = "nvidia.com/gpu";

/// Node label naming the GPU model (set by the NVIDIA GPU operator's node feature discovery).
/// Nodes without it still count, under [`DEFAULT_POOL`].
const PRODUCT_LABEL: &str = "nvidia.com/gpu.product";

/// Pool name for GPU nodes that carry no product label.
const DEFAULT_POOL: &str = "gpu";

/// How long a snapshot stays fresh before the next read re-fetches.
const SNAPSHOT_TTL: Duration = Duration::from_secs(30);

/// Per-cluster budget for the two list calls; a spoke slower than this serves its stale
/// snapshot instead of stalling the fan-out.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// A pushed snapshot older than this reports `reachable: false` — the pusher missed enough
/// cycles that the numbers can no longer be treated as current.
const PUSH_STALE: Duration = Duration::from_secs(180);

/// Priority classes whose pods never count as held capacity
/// (`CONTROLLER_STATS_EXCLUDE_PRIORITY_CLASSES`, comma-separated). Default: the GPU cloud's
/// idle-soak verification workload, which fills otherwise-idle GPUs at a preemptible priority —
/// those GPUs are available to a real dispatch, so counting them misreports every idle cluster
/// as busy.
fn excluded_priority_classes() -> std::collections::HashSet<String> {
    std::env::var("CONTROLLER_STATS_EXCLUDE_PRIORITY_CLASSES")
        .unwrap_or_else(|_| "cw-hpc-verification".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// One GPU pool on one cluster: how many GPUs its nodes offer and how many are held by
/// scheduled, non-terminal pods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GpuPool {
    pub pool: String,
    pub allocatable: u64,
    pub requested: u64,
}

/// One Kueue ClusterQueue's pressure, reduced to the dispatch-relevant numbers: how much GPU
/// quota it declares, how much is reserved by admitted workloads, and what is waiting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct KueueQueue {
    pub queue: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cohort: Option<String>,
    pub pending: u64,
    pub admitted: u64,
    pub gpu_nominal: u64,
    pub gpu_reserved: u64,
}

/// One cluster's utilization snapshot. `reachable: false` means the numbers are the last good
/// fetch (`age_secs` old) or, when there never was one, empty — `error` says why. `kueue` is
/// `None` on clusters without the Kueue CRDs.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ClusterSnapshot {
    pub cluster: String,
    pub reachable: bool,
    /// Seconds since the pools were actually fetched (0 for a fresh fetch).
    pub age_secs: u64,
    pub pools: Vec<GpuPool>,
    pub kueue: Option<Vec<KueueQueue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The TTL cache over per-cluster fetches, plus the store of pushed snapshots from clusters the
/// hub cannot reach (corp-internal spokes push through [`crate::runs::cluster_push`]). Shared via
/// [`Arc`]; one entry per cluster name.
pub struct ClusterStats {
    clients: Arc<ClusterClients>,
    cache: tokio::sync::Mutex<HashMap<String, CachedFetch>>,
    pushed: tokio::sync::Mutex<HashMap<String, PushedSnapshot>>,
}

/// One pushed snapshot and when the hub received it.
struct PushedSnapshot {
    at: Instant,
    snapshot: ClusterSnapshot,
}

/// What one successful fetch produced.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub pools: Vec<GpuPool>,
    pub kueue: Option<Vec<KueueQueue>>,
}

/// A completed fetch and when it happened; kept as the stale fallback after later failures.
struct CachedFetch {
    at: Instant,
    fetched: Fetched,
}

impl ClusterStats {
    pub fn new(clients: Arc<ClusterClients>) -> Self {
        Self {
            clients,
            cache: tokio::sync::Mutex::new(HashMap::new()),
            pushed: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Snapshots for every named cluster (fetched concurrently), then every pushed cluster the
    /// name list doesn't already cover, sorted by name. Never errors: each cluster degrades
    /// independently ([`ClusterSnapshot::reachable`]).
    pub async fn snapshots(&self, clusters: &[String]) -> Vec<ClusterSnapshot> {
        let mut out =
            futures_util::future::join_all(clusters.iter().map(|c| self.snapshot(c))).await;
        let pushed = self.pushed.lock().await;
        let mut extra: Vec<ClusterSnapshot> = pushed
            .iter()
            .filter(|(name, _)| !clusters.contains(name))
            .map(|(_, p)| render_pushed(p, Instant::now()))
            .collect();
        extra.sort_by(|a, b| a.cluster.cmp(&b.cluster));
        out.extend(extra);
        out
    }

    /// Record a snapshot pushed by a spoke-side agent. `cluster` comes from the authenticated
    /// push identity, never the body — a pusher cannot claim another cluster's card.
    pub async fn record_push(&self, cluster: &str, mut snapshot: ClusterSnapshot) {
        snapshot.cluster = cluster.to_string();
        snapshot.reachable = true;
        snapshot.age_secs = 0;
        snapshot.error = None;
        self.pushed.lock().await.insert(
            cluster.to_string(),
            PushedSnapshot {
                at: Instant::now(),
                snapshot,
            },
        );
    }

    /// One cluster's snapshot: the cached pools when fresh, else a bounded re-fetch that falls
    /// back to the stale entry on failure.
    pub async fn snapshot(&self, cluster: &str) -> ClusterSnapshot {
        if let Some(cached) = self.fresh(cluster).await {
            return cached;
        }
        let fetched = tokio::time::timeout(FETCH_TIMEOUT, self.fetch(cluster)).await;
        let outcome = match fetched {
            Ok(Ok(fetched)) => Ok(fetched),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err(format!(
                "timed out after {}s listing nodes/pods",
                FETCH_TIMEOUT.as_secs()
            )),
        };
        let mut cache = self.cache.lock().await;
        apply_fetch(&mut cache, cluster, outcome, Instant::now())
    }

    /// The cached snapshot, only while it is younger than [`SNAPSHOT_TTL`].
    async fn fresh(&self, cluster: &str) -> Option<ClusterSnapshot> {
        let cache = self.cache.lock().await;
        let cached = cache.get(cluster)?;
        let age = cached.at.elapsed();
        (age < SNAPSHOT_TTL).then(|| ClusterSnapshot {
            cluster: cluster.to_string(),
            reachable: true,
            age_secs: age.as_secs(),
            pools: cached.fetched.pools.clone(),
            kueue: cached.fetched.kueue.clone(),
            error: None,
        })
    }

    /// The list calls against one cluster (see [`gather`]).
    async fn fetch(&self, cluster: &str) -> anyhow::Result<Fetched> {
        let client = self.clients.client(cluster).await?;
        gather(client, cluster).await
    }
}

/// A pushed snapshot as served: fresh pushes read as reachable with their receive-age; one older
/// than [`PUSH_STALE`] grays out exactly like a dead pulled spoke.
fn render_pushed(p: &PushedSnapshot, now: Instant) -> ClusterSnapshot {
    let age = now.saturating_duration_since(p.at);
    let mut snap = p.snapshot.clone();
    snap.age_secs = age.as_secs();
    if age > PUSH_STALE {
        snap.reachable = false;
        snap.error = Some(format!("no push received for {}s", age.as_secs()));
    }
    snap
}

/// The list calls against one cluster: nodes + pods reduced to pools, ClusterQueues reduced
/// to queue pressure. A node/pod failure fails the gather; a Kueue failure degrades to
/// `kueue: None` — visibility of the base GPUs must not depend on an optional CRD. Public for
/// the spoke-side push agent (`crucible-controller cluster-snapshot`), which runs it against
/// its own cluster's ambient identity.
pub async fn gather(client: kube::Client, cluster: &str) -> anyhow::Result<Fetched> {
    let nodes = Api::<Node>::all(client.clone())
        .list(&ListParams::default())
        .await?
        .items;
    let pods = Api::<Pod>::all(client.clone())
        .list(&ListParams::default())
        .await?
        .items;
    let kueue = fetch_cluster_queues(client, cluster).await;
    Ok(Fetched {
        pools: aggregate_pools(&nodes, &pods),
        kueue,
    })
}

/// List Kueue ClusterQueues through the dynamic API (no typed CRD dependency). `None` when the
/// CRD is not installed (404); any other failure also degrades to `None`, logged — a broken
/// Kueue apiserver must not take the whole snapshot down with it.
async fn fetch_cluster_queues(client: kube::Client, cluster: &str) -> Option<Vec<KueueQueue>> {
    let gvk = GroupVersionKind::gvk("kueue.x-k8s.io", "v1beta1", "ClusterQueue");
    let api: Api<DynamicObject> = Api::all_with(client, &ApiResource::from_gvk(&gvk));
    match api.list(&ListParams::default()).await {
        Ok(list) => Some(parse_cluster_queues(&list.items)),
        Err(kube::Error::Api(e)) if e.code == 404 => None,
        Err(e) => {
            tracing::warn!(cluster, error = %e, "listing Kueue ClusterQueues failed; snapshot proceeds without kueue");
            None
        }
    }
}

/// Reduce ClusterQueue objects to [`KueueQueue`] rows. GPU nominal comes from the spec's
/// resource groups, GPU reserved from `status.flavorsReservation` (what admitted workloads
/// hold, the counterpart of the nominal quota); both sum the `nvidia.com/gpu` entries across
/// flavors. A queue with no status yet reads as zeros, never an error.
fn parse_cluster_queues(items: &[DynamicObject]) -> Vec<KueueQueue> {
    items
        .iter()
        .map(|cq| {
            let spec = cq.data.get("spec");
            let status = cq.data.get("status");
            KueueQueue {
                queue: cq.metadata.name.clone().unwrap_or_default(),
                cohort: spec
                    .and_then(|s| s.get("cohort"))
                    .and_then(|c| c.as_str())
                    .map(str::to_string),
                pending: count_field(status, "pendingWorkloads"),
                admitted: count_field(status, "admittedWorkloads"),
                gpu_nominal: sum_gpu(
                    spec.and_then(|s| s.get("resourceGroups")),
                    |flavor| flavor.get("resources"),
                    "nominalQuota",
                ),
                gpu_reserved: sum_gpu(
                    status.and_then(|s| s.get("flavorsReservation")),
                    |flavor| flavor.get("resources"),
                    "total",
                ),
            }
        })
        .collect()
}

/// A `u64` counter off an optional JSON object, defaulting to 0.
fn count_field(obj: Option<&serde_json::Value>, field: &str) -> u64 {
    obj.and_then(|o| o.get(field))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// Sum the `nvidia.com/gpu` quantities under `groups`: the spec shape nests flavors inside
/// resource groups; the status shape is a flat flavor list — `flavor_resources` bridges both,
/// and `amount_field` names the per-resource quantity (`nominalQuota` vs `total`).
fn sum_gpu<'a>(
    groups: Option<&'a serde_json::Value>,
    flavor_resources: impl Fn(&'a serde_json::Value) -> Option<&'a serde_json::Value>,
    amount_field: &str,
) -> u64 {
    let Some(groups) = groups.and_then(|g| g.as_array()) else {
        return 0;
    };
    // Spec entries are groups holding a `flavors` array; status entries ARE the flavors.
    let flavors = groups.iter().flat_map(|g| match g.get("flavors") {
        Some(f) => f.as_array().map(|a| a.as_slice()).unwrap_or(&[]).iter(),
        None => std::slice::from_ref(g).iter(),
    });
    flavors
        .filter_map(&flavor_resources)
        .filter_map(|r| r.as_array())
        .flatten()
        .filter(|r| r.get("name").and_then(|n| n.as_str()) == Some(GPU_RESOURCE))
        .filter_map(|r| r.get(amount_field))
        .filter_map(quantity_u64)
        .sum()
}

/// A whole-number quantity that Kueue may serialize as either a JSON number or a string
/// (`32` vs `"32"`).
fn quantity_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

/// Fold one fetch outcome into the cache and render the snapshot: success replaces the entry;
/// failure serves the stale entry (marked unreachable, with its age) or an empty snapshot when
/// the cluster never answered at all.
fn apply_fetch(
    cache: &mut HashMap<String, CachedFetch>,
    cluster: &str,
    outcome: Result<Fetched, String>,
    now: Instant,
) -> ClusterSnapshot {
    match outcome {
        Ok(fetched) => {
            cache.insert(
                cluster.to_string(),
                CachedFetch {
                    at: now,
                    fetched: fetched.clone(),
                },
            );
            ClusterSnapshot {
                cluster: cluster.to_string(),
                reachable: true,
                age_secs: 0,
                pools: fetched.pools,
                kueue: fetched.kueue,
                error: None,
            }
        }
        Err(error) => {
            let (age_secs, pools, kueue) = cache
                .get(cluster)
                .map(|c| {
                    (
                        now.saturating_duration_since(c.at).as_secs(),
                        c.fetched.pools.clone(),
                        c.fetched.kueue.clone(),
                    )
                })
                .unwrap_or((0, Vec::new(), None));
            ClusterSnapshot {
                cluster: cluster.to_string(),
                reachable: false,
                age_secs,
                pools,
                kueue,
                error: Some(error),
            }
        }
    }
}

/// Reduce node and pod lists to per-pool totals. A node joins a pool by its product label
/// ([`DEFAULT_POOL`] when unlabeled); only nodes with a nonzero GPU allocatable form pools.
/// A pod's GPUs count against the pool of the node it is scheduled on, while it is
/// non-terminal; unscheduled pods hold nothing and are skipped.
fn aggregate_pools(nodes: &[Node], pods: &[Pod]) -> Vec<GpuPool> {
    // Node name -> pool name, for scheduled-pod attribution.
    let mut node_pool: HashMap<&str, &str> = HashMap::new();
    // Pool name -> (allocatable, requested), insertion-ordered by first sighting.
    let mut pools: Vec<GpuPool> = Vec::new();

    for node in nodes {
        let Some(name) = node.metadata.name.as_deref() else {
            continue;
        };
        let allocatable = node
            .status
            .as_ref()
            .and_then(|s| s.allocatable.as_ref())
            .and_then(|a| a.get(GPU_RESOURCE))
            .and_then(|q| q.0.parse::<u64>().ok())
            .unwrap_or(0);
        if allocatable == 0 {
            continue;
        }
        let pool = node
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(PRODUCT_LABEL))
            .map(String::as_str)
            .unwrap_or(DEFAULT_POOL);
        node_pool.insert(name, pool);
        match pools.iter_mut().find(|p| p.pool == pool) {
            Some(p) => p.allocatable += allocatable,
            None => pools.push(GpuPool {
                pool: pool.to_string(),
                allocatable,
                requested: 0,
            }),
        }
    }

    let excluded = excluded_priority_classes();
    for pod in pods {
        if is_terminal(pod) || is_excluded_filler(pod, &excluded) {
            continue;
        }
        let Some(node) = pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) else {
            continue;
        };
        let Some(&pool) = node_pool.get(node) else {
            continue;
        };
        let requested = pod_gpu_request(pod);
        if requested == 0 {
            continue;
        }
        if let Some(p) = pools.iter_mut().find(|p| p.pool == pool) {
            p.requested += requested;
        }
    }

    pools
}

/// Whether the pod is preemptible filler (its priority class is in the excluded set): it holds
/// GPUs only until real work wants them, so it never counts as held capacity.
fn is_excluded_filler(pod: &Pod, excluded: &std::collections::HashSet<String>) -> bool {
    pod.spec
        .as_ref()
        .and_then(|s| s.priority_class_name.as_deref())
        .is_some_and(|pc| excluded.contains(pc))
}

/// Whether the pod no longer holds its requests.
fn is_terminal(pod: &Pod) -> bool {
    matches!(
        pod.status.as_ref().and_then(|s| s.phase.as_deref()),
        Some("Succeeded") | Some("Failed")
    )
}

/// The pod's effective GPU request: the scheduler charges `max(sum of containers, largest init
/// container)` for an extended resource.
fn pod_gpu_request(pod: &Pod) -> u64 {
    let Some(spec) = pod.spec.as_ref() else {
        return 0;
    };
    let container_gpus = |resources: Option<&k8s_openapi::api::core::v1::ResourceRequirements>| {
        resources
            .and_then(|r| r.requests.as_ref())
            .and_then(|req| req.get(GPU_RESOURCE))
            .and_then(|q| q.0.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let main: u64 = spec
        .containers
        .iter()
        .map(|c| container_gpus(c.resources.as_ref()))
        .sum();
    let init = spec
        .init_containers
        .iter()
        .flatten()
        .map(|c| container_gpus(c.resources.as_ref()))
        .max()
        .unwrap_or(0);
    main.max(init)
}

#[cfg(test)]
mod tests {
    use crate::runs::cluster_stats::*;

    fn node(name: &str, gpus: u64, product: Option<&str>) -> Node {
        let mut labels = serde_json::Map::new();
        if let Some(p) = product {
            labels.insert(
                PRODUCT_LABEL.to_string(),
                serde_json::Value::String(p.to_string()),
            );
        }
        serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "labels": labels},
            "status": {"allocatable": {GPU_RESOURCE: gpus.to_string()}}
        }))
        .unwrap()
    }

    fn pod(node: Option<&str>, phase: &str, gpus: u64) -> Pod {
        let mut spec = serde_json::json!({
            "containers": [{
                "name": "main",
                "resources": {"requests": {GPU_RESOURCE: gpus.to_string()}}
            }]
        });
        if let Some(n) = node {
            spec["nodeName"] = serde_json::Value::String(n.to_string());
        }
        serde_json::from_value(serde_json::json!({
            "metadata": {"name": "p"},
            "spec": spec,
            "status": {"phase": phase}
        }))
        .unwrap()
    }

    #[test]
    fn pools_group_by_product_label_and_sum_requests() {
        let nodes = vec![
            node("a100-01", 8, Some("NVIDIA-A100-SXM4-80GB")),
            node("a100-02", 8, Some("NVIDIA-A100-SXM4-80GB")),
            node("b200-01", 8, Some("NVIDIA-B200")),
            node("cpu-01", 0, None),
        ];
        let pods = vec![
            pod(Some("a100-01"), "Running", 4),
            pod(Some("a100-02"), "Running", 8),
            pod(Some("b200-01"), "Pending", 2), // scheduled Pending holds its claim
        ];
        let pools = aggregate_pools(&nodes, &pods);
        assert_eq!(
            pools,
            vec![
                GpuPool {
                    pool: "NVIDIA-A100-SXM4-80GB".into(),
                    allocatable: 16,
                    requested: 12
                },
                GpuPool {
                    pool: "NVIDIA-B200".into(),
                    allocatable: 8,
                    requested: 2
                },
            ]
        );
    }

    #[test]
    fn unlabeled_gpu_nodes_fall_into_the_default_pool() {
        let nodes = vec![node("gpu-01", 4, None)];
        let pools = aggregate_pools(&nodes, &[pod(Some("gpu-01"), "Running", 1)]);
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].pool, DEFAULT_POOL);
        assert_eq!((pools[0].allocatable, pools[0].requested), (4, 1));
    }

    #[test]
    fn terminal_and_unscheduled_pods_hold_nothing() {
        let nodes = vec![node("a", 8, None)];
        let pods = vec![
            pod(Some("a"), "Succeeded", 8),
            pod(Some("a"), "Failed", 8),
            pod(None, "Pending", 8), // no node yet: demand, not held capacity
        ];
        let pools = aggregate_pools(&nodes, &pods);
        assert_eq!(pools[0].requested, 0);
    }

    #[test]
    fn pods_on_unknown_or_gpuless_nodes_are_skipped() {
        let nodes = vec![node("a", 8, None)];
        let pods = vec![pod(Some("not-a-gpu-node"), "Running", 2)];
        assert_eq!(aggregate_pools(&nodes, &pods)[0].requested, 0);
    }

    #[test]
    fn effective_request_is_max_of_init_and_container_sum() {
        let p: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "p"},
            "spec": {
                "nodeName": "a",
                "initContainers": [{
                    "name": "warm",
                    "resources": {"requests": {GPU_RESOURCE: "6"}}
                }],
                "containers": [
                    {"name": "one", "resources": {"requests": {GPU_RESOURCE: "2"}}},
                    {"name": "two", "resources": {"requests": {GPU_RESOURCE: "2"}}}
                ]
            },
            "status": {"phase": "Running"}
        }))
        .unwrap();
        assert_eq!(pod_gpu_request(&p), 6);
        let pools = aggregate_pools(&[node("a", 8, None)], &[p]);
        assert_eq!(pools[0].requested, 6);
    }

    /// The pike `crucible` ClusterQueue, verbatim shape: cpu/memory in one resource group,
    /// GPUs in another, string quantities, reservation totals in the status.
    #[test]
    fn cluster_queues_reduce_to_gpu_quota_and_reservation() {
        let cq: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "kueue.x-k8s.io/v1beta1",
            "kind": "ClusterQueue",
            "metadata": {"name": "crucible"},
            "spec": {
                "resourceGroups": [
                    {"coveredResources": ["cpu", "memory"], "flavors": [{
                        "name": "default-flavor",
                        "resources": [
                            {"name": "cpu", "nominalQuota": "1722"},
                            {"name": "memory", "nominalQuota": "9951709180Ki"}
                        ]}]},
                    {"coveredResources": ["nvidia.com/gpu"], "flavors": [{
                        "name": "nvidia-gpu-flavor",
                        "resources": [{"name": "nvidia.com/gpu", "nominalQuota": "32"}]
                    }]}
                ]
            },
            "status": {
                "admittedWorkloads": 2,
                "pendingWorkloads": 0,
                "flavorsReservation": [
                    {"name": "default-flavor", "resources": [
                        {"borrowed": "0", "name": "cpu", "total": "56"},
                        {"borrowed": "0", "name": "memory", "total": "1Ti"}
                    ]},
                    {"name": "nvidia-gpu-flavor", "resources": [
                        {"borrowed": "0", "name": "nvidia.com/gpu", "total": "16"}
                    ]}
                ]
            }
        }))
        .unwrap();
        let queues = parse_cluster_queues(&[cq]);
        assert_eq!(
            queues,
            vec![KueueQueue {
                queue: "crucible".into(),
                cohort: None,
                pending: 0,
                admitted: 2,
                gpu_nominal: 32,
                gpu_reserved: 16,
            }]
        );
    }

    #[test]
    fn a_statusless_queue_reads_as_zeros_and_numbers_parse_unquoted() {
        let cq: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "kueue.x-k8s.io/v1beta1",
            "kind": "ClusterQueue",
            "metadata": {"name": "fresh"},
            "spec": {
                "cohort": "shared",
                "resourceGroups": [{"coveredResources": ["nvidia.com/gpu"], "flavors": [{
                    "name": "f",
                    "resources": [{"name": "nvidia.com/gpu", "nominalQuota": 8}]
                }]}]
            }
        }))
        .unwrap();
        let queues = parse_cluster_queues(&[cq]);
        assert_eq!(
            queues,
            vec![KueueQueue {
                queue: "fresh".into(),
                cohort: Some("shared".into()),
                pending: 0,
                admitted: 0,
                gpu_nominal: 8,
                gpu_reserved: 0,
            }]
        );
    }

    #[test]
    fn excluded_priority_class_pods_hold_nothing() {
        let nodes = vec![node("h100-01", 8, Some("NVIDIA-H100-80GB-HBM3"))];
        let mut soak = pod(Some("h100-01"), "Running", 8);
        soak.spec.as_mut().expect("spec").priority_class_name =
            Some("cw-hpc-verification".to_string());
        let real = pod(Some("h100-01"), "Running", 2);
        let pools = aggregate_pools(&nodes, &[soak, real]);
        assert_eq!(
            (pools[0].allocatable, pools[0].requested),
            (8, 2),
            "the idle-soak filler must not count as held capacity"
        );
    }

    #[test]
    fn a_fresh_push_serves_reachable_and_a_stale_one_grays_out() {
        let snap = ClusterSnapshot {
            cluster: "pike".into(),
            reachable: true,
            age_secs: 0,
            pools: vec![GpuPool {
                pool: "x".into(),
                allocatable: 8,
                requested: 3,
            }],
            kueue: None,
            error: None,
        };
        let t0 = Instant::now();
        let fresh = render_pushed(
            &PushedSnapshot {
                at: t0,
                snapshot: snap.clone(),
            },
            t0 + Duration::from_secs(30),
        );
        assert!(fresh.reachable);
        assert_eq!(fresh.age_secs, 30);
        let stale = render_pushed(
            &PushedSnapshot {
                at: t0,
                snapshot: snap,
            },
            t0 + PUSH_STALE + Duration::from_secs(60),
        );
        assert!(!stale.reachable, "a silent pusher must gray out");
        assert_eq!(stale.pools.len(), 1, "last numbers still render");
        assert!(stale.error.as_deref().unwrap_or("").contains("no push"));
    }

    #[test]
    fn a_successful_fetch_replaces_the_cache_and_reads_fresh() {
        let mut cache = HashMap::new();
        let now = Instant::now();
        let pools = vec![GpuPool {
            pool: "x".into(),
            allocatable: 8,
            requested: 3,
        }];
        let snap = apply_fetch(
            &mut cache,
            "pike",
            Ok(Fetched {
                pools: pools.clone(),
                kueue: None,
            }),
            now,
        );
        assert!(snap.reachable);
        assert_eq!(snap.age_secs, 0);
        assert_eq!(snap.pools, pools);
        assert!(snap.error.is_none());
    }

    #[test]
    fn a_failed_fetch_serves_the_stale_snapshot_marked_unreachable() {
        let mut cache = HashMap::new();
        let t0 = Instant::now();
        let pools = vec![GpuPool {
            pool: "x".into(),
            allocatable: 8,
            requested: 3,
        }];
        apply_fetch(
            &mut cache,
            "pike",
            Ok(Fetched {
                pools: pools.clone(),
                kueue: Some(vec![KueueQueue {
                    queue: "crucible".into(),
                    cohort: None,
                    pending: 0,
                    admitted: 2,
                    gpu_nominal: 32,
                    gpu_reserved: 16,
                }]),
            }),
            t0,
        );
        let later = t0 + Duration::from_secs(90);
        let snap = apply_fetch(&mut cache, "pike", Err("boom".into()), later);
        assert!(!snap.reachable);
        assert_eq!(snap.age_secs, 90, "stale age is reported");
        assert_eq!(snap.pools, pools, "last good numbers still render");
        assert_eq!(
            snap.kueue.as_deref().map(|k| k.len()),
            Some(1),
            "the stale kueue rows ride along"
        );
        assert_eq!(snap.error.as_deref(), Some("boom"));
    }

    #[test]
    fn a_failed_fetch_with_no_history_is_empty_but_never_an_error() {
        let mut cache = HashMap::new();
        let snap = apply_fetch(&mut cache, "ghost", Err("no route".into()), Instant::now());
        assert!(!snap.reachable);
        assert!(snap.pools.is_empty());
        assert_eq!(snap.error.as_deref(), Some("no route"));
    }
}
