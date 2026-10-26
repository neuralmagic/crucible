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
use forge::fleet::{ClusterEntry, ClusterIdentity, FleetFile};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::IpAddr;
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
    /// The crucible-kueue MCP server: where it runs, and the scope each run's token gets.
    #[serde(default)]
    pub kueue: Option<KueueCfg>,
    /// buildit, started in each run's loop pod on the clusters it names.
    #[serde(default)]
    pub buildit: Option<BuilditCfg>,
    /// Other MCP servers that take agent tokens, each `[[agent_servers]]`.
    #[serde(default)]
    pub agent_servers: Vec<AgentServerCfg>,
    /// The controller's OpenShift OAuth client, through which people link their username on a
    /// cluster without pasting a token.
    #[serde(default)]
    pub cluster_link: Option<ClusterLinkCfg>,
}

/// A Kubernetes object or namespace name: a DNS-1123 label.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct KubeName(String);

#[derive(Debug, thiserror::Error)]
#[error("{0:?} is not a DNS-1123 label (lowercase alphanumerics and '-', 1-63 chars)")]
pub struct NotKubeName(String);

impl TryFrom<String> for KubeName {
    type Error = NotKubeName;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let ok = (1..=63).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !value.starts_with('-')
            && !value.ends_with('-');
        match ok {
            true => Ok(Self(value)),
            false => Err(NotKubeName(value)),
        }
    }
}

impl KubeName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for KubeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "{0:?} is not a cluster name: a DNS-1123 label (lowercase alphanumerics and '-', 1-63 chars)"
)]
pub struct NotClusterName(pub String);

/// Whether `name` can name a cluster: the hub, a `[clusters.<name>]` spoke, or a cluster in
/// controller configuration. Every place that reads a cluster name checks it here, so a name one
/// accepts the others accept too.
pub fn check_cluster_name(name: &str) -> Result<(), NotClusterName> {
    KubeName::try_from(name.to_string())
        .map(|_| ())
        .map_err(|_| NotClusterName(name.to_string()))
}

/// A ServiceAccount, written `namespace/name`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct ServiceAccountRef {
    pub namespace: KubeName,
    pub name: KubeName,
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceAccountRefError {
    #[error("service account {0:?} is not written namespace/name")]
    Shape(String),
    #[error("service account {whole:?}: {source}")]
    Name {
        whole: String,
        #[source]
        source: NotKubeName,
    },
}

impl TryFrom<String> for ServiceAccountRef {
    type Error = ServiceAccountRefError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let Some((ns, name)) = value.split_once('/') else {
            return Err(ServiceAccountRefError::Shape(value));
        };
        let name_err = |source| ServiceAccountRefError::Name {
            whole: value.clone(),
            source,
        };
        Ok(Self {
            namespace: KubeName::try_from(ns.to_string()).map_err(name_err)?,
            name: KubeName::try_from(name.to_string()).map_err(name_err)?,
        })
    }
}

/// The `[kueue]` table. The server on each of `clusters` accepts runs' tokens from the controller's
/// agent issuer and acts for each as its `kube_user`, as Kubernetes RBAC decides.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KueueCfg {
    /// The server image, a tag the renderer resolves to `@sha256:…`.
    pub image: String,
    /// The dispatch clusters the server runs on, `hub` or spoke names: `clusters = ["east",
    /// "west"]`, or `cluster = "east"` for one.
    #[serde(
        default = "hub_clusters",
        alias = "cluster",
        deserialize_with = "one_or_many"
    )]
    pub clusters: Vec<String>,
    /// Namespace of the server's Deployment, Service, and ServiceAccount.
    pub namespace: KubeName,
    /// The namespaces runs' tokens name, the first as the default, and the only ones where the
    /// server's own account holds workload verbs: `workload_namespaces = [...]`, or
    /// `workload_namespace = "..."` for one.
    #[serde(alias = "workload_namespace", deserialize_with = "one_or_many_names")]
    pub workload_namespaces: Vec<KubeName>,
    /// Where the server fetches the agent issuer's keys. Default
    /// `<CONTROLLER_AGENT_ISSUER>/.well-known/jwks.json`; a server that cannot reach the issuer's
    /// own address names a route here.
    #[serde(default)]
    pub run_jwks_uri: Option<url::Url>,
    /// Where the server takes a caller's groups from for its SubjectAccessReviews. Default: the
    /// cluster's OpenShift Group objects on `platform = "openshift"`, the token's `kube_groups`
    /// claim elsewhere.
    #[serde(default)]
    pub kube_authz: Option<KubeAuthz>,
    /// The cluster's DNS domain. The sandbox proxy resolves names without the pod's search list,
    /// so the Service is addressed by its fully qualified name.
    #[serde(default = "default_cluster_domain")]
    pub cluster_domain: String,
    /// Image pull secrets in the workload namespace that agents' pod templates may name, each
    /// `name` or `name=registry`; the server adds a secret to any pod whose image is on its
    /// registry (a host or host/path prefix).
    #[serde(default)]
    pub pull_secrets: Vec<String>,
    /// Also serve people's MCP clients, with tokens from the same issuer: each cluster's server is
    /// then a resource people may get tokens for.
    #[serde(default)]
    pub people: bool,
    /// Where people's MCP clients reach the server, by cluster, when not at its Service URL (a
    /// route, or a port-forward): `people_urls = { east = "https://kueue.east.example/mcp" }`.
    /// A person's token names its cluster by this URL, so `people` on several clusters needs a
    /// distinct one for each.
    #[serde(default)]
    pub people_urls: BTreeMap<String, url::Url>,
}

/// The dispatch name of the controller's own cluster.
pub const HUB_CLUSTER: &str = "hub";

fn hub_clusters() -> Vec<String> {
    vec![HUB_CLUSTER.to_string()]
}

/// A string or a non-empty list of strings.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    let list = match OneOrMany::deserialize(d)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    };
    match list.is_empty() {
        true => Err(serde::de::Error::custom("names no cluster")),
        false => Ok(list),
    }
}

fn one_or_many_names<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<KubeName>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(KubeName),
        Many(Vec<KubeName>),
    }
    let list = match OneOrMany::deserialize(d)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    };
    match list.is_empty() {
        true => Err(serde::de::Error::custom("names no namespace")),
        false => Ok(list),
    }
}

fn default_cluster_domain() -> String {
    "cluster.local".to_string()
}

impl KueueCfg {
    pub const SERVICE_NAME: &str = "crucible-kueue";
    pub const PORT: u16 = 8850;

    /// Whether a server runs on dispatch cluster `cluster`.
    pub fn serves(&self, cluster: &str) -> bool {
        self.clusters.iter().any(|c| c == cluster)
    }

    /// `kube_authz`, or the default for a cluster on `platform`.
    pub fn kube_authz(&self, platform: forge::fleet::Platform) -> KubeAuthz {
        self.kube_authz.unwrap_or(match platform {
            forge::fleet::Platform::Openshift => KubeAuthz::Openshift,
            forge::fleet::Platform::Kubernetes => KubeAuthz::Token,
        })
    }

    /// The Service DNS name sandboxes reach the server at, and the Host the server accepts.
    pub fn service_host(&self) -> String {
        format!(
            "{}.{}.svc.{}",
            Self::SERVICE_NAME,
            self.namespace,
            self.cluster_domain
        )
    }

    /// The MCP endpoint URL handed to the loop pod.
    pub fn url(&self) -> String {
        format!("http://{}:{}/mcp", self.service_host(), Self::PORT)
    }

    /// Where people's MCP clients reach the server on `cluster`: its `people_urls` entry, else
    /// the Service URL.
    pub fn people_url(&self, cluster: &str) -> String {
        match self.people_urls.get(cluster) {
            Some(url) => url.to_string(),
            None => self.url(),
        }
    }

    /// The key set URI of `issuer`, the controller's agent issuer: `run_jwks_uri`, or the
    /// issuer's own.
    pub fn run_jwks_uri(&self, issuer: &str) -> Result<url::Url, url::ParseError> {
        jwks_uri(issuer, self.run_jwks_uri.as_ref())
    }
}

/// The key set URI of `issuer`: `over`, or the issuer's own `/.well-known/jwks.json`.
pub fn jwks_uri(issuer: &str, over: Option<&url::Url>) -> Result<url::Url, url::ParseError> {
    match over {
        Some(uri) => Ok(uri.clone()),
        None => url::Url::parse(&format!(
            "{}/.well-known/jwks.json",
            issuer.trim_end_matches('/')
        )),
    }
}

/// The `[buildit]` table. A run on one of `clusters` starts buildit in its loop pod, which runs
/// builder pods in `namespace` for the run's own sandboxes, accepting their tokens from the
/// controller's agent issuer (`CONTROLLER_AGENT_ISSUER`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilditCfg {
    /// The dispatch clusters whose runs get buildit: `clusters = [...]`, or `cluster = "..."`.
    #[serde(
        default = "hub_clusters",
        alias = "cluster",
        deserialize_with = "one_or_many"
    )]
    pub clusters: Vec<String>,
    /// Where builder pods run: a namespace of their own, never the loop, rig, sandbox or
    /// crucible-kueue namespaces, since the rights to create and exec into them reach every pod
    /// there.
    pub namespace: KubeName,
    /// Where buildit fetches the agent issuer's keys. Default
    /// `<CONTROLLER_AGENT_ISSUER>/.well-known/jwks.json`; a loop pod that cannot reach the issuer's
    /// own address names a route here.
    #[serde(default)]
    pub run_jwks_uri: Option<url::Url>,
    /// The one repository builds may be pushed to, with the loop pod's push authfile
    /// (`[secrets] push_authfile`). Unset, buildit pushes nothing.
    #[serde(default)]
    pub push_repo: Option<String>,
    /// The URL sandboxes reach buildit at; it listens on its port in the loop pod. Default
    /// `http://host.openshell.internal:8848/mcp`.
    #[serde(default)]
    pub url: Option<url::Url>,
    /// Where builder pods may connect besides DNS, since a build's `RUN` steps run unsandboxed:
    /// the registries they pull from and push to. Default: TCP 443 anywhere.
    #[serde(default = "default_builder_egress")]
    pub egress: Vec<BuilderEgress>,
    /// The ResourceQuota `hard` limits of the builder namespace. Default: 16 pods and no GPUs.
    /// Builder pods request no CPU or memory, so a `requests.cpu` or `requests.memory` limit would
    /// refuse every one.
    #[serde(default = "default_builder_quota")]
    pub quota: BTreeMap<String, String>,
}

/// An IP network builder pods may reach, on `ports`, less the networks in `except` (such as the
/// cluster's pod and service networks).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderEgress {
    pub cidr: String,
    #[serde(default = "https_ports")]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub except: Vec<String>,
}

fn https_ports() -> Vec<u16> {
    vec![443]
}

fn default_builder_quota() -> BTreeMap<String, String> {
    [
        ("pods", "16"),
        ("requests.nvidia.com/gpu", "0"),
        ("requests.amd.com/gpu", "0"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .into()
}

fn default_builder_egress() -> Vec<BuilderEgress> {
    ["0.0.0.0/0", "::/0"]
        .map(|cidr| BuilderEgress {
            cidr: cidr.to_string(),
            ports: https_ports(),
            except: Vec::new(),
        })
        .to_vec()
}

impl BuilditCfg {
    /// Whether runs on dispatch cluster `cluster` get buildit.
    pub fn serves(&self, cluster: &str) -> bool {
        self.clusters.iter().any(|c| c == cluster)
    }

    /// The `[buildit] egress` networks that reach every address (`0.0.0.0/0`, `::/0`) with no
    /// `except`: builder pods' `RUN` steps can then reach the cluster's API server and in-cluster
    /// services on those ports.
    pub fn unfenced_egress(&self) -> Vec<&str> {
        self.egress
            .iter()
            .filter(|e| e.except.is_empty() && matches!(e.cidr.as_str(), "0.0.0.0/0" | "::/0"))
            .map(|e| e.cidr.as_str())
            .collect()
    }

    /// The port buildit listens on in the loop pod: its URL's.
    pub fn port(&self) -> u16 {
        self.url
            .as_ref()
            .and_then(url::Url::port_or_known_default)
            .unwrap_or(crucible_contract::buildit::PORT)
    }

    /// The URL sandboxes reach buildit at.
    pub fn url(&self) -> String {
        match &self.url {
            Some(url) => url.to_string(),
            None => crucible_contract::buildit::URL.to_string(),
        }
    }

    /// `<issuer>=<jwks uri>` for `issuer`, the controller's agent issuer, as buildit's
    /// `--run-issuer` takes it.
    pub fn trusted_issuer(&self, issuer: &str) -> Result<String, url::ParseError> {
        let jwks = jwks_uri(issuer, self.run_jwks_uri.as_ref())?;
        Ok(format!("{issuer}={jwks}"))
    }
}

/// An `[[agent_servers]]` entry: an MCP server that takes agent tokens, deployed by its operator.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentServerCfg {
    /// The `.mcp.json` key the agent sees its tools under.
    pub name: crucible_contract::agent_servers::ServerName,
    /// Its MCP URL, the audience of its tokens.
    pub url: url::Url,
    /// The scopes its tokens are minted with. Default: none.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// The dispatch clusters whose runs get it: `clusters = [...]`, or `cluster = "..."`.
    #[serde(
        default = "hub_clusters",
        alias = "cluster",
        deserialize_with = "one_or_many"
    )]
    pub clusters: Vec<String>,
    /// Whether it authorizes as the token's `kube_user`, so no token is minted without one.
    #[serde(default)]
    pub requires_kube_user: bool,
    /// The namespaces its tokens name, the first as the default.
    #[serde(default)]
    pub namespaces: Vec<KubeName>,
    /// Also a resource people's MCP clients may get tokens for, at `url` on each cluster.
    #[serde(default)]
    pub people: bool,
}

/// The source of a caller's groups for crucible-kueue's SubjectAccessReviews (`--kube-authz`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KubeAuthz {
    /// OpenShift Group objects.
    Openshift,
    /// The token's `kube_groups` claim.
    Token,
}

impl KubeAuthz {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openshift => "openshift",
            Self::Token => "token",
        }
    }
}

/// The `[cluster_link]` table: one `OAuthClient` per cluster in `openshift`, whose only redirect is
/// the controller's callback for that cluster.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterLinkCfg {
    /// The controller's URL as people's browsers reach it.
    pub external_url: String,
    /// The `OAuthClient` name, the controller's client id on every cluster.
    #[serde(default = "cluster_link_client_id")]
    pub client_id: KubeName,
    /// Clusters whose OAuth server links people: `hub` or `[clusters.<name>]` entries.
    pub openshift: Vec<KubeName>,
    /// Lifetime of the access token a link redeems. The controller drops it after one review.
    #[serde(default = "cluster_link_token_age")]
    pub access_token_max_age_secs: u32,
}

fn cluster_link_client_id() -> KubeName {
    KubeName(crucible_contract::cluster_link::DEFAULT_CLIENT_ID.to_string())
}

fn cluster_link_token_age() -> u32 {
    300
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
    /// The service account sandbox pods run as, when it differs from `service_account`. Sandbox
    /// pods mount no token, so this account needs no RBAC, only whatever admits the sandbox pod
    /// (an OpenShift SCC, a PSA exemption); keeping that grant off the loop SA keeps it off every
    /// pod the loop SA's token creates.
    #[serde(default)]
    pub sandbox_service_account: Option<String>,
    /// Namespace OpenShell sandbox and supervisor pods run in. It must be one no loop pod or pack
    /// can create pods in: a pod there carrying another sandbox's id annotation would be issued
    /// that sandbox's SPIFFE ID. Unset puts sandboxes in `loop_namespace`, and the controller
    /// then issues no agent tokens.
    #[serde(default)]
    pub sandbox_namespace: Option<KubeName>,
    /// Identities (`namespace/name`) granted get/list on OpenShell `Sandbox` resources in
    /// `sandbox_namespace`: the controller's own identity on this cluster, which reads a bound
    /// sandbox's name from them and, under isolated gateways, starts and tears down each run's
    /// gateway there.
    #[serde(default)]
    pub sandbox_readers: Vec<ServiceAccountRef>,
    /// The SPIFFE Workload API socket path the OpenShell driver mounts (`csi.spiffe.io`) into
    /// supervisor pods, so supervisors can fetch the JWT-SVIDs they exchange for agent tokens.
    /// Unset mounts none.
    #[serde(default)]
    pub sandbox_spiffe_socket: Option<String>,
    /// The in-cluster kubeconfig configmap mounted at `/etc/kube` (points kubectl at the API server +
    /// the projected token).
    #[serde(default = "default_kubeconfig_configmap")]
    pub kubeconfig_configmap: String,
    /// The OpenShell supervisor image the nested sandbox runtime pulls (`OPENSHELL_SUPERVISOR_IMAGE`).
    pub supervisor_image: String,
    /// The image carrying the trusted `openshell-sandbox` binary the driver bootstraps every
    /// sandbox with (`OPENSHELL_SANDBOX_RUNTIME_IMAGE`). Unset = the image the OpenShell image
    /// workflow publishes for the pinned rev, see [`Cluster::sandbox_runtime_image`].
    #[serde(default)]
    pub sandbox_runtime_image: Option<String>,
    /// Publish-on-keep S3 base; when set, the wrapper passes `--results-bucket`. Unset = don't publish.
    #[serde(default)]
    pub results_bucket: Option<String>,
    /// IRSA role the loop pod assumes to publish (renderer projects an sts-audience token + `AWS_ROLE_ARN`).
    #[serde(default)]
    pub aws_role_arn: Option<String>,
    /// Read-only role the in-pod gateway runs as (web identity, same projected token) and assumes
    /// again to mint the credentials it SigV4-signs sandbox S3 egress with, the read half of the
    /// S3 role split. Its trust policy must admit itself. Unset = no S3 provider.
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
    /// The controller's base URL as a pod reaches it, e.g.
    /// `http://crucible-controller.autoresearch.svc:8080`. A run pod gets it as
    /// `CRUCIBLE_CONTROLLER_URL` and uploads its evidence to its run there under the run token
    /// (RFC-0004:C-RUN-EVIDENCE); a rerun fetches its parent's from it. A scope turn's pod uploads
    /// its pack and transcript there under its own name. Both carry a pod token from their Secret.
    /// Unset = pods fall back to the log markers, and a rerun cannot render.
    #[serde(default)]
    pub ingest_url: Option<String>,
    /// PVC mounted at the domain's `state/` dir. When set, run state (session.jsonl, the
    /// agent-session files) survives the pod: the wrapper passes `--resume` when a session log is
    /// already present and the pod restarts on failure instead of dying with its emptyDir — a
    /// crashed run continues its turn instead of discarding hours of solver context. Unset = the
    /// state dir stays pod-local and a dead pod is a dead run.
    ///
    /// A string names an existing claim; a table is a template the render materializes as
    /// `<run>-state`. A named claim is shared by every run pointed at it, so the mount carries a
    /// `state/<run>` subPath and concurrent runs cannot interleave their session logs; the claim
    /// must then be RWX if those runs land on different nodes. A template claim is already per-run
    /// and mounts at its root.
    ///
    /// Either way, state left behind is what resume reads: to start fresh under a run name already
    /// on the claim, delete that state first, or the wrapper sees the old session log and resumes.
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
    /// Hostname to IP entries for names cluster DNS cannot answer: the loop pod's `hostAliases`
    /// and, under the kubernetes driver, the sandbox supervisor's static hosts.
    #[serde(default)]
    pub host_aliases: SandboxHostAliases,
    /// Where a sandbox that asks for GPUs (`[agent.resources] gpus`) is scheduled: the GPU nodes'
    /// labels, their taints' tolerations, and the runtime class that exposes the devices. Applied
    /// only to GPU sandboxes. Takes effect under `sandbox_driver = "kubernetes"`.
    #[serde(default)]
    pub gpu_sandbox: crate::openshell::placement::GpuPlacement,
    /// Where a person's Kubernetes username on this, the hub cluster, comes from. Spokes declare
    /// theirs on `[clusters.<name>]`.
    #[serde(default)]
    pub identity: ClusterIdentity,
    /// The distribution this, the hub cluster, runs: `kubernetes` (default) or `openshift`.
    /// Spokes declare theirs on `[clusters.<name>]`.
    #[serde(default)]
    pub platform: forge::fleet::Platform,
}

/// `[cluster].host_aliases`, checked against the rules the kubernetes driver applies to a
/// sandbox's static hosts, so a profile it would refuse at `CreateSandbox` fails at render.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(try_from = "BTreeMap<String, String>")]
pub struct SandboxHostAliases(BTreeMap<String, IpAddr>);

/// A `[cluster].host_aliases` entry the driver would refuse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostAliasError {
    #[error("host_aliases: {hostname:?} maps to {raw:?}, which is not an IP address")]
    NotAnIp { hostname: String, raw: String },
    #[error("host_aliases: {0:?} is not a valid DNS name")]
    InvalidHostname(String),
    #[error("host_aliases: {0:?} is an IP literal and needs no entry")]
    IpLiteralHostname(String),
    #[error(
        "host_aliases: {0:?} is answered by the sandbox supervisor itself and cannot be overridden"
    )]
    ReservedHostname(String),
    #[error(
        "host_aliases: {hostname:?} maps to {address}, a loopback, link-local, unspecified, or \
         multicast address no sandbox may reach"
    )]
    BlockedAddress { hostname: String, address: IpAddr },
}

/// Names the sandbox supervisor answers itself.
const RESERVED_HOSTNAMES: &[&str] = &[
    "localhost",
    "policy.local",
    "host.openshell.internal",
    "host.containers.internal",
    "host.docker.internal",
];

impl TryFrom<BTreeMap<String, String>> for SandboxHostAliases {
    type Error = HostAliasError;

    fn try_from(raw: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        let mut aliases = BTreeMap::new();
        for (hostname, ip) in raw {
            let address: IpAddr = ip.trim().parse().map_err(|_| HostAliasError::NotAnIp {
                hostname: hostname.clone(),
                raw: ip.clone(),
            })?;
            validate_alias_hostname(&hostname)?;
            if openshell_core::net::is_always_blocked_ip(address) || address.is_multicast() {
                return Err(HostAliasError::BlockedAddress { hostname, address });
            }
            aliases.insert(hostname, address);
        }
        Ok(Self(aliases))
    }
}

fn validate_alias_hostname(raw: &str) -> Result<(), HostAliasError> {
    let hostname = raw.trim_end_matches('.').to_ascii_lowercase();
    if hostname.parse::<IpAddr>().is_ok() {
        return Err(HostAliasError::IpLiteralHostname(raw.to_string()));
    }
    let valid_label = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if hostname.is_empty() || hostname.len() > 253 || !hostname.split('.').all(valid_label) {
        return Err(HostAliasError::InvalidHostname(raw.to_string()));
    }
    if RESERVED_HOSTNAMES.contains(&hostname.as_str())
        || openshell_core::net::is_known_metadata_hostname(&hostname)
    {
        return Err(HostAliasError::ReservedHostname(raw.to_string()));
    }
    Ok(())
}

impl SandboxHostAliases {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, IpAddr)> {
        self.0.iter().map(|(hostname, ip)| (hostname.as_str(), *ip))
    }

    pub fn get(&self, hostname: &str) -> Option<IpAddr> {
        self.0.get(hostname).copied()
    }
}

/// The service account sandbox pods run as under isolated gateways when the profile names none.
pub const DEFAULT_SANDBOX_SERVICE_ACCOUNT: &str = "crucible-sandbox";

/// Where the OpenShell image workflow pushes the sandbox runtime image, tagged `sha-<rev>`.
pub const SANDBOX_RUNTIME_IMAGE_REPO: &str = "ghcr.io/neuralmagic/openshell-sandbox";

impl Cluster {
    /// The sandbox runtime image: `sandbox_runtime_image`, else [`SANDBOX_RUNTIME_IMAGE_REPO`] at
    /// `sha-<rev>` for the OpenShell rev this binary compiled against. Fails when neither is
    /// known (a build whose OpenShell dependency is not a git pin).
    pub fn sandbox_runtime_image(&self) -> Result<String, NoSandboxRuntimeImage> {
        match &self.sandbox_runtime_image {
            Some(image) => Ok(image.clone()),
            None => default_sandbox_runtime_image(crate::openshell::grpc::EXPECTED_GATEWAY_REV),
        }
    }

    /// The namespace sandbox and supervisor pods run in: `sandbox_namespace`, else the loop's.
    pub fn sandbox_namespace(&self) -> &str {
        self.sandbox_namespace
            .as_ref()
            .map_or(self.loop_namespace.as_str(), KubeName::as_str)
    }

    /// The service account sandbox pods run as: `sandbox_service_account`, else
    /// [`DEFAULT_SANDBOX_SERVICE_ACCOUNT`] under isolated gateways (sandboxes in a namespace of
    /// their own, where the loop's account does not exist), else the loop's.
    pub fn sandbox_service_account(&self) -> &str {
        let isolated = self.sandbox_driver == crate::openshell::gateway::ComputeDriver::Kubernetes
            && self.sandbox_namespace.is_some();
        match (&self.sandbox_service_account, isolated) {
            (Some(sa), _) => sa,
            (None, true) => DEFAULT_SANDBOX_SERVICE_ACCOUNT,
            (None, false) => &self.service_account,
        }
    }
}

/// No sandbox runtime image to render: the profile names none and the build has no OpenShell rev.
#[derive(Debug, thiserror::Error)]
#[error(
    "[cluster].sandbox_runtime_image is unset and this build has no pinned OpenShell rev to \
     derive {SANDBOX_RUNTIME_IMAGE_REPO}:sha-<rev> from; set it in the profile"
)]
pub struct NoSandboxRuntimeImage;

fn default_sandbox_runtime_image(rev: &str) -> Result<String, NoSandboxRuntimeImage> {
    if rev == "unknown" {
        return Err(NoSandboxRuntimeImage);
    }
    Ok(format!("{SANDBOX_RUNTIME_IMAGE_REPO}:sha-{rev}"))
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

/// Two profile fields that must name different namespaces name the same one.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{first} and {second} are both {namespace:?}; they must be different namespaces")]
pub struct NamespaceOverlap {
    pub first: &'static str,
    pub second: &'static str,
    pub namespace: String,
}

impl DeployProfile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading deploy profile {}", path.display()))?;
        let profile: Self = toml::from_str(&text)
            .with_context(|| format!("parsing deploy profile {}", path.display()))?;
        profile
            .check_namespaces()
            .with_context(|| format!("validating deploy profile {}", path.display()))?;
        profile
            .check_cluster_names()
            .with_context(|| format!("validating deploy profile {}", path.display()))?;
        profile
            .agent_servers()
            .with_context(|| format!("validating deploy profile {}", path.display()))?;
        profile
            .people_resources()
            .with_context(|| format!("validating deploy profile {}", path.display()))?;
        Ok(profile)
    }

    /// Refuse a spoke, `[kueue]` or `[buildit]` cluster name [`check_cluster_name`] refuses.
    pub fn check_cluster_names(&self) -> Result<(), NotClusterName> {
        let tables = [
            self.kueue.as_ref().map(|k| &k.clusters),
            self.buildit.as_ref().map(|b| &b.clusters),
        ];
        self.clusters
            .keys()
            .chain(tables.into_iter().flatten().flatten())
            .chain(self.agent_servers.iter().flat_map(|s| &s.clusters))
            .try_for_each(|name| check_cluster_name(name))
    }

    /// Refuse namespaces that must be separate trust domains but coincide: the crucible-kueue
    /// workload namespace and the sandbox namespace, each apart from where loop pods, rigs, and
    /// the crucible-kueue server run, and from each other.
    pub fn check_namespaces(&self) -> Result<(), NamespaceOverlap> {
        let mut guarded: Vec<(&'static str, &str)> = Vec::new();
        let mut hosts: Vec<(&'static str, &str)> = vec![
            (
                "cluster.loop_namespace",
                self.cluster.loop_namespace.as_str(),
            ),
            ("cluster.rig_namespace", self.cluster.rig_namespace.as_str()),
        ];
        if let Some(kueue) = &self.kueue {
            hosts.push(("kueue.namespace", kueue.namespace.as_str()));
            for workloads in &kueue.workload_namespaces {
                guarded.push(("kueue.workload_namespaces", workloads.as_str()));
            }
        }
        if let Some(sandbox) = &self.cluster.sandbox_namespace {
            guarded.push(("cluster.sandbox_namespace", sandbox.as_str()));
        }
        if let Some(buildit) = &self.buildit {
            guarded.push(("buildit.namespace", buildit.namespace.as_str()));
        }
        for (i, &(first, ns)) in guarded.iter().enumerate() {
            let clash = hosts
                .iter()
                .chain(guarded[i + 1..].iter())
                .find(|(_, other)| *other == ns);
            if let Some(&(second, _)) = clash {
                return Err(NamespaceOverlap {
                    first,
                    second,
                    namespace: ns.to_string(),
                });
            }
        }
        Ok(())
    }

    /// The platform of `cluster`: the hub's from `[cluster]`, a spoke's from its `[clusters.<name>]`
    /// entry, `kubernetes` for a cluster the profile does not know.
    pub fn platform(&self, cluster: &str) -> forge::fleet::Platform {
        match cluster {
            HUB_CLUSTER => self.cluster.platform,
            spoke => self
                .clusters
                .get(spoke)
                .map(|entry| entry.platform)
                .unwrap_or_default(),
        }
    }

    /// The identity rule of `cluster`: the hub's from `[cluster]`, a spoke's from its
    /// `[clusters.<name>]` entry. `None` for a cluster the profile does not know.
    pub fn cluster_identity(&self, cluster: &str) -> Option<&ClusterIdentity> {
        match cluster {
            HUB_CLUSTER => Some(&self.cluster.identity),
            spoke => self.clusters.get(spoke).map(|entry| &entry.identity),
        }
    }

    /// Every cluster the profile knows with its identity rule: the hub, then spokes by name.
    pub fn cluster_identities(&self) -> Vec<(&str, &ClusterIdentity)> {
        std::iter::once((HUB_CLUSTER, &self.cluster.identity))
            .chain(
                self.clusters
                    .iter()
                    .map(|(name, entry)| (name.as_str(), &entry.identity)),
            )
            .collect()
    }

    /// Load the profile and fold in the fleet file's `[clusters.<name>]` entries. The fleet file
    /// is `clusters.toml` in the profile's own directory, overridable with an explicit path (which
    /// must then exist); a profile-local `[clusters.<name>]` wins on conflict.
    pub fn load_with_fleet(path: &Path, clusters_override: Option<&Path>) -> Result<Self> {
        let mut profile = Self::load(path)?;
        if let Some(fleet_path) = FleetFile::resolve_path(path, clusters_override)? {
            FleetFile::load(&fleet_path)?.merge_into(&mut profile.clusters);
            profile
                .check_cluster_names()
                .with_context(|| format!("validating fleet file {}", fleet_path.display()))?;
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
    use crate::deploy::profile::*;

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

    #[test]
    fn the_sandbox_runtime_image_defaults_to_the_pinned_rev() {
        let profile: DeployProfile = toml::from_str(BASE).unwrap();
        assert!(profile.cluster.sandbox_runtime_image.is_none());
        assert_eq!(
            default_sandbox_runtime_image("6648bd0c290efbc41ba131ee9831ee45cd431f94").unwrap(),
            "ghcr.io/neuralmagic/openshell-sandbox:sha-6648bd0c290efbc41ba131ee9831ee45cd431f94"
        );
        let derived = profile.cluster.sandbox_runtime_image().unwrap();
        assert_eq!(
            derived,
            format!(
                "{SANDBOX_RUNTIME_IMAGE_REPO}:sha-{}",
                crate::openshell::grpc::EXPECTED_GATEWAY_REV
            )
        );
    }

    #[test]
    fn a_build_without_a_pinned_rev_needs_an_explicit_sandbox_runtime_image() {
        let err = default_sandbox_runtime_image("unknown").unwrap_err();
        assert!(err.to_string().contains("sandbox_runtime_image"), "{err:#}");
    }

    #[test]
    fn an_explicit_sandbox_runtime_image_wins() {
        let profile: DeployProfile = toml::from_str(&BASE.replace(
            "[image]",
            "sandbox_runtime_image = \"registry.example.com/sandbox:x\"\n[image]",
        ))
        .unwrap();
        assert_eq!(
            profile.cluster.sandbox_runtime_image().unwrap(),
            "registry.example.com/sandbox:x"
        );
    }

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

    #[test]
    fn cluster_host_aliases_parse_and_default_empty() {
        let profile: DeployProfile = toml::from_str(BASE).expect("profile parses");
        assert!(profile.cluster.host_aliases.is_empty());

        let text = BASE.replace(
            "[image]",
            "[cluster.host_aliases]\n\"maas.example.com\" = \"10.0.0.1\"\n\n[image]",
        );
        let profile: DeployProfile = toml::from_str(&text).expect("profile parses");
        assert_eq!(
            profile.cluster.host_aliases.get("maas.example.com"),
            Some(IpAddr::from([10, 0, 0, 1]))
        );
    }

    fn host_aliases(entries: &[(&str, &str)]) -> Result<SandboxHostAliases, HostAliasError> {
        SandboxHostAliases::try_from(
            entries
                .iter()
                .map(|(h, ip)| (h.to_string(), ip.to_string()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn host_aliases_the_driver_would_refuse_fail_at_parse() {
        for (entry, want) in [
            (("maas.example.com", "not-an-ip"), "is not an IP address"),
            (("bad_name", "10.0.0.1"), "not a valid DNS name"),
            (("-x.example.com", "10.0.0.1"), "not a valid DNS name"),
            (("10.0.0.9", "10.0.0.1"), "IP literal"),
            (
                ("localhost", "10.0.0.1"),
                "answered by the sandbox supervisor",
            ),
            (
                ("policy.local", "10.0.0.1"),
                "answered by the sandbox supervisor",
            ),
            (
                ("Host.OpenShell.Internal.", "10.0.0.1"),
                "answered by the sandbox supervisor",
            ),
            (
                ("host.containers.internal", "10.0.0.1"),
                "answered by the sandbox supervisor",
            ),
            (
                ("host.docker.internal", "10.0.0.1"),
                "answered by the sandbox supervisor",
            ),
            (
                ("metadata.google.internal", "10.0.0.1"),
                "answered by the sandbox supervisor",
            ),
            (("maas.example.com", "127.0.0.1"), "no sandbox may reach"),
            (
                ("maas.example.com", "169.254.169.254"),
                "no sandbox may reach",
            ),
            (("maas.example.com", "0.0.0.0"), "no sandbox may reach"),
            (("maas.example.com", "::1"), "no sandbox may reach"),
            (("maas.example.com", "fe80::1"), "no sandbox may reach"),
            (("maas.example.com", "224.0.0.1"), "no sandbox may reach"),
        ] {
            let err = host_aliases(&[entry]).unwrap_err().to_string();
            assert!(err.contains(want), "{entry:?}: {err}");
        }
    }

    #[test]
    fn a_routable_host_alias_parses_and_serializes_as_a_string_map() {
        let aliases = host_aliases(&[
            ("maas.example.com", "150.239.114.201"),
            ("v6.example.com", "fd00::8"),
        ])
        .unwrap();
        assert_eq!(
            serde_json::to_value(&aliases).unwrap(),
            serde_json::json!({"maas.example.com": "150.239.114.201", "v6.example.com": "fd00::8"})
        );
    }

    #[test]
    fn a_bad_host_alias_fails_the_profile_load() {
        let text = BASE.replace(
            "[image]",
            "[cluster.host_aliases]\n\"host.openshell.internal\" = \"10.0.0.1\"\n\n[image]",
        );
        let err = toml::from_str::<DeployProfile>(&text)
            .err()
            .map(|e| e.to_string());
        assert!(
            err.as_deref()
                .is_some_and(|e| e.contains("host.openshell.internal")),
            "{err:?}"
        );
    }

    #[test]
    fn a_gpu_sandbox_placement_is_typed_and_defaults_empty() {
        let profile: DeployProfile = toml::from_str(BASE).expect("profile parses");
        assert!(profile.cluster.gpu_sandbox.is_empty());

        let text = BASE.replace(
            "[image]",
            "[cluster.gpu_sandbox]\nruntime_class_name = \"nvidia\"\n\
             [[cluster.gpu_sandbox.tolerations]]\nkey = \"nvidia.com/gpu\"\n\
             operator = \"Exists\"\neffect = \"NoSchedule\"\n\n[image]",
        );
        let profile: DeployProfile = toml::from_str(&text).expect("profile parses");
        let gpu = &profile.cluster.gpu_sandbox;
        assert_eq!(gpu.runtime_class_name.as_deref(), Some("nvidia"));
        assert_eq!(gpu.tolerations.len(), 1);
        assert_eq!(gpu.tolerations[0].key.as_deref(), Some("nvidia.com/gpu"));

        let typo = BASE.replace(
            "[image]",
            "[cluster.gpu_sandbox]\nnodeSelector = { a = \"b\" }\n\n[image]",
        );
        assert!(
            toml::from_str::<DeployProfile>(&typo).is_err(),
            "a misspelled key is refused, not dropped"
        );
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

    fn kueue_profile(cluster_extra: &str, kueue_extra: &str) -> DeployProfile {
        toml::from_str(&format!(
            r#"
            [cluster]
            loop_namespace = "loops"
            rig_namespace = "rig"
            service_account = "crucible-spoke"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            {cluster_extra}
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "pull"
            [kueue]
            image = "registry.example.com/crucible-kueue:latest"
            namespace = "kueue-server"
            kube_authz = "openshift"
            {kueue_extra}
            "#
        ))
        .expect("profile parses")
    }

    #[test]
    fn builder_egress_to_everywhere_without_except_is_unfenced() {
        let buildit = |extra: &str| -> BuilditCfg {
            toml::from_str(&format!("namespace = \"builds\"\n{extra}")).unwrap()
        };
        assert_eq!(
            buildit("").unfenced_egress(),
            ["0.0.0.0/0", "::/0"],
            "the default"
        );
        assert!(
            buildit(
                "egress = [{ cidr = \"0.0.0.0/0\", except = [\"10.0.0.0/8\", \"172.30.0.0/16\"] }]"
            )
            .unfenced_egress()
            .is_empty()
        );
        assert!(
            buildit("egress = [{ cidr = \"203.0.113.0/24\" }]")
                .unfenced_egress()
                .is_empty(),
            "a registry's network"
        );
    }

    #[test]
    fn cluster_names_are_dns_labels_everywhere_the_profile_names_one() {
        for good in ["hub", "ocp", "spoke-a", "a1"] {
            assert_eq!(check_cluster_name(good), Ok(()));
        }
        for bad in [
            "",
            "Ocp",
            "spoke_a",
            "-a",
            "a-",
            "a.b",
            "a:b",
            &"a".repeat(64),
        ] {
            assert_eq!(
                check_cluster_name(bad),
                Err(NotClusterName(bad.to_string()))
            );
        }
        let ok = kueue_profile(
            "[clusters.spoke-a]\nkubeconfig_secret = \"s\"",
            "workload_namespace = \"w\"\nclusters = [\"hub\", \"spoke-a\"]",
        );
        assert_eq!(ok.check_cluster_names(), Ok(()));
        let spoke = kueue_profile(
            "[clusters.Spoke_A]\nkubeconfig_secret = \"s\"",
            "workload_namespace = \"w\"",
        );
        assert_eq!(
            spoke.check_cluster_names(),
            Err(NotClusterName("Spoke_A".into()))
        );
        let kueue = kueue_profile("", "workload_namespace = \"w\"\ncluster = \"east.1\"");
        assert_eq!(
            kueue.check_cluster_names(),
            Err(NotClusterName("east.1".into()))
        );
    }

    #[test]
    fn separate_namespaces_pass_the_overlap_check() {
        kueue_profile(
            r#"sandbox_namespace = "sandboxes""#,
            r#"workload_namespace = "workloads""#,
        )
        .check_namespaces()
        .expect("all distinct");
    }

    #[test]
    fn a_guarded_namespace_shared_with_another_is_refused_naming_both_fields() {
        let cases = [
            (
                "",
                "workload_namespace = \"loops\"",
                (
                    "kueue.workload_namespaces",
                    "cluster.loop_namespace",
                    "loops",
                ),
            ),
            (
                "",
                "workload_namespace = \"rig\"",
                ("kueue.workload_namespaces", "cluster.rig_namespace", "rig"),
            ),
            (
                "",
                "workload_namespace = \"kueue-server\"",
                (
                    "kueue.workload_namespaces",
                    "kueue.namespace",
                    "kueue-server",
                ),
            ),
            (
                "sandbox_namespace = \"loops\"",
                "workload_namespace = \"w\"",
                (
                    "cluster.sandbox_namespace",
                    "cluster.loop_namespace",
                    "loops",
                ),
            ),
            (
                "sandbox_namespace = \"w\"",
                "workload_namespace = \"w\"",
                (
                    "kueue.workload_namespaces",
                    "cluster.sandbox_namespace",
                    "w",
                ),
            ),
        ];
        for (cluster, kueue, (first, second, ns)) in cases {
            assert_eq!(
                kueue_profile(cluster, kueue).check_namespaces(),
                Err(NamespaceOverlap {
                    first,
                    second,
                    namespace: ns.to_string(),
                }),
                "{cluster} / {kueue}"
            );
        }
    }

    #[test]
    fn buildits_namespace_is_its_own() {
        let with_buildit = |ns: &str| {
            kueue_profile(
                r#"sandbox_namespace = "sandboxes""#,
                &format!("workload_namespace = \"workloads\"\n[buildit]\nnamespace = \"{ns}\""),
            )
            .check_namespaces()
        };
        assert_eq!(with_buildit("builds"), Ok(()));
        for (ns, other) in [
            ("loops", "cluster.loop_namespace"),
            ("rig", "cluster.rig_namespace"),
            ("kueue-server", "kueue.namespace"),
            ("workloads", "kueue.workload_namespaces"),
            ("sandboxes", "cluster.sandbox_namespace"),
        ] {
            let refused = with_buildit(ns).expect_err(ns);
            assert_eq!(refused.namespace, ns);
            assert!(
                [refused.first, refused.second].contains(&"buildit.namespace")
                    && [refused.first, refused.second].contains(&other),
                "{ns}: {refused:?}"
            );
        }
    }

    #[test]
    fn loading_a_profile_runs_the_overlap_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profile.toml");
        std::fs::write(
            &path,
            r#"
            [cluster]
            loop_namespace = "loops"
            rig_namespace = "rig"
            service_account = "crucible-spoke"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            sandbox_namespace = "rig"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "pull"
            "#,
        )
        .expect("write");
        let Err(err) = DeployProfile::load(&path) else {
            panic!("a sandbox namespace equal to the rig namespace loaded");
        };
        assert!(
            err.chain()
                .any(|e| e.downcast_ref::<NamespaceOverlap>().is_some()),
            "{err:#}"
        );
    }
}
