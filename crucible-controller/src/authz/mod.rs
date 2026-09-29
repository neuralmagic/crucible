//! Authorization (RFC-0003): principals, teams, and the subject set a request acts as.
//!
//! [`Caller`] is the extractor every handler that decides on ownership or membership takes: the
//! identity the guard proved, the groups the credential proves, and the teams those reach, resolved
//! from the controller's own membership records on every request.

pub mod action;
pub(crate) mod api;
pub mod bindings;
pub mod bootstrap;
pub mod decision;
pub mod entitlement;
pub mod explain;
pub mod firing;
pub mod granted;
pub mod guard;
pub mod impersonation;
pub mod model;
pub mod owner;
pub mod policy;
pub mod resolve;
pub mod shares;
pub mod store;
pub mod transfer;

use crate::api::state::ApiState;
use crate::authz::action::Action;
use crate::authz::decision::Subject;
use crate::authz::model::{Membership, Principal, Principals, TeamRole, TeamSlug};
use crate::authz::store::AuditEvent;
use crate::identity::auth::AuthPath;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

/// The request header naming the team a caller acts as (RFC-0003 C-ACT-AS), spelled `team:<slug>`.
pub const ACT_AS_HEADER: &str = "x-crucible-act-as";

/// The request's subject set and the credential path that proved it.
#[derive(Debug, Clone)]
pub struct Caller {
    pub principals: Principals,
    pub path: AuthPath,
    /// Set while the caller acts as a team; `principals` then holds that team alone.
    pub acting_as: Option<ActingAs>,
}

/// A caller acting as a team: the team, the role the subject holds in it, and the caller's own
/// resolution, which stays the actor.
#[derive(Debug, Clone)]
pub struct ActingAs {
    pub team: TeamSlug,
    pub role: TeamRole,
    pub actor: Principals,
}

/// Why a request may not act as the team it named.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActAsRefusal {
    #[error("only a team may be acted as, not {0:?}")]
    NotATeam(String),
    #[error("you are not a member of team:{0}")]
    NotHeld(TeamSlug),
}

impl IntoResponse for ActAsRefusal {
    fn into_response(self) -> Response {
        let mut response = crate::api::dto::forbidden(self.to_string());
        response.headers_mut().insert(
            ACT_AS_HEADER,
            axum::http::HeaderValue::from_static("refused"),
        );
        response
    }
}

impl Caller {
    /// The actor: the user principal that caused the request, if the request named anybody.
    pub fn actor(&self) -> Option<&Principal> {
        self.actor_principals().user()
    }

    /// Everything the actor holds in their own right, whatever they act as on this request.
    pub fn actor_principals(&self) -> &Principals {
        match &self.acting_as {
            Some(acting) => &acting.actor,
            None => &self.principals,
        }
    }

    /// The principal the decision evaluates: the team acted as, else the caller's own user.
    pub fn subject_principal(&self) -> Option<Principal> {
        match &self.acting_as {
            Some(acting) => Some(Principal::Team(acting.team.clone())),
            None => self.principals.user().cloned(),
        }
    }

    /// The subject spelled for an audit row when it differs from the actor: the team acted as.
    pub fn subject_label(&self) -> Option<String> {
        self.acting_as
            .as_ref()
            .map(|acting| Principal::Team(acting.team.clone()).to_string())
    }

    /// The subject the policy decides for, or `None` for an anonymous request.
    pub fn subject(&self) -> Option<Subject> {
        match &self.acting_as {
            Some(acting) => Some(Subject::team(
                &acting.team,
                acting.role,
                self.proves_groups(),
            )),
            None => Subject::of(&self.principals, self.proves_groups()),
        }
    }

    /// This caller acting as the principal `asked` names: that team alone, at the lesser of the
    /// caller's role in it and `maintainer`.
    pub fn act_as(self, asked: &str) -> Result<Caller, ActAsRefusal> {
        let team = match Principal::parse(asked.trim()) {
            Ok(Principal::Team(team)) => team,
            _ => return Err(ActAsRefusal::NotATeam(asked.to_string())),
        };
        let Some(held) = self.principals.teams().get(&team) else {
            return Err(ActAsRefusal::NotHeld(team));
        };
        let role = held.role.min(TeamRole::Maintainer);
        let membership = Membership {
            role,
            via: held.via.clone(),
        };
        let principals =
            Principals::new(None, &[]).with_teams(std::collections::BTreeMap::from([(
                team.clone(),
                membership,
            )]));
        let actor = match self.acting_as {
            Some(acting) => acting.actor,
            None => self.principals,
        };
        Ok(Caller {
            principals,
            path: self.path,
            acting_as: Some(ActingAs { team, role, actor }),
        })
    }

    /// Whether the groups this caller carries are proven. The open guard forwards whatever the
    /// loopback caller asserted, exactly as the role guards read it.
    pub fn proves_groups(&self) -> bool {
        self.path == AuthPath::Open || self.path.carries_groups()
    }

    /// The coarse role the admin- and operator-gated routes and `whoami` read: an owner of the
    /// platform administrators team is an admin, any other member of it or of the platform
    /// operators team an operator. A downgraded session holds no role.
    pub fn role(&self) -> crate::identity::auth::Role {
        use crate::identity::auth::Role;
        if !self.path.holds_roles() {
            return Role::Viewer;
        }
        if self.principals.is_platform_admin() {
            Role::Admin
        } else if self
            .principals
            .team_role(&model::TeamSlug::platform_administrators())
            .or_else(|| {
                self.principals
                    .team_role(&model::TeamSlug::platform_operators())
            })
            .is_some()
        {
            Role::Operator
        } else {
            Role::Viewer
        }
    }

    /// The actor spelled for an audit row: the caller's principal, or `anonymous`.
    pub fn actor_label(&self) -> String {
        self.actor()
            .map(Principal::to_string)
            .unwrap_or_else(|| "anonymous".to_string())
    }

    /// An audit row for `action` on `id` by this caller, with `subject`, `prior` and `result`
    /// left for the caller to fill.
    pub fn audit_event(
        &self,
        action: Action,
        id: impl Into<String>,
        allowed: bool,
        rule: impl Into<String>,
    ) -> AuditEvent {
        AuditEvent {
            actor: self.actor_label(),
            subject: self.subject_label(),
            auth_path: self.path.as_str().to_string(),
            action: action.to_string(),
            resource_type: action.resource.as_str().to_string(),
            resource_id: id.into(),
            allowed,
            rule: rule.into(),
            prior: None,
            result: None,
        }
    }

    /// The same caller with memberships re-read, after a write that changed them. A caller acting
    /// as a team keeps acting as it while it still holds the team.
    pub async fn refreshed(
        &self,
        pool: &sqlx::PgPool,
        roles: &crate::identity::auth::Roles,
    ) -> anyhow::Result<Caller> {
        let actor = self.actor_principals();
        let groups: Vec<String> = actor.group_paths().map(str::to_string).collect();
        let fresh = resolve_caller(pool, roles, actor.login(), &groups, self.path).await?;
        match &self.acting_as {
            Some(acting) => fresh
                .act_as(&Principal::Team(acting.team.clone()).to_string())
                .map_err(anyhow::Error::from),
            None => Ok(fresh),
        }
    }
}

impl FromRequestParts<ApiState> for Caller {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(caller) = parts.extensions.get::<Caller>() {
            return Ok(caller.clone());
        }
        let Ok(identity) =
            crate::identity::session::Identity::from_request_parts(parts, state).await;
        let Ok(groups) = crate::identity::auth::Groups::from_request_parts(parts, state).await;
        let Ok(path) = AuthPath::from_request_parts(parts, state).await;
        let caller = resolve_caller(
            state.db.pool(),
            &state.roles,
            identity.as_deref(),
            &groups.0,
            path,
        )
        .await
        .map_err(|e| crate::api::state::AppError::from(e).into_response())?;
        let asked = parts
            .headers
            .get(ACT_AS_HEADER)
            .map(|v| v.to_str().map(str::trim).unwrap_or_default().to_string());
        let caller = match asked {
            None => caller,
            Some(asked) => caller.act_as(&asked).map_err(IntoResponse::into_response)?,
        };
        parts.extensions.insert(caller.clone());
        Ok(caller)
    }
}

/// Resolve what a proven identity acts as. The open guard forwards whatever the loopback caller
/// asserted, exactly as the role guards read it; every other path proves groups only where the
/// credential carried a claim.
pub async fn resolve_caller(
    pool: &sqlx::PgPool,
    roles: &crate::identity::auth::Roles,
    identity: Option<&str>,
    groups: &[String],
    path: AuthPath,
) -> anyhow::Result<Caller> {
    let proves_groups = path == AuthPath::Open || path.carries_groups();
    let login = identity.map(str::trim).map(str::to_lowercase);
    let groups: &[String] = if proves_groups { groups } else { &[] };
    let mut teams = resolve::teams_for(pool, login.as_deref(), groups, proves_groups).await?;
    if path.holds_roles() {
        let identity = crate::identity::session::Identity(login.clone());
        let groups = crate::identity::auth::Groups(groups.to_vec());
        let configured = match roles.role(&identity, &groups) {
            crate::identity::auth::Role::Admin => Some((
                model::TeamSlug::platform_administrators(),
                model::TeamRole::Owner,
                "configured-admins",
            )),
            crate::identity::auth::Role::Operator => Some((
                model::TeamSlug::platform_operators(),
                model::TeamRole::Member,
                "configured-operators",
            )),
            crate::identity::auth::Role::Viewer => None,
        };
        if let Some((slug, role, rule)) = configured {
            let via = model::Via::Rule {
                rule: rule.to_string(),
                role,
            };
            let entry = teams.entry(slug).or_insert_with(|| model::Membership {
                role,
                via: Default::default(),
            });
            entry.role = entry.role.max(role);
            entry.via.insert(via);
        }
    }
    let principals = Principals::new(login.as_deref(), groups).with_teams(teams);
    Ok(Caller {
        principals,
        path,
        acting_as: None,
    })
}

#[cfg(test)]
mod tests {
    use crate::authz::decision::Subject;
    use crate::authz::model::{Membership, Principal, Principals, TeamRole, TeamSlug, Via};
    use crate::authz::{ActAsRefusal, Caller};
    use crate::identity::auth::{AuthPath, Role};
    use std::collections::BTreeMap;

    fn caller(teams: &[(TeamSlug, TeamRole)], path: AuthPath) -> Caller {
        let teams: BTreeMap<TeamSlug, Membership> = teams
            .iter()
            .map(|(slug, role)| {
                (
                    slug.clone(),
                    Membership {
                        role: *role,
                        via: [Via::Direct { role: *role }].into(),
                    },
                )
            })
            .collect();
        Caller {
            principals: Principals::new(Some("reed"), &[]).with_teams(teams),
            path,
            acting_as: None,
        }
    }

    #[test]
    fn a_platform_administrators_owner_is_an_admin_however_the_membership_was_granted() {
        let admins = [(TeamSlug::platform_administrators(), TeamRole::Owner)];
        assert_eq!(caller(&admins, AuthPath::Session).role(), Role::Admin);
        assert_eq!(caller(&admins, AuthPath::ApiKey).role(), Role::Admin);
    }

    #[test]
    fn any_lesser_platform_membership_is_an_operator() {
        for (team, role) in [
            (TeamSlug::platform_administrators(), TeamRole::Maintainer),
            (TeamSlug::platform_administrators(), TeamRole::Member),
            (TeamSlug::platform_operators(), TeamRole::Member),
            (TeamSlug::platform_operators(), TeamRole::Owner),
        ] {
            assert_eq!(
                caller(&[(team.clone(), role)], AuthPath::Session).role(),
                Role::Operator,
                "{team} at {role:?}"
            );
        }
    }

    #[test]
    fn membership_in_other_teams_holds_no_role() {
        let team = TeamSlug::parse("nm-mlr").unwrap();
        assert_eq!(
            caller(&[(team, TeamRole::Owner)], AuthPath::Session).role(),
            Role::Viewer
        );
        assert_eq!(caller(&[], AuthPath::Session).role(), Role::Viewer);
    }

    #[test]
    fn a_downgraded_session_holds_no_role_even_as_a_platform_owner() {
        let admins = [(TeamSlug::platform_administrators(), TeamRole::Owner)];
        assert_eq!(
            caller(&admins, AuthPath::DowngradedSession).role(),
            Role::Viewer
        );
    }

    fn slug(s: &str) -> TeamSlug {
        TeamSlug::parse(s).expect("slug")
    }

    #[test]
    fn acting_as_a_team_leaves_that_team_alone_capped_at_maintainer() {
        for (held, acts) in [
            (TeamRole::Owner, TeamRole::Maintainer),
            (TeamRole::Maintainer, TeamRole::Maintainer),
            (TeamRole::Member, TeamRole::Member),
        ] {
            let reed = caller(
                &[(slug("llm-d"), held), (slug("core"), TeamRole::Owner)],
                AuthPath::Session,
            );
            let acting = reed.act_as("team:llm-d").expect("held");
            assert_eq!(acting.principals.user(), None);
            assert_eq!(acting.principals.group_paths().count(), 0);
            assert_eq!(
                acting
                    .principals
                    .teams()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
                [slug("llm-d")]
            );
            assert_eq!(acting.principals.team_role(&slug("llm-d")), Some(acts));
            assert_eq!(
                acting.subject(),
                Some(Subject::team(&slug("llm-d"), acts, true))
            );
            assert_eq!(
                acting.subject_principal(),
                Some(Principal::Team(slug("llm-d")))
            );
            assert_eq!(acting.actor(), Some(&Principal::User("reed".into())));
            assert_eq!(acting.actor_principals().teams().len(), 2);
            assert_eq!(acting.path, AuthPath::Session);
        }
    }

    #[test]
    fn acting_again_keeps_the_original_actor() {
        let reed = caller(&[(slug("llm-d"), TeamRole::Owner)], AuthPath::Session);
        let twice = reed
            .act_as("team:llm-d")
            .expect("held")
            .act_as("team:llm-d")
            .expect("still held");
        assert_eq!(twice.actor(), Some(&Principal::User("reed".into())));
        assert_eq!(twice.actor_principals().teams().len(), 1);
    }

    #[test]
    fn only_a_held_team_may_be_acted_as() {
        let reed = caller(&[(slug("llm-d"), TeamRole::Owner)], AuthPath::Session);
        assert_eq!(
            reed.clone().act_as("team:core").expect_err("not held"),
            ActAsRefusal::NotHeld(slug("core"))
        );
        for asked in ["user:reed", "group:/groups/x", "run:r1", "llm-d", ""] {
            assert_eq!(
                reed.clone().act_as(asked).expect_err("not a team"),
                ActAsRefusal::NotATeam(asked.to_string()),
                "{asked:?}"
            );
        }
    }

    #[test]
    fn an_audit_row_names_the_actor_and_the_team_acted_as() {
        let action = crate::authz::action::Action {
            resource: crate::authz::action::ResourceType::Team,
            verb: crate::authz::action::Verb::Create,
        };
        let reed = caller(&[(slug("llm-d"), TeamRole::Owner)], AuthPath::Session);
        let own = reed.audit_event(action, "x", false, "no-rule");
        assert_eq!(own.actor, "user:reed");
        assert_eq!(own.subject, None);
        let acting = reed.act_as("team:llm-d").expect("held");
        let event = acting.audit_event(action, "x", false, "no-rule");
        assert_eq!(event.actor, "user:reed");
        assert_eq!(event.subject.as_deref(), Some("team:llm-d"));
    }
}
