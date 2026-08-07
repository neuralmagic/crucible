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
use forge::fleet::{ClusterEntry, FleetFile};
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
    /// Profile-local `[clusters.<name>]` spoke entries. Normally these live in the fleet file
    /// (`clusters.toml` beside the profile); a local block overrides the fleet file on conflict.
    #[serde(default)]
    pub clusters: BTreeMap<String, ClusterEntry>,
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
    /// Named `[clusters.<name>]` spoke the GPU jobs are delegated to (BROKER_CODEGEN_CLUSTER).
    /// Unset = the broker's ambient client, exactly as before.
    #[serde(default)]
    pub cluster: Option<String>,
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
        if let Some(c) = &self.cluster {
            env.push(("BROKER_CODEGEN_CLUSTER".to_string(), c.clone()));
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
    /// PVC mounted at the domain's `state/` dir. When set, run state (session.jsonl, the
    /// agent-session files) survives the pod: the wrapper passes `--resume` when a session log is
    /// already present and the pod restarts on failure instead of dying with its emptyDir — a
    /// crashed run continues its turn instead of discarding hours of solver context. Unset = the
    /// state dir stays pod-local and a dead pod is a dead run.
    ///
    /// A string names an existing claim; a table is a template the render materializes as
    /// `<run>-state`. A kept claim must be deleted to start fresh: the wrapper sees the old
    /// session log and resumes.
    #[serde(default)]
    pub state_pvc: Option<StatePvc>,
    /// Grant the loop container what in-pod buildah needs on a cluster that gates it with
    /// seccomp/SCC rather than AppArmor (OpenShift): `seccompProfile: Unconfined` plus the
    /// capabilities buildah fails on, one at a time — `SYS_ADMIN` (it re-execs into a user
    /// namespace and the kernel refuses the uid_map without it), `SETFCAP` (layers carry file
    /// capabilities), `SYS_RESOURCE` (it raises RLIMIT_NOFILE), and `SYS_CHROOT`/`MKNOD`/`SETUID`/
    /// `SETGID` for chroot isolation itself.
    ///
    /// Off by default, and deliberately not implied by a domain that builds in-pod: on a cluster
    /// where the AppArmor route already works, ASKING for these gets the pod rejected by a
    /// restricted PSA/SCC instead of scheduling it. Requires an SCC that permits them.
    #[serde(default)]
    pub buildah_capabilities: bool,
}

/// `state_pvc = "name"` (existing claim) or a `[cluster.state_pvc]` template.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum StatePvc {
    Existing(String),
    Template(StatePvcTemplate),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatePvcTemplate {
    /// Cluster default when unset.
    #[serde(default)]
    pub storage_class: Option<String>,
    /// State is small: a session log plus agent-session files.
    #[serde(default = "default_state_size")]
    pub size: String,
    /// The loop pod is the only consumer, so RWO unless the profile says otherwise.
    #[serde(default = "default_state_access_modes")]
    pub access_modes: Vec<AccessMode>,
    /// Merged over the render's managed-by/run labels, profile wins. Some environments gate
    /// provisioning on them.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Verbatim: storage-class parameters, ownership tags.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

/// The API types these as bare strings; a closed enum rejects a typo at parse instead of at
/// provisioning. The shared `Read` prefix is the API's naming, not ours.
#[derive(Deserialize, Clone, Copy)]
#[allow(clippy::enum_variant_names)]
pub enum AccessMode {
    ReadWriteOnce,
    ReadOnlyMany,
    ReadWriteMany,
    ReadWriteOncePod,
}

impl AccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AccessMode::ReadWriteOnce => "ReadWriteOnce",
            AccessMode::ReadOnlyMany => "ReadOnlyMany",
            AccessMode::ReadWriteMany => "ReadWriteMany",
            AccessMode::ReadWriteOncePod => "ReadWriteOncePod",
        }
    }
}

fn default_state_size() -> String {
    "1Gi".to_string()
}

fn default_state_access_modes() -> Vec<AccessMode> {
    vec![AccessMode::ReadWriteOnce]
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

    /// Load the profile and fold in the fleet file's `[clusters.<name>]` entries. The fleet file
    /// is `clusters.toml` in the profile's own directory, overridable with an explicit path (which
    /// must then exist); a profile-local `[clusters.<name>]` wins on conflict.
    pub fn load_with_fleet(path: &Path, clusters_override: Option<&Path>) -> Result<Self> {
        let mut profile = Self::load(path)?;
        if let Some(fleet_path) = FleetFile::resolve_path(path, clusters_override)? {
            FleetFile::load(&fleet_path)?.merge_into(&mut profile.clusters);
        }
        Ok(profile)
    }

    /// The `[measure].cluster` entry resolved against the merged clusters map, validated:
    /// the name resolves and its secret name is non-empty. `None` when no cluster is named.
    pub fn measure_cluster(&self) -> Result<Option<(&str, &ClusterEntry)>> {
        let Some(name) = self.measure.as_ref().and_then(|m| m.cluster.as_deref()) else {
            return Ok(None);
        };
        let entry = self.clusters.get(name).with_context(|| {
            format!(
                "[measure].cluster = {name:?} does not resolve: no [clusters.{name}] in the \
                 profile or its fleet file (clusters.toml)"
            )
        })?;
        entry.validate(name)?;
        Ok(Some((name, entry)))
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

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-profile-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    const MEASURE_GPU_EAST: &str = "\n[measure]\nnamespace = \"ns\"\ncluster = \"gpu-east\"\n";

    #[test]
    fn fleet_file_clusters_resolve_and_profile_local_wins() {
        let dir = tempdir("fleet-merge");
        std::fs::write(
            dir.join("clusters.toml"),
            "[clusters.gpu-east]\nkubeconfig_secret = \"fleet-secret\"\n\
             [clusters.pirate]\nkubeconfig_secret = \"pirate-secret\"\n",
        )
        .expect("write fleet");
        let profile_path = dir.join("profile.toml");
        std::fs::write(
            &profile_path,
            format!(
                "{BASE}{MEASURE_GPU_EAST}[clusters.gpu-east]\nkubeconfig_secret = \"local-secret\"\n"
            ),
        )
        .expect("write profile");

        let p = DeployProfile::load_with_fleet(&profile_path, None).expect("loads");
        let (name, entry) = p.measure_cluster().expect("resolves").expect("named");
        assert_eq!(name, "gpu-east");
        assert_eq!(
            entry.kubeconfig_secret, "local-secret",
            "the profile-local block must win over the fleet file"
        );
        assert_eq!(
            p.clusters
                .get("pirate")
                .map(|c| c.kubeconfig_secret.as_str()),
            Some("pirate-secret"),
            "fleet-only entries still merge in"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_clusters_override_replaces_the_sibling_and_must_exist() {
        let dir = tempdir("fleet-override");
        std::fs::write(
            dir.join("clusters.toml"),
            "[clusters.gpu-east]\nkubeconfig_secret = \"sibling\"\n",
        )
        .expect("write sibling");
        let other = dir.join("other-clusters.toml");
        std::fs::write(
            &other,
            "[clusters.gpu-east]\nkubeconfig_secret = \"explicit\"\n",
        )
        .expect("write override");
        let profile_path = dir.join("profile.toml");
        std::fs::write(&profile_path, format!("{BASE}{MEASURE_GPU_EAST}")).expect("write profile");

        let p = DeployProfile::load_with_fleet(&profile_path, Some(&other)).expect("loads");
        let (_, entry) = p.measure_cluster().expect("resolves").expect("named");
        assert_eq!(entry.kubeconfig_secret, "explicit");

        let err =
            match DeployProfile::load_with_fleet(&profile_path, Some(&dir.join("missing.toml"))) {
                Err(e) => e,
                Ok(_) => panic!("a missing explicit fleet file must be an error"),
            };
        assert!(err.to_string().contains("does not exist"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_sibling_fleet_file_is_fine_but_unresolved_cluster_errors() {
        let dir = tempdir("fleet-missing");
        let profile_path = dir.join("profile.toml");
        std::fs::write(&profile_path, format!("{BASE}{MEASURE_GPU_EAST}")).expect("write profile");
        let p = DeployProfile::load_with_fleet(&profile_path, None).expect("loads without fleet");
        let err = p.measure_cluster().expect_err("gpu-east can't resolve");
        assert!(err.to_string().contains("does not resolve"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_cluster_key_reads_nothing() {
        let profile: DeployProfile = toml::from_str(BASE).expect("profile parses");
        assert!(profile.measure_cluster().expect("ok").is_none());
        assert!(profile.clusters.is_empty());
    }

    #[test]
    fn cluster_entry_rejects_unknown_fields_and_empty_secret() {
        let text = format!(
            "{BASE}{MEASURE_GPU_EAST}[clusters.gpu-east]\nkubeconfig_secret = \"s\"\ntypo = 1\n"
        );
        assert!(toml::from_str::<DeployProfile>(&text).is_err());

        let text =
            format!("{BASE}{MEASURE_GPU_EAST}[clusters.gpu-east]\nkubeconfig_secret = \"\"\n");
        let p: DeployProfile = toml::from_str(&text).expect("parses");
        let err = p.measure_cluster().expect_err("empty secret name");
        assert!(err.to_string().contains("non-empty"), "{err:#}");
    }

    #[test]
    fn proxy_url_must_be_a_supported_scheme() {
        let text = format!(
            "{BASE}{MEASURE_GPU_EAST}[clusters.gpu-east]\nkubeconfig_secret = \"s\"\nproxy_url = \"squid.example:3128\"\n"
        );
        let p: DeployProfile = toml::from_str(&text).expect("parses");
        let err = p.measure_cluster().expect_err("schemeless proxy_url");
        assert!(err.to_string().contains("http:// or socks5://"), "{err:#}");

        let text = format!(
            "{BASE}{MEASURE_GPU_EAST}[clusters.gpu-east]\nkubeconfig_secret = \"s\"\nproxy_url = \"http://10.0.0.1:3128\"\n"
        );
        let p: DeployProfile = toml::from_str(&text).expect("parses");
        assert!(p.measure_cluster().expect("valid").is_some());
    }
}
