//! The secrets registry's authorization model: principals, the vocabulary a secret row is spelled
//! in, and the rules that decide what may be registered and by whom.
//!
//! Nothing here holds bytes. A registered value passes through [`SecretValue`] on its way to Vault
//! and is dropped with the request; every type in this module describes metadata.
//!
//! Ownership is exact: a [`crate::authz::model::Principal`], normalized once at parse and compared
//! whole.

pub(crate) mod api;
pub mod credentials;
pub mod deliver;
pub mod github_app;
pub mod grant;
pub mod launch;
pub mod manifest;
pub mod minter;
pub mod provider;
pub mod read;
pub mod store;
pub mod vault;

use crate::authz::model::Principal;
use crate::secrets::vault::{PathError, VaultPath};
use crate::wire_enum::wire_enum;
use std::fmt;

/// The longest a secret name may be. Names are declared in manifests and typed into forms; the cap
/// keeps them addressable as one Vault path segment.
const MAX_NAME: usize = 96;

/// A registered secret's value, on its way from the request body to Vault. `Debug` redacts, there
/// is no `Display`, and the only way out is [`SecretValue::expose`], which every call site of has
/// to justify.
#[derive(Clone)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(raw: impl Into<String>) -> Self {
        SecretValue(raw.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

/// The one place a value is written out: the redemption bundle body, over the pod's own
/// TokenReview-authenticated connection. No other response type in the crate holds one.
impl serde::Serialize for SecretValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// What sort of credential a secret holds. The kind decides how it is projected and whether it may
/// ever be visible to the agent.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum SecretKind {
    /// A bare string: an API key, a token, a header value.
    Opaque,
    /// File content projected into the pod's tmpfs.
    File,
    /// A container registry authfile — in-pod pulls and pushes, never the kubelet's pull secret.
    RegistryAuthfile,
    /// A cluster credential the hub itself uses.
    Kubeconfig,
    /// An inference provider's API key. Projected as an environment variable the loop reads, never
    /// relayed into the sandbox: the agent talks to the model through the loop, not directly.
    InferenceApiKey,
}

wire_enum!(SecretKind, "secret kind", both, {
    SecretKind::Opaque => "opaque",
    SecretKind::File => "file",
    SecretKind::RegistryAuthfile => "registry_authfile",
    SecretKind::Kubeconfig => "kubeconfig",
    SecretKind::InferenceApiKey => "inference_api_key",
});

/// Whether the agent inside the sandbox may see the value. `BrokerOnly` is the default everywhere;
/// `AgentVisible` is an explicit opt-in that only [`SecretKind::Opaque`] can take.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum Visibility {
    BrokerOnly,
    AgentVisible,
}

wire_enum!(Visibility, "secret visibility", both, {
    Visibility::BrokerOnly => "broker_only",
    Visibility::AgentVisible => "agent_visible",
});

/// Who consumes the bytes: a dispatched pod, or the hub on its own behalf.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum ConsumerClass {
    Run,
    Hub,
}

wire_enum!(ConsumerClass, "secret consumer class", both, {
    ConsumerClass::Run => "run",
    ConsumerClass::Hub => "hub",
});

/// Where the bytes live. A managed secret's Vault path belongs to the registry's own mount; a
/// reference points at a path someone else owns; a minted secret has none at rest.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum SecretMode {
    Managed,
    Reference,
    Minted,
}

wire_enum!(SecretMode, "secret mode", both, {
    SecretMode::Managed => "managed",
    SecretMode::Reference => "reference",
    SecretMode::Minted => "minted",
});

/// What a binding attaches a secret to.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum ScopeKind {
    Repo,
    Playbook,
    Domain,
}

wire_enum!(ScopeKind, "binding scope kind", both, {
    ScopeKind::Repo => "repo",
    ScopeKind::Playbook => "playbook",
    ScopeKind::Domain => "domain",
});

/// How a bound secret reaches the run.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum ProjectionKind {
    Env,
    File,
}

wire_enum!(ProjectionKind, "projection kind", both, {
    ProjectionKind::Env => "env",
    ProjectionKind::File => "file",
});

/// Every action the audit trail records.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum AuditAction {
    Register,
    Rotate,
    Bind,
    Unbind,
    Delete,
    Grant,
    HubRead,
    Redeem,
    Transfer,
}

wire_enum!(AuditAction, "secret audit action", both, {
    AuditAction::Register => "register",
    AuditAction::Rotate => "rotate",
    AuditAction::Bind => "bind",
    AuditAction::Unbind => "unbind",
    AuditAction::Delete => "delete",
    AuditAction::Grant => "grant",
    AuditAction::HubRead => "hub_read",
    AuditAction::Redeem => "redeem",
    AuditAction::Transfer => "transfer",
});

/// A validated secret name: one Vault path segment, and the token a manifest declares.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretName(String);

/// Why a secret name was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("a secret name cannot be empty")]
    Empty,
    #[error("a secret name is at most {MAX_NAME} characters")]
    TooLong,
    #[error("secret name {value:?} has a character outside [A-Za-z0-9._-]")]
    BadCharacter { value: String },
}

impl TryFrom<String> for SecretName {
    type Error = NameError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        SecretName::parse(&raw)
    }
}

impl SecretName {
    pub fn parse(raw: &str) -> Result<Self, NameError> {
        let raw = raw.trim().to_lowercase();
        if raw.is_empty() {
            return Err(NameError::Empty);
        }
        if raw.len() > MAX_NAME {
            return Err(NameError::TooLong);
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(NameError::BadCharacter { value: raw });
        }
        Ok(SecretName(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for SecretName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for SecretName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        SecretName::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// The prefix every path this controller owns sits under, inside whatever mount it is configured
/// with. The deploy-sync secrets already live at `<MOUNT_PREFIX>/deploy/<release>/…`; the registry
/// is their sibling, so one policy statement on `<mount>/data/crucible/*` covers both and a mount
/// shared with other applications is never claimed at its top level.
pub const MOUNT_PREFIX: &str = "crucible";

/// The KV path a managed secret's bytes live at: `crucible/registry/<principal>/<name>`.
pub fn managed_path(owner: &Principal, name: &SecretName) -> Result<VaultPath, PathError> {
    VaultPath::parse(&format!("{MOUNT_PREFIX}/registry/{owner}/{name}"))
}

/// The single KV key a managed secret's bytes are written under. One key, so a rotation replaces
/// the whole payload and a redemption never has to guess which field is the credential.
pub const VALUE_KEY: &str = "value";

/// Why a registration or a binding was refused on its own terms, before any authorization check.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("a {kind} secret is broker-side only and cannot be agent_visible")]
    NotAgentVisible { kind: &'static str },
    #[error("consumer class hub is only for kubeconfig secrets")]
    HubNeedsKubeconfig,
    #[error("a reference secret has no version to rotate; re-point it instead")]
    RotateReference,
    #[error(
        "a minted secret has no stored version to rotate; it is issued fresh at every dispatch"
    )]
    RotateMinted,
    #[error("a {kind} secret cannot be minted; a minter issues an opaque token")]
    MintNeedsOpaque { kind: &'static str },
    #[error("a minted secret is delivered to a run, not read by the hub")]
    MintNeedsRunConsumer,
    #[error("a {kind} secret is projected as a file, not an environment variable")]
    NeedsFileProjection { kind: &'static str },
    #[error(
        "an agent_visible secret cannot hold a controller api key (a `{}` value): an agent that reads one can drive this controller's own mutation surface",
        crate::identity::api_key::PREFIX
    )]
    AgentVisibleControllerKey,
}

impl SecretKind {
    /// Whether the agent may ever see this kind. File-shaped credentials are broker-side only: an
    /// agent that can read one can put it in a PR.
    pub fn may_be_agent_visible(self) -> bool {
        matches!(self, SecretKind::Opaque)
    }

    /// The consumer class a registration that named none gets.
    pub fn default_consumer(self) -> ConsumerClass {
        match self {
            SecretKind::Kubeconfig => ConsumerClass::Hub,
            _ => ConsumerClass::Run,
        }
    }
}

/// The kind/visibility/consumer rules, checked once wherever a secret is registered.
pub fn check_shape(
    kind: SecretKind,
    visibility: Visibility,
    consumer: ConsumerClass,
) -> Result<(), PolicyError> {
    if visibility == Visibility::AgentVisible && !kind.may_be_agent_visible() {
        return Err(PolicyError::NotAgentVisible {
            kind: kind.as_str(),
        });
    }
    if consumer == ConsumerClass::Hub && kind != SecretKind::Kubeconfig {
        return Err(PolicyError::HubNeedsKubeconfig);
    }
    Ok(())
}

/// The extra rules a minted registration answers to, on top of [`check_shape`].
pub fn check_mint(kind: SecretKind, consumer: ConsumerClass) -> Result<(), PolicyError> {
    if kind != SecretKind::Opaque {
        return Err(PolicyError::MintNeedsOpaque {
            kind: kind.as_str(),
        });
    }
    if consumer != ConsumerClass::Run {
        return Err(PolicyError::MintNeedsRunConsumer);
    }
    Ok(())
}

/// What an agent-visible value may not be, asked wherever a value is set, where a secret is bound,
/// and on the redeemed bytes at dispatch.
///
/// The predicate is [`crate::identity::api_key::contains_key`]: a key at the start of the value, or anywhere
/// inside it, since whatever wraps it (whitespace, a header, a JSON document) is wrapping the
/// holder of the value can strip.
pub fn check_agent_visible_value(
    visibility: Visibility,
    value: &SecretValue,
) -> Result<(), PolicyError> {
    if visibility == Visibility::AgentVisible
        && crate::identity::api_key::contains_key(value.expose())
    {
        return Err(PolicyError::AgentVisibleControllerKey);
    }
    Ok(())
}

/// The projection rules: a file-shaped kind is written to a path, never to an environment
/// variable, whose value would be echoed by anything that dumps the environment.
pub fn check_projection(kind: SecretKind, projection: ProjectionKind) -> Result<(), PolicyError> {
    match (kind, projection) {
        (SecretKind::Opaque | SecretKind::InferenceApiKey, _) => Ok(()),
        (_, ProjectionKind::File) => Ok(()),
        (kind, ProjectionKind::Env) => Err(PolicyError::NeedsFileProjection {
            kind: kind.as_str(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_opaque_kind_may_be_agent_visible() {
        for kind in [
            SecretKind::File,
            SecretKind::RegistryAuthfile,
            SecretKind::Kubeconfig,
            SecretKind::InferenceApiKey,
        ] {
            assert!(!kind.may_be_agent_visible());
            assert_eq!(
                check_shape(kind, Visibility::AgentVisible, kind.default_consumer()),
                Err(PolicyError::NotAgentVisible {
                    kind: kind.as_str()
                })
            );
        }
        assert!(
            check_shape(
                SecretKind::Opaque,
                Visibility::AgentVisible,
                ConsumerClass::Run
            )
            .is_ok()
        );
    }

    #[test]
    fn the_hub_consumer_class_is_kubeconfig_only() {
        assert_eq!(
            check_shape(
                SecretKind::Opaque,
                Visibility::BrokerOnly,
                ConsumerClass::Hub
            ),
            Err(PolicyError::HubNeedsKubeconfig)
        );
        assert!(
            check_shape(
                SecretKind::Kubeconfig,
                Visibility::BrokerOnly,
                ConsumerClass::Hub
            )
            .is_ok()
        );
        assert_eq!(
            SecretKind::Kubeconfig.default_consumer(),
            ConsumerClass::Hub
        );
        assert_eq!(SecretKind::Opaque.default_consumer(), ConsumerClass::Run);
    }

    #[test]
    fn a_file_kind_cannot_be_projected_as_an_env_var() {
        assert_eq!(
            check_projection(SecretKind::RegistryAuthfile, ProjectionKind::Env),
            Err(PolicyError::NeedsFileProjection {
                kind: "registry_authfile"
            })
        );
        assert!(check_projection(SecretKind::File, ProjectionKind::File).is_ok());
        assert!(check_projection(SecretKind::Opaque, ProjectionKind::Env).is_ok());
    }

    /// The whole point of the kind: a provider key reaches the loop as an environment variable the
    /// harness reads. It stays out of the sandbox because it is never agent-visible, and it cannot
    /// be minted because no minter issues one.
    #[test]
    fn an_inference_key_projects_as_an_env_var_and_is_never_minted() {
        assert!(check_projection(SecretKind::InferenceApiKey, ProjectionKind::Env).is_ok());
        assert!(check_projection(SecretKind::InferenceApiKey, ProjectionKind::File).is_ok());
        assert!(!SecretKind::InferenceApiKey.may_be_agent_visible());
        assert_eq!(
            SecretKind::InferenceApiKey.default_consumer(),
            ConsumerClass::Run
        );
        assert_eq!(
            check_mint(SecretKind::InferenceApiKey, ConsumerClass::Run),
            Err(PolicyError::MintNeedsOpaque {
                kind: "inference_api_key"
            })
        );
    }

    #[test]
    fn a_managed_path_is_the_registry_prefix_then_the_owner_then_the_name() {
        let owner = Principal::parse("group:/groups/team-x").expect("parses");
        let name = SecretName::parse("PR-Token").expect("parses");
        assert_eq!(
            managed_path(&owner, &name).expect("a path").as_str(),
            "crucible/registry/group:/groups/team-x/pr-token"
        );
        let user = Principal::parse("user:will").expect("parses");
        assert_eq!(
            managed_path(&user, &name).expect("a path").as_str(),
            "crucible/registry/user:will/pr-token"
        );
    }

    /// A registered secret must never land at the top level of the mount, which the deployment
    /// shares with other applications: a policy scoped to this controller's own subtree refuses
    /// the write, which is how the first registration was found to be writing outside it.
    #[test]
    fn a_managed_path_never_escapes_the_controllers_own_prefix() {
        let name = SecretName::parse("token").expect("parses");
        for raw in ["user:will", "group:/groups/team-x", "user:a.b-c"] {
            let owner = Principal::parse(raw).expect("parses");
            let path = managed_path(&owner, &name).expect("a path");
            assert!(
                path.as_str()
                    .starts_with(&format!("{MOUNT_PREFIX}/registry/")),
                "{raw} produced {}",
                path.as_str()
            );
        }
    }

    #[test]
    fn a_bad_secret_name_is_refused() {
        for raw in ["", "  ", "a b", "a/b", "a:b", &"x".repeat(MAX_NAME + 1)] {
            assert!(SecretName::parse(raw).is_err(), "{raw:?} must be refused");
        }
        assert_eq!(
            SecretName::parse(" GH_Token ").expect("parses").as_str(),
            "gh_token"
        );
    }

    /// A minted controller key is opaque, so the kind rules admit it and only the value check
    /// stands between it and an agent that can call the mutation surface.
    #[test]
    fn an_agent_visible_registration_refuses_a_controller_key() {
        let key = SecretValue::new(format!(
            "{}0123456789abcdef_s3cr3t",
            crate::identity::api_key::PREFIX
        ));
        assert!(
            check_shape(
                SecretKind::Opaque,
                Visibility::AgentVisible,
                ConsumerClass::Run
            )
            .is_ok()
        );
        assert_eq!(
            check_agent_visible_value(Visibility::AgentVisible, &key),
            Err(PolicyError::AgentVisibleControllerKey)
        );
        assert!(
            check_agent_visible_value(Visibility::AgentVisible, &key)
                .expect_err("refused")
                .to_string()
                .contains("controller api key")
        );
    }

    /// The bind path asks the same question of the current value, so a rotation into a key-shaped
    /// value cannot reach an agent through a binding made when the value was harmless.
    #[test]
    fn a_rotation_into_a_key_shaped_value_is_refused_at_bind() {
        let harmless = SecretValue::new("ghp_notacontrollerkey");
        assert!(check_agent_visible_value(Visibility::AgentVisible, &harmless).is_ok());
        let rotated = SecretValue::new(format!("{}abc_def", crate::identity::api_key::PREFIX));
        assert_eq!(
            check_agent_visible_value(Visibility::AgentVisible, &rotated),
            Err(PolicyError::AgentVisibleControllerKey)
        );
    }

    /// The prefix anywhere is the whole test: a `crk_` value the key guard would itself refuse,
    /// padded, malformed, or wrapped in an envelope the agent can unwrap, is still a value an agent
    /// must not see.
    #[test]
    fn a_padded_malformed_or_wrapped_controller_key_is_still_refused() {
        for raw in [
            "  crk_abc_def",
            "crk_abc_def\n",
            "crk__",
            "crk_nosecondhalf",
            "Bearer crk_abc_def",
            "{\"token\":\"crk_abc_def\"}",
            "CONTROLLER_KEY=crk_abc_def",
        ] {
            assert_eq!(
                check_agent_visible_value(Visibility::AgentVisible, &SecretValue::new(raw)),
                Err(PolicyError::AgentVisibleControllerKey),
                "{raw:?} must be refused"
            );
        }
    }

    /// A broker-only secret is never read by the agent, so the same bytes pass.
    #[test]
    fn a_non_key_opaque_value_and_a_broker_only_key_are_unaffected() {
        for raw in ["ghp_deadbeef", "hunter2", "crucible", "cr", ""] {
            assert!(
                check_agent_visible_value(Visibility::AgentVisible, &SecretValue::new(raw)).is_ok(),
                "{raw:?} must be allowed"
            );
        }
        let key = SecretValue::new(format!("{}abc_def", crate::identity::api_key::PREFIX));
        assert!(check_agent_visible_value(Visibility::BrokerOnly, &key).is_ok());
    }

    #[test]
    fn a_secret_value_never_prints_itself() {
        let value = SecretValue::new("hunter2");
        assert_eq!(format!("{value:?}"), "SecretValue(<redacted>)");
        assert!(!format!("{value:?}").contains("hunter2"));
        assert_eq!(value.expose(), "hunter2");
    }
}
