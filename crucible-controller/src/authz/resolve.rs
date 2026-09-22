//! Team membership resolution (RFC-0003 C-TEAMS): from the principals a credential proves to the
//! teams they reach, through group and rule members and nested teams.
//!
//! ```text
//!   claims: user:alice, group:/groups/llm-d, email alice@example.com
//!
//!   team llm-d      ─ user:alice @ owner          -> alice: owner   (direct)
//!   team platform   ─ team:llm-d @ maintainer     -> alice: min(owner, maintainer) = maintainer
//!   team everyone   ─ rule email-domain:example.com @ member
//!                   ─ team:platform @ member      -> alice: max(member, min(maintainer, member)) = member
//! ```
//!
//! Along a nesting path the role is the minimum of the roles on the path; over paths it is the
//! maximum. A credential that proves no groups seeds nothing from group or rule members, so nothing
//! derived from them can reach it through a nested team either.

use crate::authz::model::{MemberRef, Membership, TeamSlug, Via};
use crate::authz::store::{KnownUser, MemberRow};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// What the identity layer verified about the caller.
#[derive(Debug, Clone, Copy, Default)]
pub struct Claims<'a> {
    pub login: Option<&'a str>,
    pub email: Option<&'a str>,
    pub groups: &'a [String],
    /// Whether the credential's groups are the issuer's word ([`crate::identity::auth::AuthPath::carries_groups`]).
    /// False leaves group and rule members unmatched whatever `groups` holds.
    pub proves_groups: bool,
}

/// Every team the claims reach, with the role held and how.
pub fn resolve(members: &[MemberRow], claims: &Claims<'_>) -> BTreeMap<TeamSlug, Membership> {
    let mut held: BTreeMap<TeamSlug, Membership> = BTreeMap::new();

    for row in members {
        let via = match &row.member {
            MemberRef::User(login) if claims.login == Some(login.as_str()) => {
                Via::Direct { role: row.role }
            }
            MemberRef::Group(path) if claims.proves_groups && claims.groups.contains(path) => {
                Via::Group {
                    group: path.clone(),
                    role: row.role,
                }
            }
            MemberRef::Rule(rule)
                if claims.proves_groups && rule.matches(claims.email, claims.groups) =>
            {
                Via::Rule {
                    rule: rule.to_string(),
                    role: row.role,
                }
            }
            _ => continue,
        };
        hold(&mut held, &row.team, via);
    }

    loop {
        let mut changed = false;
        for row in members {
            let MemberRef::Team(child) = &row.member else {
                continue;
            };
            let Some(child_role) = held.get(child).map(|m| m.role) else {
                continue;
            };
            let via = Via::Team {
                team: child.clone(),
                role: child_role.min(row.role),
            };
            changed |= hold(&mut held, &row.team, via);
        }
        if !changed {
            break;
        }
    }
    held
}

/// Record one source of membership; true when it changed anything.
fn hold(held: &mut BTreeMap<TeamSlug, Membership>, team: &TeamSlug, via: Via) -> bool {
    let entry = held.entry(team.clone()).or_insert_with(|| Membership {
        role: via.role(),
        via: BTreeSet::new(),
    });
    entry.role = entry.role.max(via.role());
    entry.via.insert(via)
}

/// Whether listing `child` as a member of `parent` would close a cycle: `parent` is `child`, or
/// is already (transitively) a member of `child`.
pub fn would_cycle(members: &[MemberRow], parent: &TeamSlug, child: &TeamSlug) -> bool {
    if parent == child {
        return true;
    }
    let mut seen: HashSet<&TeamSlug> = HashSet::new();
    let mut stack = vec![child];
    while let Some(team) = stack.pop() {
        for row in members.iter().filter(|r| &r.team == team) {
            if let MemberRef::Team(nested) = &row.member {
                if nested == parent {
                    return true;
                }
                if seen.insert(nested) {
                    stack.push(nested);
                }
            }
        }
    }
    false
}

/// Whether any membership of `team` currently resolves to a user the controller has seen: a
/// listed login, a group or rule some signed-in user's recorded claims satisfy, or a nested team
/// that does. A team that reaches nobody is listed as unreachable to platform administrators.
pub fn reachable(members: &[MemberRow], users: &[KnownUser], team: &TeamSlug) -> bool {
    let mut seen: HashSet<&TeamSlug> = HashSet::new();
    let mut stack = vec![team];
    while let Some(team) = stack.pop() {
        for row in members.iter().filter(|r| &r.team == team) {
            match &row.member {
                MemberRef::User(_) => return true,
                MemberRef::Group(path) => {
                    if users.iter().any(|u| u.groups.contains(path)) {
                        return true;
                    }
                }
                MemberRef::Rule(rule) => {
                    if users
                        .iter()
                        .any(|u| rule.matches(u.email.as_deref(), &u.groups))
                    {
                        return true;
                    }
                }
                MemberRef::Team(nested) => {
                    if seen.insert(nested) {
                        stack.push(nested);
                    }
                }
            }
        }
    }
    false
}

/// The teams a login and its groups reach, resolved from the current membership records. The
/// login's email is looked up only when the groups are proven, which is the only case a rule
/// consults it.
pub async fn teams_for(
    pool: &sqlx::PgPool,
    login: Option<&str>,
    groups: &[String],
    proves_groups: bool,
) -> anyhow::Result<std::collections::BTreeMap<TeamSlug, Membership>> {
    let email = match (login, proves_groups) {
        (Some(login), true) => crate::authz::store::email_of(pool, login).await?,
        _ => None,
    };
    let members = crate::authz::store::all_members(pool).await?;
    let claims = Claims {
        login,
        email: email.as_deref(),
        groups,
        proves_groups,
    };
    Ok(resolve(&members, &claims))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::model::{MemberKind, MembershipRule, TeamRole};

    fn row(team: &str, kind: MemberKind, member: &str, role: TeamRole) -> MemberRow {
        MemberRow {
            team: TeamSlug::parse(team).expect("slug"),
            member: MemberRef::parse(kind, member).expect("member"),
            role,
            since: "2026-09-13T00:00:00Z".into(),
            added_by: None,
        }
    }

    fn slug(s: &str) -> TeamSlug {
        TeamSlug::parse(s).expect("slug")
    }

    fn graph() -> Vec<MemberRow> {
        vec![
            row("llm-d", MemberKind::User, "alice", TeamRole::Owner),
            row(
                "llm-d",
                MemberKind::Group,
                "/groups/llm-d",
                TeamRole::Member,
            ),
            row("platform", MemberKind::Team, "llm-d", TeamRole::Maintainer),
            row(
                "everyone",
                MemberKind::Rule,
                "email-domain:example.com",
                TeamRole::Member,
            ),
            row("everyone", MemberKind::Team, "platform", TeamRole::Member),
            row("ops", MemberKind::User, "bob", TeamRole::Maintainer),
        ]
    }

    #[test]
    fn a_direct_owner_is_capped_along_the_path_and_maxed_over_paths() {
        let groups = vec!["/groups/llm-d".to_string()];
        let held = resolve(
            &graph(),
            &Claims {
                login: Some("alice"),
                email: Some("alice@example.com"),
                groups: &groups,
                proves_groups: true,
            },
        );
        assert_eq!(held[&slug("llm-d")].role, TeamRole::Owner);
        assert_eq!(held[&slug("llm-d")].via.len(), 2, "direct and group");
        assert_eq!(held[&slug("platform")].role, TeamRole::Maintainer);
        assert_eq!(held[&slug("everyone")].role, TeamRole::Member);
        assert!(held[&slug("everyone")].via.contains(&Via::Rule {
            rule: "email-domain:example.com".into(),
            role: TeamRole::Member
        }));
        assert!(held[&slug("everyone")].via.contains(&Via::Team {
            team: slug("platform"),
            role: TeamRole::Member
        }));
        assert!(!held.contains_key(&slug("ops")));
    }

    /// A static token, a cluster token, or a downgraded session: only the login counts, and
    /// nothing a group or rule would have conferred leaks through a nested team.
    #[test]
    fn a_credential_without_groups_holds_only_login_derived_memberships() {
        let groups = vec!["/groups/llm-d".to_string()];
        let held = resolve(
            &graph(),
            &Claims {
                login: Some("carol"),
                email: Some("carol@example.com"),
                groups: &groups,
                proves_groups: false,
            },
        );
        assert!(held.is_empty());

        let held = resolve(
            &graph(),
            &Claims {
                login: Some("alice"),
                email: Some("alice@example.com"),
                groups: &groups,
                proves_groups: false,
            },
        );
        assert_eq!(held[&slug("llm-d")].role, TeamRole::Owner);
        assert_eq!(
            held[&slug("llm-d")].via,
            BTreeSet::from([Via::Direct {
                role: TeamRole::Owner
            }])
        );
        assert_eq!(held[&slug("platform")].role, TeamRole::Maintainer);
        assert_eq!(held[&slug("everyone")].role, TeamRole::Member);
    }

    #[test]
    fn a_group_member_reaches_through_nesting_at_the_minimum_role() {
        let groups = vec![
            "/groups/llm-d/devs".to_string(),
            "/groups/llm-d".to_string(),
        ];
        let held = resolve(
            &graph(),
            &Claims {
                login: Some("dave"),
                email: None,
                groups: &groups,
                proves_groups: true,
            },
        );
        assert_eq!(held[&slug("llm-d")].role, TeamRole::Member);
        assert_eq!(held[&slug("platform")].role, TeamRole::Member);
        assert_eq!(held[&slug("everyone")].role, TeamRole::Member);
    }

    #[test]
    fn an_anonymous_caller_reaches_nothing() {
        assert!(resolve(&graph(), &Claims::default()).is_empty());
    }

    #[test]
    fn a_cycle_is_detected_through_any_depth() {
        let members = graph();
        assert!(would_cycle(&members, &slug("llm-d"), &slug("llm-d")));
        assert!(would_cycle(&members, &slug("llm-d"), &slug("platform")));
        assert!(would_cycle(&members, &slug("llm-d"), &slug("everyone")));
        assert!(!would_cycle(&members, &slug("everyone"), &slug("ops")));
        assert!(!would_cycle(&members, &slug("ops"), &slug("everyone")));
    }

    #[test]
    fn reachability_follows_users_groups_rules_and_nesting() {
        let members = vec![
            row("by-group", MemberKind::Group, "/groups/x", TeamRole::Member),
            row(
                "by-rule",
                MemberKind::Rule,
                "email-domain:example.com",
                TeamRole::Member,
            ),
            row("nested", MemberKind::Team, "by-group", TeamRole::Member),
            row("empty", MemberKind::Team, "hollow", TeamRole::Member),
            row("hollow", MemberKind::Group, "/nobody", TeamRole::Member),
        ];
        let users = vec![KnownUser {
            login: "alice".into(),
            email: Some("alice@example.com".into()),
            groups: vec!["/groups/x".into()],
        }];
        assert!(reachable(&members, &users, &slug("by-group")));
        assert!(reachable(&members, &users, &slug("by-rule")));
        assert!(reachable(&members, &users, &slug("nested")));
        assert!(!reachable(&members, &users, &slug("empty")));
        assert!(!reachable(&members, &[], &slug("by-group")));
        assert!(!reachable(&members, &users, &slug("missing")));
        let _ = MembershipRule::EmailDomain("x.io".into());
    }
}
