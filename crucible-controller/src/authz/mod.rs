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
use crate::authz::model::{Principal, Principals};
use crate::authz::store::AuditEvent;
use crate::identity::auth::AuthPath;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

/// The request's subject set and the credential path that proved it.
#[derive(Debug, Clone)]
pub struct Caller {
    pub principals: Principals,
    pub path: AuthPath,
}

impl Caller {
    /// The actor: the user principal that caused the request, if the request named anybody.
    pub fn actor(&self) -> Option<&Principal> {
        self.principals.user()
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
            subject: None,
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

    /// The same caller with memberships re-read, after a write that changed them.
    pub async fn refreshed(
        &self,
        pool: &sqlx::PgPool,
        roles: &crate::identity::auth::Roles,
    ) -> anyhow::Result<Caller> {
        let groups: Vec<String> = self.principals.group_paths().map(str::to_string).collect();
        resolve_caller(pool, roles, self.principals.login(), &groups, self.path).await
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
    Ok(Caller { principals, path })
}

#[cfg(test)]
mod tests {
    use crate::authz::Caller;
    use crate::authz::model::{Membership, Principals, TeamRole, TeamSlug, Via};
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
}
