//! What a dispatch is allowed to resolve, and what it resolves to.
//!
//! A launch names a scope (the repo an issue belongs to, the playbook a launch runs, a domain).
//! Every binding on that scope is projected into the run, and the launcher's principals have to
//! cover the owner of every one of them. A pack that declares a name the scope has no binding for
//! does not launch: the run would start without a credential it says it needs.
//!
//! Nothing here reads a value. The output is the item list the dispatch reads Vault for and
//! delivers as the run's Secret.

use crate::authz::model::Principals;
use crate::playbooks::providers::ModelProvider;
use crate::secrets::grant::{self, GrantMint};
use crate::secrets::manifest::DeclaredSecret;
use crate::secrets::store::{self, BindingRow, SecretRow};
use crate::secrets::{ScopeKind, SecretKind, SecretName};
use anyhow::Result;
use std::fmt;

/// What a launch resolves its bindings against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub id: String,
}

impl Scope {
    pub fn repo(id: impl Into<String>) -> Self {
        Scope {
            kind: ScopeKind::Repo,
            id: id.into(),
        }
    }

    pub fn playbook(id: impl Into<String>) -> Self {
        Scope {
            kind: ScopeKind::Playbook,
            id: id.into(),
        }
    }

    pub fn domain(id: impl Into<String>) -> Self {
        Scope {
            kind: ScopeKind::Domain,
            id: id.into(),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind.as_str(), self.id)
    }
}

/// Why a launch may not resolve its secrets. Every variant names the secret or the declared name
/// it is about, because the refusal is what the launcher reads.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error(
        "secret {name} is bound to {scope} and owned by {owner}; the launcher is not {owner} \
         and holds no group that is"
    )]
    NotOwned {
        name: SecretName,
        owner: String,
        scope: Scope,
    },
    #[error("the pack declares secret {name}, and {scope} has no binding for it")]
    MissingBinding { name: SecretName, scope: Scope },
    #[error(
        "secret {name} was bound to {scope} against pack revision {bound}, which the pin bump to \
         {current} moved; an owner member has to re-bind it"
    )]
    Stale {
        name: SecretName,
        scope: Scope,
        bound: String,
        current: String,
    },
    #[error("{scope} has bindings, and this launch carries no authenticated principal")]
    Anonymous { scope: Scope },
    #[error(
        "secret {name} was bound to {scope} against pack revision {bound}; a draft test-fire runs \
         content no one has reviewed, so the binding does not follow it. Re-bind without pinning a \
         revision to use it from a draft"
    )]
    Unreviewed {
        name: SecretName,
        scope: Scope,
        bound: String,
    },
    #[error("provider {provider} names secret {name:?}, which is not a secret name: {reason}")]
    ProviderSecretName {
        provider: String,
        name: String,
        reason: String,
    },
    #[error(
        "provider {provider} names secret {name} under {owner}, which the registry does not hold"
    )]
    ProviderSecretMissing {
        provider: String,
        name: SecretName,
        owner: String,
    },
    #[error(
        "provider {provider} names secret {name}, which is a {kind} secret; an inference provider \
         takes an inference_api_key"
    )]
    ProviderSecretWrongKind {
        provider: String,
        name: SecretName,
        kind: &'static str,
    },
    #[error(
        "provider {provider} is a {kind} provider, which authenticates with the deploy profile's \
         ambient credentials; it may not name secret {name}"
    )]
    ProviderTakesNoSecret {
        provider: String,
        kind: &'static str,
        name: String,
    },
    #[error("provider {provider} names secret {name}, whose value cannot be delivered: {reason}")]
    ProviderCredentials {
        provider: String,
        name: SecretName,
        reason: String,
    },
    #[error(
        "the provider's key {name} lands on {projection}, which this scope's binding of {bound} \
         already holds; one of the two would be overwritten"
    )]
    ProviderSecretCollides {
        name: SecretName,
        projection: String,
        bound: SecretName,
    },
    #[error(
        "secret {name} is bound to {scope} as agent-visible {projection}, and the pack's stored \
         exposure discloses no agent-context credential under that name (it discloses: {declared})"
    )]
    Undisclosed {
        name: SecretName,
        scope: Scope,
        /// The name the value would appear under inside the run.
        projection: String,
        /// The agent-context credentials the exposure does disclose.
        declared: String,
    },
    #[error(
        "secret {name} is bound to {scope} as agent-visible {projection}, and this revision stored \
         no exposure to disclose it in; re-register the pack with an engine that declares one"
    )]
    Undeclared {
        name: SecretName,
        scope: Scope,
        projection: String,
    },
}

/// What content a launch is about to run, which decides whether a binding's review still applies.
/// A draft is not a revision: its content changes on every save, so nothing about it was reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revision<'a> {
    /// A registered playbook at the revision it is pinned to. `None` is a scope with no pack
    /// revision to compare against, which pins nothing.
    Published(Option<&'a str>),
    /// A draft test-fire.
    Draft,
}

/// The owned form of [`Revision`], for the dispatch struct that carries it across an await.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedRevision {
    Published(Option<String>),
    Draft,
}

impl OwnedRevision {
    pub fn as_revision(&self) -> Revision<'_> {
        match self {
            OwnedRevision::Published(rev) => Revision::Published(rev.as_deref()),
            OwnedRevision::Draft => Revision::Draft,
        }
    }
}

/// Resolve one launch. `Ok(Ok(..))` is the item list a grant is minted from — empty when the scope
/// binds nothing, which is every launch until somebody binds a secret. `Ok(Err(..))` is a refusal
/// the caller reports verbatim; `Err` is the database failing.
///
/// `revision` is what this launch runs. A binding made against a different published revision is
/// stale: the declared names, their kinds, and their projections all belong to the revision the
/// binder reviewed. A draft carries that reasoning further — it has no reviewed revision at all, so
/// only an unpinned binding follows it, and only ever to a launcher who already owns the secret.
pub async fn resolve(
    pool: &sqlx::PgPool,
    scope: &Scope,
    declared: &[DeclaredSecret],
    launcher: &Principals,
    revision: Revision<'_>,
    exposure: Option<&crate::playbooks::exposure::Exposure>,
) -> Result<Result<Vec<GrantMint>, Refusal>> {
    let bound = store::bindings_for_scope(pool, scope.kind, &scope.id).await?;
    if let Some(missing) = declared
        .iter()
        .find(|d| !bound.iter().any(|(b, _)| b.declared_name == d.name))
    {
        return Ok(Err(Refusal::MissingBinding {
            name: missing.name.clone(),
            scope: scope.clone(),
        }));
    }
    if bound.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    if launcher.all().is_empty() {
        return Ok(Err(Refusal::Anonymous {
            scope: scope.clone(),
        }));
    }
    let mut mints = Vec::with_capacity(bound.len());
    for (binding, secret) in &bound {
        if let Some(refusal) = check_one(scope, binding, secret, launcher, revision) {
            return Ok(Err(refusal));
        }
        if let Some(refusal) = check_disclosed(scope, binding, secret, exposure) {
            return Ok(Err(refusal));
        }
        mints.push(grant::mint_from(binding, secret));
    }
    Ok(Ok(mints))
}

/// The secret a resolved provider spends, once the registry has been asked whether it would take
/// it: the row is what a delivery reads the bytes of, and the variable is where the harness expects
/// the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSecret {
    pub row: SecretRow,
    pub key_env: &'static str,
}

/// The secret a resolved provider contributes to a dispatch. `Ok(Ok(None))` is a provider that
/// names none, which is every Vertex provider and any deployment whose keys come from the pod's
/// environment already.
///
/// Unlike a scope binding, this is not checked against the launcher's principals: the key was
/// attached to the provider by an administrator, and every launcher who may pick the provider is
/// thereby entitled to spend it. The kind keeps it broker-only, so no agent ever reads the value.
pub async fn resolve_provider_secret(
    pool: &sqlx::PgPool,
    provider: &ModelProvider,
) -> Result<Result<Option<ProviderSecret>, Refusal>> {
    let Some(secret) = provider.secret.as_ref() else {
        return Ok(Ok(None));
    };
    let Some(key_env) = provider.api_key_env() else {
        return Ok(Err(Refusal::ProviderTakesNoSecret {
            provider: provider.id.clone(),
            kind: provider.kind.as_str(),
            name: secret.name.clone(),
        }));
    };
    let name = match SecretName::parse(&secret.name) {
        Ok(name) => name,
        Err(e) => {
            return Ok(Err(Refusal::ProviderSecretName {
                provider: provider.id.clone(),
                name: secret.name.clone(),
                reason: e.to_string(),
            }));
        }
    };
    let Some(row) = store::find_owned(pool, &secret.owner, &name).await? else {
        return Ok(Err(Refusal::ProviderSecretMissing {
            provider: provider.id.clone(),
            name,
            owner: secret.owner.to_string(),
        }));
    };
    if row.kind != SecretKind::InferenceApiKey {
        return Ok(Err(Refusal::ProviderSecretWrongKind {
            provider: provider.id.clone(),
            name,
            kind: row.kind.as_str(),
        }));
    }
    Ok(Ok(Some(ProviderSecret { row, key_env })))
}

/// The per-binding half: ownership first, then the revision the binding was reviewed against.
fn check_one(
    scope: &Scope,
    binding: &BindingRow,
    secret: &SecretRow,
    launcher: &Principals,
    revision: Revision<'_>,
) -> Option<Refusal> {
    if !launcher.covers(&secret.owner) {
        return Some(Refusal::NotOwned {
            name: secret.name.clone(),
            owner: secret.owner.to_string(),
            scope: scope.clone(),
        });
    }
    match (binding.pack_rev.as_deref(), revision) {
        (Some(bound), Revision::Published(Some(current))) if bound != current => {
            Some(Refusal::Stale {
                name: secret.name.clone(),
                scope: scope.clone(),
                bound: bound.to_string(),
                current: current.to_string(),
            })
        }
        (Some(bound), Revision::Draft) => Some(Refusal::Unreviewed {
            name: secret.name.clone(),
            scope: scope.clone(),
            bound: bound.to_string(),
        }),
        _ => None,
    }
}

/// A value the agent itself can read has to be in the pack's disclosure: the engine discloses
/// `[agent].env` names as `context = agent` credentials. `exposure` of `None` is absent-legacy,
/// a revision that disclosed nothing, so it is granted nothing the agent could read; a broker-only
/// binding is never agent-readable.
fn check_disclosed(
    scope: &Scope,
    binding: &BindingRow,
    secret: &SecretRow,
    exposure: Option<&crate::playbooks::exposure::Exposure>,
) -> Option<Refusal> {
    if secret.visibility != crate::secrets::Visibility::AgentVisible {
        return None;
    }
    let Some(exposure) = exposure else {
        return Some(Refusal::Undeclared {
            name: secret.name.clone(),
            scope: scope.clone(),
            projection: binding.projection.clone(),
        });
    };
    let declared_name = binding.declared_name.to_string();
    if exposure.covers_agent_credential(&binding.projection)
        || exposure.covers_agent_credential(&declared_name)
    {
        return None;
    }
    let declared = match exposure.agent_credentials().as_slice() {
        [] => "nothing".to_string(),
        names => names.join(", "),
    };
    Some(Refusal::Undisclosed {
        name: secret.name.clone(),
        scope: scope.clone(),
        projection: binding.projection.clone(),
        declared,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::model::Principal;
    use crate::secrets::store::{NewBinding, NewSecret};
    use crate::secrets::{ConsumerClass, ProjectionKind, SecretKind, SecretMode, Visibility};

    /// Register one secret and bind it to `scope` under `declared`, returning the secret id.
    async fn bind(
        pool: &sqlx::PgPool,
        owner: &str,
        name: &str,
        scope: &Scope,
        declared: &str,
        pack_rev: Option<&str>,
    ) -> String {
        let owner = Principal::parse(owner).expect("owner");
        let name = SecretName::parse(name).expect("name");
        let id = uuid::Uuid::now_v7().to_string();
        let binding_id = uuid::Uuid::now_v7().to_string();
        let declared = SecretName::parse(declared).expect("declared");
        let mut conn = pool.acquire().await.expect("conn");
        store::insert(
            &mut conn,
            &NewSecret {
                id: &id,
                name: &name,
                owner: &owner,
                kind: SecretKind::Opaque,
                visibility: Visibility::BrokerOnly,
                consumer: ConsumerClass::Run,
                mode: SecretMode::Managed,
                vault_path: "user:x/y",
                current_version: Some(1),
                created_by: Some("alice"),
            },
        )
        .await
        .expect("register");
        store::insert_binding(
            &mut conn,
            &NewBinding {
                id: &binding_id,
                secret_id: &id,
                scope_kind: scope.kind,
                scope_id: &scope.id,
                projection_kind: ProjectionKind::Env,
                projection: "PR_TOKEN",
                declared_name: &declared,
                pack_rev,
                schema_digest: None,
                created_by: Some("alice"),
            },
        )
        .await
        .expect("bind");
        id
    }

    /// Register one secret with no binding at all, the way an inference key is registered.
    async fn register(pool: &sqlx::PgPool, owner: &str, name: &str, kind: SecretKind) -> String {
        let owner = Principal::parse(owner).expect("owner");
        let name = SecretName::parse(name).expect("name");
        let id = uuid::Uuid::now_v7().to_string();
        let mut conn = pool.acquire().await.expect("conn");
        store::insert(
            &mut conn,
            &NewSecret {
                id: &id,
                name: &name,
                owner: &owner,
                kind,
                visibility: Visibility::BrokerOnly,
                consumer: ConsumerClass::Run,
                mode: SecretMode::Managed,
                vault_path: "platform/inference",
                current_version: Some(1),
                created_by: Some("alice"),
            },
        )
        .await
        .expect("register");
        id
    }

    /// A provider whose key, when it names one, lives in alice's keyspace.
    fn provider(
        id: &str,
        kind: crate::playbooks::providers::ProviderKind,
        secret: Option<&str>,
    ) -> ModelProvider {
        provider_owned(id, kind, secret.map(|name| ("user:alice", name)))
    }

    fn provider_owned(
        id: &str,
        kind: crate::playbooks::providers::ProviderKind,
        secret: Option<(&str, &str)>,
    ) -> ModelProvider {
        ModelProvider {
            owner: crate::authz::model::Principal::platform(),
            id: id.to_string(),
            display_name: id.to_string(),
            kind,
            models: kind
                .curated_models()
                .iter()
                .map(|m| m.to_string())
                .collect(),
            default_model: kind.default_model().unwrap_or_default().to_string(),
            secret: secret.map(
                |(owner, name)| crate::playbooks::providers::ProviderSecretRef {
                    name: name.to_string(),
                    owner: Principal::parse(owner).expect("a principal"),
                },
            ),
            endpoint: None,
            harness: None,
            enabled: true,
            created_by: "alice".to_string(),
            created_at: "2026-08-29T00:00:00Z".to_string(),
            updated_at: "2026-08-29T00:00:00Z".to_string(),
        }
    }

    /// Register an agent-visible secret bound to `scope` under `declared`.
    async fn bind_agent_visible(
        pool: &sqlx::PgPool,
        name: &str,
        scope: &Scope,
        projection: &str,
    ) -> String {
        let owner = Principal::parse("user:alice").expect("owner");
        let secret_name = SecretName::parse(name).expect("name");
        let id = uuid::Uuid::now_v7().to_string();
        let binding_id = uuid::Uuid::now_v7().to_string();
        let declared = SecretName::parse(name).expect("declared");
        let mut conn = pool.acquire().await.expect("conn");
        store::insert(
            &mut conn,
            &NewSecret {
                id: &id,
                name: &secret_name,
                owner: &owner,
                kind: SecretKind::Opaque,
                visibility: Visibility::AgentVisible,
                consumer: ConsumerClass::Run,
                mode: SecretMode::Managed,
                vault_path: "user:alice/gh",
                current_version: Some(1),
                created_by: Some("alice"),
            },
        )
        .await
        .expect("register");
        store::insert_binding(
            &mut conn,
            &NewBinding {
                id: &binding_id,
                secret_id: &id,
                scope_kind: scope.kind,
                scope_id: &scope.id,
                projection_kind: ProjectionKind::Env,
                projection,
                declared_name: &declared,
                pack_rev: None,
                schema_digest: None,
                created_by: Some("alice"),
            },
        )
        .await
        .expect("bind");
        id
    }

    fn exposure_with(agent_credential: Option<&str>) -> crate::playbooks::exposure::Exposure {
        let capabilities = agent_credential
            .map(|name| {
                vec![crate::playbooks::exposure::Capability::Known(
                    crate::playbooks::exposure::KnownCapability::Credential {
                        name: name.to_string(),
                        context: crate::playbooks::exposure::CredentialContext::Agent,
                        system: Some("github".to_string()),
                        scope: None,
                    },
                )]
            })
            .unwrap_or_default();
        crate::playbooks::exposure::Exposure {
            version: 1,
            outputs: Vec::new(),
            capabilities,
        }
    }

    /// A value the agent can read has to be in the pack's disclosure. The refusal names the grant
    /// and what the exposure does disclose; absent-legacy checks nothing.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_undisclosed_agent_visible_binding_is_refused(pool: sqlx::PgPool) {
        let scope = Scope::playbook("survey");
        bind_agent_visible(&pool, "gh_token", &scope, "GH_TOKEN").await;
        let launcher = Principals::new(Some("alice"), &[]);
        let want = declared("gh_token");

        let refusal = resolve(
            &pool,
            &scope,
            &want,
            &launcher,
            Revision::Published(None),
            Some(&exposure_with(Some("SOMETHING_ELSE"))),
        )
        .await
        .expect("resolve")
        .expect_err("an undisclosed agent-visible binding is refused");
        let message = refusal.to_string();
        assert!(
            message.contains("gh_token") && message.contains("GH_TOKEN"),
            "the refusal names the grant: {message}"
        );
        assert!(
            message.contains("SOMETHING_ELSE"),
            "and what the pack did disclose: {message}"
        );

        resolve(
            &pool,
            &scope,
            &want,
            &launcher,
            Revision::Published(None),
            Some(&exposure_with(Some("GH_TOKEN"))),
        )
        .await
        .expect("resolve")
        .expect("a disclosed credential launches");

        let refusal = resolve(
            &pool,
            &scope,
            &want,
            &launcher,
            Revision::Published(None),
            None,
        )
        .await
        .expect("resolve")
        .expect_err("a revision with no stored exposure disclosed nothing the agent may read");
        assert!(
            matches!(&refusal, Refusal::Undeclared { projection, .. } if projection == "GH_TOKEN"),
            "{refusal}"
        );
        let message = refusal.to_string();
        assert!(
            message.contains("gh_token") && message.contains("no exposure"),
            "the refusal names the grant and the missing declaration: {message}"
        );
    }

    /// A broker-held value is never agent-readable, so it launches under an exposure naming none,
    /// and under no exposure at all.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_broker_only_binding_is_unaffected_by_the_agent_disclosure(pool: sqlx::PgPool) {
        let scope = Scope::playbook("survey");
        bind(&pool, "user:alice", "pr_token", &scope, "pr_token", None).await;
        let launcher = Principals::new(Some("alice"), &[]);
        for exposure in [Some(exposure_with(None)), None] {
            resolve(
                &pool,
                &scope,
                &declared("pr_token"),
                &launcher,
                Revision::Published(None),
                exposure.as_ref(),
            )
            .await
            .expect("resolve")
            .expect("a broker-only binding needs no agent disclosure");
        }
    }

    fn declared(name: &str) -> Vec<DeclaredSecret> {
        vec![DeclaredSecret {
            name: SecretName::parse(name).expect("name"),
            kind: SecretKind::Opaque,
            projection: None,
        }]
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_scope_that_binds_nothing_resolves_to_nothing(pool: sqlx::PgPool) {
        let scope = Scope::playbook("survey");
        let launcher = Principals::new(Some("alice"), &[]);
        let out = resolve(
            &pool,
            &scope,
            &[],
            &launcher,
            Revision::Published(None),
            None,
        )
        .await
        .expect("resolve")
        .expect("no refusal");
        assert!(out.is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_launcher_who_covers_the_owner_resolves_the_binding(pool: sqlx::PgPool) {
        let scope = Scope::playbook("survey");
        let id = bind(
            &pool,
            "group:/groups/team-x",
            "pr_token",
            &scope,
            "pr_token",
            None,
        )
        .await;
        let launcher = Principals::new(Some("alice"), &["/groups/team-x".to_string()]);
        let out = resolve(
            &pool,
            &scope,
            &declared("pr_token"),
            &launcher,
            Revision::Published(None),
            None,
        )
        .await
        .expect("resolve")
        .expect("no refusal");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].item.secret_id, id);
        assert_eq!(out[0].item.projection, "PR_TOKEN");
        assert_eq!(out[0].owner.to_string(), "group:/groups/team-x");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_launcher_outside_the_owner_group_is_refused_with_the_secret_named(
        pool: sqlx::PgPool,
    ) {
        let scope = Scope::playbook("survey");
        bind(
            &pool,
            "group:/groups/team-x",
            "pr_token",
            &scope,
            "pr_token",
            None,
        )
        .await;
        let launcher = Principals::new(Some("bob"), &["/groups/team-y".to_string()]);
        let refusal = resolve(
            &pool,
            &scope,
            &declared("pr_token"),
            &launcher,
            Revision::Published(None),
            None,
        )
        .await
        .expect("resolve")
        .expect_err("refused");
        let Refusal::NotOwned { name, owner, .. } = &refusal else {
            panic!("expected a not-owned refusal, got {refusal:?}");
        };
        assert_eq!(name.as_str(), "pr_token");
        assert_eq!(owner, "group:/groups/team-x");
        assert!(refusal.to_string().contains("pr_token"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_declared_name_with_no_binding_is_refused_with_the_name(pool: sqlx::PgPool) {
        let scope = Scope::playbook("survey");
        bind(&pool, "user:alice", "pr_token", &scope, "pr_token", None).await;
        let launcher = Principals::new(Some("alice"), &[]);
        let mut want = declared("pr_token");
        want.push(DeclaredSecret {
            name: SecretName::parse("registry").expect("name"),
            kind: SecretKind::RegistryAuthfile,
            projection: None,
        });
        let refusal = resolve(
            &pool,
            &scope,
            &want,
            &launcher,
            Revision::Published(None),
            None,
        )
        .await
        .expect("resolve")
        .expect_err("refused");
        assert!(
            matches!(&refusal, Refusal::MissingBinding { name, .. } if name.as_str() == "registry"),
            "got {refusal:?}"
        );
        assert!(refusal.to_string().contains("registry"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_bound_scope_refuses_a_launch_with_no_principal(pool: sqlx::PgPool) {
        let scope = Scope::playbook("survey");
        bind(&pool, "user:alice", "pr_token", &scope, "pr_token", None).await;
        let refusal = resolve(
            &pool,
            &scope,
            &declared("pr_token"),
            &Principals::default(),
            Revision::Published(None),
            None,
        )
        .await
        .expect("resolve")
        .expect_err("refused");
        assert!(matches!(refusal, Refusal::Anonymous { .. }), "{refusal:?}");
    }

    /// A pin bump moves the pack revision, and the binding was reviewed against the old one: the
    /// launch parks until an owner member re-binds against the revision now pinned.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_pack_revision_the_binding_predates_is_refused(pool: sqlx::PgPool) {
        let scope = Scope::playbook("survey");
        bind(
            &pool,
            "user:alice",
            "pr_token",
            &scope,
            "pr_token",
            Some("rev-1"),
        )
        .await;
        let launcher = Principals::new(Some("alice"), &[]);
        let want = declared("pr_token");
        let refusal = resolve(
            &pool,
            &scope,
            &want,
            &launcher,
            Revision::Published(Some("rev-2")),
            None,
        )
        .await
        .expect("resolve")
        .expect_err("refused");
        let Refusal::Stale { bound, current, .. } = &refusal else {
            panic!("expected a stale refusal, got {refusal:?}");
        };
        assert_eq!((bound.as_str(), current.as_str()), ("rev-1", "rev-2"));
        // Re-binding against the revision now pinned clears it.
        resolve(
            &pool,
            &scope,
            &want,
            &launcher,
            Revision::Published(Some("rev-1")),
            None,
        )
        .await
        .expect("resolve")
        .expect("no refusal");
    }

    /// A draft test-fire is how a pack that needs a credential gets iterated on at all, so an
    /// unpinned binding follows it. The owner check is what keeps that safe: the launcher already
    /// holds the secret, so the draft can reach nothing they could not read anyway.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_draft_resolves_an_unpinned_binding(pool: sqlx::PgPool) {
        let scope = Scope::playbook("docs-drift");
        bind(&pool, "user:alice", "pr_token", &scope, "pr_token", None).await;
        let launcher = Principals::new(Some("alice"), &[]);
        let mints = resolve(
            &pool,
            &scope,
            &declared("pr_token"),
            &launcher,
            Revision::Draft,
            None,
        )
        .await
        .expect("resolve")
        .expect("no refusal");
        assert_eq!(mints.len(), 1);
    }

    /// A binding pinned to a published revision was reviewed against that content. A draft is not
    /// it and changes on every save, so the binding does not follow — the refusal says how to opt
    /// in rather than leaving the author guessing why the value is absent.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_draft_refuses_a_binding_pinned_to_a_reviewed_revision(pool: sqlx::PgPool) {
        let scope = Scope::playbook("docs-drift");
        bind(
            &pool,
            "user:alice",
            "pr_token",
            &scope,
            "pr_token",
            Some("rev-1"),
        )
        .await;
        let launcher = Principals::new(Some("alice"), &[]);
        let refusal = resolve(
            &pool,
            &scope,
            &declared("pr_token"),
            &launcher,
            Revision::Draft,
            None,
        )
        .await
        .expect("resolve")
        .expect_err("refused");
        let Refusal::Unreviewed { bound, .. } = &refusal else {
            panic!("expected an unreviewed refusal, got {refusal:?}");
        };
        assert_eq!(bound, "rev-1");
    }

    /// Ownership still gates a draft: someone else's secret is refused exactly as it is on a
    /// registered launch, so allowing drafts widens nothing.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_draft_still_refuses_a_secret_the_launcher_does_not_own(pool: sqlx::PgPool) {
        let scope = Scope::playbook("docs-drift");
        bind(
            &pool,
            "group:/groups/team-x",
            "pr_token",
            &scope,
            "pr_token",
            None,
        )
        .await;
        let launcher = Principals::new(Some("mallory"), &[]);
        let refusal = resolve(
            &pool,
            &scope,
            &declared("pr_token"),
            &launcher,
            Revision::Draft,
            None,
        )
        .await
        .expect("resolve")
        .expect_err("refused");
        assert!(matches!(refusal, Refusal::NotOwned { .. }), "{refusal:?}");
    }

    /// The zero-config case, and the Vertex case: nothing to project, so a dispatch renders exactly
    /// as it did before the registry existed.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_provider_that_names_no_secret_projects_nothing(pool: sqlx::PgPool) {
        for kind in [
            crate::playbooks::providers::ProviderKind::Vertex,
            crate::playbooks::providers::ProviderKind::OpenAi,
        ] {
            let out = resolve_provider_secret(&pool, &provider("p", kind, None))
                .await
                .expect("resolve")
                .expect("no refusal");
            assert!(out.is_none(), "{kind:?} named no secret");
        }
    }

    /// The key lands in the variable the provider's own harness reads, and nowhere else.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_provider_key_projects_as_its_harness_env_var(pool: sqlx::PgPool) {
        let anthropic = register(
            &pool,
            "group:/groups/platform",
            "anthropic_key",
            SecretKind::InferenceApiKey,
        )
        .await;
        let openai = register(
            &pool,
            "user:alice",
            "openai_key",
            SecretKind::InferenceApiKey,
        )
        .await;
        let cases = [
            (
                crate::playbooks::providers::ProviderKind::Anthropic,
                ("group:/groups/platform", "anthropic_key"),
                anthropic,
                "ANTHROPIC_API_KEY",
            ),
            (
                crate::playbooks::providers::ProviderKind::OpenAi,
                ("user:alice", "openai_key"),
                openai,
                "OPENAI_API_KEY",
            ),
        ];
        for (kind, secret, id, env) in cases {
            let name = secret.1;
            let found = resolve_provider_secret(&pool, &provider_owned("p", kind, Some(secret)))
                .await
                .expect("resolve")
                .expect("no refusal")
                .expect("a secret");
            assert_eq!(found.row.id, id);
            assert_eq!(found.key_env, env);
            assert_eq!(found.row.name.as_str(), name);
        }
    }

    /// Vertex runs on the deploy profile's ADC. A key attached to one would be paid for and never
    /// used, so the dispatch says so instead of dropping it silently.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_vertex_provider_may_not_carry_a_key(pool: sqlx::PgPool) {
        register(
            &pool,
            "user:alice",
            "vertex_key",
            SecretKind::InferenceApiKey,
        )
        .await;
        let refusal = resolve_provider_secret(
            &pool,
            &provider(
                "vx",
                crate::playbooks::providers::ProviderKind::Vertex,
                Some("vertex_key"),
            ),
        )
        .await
        .expect("resolve")
        .expect_err("refused");
        assert!(
            matches!(&refusal, Refusal::ProviderTakesNoSecret { kind, .. } if *kind == "vertex"),
            "{refusal:?}"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_provider_key_of_the_wrong_kind_is_refused(pool: sqlx::PgPool) {
        register(&pool, "user:alice", "openai_key", SecretKind::Opaque).await;
        let refusal = resolve_provider_secret(
            &pool,
            &provider(
                "oa",
                crate::playbooks::providers::ProviderKind::OpenAi,
                Some("openai_key"),
            ),
        )
        .await
        .expect("resolve")
        .expect_err("refused");
        assert!(
            matches!(&refusal, Refusal::ProviderSecretWrongKind { kind, .. } if *kind == "opaque"),
            "{refusal:?}"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_provider_key_the_registry_does_not_hold_is_refused(pool: sqlx::PgPool) {
        let refusal = resolve_provider_secret(
            &pool,
            &provider(
                "oa",
                crate::playbooks::providers::ProviderKind::OpenAi,
                Some("gone"),
            ),
        )
        .await
        .expect("resolve")
        .expect_err("refused");
        assert!(
            matches!(refusal, Refusal::ProviderSecretMissing { .. }),
            "{refusal:?}"
        );
    }

    /// A name is unique per owner, so two owners can hold one. The reference carries the owner, so
    /// somebody else registering the same name in their own keyspace neither moves the key the
    /// provider spends nor refuses the dispatch that spends it.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_second_owner_of_the_same_name_cannot_shadow_a_provider_key(pool: sqlx::PgPool) {
        register(
            &pool,
            "group:/groups/platform",
            "openai_key",
            SecretKind::InferenceApiKey,
        )
        .await;
        register(
            &pool,
            "user:alice",
            "openai_key",
            SecretKind::InferenceApiKey,
        )
        .await;
        let found = resolve_provider_secret(
            &pool,
            &provider_owned(
                "oa",
                crate::playbooks::providers::ProviderKind::OpenAi,
                Some(("group:/groups/platform", "openai_key")),
            ),
        )
        .await
        .expect("resolve")
        .expect("no refusal")
        .expect("a secret");
        assert_eq!(
            found.row.owner,
            Principal::parse("group:/groups/platform").expect("a principal")
        );

        // The same name in a keyspace the provider does not point at is simply not this key.
        let refusal = resolve_provider_secret(
            &pool,
            &provider_owned(
                "oa",
                crate::playbooks::providers::ProviderKind::OpenAi,
                Some(("group:/groups/nobody", "openai_key")),
            ),
        )
        .await
        .expect("resolve")
        .expect_err("refused");
        assert!(
            matches!(refusal, Refusal::ProviderSecretMissing { .. }),
            "{refusal:?}"
        );
    }

    /// The mint feeds the same assembly every bound secret does, so the key reaches the pod as a
    /// `secretKeyRef` and never as a plain value.
    /// A single key lands under the harness's variable; a credentials map lands one variable per
    /// entry, each in its own Secret key named after the secret, so two providers' keys and a
    /// scope binding of the same name can never share one.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_provider_key_expands_into_the_run_delivery(pool: sqlx::PgPool) {
        register(
            &pool,
            "user:alice",
            "openai_key",
            SecretKind::InferenceApiKey,
        )
        .await;
        let found = resolve_provider_secret(
            &pool,
            &provider(
                "oa",
                crate::playbooks::providers::ProviderKind::OpenAi,
                Some("openai_key"),
            ),
        )
        .await
        .expect("resolve")
        .expect("no refusal")
        .expect("a secret");
        let single =
            crate::secrets::deliver::expand_credentials(&found, "sk-test").expect("expand");
        assert_eq!(
            single.env,
            vec![(
                "OPENAI_API_KEY".to_string(),
                "openai_key.OPENAI_API_KEY".to_string()
            )]
        );
        assert_eq!(
            single
                .data
                .get("openai_key.OPENAI_API_KEY")
                .map(String::as_str),
            Some("sk-test")
        );
        assert!(
            single.agent_visible.is_empty(),
            "an inference key never crosses into the sandbox"
        );

        let map = crate::secrets::deliver::expand_credentials(
            &found,
            r#"{"OPENAI_API_KEY": "sk-map", "OPENAI_ORG_ID": "org-7"}"#,
        )
        .expect("expand");
        assert_eq!(map.env.len(), 2);
        assert_eq!(
            map.data.get("openai_key.OPENAI_ORG_ID").map(String::as_str),
            Some("org-7")
        );

        let wrong = crate::secrets::deliver::expand_credentials(
            &found,
            r#"{"ANTHROPIC_API_KEY": "sk-ant"}"#,
        )
        .expect_err("a map without the harness's key is refused");
        assert!(wrong.contains("OPENAI_API_KEY"), "{wrong}");
    }
}
