//! The action vocabulary of RFC-0003 C-ACTIONS: a fixed `<resource>:<verb>` identifier per
//! operation, served at `GET /api/authz/actions` and rendered into the Cedar schema.

use crate::wire_enum::wire_enum;
use std::fmt;

/// A governed resource type. Every type defines the six base verbs; the per-type extras follow
/// the RFC's list.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    /// The deployment itself: configuration, the autopilot switch, reconcile, images, clusters.
    Platform,
    Issue,
    Repo,
    Build,
    Run,
    Playbook,
    PlaybookDraft,
    PackImport,
    Scope,
    StandingLaunch,
    OneShot,
    Secret,
    ModelProvider,
    DispatchTarget,
    ApiKey,
    UserPrefs,
    PolicySet,
    Team,
}

wire_enum!(ResourceType, "resource type", both, {
    ResourceType::Platform => "platform",
    ResourceType::Issue => "issue",
    ResourceType::Repo => "repo",
    ResourceType::Build => "build",
    ResourceType::Run => "run",
    ResourceType::Playbook => "playbook",
    ResourceType::PlaybookDraft => "playbook_draft",
    ResourceType::PackImport => "pack_import",
    ResourceType::Scope => "scope",
    ResourceType::StandingLaunch => "standing_launch",
    ResourceType::OneShot => "one_shot",
    ResourceType::Secret => "secret",
    ResourceType::ModelProvider => "model_provider",
    ResourceType::DispatchTarget => "dispatch_target",
    ResourceType::ApiKey => "api_key",
    ResourceType::UserPrefs => "user_prefs",
    ResourceType::PolicySet => "policy_set",
    ResourceType::Team => "team",
});

impl ResourceType {
    pub const ALL: [ResourceType; 18] = [
        ResourceType::Platform,
        ResourceType::Issue,
        ResourceType::Repo,
        ResourceType::Build,
        ResourceType::Run,
        ResourceType::Playbook,
        ResourceType::PlaybookDraft,
        ResourceType::PackImport,
        ResourceType::Scope,
        ResourceType::StandingLaunch,
        ResourceType::OneShot,
        ResourceType::Secret,
        ResourceType::ModelProvider,
        ResourceType::DispatchTarget,
        ResourceType::ApiKey,
        ResourceType::UserPrefs,
        ResourceType::PolicySet,
        ResourceType::Team,
    ];

    /// The Cedar entity type name for the resource.
    pub fn entity_type(self) -> &'static str {
        match self {
            ResourceType::Platform => "Platform",
            ResourceType::Issue => "Issue",
            ResourceType::Repo => "Repo",
            ResourceType::Build => "Build",
            ResourceType::Run => "Run",
            ResourceType::Playbook => "Playbook",
            ResourceType::PlaybookDraft => "PlaybookDraft",
            ResourceType::PackImport => "PackImport",
            ResourceType::Scope => "Scope",
            ResourceType::StandingLaunch => "StandingLaunch",
            ResourceType::OneShot => "OneShot",
            ResourceType::Secret => "Secret",
            ResourceType::ModelProvider => "ModelProvider",
            ResourceType::DispatchTarget => "DispatchTarget",
            ResourceType::ApiKey => "ApiKey",
            ResourceType::UserPrefs => "UserPrefs",
            ResourceType::PolicySet => "PolicySet",
            ResourceType::Team => "Team",
        }
    }

    /// The verbs the type defines: the six every type has, then the RFC's per-type additions.
    pub fn verbs(self) -> Vec<Verb> {
        let mut verbs = vec![
            Verb::Read,
            Verb::Create,
            Verb::Update,
            Verb::Delete,
            Verb::Transfer,
            Verb::Share,
        ];
        let extra: &[Verb] = match self {
            ResourceType::Issue => &[Verb::Launch, Verb::Approve],
            ResourceType::Playbook | ResourceType::StandingLaunch => &[Verb::Launch],
            ResourceType::PlaybookDraft => &[Verb::Launch, Verb::Approve, Verb::Publish],
            ResourceType::PackImport | ResourceType::Scope => &[Verb::Approve],
            ResourceType::Secret => &[Verb::Bind, Verb::Rotate],
            ResourceType::DispatchTarget => &[Verb::Dispatch],
            ResourceType::Run => &[Verb::Publish],
            ResourceType::PolicySet => &[Verb::Activate],
            ResourceType::Team => &[Verb::ManageMembers],
            _ => &[],
        };
        verbs.extend_from_slice(extra);
        verbs
    }

    pub fn defines(self, verb: Verb) -> bool {
        self.verbs().contains(&verb)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, utoipa::ToSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Verb {
    Read,
    Create,
    Update,
    Delete,
    Transfer,
    Share,
    Launch,
    Approve,
    Bind,
    Rotate,
    Dispatch,
    Publish,
    Activate,
    ManageMembers,
}

wire_enum!(Verb, "action verb", both, {
    Verb::Read => "read",
    Verb::Create => "create",
    Verb::Update => "update",
    Verb::Delete => "delete",
    Verb::Transfer => "transfer",
    Verb::Share => "share",
    Verb::Launch => "launch",
    Verb::Approve => "approve",
    Verb::Bind => "bind",
    Verb::Rotate => "rotate",
    Verb::Dispatch => "dispatch",
    Verb::Publish => "publish",
    Verb::Activate => "activate",
    Verb::ManageMembers => "manage-members",
});

/// One entry of the vocabulary: a verb the resource type defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Action {
    pub resource: ResourceType,
    pub verb: Verb,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ActionError {
    #[error("an action is spelled <resource>:<verb>, not {value:?}")]
    Malformed { value: String },
    #[error("{0}")]
    Unknown(String),
    #[error("resource type {resource} does not define {verb}")]
    Undefined { resource: String, verb: String },
}

impl Action {
    /// The pair, refused when the type does not define the verb.
    pub fn new(resource: ResourceType, verb: Verb) -> Result<Self, ActionError> {
        if resource.defines(verb) {
            Ok(Action { resource, verb })
        } else {
            Err(ActionError::Undefined {
                resource: resource.as_str().to_string(),
                verb: verb.as_str().to_string(),
            })
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ActionError> {
        let Some((resource, verb)) = raw.split_once(':') else {
            return Err(ActionError::Malformed {
                value: raw.to_string(),
            });
        };
        let resource =
            ResourceType::parse(resource).map_err(|e| ActionError::Unknown(e.to_string()))?;
        let verb = Verb::parse(verb).map_err(|e| ActionError::Unknown(e.to_string()))?;
        Action::new(resource, verb)
    }

    /// Every action in the vocabulary, in resource then verb order.
    pub fn all() -> Vec<Action> {
        ResourceType::ALL
            .iter()
            .flat_map(|&resource| {
                resource
                    .verbs()
                    .into_iter()
                    .map(move |verb| Action { resource, verb })
            })
            .collect()
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.resource.as_str(), self.verb.as_str())
    }
}

impl serde::Serialize for Action {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_defines_the_six_base_verbs_and_the_rfc_extras() {
        for resource in ResourceType::ALL {
            for verb in [
                Verb::Read,
                Verb::Create,
                Verb::Update,
                Verb::Delete,
                Verb::Transfer,
                Verb::Share,
            ] {
                assert!(resource.defines(verb), "{resource:?} lacks {verb:?}");
            }
        }
        assert!(ResourceType::Playbook.defines(Verb::Launch));
        assert!(ResourceType::PlaybookDraft.defines(Verb::Launch));
        assert!(ResourceType::PlaybookDraft.defines(Verb::Approve));
        assert!(ResourceType::StandingLaunch.defines(Verb::Launch));
        assert!(ResourceType::PackImport.defines(Verb::Approve));
        assert!(ResourceType::Scope.defines(Verb::Approve));
        assert!(ResourceType::Secret.defines(Verb::Bind));
        assert!(ResourceType::Secret.defines(Verb::Rotate));
        assert!(ResourceType::DispatchTarget.defines(Verb::Dispatch));
        assert!(ResourceType::Run.defines(Verb::Publish));
        assert!(ResourceType::PolicySet.defines(Verb::Activate));
        assert!(ResourceType::Team.defines(Verb::ManageMembers));
        assert!(!ResourceType::Secret.defines(Verb::Launch));
    }

    #[test]
    fn actions_render_and_parse_as_resource_colon_verb() {
        let a = Action::parse("team:manage-members").expect("parses");
        assert_eq!(a.resource, ResourceType::Team);
        assert_eq!(a.verb, Verb::ManageMembers);
        assert_eq!(a.to_string(), "team:manage-members");
        assert_eq!(
            Action::parse("secret:launch"),
            Err(ActionError::Undefined {
                resource: "secret".into(),
                verb: "launch".into()
            })
        );
        assert!(matches!(
            Action::parse("nope:read"),
            Err(ActionError::Unknown(_))
        ));
        assert!(matches!(
            Action::parse("read"),
            Err(ActionError::Malformed { .. })
        ));
        let all = Action::all();
        assert_eq!(all.len(), 18 * 6 + 15);
        let mut sorted = all.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
    }
}
