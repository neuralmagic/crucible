//! The authorization vocabulary of RFC-0003: principals, team roles, team members, membership
//! rules, and the subject set a request acts as.
//!
//! Ownership is exact. A [`Principal`] is `user:<login>`, `group:<path>`, `team:<slug>`, or
//! `run:<id>`, normalized once at parse and compared whole. The operator role's tail-segment match
//! ([`crate::identity::auth::Roles`]) is deliberately not reachable from here, so a configured
//! short group name can never widen who owns a resource.

use crate::wire_enum::wire_enum;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The slug of the team that holds platform administration (RFC-0003 C-TEAMS).
pub const PLATFORM_ADMINISTRATORS: &str = "platform-administrators";

/// The slug of the team the configured operator lists migrate into (RFC-0003 C-COMPATIBILITY).
pub const PLATFORM_OPERATORS: &str = "platform-operators";

/// The slug of the team whose members may publish drafts they own without review.
pub const PLAYBOOK_PUBLISHERS: &str = "playbook-publishers";

/// Who may own or act. Every spelling derives from something the controller verified: a login the
/// bearer guard proved, a group in validated claims, a team in the controller's own membership
/// records, or a run credential the controller minted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Principal {
    User(String),
    Group(String),
    Team(TeamSlug),
    Run(String),
}

/// Why a principal was refused. Every one of these would otherwise become part of a Vault path or
/// an ownership comparison.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PrincipalError {
    #[error(
        "a principal is spelled user:<login>, group:<path>, team:<slug>, or run:<id>, not {value:?}"
    )]
    Malformed { value: String },
    #[error("a principal needs a name after its prefix")]
    Empty,
    #[error(
        "principal {value:?} has a character outside [A-Za-z0-9._@-] (groups may also use / and :)"
    )]
    BadCharacter { value: String },
    #[error("principal {value:?} has an empty or traversing path segment")]
    BadSegment { value: String },
    #[error(transparent)]
    Slug(#[from] SlugError),
}

fn char_ok(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@')
}

impl TryFrom<String> for Principal {
    type Error = PrincipalError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Principal::parse(&raw)
    }
}

impl Principal {
    /// Parse and normalize: trimmed, lowercased, prefix required. A group keeps its full asserted
    /// spelling (`/groups/team-x`, or the `org:team` a GitHub connector asserts), because an
    /// ownership check compares whole paths.
    pub fn parse(raw: &str) -> Result<Self, PrincipalError> {
        let raw = raw.trim().to_lowercase();
        let Some((scheme, tail)) = raw.split_once(':') else {
            return Err(PrincipalError::Malformed { value: raw.clone() });
        };
        let tail = tail.trim();
        if tail.is_empty() {
            return Err(PrincipalError::Empty);
        }
        match scheme {
            "user" | "run" => {
                if !tail.chars().all(char_ok) {
                    return Err(PrincipalError::BadCharacter { value: raw.clone() });
                }
                Ok(if scheme == "user" {
                    Principal::User(tail.to_string())
                } else {
                    Principal::Run(tail.to_string())
                })
            }
            "group" => {
                if !tail.chars().all(|c| char_ok(c) || matches!(c, '/' | ':')) {
                    return Err(PrincipalError::BadCharacter { value: raw.clone() });
                }
                if tail.ends_with('/') {
                    return Err(PrincipalError::BadSegment { value: raw.clone() });
                }
                for segment in tail.trim_start_matches('/').split('/') {
                    if segment.is_empty() || segment == "." || segment == ".." {
                        return Err(PrincipalError::BadSegment { value: raw.clone() });
                    }
                }
                Ok(Principal::Group(tail.to_string()))
            }
            "team" => Ok(Principal::Team(TeamSlug::parse(tail)?)),
            _ => Err(PrincipalError::Malformed { value: raw.clone() }),
        }
    }

    pub fn user(login: &str) -> Result<Self, PrincipalError> {
        Principal::parse(&format!("user:{login}"))
    }

    pub fn team(slug: &TeamSlug) -> Self {
        Principal::Team(slug.clone())
    }

    /// The platform administrators team, which owns the deployment's own resources.
    pub fn platform() -> Self {
        Principal::Team(TeamSlug::platform_administrators())
    }

    /// The owner a stored `owner_principal` column names: the platform when the row predates
    /// ownership, so only a platform administrator may touch it.
    pub fn stored(raw: Option<&str>) -> Self {
        raw.and_then(|p| Principal::parse(p).ok())
            .unwrap_or_else(Principal::platform)
    }

    /// Whether this is a principal a resource may be owned by (RFC-0003 C-OWNERSHIP: a user or a
    /// team; a group only through the migration window).
    pub fn may_own(&self) -> bool {
        !matches!(self, Principal::Run(_))
    }

    /// The login, group path, slug, or run id without its prefix.
    pub fn name(&self) -> &str {
        match self {
            Principal::User(v) | Principal::Group(v) | Principal::Run(v) => v,
            Principal::Team(slug) => slug.as_str(),
        }
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Principal::User(v) => write!(f, "user:{v}"),
            Principal::Group(v) => write!(f, "group:{v}"),
            Principal::Team(v) => write!(f, "team:{v}"),
            Principal::Run(v) => write!(f, "run:{v}"),
        }
    }
}

impl serde::Serialize for Principal {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// A team's unique slug: `[a-z0-9][a-z0-9-]{1,62}`, lowercased and trimmed at parse.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct TeamSlug(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SlugError {
    #[error(
        "team slug {value:?} must be 2 to 63 characters of [a-z0-9-] starting with a letter or digit"
    )]
    Invalid { value: String },
}

impl TeamSlug {
    pub fn parse(raw: &str) -> Result<Self, SlugError> {
        let value = raw.trim().to_lowercase();
        let mut chars = value.chars();
        let head_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
        let len_ok = (2..=63).contains(&value.len());
        let tail_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '-');
        if head_ok && len_ok && tail_ok {
            Ok(TeamSlug(value))
        } else {
            Err(SlugError::Invalid { value })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn platform_administrators() -> TeamSlug {
        TeamSlug(PLATFORM_ADMINISTRATORS.to_string())
    }

    pub fn platform_operators() -> TeamSlug {
        TeamSlug(PLATFORM_OPERATORS.to_string())
    }

    pub fn playbook_publishers() -> TeamSlug {
        TeamSlug(PLAYBOOK_PUBLISHERS.to_string())
    }
}

impl fmt::Display for TeamSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for TeamSlug {
    type Error = SlugError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        TeamSlug::parse(&raw)
    }
}

impl<'de> serde::Deserialize<'de> for TeamSlug {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        TeamSlug::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl utoipa::PartialSchema for TeamSlug {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::SchemaType::Type(
                utoipa::openapi::Type::String,
            ))
            .pattern(Some("^[a-z0-9][a-z0-9-]{1,62}$"))
            .into()
    }
}

impl utoipa::ToSchema for TeamSlug {}

/// The role a member holds in a team, totally ordered (RFC-0003 C-TEAMS). Along a nesting path
/// the conferred role is the minimum; over paths it is the maximum.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TeamRole {
    Member,
    Maintainer,
    Owner,
}

wire_enum!(TeamRole, "team role", both, {
    TeamRole::Member => "member",
    TeamRole::Maintainer => "maintainer",
    TeamRole::Owner => "owner",
});

/// What kind of thing a team member row names.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum MemberKind {
    User,
    Group,
    Team,
    Rule,
}

wire_enum!(MemberKind, "team member kind", both, {
    MemberKind::User => "user",
    MemberKind::Group => "group",
    MemberKind::Team => "team",
    MemberKind::Rule => "rule",
});

/// A predicate over attributes the identity layer verified (RFC-0003 C-TEAMS). Spelled
/// `email-domain:<domain>`, `group-prefix:<path>`, or `group-suffix:<segment>` in the member row.
/// The suffix form is the configured operator group's tail match, kept so that list migrates.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MembershipRule {
    EmailDomain(String),
    GroupPrefix(String),
    GroupSuffix(String),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RuleError {
    #[error(
        "a membership rule is spelled email-domain:<domain>, group-prefix:<path>, or group-suffix:<segment>, not {value:?}"
    )]
    Malformed { value: String },
    #[error("membership rule {value:?} names an empty or malformed operand")]
    BadOperand { value: String },
}

impl MembershipRule {
    pub fn parse(raw: &str) -> Result<Self, RuleError> {
        let raw = raw.trim().to_lowercase();
        let Some((kind, operand)) = raw.split_once(':') else {
            return Err(RuleError::Malformed { value: raw });
        };
        let operand = operand.trim();
        match kind {
            "email-domain" => {
                let ok = !operand.is_empty()
                    && operand.contains('.')
                    && operand
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
                if ok {
                    Ok(MembershipRule::EmailDomain(operand.to_string()))
                } else {
                    Err(RuleError::BadOperand { value: raw })
                }
            }
            "group-prefix" => match Principal::parse(&format!("group:{operand}")) {
                Ok(Principal::Group(path)) => Ok(MembershipRule::GroupPrefix(path)),
                _ => Err(RuleError::BadOperand { value: raw }),
            },
            "group-suffix" => match Principal::parse(&format!("group:{operand}")) {
                Ok(Principal::Group(segment)) if !segment.contains('/') => {
                    Ok(MembershipRule::GroupSuffix(segment))
                }
                _ => Err(RuleError::BadOperand { value: raw }),
            },
            _ => Err(RuleError::Malformed { value: raw }),
        }
    }

    /// Whether the verified claims satisfy the rule. `email` and `groups` are what the identity
    /// layer proved; a caller with no proven groups passes an empty slice.
    pub fn matches(&self, email: Option<&str>, groups: &[String]) -> bool {
        match self {
            MembershipRule::EmailDomain(domain) => email
                .and_then(|e| e.rsplit_once('@'))
                .is_some_and(|(_, d)| d.eq_ignore_ascii_case(domain)),
            MembershipRule::GroupPrefix(prefix) => groups
                .iter()
                .any(|g| g == prefix || g.starts_with(&format!("{prefix}/"))),
            MembershipRule::GroupSuffix(segment) => groups
                .iter()
                .any(|g| g == segment || g.ends_with(&format!("/{segment}"))),
        }
    }
}

impl fmt::Display for MembershipRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MembershipRule::EmailDomain(d) => write!(f, "email-domain:{d}"),
            MembershipRule::GroupPrefix(p) => write!(f, "group-prefix:{p}"),
            MembershipRule::GroupSuffix(s) => write!(f, "group-suffix:{s}"),
        }
    }
}

/// One member of a team, as the API and the resolver see it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemberRef {
    User(String),
    Group(String),
    Team(TeamSlug),
    Rule(MembershipRule),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemberError {
    #[error(transparent)]
    Principal(#[from] PrincipalError),
    #[error(transparent)]
    Rule(#[from] RuleError),
    #[error(transparent)]
    Slug(#[from] SlugError),
    #[error("a {kind} member must be spelled as one, not {value:?}")]
    Mismatch { kind: &'static str, value: String },
}

impl MemberRef {
    /// Parse the stored or submitted spelling for a member kind: a login, a group path, a team
    /// slug, or a rule.
    pub fn parse(kind: MemberKind, raw: &str) -> Result<Self, MemberError> {
        match kind {
            MemberKind::User => match Principal::parse(&format!("user:{}", raw.trim()))? {
                Principal::User(login) => Ok(MemberRef::User(login)),
                _ => Err(MemberError::Mismatch {
                    kind: "user",
                    value: raw.to_string(),
                }),
            },
            MemberKind::Group => match Principal::parse(&format!("group:{}", raw.trim()))? {
                Principal::Group(path) => Ok(MemberRef::Group(path)),
                _ => Err(MemberError::Mismatch {
                    kind: "group",
                    value: raw.to_string(),
                }),
            },
            MemberKind::Team => Ok(MemberRef::Team(TeamSlug::parse(raw)?)),
            MemberKind::Rule => Ok(MemberRef::Rule(MembershipRule::parse(raw)?)),
        }
    }

    pub fn kind(&self) -> MemberKind {
        match self {
            MemberRef::User(_) => MemberKind::User,
            MemberRef::Group(_) => MemberKind::Group,
            MemberRef::Team(_) => MemberKind::Team,
            MemberRef::Rule(_) => MemberKind::Rule,
        }
    }

    /// The stored spelling: the login, path, slug, or rule without a kind prefix.
    pub fn stored(&self) -> String {
        match self {
            MemberRef::User(v) | MemberRef::Group(v) => v.clone(),
            MemberRef::Team(slug) => slug.to_string(),
            MemberRef::Rule(rule) => rule.to_string(),
        }
    }
}

/// A member at a role: the unit the create and manage-members requests carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub member: MemberRef,
    pub role: TeamRole,
}

/// How a subject holds a membership: one source, with the role it confers on its own.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Via {
    /// The subject's login is listed.
    Direct { role: TeamRole },
    /// A group the credential proves is listed.
    Group { group: String, role: TeamRole },
    /// A membership rule the verified claims satisfy.
    Rule { rule: String, role: TeamRole },
    /// Held through a nested team the subject is in.
    Team { team: TeamSlug, role: TeamRole },
}

impl Via {
    pub fn role(&self) -> TeamRole {
        match self {
            Via::Direct { role }
            | Via::Group { role, .. }
            | Via::Rule { role, .. }
            | Via::Team { role, .. } => *role,
        }
    }
}

/// A subject's standing in one team: the role (maximum over paths) and every source that holds it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct Membership {
    pub role: TeamRole,
    pub via: BTreeSet<Via>,
}

/// Everything a request acts as: the user principal the guard proved, the groups the credential
/// proves, and the teams those principals reach. This is the only thing an ownership check
/// consults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Principals {
    user: Option<Principal>,
    groups: Vec<Principal>,
    teams: BTreeMap<TeamSlug, Membership>,
}

impl Principals {
    /// Build from a validated identity and the caller's validated claims, with no team resolution.
    /// A group that is not a well-formed principal is dropped rather than refused: the claim list
    /// is the IdP's, and one unusable entry must not lock the caller out of their other groups.
    pub fn new(identity: Option<&str>, groups: &[String]) -> Self {
        let user = identity.and_then(|id| Principal::user(id).ok());
        let groups = groups
            .iter()
            .filter_map(|g| Principal::parse(&format!("group:{g}")).ok())
            .collect();
        Principals {
            user,
            groups,
            teams: BTreeMap::new(),
        }
    }

    /// The same, with the team memberships the resolver found.
    pub fn with_teams(mut self, teams: BTreeMap<TeamSlug, Membership>) -> Self {
        self.teams = teams;
        self
    }

    /// The caller's own principal, if the request named anybody.
    pub fn user(&self) -> Option<&Principal> {
        self.user.as_ref()
    }

    /// The caller's login, if any.
    pub fn login(&self) -> Option<&str> {
        match &self.user {
            Some(Principal::User(login)) => Some(login),
            _ => None,
        }
    }

    /// The group paths the credential proves.
    pub fn group_paths(&self) -> impl Iterator<Item = &str> {
        self.groups.iter().map(Principal::name)
    }

    pub fn teams(&self) -> &BTreeMap<TeamSlug, Membership> {
        &self.teams
    }

    /// The role the caller holds in `team`, if any.
    pub fn team_role(&self, team: &TeamSlug) -> Option<TeamRole> {
        self.teams.get(team).map(|m| m.role)
    }

    /// Whether the caller is an owner of the platform administrators team.
    pub fn is_platform_admin(&self) -> bool {
        self.team_role(&TeamSlug::platform_administrators())
            .is_some_and(|role| role == TeamRole::Owner)
    }

    /// Whether the caller is (or is a member of) `owner`. Exact full-path comparison. A team owner
    /// is covered at any role; what a role may do is the policy's question, not this one's.
    pub fn covers(&self, owner: &Principal) -> bool {
        match owner {
            Principal::Team(slug) => self.teams.contains_key(slug),
            Principal::Run(_) => false,
            _ => self.user.as_ref() == Some(owner) || self.groups.iter().any(|g| g == owner),
        }
    }

    /// Every principal the caller may act as, for an error body that says what would work.
    pub fn all(&self) -> Vec<Principal> {
        self.user
            .iter()
            .chain(self.groups.iter())
            .cloned()
            .chain(self.teams.keys().cloned().map(Principal::Team))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_principal_normalizes_and_keeps_the_whole_group_path() {
        let group = Principal::parse("  GROUP:/Groups/Team-X  ").expect("parses");
        assert_eq!(group, Principal::Group("/groups/team-x".to_string()));
        assert_eq!(group.to_string(), "group:/groups/team-x");
        let user = Principal::parse("User:Will").expect("parses");
        assert_eq!(user.to_string(), "user:will");
        let team = Principal::parse("Team:LLM-D").expect("parses");
        assert_eq!(team.to_string(), "team:llm-d");
        let run = Principal::parse("run:r-01ABC").expect("parses");
        assert_eq!(run.to_string(), "run:r-01abc");
    }

    #[test]
    fn a_group_keeps_the_org_and_team_a_github_connector_asserts() {
        let group = Principal::parse("group:NeuralMagic:nm_mlr").expect("parses");
        assert_eq!(group, Principal::Group("neuralmagic:nm_mlr".to_string()));
        assert_eq!(group.to_string(), "group:neuralmagic:nm_mlr");
        assert_eq!(
            Principal::parse(&group.to_string()).expect("round trips"),
            group
        );
        assert_eq!(
            MemberRef::parse(MemberKind::Group, "neuralmagic:nm_mlr").expect("a member"),
            MemberRef::Group("neuralmagic:nm_mlr".to_string())
        );
        assert_eq!(
            MembershipRule::parse("group-suffix:neuralmagic:nm_mlr").expect("a rule"),
            MembershipRule::GroupSuffix("neuralmagic:nm_mlr".to_string())
        );
        let principals = Principals::new(Some("reed"), &["neuralmagic:nm_mlr".to_string()]);
        assert_eq!(
            principals.group_paths().collect::<Vec<_>>(),
            vec!["neuralmagic:nm_mlr"],
            "the asserted group is acted as, not dropped"
        );
        for raw in ["user:a:b", "run:a:b", "team:a:b"] {
            assert!(Principal::parse(raw).is_err(), "{raw:?} must be refused");
        }
    }

    #[test]
    fn a_malformed_principal_is_refused() {
        for raw in [
            "will", "user:", "group:", "team-x", "user:a b", "user:a/b", "team:", "team:x",
            "team:-ab", "team:a_b", "run:", "svc:x",
        ] {
            assert!(Principal::parse(raw).is_err(), "{raw:?} must be refused");
        }
        for raw in ["group:/groups//team", "group:/groups/../root", "group:x/"] {
            assert!(Principal::parse(raw).is_err(), "{raw:?} must be refused");
        }
    }

    #[test]
    fn a_slug_is_two_to_sixty_three_of_the_allowed_characters() {
        assert!(TeamSlug::parse("a").is_err());
        assert!(TeamSlug::parse("ab").is_ok());
        assert!(TeamSlug::parse(&"a".repeat(63)).is_ok());
        assert!(TeamSlug::parse(&"a".repeat(64)).is_err());
        assert!(TeamSlug::parse("-ab").is_err());
        assert!(TeamSlug::parse("a-b-9").is_ok());
        assert_eq!(
            TeamSlug::parse(" LLM-D ").expect("parses").as_str(),
            "llm-d"
        );
    }

    /// The whole point of storing full paths: a configured short name that grants the operator role
    /// must not also grant ownership of `group:/groups/team-x`.
    #[test]
    fn ownership_never_matches_a_tail_segment() {
        let caller = Principals::new(Some("alice"), &["/groups/team-x".to_string()]);
        assert!(caller.covers(&Principal::parse("group:/groups/team-x").expect("parses")));
        assert!(!caller.covers(&Principal::parse("group:team-x").expect("parses")));
        assert!(caller.covers(&Principal::parse("user:alice").expect("parses")));
        assert!(!caller.covers(&Principal::parse("user:bob").expect("parses")));
    }

    #[test]
    fn an_anonymous_caller_covers_nothing() {
        let caller = Principals::new(None, &[]);
        assert!(caller.user().is_none());
        assert!(!caller.covers(&Principal::parse("user:alice").expect("parses")));
        assert!(!caller.covers(&Principal::parse("team:llm-d").expect("parses")));
    }

    #[test]
    fn an_unparseable_claim_does_not_drop_the_others() {
        let caller = Principals::new(
            Some("alice"),
            &["not a group".to_string(), "ok".to_string()],
        );
        assert!(caller.covers(&Principal::parse("group:ok").expect("parses")));
    }

    #[test]
    fn a_team_is_covered_at_any_role_and_a_run_never() {
        let slug = TeamSlug::parse("llm-d").expect("slug");
        let caller = Principals::new(Some("alice"), &[]).with_teams(BTreeMap::from([(
            slug.clone(),
            Membership {
                role: TeamRole::Member,
                via: BTreeSet::from([Via::Direct {
                    role: TeamRole::Member,
                }]),
            },
        )]));
        assert!(caller.covers(&Principal::Team(slug.clone())));
        assert_eq!(caller.team_role(&slug), Some(TeamRole::Member));
        assert!(!caller.covers(&Principal::Run("r1".into())));
        assert!(!caller.is_platform_admin());
        assert_eq!(caller.all().len(), 2);
    }

    #[test]
    fn team_roles_order_member_below_maintainer_below_owner() {
        assert!(TeamRole::Member < TeamRole::Maintainer);
        assert!(TeamRole::Maintainer < TeamRole::Owner);
        assert_eq!(TeamRole::parse("owner").expect("parses"), TeamRole::Owner);
        assert!(TeamRole::parse("admin").is_err());
    }

    #[test]
    fn rules_parse_normalize_and_match_verified_claims() {
        let domain = MembershipRule::parse("Email-Domain:Example.COM").expect("parses");
        assert_eq!(domain.to_string(), "email-domain:example.com");
        assert!(domain.matches(Some("Alice@example.com"), &[]));
        assert!(!domain.matches(Some("alice@example.org"), &[]));
        assert!(!domain.matches(None, &[]));

        let prefix = MembershipRule::parse("group-prefix:/Groups/LLM-D").expect("parses");
        assert_eq!(prefix.to_string(), "group-prefix:/groups/llm-d");
        assert!(prefix.matches(None, &["/groups/llm-d".to_string()]));
        assert!(prefix.matches(None, &["/groups/llm-d/devs".to_string()]));
        assert!(!prefix.matches(None, &["/groups/llm-dx".to_string()]));

        let suffix = MembershipRule::parse("group-suffix:Team-X").expect("parses");
        assert_eq!(suffix.to_string(), "group-suffix:team-x");
        assert!(suffix.matches(None, &["/groups/team-x".to_string()]));
        assert!(suffix.matches(None, &["team-x".to_string()]));
        assert!(!suffix.matches(None, &["/groups/team-xy".to_string()]));

        for raw in [
            "email-domain:",
            "email-domain:nodot",
            "group-prefix:",
            "group-prefix:a/",
            "group-suffix:/a/b",
            "suffix:x",
            "nocolon",
        ] {
            assert!(
                MembershipRule::parse(raw).is_err(),
                "{raw:?} must be refused"
            );
        }
    }

    #[test]
    fn a_member_ref_parses_per_kind() {
        assert_eq!(
            MemberRef::parse(MemberKind::User, " Alice ").expect("parses"),
            MemberRef::User("alice".into())
        );
        assert_eq!(
            MemberRef::parse(MemberKind::Group, "/Groups/X").expect("parses"),
            MemberRef::Group("/groups/x".into())
        );
        assert_eq!(
            MemberRef::parse(MemberKind::Team, "llm-d")
                .expect("parses")
                .stored(),
            "llm-d"
        );
        assert_eq!(
            MemberRef::parse(MemberKind::Rule, "email-domain:x.io")
                .expect("parses")
                .kind(),
            MemberKind::Rule
        );
        assert!(MemberRef::parse(MemberKind::User, "a/b").is_err());
        assert!(MemberRef::parse(MemberKind::Team, "_").is_err());
    }
}
