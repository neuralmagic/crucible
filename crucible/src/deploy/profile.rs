//! The deploy profile: the *only* hand-written part of a deployment, the per-cluster facts crucible
//! can't derive from the domain manifest (namespaces, the service account, secret *names*, resources,
//! the loop image, the live-deployment gate endpoint). One small file per cluster rather than per run.
//!
//! Everything else the renderer projects from the manifest (the source of truth). Secrets are
//! referenced by name only, the renderer never holds a credential; a human/External Secrets
//! provisions the contents, and a missing secret is a clear render-time reference, not a surprise
//! `CreateContainerConfigError` later.

use crate::openshell::gateway::ComputeDriver;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployProfile {
    pub cluster: Cluster,
    pub image: ImageCfg,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub secrets: Secrets,
    /// Generic pod env literals the *domain* needs but the engine can't name, the hook env
    /// (deployment namespaces, gate endpoints, bench knobs), the backend's Vertex
    /// project/region. The engine projects these verbatim; it hardcodes none of the names, so a new
    /// domain (or a non-Vertex backend) just writes a different map.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Generic pod env sourced from a `secretKeyRef` (e.g. `GCLOUD_CREDENTIALS` ← `gcloud-adc/adc.json`).
    /// Reference-only, the renderer never reads the secret's contents.
    #[serde(default)]
    pub secret_env: Vec<SecretEnv>,
    /// The outer-loop controller's environment half: image, state volume, discovery cadence, watched
    /// repos, admission caps. Absent unless the profile is used for the deprecated
    /// `crucible deploy render --controller` (see the `crucible-controller` Helm chart).
    #[serde(default)]
    pub controller: Option<ControllerCfg>,
    /// Per-cluster substrate for the codegen measure jobs (Kueue queue, PVCs, GPU ceiling). Only
    /// needed when the domain manifest declares `[measure]`. The engine projects these as the
    /// BROKER_CODEGEN_* substrate env the broker reads.
    #[serde(default)]
    pub measure: Option<MeasureSubstrate>,
}

/// Per-cluster substrate facts for the codegen measure jobs: the namespace/queue/PVCs/ceiling the
/// broker's GPU Jobs run against. These are CLUSTER facts (which Kueue queue, which prewarmed-weights
/// PVC), distinct from the domain's own frozen-command contract (that lives in the manifest's
/// `[measure]`). Projected as the BROKER_CODEGEN_* substrate env.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasureSubstrate {
    /// Namespace the GPU measure Jobs run in (BROKER_CODEGEN_NAMESPACE, required by the broker).
    pub namespace: String,
    /// Kueue LocalQueue (BROKER_CODEGEN_QUEUE; broker default is "crucible-measure").
    #[serde(default)]
    pub queue: Option<String>,
    /// Prewarmed-weights PVC mounted RO (BROKER_CODEGEN_MODEL_PVC). Optional: unset means the GPU
    /// jobs get no weights mount (kernel domains measure source, not a served model).
    #[serde(default)]
    pub model_pvc: Option<String>,
    /// Trace-transport PVC for profile jobs (BROKER_CODEGEN_ARTIFACTS_PVC).
    #[serde(default)]
    pub artifacts_pvc: Option<String>,
    /// Substrate GPU ceiling; a [measure] gpus over this is rejected at broker finalize (BROKER_CODEGEN_MAX_GPUS).
    #[serde(default)]
    pub max_gpus: Option<u32>,
    /// Where the agent's edits live in the sandbox; the broker pulls this tree to build (BROKER_CODEGEN_SANDBOX_WORKDIR).
    #[serde(default)]
    pub sandbox_workdir: Option<String>,
}

impl MeasureSubstrate {
    /// The BROKER_CODEGEN_* substrate env (namespace/queue/PVCs/ceiling/workdir). Reference-only,
    /// never a credential. Deliberately does NOT set BROKER_CODEGEN_WORKSPACE_PVC: mounting the
    /// workspace at measure time shadows the candidate image's baked tree (a measurement must be a
    /// function of the digest alone).
    pub fn broker_env(&self) -> Vec<(String, String)> {
        let mut env = vec![(
            "BROKER_CODEGEN_NAMESPACE".to_string(),
            self.namespace.clone(),
        )];
        if let Some(q) = &self.queue {
            env.push(("BROKER_CODEGEN_QUEUE".to_string(), q.clone()));
        }
        if let Some(p) = &self.model_pvc {
            env.push(("BROKER_CODEGEN_MODEL_PVC".to_string(), p.clone()));
        }
        if let Some(p) = &self.artifacts_pvc {
            env.push(("BROKER_CODEGEN_ARTIFACTS_PVC".to_string(), p.clone()));
        }
        if let Some(g) = &self.max_gpus {
            env.push(("BROKER_CODEGEN_MAX_GPUS".to_string(), g.to_string()));
        }
        if let Some(w) = &self.sandbox_workdir {
            env.push(("BROKER_CODEGEN_SANDBOX_WORKDIR".to_string(), w.clone()));
        }
        env
    }
}

/// The `[controller]` table: the environment-side facts the outer-loop controller needs that the
/// engine can't derive, its own image, how big to make the state volume, how often to poll for new
/// issues, which repos to watch, and the admission caps that keep it from running away unattended.
/// Cadence/caps/repos are projected into the rendered Deployment as `CONTROLLER_*` env (the
/// clap-with-env-fallback contract `ControllerCfg` reads); `image`/`state_volume_size` size the
/// Deployment/PVC directly and aren't env.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerCfg {
    /// The controller image, a tag the renderer resolves to `@sha256:…` at render time, same as
    /// `[image].loop`.
    pub image: String,
    /// The state PVC's size (holds `outer.sqlite` + `controller-events.jsonl`).
    pub state_volume_size: String,
    /// How often the discovery timer polls watched repos, in seconds (`CONTROLLER_DISCOVERY_CADENCE_SECS`).
    pub discovery_cadence_secs: u64,
    /// Repos the controller triages/polls, `owner/repo` (`CONTROLLER_WATCHED_REPOS`, comma-joined).
    pub watched_repos: Vec<String>,
    /// The token-blowout circuit breakers, all enforced inside reconcile.
    pub caps: ControllerCaps,
}

/// The controller's four caps, verbatim.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerCaps {
    /// Per-reconcile cost ceiling in USD (scope turns + launched runs), `CONTROLLER_PER_RECONCILE_COST_USD`.
    pub per_reconcile_cost_usd: f64,
    /// Max concurrent loop pods admitted, `CONTROLLER_MAX_CONCURRENT_PODS`.
    pub max_concurrent_pods: u32,
    /// Max new scopes started per day, `CONTROLLER_MAX_SCOPES_PER_DAY`.
    pub max_scopes_per_day: u32,
    /// Global daily cost ceiling in USD that parks the autopilot entirely when crossed,
    /// `CONTROLLER_DAILY_COST_CEILING_USD`.
    pub daily_cost_ceiling_usd: f64,
}

/// One `valueFrom: secretKeyRef` pod env entry. Generic, the name/secret/key are the domain's, not the
/// engine's.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretEnv {
    pub name: String,
    pub secret: String,
    pub key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cluster {
    /// Namespace the loop pod runs in.
    pub loop_namespace: String,
    /// Namespace the system under test and its Deployments live in (the broker's kubectl hooks target it; the
    /// RoleBinding grants the loop SA `edit` here).
    pub rig_namespace: String,
    /// The service account the loop pod runs as (its projected token is what kubectl authenticates with).
    pub service_account: String,
    /// The in-cluster kubeconfig configmap mounted at `/etc/kube` (points kubectl at the API server +
    /// the projected token).
    #[serde(default = "default_kubeconfig_configmap")]
    pub kubeconfig_configmap: String,
    /// The OpenShell supervisor image the nested sandbox runtime pulls (`OPENSHELL_SUPERVISOR_IMAGE`).
    pub supervisor_image: String,
    /// Publish-on-keep S3 base; when set, the wrapper passes `--results-bucket`. Unset = don't publish.
    #[serde(default)]
    pub results_bucket: Option<String>,
    /// IRSA role the loop pod assumes to publish (renderer projects an sts-audience token + `AWS_ROLE_ARN`).
    #[serde(default)]
    pub aws_role_arn: Option<String>,
    /// Read-only role the in-pod gateway assumes (web identity, same projected token) to SigV4-sign
    /// sandbox S3 egress at the proxy, the read half of the S3 role split. Unset = no S3 provider.
    #[serde(default)]
    pub aws_sandbox_role_arn: Option<String>,
    /// The OpenShell compute driver for the sandbox: `podman` nests it inside the loop pod
    /// (laptop/EC2); `kubernetes` schedules it as a sibling pod in-cluster.
    #[serde(default)]
    pub sandbox_driver: ComputeDriver,
    /// Nodes the cluster operator has marked bad (e.g. broken CNI); rendered as a required
    /// nodeAffinity `NotIn` on `kubernetes.io/hostname` for every pod this profile renders.
    #[serde(default)]
    pub avoid_nodes: Vec<String>,
    /// The controller's ingest drop-box base URL, e.g.
    /// `http://crucible-controller.autoresearch.svc:8080`. When set, a rendered turn pod gets a
    /// projected `crucible-ingest`-audience ServiceAccount token + the `CRUCIBLE_INGEST_URL` /
    /// `CRUCIBLE_INGEST_TOKEN_PATH` env so its large artifacts POST to the drop-box instead of riding
    /// the pod logs. Unset = no drop-box; the engine falls back to marker emission.
    #[serde(default)]
    pub ingest_url: Option<String>,
}

fn default_kubeconfig_configmap() -> String {
    "autoresearch-kubeconfig".to_string()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageCfg {
    /// The loop image, a tag the renderer resolves to `@sha256:…` at render time (the footgun fix:
    /// the pin is computed, not hand-typed and forgotten after a rebuild).
    #[serde(rename = "loop")]
    pub loop_image: String,
    /// The pull secret for the (private) loop image (`imagePullSecrets`).
    pub pull_secret: String,
    /// Where the loop image bakes the domain packs (the wrapper resolves `<root>/<composite>`).
    #[serde(default = "default_domains_root")]
    pub domains_root: String,
}

fn default_domains_root() -> String {
    "/opt/crucible/domains".to_string()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default = "default_cpu_request")]
    pub cpu_request: String,
    #[serde(default = "default_mem_request")]
    pub mem_request: String,
    #[serde(default = "default_cpu_limit")]
    pub cpu_limit: String,
    #[serde(default = "default_mem_limit")]
    pub mem_limit: String,
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            cpu_request: default_cpu_request(),
            mem_request: default_mem_request(),
            cpu_limit: default_cpu_limit(),
            mem_limit: default_mem_limit(),
        }
    }
}

fn default_cpu_request() -> String {
    "2".to_string()
}
fn default_mem_request() -> String {
    "6Gi".to_string()
}
fn default_cpu_limit() -> String {
    "8".to_string()
}
fn default_mem_limit() -> String {
    "16Gi".to_string()
}

/// The registry authfile secrets the openshell-build template mounts as *volumes* (these name volumes,
/// not env, so they stay typed). Both optional, a domain that neither pulls a private sandbox nor pushes
/// a candidate image leaves them unset and the renderer skips the mount.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    /// The registry *pull* authfile, so the gateway's podman pulls a private sandbox (and supervisor)
    /// image. Mounted at `/etc/containers/auth.json` and named by `REGISTRY_AUTH_FILE`, which podman
    /// honors ahead of its own lookup path. A standard containers-auth.json: it may hold as many
    /// registries as the run needs, and any auth kind podman understands.
    #[serde(default)]
    pub pull_authfile: Option<String>,
    /// The registry *push* authfile (dockerconfigjson) for forge build/derive (`FORGE_AUTHFILE`).
    /// Mounted at `/etc/quay/push.json`.
    #[serde(default)]
    pub push_authfile: Option<String>,
}

impl DeployProfile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading deploy profile {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing deploy profile {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
        [cluster]
        loop_namespace = "autoresearch"
        rig_namespace = "rig"
        service_account = "autoresearch-publisher"
        supervisor_image = "registry.example.com/openshell-supervisor:latest"
        [image]
        loop = "registry.example.com/crucible-loop:latest"
        pull_secret = "quay-pull"
    "#;

    /// Every profile written before `avoid_nodes` existed must keep parsing, with an empty list.
    #[test]
    fn profile_without_avoid_nodes_parses_to_empty() {
        let profile: DeployProfile = toml::from_str(BASE).expect("profile parses");
        assert!(profile.cluster.avoid_nodes.is_empty());
    }

    #[test]
    fn profile_with_avoid_nodes_parses_the_list() {
        let text = BASE.replace(
            "[cluster]",
            "[cluster]\navoid_nodes = [\"g12e022\", \"g12e099\"]",
        );
        let profile: DeployProfile = toml::from_str(&text).expect("profile parses");
        assert_eq!(profile.cluster.avoid_nodes, vec!["g12e022", "g12e099"]);
    }
}
