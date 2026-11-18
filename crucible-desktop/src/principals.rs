//! The users, teams and groups the app has seen, to complete a principal as it is typed. The
//! controller has no directory to ask, so these come from what its other answers name.

use std::collections::BTreeSet;

use crux::dto::MemberKind;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Known {
    users: BTreeSet<String>,
    teams: BTreeSet<String>,
    groups: BTreeSet<String>,
}

/// What a field takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// A grantee or owner: `user:<login>` or `team:<slug>`.
    Principal,
    /// A bare name of one kind, as a team's member list spells it.
    Member(MemberKind),
}

impl Known {
    /// Take `user:<login>`, `team:<slug>` or `group:<path>`; anything else is skipped.
    pub fn principal(&mut self, principal: &str) {
        let Some((kind, name)) = principal.trim().split_once(':') else {
            return;
        };
        let into = match kind {
            "user" => &mut self.users,
            "team" => &mut self.teams,
            "group" => &mut self.groups,
            _ => return,
        };
        note(into, name);
    }

    pub fn user(&mut self, login: &str) {
        note(&mut self.users, login);
    }

    pub fn team(&mut self, slug: &str) {
        note(&mut self.teams, slug);
    }

    pub fn member(&mut self, kind: MemberKind, name: &str) {
        match kind {
            MemberKind::User => self.user(name),
            MemberKind::Team => self.team(name),
            MemberKind::Group => note(&mut self.groups, name),
            MemberKind::Rule => {}
        }
    }

    /// Up to `limit` completions of `typed`: names starting with it first, then names containing
    /// it, each group in order. Nothing for an empty field or one already complete.
    pub fn suggest(&self, typed: &str, want: Want, limit: usize) -> Vec<String> {
        let typed = typed.trim().to_ascii_lowercase();
        if typed.is_empty() {
            return Vec::new();
        }
        let candidates: Vec<String> = match want {
            Want::Principal => self
                .users
                .iter()
                .map(|login| format!("user:{login}"))
                .chain(self.teams.iter().map(|slug| format!("team:{slug}")))
                .collect(),
            Want::Member(MemberKind::User) => self.users.iter().cloned().collect(),
            Want::Member(MemberKind::Team) => self.teams.iter().cloned().collect(),
            Want::Member(MemberKind::Group) => self.groups.iter().cloned().collect(),
            Want::Member(MemberKind::Rule) => Vec::new(),
        };
        let name = |candidate: &str| {
            candidate
                .split_once(':')
                .map_or(candidate, |(_, name)| name)
                .to_ascii_lowercase()
        };
        let starts = |candidate: &String| {
            candidate.to_ascii_lowercase().starts_with(&typed)
                || name(candidate).starts_with(&typed)
        };
        let contains = |candidate: &String| candidate.to_ascii_lowercase().contains(&typed);
        let open = |candidate: &&String| candidate.to_ascii_lowercase() != typed;
        let first = candidates.iter().filter(open).filter(|c| starts(c));
        let then = candidates
            .iter()
            .filter(open)
            .filter(|c| !starts(c) && contains(c));
        first.chain(then).take(limit).cloned().collect()
    }
}

impl crate::App {
    /// Every principal named by what the controller has answered so far.
    pub fn known_principals(&self) -> Known {
        let mut known = Known::default();
        for owner in self.owners() {
            known.principal(&owner);
        }
        for launch in self.remote.live.launches() {
            known.user(&launch.by);
        }
        let admin = &self.remote.admin;
        for team in admin.teams.iter().flatten().flatten() {
            known.team(&team.slug);
            for member in &team.members {
                known.member(member.kind, &member.member);
                if let Some(by) = &member.added_by {
                    known.principal(by);
                }
            }
        }
        for secret in admin.secrets.iter().flatten().flatten() {
            known.principal(&secret.owner);
        }
        for provider in admin.providers.iter().flatten().flatten() {
            known.principal(&provider.owner);
            if let Some(owner) = &provider.secret_owner {
                known.principal(owner);
            }
        }
        for owner in admin
            .schedules
            .iter()
            .flatten()
            .flatten()
            .filter_map(|schedule| schedule.owner_principal.as_ref())
        {
            known.principal(owner);
        }
        for owner in admin
            .webhooks
            .iter()
            .flatten()
            .flatten()
            .filter_map(|hook| hook.owner_principal.as_ref())
        {
            known.principal(owner);
        }
        if let Some((_, Some(Ok(shares)))) = &admin.shares {
            for share in shares {
                known.principal(&share.grantee);
            }
        }
        known
    }
}

fn note(into: &mut BTreeSet<String>, name: &str) {
    let name = name.trim();
    if !name.is_empty() && !name.chars().any(char::is_whitespace) {
        into.insert(name.to_string());
    }
}

#[cfg(test)]
mod tests {
    use crux::dto::MemberKind;

    use crate::principals::{Known, Want};

    fn known() -> Known {
        let mut known = Known::default();
        for principal in [
            "user:weaton",
            "user:wynn",
            "team:platform-administrators",
            "team:weather",
            "group:inference-eng",
            "service:ignored",
            "no-kind",
            "user:two words",
        ] {
            known.principal(principal);
        }
        known.member(MemberKind::User, "abe");
        known.member(MemberKind::Team, "infra");
        known.member(MemberKind::Group, "inference-ai-midstream");
        known.member(MemberKind::Rule, "email-domain:example.com");
        known
    }

    #[test]
    fn a_principal_completes_by_name_or_with_its_kind_prefix_first() {
        let known = known();
        assert_eq!(
            known.suggest("we", Want::Principal, 8),
            ["user:weaton", "team:weather"]
        );
        assert_eq!(
            known.suggest("team:we", Want::Principal, 8),
            ["team:weather"]
        );
        assert_eq!(
            known.suggest("ADMIN", Want::Principal, 8),
            ["team:platform-administrators"]
        );
        assert_eq!(
            known.suggest("a", Want::Principal, 2),
            ["user:abe", "user:weaton"],
            "names starting with it come before names containing it, up to the limit"
        );
    }

    #[test]
    fn nothing_is_offered_for_an_empty_or_complete_field_or_an_unknown_kind() {
        let known = known();
        assert!(known.suggest("  ", Want::Principal, 8).is_empty());
        assert!(known.suggest("user:wynn", Want::Principal, 8).is_empty());
        assert!(known.suggest("ignored", Want::Principal, 8).is_empty());
        assert!(known.suggest("two", Want::Principal, 8).is_empty());
        assert!(
            known.suggest("inference", Want::Principal, 8).is_empty(),
            "a group is not a grantee"
        );
    }

    #[test]
    fn a_member_completes_as_a_bare_name_of_its_kind() {
        let known = known();
        assert_eq!(
            known.suggest("w", Want::Member(MemberKind::User), 8),
            ["weaton", "wynn"]
        );
        assert_eq!(
            known.suggest("inf", Want::Member(MemberKind::Team), 8),
            ["infra"]
        );
        assert_eq!(
            known.suggest("inf", Want::Member(MemberKind::Group), 8),
            ["inference-ai-midstream", "inference-eng"]
        );
        assert!(
            known
                .suggest("email", Want::Member(MemberKind::Rule), 8)
                .is_empty()
        );
    }
}
