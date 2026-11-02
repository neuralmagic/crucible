//! What a deployment decides about the server: the label domain it stamps, which objects a caller
//! may control, the manifest allowlist, how token claims map to a Kubernetes identity, the queue
//! verb, batch lifetimes, and which node resources count as accelerators. [`Config::default`] is
//! crucible's deployment.

use crate::caller::Manager;
use crate::caller::{Launcher, NameError, RunId};
use crate::profile::Profile;
use crucible_mcp_auth::kube::{GROUPS_CLAIM, USER_CLAIM};
use crucible_mcp_auth::principal::Principal;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const DEFAULT_LABEL_DOMAIN: &str = "crucible.dev";
pub const QUEUE_LABEL: &str = "kueue.x-k8s.io/queue-name";
/// The default value of the managed-by label on everything this server creates, and the field
/// manager of its writes.
pub const DEFAULT_MANAGER: &str = "crucible-kueue";
/// Claims the server reads after [`ClaimMap::canonicalize`] has mapped the token's own.
pub const RUN_ID_CLAIM: &str = "run_id";
pub const NAMESPACE_CLAIM: &str = "namespace";
pub const NAMESPACES_CLAIM: &str = "namespaces";

#[derive(Debug, Clone)]
pub struct Config {
    pub labels: Labels,
    pub ownership: Ownership,
    pub profile: Profile,
    pub claims: ClaimMap,
    pub queue_verb: QueueVerb,
    pub lifetimes: Lifetimes,
    pub hardware: Hardware,
    /// Namespaces this server is installed in; empty discovers them from LocalQueues.
    pub namespaces: Vec<String>,
    /// The image pull secrets a pod template may name, and the ones the server adds itself to a
    /// pod whose image is on their registries; none when empty.
    pub pull_secrets: Vec<PullSecret>,
}

/// An image pull secret in the served namespaces and the registries it pulls from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullSecret {
    pub name: String,
    /// Registry hosts or host/path prefixes (`quay.io`, `quay.io/acme`). Empty: the secret may be
    /// named but is never added.
    pub registries: Vec<String>,
}

impl PullSecret {
    /// `name` or `name=registry`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (name, registry) = match s.split_once('=') {
            Some((n, r)) => (n.trim(), Some(r.trim().trim_end_matches('/'))),
            None => (s.trim(), None),
        };
        if name.is_empty() || registry.is_some_and(str::is_empty) {
            return Err(format!("{s:?} is not name or name=registry"));
        }
        Ok(Self {
            name: name.to_string(),
            registries: registry.map(str::to_string).into_iter().collect(),
        })
    }

    /// Whether this secret pulls `image`: its registry host, or a host/path prefix of its
    /// repository, is one of ours.
    pub fn serves(&self, image: &str) -> bool {
        let repository = repository_of(image);
        self.registries.iter().any(|r| {
            repository == *r
                || repository
                    .strip_prefix(r.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }
}

impl std::fmt::Display for PullSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.registries.is_empty() {
            true => f.write_str(&self.name),
            false => write!(f, "{} ({})", self.name, self.registries.join(", ")),
        }
    }
}

/// One entry per secret, its registries gathered from every flag that named it.
pub fn merge_pull_secrets(secrets: Vec<PullSecret>) -> Vec<PullSecret> {
    let mut out: Vec<PullSecret> = Vec::new();
    for s in secrets {
        match out.iter_mut().find(|o| o.name == s.name) {
            Some(o) => {
                for r in s.registries {
                    if !o.registries.contains(&r) {
                        o.registries.push(r);
                    }
                }
            }
            None => out.push(s),
        }
    }
    out
}

/// `host/path` of an image reference without tag or digest, with Docker Hub's implicit host and
/// `library/` filled in.
fn repository_of(image: &str) -> String {
    let without_digest = image.split('@').next().unwrap_or_default();
    let (first, rest) = match without_digest.split_once('/') {
        Some((f, r)) => (f, Some(r)),
        None => (without_digest, None),
    };
    let is_host = |c: &str| c.contains('.') || c.contains(':') || c == "localhost";
    let full = match rest {
        Some(r) if is_host(first) => format!("{first}/{r}"),
        Some(_) => format!("docker.io/{without_digest}"),
        None => format!("docker.io/library/{without_digest}"),
    };
    match full.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => repo.to_string(),
        _ => full,
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            labels: Labels::new(DEFAULT_LABEL_DOMAIN),
            ownership: Ownership::Managed,
            profile: Profile::strict(),
            claims: ClaimMap::default(),
            queue_verb: QueueVerb::Use,
            lifetimes: Lifetimes::default(),
            hardware: Hardware::default(),
            namespaces: Vec::new(),
            pull_secrets: Vec::new(),
        }
    }
}

/// The label and annotation keys the server stamps, all under one domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labels {
    pub domain: String,
    /// The managed-by label's value and the field manager of every write. Two servers in one
    /// cluster with different managers each see only their own objects as managed.
    pub manager: Manager,
    pub managed_by: String,
    pub run_id: String,
    /// Whether `run_id` names a run or a person's subject.
    pub owner_kind: String,
    /// The human a delegated token acted for. Set from the caller, never the manifest.
    pub launched_by: String,
    /// On pod templates: the object the pod belongs to.
    pub workload: String,
    pub plan_hash: String,
    pub stopped_replicas: String,
    /// The sandbox that presented a run's token when the workload was planned and submitted.
    pub actor: String,
    pub task_cancelled: String,
}

impl Labels {
    pub fn new(domain: &str) -> Self {
        let key = |name: &str| format!("{domain}/{name}");
        Self {
            domain: domain.to_string(),
            manager: Manager::default(),
            managed_by: key("managed-by"),
            run_id: key("run-id"),
            owner_kind: key("owner-kind"),
            launched_by: key("launched-by"),
            workload: key("workload"),
            plan_hash: key("plan-hash"),
            stopped_replicas: key("stopped-replicas"),
            actor: key("actor"),
            task_cancelled: key("task-cancelled"),
        }
    }

    /// The same keys, stamped and written as `manager`.
    pub fn managed_as(self, manager: Manager) -> Self {
        Self { manager, ..self }
    }
}

/// Which objects a caller may observe and control besides creating its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    /// Stop, resume, scale and delete only objects this server created for the caller's run.
    Managed,
    /// Any object of a served kind the caller's RBAC permits, including ones made by kubectl or
    /// a pipeline.
    Rbac,
}

/// The verb on a LocalQueue that lets a caller submit to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum QueueVerb {
    /// Kueue's custom `use` verb, granted per queue.
    Use,
    /// Plain `get`, for clusters whose roles do not grant `use`.
    Get,
}

impl QueueVerb {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Get => "get",
        }
    }
}

/// Lifetimes a batch workload gets where its manifest sets none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lifetimes {
    pub active_deadline_seconds: Option<i64>,
    pub ttl_seconds_after_finished: Option<i64>,
    /// Create Jobs suspended, for Kueue to start on admission.
    pub suspend: bool,
}

impl Default for Lifetimes {
    fn default() -> Self {
        Self {
            active_deadline_seconds: Some(24 * 60 * 60),
            ttl_seconds_after_finished: Some(60 * 60),
            suspend: true,
        }
    }
}

/// Node resources that count as accelerators and the node labels naming their product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hardware {
    /// Extended resource names; a trailing `*` matches a prefix (`nvidia.com/mig-*`).
    pub resources: Vec<String>,
    pub product_labels: Vec<String>,
}

impl Default for Hardware {
    fn default() -> Self {
        Self {
            resources: [
                "nvidia.com/gpu",
                "nvidia.com/mig-*",
                "amd.com/gpu",
                "habana.ai/gaudi",
                "google.com/tpu",
            ]
            .map(String::from)
            .to_vec(),
            product_labels: [
                "nvidia.com/gpu.product",
                "amd.com/gpu.product-name",
                "habana.ai/gaudi.product",
                "cloud.google.com/gke-tpu-accelerator",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

impl Hardware {
    pub fn is_accelerator(&self, resource: &str) -> bool {
        self.resources.iter().any(|r| match r.strip_suffix('*') {
            Some(prefix) => resource.starts_with(prefix),
            None => resource == r,
        })
    }
}

/// Which token claims name the caller's Kubernetes user, groups, run, default namespace and the
/// namespaces it is confined to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimMap {
    pub user: String,
    /// Prepended to the user claim's value, as the API server's `--oidc-username-prefix` does.
    pub user_prefix: String,
    pub groups: String,
    /// `None`: no token names a run; every caller owns its workloads under its subject.
    pub run_id: Option<String>,
    pub namespace: String,
    pub namespaces: String,
}

impl Default for ClaimMap {
    fn default() -> Self {
        Self {
            user: USER_CLAIM.to_string(),
            user_prefix: String::new(),
            groups: GROUPS_CLAIM.to_string(),
            run_id: Some(RUN_ID_CLAIM.to_string()),
            namespace: NAMESPACE_CLAIM.to_string(),
            namespaces: NAMESPACES_CLAIM.to_string(),
        }
    }
}

impl ClaimMap {
    /// Rewrites the principal so the claims the guard, the access reviewer and the tools read
    /// (`kube_user`, `kube_groups`, `run_id`, `namespace`, `namespaces`) hold what the configured
    /// claims say.
    /// A token cannot set a canonical claim itself unless it is the configured one.
    pub fn canonicalize(&self, principal: &mut Principal) {
        let claims = &mut principal.claims;
        let user = claims
            .get(&self.user)
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(|u| format!("{}{u}", self.user_prefix));
        let groups = claims.get(&self.groups).cloned();
        let run = self.run_id.as_ref().and_then(|c| claims.get(c).cloned());
        let namespace = claims.get(&self.namespace).cloned();
        let namespaces = claims.get(&self.namespaces).cloned();
        let mut set = |key: &str, value: Option<Value>| match value {
            Some(v) => {
                claims.insert(key.to_string(), v);
            }
            None => {
                claims.remove(key);
            }
        };
        set(USER_CLAIM, user.map(Value::String));
        set(GROUPS_CLAIM, groups);
        set(RUN_ID_CLAIM, run);
        set(NAMESPACE_CLAIM, namespace.clone());
        set(NAMESPACES_CLAIM, namespaces);
        principal.namespace = namespace.and_then(|v| v.as_str().map(str::to_string));
    }
}

/// `value` as a label value: itself when valid, else its label-safe characters with a hash of
/// the original appended, so distinct values stay distinct.
pub fn label_safe(value: &str) -> String {
    if RunId::parse(value).is_ok() {
        return value.to_string();
    }
    let hash = hex::encode(&Sha256::digest(value.as_bytes())[..5]);
    let kept: String = value
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '-',
        })
        .take(63 - hash.len() - 1)
        .collect();
    let kept = kept.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    match kept.is_empty() {
        true => hash,
        false => format!("{kept}-{hash}"),
    }
}

pub fn run_id_of(value: &str) -> Result<RunId, NameError> {
    RunId::parse(&label_safe(value))
}

pub fn launcher_of(value: &str) -> Result<Launcher, NameError> {
    Launcher::parse(&label_safe(value))
}

#[cfg(test)]
mod tests {
    use crate::config::*;
    use crucible_mcp_auth::principal::Callers;
    use serde_json::json;

    fn principal(claims: Value) -> Principal {
        match claims {
            Value::Object(m) => Principal::from_claims(m, Callers::People).unwrap(),
            other => panic!("{other}"),
        }
    }

    #[test]
    fn the_default_label_domain_is_crucibles() {
        let l = Labels::new(DEFAULT_LABEL_DOMAIN);
        assert_eq!(l.managed_by, "crucible.dev/managed-by");
        assert_eq!(l.run_id, "crucible.dev/run-id");
        assert_eq!(l.launched_by, "crucible.dev/launched-by");
        assert_eq!(l.workload, "crucible.dev/workload");
        assert_eq!(l.plan_hash, "crucible.dev/plan-hash");
        assert_eq!(l.stopped_replicas, "crucible.dev/stopped-replicas");
        assert_eq!(l.actor, "crucible.dev/actor");
        assert_eq!(l.task_cancelled, "crucible.dev/task-cancelled");
        assert_eq!(
            Labels::new("kueue-mcp.example.com").run_id,
            "kueue-mcp.example.com/run-id"
        );
    }

    #[test]
    fn default_claims_leave_a_crucible_token_as_it_was() {
        let claims = json!({"sub": "alice", "kube_user": "alice", "kube_groups": ["ml"],
            "run_id": "r1", "namespace": "team-a", "scope": "kueue.read"});
        let mut p = principal(claims.clone());
        ClaimMap::default().canonicalize(&mut p);
        assert_eq!(Value::Object(p.claims.clone()), claims);
        assert_eq!(p.namespace.as_deref(), Some("team-a"));
    }

    #[test]
    fn mapped_claims_replace_the_canonical_ones_a_token_carries() {
        let map = ClaimMap {
            user: "preferred_username".into(),
            user_prefix: "oidc:".into(),
            groups: "groups".into(),
            run_id: None,
            namespace: "default_namespace".into(),
            namespaces: "allowed_namespaces".into(),
        };
        let mut p = principal(json!({
            "sub": "f3a1", "preferred_username": "alice", "groups": ["ml-team"],
            "kube_user": "admin", "kube_groups": ["cluster-admins"], "run_id": "forged",
            "namespace": "kube-system", "default_namespace": "team-a",
        }));
        map.canonicalize(&mut p);
        assert_eq!(p.claims["kube_user"], "oidc:alice");
        assert_eq!(p.claims["kube_groups"], json!(["ml-team"]));
        assert!(
            !p.claims.contains_key("run_id"),
            "no run claim is configured"
        );
        assert_eq!(p.claims["namespace"], "team-a");
        assert_eq!(p.namespace.as_deref(), Some("team-a"));

        let mut bare = principal(json!({"sub": "s", "kube_user": "admin"}));
        map.canonicalize(&mut bare);
        assert!(!bare.claims.contains_key("kube_user"), "{:?}", bare.claims);
        assert_eq!(bare.namespace, None);
    }

    #[test]
    fn values_that_are_not_label_safe_are_sanitized_and_kept_distinct() {
        assert_eq!(label_safe("alice"), "alice");
        assert_eq!(label_safe("r-1.x_y"), "r-1.x_y");
        let email = label_safe("alice@example.com");
        assert!(email.starts_with("alice-example.com-"), "{email}");
        assert_ne!(email, label_safe("alice#example.com"));
        let long = label_safe(&format!("{}@x", "a".repeat(100)));
        assert!(long.len() <= 63, "{long}");
        assert!(RunId::parse(&long).is_ok());
        let symbols = label_safe("@@@");
        assert_eq!(symbols.len(), 10);
        assert!(RunId::parse(&symbols).is_ok());
        assert_eq!(label_safe("@@@"), symbols, "deterministic");
        assert!(run_id_of("auth0|abc").is_ok());
        assert!(launcher_of("CN=Alice Smith,OU=x").is_ok());
    }

    #[test]
    fn a_pull_secret_serves_images_on_its_registry_or_under_its_prefix() {
        let quay = PullSecret::parse("quay-pull=quay.io/example").unwrap();
        assert!(quay.serves("quay.io/example/crucible-smoke:abc"));
        assert!(quay.serves("quay.io/example/x@sha256:0123"));
        assert!(
            !quay.serves("quay.io/examplex/y:1"),
            "a prefix ends at a path boundary"
        );
        assert!(!quay.serves("quay.io/other/y:1"));
        let host = PullSecret::parse("local=registry.local:5000").unwrap();
        assert!(host.serves("registry.local:5000/a/b:1"));
        assert!(!host.serves("registry.local/a/b:1"));
        let hub = PullSecret::parse("hub=docker.io/library").unwrap();
        assert!(hub.serves("ubuntu:24.04"));
        assert!(hub.serves("library/ubuntu"));
        assert!(!hub.serves("acme/tool:1"));
        let named = PullSecret::parse("plain").unwrap();
        assert!(named.registries.is_empty());
        assert!(!named.serves("quay.io/example/x"));
        for bad in ["", "=quay.io", "name="] {
            assert!(PullSecret::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn repeated_pull_secrets_gather_their_registries() {
        let merged = merge_pull_secrets(
            ["a=quay.io", "b", "a=ghcr.io", "a=quay.io"]
                .iter()
                .map(|s| PullSecret::parse(s).unwrap())
                .collect(),
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].registries, ["quay.io", "ghcr.io"]);
        assert_eq!(merged[0].to_string(), "a (quay.io, ghcr.io)");
        assert_eq!(merged[1].to_string(), "b");
    }

    #[test]
    fn accelerator_resources_match_exactly_or_by_prefix() {
        let h = Hardware::default();
        for r in [
            "nvidia.com/gpu",
            "nvidia.com/mig-1g.10gb",
            "amd.com/gpu",
            "habana.ai/gaudi",
            "google.com/tpu",
        ] {
            assert!(h.is_accelerator(r), "{r}");
        }
        for r in ["cpu", "nvidia.com/gpu.shared", "memory"] {
            assert!(!h.is_accelerator(r), "{r}");
        }
    }
}
