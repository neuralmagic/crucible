use crate::deploy::profile::DeployProfile;
use crate::manifest::{AgentCfg, CompositeManifest, DeployCfg, Manifest, MeasureCfg};
use crate::openshell::gateway::{CLIENT_TLS_SECRET, ComputeDriver, GATEWAY_PORT};
use anyhow::{Context, Result};
use forge::fleet::ClusterEntry;
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::api::networking::v1 as networking;
use k8s_openapi::api::rbac::v1 as rbac;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
    LabelSelector, LabelSelectorRequirement, ObjectMeta,
};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
#[error(
    "{name}: rendering a single-domain manifest needs a [deploy] block (deploy_name and/or \
     buildah); add one, or run this domain as part of a composite instead"
)]
struct MissingDeployBlock {
    name: String,
}

/// What the loop-pod render refuses: an unimplemented topology, and the two pack-delivery
/// limits that would otherwise produce a ConfigMap the API server rejects.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error(
        "[clusters.{cluster}] has a bastion block, but the SSH tunnel is not implemented yet; \
         remove the bastion block or target a routable spoke"
    )]
    BastionUnsupported { cluster: String },
    #[error(
        "pack dir {} has no files to deliver (expected at least crucible.toml)",
        .path.display()
    )]
    EmptyPackDir { path: std::path::PathBuf },
    #[error(
        "pack at {} is {bytes} bytes, over the {CONFIGMAP_MAX_BYTES}-byte ConfigMap budget — \
         it won't fit in a ConfigMap; shrink the pack",
        .path.display()
    )]
    PackOverConfigMapBudget {
        path: std::path::PathBuf,
        bytes: usize,
    },
}

/// In-pod paths fixed by the openshell-loop template (the volume mounts the renderer always emits).
const KUBECONFIG_PATH: &str = "/etc/kube/kubeconfig";
const PUSH_AUTHFILE_PATH: &str = "/etc/quay/push.json";
pub(super) const PULL_AUTHFILE_PATH: &str = "/etc/containers/auth.json";
pub(super) const FORGE_STORAGE_ROOT: &str = "/var/lib/forge";
const KUBE_API_DIR: &str = "/var/run/kube";
const KUBECONFIG_DIR: &str = "/etc/kube";
const PUSH_AUTHFILE_DIR: &str = "/etc/quay";
/// The in-pod control bridge port (`--control-port`).
const CONTROL_PORT: u16 = 7777;
/// The `crucible ps` selector every rendered loop pod carries, so listing needs no per-domain wiring.
/// The shared `key=value` literal lives in `crucible-contract` (both crates select on it).
pub use crucible_contract::MANAGED_BY_SELECTOR as MANAGED_BY_LABEL;
/// Trace-transport mount: profile jobs write $OUT here and the in-pod broker reads (then deletes)
/// it from the same path, so the loop pod mounts the same claim writable. Must match the broker's
/// BROKER_CODEGEN_ARTIFACTS_MOUNT default.
const ARTIFACTS_MOUNT: &str = "/artifacts";
/// Mount + file for the IRSA web-identity token (publish-on-keep).
const AWS_TOKEN_DIR: &str = "/var/run/secrets/aws";
const AWS_TOKEN_PATH: &str = "/var/run/secrets/aws/token";
/// Mount + file for the spoke kubeconfig Secret (key `kubeconfig`) named by the selected
/// `[clusters.<name>]` entry; the in-pod broker reads it via BROKER_CODEGEN_KUBECONFIG.
const SPOKE_KUBECONFIG_DIR: &str = "/etc/crucible/spoke";
const SPOKE_KUBECONFIG_PATH: &str = "/etc/crucible/spoke/kubeconfig";
/// The label the OpenShell kubernetes driver sets on sandbox pods
/// (`openshell-core::driver_utils::LABEL_MANAGED_BY`). The NetworkPolicy uses it to allow
/// ingress from sandbox pods only.
const OPENSHELL_MANAGED_BY_LABEL: &str = "openshell.ai/managed-by";
const OPENSHELL_MANAGED_BY_VALUE: &str = "openshell";
/// The label the agent-sandbox controller stamps on every pod it creates from a Sandbox CR,
/// the identity CRD-path sandbox pods actually carry (the managed-by label above is
/// SPIFFE-gated in the pinned driver).
const SANDBOX_NAME_HASH_LABEL: &str = "agents.x-k8s.io/sandbox-name-hash";
/// Env var injected via the downward API carrying the loop pod's own IP. The wrapper reads it
/// and passes it into the gateway config as `host_gateway_ip`, so the kubernetes driver injects
/// the right `hostAliases` on sandbox pods.
pub(super) const POD_IP_ENV: &str = "CRUCIBLE_POD_IP";

/// Options that vary per render invocation (not per cluster).
pub struct RenderOpts {
    /// Agent iterations the wrapper runs. The controller passes its `run_iterations` knob; a manual
    /// `crucible deploy render` defaults to 1.
    pub iterations: u32,
    /// Cumulative agent-cost ceiling in USD the wrapper passes as `--max-cost` (0 = unlimited). The
    /// controller passes its `run_max_cost` knob so a dispatched run has a real budget; a manual
    /// render defaults to 0.
    pub max_cost: f64,
    /// Resolve image tags to `@sha256:…` via the in-process registry client (the footgun fix). Off
    /// emits the tag verbatim, for an air-gapped render where the registry isn't reachable.
    pub pin_digests: bool,
    /// Publish-on-keep single-repo fork (`owner/repo`): the loop opens its kept-commits draft PR here.
    /// The controller passes its per-repo default so a dispatched run publishes; a manual render leaves
    /// it `None` (no `--pr-repo`, so `crucible run` opens no PR unless the manifest's `[publish] pr_repo`
    /// names one). Composite forks ride the manifest's `[[component]].pr_repo`, not this.
    pub pr_repo: Option<String>,
    /// Pack delivery: the manifest being rendered is a controller-drafted PACK on the
    /// state PVC, NOT a domain baked into the loop image. `Some` makes the render (a) emit a ConfigMap
    /// document carrying every pack file, and (b) grow the pod an init-container that stages that
    /// ConfigMap into a writable emptyDir at the in-pod domain path the wrapper resolves, so `crucible
    /// run` finds the manifest the image never baked. `None` is a baked-domain render (the gitops path):
    /// no ConfigMap, byte-identical output to before this feature. This is an explicit flag, not a
    /// path-sniff, so the ONLY caller that gets pack delivery is the one that means it (the controller's
    /// run dispatch); a human `crucible deploy render` of a baked domain stays unchanged.
    pub pack: Option<PackDelivery>,
    /// Explicit fleet-file path (`--clusters`), overriding the `clusters.toml` sibling of the
    /// profile. `None` = the sibling, when it exists.
    pub clusters_file: Option<std::path::PathBuf>,
}

/// The knobs the controller supplies for a pack render (see [`RenderOpts::pack`]).
pub struct PackDelivery {
    /// The ConfigMap object name, the controller owns it (run-unique, derived from the pod name), and
    /// it is used for BOTH the emitted ConfigMap's `metadata.name` AND the pod volume that references
    /// it, so the two never drift. The controller creates the CM under exactly this name and owner-refs
    /// it to the created pod for cascade GC.
    pub configmap_name: String,
}

/// The in-pod mount dir for the projected Tier 2 ingest ServiceAccount token (Tier 2 ingest). The turn
/// reads `<dir>/token` and sends it as the bearer to the controller's ingest drop-box.
pub(super) const INGEST_TOKEN_DIR: &str = "/var/run/secrets/crucible.io/ingest";
/// The pod-volume name for the ingest token projection.
pub(super) const INGEST_TOKEN_VOLUME: &str = "crucible-ingest-token";
/// The ingest token's TTL. Short by design (a turn is minutes, not hours), the kubelet rotates it
/// and the API server invalidates it the instant the pod dies.
pub(super) const INGEST_TOKEN_TTL_SECS: i64 = 900;

/// The in-pod init-container mount where the pack ConfigMap is projected (read-only) before it is
/// staged into the writable domain dir.
const PACK_SRC_DIR: &str = "/opt/crucible/pack-src";
/// The pod-volume name for the pack ConfigMap projection.
const PACK_CM_VOLUME: &str = "pack-cm";
/// The pod-volume name for the writable emptyDir the pack is staged into (the domain dir the wrapper
/// resolves). Read-write: the loop writes `STEER.md`, `state/`, and the cloned workspace INTO the
/// manifest dir, so a bare read-only ConfigMap mount would break it, the init-container copy is what
/// makes the whole tree writable while still frozen-inject honest (see `command_judge`'s re-copy).
const PACK_WORKDIR_VOLUME: &str = "pack-workdir";
/// A ConfigMap's hard size limit (1 MiB of keys). Packs are capped well under this upstream (#134);
/// this is a defensive floor so an oversize pack fails the render loudly, not the kubelet silently.
const CONFIGMAP_MAX_BYTES: usize = 900 * 1024;

/// The renderer's manifest-derived inputs, factored so a plain [`crate::manifest::Manifest`]
/// (single domain) and a [`crate::manifest::CompositeManifest`] both produce one, a single domain
/// is a degenerate composite of one component. Everything the template needs from the manifest
/// flows through here; `render()` itself no longer knows which shape it came from.
pub struct RenderInput<'a> {
    /// The k8s object name (pod `<name>-loop`, the `issue` label, the RBAC's target-namespace binding).
    /// The composite's `[composite].name` for a composite (may differ from its domain directory, e.g.
    /// an issue overlay); the domain directory name for a single domain.
    pub name: &'a str,
    pub agent: &'a AgentCfg,
    pub measure_cmd: &'a str,
    pub apply_cmd: Option<&'a str>,
    /// One deploy target per component (a single domain contributes exactly one, itself).
    pub deploy_targets: Vec<DeployCfg>,
    /// The domain's codegen tool contract (`[measure]`), projected as BROKER_CODEGEN + the
    /// tools-defaults JSON. `None` unless the domain builds + measures a candidate on a GPU.
    pub measure: Option<&'a MeasureCfg>,
    /// The manifest's `[workspace].dir`. Its basename decides the IN-SANDBOX workspace path
    /// (`/sandbox/<basename>`, the openshell driver's upload rule), projected as
    /// BROKER_SANDBOX_WORKDIR so the broker's live-sandbox pull targets the real tree.
    pub workspace_dir: &'a str,
}

impl<'a> RenderInput<'a> {
    /// Project a composite manifest: one deploy target per resolved component, the composite's own
    /// name/agent/judge/world.
    pub fn from_composite(composite: &'a CompositeManifest, manifest_dir: &Path) -> Result<Self> {
        let deploy_targets = composite
            .resolve_components(manifest_dir)?
            .iter()
            .filter_map(|c| composite.deploy_for(c))
            .collect();
        Ok(Self {
            name: &composite.composite.name,
            agent: &composite.agent,
            measure_cmd: &composite.judge.measure_cmd,
            apply_cmd: composite.world.apply_cmd.as_deref(),
            deploy_targets,
            measure: composite.measure.as_ref(),
            workspace_dir: &composite.workspace.dir,
        })
    }

    /// Project a plain single-domain manifest: a degenerate composite of one component (itself), so
    /// it needs its own `[deploy]` block naming that one target.
    pub fn from_manifest(manifest: &'a Manifest, name: &'a str) -> Result<Self> {
        let deploy = manifest.deploy.as_ref().ok_or_else(|| MissingDeployBlock {
            name: name.to_owned(),
        })?;
        Ok(Self {
            name,
            agent: &manifest.agent,
            measure_cmd: &manifest.judge.measure_cmd,
            apply_cmd: manifest.world.apply_cmd.as_deref(),
            deploy_targets: vec![deploy.clone()],
            measure: manifest.measure.as_ref(),
            workspace_dir: &manifest.workspace.dir,
        })
    }
}

/// Render the loop pod + RBAC as one multi-document YAML string.
pub fn render(
    input: RenderInput<'_>,
    manifest_dir: &Path,
    manifest_file: &str,
    profile: &DeployProfile,
    opts: &RenderOpts,
) -> Result<String> {
    let image = if opts.pin_digests {
        forge::oci::pin_digest(&profile.image.loop_image, None)
            .with_context(|| format!("pinning loop image {}", profile.image.loop_image))?
    } else {
        profile.image.loop_image.clone()
    };
    // The loop image bakes the domain pack at its DIRECTORY name, which is
    // not the composite's `[composite].name` (an issue overlay may rename it). The wrapper resolves
    // `<domains_root>/<domain-dir>`.
    let domain = manifest_dir
        .file_name()
        .and_then(|n| n.to_str())
        .context("manifest dir has a name")?;
    // The openshell-loop template runs a sandboxed agent turn, so the sandbox image is required.
    let sandbox_image = input.agent.sandbox_image.clone().context(
        "the openshell-loop deploy template needs [agent].sandbox_image (the agent's sandbox)",
    )?;
    // Delegated GPU jobs: the [measure].cluster spoke, resolved + validated up front. Only a
    // measuring domain reads it; a bastioned spoke has no tunnel yet, so refuse it loudly.
    let spoke = match input.measure {
        Some(_) => profile
            .measure_cluster()?
            .map(|(name, entry)| (name.to_string(), entry.clone())),
        None => None,
    };
    if let Some((name, entry)) = &spoke
        && entry.bastion.is_some()
    {
        return Err(RenderError::BastionUnsupported {
            cluster: name.clone(),
        }
        .into());
    }
    let r = Renderer {
        input,
        domain,
        manifest_dir,
        manifest_file,
        profile,
        opts,
        image,
        sandbox_image,
        driver: profile.cluster.sandbox_driver,
        spoke,
    };

    let pod_yaml = serde_norway::to_string(&r.pod()?).context("serializing the loop pod")?;
    let rbac_yaml = serde_norway::to_string(&r.rbac()).context("serializing the RBAC")?;
    let netpol_yaml =
        serde_norway::to_string(&r.netpol()).context("serializing the NetworkPolicy")?;
    let mut out = format!("{pod_yaml}---\n{rbac_yaml}---\n{netpol_yaml}");
    // Appended, not prepended: controllers index the [pod, rbac, netpol] head layout by `kind`.
    // Apply-order within one file is irrelevant, the pod waits for its claim.
    if let Some(pvc) = r.state_pvc_doc() {
        let yaml = serde_norway::to_string(&pvc).context("serializing the state PVC")?;
        out.push_str(&format!("---\n{yaml}"));
    }
    // Under the kubernetes driver the gateway SA needs sandbox CRD RBAC + a ClusterRole for
    // tokenreviews and node reads.
    if profile.cluster.sandbox_driver == ComputeDriver::Kubernetes {
        for obj in r.sandbox_rbac() {
            let yaml = serde_norway::to_string(&obj).context("serializing sandbox RBAC")?;
            out.push_str(&format!("---\n{yaml}"));
        }
    }
    // Pack delivery: append the ConfigMap carrying the pack files. Last so a baked-domain render (no
    // pack) keeps its exact three-doc [pod, rbac, netpol] layout, the controller extracts by `kind`,
    // never by index, so ordering is free here.
    if let Some(pack) = &opts.pack {
        let cm = pack_configmap(
            manifest_dir,
            &pack.configmap_name,
            &profile.cluster.loop_namespace,
        )
        .context("building the pack ConfigMap")?;
        let cm_yaml = serde_norway::to_string(&cm).context("serializing the pack ConfigMap")?;
        out.push_str(&format!("---\n{cm_yaml}"));
    }
    Ok(out)
}

/// Collect every pack file under `manifest_dir` into a ConfigMap: text files land in `data`, any
/// non-UTF-8 file in `binaryData` (base64), and each file's key→relative-path mapping is preserved so
/// the volume that mounts this CM reproduces the pack's EXACT layout, nested `tools/measure.sh` and
/// all. ConfigMap data keys can't contain `/`, so a nested path is stored under a slash-free key while
/// its true relative path rides the volume's `items:` mapping (emitted by the pod builder). The
/// per-run `state/`, `.git/`, and any `workspace/` are skipped: they're pod-side runtime, never pack
/// content. The [`CONFIGMAP_MAX_BYTES`] guard fails an oversize pack loudly (packs are capped upstream,
/// so this only ever catches a regression).
fn pack_configmap(manifest_dir: &Path, name: &str, namespace: &str) -> Result<core::ConfigMap> {
    let files = collect_pack_files(manifest_dir)?;
    if files.is_empty() {
        return Err(RenderError::EmptyPackDir {
            path: manifest_dir.to_path_buf(),
        }
        .into());
    }
    let total: usize = files.iter().map(|(_, b)| b.len()).sum();
    if total > CONFIGMAP_MAX_BYTES {
        return Err(RenderError::PackOverConfigMapBudget {
            path: manifest_dir.to_path_buf(),
            bytes: total,
        }
        .into());
    }
    let mut data: BTreeMap<String, String> = BTreeMap::new();
    let mut binary: BTreeMap<String, k8s_openapi::ByteString> = BTreeMap::new();
    let mut used_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (rel, bytes) in &files {
        let key = unique_key(rel, &mut used_keys);
        match String::from_utf8(bytes.clone()) {
            Ok(text) => {
                data.insert(key, text);
            }
            Err(_) => {
                binary.insert(key, k8s_openapi::ByteString(bytes.clone()));
            }
        }
    }
    Ok(core::ConfigMap {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        data: (!data.is_empty()).then_some(data),
        binary_data: (!binary.is_empty()).then_some(binary),
        immutable: Some(true),
    })
}

/// The `key → relative-path` list the pod's ConfigMap volume mounts with (`items:`), so the projected
/// tree matches the pack layout exactly (subdirs and all). Deterministic: same walk + same key
/// disambiguation as [`pack_configmap`], so the CM keys and the volume items always line up.
fn pack_configmap_items(manifest_dir: &Path) -> Result<Vec<core::KeyToPath>> {
    let files = collect_pack_files(manifest_dir)?;
    let mut used_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    Ok(files
        .iter()
        .map(|(rel, _)| {
            let key = unique_key(rel, &mut used_keys);
            core::KeyToPath {
                key,
                path: rel.clone(),
                mode: None,
            }
        })
        .collect())
}

/// A slash-free, ConfigMap-legal key for a pack file's relative path. Replaces every char outside
/// `[-._a-zA-Z0-9]` (notably `/`) with `_`, then disambiguates a collision with a `-N` suffix. The
/// key spelling is cosmetic (the volume's `items:` entry carries the file's TRUE path) but it must
/// be unique and legal, so two paths that sanitize alike still get distinct keys.
fn unique_key(rel: &str, used: &mut std::collections::BTreeSet<String>) -> String {
    let base: String = rel
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if used.insert(base.clone()) {
        return base;
    }
    for n in 1.. {
        let candidate = format!("{base}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("the counter is unbounded")
}

/// Walk `manifest_dir` and return `(relative-path, bytes)` for every pack file, sorted by path (a
/// stable render). Skips pod-side runtime dirs (`state/`, `.git/`, `workspace/`) at any depth. Paths
/// use `/` separators (the in-pod layout), independent of the host OS.
fn collect_pack_files(manifest_dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .with_context(|| format!("reading pack dir {}", dir.display()))?
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("listing pack dir {}", dir.display()))?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let file_type = entry.file_type().with_context(|| format!("stat {rel}"))?;
            if file_type.is_dir() {
                // Pod-side runtime, never pack content, the loop creates these IN the pod.
                if matches!(name.as_str(), "state" | ".git" | "workspace") {
                    continue;
                }
                walk(&entry.path(), &rel, out)?;
            } else if file_type.is_file() {
                let bytes = std::fs::read(entry.path())
                    .with_context(|| format!("reading pack file {rel}"))?;
                out.push((rel, bytes));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(manifest_dir, "", &mut out)?;
    Ok(out)
}

/// One render's resolved inputs, the shared context every builder method reads, so the helpers stop
/// threading the same six values through their signatures.
struct Renderer<'a> {
    input: RenderInput<'a>,
    /// The domain pack's directory name (where the loop image baked it).
    domain: &'a str,
    /// The pack dir on disk (the manifest's parent), walked to build the ConfigMap `items:` mapping
    /// under pack delivery. Unused for a baked-domain render.
    manifest_dir: &'a Path,
    /// The manifest's file name (the wrapper passes it to the nested `crucible --manifest`).
    manifest_file: &'a str,
    profile: &'a DeployProfile,
    opts: &'a RenderOpts,
    /// The loop image, digest-resolved when pinning is on.
    image: String,
    /// The agent's sandbox image (required for the openshell template).
    sandbox_image: String,
    /// The compute driver governing sandbox scheduling.
    driver: ComputeDriver,
    /// The resolved `[measure].cluster` spoke (name + entry), when the domain measures remotely.
    spoke: Option<(String, ClusterEntry)>,
}

/// The agent pod's security context, shared by the loop pod and the turn pods: privileged ONLY
/// under the podman driver (nested rootless podman needs a privileged outer container). The
/// kubernetes driver runs sandboxes as sibling pods, so this base context is `None` and the pod
/// schedules under a restricted PSA/SCC (OpenShift `restricted-v2` included); the loop pod layers
/// AppArmor on top when the domain builds in-pod (see `loop_security_context`).
pub(crate) fn agent_security_context(driver: ComputeDriver) -> Option<core::SecurityContext> {
    (driver == ComputeDriver::Podman).then(|| core::SecurityContext {
        privileged: Some(true),
        ..Default::default()
    })
}

impl Renderer<'_> {
    /// Whether the loop pod runs buildah itself (`build_epp` / codegen builds). containerd's
    /// default AppArmor profile denies mount syscalls even inside a user namespace, which kills
    /// every in-pod build (storage init, layer extraction, chroot isolation), so those pods need
    /// `appArmorProfile: Unconfined` on the loop container — the same fix forge applies to its
    /// cluster build Jobs. Scoped to building domains only: Unconfined violates baseline PSA, and
    /// plain domains should keep scheduling under restricted PSA/SCC.
    fn builds_in_pod(&self) -> bool {
        self.input
            .deploy_targets
            .iter()
            .any(|d| d.buildah.is_some())
            || self.input.measure.is_some()
    }

    fn loop_security_context(&self) -> Option<core::SecurityContext> {
        let mut sc = agent_security_context(self.driver);
        if self.builds_in_pod() {
            sc.get_or_insert_with(Default::default).app_armor_profile =
                Some(core::AppArmorProfile {
                    type_: "Unconfined".to_string(),
                    localhost_profile: None,
                });
        }
        sc
    }

    /// The profile-named existing claim, or `<run>-state` when the profile carries a template.
    /// None = no persistence.
    fn state_claim_name(&self) -> Option<String> {
        match self.profile.cluster.state_pvc.as_ref()? {
            crate::deploy::profile::StatePvc::Existing(name) => Some(name.clone()),
            crate::deploy::profile::StatePvc::Template(_) => {
                Some(format!("{}-state", self.input.name))
            }
        }
    }

    /// Generated when `state_pvc` is a template. No ownerReferences: a static render has no owner
    /// UID to point at, and the claim outliving the pod is the point.
    fn state_pvc_doc(&self) -> Option<core::PersistentVolumeClaim> {
        let crate::deploy::profile::StatePvc::Template(t) =
            self.profile.cluster.state_pvc.as_ref()?
        else {
            return None;
        };
        let mut labels = BTreeMap::from([
            (
                crucible_contract::MANAGED_BY_KEY.to_string(),
                crucible_contract::MANAGED_BY_VALUE.to_string(),
            ),
            ("crucible/run".to_string(), self.input.name.to_string()),
        ]);
        labels.extend(t.labels.clone());
        Some(core::PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: self.state_claim_name(),
                namespace: Some(self.profile.cluster.loop_namespace.clone()),
                labels: Some(labels),
                annotations: (!t.annotations.is_empty()).then(|| t.annotations.clone()),
                ..Default::default()
            },
            spec: Some(core::PersistentVolumeClaimSpec {
                access_modes: Some(
                    t.access_modes
                        .iter()
                        .map(|m| m.as_str().to_string())
                        .collect(),
                ),
                storage_class_name: t.storage_class.clone(),
                resources: Some(core::VolumeResourceRequirements {
                    requests: Some(BTreeMap::from([(
                        "storage".to_string(),
                        k8s_openapi::apimachinery::pkg::api::resource::Quantity(t.size.clone()),
                    )])),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn pod(&self) -> Result<core::Pod> {
        let container = core::Container {
            name: "loop".to_string(),
            image: Some(self.image.clone()),
            image_pull_policy: Some("IfNotPresent".to_string()),
            command: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
            args: Some(vec![self.wrapper_script()]),
            security_context: self.loop_security_context(),
            env: Some(self.env()?),
            volume_mounts: Some(self.volume_mounts()),
            resources: Some(self.resources()),
            ..Default::default()
        };

        Ok(core::Pod {
            metadata: ObjectMeta {
                name: Some(format!("{}-loop", self.input.name)),
                namespace: Some(self.profile.cluster.loop_namespace.clone()),
                annotations: Some(BTreeMap::from([(
                    "sidecar.istio.io/inject".to_string(),
                    "false".to_string(),
                )])),
                labels: Some(BTreeMap::from([
                    ("app".to_string(), "autoresearch-local".to_string()),
                    ("issue".to_string(), self.input.name.to_string()),
                    // The `crucible ps` selector: every rendered loop pod, regardless of domain/profile.
                    (
                        crucible_contract::MANAGED_BY_KEY.to_string(),
                        crucible_contract::MANAGED_BY_VALUE.to_string(),
                    ),
                    ("crucible/run".to_string(), self.input.name.to_string()),
                ])),
                ..Default::default()
            },
            spec: Some(core::PodSpec {
                image_pull_secrets: Some(vec![core::LocalObjectReference {
                    name: self.profile.image.pull_secret.clone(),
                }]),
                service_account_name: Some(self.profile.cluster.service_account.clone()),
                automount_service_account_token: Some(false),
                // With persistent state a crash is resumable, so let the kubelet restart the pod
                // (the wrapper detects the session log and passes --resume). Without it a restart
                // would silently start a fresh run on a blank emptyDir, so the pod stays one-shot.
                restart_policy: Some(
                    if self.profile.cluster.state_pvc.is_some() {
                        "OnFailure"
                    } else {
                        "Never"
                    }
                    .to_string(),
                ),
                affinity: node_avoid_affinity(self.profile),
                host_aliases: self.host_aliases(),
                init_containers: self.init_containers(),
                containers: vec![container],
                volumes: Some(self.volumes()),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    /// The in-pod domain directory the wrapper resolves (`<domains_root>/<domain>`). For a baked domain
    /// this path already exists in the loop image (read-only rootfs content); for a PACK it is a
    /// writable emptyDir the init-container stages the ConfigMap into.
    fn domain_dir(&self) -> String {
        format!(
            "{}/{}",
            self.profile.image.domains_root.trim_end_matches('/'),
            self.domain
        )
    }

    /// The pack-staging init-container, present only under pack delivery: copy the read-only ConfigMap
    /// projection into the writable domain-dir emptyDir so the main container reads AND writes there
    /// (`STEER.md`, `state/`, the workspace clone). `-L` dereferences the ConfigMap's `..data` symlinks
    /// into real files. Reuses the loop image (it has `/bin/sh` + `cp`), no extra pull.
    fn init_containers(&self) -> Option<Vec<core::Container>> {
        let _pack = self.opts.pack.as_ref()?;
        let domain_dir = self.domain_dir();
        Some(vec![core::Container {
            name: "pack-stage".to_string(),
            image: Some(self.image.clone()),
            image_pull_policy: Some("IfNotPresent".to_string()),
            command: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
            args: Some(vec![format!(
                "set -e\nmkdir -p {domain_dir}\ncp -rL {PACK_SRC_DIR}/. {domain_dir}/\n"
            )]),
            volume_mounts: Some(vec![
                core::VolumeMount {
                    name: PACK_CM_VOLUME.to_string(),
                    mount_path: PACK_SRC_DIR.to_string(),
                    read_only: Some(true),
                    ..Default::default()
                },
                core::VolumeMount {
                    name: PACK_WORKDIR_VOLUME.to_string(),
                    mount_path: domain_dir,
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }])
    }

    /// The broker-child + gate env, projected from the manifest (the source of truth) + the profile. The
    /// engine names only what it itself consumes (`BROKER_*` it reads, forge's `FORGE_*`, the openshell
    /// backend's runtime, mount-path consts); everything domain/environment-specific flows through the
    /// profile's + `[deploy]`'s generic maps, so no domain or Vertex name is baked into the engine.
    fn env(&self) -> Result<Vec<core::EnvVar>> {
        let mut env = Vec::new();
        let plain = |name: &str, value: String| core::EnvVar {
            name: name.to_string(),
            value: Some(value),
            value_from: None,
        };

        // Generic pod env from secrets (e.g. the backend's Vertex ADC), names are the profile's.
        env.extend(secret_env_vars(self.profile));

        // The nested-sandbox runtime image (the openshell backend's contract, engine-known, not a
        // domain name, so the engine projects it).
        env.push(plain(
            "OPENSHELL_SUPERVISOR_IMAGE",
            self.profile.cluster.supervisor_image.clone(),
        ));

        // Broker / composite wiring (manifest-derived, the broker IS the engine, so these are generic).
        // BROKER_BUILD is set by crucible when it spawns the broker (from [agent.broker].build), not here.
        if self.input.agent.broker.build {
            env.push(plain("BROKER_COMPOSITE", "1".to_string()));
        }
        let ws_basename = Path::new(self.input.workspace_dir)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".to_string());
        env.push(plain(
            "BROKER_SANDBOX_WORKDIR",
            format!("/sandbox/{ws_basename}"),
        ));
        env.push(plain(
            "BROKER_COMPOSITE_CTX",
            format!("{FORGE_STORAGE_ROOT}/composite-ctx"),
        ));
        env.push(plain(
            "BROKER_MEASURE_CMD",
            self.input.measure_cmd.to_string(),
        ));
        if let Some(apply) = self.input.apply_cmd {
            env.push(plain("BROKER_COMPOSITE_APPLY_CMD", apply.to_string()));
        }

        // In-pod buildah can't mount: no /dev/fuse for the image default (overlay+fuse-overlayfs)
        // and no privileges for kernel overlay, so builds run vfs + chroot — the same recipe as
        // forge's cluster build Jobs. Pairs with the Unconfined AppArmor profile
        // (loop_security_context); both halves are needed before a build succeeds.
        if self.builds_in_pod() {
            env.push(plain("STORAGE_DRIVER", "vfs".to_string()));
            env.push(plain("BUILDAH_ISOLATION", "chroot".to_string()));
        }

        // kubectl + forge path consts fixed by the template (the broker's hooks read these).
        env.push(plain("KUBECONFIG", KUBECONFIG_PATH.to_string()));
        env.push(plain("FORGE_STORAGE_ROOT", FORGE_STORAGE_ROOT.to_string()));

        // Point every containers/image consumer at the mounted authfiles, rather than shelling out to
        // `podman login`: podman honors REGISTRY_AUTH_FILE above all other lookup paths, so the
        // gateway's inherited `podman system service` pulls a private sandbox (and a private
        // supervisor, on any registry) with whatever auth kind the file carries. Only when the
        // profile actually mounts one, an unset var falls back to podman's own lookup, whereas a
        // var pointing at an absent file is an error on the very first anonymous pull.
        if self.profile.secrets.pull_authfile.is_some() {
            env.push(plain("REGISTRY_AUTH_FILE", PULL_AUTHFILE_PATH.to_string()));
        }
        if self.profile.secrets.push_authfile.is_some() {
            env.push(plain("FORGE_AUTHFILE", PUSH_AUTHFILE_PATH.to_string()));
        }

        // Per-component build/deploy targets, from [deploy] (FORGE_* + the hook's own env names).
        for d in &self.input.deploy_targets {
            for (k, v) in d.broker_env() {
                env.push(plain(&k, v));
            }
        }

        // Codegen tool contract: the domain's [measure] becomes BROKER_CODEGEN_TOOLS_DEFAULTS, the
        // deploy profile's [measure] substrate becomes the BROKER_CODEGEN_* facts. Absent [measure]
        // => none of this is emitted.
        if let Some(measure) = self.input.measure {
            for (k, v) in measure.broker_env().map_err(anyhow::Error::msg)? {
                env.push(plain(&k, v));
            }
            if let Some(sub) = &self.profile.measure {
                for (k, v) in sub.broker_env() {
                    env.push(plain(&k, v));
                }
            }
            // Delegated spoke: the mounted kubeconfig path + the tier (named in unreachable-spoke
            // tool errors). BROKER_CODEGEN_CLUSTER itself rides broker_env() above.
            if let Some((_, entry)) = &self.spoke {
                env.push(plain(
                    "BROKER_CODEGEN_KUBECONFIG",
                    SPOKE_KUBECONFIG_PATH.to_string(),
                ));
                env.push(plain(
                    "BROKER_CODEGEN_CLUSTER_TIER",
                    entry.tier().as_str().to_string(),
                ));
                if let Some(proxy) = &entry.proxy_url {
                    env.push(plain("BROKER_CODEGEN_PROXY_URL", proxy.clone()));
                }
            }
        }

        // Publish-on-keep AWS auth (IRSA): the role + the projected sts-token path (region from profile env).
        if let Some(arn) = self.profile.cluster.aws_role_arn.as_deref()
            && !arn.is_empty()
        {
            env.push(plain("AWS_ROLE_ARN", arn.to_string()));
            env.push(plain(
                "AWS_WEB_IDENTITY_TOKEN_FILE",
                AWS_TOKEN_PATH.to_string(),
            ));
        }

        // Sandbox S3 reads: the gateway's `aws-s3` provider assumes this read-only role via the
        // same projected token and signs sandbox egress at the proxy (see openshell::run).
        if let Some(arn) = self.profile.cluster.aws_sandbox_role_arn.as_deref()
            && !arn.is_empty()
        {
            env.push(plain("CRUCIBLE_AWS_SANDBOX_ROLE_ARN", arn.to_string()));
            env.push(plain(
                "CRUCIBLE_AWS_SANDBOX_TOKEN_FILE",
                AWS_TOKEN_PATH.to_string(),
            ));
        }

        // Under the kubernetes driver, project the config the runtime `gateway_toml()` reads to
        // build the `[openshell.drivers.kubernetes]` block.
        if self.driver == ComputeDriver::Kubernetes {
            env.extend(kubernetes_sandbox_env(self.profile, &self.sandbox_image));
        }

        // Generic environment env the domain's hooks/gate need, names are the profile's. Last so
        // a profile can override a default if it ever needs to.
        for (k, v) in &self.profile.env {
            env.push(plain(k, v.clone()));
        }

        // Tier 2 ingest: when the profile names the controller's drop-box URL, tell the
        // loop where to POST its run-session (and, when the R5 collector produced one, the otel log)
        // and where its projected `crucible-ingest`-audience token is. Absent = no drop-box; the loop
        // falls back to the `SESSION` delimiter the wrapper still emits (old controller / local run).
        if let Some(ingest_url) = &self.profile.cluster.ingest_url {
            env.push(plain(crucible_contract::ENV_INGEST_URL, ingest_url.clone()));
            env.push(plain(
                crucible_contract::ENV_INGEST_TOKEN_PATH,
                format!("{INGEST_TOKEN_DIR}/token"),
            ));
        }

        // The pod's own identity from the downward API. The name makes the ingest `{pod}` path
        // segment equal the token's bound-pod claim by construction (pod-binding = run-scoping);
        // name + uid + namespace let the broker set this pod as its GPU Jobs' owner, so orphaned
        // jobs garbage-collect when the pod dies instead of holding a GPU for nobody.
        let downward = |name: &str, field_path: &str| core::EnvVar {
            name: name.to_string(),
            value: None,
            value_from: Some(core::EnvVarSource {
                field_ref: Some(core::ObjectFieldSelector {
                    field_path: field_path.to_string(),
                    api_version: None,
                }),
                ..Default::default()
            }),
        };
        env.push(downward(crucible_contract::ENV_POD_NAME, "metadata.name"));
        env.push(downward("CRUCIBLE_POD_UID", "metadata.uid"));
        env.push(downward("CRUCIBLE_POD_NAMESPACE", "metadata.namespace"));

        Ok(env)
    }

    /// The container args: the openshell-loop wrapper. Captures the pristine per-component base sha
    /// (the upstream sha the candidate diff is relative to), then runs crucible. Registry auth is not
    /// the wrapper's business: `REGISTRY_AUTH_FILE` (see [`Self::env`]) points podman at the mounted
    /// authfile, which the gateway's `podman system service` inherits.
    ///
    /// Session delivery is two-tier (Tier 2 ingest): when `CRUCIBLE_INGEST_URL` is set (a new
    /// controller injected the drop-box env), crucible POSTs `state/session.jsonl` to the Tier 2
    /// drop-box itself, so the wrapper stops dumping the (up to 128 MiB) session into the pod logs.
    /// When it is absent (an old controller, or a local run) the wrapper falls back to the
    /// `SESSION (rc=…)` delimiter contract, the controller scrapes everything after the shared
    /// [`crucible_contract::RUN_SESSION_DELIMITER`] line as the run's session log.
    fn wrapper_script(&self) -> String {
        let domain_dir = self.domain_dir();
        let sandbox_image = &self.sandbox_image;
        let iterations = self.opts.iterations;
        let max_cost = self.opts.max_cost;
        let manifest_file = self.manifest_file;
        let session_delimiter = crucible_contract::RUN_SESSION_DELIMITER;
        // Publish-on-keep: when the profile names a bucket, the loop pod publishes its run record.
        let results_bucket_flag = match &self.profile.cluster.results_bucket {
            Some(b) if !b.is_empty() => format!(" --results-bucket={b}"),
            _ => String::new(),
        };
        // Publish-on-keep fork: a manifest's `[publish] pr_repo` still overrides this at run time;
        // the push token rides the profile's secret env (`AUTORESEARCH_PR_TOKEN`).
        let pr_repo_flag = match &self.opts.pr_repo {
            Some(r) if !r.is_empty() => format!(" --pr-repo={r}"),
            _ => String::new(),
        };
        let compute_driver_flag = match self.driver {
            ComputeDriver::Kubernetes => " --compute-driver=kubernetes",
            ComputeDriver::Podman => "",
        };
        // Persistent state: a non-empty session log on the mounted PVC means a prior pod of this
        // run already produced rows, so this start is a continuation, not a fresh run.
        let resume_flag = if self.profile.cluster.state_pvc.is_some() {
            r#" $([ -s "$D/state/session.jsonl" ] && echo --resume)"#
        } else {
            ""
        };
        format!(
            r#"D={domain_dir}
crucible --manifest="$D/{manifest_file}" --ui=stream --agent-backend=openshell \
  --sandbox-image={sandbox_image} --control-port={CONTROL_PORT} --iterations={iterations} --max-cost={max_cost}{results_bucket_flag}{pr_repo_flag}{compute_driver_flag}{resume_flag}
rc=$?
if [ -z "${{CRUCIBLE_INGEST_URL:-}}" ]; then
  echo "=================== {session_delimiter}$rc) ==================="
  cat "$D/state/session.jsonl" 2>/dev/null
fi
exit $rc
"#
        )
    }

    fn volume_mounts(&self) -> Vec<core::VolumeMount> {
        let ro = |name: &str, path: &str, sub: Option<&str>| core::VolumeMount {
            name: name.to_string(),
            mount_path: path.to_string(),
            sub_path: sub.map(str::to_string),
            read_only: Some(true),
            ..Default::default()
        };
        let mut mounts = Vec::new();
        if self.profile.secrets.pull_authfile.is_some() {
            mounts.push(ro("quay-auth", PULL_AUTHFILE_PATH, Some("auth.json")));
        }
        mounts.push(ro("kube-api", KUBE_API_DIR, None));
        mounts.push(ro("kubeconfig", KUBECONFIG_DIR, None));
        if self.profile.secrets.push_authfile.is_some() {
            mounts.push(ro("quay-push", PUSH_AUTHFILE_DIR, None));
        }
        if self.spoke.is_some() {
            mounts.push(ro("spoke-kubeconfig", SPOKE_KUBECONFIG_DIR, None));
        }
        if self.profile.cluster.aws_role_arn.is_some()
            || self.profile.cluster.aws_sandbox_role_arn.is_some()
        {
            mounts.push(ro("aws-token", AWS_TOKEN_DIR, None));
        }
        // Isolated buildah storage + pulled build context + composite ctx; emptyDir = per-run fresh.
        mounts.push(core::VolumeMount {
            name: "forge-storage".to_string(),
            mount_path: FORGE_STORAGE_ROOT.to_string(),
            ..Default::default()
        });
        // Pack delivery: the staged, writable domain dir (the init-container populated it from the CM).
        if self.opts.pack.is_some() {
            mounts.push(core::VolumeMount {
                name: PACK_WORKDIR_VOLUME.to_string(),
                mount_path: self.domain_dir(),
                ..Default::default()
            });
        }
        // Persistent run state: mounted OVER the domain dir's state/ subdir so session.jsonl and
        // the agent-session files outlive the pod (the wrapper's `--resume` reads them back).
        if self.profile.cluster.state_pvc.is_some() {
            mounts.push(core::VolumeMount {
                name: "run-state".to_string(),
                mount_path: format!("{}/state", self.domain_dir()),
                ..Default::default()
            });
        }
        // Tier 2 ingest token (Tier 2 ingest): the projected `crucible-ingest`-audience token the loop
        // reads to POST its run-session. Mounted read-only, only when the drop-box URL is configured.
        if self.profile.cluster.ingest_url.is_some() {
            mounts.push(core::VolumeMount {
                name: INGEST_TOKEN_VOLUME.to_string(),
                mount_path: INGEST_TOKEN_DIR.to_string(),
                read_only: Some(true),
                ..Default::default()
            });
        }
        // Trace transport: writable, the broker deletes each trace after collecting it.
        if self
            .profile
            .measure
            .as_ref()
            .is_some_and(|m| m.artifacts_pvc.is_some())
        {
            mounts.push(core::VolumeMount {
                name: "artifacts".to_string(),
                mount_path: ARTIFACTS_MOUNT.to_string(),
                ..Default::default()
            });
        }
        mounts
    }

    fn volumes(&self) -> Vec<core::Volume> {
        let mut volumes = Vec::new();

        // The registry PULL authfile, when the profile names one.
        if let Some(secret) = &self.profile.secrets.pull_authfile {
            volumes.push(core::Volume {
                name: "quay-auth".to_string(),
                secret: Some(core::SecretVolumeSource {
                    secret_name: Some(secret.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }

        // kubectl auth: a projected, auto-rotating SA token + the cluster CA.
        volumes.push(core::Volume {
            name: "kube-api".to_string(),
            projected: Some(core::ProjectedVolumeSource {
                sources: Some(vec![
                    core::VolumeProjection {
                        service_account_token: Some(core::ServiceAccountTokenProjection {
                            expiration_seconds: Some(3600),
                            path: "token".to_string(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    core::VolumeProjection {
                        config_map: Some(core::ConfigMapProjection {
                            name: "kube-root-ca.crt".to_string(),
                            items: Some(vec![core::KeyToPath {
                                key: "ca.crt".to_string(),
                                path: "ca.crt".to_string(),
                                ..Default::default()
                            }]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        });

        volumes.push(core::Volume {
            name: "kubeconfig".to_string(),
            config_map: Some(core::ConfigMapVolumeSource {
                name: self.profile.cluster.kubeconfig_configmap.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });

        // The registry robot PUSH cred (dockerconfigjson) surfaced as /etc/quay/push.json, when named.
        if let Some(secret) = &self.profile.secrets.push_authfile {
            volumes.push(core::Volume {
                name: "quay-push".to_string(),
                secret: Some(core::SecretVolumeSource {
                    secret_name: Some(secret.clone()),
                    items: Some(vec![core::KeyToPath {
                        key: ".dockerconfigjson".to_string(),
                        path: "push.json".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }

        // The spoke kubeconfig Secret (key `kubeconfig`), mounted read-only on the loop pod only;
        // the sandbox never sees it (crucible check asserts the sandbox SA has no Secret read).
        if let Some((_, entry)) = &self.spoke {
            volumes.push(core::Volume {
                name: "spoke-kubeconfig".to_string(),
                secret: Some(core::SecretVolumeSource {
                    secret_name: Some(entry.kubeconfig_secret.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }

        // Projected SA token audience'd to AWS STS (the role trust requires aud=sts.amazonaws.com).
        if self.profile.cluster.aws_role_arn.is_some()
            || self.profile.cluster.aws_sandbox_role_arn.is_some()
        {
            volumes.push(core::Volume {
                name: "aws-token".to_string(),
                projected: Some(core::ProjectedVolumeSource {
                    sources: Some(vec![core::VolumeProjection {
                        service_account_token: Some(core::ServiceAccountTokenProjection {
                            audience: Some("sts.amazonaws.com".to_string()),
                            expiration_seconds: Some(3600),
                            path: "token".to_string(),
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }

        // Ephemeral per-run buildah storage + build context + composite ctx.
        volumes.push(core::Volume {
            name: "forge-storage".to_string(),
            empty_dir: Some(core::EmptyDirVolumeSource::default()),
            ..Default::default()
        });

        // Pack delivery: the read-only ConfigMap projection (items preserve the pack's nested layout)
        // + the writable emptyDir the init-container stages it into.
        if let Some(pack) = &self.opts.pack {
            // Re-walk the same pack dir `render()` fed the ConfigMap builder, so keys ↔ items line up.
            // An unreadable pack dir already failed the CM build in `render()`, so an empty list here
            // only ever means a flat pack (no nesting), which needs no `items:`.
            let items = pack_configmap_items(self.manifest_dir).unwrap_or_default();
            volumes.push(core::Volume {
                name: PACK_CM_VOLUME.to_string(),
                config_map: Some(core::ConfigMapVolumeSource {
                    name: pack.configmap_name.clone(),
                    items: (!items.is_empty()).then_some(items),
                    // ConfigMap volumes default files to 0644; the staging `cp` preserves that and
                    // the pack's gate scripts lose their execute bit, iteration 0 then dies with
                    // "measure produced no JSON line" (the live failure). 0755 across the pack is
                    // harmless: it's all text the loop already trusts.
                    default_mode: Some(0o755),
                    ..Default::default()
                }),
                ..Default::default()
            });
            volumes.push(core::Volume {
                name: PACK_WORKDIR_VOLUME.to_string(),
                empty_dir: Some(core::EmptyDirVolumeSource::default()),
                ..Default::default()
            });
        }

        // Tier 2 ingest token (Tier 2 ingest): a projected SA token audience'd to the ingest endpoint
        // only, useless against the kube API. Added only when the drop-box URL is configured; the
        // pod's default `automountServiceAccountToken: false` stays (this is one explicit file).
        if self.profile.cluster.ingest_url.is_some() {
            volumes.push(core::Volume {
                name: INGEST_TOKEN_VOLUME.to_string(),
                projected: Some(core::ProjectedVolumeSource {
                    sources: Some(vec![core::VolumeProjection {
                        service_account_token: Some(core::ServiceAccountTokenProjection {
                            audience: Some(crucible_contract::INGEST_TOKEN_AUDIENCE.to_string()),
                            expiration_seconds: Some(INGEST_TOKEN_TTL_SECS),
                            path: "token".to_string(),
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }

        // Persistent run state (see volume_mounts).
        if let Some(claim) = self.state_claim_name() {
            volumes.push(core::Volume {
                name: "run-state".to_string(),
                persistent_volume_claim: Some(core::PersistentVolumeClaimVolumeSource {
                    claim_name: claim,
                    ..Default::default()
                }),
                ..Default::default()
            });
        }

        // Trace transport: the same claim the broker mounts on profile jobs (they write $OUT there,
        // the in-pod broker collects it from ARTIFACTS_MOUNT). RWX-backed, shared with the jobs.
        if let Some(pvc) = self
            .profile
            .measure
            .as_ref()
            .and_then(|m| m.artifacts_pvc.as_ref())
        {
            volumes.push(core::Volume {
                name: "artifacts".to_string(),
                persistent_volume_claim: Some(core::PersistentVolumeClaimVolumeSource {
                    claim_name: pvc.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }

        volumes
    }

    fn resources(&self) -> core::ResourceRequirements {
        resources(self.profile)
    }

    /// The spoke's `host_aliases` (hostname -> IP) as pod-spec hostAliases, grouped by IP with
    /// hostnames sorted (BTreeMap order) so the render is deterministic. The DNS-dark tier.
    fn host_aliases(&self) -> Option<Vec<core::HostAlias>> {
        let (_, entry) = self.spoke.as_ref()?;
        if entry.host_aliases.is_empty() {
            return None;
        }
        let mut by_ip: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (hostname, ip) in &entry.host_aliases {
            by_ip.entry(ip.as_str()).or_default().push(hostname.clone());
        }
        Some(
            by_ip
                .into_iter()
                .map(|(ip, hostnames)| core::HostAlias {
                    ip: ip.to_string(),
                    hostnames: Some(hostnames),
                })
                .collect(),
        )
    }

    /// NetworkPolicy on the loop pod. Under `podman` the sandbox is nested inside the loop pod and
    /// reaches the broker over the pod-internal bridge (traffic a NetworkPolicy never sees), so this
    /// is a pure deny-all-ingress lockdown. Under `kubernetes` the sandbox is a sibling pod, so
    /// gateway (:17670) and broker (:8849) traffic becomes real cluster networking. The policy then
    /// allows ingress **only** from sandbox pods, **only** on those two ports. Sandbox pods are
    /// matched by either identity they may carry: the OpenShell managed-by label (the driver only
    /// stamps it on SPIFFE-enabled pods since the CRD path landed), or the agent-sandbox
    /// controller's name-hash label, which every Sandbox-CR pod gets. Dropping either selector
    /// silently strands the supervisor's dial-back (the policy-load timeout, not a crash). kubectl
    /// port-forward traffic arrives via the kubelet, which NetworkPolicy does not govern, so it
    /// survives either way. Defense in depth alongside the broker's bearer token. Egress is
    /// untouched (registry, k8s API, forges, Vertex).
    fn netpol(&self) -> networking::NetworkPolicy {
        let ingress = match self.driver {
            ComputeDriver::Podman => None,
            ComputeDriver::Kubernetes => Some(vec![networking::NetworkPolicyIngressRule {
                from: Some(vec![
                    networking::NetworkPolicyPeer {
                        pod_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([(
                                OPENSHELL_MANAGED_BY_LABEL.to_string(),
                                OPENSHELL_MANAGED_BY_VALUE.to_string(),
                            )])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    networking::NetworkPolicyPeer {
                        pod_selector: Some(LabelSelector {
                            match_expressions: Some(vec![LabelSelectorRequirement {
                                key: SANDBOX_NAME_HASH_LABEL.to_string(),
                                operator: "Exists".to_string(),
                                values: None,
                            }]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ]),
                ports: Some(vec![
                    networking::NetworkPolicyPort {
                        port: Some(IntOrString::Int(GATEWAY_PORT.into())),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    },
                    networking::NetworkPolicyPort {
                        // The manifest's `[agent.broker].bind` port, never a second constant: a
                        // domain that moves the broker must not silently lose its ingress rule.
                        port: Some(IntOrString::Int(broker_ingress_port(
                            &self.input.agent.broker.bind,
                        ))),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    },
                ]),
            }]),
        };
        networking::NetworkPolicy {
            metadata: ObjectMeta {
                name: Some(format!("{}-loop-deny-ingress", self.input.name)),
                namespace: Some(self.profile.cluster.loop_namespace.clone()),
                ..Default::default()
            },
            spec: Some(networking::NetworkPolicySpec {
                pod_selector: Some(LabelSelector {
                    match_labels: Some(BTreeMap::from([(
                        "crucible/run".to_string(),
                        self.input.name.to_string(),
                    )])),
                    ..Default::default()
                }),
                policy_types: Some(vec!["Ingress".to_string()]),
                ingress,
                ..Default::default()
            }),
        }
    }

    /// The cross-namespace `edit` RoleBinding: the loop SA (in the loop ns) gets `edit` in the system-under-test namespace so
    /// the broker's kubectl hooks can roll its Deployments.
    fn rbac(&self) -> rbac::RoleBinding {
        role_binding(
            format!("{}-edit", self.profile.cluster.service_account),
            self.profile.cluster.rig_namespace.clone(),
            "ClusterRole",
            "edit",
            self.profile.cluster.service_account.clone(),
            self.profile.cluster.loop_namespace.clone(),
        )
    }

    /// The RBAC the kubernetes sandbox driver needs, cross-checked against OpenShell's own helm
    /// chart (`role.yaml`, `clusterrole.yaml`):
    ///
    ///  1. A namespaced `Role` in the loop namespace: sandbox CRD verbs + event/pod reads +
    ///     secret writes (the loop pod publishes the sandbox client-TLS Secret).
    ///  2. A `RoleBinding` granting that Role to the loop SA.
    ///  3. A `ClusterRole`: `tokenreviews` (create) + `nodes` (get/list/watch).
    ///  4. A `ClusterRoleBinding` granting it to the loop SA.
    fn sandbox_rbac(&self) -> Vec<SandboxRbacObject> {
        let sa = &self.profile.cluster.service_account;
        let ns = &self.profile.cluster.loop_namespace;
        let role_name = format!("{sa}-sandbox");
        let cr_name = format!("{sa}-node-reader");

        // 1. Namespaced Role: sandbox CRD + events + pods + the published client-TLS secret.
        let role = rbac::Role {
            metadata: ObjectMeta {
                name: Some(role_name.clone()),
                namespace: Some(ns.clone()),
                ..Default::default()
            },
            rules: Some(vec![
                rbac::PolicyRule {
                    api_groups: Some(vec!["agents.x-k8s.io".to_string()]),
                    resources: Some(vec![
                        "sandboxes".to_string(),
                        "sandboxes/status".to_string(),
                    ]),
                    verbs: vec![
                        "create".to_string(),
                        "delete".to_string(),
                        "get".to_string(),
                        "list".to_string(),
                        "patch".to_string(),
                        "update".to_string(),
                        "watch".to_string(),
                    ],
                    ..Default::default()
                },
                rbac::PolicyRule {
                    api_groups: Some(vec![String::new()]),
                    resources: Some(vec!["events".to_string()]),
                    verbs: vec!["get".to_string(), "list".to_string(), "watch".to_string()],
                    ..Default::default()
                },
                rbac::PolicyRule {
                    api_groups: Some(vec![String::new()]),
                    resources: Some(vec!["pods".to_string()]),
                    verbs: vec!["get".to_string()],
                    ..Default::default()
                },
                // The loop pod publishes the client-TLS Secret sandbox pods mount to dial the
                // gateway back over mTLS (server-side apply = create on first boot, patch on
                // cert refresh). Two rules because RBAC can't constrain `create` by
                // resourceNames (the name doesn't exist yet at admission), while get/patch
                // stay pinned to the one Secret we own.
                rbac::PolicyRule {
                    api_groups: Some(vec![String::new()]),
                    resources: Some(vec!["secrets".to_string()]),
                    verbs: vec!["create".to_string()],
                    ..Default::default()
                },
                rbac::PolicyRule {
                    api_groups: Some(vec![String::new()]),
                    resources: Some(vec!["secrets".to_string()]),
                    resource_names: Some(vec![CLIENT_TLS_SECRET.to_string()]),
                    verbs: vec!["get".to_string(), "patch".to_string()],
                    ..Default::default()
                },
            ]),
        };

        let rb = role_binding(
            role_name.clone(),
            ns.clone(),
            "Role",
            &role_name,
            sa.clone(),
            ns.clone(),
        );

        // 3. ClusterRole: tokenreviews (IssueSandboxToken bootstrap) + nodes.
        let cr = rbac::ClusterRole {
            metadata: ObjectMeta {
                name: Some(cr_name.clone()),
                ..Default::default()
            },
            rules: Some(vec![
                rbac::PolicyRule {
                    api_groups: Some(vec!["authentication.k8s.io".to_string()]),
                    resources: Some(vec!["tokenreviews".to_string()]),
                    verbs: vec!["create".to_string()],
                    ..Default::default()
                },
                rbac::PolicyRule {
                    api_groups: Some(vec![String::new()]),
                    resources: Some(vec!["nodes".to_string()]),
                    verbs: vec!["get".to_string(), "list".to_string(), "watch".to_string()],
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let crb = rbac::ClusterRoleBinding {
            metadata: ObjectMeta {
                name: Some(cr_name.clone()),
                ..Default::default()
            },
            role_ref: rbac::RoleRef {
                api_group: "rbac.authorization.k8s.io".to_string(),
                kind: "ClusterRole".to_string(),
                name: cr_name,
            },
            subjects: Some(vec![rbac::Subject {
                kind: "ServiceAccount".to_string(),
                name: sa.clone(),
                namespace: Some(ns.clone()),
                ..Default::default()
            }]),
        };

        vec![
            SandboxRbacObject::Role(role),
            SandboxRbacObject::RoleBinding(rb),
            SandboxRbacObject::ClusterRole(cr),
            SandboxRbacObject::ClusterRoleBinding(crb),
        ]
    }
}

/// A typed wrapper so `sandbox_rbac()` can return a mixed vec of RBAC objects and they all
/// serialize through one `serde_norway::to_string` call.
enum SandboxRbacObject {
    Role(rbac::Role),
    RoleBinding(rbac::RoleBinding),
    ClusterRole(rbac::ClusterRole),
    ClusterRoleBinding(rbac::ClusterRoleBinding),
}

impl serde::Serialize for SandboxRbacObject {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Role(r) => r.serialize(s),
            Self::RoleBinding(r) => r.serialize(s),
            Self::ClusterRole(r) => r.serialize(s),
            Self::ClusterRoleBinding(r) => r.serialize(s),
        }
    }
}

/// A `RoleBinding` naming `binding_name`, granting `role_kind`/`role_name` in `target_ns` to
/// `subject_sa` (a `ServiceAccount` in `subject_ns`). Shared by the loop pod's cross-namespace `edit`
/// binding and the controller's same-namespace pod-watch `Role` binding, one RBAC render, two callers.
pub(in crate::deploy) fn role_binding(
    binding_name: String,
    target_ns: String,
    role_kind: &str,
    role_name: &str,
    subject_sa: String,
    subject_ns: String,
) -> rbac::RoleBinding {
    rbac::RoleBinding {
        metadata: ObjectMeta {
            name: Some(binding_name),
            namespace: Some(target_ns),
            ..Default::default()
        },
        role_ref: rbac::RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: role_kind.to_string(),
            name: role_name.to_string(),
        },
        subjects: Some(vec![rbac::Subject {
            kind: "ServiceAccount".to_string(),
            name: subject_sa,
            namespace: Some(subject_ns),
            ..Default::default()
        }]),
    }
}

/// The broker's ingress port for the NetworkPolicy, parsed from `[agent.broker].bind`. A bind the
/// engine cannot parse falls back to the documented default rather than to `0`, which would render a
/// policy that silently drops every broker call.
fn broker_ingress_port(bind: &str) -> i32 {
    const DEFAULT_BROKER_PORT: i32 = 8849;
    crate::manifest::broker_port(bind)
        .parse()
        .unwrap_or(DEFAULT_BROKER_PORT)
}

/// The pod's CPU/memory requests+limits, from the profile's `[resources]`. Shared by the loop pod
/// and a turn pod (both are the same agent-turn shape, resource-wise).
pub(super) fn resources(profile: &DeployProfile) -> core::ResourceRequirements {
    let r = &profile.resources;
    core::ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(r.cpu_request.clone())),
            ("memory".to_string(), Quantity(r.mem_request.clone())),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(r.cpu_limit.clone())),
            ("memory".to_string(), Quantity(r.mem_limit.clone())),
        ])),
        ..Default::default()
    }
}

/// The profile's generic `secretKeyRef` pod env (e.g. the backend's Vertex ADC). Shared: a turn pod
/// is the same caller as a loop pod, so it needs the same credential env to mint a Vertex token.
pub(super) fn secret_env_vars(profile: &DeployProfile) -> Vec<core::EnvVar> {
    profile
        .secret_env
        .iter()
        .map(|se| core::EnvVar {
            name: se.name.clone(),
            value: None,
            value_from: Some(core::EnvVarSource {
                secret_key_ref: Some(core::SecretKeySelector {
                    name: se.secret.clone(),
                    key: se.key.clone(),
                    optional: None,
                }),
                ..Default::default()
            }),
        })
        .collect()
}

/// The env the runtime `gateway_toml()` reads to build the `[openshell.drivers.kubernetes]` block,
/// so the openshell backend resolves/pulls the sandbox image through the kubelet's
/// `imagePullSecrets` (a real Kubernetes pull) instead of falling back to the podman driver's
/// nested, authfile-based pull. Shared by the loop pod and a turn pod: whichever process boots the
/// gateway, under `sandbox_driver = "kubernetes"` it must see this env, or `ComputeDriver::default()`
/// (`--compute-driver` unset) silently reverts to podman and the sandbox image pull is authenticated
/// with the wrong credential. `status.podIP` (unknowable at render time) is the `host_gateway_ip`
/// that makes sandbox hostAliases work; the rest comes from the profile.
pub(super) fn kubernetes_sandbox_env(
    profile: &DeployProfile,
    sandbox_image: &str,
) -> Vec<core::EnvVar> {
    let plain = |name: &str, value: String| core::EnvVar {
        name: name.to_string(),
        value: Some(value),
        value_from: None,
    };
    let mut env = vec![core::EnvVar {
        name: POD_IP_ENV.to_string(),
        value: None,
        value_from: Some(core::EnvVarSource {
            field_ref: Some(core::ObjectFieldSelector {
                field_path: "status.podIP".to_string(),
                api_version: None,
            }),
            ..Default::default()
        }),
    }];
    // Sandboxes run in the loop/turn pod's own namespace. Keeps the NetworkPolicy a simple
    // podSelector and avoids provisioning a second ServiceAccount.
    env.push(plain(
        "CRUCIBLE_SANDBOX_NAMESPACE",
        profile.cluster.loop_namespace.clone(),
    ));
    env.push(plain(
        "CRUCIBLE_SANDBOX_SERVICE_ACCOUNT",
        profile.cluster.service_account.clone(),
    ));
    env.push(plain(
        "CRUCIBLE_SANDBOX_DEFAULT_IMAGE",
        sandbox_image.to_string(),
    ));
    // The kubelet pulls the sandbox image, so this must be a `kubernetes.io/dockerconfigjson`
    // secret. `[secrets].pull_authfile` is NOT one: it is an Opaque secret holding an
    // `auth.json` for the nested podman, and handing it to `imagePullSecrets` yields an
    // ImagePullBackOff complaining about a missing `.dockerconfigjson` key.
    if !profile.image.pull_secret.is_empty() {
        env.push(plain(
            "CRUCIBLE_SANDBOX_IMAGE_PULL_SECRETS",
            profile.image.pull_secret.clone(),
        ));
    }
    env.push(plain(
        "CRUCIBLE_SANDBOX_APP_ARMOR_PROFILE",
        "Unconfined".to_string(),
    ));
    env
}

/// The profile's node avoid-list as a required nodeAffinity `NotIn` on `kubernetes.io/hostname`
/// (nodes the operator marked bad, e.g. broken CNI). `None` when the list is empty, so a profile
/// without one renders byte-identically to before. Shared by every pod the profile renders: the
/// loop pod, the turn pods, and the controller Deployment's template.
pub(in crate::deploy) fn node_avoid_affinity(profile: &DeployProfile) -> Option<core::Affinity> {
    let avoid = &profile.cluster.avoid_nodes;
    if avoid.is_empty() {
        return None;
    }
    Some(core::Affinity {
        node_affinity: Some(core::NodeAffinity {
            required_during_scheduling_ignored_during_execution: Some(core::NodeSelector {
                node_selector_terms: vec![core::NodeSelectorTerm {
                    match_expressions: Some(vec![core::NodeSelectorRequirement {
                        key: "kubernetes.io/hostname".to_string(),
                        operator: "NotIn".to_string(),
                        values: Some(avoid.clone()),
                    }]),
                    ..Default::default()
                }],
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::render::turn::{TurnKind, TurnOpts, render_turn};

    /// The synthetic gamma composite fixture, the render tests' stand-in for a real composite
    /// domain pack, which lives out of tree.
    fn fixture_gamma_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/domains/gamma")
    }

    fn fixture_profile(overlay: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "tests/fixtures/deploy/gamma/{overlay}/profile.toml"
        ))
    }

    /// A composite overlay manifest + a kubernetes-driver profile must render a valid, fully-projected
    /// loop pod + RBAC from config alone (no engine special-casing). This is the end-to-end canary:
    /// change the manifest/profile shape and it fails here before an operator hits it. Pinning off so
    /// it never touches a registry.
    #[test]
    fn composite_overlay_renders_from_manifest_plus_profile() {
        let dir = fixture_gamma_dir();
        let manifest = dir.join("crucible.delta.toml");
        let composite = CompositeManifest::load(&manifest).expect("composite parses");
        let profile = DeployProfile::load(&fixture_profile("delta")).expect("profile parses");
        let input = RenderInput::from_composite(&composite, &dir).expect("render input");
        let yaml = render(
            input,
            &dir,
            "crucible.delta.toml",
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render");

        // This profile runs the kubernetes sandbox driver, so the render also carries the
        // sandbox CRD RBAC (Role + RoleBinding + ClusterRole + ClusterRoleBinding) alongside the base
        // pod + edit RoleBinding + netpol.
        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        assert_eq!(
            docs.len(),
            7,
            "pod + edit RoleBinding + netpol + sandbox Role + sandbox RoleBinding + ClusterRole + \
             ClusterRoleBinding: {yaml}"
        );
        assert!(docs[0].contains("kind: Pod"));
        assert!(docs[1].contains("kind: RoleBinding"));
        assert!(docs[2].contains("kind: NetworkPolicy"));
        assert!(
            docs.iter().any(|d| d.contains("kind: Role")
                && d.contains("sandbox")
                && d.contains("agents.x-k8s.io")),
            "sandbox Role: {yaml}"
        );
        assert!(
            docs.iter().any(|d| d.contains("kind: ClusterRole")
                && !d.contains("ClusterRoleBinding")
                && d.contains("tokenreviews")),
            "sandbox ClusterRole: {yaml}"
        );
        assert!(
            docs.iter().any(|d| d.contains("kind: ClusterRoleBinding")),
            "sandbox ClusterRoleBinding: {yaml}"
        );

        // Manifest-derived env (the broker contract the engine owns).
        assert!(yaml.contains("name: BROKER_MEASURE_CMD"));
        assert!(yaml.contains("value: delta-bench"), "judge.measure_cmd");
        assert!(yaml.contains("value: delta-apply"), "world.apply_cmd");
        assert!(
            yaml.contains("value: '1'"),
            "BROKER_COMPOSITE from broker.build"
        );

        // [deploy]-projected build targets (forge contract + the hook's own names).
        assert!(
            yaml.contains("value: registry.example.com/alpha-candidate"),
            "FORGE_REGISTRY"
        );
        assert!(
            yaml.contains("name: BETA_CANDIDATE_REPO"),
            "beta hook env passed through (the apply hook derives the base ref from it)"
        );
        assert!(
            yaml.contains("name: ALPHA_DEPLOY"),
            "alpha hook env passed through"
        );

        // Profile-projected env + secret env (domain/vendor names live in config, not the engine).
        assert!(yaml.contains("name: GCLOUD_CREDENTIALS"));
        assert!(yaml.contains("secretKeyRef"));
        assert!(yaml.contains("name: RIG_ENDPOINT"));
        assert!(yaml.contains("name: BENCH_MODEL"));

        // The wrapper resolves the domain DIR (gamma), not the composite name (delta).
        assert!(yaml.contains("D=/opt/crucible/domains/gamma"));
        // Publishing is native now (crucible opens the per-fork draft PRs in-process), so the wrapper no
        // longer captures/emits per-component base shas for a downstream log-parsing publisher.
        assert!(
            !yaml.contains("BASE_SHA") && !yaml.contains("base-shas"),
            "no base-sha log-emission hack: {yaml}"
        );

        // Registry auth reaches the gateway's podman as REGISTRY_AUTH_FILE, not a `podman login`, so it
        // is not pinned to the single registry parsed out of the sandbox image.
        assert!(yaml.contains("name: REGISTRY_AUTH_FILE"));
        assert!(!yaml.contains("podman login"));

        // Publish-on-keep: results_bucket -> --results-bucket, plus the IRSA role env + sts-audience token.
        assert!(
            yaml.contains("--results-bucket=s3://"),
            "wrapper passes results_bucket"
        );
        assert!(yaml.contains("name: AWS_ROLE_ARN"), "IRSA role env");
        assert!(
            yaml.contains("audience: sts.amazonaws.com"),
            "sts-audience token projected"
        );

        // RBAC binds the loop SA to edit in the system-under-test namespace.
        assert!(docs[1].contains("name: autoresearch-publisher-edit"));
        assert!(docs[1].contains("name: edit"));

        // The `crucible ps` selector labels, on every rendered loop pod.
        assert!(
            docs[0].contains("app.kubernetes.io/managed-by: crucible"),
            "ps selector label"
        );
        assert!(docs[0].contains("crucible/run: delta"), "run label");

        // The ingress lockdown: selects exactly this run's pod, denies all ingress by default, leaves
        // egress alone (no Egress in policyTypes). Under the kubernetes sandbox driver the sandbox is a
        // real sibling pod, so the netpol carries an explicit allow rule for it on the gateway
        // (17670) and broker (8849) ports, the sandbox->broker path is real cluster networking here,
        // not the podman-bridge traffic a NetworkPolicy never sees.
        assert!(
            docs[2].contains("name: delta-loop-deny-ingress"),
            "netpol named per run"
        );
        assert!(
            docs[2].contains("crucible/run: delta"),
            "netpol selects this run's pod"
        );
        assert!(docs[2].contains("- Ingress"), "policyTypes: [Ingress]");
        assert!(
            !docs[2].contains("- Egress"),
            "no egress lockdown: {}",
            docs[2]
        );
        assert!(
            docs[2].contains("openshell.ai/managed-by: openshell"),
            "sandbox ingress allow rule (kubernetes driver): {}",
            docs[2]
        );

        // Publish-on-keep: with no `pr_repo` opt (the None above), the wrapper emits no `--pr-repo`.
        assert!(
            !yaml.contains("--pr-repo"),
            "no --pr-repo flag when RenderOpts.pr_repo is None"
        );
    }

    /// The controller's per-repo publish default reaches the loop: a `RenderOpts.pr_repo` renders as the
    /// wrapper's `--pr-repo=<fork>`, so a kept candidate opens its draft PR against that fork. An empty
    /// string (a repo with no mapping) must render NO flag, so `crucible run` falls back to the manifest's
    /// own `[publish] pr_repo` (or opens nothing) rather than being handed `--pr-repo=`.
    #[test]
    fn wrapper_emits_pr_repo_flag_when_set() {
        let dir = fixture_gamma_dir();
        let manifest = dir.join("crucible.delta.toml");
        let profile = DeployProfile::load(&fixture_profile("delta")).expect("profile parses");

        let render_with = |pr_repo: Option<&str>| {
            let composite = CompositeManifest::load(&manifest).expect("composite parses");
            let input = RenderInput::from_composite(&composite, &dir).expect("render input");
            render(
                input,
                &dir,
                "crucible.delta.toml",
                &profile,
                &RenderOpts {
                    iterations: 1,
                    max_cost: 0.0,
                    pin_digests: false,
                    pr_repo: pr_repo.map(str::to_string),
                    pack: None,
                    clusters_file: None,
                },
            )
            .expect("render")
        };

        let set = render_with(Some("wseaton/relay-testbed"));
        assert!(
            set.contains("--pr-repo=wseaton/relay-testbed"),
            "wrapper forwards the fork as --pr-repo: {set}"
        );

        // An empty mapping renders no flag (not `--pr-repo=`), so the manifest's own publish target wins.
        let empty = render_with(Some(""));
        assert!(
            !empty.contains("--pr-repo"),
            "an empty pr_repo renders no flag: {empty}"
        );
    }

    /// A second overlay + a podman-driver profile must render the same way delta does, plus the
    /// overlay's distinguishing piece: the broker's native JIRA Cloud env projected from the profile
    /// (JIRA_URL + JIRA_USERNAME, with the API token sourced from a secretKeyRef, never a literal).
    /// Catches a drift in the overlay manifest/profile shape before an operator hits it. Pinning off
    /// so it never touches a registry.
    #[test]
    fn jira_profile_overlay_renders_from_manifest_plus_profile() {
        let dir = fixture_gamma_dir();
        let manifest = dir.join("crucible.omega.toml");
        let composite = CompositeManifest::load(&manifest).expect("composite parses");
        let profile = DeployProfile::load(&fixture_profile("omega")).expect("profile parses");
        let input = RenderInput::from_composite(&composite, &dir).expect("render input");
        let yaml = render(
            input,
            &dir,
            "crucible.omega.toml",
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render");

        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        assert_eq!(docs.len(), 3, "pod + rbac + netpol");

        // Manifest-derived broker contract (the overlay hooks + gate).
        assert!(yaml.contains("value: gamma-bench"), "judge.measure_cmd");
        assert!(yaml.contains("value: omega-apply"), "world.apply_cmd");

        // [deploy]-projected build targets for BOTH components (the overlay candidate repos + deploy names).
        assert!(
            yaml.contains("value: registry.example.com/alpha-candidate"),
            "alpha FORGE_REGISTRY"
        );
        assert!(yaml.contains("name: ALPHA_DEPLOY"), "alpha hook env");
        assert!(yaml.contains("name: BETA_DEPLOY"), "beta hook env");
        assert!(
            yaml.contains("name: BETA_CANDIDATE_REPO"),
            "beta derive base (the apply hook derives the base ref from it)"
        );

        // The overlay's distinguishing env: the broker's NATIVE JIRA Cloud client switched on via the
        // profile (your-org.atlassian.net basic auth), with the API token sourced from a secret (never a
        // literal in the rendered spec).
        assert!(yaml.contains("name: JIRA_URL"), "JIRA Cloud base URL");
        assert!(
            yaml.contains("name: JIRA_USERNAME"),
            "JIRA Cloud basic-auth user"
        );
        assert!(
            yaml.contains("name: JIRA_API_TOKEN"),
            "Cloud API token env present"
        );
        assert!(
            yaml.contains("name: jira-cloud"),
            "token from a secretKeyRef"
        );
        assert!(
            !yaml.contains("JIRA_PERSONAL_TOKEN") && !yaml.contains("mcp-atlassian"),
            "native Cloud client, not the old Server/DC mcp-atlassian proxy"
        );

        // The overlay's frozen-workload knob the gate reads.
        assert!(yaml.contains("name: BENCH_ADAPTER_MIX"), "adapter mix");

        // The wrapper resolves the domain DIR (gamma), not the composite name (omega).
        assert!(yaml.contains("D=/opt/crucible/domains/gamma"));

        // The `crucible ps` selector labels, on every rendered loop pod.
        assert!(docs[0].contains("app.kubernetes.io/managed-by: crucible"));
        assert!(docs[0].contains("crucible/run: omega"));
    }

    /// A plain single-domain manifest (no `[composite]` table) plus its `[deploy]` block must render the
    /// same loop pod + RBAC shape as a composite, a single domain is a degenerate composite of one
    /// component. Mirrors the two composite render tests above; inline TOML, no fixture file on disk.
    #[test]
    fn single_domain_renders_from_manifest_plus_profile() {
        let manifest: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "shrink p95"
            sandbox_image = "registry.example.com/alpha-sandbox:latest"
            [judge]
            measure_cmd = "./measure.nu"
            direction = "lower"
            [world]
            apply_cmd = "./apply.sh"
            [deploy]
            deploy_name = "alpha"
            [deploy.buildah]
            registry = "registry.example.com/alpha-candidate"
            dockerfile = "Dockerfile"
            [deploy.env]
            ALPHA_DEPLOY = "alpha"
        "#,
        )
        .expect("manifest parses");

        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
        "#,
        )
        .expect("profile parses");

        let dir = std::path::Path::new("/opt/crucible/domains/alpha");
        let input = RenderInput::from_manifest(&manifest, "alpha").expect("render input");
        let yaml = render(
            input,
            dir,
            "crucible.toml",
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render");

        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        assert_eq!(docs.len(), 3, "pod + rbac + netpol");
        assert!(docs[0].contains("kind: Pod"));
        assert!(docs[1].contains("kind: RoleBinding"));
        assert!(docs[2].contains("kind: NetworkPolicy"));
        assert!(
            docs[0].contains("name: alpha-loop"),
            "pod named from [deploy]'s name arg"
        );

        assert!(
            yaml.contains("value: registry.example.com/alpha-candidate"),
            "FORGE_REGISTRY from [deploy].buildah"
        );
        assert!(
            yaml.contains("name: ALPHA_DEPLOY"),
            "hook env passed through"
        );
        // This profile names no [secrets]. Nothing may claim a credential that was never mounted:
        // a REGISTRY_AUTH_FILE pointing at an absent path breaks even an anonymous pull.
        assert!(
            !yaml.contains("REGISTRY_AUTH_FILE"),
            "no pull authfile mounted => no REGISTRY_AUTH_FILE"
        );
        assert!(
            !yaml.contains("FORGE_AUTHFILE"),
            "no push authfile mounted => no FORGE_AUTHFILE"
        );
        assert!(
            !yaml.contains("podman login"),
            "registry auth is env, never a login shell-out with a password on argv"
        );
        assert!(
            yaml.contains("value: ./apply.sh"),
            "world.apply_cmd projected as BROKER_COMPOSITE_APPLY_CMD"
        );
        assert!(
            yaml.contains("value: ./measure.nu"),
            "judge.measure_cmd projected as BROKER_MEASURE_CMD"
        );
        assert!(
            yaml.contains("D=/opt/crucible/domains/alpha"),
            "wrapper resolves the domain dir"
        );

        // The `crucible ps` selector labels, on a single-domain render too.
        assert!(docs[0].contains("app.kubernetes.io/managed-by: crucible"));
        assert!(docs[0].contains("crucible/run: alpha"));

        // No avoid-list in the profile, no affinity stanza, the pre-avoid_nodes output verbatim.
        assert!(!yaml.contains("affinity"), "no affinity key: {yaml}");
        // Backward compat: a baked-domain render (no pack) emits NO ConfigMap and no pack-staging.
        assert!(
            !yaml.contains("kind: ConfigMap"),
            "a baked-domain render carries no pack ConfigMap: {yaml}"
        );
        assert!(
            !yaml.contains("initContainers") && !yaml.contains("pack-stage"),
            "no pack-staging init-container on a baked-domain render"
        );
    }

    /// A minimal single-domain loop manifest (openshell backend), shared by the run-session tests.
    fn loop_manifest() -> Manifest {
        toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "shrink p95"
            sandbox_image = "registry.example.com/alpha-sandbox:latest"
            [judge]
            measure_cmd = "./measure.nu"
            direction = "lower"
            [world]
            apply_cmd = "./apply.sh"
            [deploy]
            deploy_name = "alpha"
            [deploy.buildah]
            registry = "registry.example.com/alpha-candidate"
            dockerfile = "Dockerfile"
            [deploy.env]
            ALPHA_DEPLOY = "alpha"
        "#,
        )
        .expect("manifest parses")
    }

    fn render_loop_pod(profile: &DeployProfile) -> String {
        let manifest = loop_manifest();
        let dir = std::path::Path::new("/opt/crucible/domains/alpha");
        let input = RenderInput::from_manifest(&manifest, "alpha").expect("render input");
        render(
            input,
            dir,
            "crucible.toml",
            profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render")
    }

    /// A loop pod with `[cluster].ingest_url` set grows the Tier 2 run-session scaffolding: the
    /// `CRUCIBLE_INGEST_URL`/`_TOKEN_PATH`/`POD_NAME` env, a projected
    /// `crucible-ingest`-audience token volume + mount, and (the behavioral switch) the wrapper
    /// GUARDS the `SESSION` delimiter behind the env's absence so a new-controller loop stops dumping
    /// the (up to 128 MiB) session into the pod logs (crucible POSTs it to the drop-box instead).
    #[test]
    fn loop_pod_with_ingest_url_delivers_the_session_over_the_dropbox() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            ingest_url = "http://crucible-controller.autoresearch.svc:8080"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
        "#,
        )
        .expect("profile parses");

        let yaml = render_loop_pod(&profile);

        // The env carrying the drop-box URL + the token path + the pod's own name (downward API).
        assert!(yaml.contains("name: CRUCIBLE_INGEST_URL"));
        assert!(yaml.contains("http://crucible-controller.autoresearch.svc:8080"));
        assert!(yaml.contains("name: CRUCIBLE_INGEST_TOKEN_PATH"));
        assert!(yaml.contains("name: CRUCIBLE_POD_NAME"));
        assert!(yaml.contains("metadata.name"), "downward-API pod name");
        // The projected, audience-locked token volume + read-only mount.
        assert!(yaml.contains("crucible-ingest-token"));
        assert!(yaml.contains("audience: crucible-ingest"));
        assert!(yaml.contains("/var/run/secrets/crucible.io/ingest"));
        // The behavioral switch: the delimiter dump is now guarded by the env's absence.
        assert!(
            yaml.contains("if [ -z \"${CRUCIBLE_INGEST_URL:-}\" ]; then"),
            "the SESSION delimiter dump is guarded behind the drop-box env: {yaml}"
        );
        // `automountServiceAccountToken: false` still stands, the ingest token is one explicit file.
        assert!(yaml.contains("automountServiceAccountToken: false"));
    }

    /// Without a drop-box URL a loop pod carries NO ingest scaffolding, and the wrapper still emits the
    /// `SESSION` delimiter (guarded, but the guard is TRUE when the env is absent) so an old controller
    /// scrapes the session exactly as before, the fallback the migration keeps until R4.
    #[test]
    fn loop_pod_without_ingest_url_keeps_the_delimiter_and_no_scaffolding() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
        "#,
        )
        .expect("profile parses");

        let yaml = render_loop_pod(&profile);

        // The env DECLARATION is absent (the wrapper's guard string mentions the var name, but no
        // `name: CRUCIBLE_INGEST_URL` env is projected and no token is mounted).
        assert!(
            !yaml.contains("name: CRUCIBLE_INGEST_URL"),
            "no ingest env without a drop-box: {yaml}"
        );
        assert!(
            !yaml.contains("crucible-ingest-token"),
            "no ingest token volume without a drop-box"
        );
        // The delimiter contract survives (behind the same guard, which is true with the env absent).
        assert!(
            yaml.contains("SESSION (rc="),
            "the SESSION delimiter is still emitted"
        );
        assert!(yaml.contains("if [ -z \"${CRUCIBLE_INGEST_URL:-}\" ]; then"));
    }

    /// The manifest + a fake single-domain profile, plus a pack dir on disk with a NESTED
    /// `tools/measure.sh`. A minimal helper the pack tests share.
    fn pack_manifest_and_profile() -> (Manifest, DeployProfile) {
        let manifest: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "shrink p95"
            sandbox_image = "registry.example.com/alpha-sandbox:latest"
            [judge]
            measure_cmd = "./tools/measure.sh"
            direction = "lower"
            [deploy]
            deploy_name = "alpha"
            [deploy.buildah]
            registry = "registry.example.com/alpha-candidate"
            dockerfile = "Dockerfile"
            [deploy.env]
            ALPHA_DEPLOY = "alpha"
        "#,
        )
        .expect("manifest parses");
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
        "#,
        )
        .expect("profile parses");
        (manifest, profile)
    }

    /// A self-cleaning scratch dir (crucible has no `tempfile` dep). Unique per call so tests can run
    /// in parallel; the `Drop` best-effort removes the tree.
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "crucible-render-test-{}-{tag}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("mkdir scratch");
            Scratch(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write a pack tree with a nested `tools/measure.sh` into `dir`, returning the manifest dir. The
    /// `state/` dir is written too, to prove the walker skips it.
    fn write_pack_dir(root: &Path) {
        std::fs::create_dir_all(root.join("tools")).expect("mkdir tools");
        std::fs::create_dir_all(root.join("state")).expect("mkdir state");
        std::fs::write(root.join("crucible.toml"), "# pack manifest\n").expect("crucible.toml");
        std::fs::write(root.join("goal.md"), "# goal\n").expect("goal.md");
        std::fs::write(root.join("tools/measure.sh"), "#!/bin/sh\necho 1\n").expect("measure.sh");
        std::fs::write(root.join("state/session.jsonl"), "STALE").expect("state file");
    }

    /// A PACK render (the controller path) emits a ConfigMap alongside the pod, carrying every pack
    /// file, and grows the pod a pack-staging init-container + the two pack volumes, so the loop image
    /// that never baked this domain still finds the manifest at the path the wrapper resolves.
    #[test]
    fn pack_render_emits_configmap_and_pod_with_staging() {
        let (manifest, profile) = pack_manifest_and_profile();
        let tmp = Scratch::new("pack-cm");
        let dir = tmp.path().join("llm-d_llm-d-router_1650");
        std::fs::create_dir_all(&dir).expect("mkdir pack");
        write_pack_dir(&dir);

        let input =
            RenderInput::from_manifest(&manifest, "llm-d_llm-d-router_1650").expect("render input");
        let yaml = render(
            input,
            &dir,
            "crucible.toml",
            &profile,
            &RenderOpts {
                iterations: 7,
                max_cost: 25.0,
                pin_digests: false,
                pr_repo: None,
                pack: Some(PackDelivery {
                    configmap_name: "crucible-run-llm-d-1650-pack".to_string(),
                }),
                clusters_file: None,
            },
        )
        .expect("render");

        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        assert_eq!(docs.len(), 4, "pod + rbac + netpol + configmap: {yaml}");
        assert!(docs[0].contains("kind: Pod"));
        // The run knobs reach the loop wrapper: a real iteration count + cost budget, not the stale
        // `--iterations=1` / no-`--max-cost` that cut the first dispatched run off.
        assert!(
            docs[0].contains("--iterations=7") && docs[0].contains("--max-cost=25"),
            "the loop wrapper carries the run's iteration + budget knobs: {}",
            docs[0]
        );
        let cm = docs
            .iter()
            .find(|d| d.contains("kind: ConfigMap"))
            .expect("a ConfigMap doc");
        assert!(
            cm.contains("name: crucible-run-llm-d-1650-pack"),
            "CM named from the controller-supplied name: {cm}"
        );
        // Every pack file rides the CM data (text), the stale state/ file does NOT.
        assert!(cm.contains("crucible.toml"));
        assert!(cm.contains("goal.md"));
        assert!(cm.contains("immutable: true"), "pack CM is immutable");
        assert!(
            !cm.contains("STALE") && !cm.contains("session.jsonl"),
            "the runtime state/ dir is skipped, never delivered: {cm}"
        );

        // The pod mounts the CM by the same name and stages it into the domain dir the wrapper resolves.
        assert!(
            docs[0].contains("name: crucible-run-llm-d-1650-pack"),
            "pod volume refs the CM"
        );
        assert!(docs[0].contains("pack-stage"), "the staging init-container");
        assert!(
            docs[0].contains(
                "cp -rL /opt/crucible/pack-src/. /opt/crucible/domains/llm-d_llm-d-router_1650/"
            ),
            "init copies the CM into the writable domain dir: {}",
            docs[0]
        );
        // The main container mounts the writable emptyDir at the domain path (so STEER.md/state writes).
        assert!(docs[0].contains("mountPath: /opt/crucible/domains/llm-d_llm-d-router_1650"));
    }

    /// A nested pack file (`tools/measure.sh`) round-trips exactly through the mount: its slash-free
    /// ConfigMap key maps back to the real `tools/measure.sh` path via the volume's `items:` entry, so
    /// the projected tree reproduces the pack layout the loop expects.
    #[test]
    fn pack_render_preserves_nested_paths_via_items() {
        let (manifest, profile) = pack_manifest_and_profile();
        let tmp = Scratch::new("nested");
        let dir = tmp.path().join("pack");
        std::fs::create_dir_all(&dir).expect("mkdir pack");
        write_pack_dir(&dir);

        let input = RenderInput::from_manifest(&manifest, "pack").expect("render input");
        let yaml = render(
            input,
            &dir,
            "crucible.toml",
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: Some(PackDelivery {
                    configmap_name: "pack-cm-name".to_string(),
                }),
                clusters_file: None,
            },
        )
        .expect("render");

        // The CM key can't hold a `/`, so the nested file lands under a slash-free key.
        let cm = yaml
            .split("\n---\n")
            .find(|d| d.contains("kind: ConfigMap"))
            .expect("configmap");
        assert!(
            cm.contains("tools_measure.sh"),
            "nested key is slash-free: {cm}"
        );
        // The volume's items: maps that key back to the TRUE nested path, so the mount rebuilds it.
        let pod = yaml.split("\n---\n").next().expect("pod doc");
        assert!(pod.contains("key: tools_measure.sh"), "items key: {pod}");
        assert!(
            pod.contains("path: tools/measure.sh"),
            "items maps to the real path: {pod}"
        );
    }
    #[test]
    fn avoid_nodes_render_the_notin_affinity_on_every_pod() {
        let manifest: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "shrink p95"
            sandbox_image = "registry.example.com/alpha-sandbox:latest"
            [judge]
            measure_cmd = "./measure.nu"
            direction = "lower"
            [deploy]
            deploy_name = "alpha"
        "#,
        )
        .expect("manifest parses");

        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            avoid_nodes = ["g12e022", "g12e099"]
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
        "#,
        )
        .expect("profile parses");

        let assert_notin = |yaml: &str, what: &str| {
            assert!(yaml.contains("nodeAffinity"), "{what}: {yaml}");
            assert!(
                yaml.contains("requiredDuringSchedulingIgnoredDuringExecution"),
                "{what}: required, not preferred"
            );
            assert!(yaml.contains("nodeSelectorTerms"), "{what}");
            assert!(yaml.contains("key: kubernetes.io/hostname"), "{what}");
            assert!(yaml.contains("operator: NotIn"), "{what}");
            assert!(
                yaml.contains("- g12e022") && yaml.contains("- g12e099"),
                "{what}: the exact hostnames: {yaml}"
            );
        };

        let input = RenderInput::from_manifest(&manifest, "alpha").expect("render input");
        let loop_yaml = render(
            input,
            std::path::Path::new("/opt/crucible/domains/alpha"),
            "crucible.toml",
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render");
        let docs: Vec<&str> = loop_yaml.split("\n---\n").collect();
        assert_notin(docs[0], "loop pod");
        // Only the pod schedules; the RBAC + netpol don't grow an affinity.
        assert!(!docs[1].contains("affinity") && !docs[2].contains("affinity"));

        let turn_yaml = render_turn(
            &profile,
            &TurnOpts {
                kind: TurnKind::Rank,
                name: "crucible-turn-owner-repo-42-abcd".to_string(),
                issue: "owner/repo#42".to_string(),
                goal_text: None,
                repo_url: "https://github.com/owner/repo.git".to_string(),
                sandbox_image: "registry.example.com/alpha-sandbox:latest".to_string(),
                max_cost: 5.0,
                pin_digests: false,
                tier: None,
                gaming_refine_rounds: 1,
                skip_gaming_review: false,
                authoritative: false,
            },
        )
        .expect("render turn");
        assert_notin(&turn_yaml, "turn pod");
    }

    /// The private-registry path: a profile naming both authfiles mounts them and points podman and
    /// forge at the mounted paths. `REGISTRY_AUTH_FILE` is what lets the gateway's inherited `podman
    /// system service` pull a private sandbox *and* a private supervisor, on any number of registries,
    /// with any auth kind podman understands (basic, credHelpers, identitytoken).
    #[test]
    fn authfile_secrets_render_as_env_not_a_podman_login() {
        let manifest: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "shrink p95"
            sandbox_image = "registry.internal:5000/alpha-sandbox:latest"
            [judge]
            measure_cmd = "./measure.nu"
            direction = "lower"
            [deploy]
            deploy_name = "alpha"
            [deploy.buildah]
            registry = "registry.internal:5000/alpha-candidate"
            dockerfile = "Dockerfile"
        "#,
        )
        .expect("manifest parses");

        // The supervisor lives on a *different* registry than the sandbox. Under the old
        // `podman login <registry-of-sandbox-image>` this was unpullable; one authfile covers both.
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "ghcr.io/neuralmagic/openshell-supervisor:latest"
            [image]
            loop = "registry.internal:5000/crucible-loop:latest"
            pull_secret = "internal-pull"
            [secrets]
            pull_authfile = "internal-authfile"
            push_authfile = "internal-push"
        "#,
        )
        .expect("profile parses");

        let dir = std::path::Path::new("/opt/crucible/domains/alpha");
        let input = RenderInput::from_manifest(&manifest, "alpha").expect("render input");
        let yaml = render(
            input,
            dir,
            "crucible.toml",
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render");

        assert!(yaml.contains("name: REGISTRY_AUTH_FILE"));
        assert!(
            yaml.contains(&format!("value: {PULL_AUTHFILE_PATH}")),
            "REGISTRY_AUTH_FILE points at the mounted authfile"
        );
        assert!(yaml.contains("name: FORGE_AUTHFILE"));
        assert!(yaml.contains("secretName: internal-authfile"), "pull mount");
        assert!(yaml.contains("secretName: internal-push"), "push mount");

        // The credential never passes through a shell, a python one-liner, or a process argv.
        assert!(!yaml.contains("podman login"));
        assert!(!yaml.contains("python3"));
    }

    /// A single-domain manifest with no `[deploy]` block can't be rendered, there's nothing to project
    /// as the build/deploy target, so it must fail loudly with a fix-it, not render a hollow pod.
    #[test]
    fn single_domain_without_deploy_block_fails_with_clear_error() {
        let manifest: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "command"
            agent_cmd = "./bump.nu"
            goal = "g"
            [judge]
            measure_cmd = "m"
            direction = "higher"
        "#,
        )
        .expect("manifest parses");
        match RenderInput::from_manifest(&manifest, "alpha") {
            Ok(_) => panic!("expected an error: no [deploy] block"),
            Err(err) => assert!(
                err.to_string().contains("[deploy]"),
                "error names the missing block: {err}"
            ),
        }
    }

    /// A GPU-measured code domain: the manifest's `[measure]` tool contract + the profile's
    /// `[measure]` substrate project the full BROKER_CODEGEN_* env into the loop pod, so onboarding a
    /// codegen domain is declarative (no hand-written codegen JSON in the profile). The tools-defaults
    /// JSON carries the domain's frozen-command contract; the substrate carries the cluster facts.
    #[test]
    fn measure_section_projects_broker_codegen_env() {
        let manifest: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "make the kernel faster"
            sandbox_image = "ghcr.io/neuralmagic/kappa-sandbox:latest"
            [judge]
            measure_cmd = "./gate.py"
            direction = "higher"
            [deploy]
            deploy_name = "kappa"
            [measure]
            gpus = 2
            [measure.build]
            base_image = "ghcr.io/neuralmagic/kappa-base:latest"
            [measure.benchmark]
            cmd = "./bench.sh"
        "#,
        )
        .expect("manifest parses");

        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
            [measure]
            namespace = "crucible-gpu"
            queue = "crucible-measure"
            model_pvc = "prewarmed-weights"
            max_gpus = 8
        "#,
        )
        .expect("profile parses");

        let dir = std::path::Path::new("/opt/crucible/domains/kappa");
        let input = RenderInput::from_manifest(&manifest, "kappa").expect("render input");
        let yaml = render(
            input,
            dir,
            "crucible.toml",
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render");

        // The enable flag + the tools-defaults JSON (the frozen-command contract from the manifest).
        assert!(yaml.contains("name: BROKER_CODEGEN"), "enable flag: {yaml}");
        assert!(
            yaml.contains("name: BROKER_CODEGEN_TOOLS_DEFAULTS"),
            "tools-defaults JSON present"
        );
        assert!(
            yaml.contains("ghcr.io/neuralmagic/kappa-base:latest"),
            "build.base_image rides the tools-defaults JSON"
        );
        // The substrate facts (cluster-level) from the profile's [measure].
        assert!(
            yaml.contains("name: BROKER_CODEGEN_NAMESPACE"),
            "substrate namespace"
        );
        assert!(yaml.contains("value: crucible-gpu"), "namespace value");
        assert!(
            yaml.contains("name: BROKER_CODEGEN_QUEUE"),
            "substrate queue"
        );
        assert!(
            yaml.contains("name: BROKER_CODEGEN_MODEL_PVC"),
            "substrate model PVC"
        );
        assert!(
            yaml.contains("name: BROKER_CODEGEN_MAX_GPUS"),
            "substrate GPU ceiling"
        );
        // Never mount the workspace PVC at measure time (a measurement is a function of the digest alone).
        assert!(
            !yaml.contains("BROKER_CODEGEN_WORKSPACE_PVC"),
            "the workspace PVC is deliberately never projected"
        );
        // No [measure].cluster: no spoke mount, env, or hostAliases.
        assert!(!yaml.contains("BROKER_CODEGEN_KUBECONFIG"));
        assert!(!yaml.contains("spoke-kubeconfig"));
        assert!(!yaml.contains("hostAliases"));
    }

    fn spoke_manifest() -> Manifest {
        toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "make the kernel faster"
            sandbox_image = "ghcr.io/neuralmagic/kappa-sandbox:latest"
            [judge]
            measure_cmd = "./gate.py"
            direction = "higher"
            [deploy]
            deploy_name = "kappa"
            [measure]
            gpus = 2
            [measure.build]
            base_image = "ghcr.io/neuralmagic/kappa-base:latest"
            [measure.benchmark]
            cmd = "./bench.sh"
        "#,
        )
        .expect("manifest parses")
    }

    const SPOKE_PROFILE_BASE: &str = r#"
        [cluster]
        loop_namespace = "autoresearch"
        rig_namespace = "rig"
        service_account = "autoresearch-publisher"
        supervisor_image = "registry.example.com/openshell-supervisor:latest"
        [image]
        loop = "registry.example.com/crucible-loop:latest"
        pull_secret = "quay-pull"
        [measure]
        namespace = "crucible-gpu"
        queue = "crucible-measure"
        cluster = "gpu-east"
    "#;

    /// A `[measure].cluster` spoke projects the kubeconfig Secret mount, the spoke env
    /// (BROKER_CODEGEN_KUBECONFIG/CLUSTER/CLUSTER_TIER/PROXY_URL), and the entry's
    /// `host_aliases` as pod-spec hostAliases on the loop pod.
    #[test]
    fn measure_cluster_projects_spoke_mount_env_and_host_aliases() {
        let profile: DeployProfile = toml::from_str(&format!(
            r#"{SPOKE_PROFILE_BASE}
            [clusters.gpu-east]
            kubeconfig_secret = "spoke-gpu-east-kubeconfig"
            proxy_url = "http://10.0.0.9:3128"
            host_aliases = {{ "api.spoke.example" = "10.0.0.1" }}
        "#
        ))
        .expect("profile parses");

        let manifest = spoke_manifest();
        let input = RenderInput::from_manifest(&manifest, "kappa").expect("render input");
        let yaml = render(
            input,
            std::path::Path::new("/opt/crucible/domains/kappa"),
            "crucible.toml",
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render");

        assert!(yaml.contains("name: BROKER_CODEGEN_CLUSTER"), "{yaml}");
        assert!(yaml.contains("value: gpu-east"));
        assert!(yaml.contains("name: BROKER_CODEGEN_KUBECONFIG"));
        assert!(yaml.contains("value: /etc/crucible/spoke/kubeconfig"));
        assert!(yaml.contains("name: BROKER_CODEGEN_CLUSTER_TIER"));
        assert!(yaml.contains("value: proxy"), "proxy_url wins the tier");
        assert!(yaml.contains("name: BROKER_CODEGEN_PROXY_URL"));
        assert!(yaml.contains("value: http://10.0.0.9:3128"));
        assert!(yaml.contains("secretName: spoke-gpu-east-kubeconfig"));
        assert!(yaml.contains("mountPath: /etc/crucible/spoke"));
        assert!(yaml.contains("hostAliases"));
        assert!(yaml.contains("ip: 10.0.0.1"));
        assert!(yaml.contains("- api.spoke.example"));
    }

    /// A bastioned spoke is schema-accepted but refused at render until the tunnel exists.
    #[test]
    fn bastioned_spoke_is_refused_at_render() {
        let profile: DeployProfile = toml::from_str(&format!(
            r#"{SPOKE_PROFILE_BASE}
            [clusters.gpu-east]
            kubeconfig_secret = "spoke-gpu-east-kubeconfig"
            [clusters.gpu-east.bastion]
            host = "jump.example"
            user = "crucible"
            key_secret = "bastion-key"
        "#
        ))
        .expect("profile parses");

        let manifest = spoke_manifest();
        let input = RenderInput::from_manifest(&manifest, "kappa").expect("render input");
        let err = match render(
            input,
            std::path::Path::new("/opt/crucible/domains/kappa"),
            "crucible.toml",
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        ) {
            Err(e) => e,
            Ok(_) => panic!("a bastioned spoke must refuse to render"),
        };
        assert!(err.to_string().contains("not implemented"), "{err:#}");
    }

    /// A domain WITHOUT `[measure]` (the common config-tuning / live-deployment case) emits NO BROKER_CODEGEN_*
    /// env at all, the `if let Some` guard keeps the existing render byte-clean.
    #[test]
    fn no_measure_section_emits_no_broker_codegen_env() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
        "#,
        )
        .expect("profile parses");
        let yaml = render_loop_pod(&profile);
        assert!(
            !yaml.contains("BROKER_CODEGEN"),
            "no [measure] => no codegen env: {yaml}"
        );
    }

    // --- kubernetes driver render tests ---

    /// A helper that builds a kubernetes-driver profile from a minimal podman profile string by
    /// injecting `sandbox_driver = "kubernetes"`.
    fn k8s_profile(extra: &str) -> DeployProfile {
        let text = format!(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            sandbox_driver = "kubernetes"
            {extra}
            [image]
            loop = "ghcr.io/neuralmagic/crucible:latest"
            pull_secret = "quay-pull"
            [secrets]
            pull_authfile = "quay-authfile"
        "#
        );
        toml::from_str(&text).expect("k8s profile parses")
    }

    fn k8s_manifest() -> Manifest {
        loop_manifest()
    }

    fn render_k8s(profile: &DeployProfile) -> String {
        let manifest = k8s_manifest();
        let dir = std::path::Path::new("/opt/crucible/domains/alpha");
        let input = RenderInput::from_manifest(&manifest, "alpha").expect("render input");
        render(
            input,
            dir,
            "crucible.toml",
            profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render")
    }

    /// The wrapper carries `--compute-driver=kubernetes` and the pod has the `status.podIP`
    /// downward-API env var.
    #[test]
    fn kubernetes_wrapper_passes_compute_driver_flag() {
        let profile = k8s_profile("");
        let yaml = render_k8s(&profile);
        assert!(
            yaml.contains("--compute-driver=kubernetes"),
            "wrapper passes the compute driver: {yaml}"
        );
        assert!(
            !yaml.contains("privileged"),
            "never privileged under the kubernetes driver (sandboxes are sibling pods): {yaml}"
        );
        assert!(
            yaml.contains("name: CRUCIBLE_POD_IP"),
            "downward API pod IP env: {yaml}"
        );
        assert!(
            yaml.contains("fieldPath: status.podIP"),
            "pod IP from downward API: {yaml}"
        );
    }

    /// `state_pvc` makes a run survive its pod: the claim mounts over the domain's state/ dir,
    /// the pod restarts on failure, and the wrapper passes `--resume` when a session log already
    /// exists. Without it the pod stays one-shot (a restart would silently fresh-start a run).
    #[test]
    fn state_pvc_mounts_resumes_and_restarts_on_failure() {
        // String form references an existing claim, never generates one.
        let with = k8s_profile("state_pvc = \"deepgemm-state\"");
        let yaml = render_k8s(&with);
        assert!(yaml.contains("claimName: deepgemm-state"), "{yaml}");
        assert!(
            yaml.contains("mountPath: /opt/crucible/domains/alpha/state"),
            "{yaml}"
        );
        assert!(yaml.contains("restartPolicy: OnFailure"), "{yaml}");
        assert!(
            yaml.contains(r#"$([ -s "$D/state/session.jsonl" ] && echo --resume)"#),
            "{yaml}"
        );
        assert!(!yaml.contains("kind: PersistentVolumeClaim"), "{yaml}");

        let without = k8s_profile("");
        let yaml = render_k8s(&without);
        assert!(yaml.contains("restartPolicy: Never"), "{yaml}");
        assert!(!yaml.contains("--resume"), "{yaml}");
        assert!(!yaml.contains("run-state"), "{yaml}");
    }

    /// Table form emits `<run>-state` with the profile's storage class, size, access modes,
    /// labels, and annotations.
    #[test]
    fn state_pvc_template_generates_the_claim() {
        let profile = k8s_profile(
            r#"[cluster.state_pvc]
            storage_class = "shared-vast"
            size = "2Gi"
            access_modes = ["ReadWriteMany"]
            labels = { "cost-center" = "llm-d" }
            annotations = { "backup.example.com/policy" = "none" }
            "#,
        );
        let yaml = render_k8s(&profile);
        assert!(yaml.contains("kind: PersistentVolumeClaim"), "{yaml}");
        assert!(yaml.contains("name: alpha-state"), "{yaml}");
        assert!(yaml.contains("claimName: alpha-state"), "{yaml}");
        assert!(yaml.contains("storageClassName: shared-vast"), "{yaml}");
        assert!(yaml.contains("storage: 2Gi"), "{yaml}");
        assert!(yaml.contains("ReadWriteMany"), "{yaml}");
        assert!(yaml.contains("cost-center: llm-d"), "{yaml}");
        assert!(yaml.contains("backup.example.com/policy: none"), "{yaml}");
        // The render's own labels survive the merge.
        assert!(yaml.contains("crucible/run: alpha"), "{yaml}");
        assert!(yaml.contains("restartPolicy: OnFailure"), "{yaml}");

        assert!(
            toml::from_str::<DeployProfile>(
                r#"
                [cluster]
                loop_namespace = "a"
                rig_namespace = "a"
                service_account = "a"
                supervisor_image = "img"
                [cluster.state_pvc]
                access_modes = ["ReadWriteTypo"]
                [image]
                loop = "img"
                pull_secret = "s"
                "#
            )
            .is_err(),
            "an unknown access mode must fail at parse"
        );
    }

    /// A domain that builds in-pod (buildah deploy target or `[measure]`) gets AppArmor Unconfined
    /// on the loop container: containerd's default profile denies the mount syscalls buildah needs,
    /// even inside a user namespace. One without stays context-free for restricted PSA/SCC.
    #[test]
    fn kubernetes_loop_apparmor_scoped_to_building_domains() {
        let profile = k8s_profile("");
        // loop_manifest has [deploy.buildah] => the loop pod runs buildah itself.
        let yaml = render_k8s(&profile);
        assert!(
            yaml.contains("appArmorProfile") && yaml.contains("type: Unconfined"),
            "in-pod builds need AppArmor Unconfined on the loop container: {yaml}"
        );
        for (k, v) in [("STORAGE_DRIVER", "vfs"), ("BUILDAH_ISOLATION", "chroot")] {
            assert!(
                yaml.contains(&format!("name: {k}")) && yaml.contains(&format!("value: {v}")),
                "in-pod builds run buildah as {k}={v}: {yaml}"
            );
        }

        // deploy_name only (config tuning, no in-pod buildah) => no securityContext at all.
        let manifest: Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "shrink p95"
            sandbox_image = "registry.example.com/alpha-sandbox:latest"
            [judge]
            measure_cmd = "./measure.nu"
            direction = "lower"
            [deploy]
            deploy_name = "alpha"
        "#,
        )
        .expect("manifest parses");
        let input = RenderInput::from_manifest(&manifest, "alpha").expect("render input");
        let yaml = render(
            input,
            std::path::Path::new("/opt/crucible/domains/alpha"),
            "crucible.toml",
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render");
        assert!(
            !yaml.contains("appArmorProfile"),
            "no in-pod builds => restricted-PSA-clean loop pod: {yaml}"
        );
        assert!(!yaml.contains("securityContext"), "{yaml}");
        assert!(
            !yaml.contains("STORAGE_DRIVER"),
            "no in-pod builds => no buildah env: {yaml}"
        );
    }

    /// The sandbox driver config env vars are projected: namespace, service account, default
    /// image, image pull secrets, app armor profile.
    #[test]
    fn kubernetes_projects_sandbox_driver_config_env() {
        let profile = k8s_profile("");
        let yaml = render_k8s(&profile);
        assert!(
            yaml.contains("name: CRUCIBLE_SANDBOX_NAMESPACE"),
            "sandbox namespace env: {yaml}"
        );
        assert!(
            yaml.contains("name: CRUCIBLE_SANDBOX_SERVICE_ACCOUNT"),
            "sandbox SA env: {yaml}"
        );
        assert!(
            yaml.contains("name: CRUCIBLE_SANDBOX_DEFAULT_IMAGE"),
            "sandbox default image env: {yaml}"
        );
        assert!(
            yaml.contains("name: CRUCIBLE_SANDBOX_IMAGE_PULL_SECRETS"),
            "sandbox pull secrets env: {yaml}"
        );
        assert!(
            yaml.contains("name: CRUCIBLE_SANDBOX_APP_ARMOR_PROFILE"),
            "sandbox app armor env: {yaml}"
        );
        assert!(
            yaml.contains("value: Unconfined"),
            "app armor is Unconfined: {yaml}"
        );
    }

    /// Under kubernetes the Role, RoleBinding, ClusterRole, and ClusterRoleBinding for the
    /// sandbox CRD + tokenreviews + nodes are emitted alongside the existing edit RoleBinding.
    #[test]
    fn kubernetes_emits_sandbox_rbac() {
        let profile = k8s_profile("");
        let yaml = render_k8s(&profile);
        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        // pod + edit RoleBinding + netpol + Role + sandbox RoleBinding + ClusterRole + ClusterRoleBinding
        assert_eq!(docs.len(), 7, "7 docs for kubernetes: {yaml}");

        // The sandbox Role with the CRD verbs.
        let role_doc = docs
            .iter()
            .find(|d| d.contains("kind: Role") && d.contains("sandbox"))
            .expect("sandbox Role doc");
        assert!(
            role_doc.contains("agents.x-k8s.io"),
            "sandbox CRD apiGroup: {role_doc}"
        );
        assert!(role_doc.contains("sandboxes"), "sandboxes resource");
        assert!(
            role_doc.contains("sandboxes/status"),
            "sandboxes/status resource"
        );
        for verb in &[
            "create", "delete", "get", "list", "patch", "update", "watch",
        ] {
            assert!(
                role_doc.contains(&format!("- {verb}")),
                "verb {verb} in sandbox Role: {role_doc}"
            );
        }
        assert!(role_doc.contains("events"), "events resource");
        assert!(role_doc.contains("pods"), "pods resource");
        assert!(
            role_doc.contains("secrets"),
            "secrets resource (the published client-TLS Secret): {role_doc}"
        );
        // The split grant: unrestricted create (RBAC can't name-scope create), get/patch
        // pinned to the client-TLS Secret.
        assert!(
            role_doc.contains(CLIENT_TLS_SECRET),
            "get/patch resourceNames-scoped to the client-TLS Secret: {role_doc}"
        );
        let secrets_create_unscoped = role_doc.split("- apiGroups").any(|rule| {
            rule.contains("secrets") && rule.contains("create") && !rule.contains("resourceNames")
        });
        assert!(
            secrets_create_unscoped,
            "secrets create rule must not carry resourceNames: {role_doc}"
        );

        // The sandbox RoleBinding.
        let sandbox_rb = docs
            .iter()
            .find(|d| {
                d.contains("kind: RoleBinding") && d.contains("sandbox") && d.contains("kind: Role")
            })
            .expect("sandbox RoleBinding doc");
        assert!(
            sandbox_rb.contains("autoresearch-publisher-sandbox"),
            "binding name: {sandbox_rb}"
        );

        // The ClusterRole: tokenreviews + nodes.
        let cr_doc = docs
            .iter()
            .find(|d| d.contains("kind: ClusterRole") && !d.contains("Binding"))
            .expect("ClusterRole doc");
        assert!(
            cr_doc.contains("authentication.k8s.io"),
            "tokenreviews apiGroup: {cr_doc}"
        );
        assert!(cr_doc.contains("tokenreviews"), "tokenreviews resource");
        assert!(
            cr_doc.contains("- create"),
            "tokenreviews create verb: {cr_doc}"
        );
        assert!(cr_doc.contains("nodes"), "nodes resource: {cr_doc}");
        for verb in &["get", "list", "watch"] {
            assert!(
                cr_doc.contains(&format!("- {verb}")),
                "verb {verb} in ClusterRole: {cr_doc}"
            );
        }

        // The ClusterRoleBinding.
        let crb_doc = docs
            .iter()
            .find(|d| d.contains("kind: ClusterRoleBinding"))
            .expect("ClusterRoleBinding doc");
        assert!(
            crb_doc.contains("node-reader"),
            "ClusterRoleBinding name: {crb_doc}"
        );
        assert!(
            crb_doc.contains("autoresearch-publisher"),
            "subject SA: {crb_doc}"
        );
    }

    /// The NetworkPolicy under kubernetes allows ingress from sandbox pods (the openshell
    /// managed-by label) on exactly the gateway port (17670) and broker port (8849).
    #[test]
    fn kubernetes_netpol_allows_sandbox_ingress_on_two_ports() {
        let profile = k8s_profile("");
        let yaml = render_k8s(&profile);
        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        let netpol = docs
            .iter()
            .find(|d| d.contains("kind: NetworkPolicy"))
            .expect("netpol doc");

        // Ingress rules exist (under podman there are none).
        assert!(
            netpol.contains("ingress:"),
            "kubernetes netpol has ingress rules: {netpol}"
        );
        // The selector uses the openshell managed-by label.
        assert!(
            netpol.contains("openshell.ai/managed-by: openshell"),
            "sandbox podSelector: {netpol}"
        );
        // Exactly two ports: gateway (17670) and broker (8849).
        assert!(
            netpol.contains("port: 17670"),
            "gateway port in netpol: {netpol}"
        );
        assert!(
            netpol.contains("port: 8849"),
            "broker port in netpol: {netpol}"
        );
        // policyTypes still declares Ingress.
        assert!(netpol.contains("- Ingress"), "policyTypes: {netpol}");
        // No Egress lockdown.
        assert!(!netpol.contains("- Egress"), "no egress: {netpol}");
    }

    /// Under podman the render is byte-identical to before: no `--compute-driver` flag, no
    /// downward-API pod IP env, no sandbox RBAC, and the netpol is pure deny-all with no
    /// ingress rules.
    /// The sandbox-read role ALONE projects the sts-audience token and the gateway env, a
    /// profile can grant sandbox S3 reads without publish-on-keep.
    #[test]
    fn sandbox_role_alone_projects_token_and_gateway_env() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            aws_sandbox_role_arn = "arn:aws:iam::1:role/sandbox-ro"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
        "#,
        )
        .expect("profile parses");
        let yaml = render_loop_pod(&profile);
        assert!(yaml.contains("name: CRUCIBLE_AWS_SANDBOX_ROLE_ARN"));
        assert!(yaml.contains("value: arn:aws:iam::1:role/sandbox-ro"));
        assert!(yaml.contains("name: CRUCIBLE_AWS_SANDBOX_TOKEN_FILE"));
        assert!(
            yaml.contains("audience: sts.amazonaws.com"),
            "sts token projected without aws_role_arn: {yaml}"
        );
        assert!(
            !yaml.contains("name: AWS_ROLE_ARN"),
            "publisher role env absent when only the sandbox role is set"
        );
    }

    #[test]
    fn podman_render_unchanged_by_kubernetes_code() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
        "#,
        )
        .expect("profile parses");
        let yaml = render_loop_pod(&profile);
        let docs: Vec<&str> = yaml.split("\n---\n").collect();

        // Nested rootless podman still needs the privileged outer pod.
        assert!(yaml.contains("privileged: true"));
        // Still 3 docs (pod + rbac + netpol), no sandbox RBAC.
        assert_eq!(docs.len(), 3, "podman is still 3 docs: {yaml}");
        // No --compute-driver flag in the wrapper (podman is default).
        assert!(
            !yaml.contains("--compute-driver"),
            "no compute-driver flag under podman: {yaml}"
        );
        // No downward-API pod IP env.
        assert!(
            !yaml.contains("CRUCIBLE_POD_IP"),
            "no pod IP env under podman"
        );
        // No sandbox driver config env.
        assert!(
            !yaml.contains("CRUCIBLE_SANDBOX_NAMESPACE"),
            "no sandbox namespace env under podman"
        );
        // The netpol is pure deny-all: no ingress rules.
        assert!(
            !docs[2].contains("ingress:"),
            "no ingress rules under podman: {}",
            docs[2]
        );
        // No sandbox RBAC objects.
        assert!(
            !yaml.contains("kind: ClusterRoleBinding"),
            "no ClusterRoleBinding under podman"
        );
        assert!(
            !yaml.contains("agents.x-k8s.io"),
            "no sandbox CRD apiGroup under podman"
        );
    }

    /// The emitted `[openshell.drivers.kubernetes]` table round-trips as TOML and only carries
    /// field names accepted by the real `KubernetesComputeConfig` (which uses
    /// `deny_unknown_fields`).
    #[test]
    fn kubernetes_driver_config_round_trips_as_valid_toml() {
        use crate::openshell::gateway::KubernetesDriverConfig;

        let mut cfg = KubernetesDriverConfig::new(Some("registry.example.com/supervisor:latest"));
        cfg.namespace = Some("autoresearch".to_string());
        cfg.service_account_name = Some("autoresearch-publisher".to_string());
        cfg.default_image = Some("registry.example.com/alpha-sandbox:latest".to_string());
        cfg.image_pull_secrets = vec!["quay-pull".to_string()];
        cfg.host_gateway_ip = Some("10.0.0.1".to_string());
        cfg.app_armor_profile = Some("Unconfined".to_string());

        let toml_str = toml::to_string(&cfg).expect("serialize to TOML");
        // Re-parse as a generic TOML table to inspect field names.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str(&toml_str).expect("re-parse as TOML table");

        // Every emitted field name must be one the real config struct accepts.
        // Cross-checked against the authoritative `KubernetesComputeConfig` in os-pinned.
        let accepted_fields = [
            "namespace",
            "service_account_name",
            "default_image",
            "image_pull_policy",
            "image_pull_secrets",
            "supervisor_image",
            "supervisor_image_pull_policy",
            "supervisor_sideload_method",
            "grpc_endpoint",
            "ssh_socket_path",
            "client_tls_secret_name",
            "host_gateway_ip",
            "enable_user_namespaces",
            "app_armor_profile",
            "workspace_default_storage_size",
            "default_runtime_class_name",
            "sa_token_ttl_secs",
            "provider_spiffe_workload_api_socket_path",
        ];
        for key in table.keys() {
            assert!(
                accepted_fields.contains(&key.as_str()),
                "emitted field `{key}` is not in KubernetesComputeConfig (deny_unknown_fields \
                 would reject it). Accepted: {accepted_fields:?}"
            );
        }

        // Verify values round-tripped.
        assert_eq!(
            table["supervisor_sideload_method"].as_str(),
            Some("init-container")
        );
        assert_eq!(table["namespace"].as_str(), Some("autoresearch"));
        assert_eq!(table["host_gateway_ip"].as_str(), Some("10.0.0.1"));
        assert_eq!(table["app_armor_profile"].as_str(), Some("Unconfined"));
    }

    /// `sandbox_driver = "kubernetes"` parses in a profile, default is still podman.
    #[test]
    fn profile_sandbox_driver_parses() {
        let podman_profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "sa"
            supervisor_image = "img"
            [image]
            loop = "loop"
            pull_secret = "pull"
        "#,
        )
        .expect("podman profile");
        assert_eq!(
            podman_profile.cluster.sandbox_driver,
            ComputeDriver::Podman,
            "default is podman"
        );

        let k8s_profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "sa"
            supervisor_image = "img"
            sandbox_driver = "kubernetes"
            [image]
            loop = "loop"
            pull_secret = "pull"
        "#,
        )
        .expect("k8s profile");
        assert_eq!(
            k8s_profile.cluster.sandbox_driver,
            ComputeDriver::Kubernetes,
        );
    }

    /// The `imagePullSecrets` flows into the sandbox driver config env when the profile names
    /// a pull_authfile secret. Smoke-tests the image-pull path end to end.
    #[test]
    fn kubernetes_image_pull_secrets_reaches_driver_config() {
        let profile = k8s_profile("");
        let yaml = render_k8s(&profile);
        // The pull secret name reaches the sandbox config env.
        assert!(
            yaml.contains("name: CRUCIBLE_SANDBOX_IMAGE_PULL_SECRETS"),
            "image pull secrets env projected"
        );
        assert!(
            yaml.contains("value: quay-pull"),
            "the pull secret name is the value: {yaml}"
        );
    }

    /// `imagePullSecrets` must name a `kubernetes.io/dockerconfigjson` secret. `[secrets].pull_authfile`
    /// is an Opaque `auth.json` for the nested podman; handing it to the kubelet is an ImagePullBackOff.
    /// A real profile sets BOTH keys to different names, which is why the fixture does too.
    #[test]
    fn kubernetes_pull_secret_is_the_dockerconfigjson_not_the_authfile() {
        let profile = k8s_profile("");
        assert_eq!(
            profile.secrets.pull_authfile.as_deref(),
            Some("quay-authfile")
        );
        let yaml = render_k8s(&profile);
        let env_line = yaml
            .lines()
            .skip_while(|l| !l.contains("name: CRUCIBLE_SANDBOX_IMAGE_PULL_SECRETS"))
            .nth(1)
            .unwrap_or_default()
            .to_string();
        assert!(
            env_line.contains("quay-pull"),
            "sandbox imagePullSecrets must be the dockerconfigjson secret, got: {env_line}"
        );
        assert!(
            !env_line.contains("quay-authfile"),
            "the Opaque authfile must never be used as an imagePullSecret: {env_line}"
        );
    }

    /// The netpol's broker port comes from `[agent.broker].bind`, not a second constant. A domain that
    /// moves its broker must not silently lose the ingress rule that lets the sandbox reach it.
    #[test]
    fn kubernetes_netpol_broker_port_follows_the_manifest_bind() {
        assert_eq!(broker_ingress_port("0.0.0.0:8849"), 8849);
        assert_eq!(broker_ingress_port("0.0.0.0:9999"), 9999);
        // A bind the engine cannot parse falls back to the default, never to 0 (which would render a
        // policy that drops every broker call).
        assert_eq!(broker_ingress_port("garbage"), 8849);
    }
}
