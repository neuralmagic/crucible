//! The inputs and the answer of the one decision (RFC-0003 C-DECISION): who is asking, on what,
//! and the rules that decided.

use crate::authz::action::ResourceType;
use crate::authz::model::{PLATFORM_ADMINISTRATORS, Principal, Principals, TeamRole, TeamSlug};
use crate::wire_enum::wire_enum;
use std::collections::BTreeMap;

/// The subject: the principal evaluated, with a tag per principal it acts as (`user:alice` and
/// `group:/x` at `owner`, `team:<slug>` at the resolved role).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub principal: Principal,
    pub tags: BTreeMap<String, String>,
    pub platform_admin: bool,
    pub proves_groups: bool,
}

impl Subject {
    pub fn user(
        login: &str,
        platform_admin: bool,
        proves_groups: bool,
        tags: BTreeMap<String, String>,
    ) -> Self {
        let mut tags = tags;
        tags.insert(
            format!("user:{login}"),
            TeamRole::Owner.as_str().to_string(),
        );
        Subject {
            principal: Principal::User(login.to_string()),
            tags,
            platform_admin,
            proves_groups,
        }
    }

    /// A run principal: no tags, no groups, nothing but its own id.
    pub fn run(id: &str) -> Self {
        Subject {
            principal: Principal::Run(id.to_string()),
            tags: BTreeMap::new(),
            platform_admin: false,
            proves_groups: false,
        }
    }

    /// A team firing a standing launch: the team principal at `maintainer`.
    pub fn team_firing(slug: &TeamSlug) -> Self {
        Subject {
            principal: Principal::Team(slug.clone()),
            tags: BTreeMap::from([(
                format!("team:{slug}"),
                TeamRole::Maintainer.as_str().to_string(),
            )]),
            platform_admin: false,
            proves_groups: true,
        }
    }

    /// The subject a resolved caller acts as, or `None` for an anonymous request.
    pub fn of(principals: &Principals, proves_groups: bool) -> Option<Self> {
        let login = principals.login()?;
        let mut tags = BTreeMap::new();
        for group in principals.group_paths() {
            tags.insert(
                format!("group:{group}"),
                TeamRole::Owner.as_str().to_string(),
            );
        }
        for (team, membership) in principals.teams() {
            tags.insert(format!("team:{team}"), membership.role.as_str().to_string());
        }
        Some(Subject::user(
            login,
            principals.is_platform_admin(),
            proves_groups,
            tags,
        ))
    }

    /// A user holding one role in one team, for the load-time probes.
    pub(crate) fn probe_member(team: &TeamSlug, role: TeamRole) -> Self {
        Subject::user(
            "probe",
            false,
            true,
            BTreeMap::from([(format!("team:{team}"), role.as_str().to_string())]),
        )
    }

    /// A platform administrator, for the load-time probes.
    pub(crate) fn probe_platform_admin() -> Self {
        Subject::user(
            "probe",
            true,
            true,
            BTreeMap::from([(
                format!("team:{PLATFORM_ADMINISTRATORS}"),
                TeamRole::Owner.as_str().to_string(),
            )]),
        )
    }

    pub(crate) fn entity_type(&self) -> &'static str {
        match self.principal {
            Principal::User(_) | Principal::Group(_) => "UserPrincipal",
            Principal::Team(_) => "TeamPrincipal",
            Principal::Run(_) => "RunPrincipal",
        }
    }
}

/// A share role (RFC-0003 C-SHARING).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ShareRole {
    Viewer,
    Launcher,
    Editor,
}

wire_enum!(ShareRole, "share role", both, {
    ShareRole::Viewer => "viewer",
    ShareRole::Launcher => "launcher",
    ShareRole::Editor => "editor",
});

/// The resource a decision is about: its type, identifier, owner, and what the subject holds on
/// it through a share or a run credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub rtype: ResourceType,
    pub id: String,
    pub owner: Principal,
    /// The best share role the subject holds on the resource or an ancestor, already filtered for
    /// expiry at decision time.
    pub share: Option<ShareRole>,
    /// The run id this resource belongs to, when it is a run or a child of one.
    pub run: Option<String>,
}

impl Resource {
    pub fn new(rtype: ResourceType, id: impl Into<String>, owner: Principal) -> Self {
        Resource {
            rtype,
            id: id.into(),
            owner,
            share: None,
            run: None,
        }
    }

    /// A resource the platform administrators team owns.
    pub fn platform(rtype: ResourceType, id: impl Into<String>) -> Self {
        Resource::new(rtype, id, Principal::platform())
    }

    pub fn owner_kind(&self) -> &'static str {
        match self.owner {
            Principal::User(_) => "user",
            Principal::Group(_) => "group",
            Principal::Team(_) => "team",
            Principal::Run(_) => "run",
        }
    }

    /// The role the subject holds in the owner: `owner` for its own user or a group it proves,
    /// the team role for a team.
    pub fn owner_role(&self, subject: &Subject) -> Option<TeamRole> {
        match &self.owner {
            Principal::User(login) => {
                (subject.principal == Principal::User(login.clone())).then_some(TeamRole::Owner)
            }
            Principal::Group(_) | Principal::Team(_) => subject
                .tags
                .get(&self.owner.to_string())
                .and_then(|role| TeamRole::parse(role).ok()),
            Principal::Run(_) => None,
        }
    }
}

/// The answer: allow or deny, and the policy ids that decided.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct Decision {
    pub allowed: bool,
    pub rules: Vec<String>,
}

impl Decision {
    pub fn denied(rule: impl Into<String>) -> Self {
        Decision {
            allowed: false,
            rules: vec![rule.into()],
        }
    }

    /// The structured reason: the deciding rules joined, or `no-rule`.
    pub fn reason(&self) -> String {
        if self.rules.is_empty() {
            "no-rule".to_string()
        } else {
            self.rules.join(",")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::model::{Membership, Via};
    use std::collections::BTreeSet;

    #[test]
    fn a_subject_carries_a_tag_per_principal_it_acts_as() {
        let slug = TeamSlug::parse("llm-d").expect("slug");
        let principals = Principals::new(Some("alice"), &["/groups/x".to_string()]).with_teams(
            BTreeMap::from([(
                slug.clone(),
                Membership {
                    role: TeamRole::Maintainer,
                    via: BTreeSet::from([Via::Direct {
                        role: TeamRole::Maintainer,
                    }]),
                },
            )]),
        );
        let subject = Subject::of(&principals, true).expect("named");
        assert_eq!(subject.principal, Principal::User("alice".into()));
        assert_eq!(subject.tags["user:alice"], "owner");
        assert_eq!(subject.tags["group:/groups/x"], "owner");
        assert_eq!(subject.tags["team:llm-d"], "maintainer");
        assert!(!subject.platform_admin);
        assert!(Subject::of(&Principals::new(None, &[]), true).is_none());

        let mine = Resource::new(ResourceType::Secret, "s", Principal::User("alice".into()));
        assert_eq!(mine.owner_role(&subject), Some(TeamRole::Owner));
        let theirs = Resource::new(ResourceType::Secret, "s", Principal::User("bob".into()));
        assert_eq!(theirs.owner_role(&subject), None);
        let team = Resource::new(ResourceType::Secret, "s", Principal::Team(slug));
        assert_eq!(team.owner_role(&subject), Some(TeamRole::Maintainer));
        let group = Resource::new(
            ResourceType::Secret,
            "s",
            Principal::Group("/groups/x".into()),
        );
        assert_eq!(group.owner_role(&subject), Some(TeamRole::Owner));
        assert_eq!(group.owner_kind(), "group");
        assert_eq!(
            Resource::platform(ResourceType::Platform, "p")
                .owner
                .to_string(),
            "team:platform-administrators"
        );
    }

    #[test]
    fn a_decision_reason_names_the_rules_or_no_rule() {
        assert_eq!(Decision::denied("x").reason(), "x");
        assert_eq!(
            Decision {
                allowed: true,
                rules: vec!["a".into(), "b".into()]
            }
            .reason(),
            "a,b"
        );
        assert_eq!(
            Decision {
                allowed: false,
                rules: vec![]
            }
            .reason(),
            "no-rule"
        );
    }
}

/// A refusal in the shape the SPA renders verbatim: the sentence, and the rule that decided.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct DenialBody {
    pub error: String,
    pub rule: String,
}
