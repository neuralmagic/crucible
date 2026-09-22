//! Which cluster a launch may dispatch onto, and who may choose it.
//!
//! A dispatch target is a name [`crate::runs::clusters::ClusterClients`] can resolve: the reserved `hub`,
//! a spoke mounted under the clusters directory, or a personal target a user registered as a
//! `kubeconfig` secret. Selection is an authorization decision, not a preference — the eligible set
//! is derived from the caller's principals, controller policy, and what the pack's declared agent
//! substrate needs, and a name outside that set is refused rather than silently downgraded.
//!
//! Three rules shape the set:
//!
//!   * A shared cluster with no [`ClusterPolicy`] entry is open to every authenticated caller. This
//!     is what keeps a deployment that never configures policy behaving exactly as it did before
//!     targets existed.
//!   * A shared cluster with an entry admits only callers whose principals intersect it.
//!   * A personal target admits its owner alone, and appears in nobody else's set — not even an
//!     admin's, who can act on the secret but may not dispatch through it.
//!
//! The chosen name is persisted on the issue ([`crate::issues::store::set_dispatch_target`]) so every
//! pod that issue's lifecycle dispatches — scope turn, grounded rank, loop run — lands on the same
//! cluster the launcher was authorized for.

#![allow(clippy::disallowed_macros)]

use crate::authz::action::ResourceType;
use crate::authz::decision::Resource;
use crate::authz::model::{Principal, Principals};
use crate::playbooks::dispatch::{DispatchCapability, PackAgent};
use crate::runs::clusters::{personal_name, personal_secret_id};
use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// What sort of credential backs a target, which is also what decides who may see it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// The controller's own cluster.
    Hub,
    /// A kubeconfig the deployment mounted under the clusters directory.
    Spoke,
    /// A kubeconfig one user registered, usable by that user alone.
    Personal,
}

/// One target a caller may choose, as the launch form renders it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct EligibleTarget {
    /// The name to send back as `dispatch_target`.
    pub name: String,
    /// What to show a human. The cluster name for a shared target, the secret's own name for a
    /// personal one (whose `name` is an opaque id).
    pub label: String,
    pub kind: TargetKind,
    /// Whether omitting `dispatch_target` selects this one.
    pub default: bool,
    /// The owning principal of a personal target; null for a shared cluster.
    pub owner: Option<String>,
}

/// Why a launch may not dispatch where it asked. Every variant names what the caller would have to
/// change, because the refusal is what they read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TargetRefusal {
    #[error(
        "dispatch target {name:?} is not a cluster this controller can reach; \
         eligible targets are: {}",
        render_names(eligible)
    )]
    Unknown { name: String, eligible: Vec<String> },
    #[error(
        "dispatch target {name:?} exists but this caller is not authorized for it; \
         eligible targets are: {}",
        render_names(eligible)
    )]
    NotAuthorized { name: String, eligible: Vec<String> },
    #[error("dispatch target {name:?} cannot run this pack: {detail}")]
    Incompatible { name: String, detail: String },
    #[error("this caller has no dispatch target that can run this pack: {detail}")]
    NoneEligible { detail: String },
    #[error("choosing a dispatch target needs an authenticated caller")]
    Anonymous,
}

/// Render an eligible-target list for a refusal message. An empty set says so rather than trailing
/// a bare colon.
fn render_names(names: &[String]) -> String {
    match names.is_empty() {
        true => "none".to_string(),
        false => names.join(", "),
    }
}

/// Which principals may dispatch to which shared cluster. A cluster absent from the map is
/// unrestricted; a cluster present admits only the principals listed against it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClusterPolicy {
    allowed: BTreeMap<String, Vec<Principal>>,
}

/// Who may dispatch to which shared cluster. Parsed at startup so a malformed entry fails the
/// boot rather than silently leaving a cluster unrestricted.
pub fn cluster_policy(cfg: &crate::config::ControllerCfg) -> Result<ClusterPolicy> {
    ClusterPolicy::parse(&cfg.cluster_policy)
}

impl ClusterPolicy {
    /// Parse `cluster=principal[,principal…]` entries. A principal is the same `user:<login>` /
    /// `group:<path>` grammar the secrets registry uses, so one identity model covers both.
    pub fn parse(entries: &[String]) -> Result<Self> {
        let mut allowed: BTreeMap<String, Vec<Principal>> = BTreeMap::new();
        for raw in entries {
            let entry = raw.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((cluster, principals)) = entry.split_once('=') else {
                bail!(
                    "CONTROLLER_CLUSTER_POLICY entry `{entry}` is not \
                     `cluster=principal[,principal]`"
                );
            };
            let cluster = cluster.trim();
            if cluster.is_empty() {
                bail!("CONTROLLER_CLUSTER_POLICY entry `{entry}` names no cluster");
            }
            let parsed = principals
                .split(';')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| {
                    Principal::parse(p).map_err(|e| {
                        anyhow::anyhow!("CONTROLLER_CLUSTER_POLICY entry `{entry}`: {e}")
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if parsed.is_empty() {
                bail!(
                    "CONTROLLER_CLUSTER_POLICY entry `{entry}` lists no principals; omit the \
                     cluster entirely to leave it unrestricted"
                );
            }
            allowed
                .entry(cluster.to_string())
                .or_default()
                .extend(parsed);
        }
        Ok(ClusterPolicy { allowed })
    }

    /// Whether `caller` may dispatch to the shared cluster `name`.
    pub fn admits(&self, name: &str, caller: &Principals) -> bool {
        match self.allowed.get(name) {
            None => true,
            Some(owners) => owners.iter().any(|o| caller.covers(o)),
        }
    }
}

/// A personal target as the registry holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalTarget {
    /// The `personal:<secret id>` name a launch sends back.
    pub name: String,
    /// The secret name its owner gave it, for the launch form to show.
    pub label: String,
    pub owner: Principal,
}

/// Every kubeconfig target `caller` owns. A `kubeconfig`-kind secret whose consumer is the hub is
/// a cluster credential the controller may dispatch through; any other kind or consumer is a run
/// credential and is not a target.
///
/// Ownership is the filter, and it is the only one: a personal target is never listed for, offered
/// to, or resolvable by anybody but the principals that cover its owner. An admin reading the
/// secrets registry sees the row; they still get no dispatch target out of it.
pub async fn personal_targets(
    pool: &sqlx::PgPool,
    caller: &Principals,
) -> anyhow::Result<Vec<PersonalTarget>> {
    let owners: Vec<Principal> = caller.all();
    if owners.is_empty() {
        return Ok(Vec::new());
    }
    Ok(crate::secrets::store::list(pool, Some(&owners))
        .await?
        .into_iter()
        .filter(|s| {
            s.kind == crate::secrets::SecretKind::Kubeconfig
                && s.consumer == crate::secrets::ConsumerClass::Hub
        })
        .map(|s| PersonalTarget {
            name: personal_name(&s.id),
            label: s.name.to_string(),
            owner: s.owner,
        })
        .collect())
}

/// Everything the eligible-set derivation reads. Grouped rather than passed as six arguments,
/// because the launch endpoints, the preview gate and the tests all build the same shape.
pub struct TargetContext<'a> {
    /// Every connected shared cluster, hub first ([`crate::runs::clusters::ClusterClients::names`]).
    pub connected: &'a [String],
    /// The controller's configured default, chosen when a launch names no target.
    pub default: &'a str,
    pub policy: &'a ClusterPolicy,
    /// The `dispatch_target:dispatch` decision under the policy set in force (RFC-0003
    /// C-CONDITIONS): a shared cluster is a platform-owned target, a personal one is owned by
    /// its secret's owner.
    pub rules: &'a (dyn Fn(&Resource) -> bool + Sync),
    /// The caller's registered kubeconfig targets, already filtered to what they own.
    pub personal: &'a [PersonalTarget],
    pub capability: DispatchCapability,
}

/// The target `name` as the decision sees it.
pub fn target_resource(name: &str, owner: Option<&Principal>) -> Resource {
    match owner {
        Some(owner) => Resource::new(ResourceType::DispatchTarget, name, owner.clone()),
        None => Resource::platform(ResourceType::DispatchTarget, name),
    }
}

/// Every target `caller` may dispatch `agent` onto, default first and the rest in connection order.
/// Empty when the caller may reach nothing that can run the pack, which the launch endpoints turn
/// into [`TargetRefusal::NoneEligible`].
pub fn eligible(
    ctx: &TargetContext<'_>,
    caller: &Principals,
    agent: &PackAgent,
) -> Vec<EligibleTarget> {
    // A pack this deployment cannot dispatch at all has no eligible target anywhere: the refusal
    // is about the deployment's substrate, not about which cluster it lands on.
    if ctx.capability.refusal(agent).is_some() {
        return Vec::new();
    }
    // Local mode runs the engine as a subprocess here; there is nothing to choose between.
    if ctx.capability.is_local() {
        return Vec::new();
    }
    let mut out: Vec<EligibleTarget> = ctx
        .connected
        .iter()
        .filter(|name| ctx.policy.admits(name, caller))
        .filter(|name| (ctx.rules)(&target_resource(name, None)))
        .map(|name| EligibleTarget {
            name: name.clone(),
            label: name.clone(),
            kind: match name.as_str() {
                crate::runs::clusters::HUB_CLUSTER => TargetKind::Hub,
                _ => TargetKind::Spoke,
            },
            default: name == ctx.default,
            owner: None,
        })
        .collect();
    out.extend(
        ctx.personal
            .iter()
            .filter(|t| caller.covers(&t.owner))
            .filter(|t| (ctx.rules)(&target_resource(&t.name, Some(&t.owner))))
            .map(|t| EligibleTarget {
                name: t.name.clone(),
                label: t.label.clone(),
                kind: TargetKind::Personal,
                default: false,
                owner: Some(t.owner.to_string()),
            }),
    );
    out.sort_by_key(|t| !t.default);
    out
}

/// Resolve what a launch asked for into the target it will dispatch onto. `None` selects the
/// controller default, which is what every launch authored before targets existed sends.
///
/// Fails closed: an unreachable name, a name the caller holds no principal for, and a caller with
/// no eligible target at all are three distinct refusals, because they need three different fixes.
pub fn resolve(
    ctx: &TargetContext<'_>,
    caller: &Principals,
    agent: &PackAgent,
    requested: Option<&str>,
) -> Result<String, TargetRefusal> {
    if let Some(detail) = ctx.capability.refusal(agent) {
        return Err(match requested {
            Some(name) => TargetRefusal::Incompatible {
                name: name.to_string(),
                detail,
            },
            None => TargetRefusal::NoneEligible { detail },
        });
    }
    // Local mode has one place to run and no choice to authorize. A launch that names a target
    // anyway is refused rather than quietly running somewhere else.
    if ctx.capability.is_local() {
        return match requested {
            None => Ok(ctx.default.to_string()),
            Some(name) => Err(TargetRefusal::Incompatible {
                name: name.to_string(),
                detail: "this deployment runs playbooks as a local subprocess \
                         (CONTROLLER_PLAYBOOK_EXECUTOR=local), so there is no cluster to choose"
                    .to_string(),
            }),
        };
    }

    let eligible_targets = eligible(ctx, caller, agent);
    let names: Vec<String> = eligible_targets.iter().map(|t| t.name.clone()).collect();

    let Some(requested) = requested.map(str::trim).filter(|r| !r.is_empty()) else {
        // The configured default has to survive the caller's own policy: a deployment that fences
        // its default cluster off from a group has fenced that group out of launching at all, and
        // saying so beats dispatching them somewhere they did not ask for.
        return match names.iter().any(|n| n == ctx.default) {
            true => Ok(ctx.default.to_string()),
            false => Err(TargetRefusal::NotAuthorized {
                name: ctx.default.to_string(),
                eligible: names,
            }),
        };
    };

    if names.iter().any(|n| n == requested) {
        return Ok(requested.to_string());
    }
    // Distinguish "no such cluster" from "not yours": a personal target belonging to someone else
    // reads as unknown, because confirming it exists leaks another user's registration.
    let exists_shared = ctx.connected.iter().any(|n| n == requested);
    if caller.user().is_none() && caller.all().is_empty() {
        return Err(TargetRefusal::Anonymous);
    }
    Err(match exists_shared {
        true => TargetRefusal::NotAuthorized {
            name: requested.to_string(),
            eligible: names,
        },
        false => TargetRefusal::Unknown {
            name: requested.to_string(),
            eligible: names,
        },
    })
}

/// The registry-backed [`crate::runs::clusters::PersonalKubeconfigs`]: look the secret up by the id
/// inside the target name and read its current bytes with the hub's Vault identity.
///
/// Deleting the registry row is the revocation: the lookup misses, the resolver answers `None`, and
/// [`crate::runs::clusters::ClusterClients::evict`] drops whatever client the target had cached.
pub struct RegistryKubeconfigs {
    pool: sqlx::PgPool,
    vault: std::sync::Arc<crate::secrets::vault::VaultClient>,
}

impl RegistryKubeconfigs {
    pub fn new(
        pool: sqlx::PgPool,
        vault: std::sync::Arc<crate::secrets::vault::VaultClient>,
    ) -> Self {
        RegistryKubeconfigs { pool, vault }
    }
}

#[async_trait::async_trait]
impl crate::runs::clusters::PersonalKubeconfigs for RegistryKubeconfigs {
    async fn kubeconfig(&self, target: &str) -> Result<Option<String>> {
        let Some(id) = personal_secret_id(target) else {
            return Ok(None);
        };
        let Some(secret) = crate::secrets::store::get(&self.pool, id).await? else {
            return Ok(None);
        };
        // The kind and consumer are re-checked at resolution, not just at listing: a row whose
        // kind was changed out from under a pinned target must stop resolving, not keep working.
        if secret.kind != crate::secrets::SecretKind::Kubeconfig
            || secret.consumer != crate::secrets::ConsumerClass::Hub
        {
            bail!(
                "secret {} is not a hub-consumed kubeconfig, so it is not a dispatch target",
                secret.name
            );
        }
        let owner = secret.owner.clone();
        let name = secret.name.to_string();
        let (value, version) = crate::secrets::read::current_value(&self.vault, &secret).await?;
        let mut conn = self.pool.acquire().await?;
        crate::secrets::store::audit(
            &mut conn,
            &crate::secrets::store::NewAudit {
                secret_id: Some(id),
                secret_name: &name,
                owner: &owner,
                action: crate::secrets::AuditAction::HubRead,
                actor: None,
                detail: Some(&format!("dispatch target resolved at version {version}")),
            },
        )
        .await?;
        Ok(Some(value))
    }
}

/// Why a kubeconfig cannot serve as a dispatch target. The probe runs at registration, where the
/// person who pointed at the cluster is watching, rather than at the first launch.
#[derive(Debug, thiserror::Error)]
pub enum ProbeFailure {
    #[error("the kubeconfig does not parse: {0}")]
    Unusable(String),
    #[error("the kubeconfig names no namespace; a dispatch target must be namespace-scoped")]
    NoNamespace,
    #[error("the cluster could not be reached: {0}")]
    Unreachable(String),
    #[error(
        "this credential may not create pods in namespace {namespace}, so no run could be \
         dispatched through it"
    )]
    Denied { namespace: String },
}

/// What a probed target turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedTarget {
    /// The namespace the kubeconfig's context scopes it to; every pod dispatched through this
    /// target lands here.
    pub namespace: String,
    /// The API server the context points at, for the registration receipt.
    pub server: String,
}

/// Prove a kubeconfig can actually run work before it is offered as a target: it resolves, it names
/// a namespace, the API server answers, and the credential may create pods there.
///
/// A `SelfSubjectAccessReview` is the right question because it is asked AS the credential — the
/// controller learns whether that user can dispatch without needing any permission of its own on
/// the target cluster.
pub async fn probe(kubeconfig_yaml: &str) -> Result<ProbedTarget, ProbeFailure> {
    use k8s_openapi::api::authorization::v1::{
        ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
    };

    crate::install_crypto_provider();
    let parsed = kube::config::Kubeconfig::from_yaml(kubeconfig_yaml)
        .map_err(|e| ProbeFailure::Unusable(e.to_string()))?;
    let config = kube::Config::from_custom_kubeconfig(parsed, &Default::default())
        .await
        .map_err(|e| ProbeFailure::Unusable(e.to_string()))?;
    let server = config.cluster_url.to_string();
    let namespace = config.default_namespace.clone();
    if namespace.trim().is_empty() || namespace == "default" {
        return Err(ProbeFailure::NoNamespace);
    }
    let client =
        kube::Client::try_from(config).map_err(|e| ProbeFailure::Unreachable(e.to_string()))?;
    let api: kube::Api<SelfSubjectAccessReview> = kube::Api::all(client);
    let review = SelfSubjectAccessReview {
        spec: SelfSubjectAccessReviewSpec {
            resource_attributes: Some(ResourceAttributes {
                namespace: Some(namespace.clone()),
                verb: Some("create".to_string()),
                resource: Some("pods".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let answer = api
        .create(&Default::default(), &review)
        .await
        .map_err(|e| ProbeFailure::Unreachable(format!("{e}")))?;
    match answer.status.is_some_and(|s| s.allowed) {
        true => Ok(ProbedTarget { namespace, server }),
        false => Err(ProbeFailure::Denied { namespace }),
    }
}

#[cfg(test)]
mod tests {
    use crate::config::PlaybookExecutor;
    use crate::playbooks::dispatch::{DispatchCapability, PackAgent};
    use crate::runs::dispatch_target::*;

    fn caller(user: &str, groups: &[&str]) -> Principals {
        let groups: Vec<String> = groups.iter().map(|g| g.to_string()).collect();
        Principals::new(Some(user), &groups)
    }

    fn agent(backend: &str) -> PackAgent {
        PackAgent::new(backend.to_string(), None)
    }

    /// The policy set decides per target beside the cluster policy: a shared cluster it refuses
    /// leaves the eligible set and resolves as not authorized, and a personal target it refuses is
    /// absent even for its owner.
    #[test]
    fn the_policy_rules_gate_each_target_beside_the_cluster_policy() {
        let connected = vec!["hub".to_string(), "wharf".to_string()];
        let policy = ClusterPolicy::default();
        let alice = Principals::new(Some("alice"), &[]);
        let personal = vec![PersonalTarget {
            name: "personal:s1".to_string(),
            label: "my-cluster".to_string(),
            owner: Principal::User("alice".into()),
        }];
        let refuse_wharf_and_personal =
            |r: &Resource| r.id != "wharf" && r.owner == Principal::platform();
        let ctx = TargetContext {
            rules: &refuse_wharf_and_personal,
            ..ctx(&connected, &policy, &personal)
        };
        let agent = PackAgent::new("local".to_string(), None);
        let names: Vec<String> = eligible(&ctx, &alice, &agent)
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["hub"]);
        assert!(matches!(
            resolve(&ctx, &alice, &agent, Some("wharf")),
            Err(TargetRefusal::NotAuthorized { name, .. }) if name == "wharf"
        ));
        assert!(matches!(
            resolve(&ctx, &alice, &agent, Some("personal:s1")),
            Err(TargetRefusal::Unknown { .. })
        ));
        assert_eq!(resolve(&ctx, &alice, &agent, None).expect("default"), "hub");
        assert_eq!(target_resource("wharf", None).owner, Principal::platform());
        assert_eq!(
            target_resource("personal:s1", Some(&personal[0].owner)).owner,
            Principal::User("alice".into())
        );
    }

    fn ctx<'a>(
        connected: &'a [String],
        policy: &'a ClusterPolicy,
        personal: &'a [PersonalTarget],
    ) -> TargetContext<'a> {
        TargetContext {
            connected,
            default: "hub",
            policy,
            rules: &|_| true,
            personal,
            capability: DispatchCapability::new(PlaybookExecutor::Pod, true),
        }
    }

    /// The compatibility promise: a deployment that configures no policy offers every connected
    /// cluster to every caller, and a launch naming nothing lands on the configured default.
    #[test]
    fn no_policy_leaves_every_connected_cluster_open() {
        let connected = vec!["hub".to_string(), "wharf".to_string()];
        let policy = ClusterPolicy::default();
        let ctx = ctx(&connected, &policy, &[]);
        let who = caller("alice", &[]);

        let offered = eligible(&ctx, &who, &agent("local"));
        assert_eq!(
            offered.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["hub", "wharf"]
        );
        assert!(offered[0].default, "the configured default sorts first");
        assert_eq!(
            resolve(&ctx, &who, &agent("local"), None).expect("default"),
            "hub"
        );
        assert_eq!(
            resolve(&ctx, &who, &agent("local"), Some("wharf")).expect("named"),
            "wharf"
        );
    }

    /// A policy entry fences its cluster: a caller holding none of the listed principals does not
    /// see it and may not name it, and the refusal names what they could have used instead.
    #[test]
    fn a_policy_entry_admits_only_the_principals_it_lists() {
        let connected = vec!["hub".to_string(), "wharf".to_string()];
        let policy =
            ClusterPolicy::parse(&["wharf=group:/groups/llm-d".to_string()]).expect("parses");
        let ctx = ctx(&connected, &policy, &[]);

        let member = caller("alice", &["/groups/llm-d"]);
        assert_eq!(
            resolve(&ctx, &member, &agent("local"), Some("wharf")).expect("member"),
            "wharf"
        );

        let outsider = caller("bob", &["/groups/team-y"]);
        assert_eq!(
            eligible(&ctx, &outsider, &agent("local"))
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["hub"],
            "a fenced cluster is not offered"
        );
        let refusal = resolve(&ctx, &outsider, &agent("local"), Some("wharf")).expect_err("fenced");
        assert!(
            matches!(refusal, TargetRefusal::NotAuthorized { .. }),
            "{refusal:?}"
        );
        assert!(
            refusal.to_string().contains("hub"),
            "names the eligible set"
        );
    }

    /// Fail closed rather than silently downgrading: a caller fenced out of the configured default
    /// is told so, not dispatched somewhere they never chose.
    #[test]
    fn a_caller_fenced_off_the_default_is_refused_not_redirected() {
        let connected = vec!["hub".to_string(), "wharf".to_string()];
        let policy =
            ClusterPolicy::parse(&["hub=group:/groups/admins".to_string()]).expect("parses");
        let ctx = ctx(&connected, &policy, &[]);
        let who = caller("bob", &["/groups/team-y"]);

        match resolve(&ctx, &who, &agent("local"), None).expect_err("refused") {
            TargetRefusal::NotAuthorized { name, eligible } => {
                assert_eq!(name, "hub");
                assert_eq!(eligible, ["wharf"]);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A personal target belongs to its owner alone. Nobody else sees it, and to them it reads as
    /// unknown rather than forbidden, so its existence is not confirmed to a stranger.
    #[test]
    fn a_personal_target_is_invisible_to_everyone_but_its_owner() {
        let connected = vec!["hub".to_string()];
        let policy = ClusterPolicy::default();
        let mine = PersonalTarget {
            name: personal_name("secret-id"),
            label: "my-cluster".to_string(),
            owner: Principal::parse("user:alice").expect("principal"),
        };
        let personal = [mine.clone()];
        let ctx = ctx(&connected, &policy, &personal);

        let owner = caller("alice", &[]);
        let offered = eligible(&ctx, &owner, &agent("local"));
        assert_eq!(offered.len(), 2);
        let row = offered
            .iter()
            .find(|t| t.kind == TargetKind::Personal)
            .expect("offered");
        assert_eq!(row.label, "my-cluster", "the form shows the secret's name");
        assert!(!row.default, "a personal target is never the default");
        assert_eq!(
            resolve(&ctx, &owner, &agent("local"), Some(&mine.name)).expect("owner"),
            mine.name
        );

        // An admin is still not the owner: acting on the registry row grants no dispatch through it.
        let admin = caller("root", &["/groups/admins"]);
        assert_eq!(
            eligible(&ctx, &admin, &agent("local"))
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["hub"]
        );
        let refusal =
            resolve(&ctx, &admin, &agent("local"), Some(&mine.name)).expect_err("not theirs");
        assert!(
            matches!(refusal, TargetRefusal::Unknown { .. }),
            "another user's target reads as unknown, never as forbidden: {refusal:?}"
        );
    }

    /// A group-owned target reaches every member, which is what makes a team credential usable.
    #[test]
    fn a_group_owned_target_reaches_its_members() {
        let connected = vec!["hub".to_string()];
        let policy = ClusterPolicy::default();
        let personal = [PersonalTarget {
            name: personal_name("team-id"),
            label: "team-cluster".to_string(),
            owner: Principal::parse("group:/groups/llm-d").expect("principal"),
        }];
        let ctx = ctx(&connected, &policy, &personal);

        assert_eq!(
            eligible(&ctx, &caller("alice", &["/groups/llm-d"]), &agent("local")).len(),
            2
        );
        assert_eq!(
            eligible(&ctx, &caller("bob", &[]), &agent("local")).len(),
            1
        );
    }

    /// A pack this deployment cannot dispatch has no eligible target anywhere: the refusal is about
    /// the substrate, so it must not read as a cluster-authorization problem.
    #[test]
    fn a_pack_the_deployment_cannot_run_has_no_target_at_all() {
        let connected = vec!["hub".to_string(), "wharf".to_string()];
        let policy = ClusterPolicy::default();
        let mut ctx = ctx(&connected, &policy, &[]);
        ctx.capability = DispatchCapability::new(PlaybookExecutor::Pod, false);
        let who = caller("alice", &[]);

        assert!(eligible(&ctx, &who, &agent("local")).is_empty());
        let refusal = resolve(&ctx, &who, &agent("local"), None).expect_err("refused");
        assert!(
            matches!(refusal, TargetRefusal::NoneEligible { .. }),
            "{refusal:?}"
        );
        assert!(refusal.to_string().contains("CONTROLLER_DEPLOY_PROFILE"));
    }

    /// Local mode has one place to run. Omitting a target still works (the compatibility path);
    /// naming one is refused rather than quietly ignored.
    #[test]
    fn local_mode_takes_no_target_but_still_launches() {
        let connected = vec!["hub".to_string()];
        let policy = ClusterPolicy::default();
        let mut ctx = ctx(&connected, &policy, &[]);
        ctx.capability = DispatchCapability::new(PlaybookExecutor::Local, false);
        let who = caller("alice", &[]);

        assert_eq!(
            resolve(&ctx, &who, &agent("local"), None).expect("default"),
            "hub"
        );
        let refusal = resolve(&ctx, &who, &agent("local"), Some("wharf")).expect_err("refused");
        assert!(
            matches!(refusal, TargetRefusal::Incompatible { .. }),
            "{refusal:?}"
        );
    }

    /// A name no cluster answers to is distinct from one the caller may not use: the two need
    /// different fixes, so they are different refusals.
    #[test]
    fn an_unreachable_name_is_unknown_not_unauthorized() {
        let connected = vec!["hub".to_string()];
        let policy = ClusterPolicy::default();
        let ctx = ctx(&connected, &policy, &[]);
        let refusal = resolve(
            &ctx,
            &caller("alice", &[]),
            &agent("local"),
            Some("nowhere"),
        )
        .expect_err("refused");
        assert!(
            matches!(refusal, TargetRefusal::Unknown { .. }),
            "{refusal:?}"
        );
    }

    /// A blank target is an empty form field, not a choice; it selects the default.
    #[test]
    fn a_blank_target_reads_as_no_choice() {
        let connected = vec!["hub".to_string()];
        let policy = ClusterPolicy::default();
        let ctx = ctx(&connected, &policy, &[]);
        assert_eq!(
            resolve(&ctx, &caller("alice", &[]), &agent("local"), Some("   ")).expect("default"),
            "hub"
        );
    }

    #[test]
    fn policy_parsing_refuses_what_it_cannot_enforce() {
        assert!(
            ClusterPolicy::parse(&["wharf".to_string()]).is_err(),
            "no `=`"
        );
        assert!(
            ClusterPolicy::parse(&["=user:alice".to_string()]).is_err(),
            "no cluster"
        );
        assert!(
            ClusterPolicy::parse(&["wharf=".to_string()]).is_err(),
            "no principals"
        );
        assert!(
            ClusterPolicy::parse(&["wharf=alice".to_string()]).is_err(),
            "an unprefixed principal is not a principal"
        );
        assert_eq!(
            ClusterPolicy::parse(&["  ".to_string()]).expect("blank ok"),
            ClusterPolicy::default()
        );

        let policy = ClusterPolicy::parse(&["wharf=user:alice;group:/groups/llm-d".to_string()])
            .expect("parses");
        assert!(policy.admits("wharf", &caller("alice", &[])));
        assert!(policy.admits("wharf", &caller("bob", &["/groups/llm-d"])));
        assert!(!policy.admits("wharf", &caller("bob", &[])));
        assert!(
            policy.admits("hub", &caller("bob", &[])),
            "unlisted stays open"
        );
    }

    /// The name split is what keeps a personal target from colliding with a mounted spoke, and what
    /// lets a stored target say which resolver owns it.
    #[test]
    fn a_personal_name_round_trips_and_never_looks_like_a_cluster() {
        let name = personal_name("0199c0de");
        assert_eq!(personal_secret_id(&name), Some("0199c0de"));
        assert_eq!(personal_secret_id("hub"), None);
        assert_eq!(personal_secret_id("wharf"), None);
    }
}
