//! Kueue-admitted GPU measure jobs: render a `batch/v1` Job that runs a digest-pinned candidate on
//! the GPU, drive it to a terminal state, and collect its logs.

use crate::kube::{self, JobResult, KubeTarget};
use anyhow::{Context, Result};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const KUEUE_QUEUE_LABEL: &str = "kueue.x-k8s.io/queue-name";
const MEASURE_CONTAINER: &str = "measure";
const GPU_RESOURCE: &str = "nvidia.com/gpu";
const SHM_MOUNT: &str = "/dev/shm";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PvcMount {
    pub claim_name: String,
    pub mount_path: String,
    pub read_only: bool,
}

/// Everything a GPU measure Job needs, resolved by the caller (no manifest/template indirection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuJobSpec {
    pub name: String,
    pub namespace: String,
    /// The digest-pinned candidate image.
    pub image: String,
    /// The Kueue LocalQueue to submit through. `None` on a cluster WITHOUT Kueue: the Job then
    /// carries no queue label and starts unsuspended, because nothing else would ever admit it.
    pub queue_name: Option<String>,
    pub pull_secret: Option<String>,
    /// Shell body run as `sh -c`.
    pub command: String,
    pub env: Vec<(String, String)>,
    /// Request AND limit (k8s requires them equal for extended resources).
    pub gpus: u32,
    pub cpu: String,
    pub mem_request: String,
    pub mem_limit: String,
    pub shm_size_gi: u32,
    pub active_deadline_seconds: i64,
    pub ttl_seconds: i32,
    pub pvc_mounts: Vec<PvcMount>,
    /// Node labels the Job must land on. Without Kueue there is no flavor to pin placement, so a
    /// heterogeneous cluster will happily schedule an NVFP4 job onto the wrong GPU generation.
    pub node_selector: BTreeMap<String, String>,
    /// Owner the Job is garbage-collected with, normally the submitting loop pod: a job whose
    /// consumer died is a GPU burning for nobody (an orphaned sanitizer once held an H200 for
    /// 90 minutes). `None` for cross-cluster (spoke) or cross-namespace submits, where an
    /// ownerReference is impossible.
    pub owner: Option<JobOwner>,
}

/// The same-namespace pod a measure Job is owned by (`kind: Pod`, `controller: false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOwner {
    pub name: String,
    pub uid: String,
}

/// A driven Job's terminal state, captured logs, and the wall-clock it held resources for.
#[derive(Debug, Clone)]
pub struct GpuJobRun {
    pub result: JobResult,
    pub logs: String,
    pub elapsed: Duration,
}

impl GpuJobSpec {
    fn labels(&self) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::from([
            (
                "app.kubernetes.io/managed-by".to_string(),
                "crucible".to_string(),
            ),
            ("app".to_string(), "crucible".to_string()),
        ]);
        if let Some(queue) = &self.queue_name {
            labels.insert(KUEUE_QUEUE_LABEL.to_string(), queue.clone());
        }
        labels
    }

    fn container(&self) -> core::Container {
        // gpus == 0 omits the GPU resource entirely (a CPU-only job, e.g. the spoke smoketest):
        // an explicit `nvidia.com/gpu: 0` still demands a GPU-quota'd Kueue flavor.
        let mut requests = BTreeMap::from([
            ("cpu".to_string(), Quantity(self.cpu.clone())),
            ("memory".to_string(), Quantity(self.mem_request.clone())),
        ]);
        let mut limits = BTreeMap::from([("memory".to_string(), Quantity(self.mem_limit.clone()))]);
        if self.gpus > 0 {
            requests.insert(GPU_RESOURCE.to_string(), Quantity(self.gpus.to_string()));
            limits.insert(GPU_RESOURCE.to_string(), Quantity(self.gpus.to_string()));
        }

        let mut volume_mounts: Vec<core::VolumeMount> = self
            .pvc_mounts
            .iter()
            .map(|m| core::VolumeMount {
                name: pvc_volume_name(&m.claim_name),
                mount_path: m.mount_path.clone(),
                read_only: Some(m.read_only),
                ..Default::default()
            })
            .collect();
        volume_mounts.push(core::VolumeMount {
            name: "shm".to_string(),
            mount_path: SHM_MOUNT.to_string(),
            ..Default::default()
        });

        core::Container {
            name: MEASURE_CONTAINER.to_string(),
            image: Some(self.image.clone()),
            command: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
            args: Some(vec![self.command.clone()]),
            env: Some(
                self.env
                    .iter()
                    .map(|(k, v)| core::EnvVar {
                        name: k.clone(),
                        value: Some(v.clone()),
                        value_from: None,
                    })
                    .collect(),
            ),
            resources: Some(core::ResourceRequirements {
                requests: Some(requests),
                limits: Some(limits),
                ..Default::default()
            }),
            volume_mounts: Some(volume_mounts),
            ..Default::default()
        }
    }

    fn volumes(&self) -> Vec<core::Volume> {
        let mut volumes: Vec<core::Volume> = self
            .pvc_mounts
            .iter()
            .map(|m| core::Volume {
                name: pvc_volume_name(&m.claim_name),
                persistent_volume_claim: Some(core::PersistentVolumeClaimVolumeSource {
                    claim_name: m.claim_name.clone(),
                    read_only: Some(m.read_only),
                }),
                ..Default::default()
            })
            .collect();
        volumes.push(core::Volume {
            name: "shm".to_string(),
            empty_dir: Some(core::EmptyDirVolumeSource {
                medium: Some("Memory".to_string()),
                size_limit: Some(Quantity(format!("{}Gi", self.shm_size_gi))),
            }),
            ..Default::default()
        });
        volumes
    }
}

/// Render the measure Job (pure, no cluster contact). `suspend: true`: Kueue owns when it runs.
fn render_gpu_job(spec: &GpuJobSpec) -> Job {
    let labels = spec.labels();
    let pod_spec = core::PodSpec {
        restart_policy: Some("Never".to_string()),
        image_pull_secrets: spec
            .pull_secret
            .clone()
            .map(|name| vec![core::LocalObjectReference { name }]),
        containers: vec![spec.container()],
        volumes: Some(spec.volumes()),
        node_selector: (!spec.node_selector.is_empty()).then(|| spec.node_selector.clone()),
        ..Default::default()
    };
    Job {
        metadata: ObjectMeta {
            name: Some(spec.name.clone()),
            namespace: Some(spec.namespace.clone()),
            labels: Some(labels.clone()),
            owner_references: spec.owner.as_ref().map(|o| {
                vec![
                    k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                        api_version: "v1".to_string(),
                        kind: "Pod".to_string(),
                        name: o.name.clone(),
                        uid: o.uid.clone(),
                        controller: Some(false),
                        block_owner_deletion: Some(false),
                    },
                ]
            }),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            // No Kueue on this cluster means no admission controller to unsuspend it.
            suspend: Some(spec.queue_name.is_some()),
            active_deadline_seconds: Some(spec.active_deadline_seconds.max(1)),
            ttl_seconds_after_finished: Some(spec.ttl_seconds),
            template: core::PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// How often the streaming wait snapshots the pod log between job-status polls.
const LIVE_LOG_POLL: Duration = Duration::from_secs(15);

/// Submit the Job, wait for a terminal state, collect logs, calling `on_logs` with a pod-log
/// snapshot every [`LIVE_LOG_POLL`]. The snapshot contract is MONOTONIC: each emission is the FULL
/// log so far (`backoffLimit: 0`, so a growing prefix, never a sliding window) and only strictly-longer
/// snapshots are emitted, so a concurrent reader's offset can never shrink; the returned `logs` is
/// guarded the same way. `queue_slack` extends the wait for time spent suspended in the queue (which
/// doesn't tick the deadline). A timed-out Job is deleted (TTL only reaps finished jobs).
pub fn run_gpu_job_with(
    target: &KubeTarget,
    spec: &GpuJobSpec,
    queue_slack: Duration,
    mut on_logs: impl FnMut(&str),
) -> Result<GpuJobRun> {
    let job = render_gpu_job(spec);
    let started = Instant::now();
    kube::create_job(target, &spec.namespace, &job)
        .with_context(|| format!("submitting measure Job {}", spec.name))?;
    tracing::info!(
        job = %spec.name,
        queue = spec.queue_name.as_deref().unwrap_or("none"),
        gpus = spec.gpus,
        suspended = spec.queue_name.is_some(),
        "gpu job submitted"
    );
    let wait = Duration::from_secs(spec.active_deadline_seconds.max(1) as u64) + queue_slack;
    let deadline = Instant::now() + wait;
    let mut longest = String::new();
    let result = loop {
        let slice = LIVE_LOG_POLL.min(deadline.saturating_duration_since(Instant::now()));
        if slice.is_zero() {
            break JobResult::TimedOut;
        }
        // A short wait slice: `TimedOut` here just means "still running after the slice".
        let step = kube::wait_for_job(target, &spec.namespace, &spec.name, slice)?;
        if let Ok(snapshot) =
            kube::job_logs(target, &spec.namespace, &spec.name, MEASURE_CONTAINER, None)
            && snapshot.len() > longest.len()
        {
            on_logs(&snapshot);
            longest = snapshot;
        }
        if step != JobResult::TimedOut {
            break step;
        }
    };
    tracing::info!(
        job = %spec.name,
        result = ?result,
        elapsed_s = started.elapsed().as_secs(),
        "gpu job reached a terminal state"
    );
    let logs = match kube::job_logs(target, &spec.namespace, &spec.name, MEASURE_CONTAINER, None) {
        Ok(finale) if finale.len() >= longest.len() => finale,
        _ => longest,
    };
    if result == JobResult::TimedOut
        && let Err(e) = kube::delete_job(target, &spec.namespace, &spec.name)
    {
        eprintln!(
            "warning: failed to delete timed-out measure Job {}: {e:#}",
            spec.name
        );
    }
    Ok(GpuJobRun {
        result,
        logs,
        elapsed: started.elapsed(),
    })
}

/// DNS-1123 volume name derived from a PVC claim name.
fn pvc_volume_name(claim: &str) -> String {
    let mut out: String = claim
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(63);
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "pvc".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> GpuJobSpec {
        GpuJobSpec {
            name: "crucible-bench-abc".into(),
            namespace: "test-ns".into(),
            image: "ghcr.io/example/img@sha256:deadbeef".into(),
            queue_name: Some("crucible-measure".into()),
            pull_secret: Some("test-pull-secret".into()),
            command: "bench throughput ...".into(),
            env: vec![("HF_HUB_OFFLINE".into(), "1".into())],
            node_selector: BTreeMap::from([(
                "nvidia.com/gpu.product".to_string(),
                "NVIDIA-B200".to_string(),
            )]),
            gpus: 2,
            cpu: "16".into(),
            mem_request: "128Gi".into(),
            mem_limit: "200Gi".into(),
            shm_size_gi: 16,
            active_deadline_seconds: 5400,
            ttl_seconds: 86400,
            owner: None,
            pvc_mounts: vec![
                PvcMount {
                    claim_name: "model-cache".into(),
                    mount_path: "/models".into(),
                    read_only: true,
                },
                PvcMount {
                    claim_name: "workspace".into(),
                    mount_path: "/workspace".into(),
                    read_only: false,
                },
            ],
        }
    }

    #[test]
    fn job_carries_the_kueue_queue_label_and_starts_suspended() {
        let job = render_gpu_job(&spec());
        let labels = job.metadata.labels.unwrap();
        assert_eq!(
            labels.get(KUEUE_QUEUE_LABEL).map(String::as_str),
            Some("crucible-measure")
        );
        let js = job.spec.unwrap();
        assert_eq!(js.suspend, Some(true), "Kueue owns when it runs");
        assert_eq!(js.backoff_limit, Some(0), "a measure runs once");
        assert_eq!(js.active_deadline_seconds, Some(5400));
        assert_eq!(js.ttl_seconds_after_finished, Some(86400));
        // The pod template must also carry the queue label (Kueue matches on it).
        let tmpl_labels = js.template.metadata.unwrap().labels.unwrap();
        assert_eq!(
            tmpl_labels.get(KUEUE_QUEUE_LABEL).map(String::as_str),
            Some("crucible-measure")
        );
    }

    /// Without Kueue nothing would ever unsuspend the Job, so it must start running and carry no
    /// queue label (a stray label on a Kueue-less cluster is a lie about how it got admitted).
    #[test]
    fn a_queueless_job_starts_unsuspended_and_unlabelled() {
        let mut spec = spec();
        spec.queue_name = None;
        let job = render_gpu_job(&spec);
        assert_eq!(job.metadata.labels.unwrap().get(KUEUE_QUEUE_LABEL), None);
        let js = job.spec.unwrap();
        assert_eq!(js.suspend, Some(false), "no admission controller exists");
        assert_eq!(
            js.template
                .metadata
                .unwrap()
                .labels
                .unwrap()
                .get(KUEUE_QUEUE_LABEL),
            None
        );
    }

    /// The node selector is what keeps an arch-specific measure off the wrong GPU generation; an
    /// empty selector must stay absent rather than render as `{}`.
    #[test]
    fn the_node_selector_reaches_the_pod_template() {
        let job = render_gpu_job(&spec());
        let pod = job.spec.unwrap().template.spec.unwrap();
        assert_eq!(
            pod.node_selector
                .as_ref()
                .and_then(|s| s.get("nvidia.com/gpu.product"))
                .map(String::as_str),
            Some("NVIDIA-B200")
        );

        let mut bare = spec();
        bare.node_selector = BTreeMap::new();
        let pod = render_gpu_job(&bare).spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.node_selector, None, "an empty selector pins nothing");
    }

    /// An owner makes the Job garbage-collect with its submitting pod (a dead consumer must not
    /// keep a GPU busy); no owner (spoke/cross-namespace submits) renders no ownerReferences.
    #[test]
    fn owner_renders_a_pod_owner_reference() {
        let mut with = spec();
        with.owner = Some(JobOwner {
            name: "deepgemm-loop".into(),
            uid: "abc-123".into(),
        });
        let refs = render_gpu_job(&with).metadata.owner_references.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "Pod");
        assert_eq!(refs[0].name, "deepgemm-loop");
        assert_eq!(refs[0].uid, "abc-123");
        assert_eq!(refs[0].controller, Some(false));

        assert!(
            render_gpu_job(&spec()).metadata.owner_references.is_none(),
            "no owner => no ownerReferences"
        );
    }

    #[test]
    fn gpu_request_and_limit_are_equal() {
        let job = render_gpu_job(&spec());
        let c = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let res = c.resources.as_ref().unwrap();
        let req = res.requests.as_ref().unwrap().get(GPU_RESOURCE).unwrap();
        let lim = res.limits.as_ref().unwrap().get(GPU_RESOURCE).unwrap();
        assert_eq!(req.0, "2");
        assert_eq!(
            lim.0, "2",
            "GPU limit must equal request or k8s rejects the pod"
        );
    }

    #[test]
    fn zero_gpus_omits_the_gpu_resource_entirely() {
        // The spoke smoketest submits CPU-only; an explicit `nvidia.com/gpu: 0` would still
        // demand a GPU-quota'd Kueue flavor and wedge Inadmissible.
        let mut s = spec();
        s.gpus = 0;
        let job = render_gpu_job(&s);
        let c = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let res = c.resources.as_ref().unwrap();
        assert!(res.requests.as_ref().unwrap().get(GPU_RESOURCE).is_none());
        assert!(res.limits.as_ref().unwrap().get(GPU_RESOURCE).is_none());
        assert!(res.requests.as_ref().unwrap().contains_key("cpu"));
    }

    #[test]
    fn shm_is_a_memory_backed_emptydir() {
        let job = render_gpu_job(&spec());
        let vols = job.spec.unwrap().template.spec.unwrap().volumes.unwrap();
        let shm = vols.iter().find(|v| v.name == "shm").unwrap();
        let ed = shm.empty_dir.as_ref().unwrap();
        assert_eq!(ed.medium.as_deref(), Some("Memory"));
        assert_eq!(ed.size_limit.as_ref().unwrap().0, "16Gi");
    }

    #[test]
    fn model_cache_mounts_read_only_and_workspace_writable() {
        let job = render_gpu_job(&spec());
        let spec_pod = job.spec.unwrap().template.spec.unwrap();
        let c = &spec_pod.containers[0];
        let mounts = c.volume_mounts.as_ref().unwrap();
        let model = mounts.iter().find(|m| m.mount_path == "/models").unwrap();
        assert_eq!(model.read_only, Some(true));
        let ws = mounts
            .iter()
            .find(|m| m.mount_path == "/workspace")
            .unwrap();
        assert_eq!(ws.read_only, Some(false));
        let vols = spec_pod.volumes.unwrap();
        assert!(vols.iter().any(|v| {
            v.persistent_volume_claim
                .as_ref()
                .is_some_and(|p| p.claim_name == "model-cache")
        }));
    }

    #[test]
    fn pull_secret_and_command_are_wired() {
        let job = render_gpu_job(&spec());
        let pod = job.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.image_pull_secrets.unwrap()[0].name, "test-pull-secret");
        let c = &pod.containers[0];
        assert_eq!(c.command.as_ref().unwrap(), &["/bin/sh", "-c"]);
        assert!(c.args.as_ref().unwrap()[0].contains("bench"));
        let env = c.env.as_ref().unwrap();
        assert!(
            env.iter()
                .any(|e| e.name == "HF_HUB_OFFLINE" && e.value.as_deref() == Some("1"))
        );
    }

    #[test]
    fn no_pull_secret_renders_no_image_pull_secrets() {
        let mut s = spec();
        s.pull_secret = None;
        let pod = render_gpu_job(&s).spec.unwrap().template.spec.unwrap();
        assert!(pod.image_pull_secrets.is_none());
    }

    #[test]
    fn pvc_volume_name_is_dns1123() {
        assert_eq!(pvc_volume_name("model-cache"), "model-cache");
        assert_eq!(pvc_volume_name("Weird_Claim.Name"), "weird-claim-name");
        assert_eq!(pvc_volume_name("---"), "pvc");
    }
}
