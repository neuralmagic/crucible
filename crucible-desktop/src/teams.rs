//! The Teams page's own state: the order teams and members list in, a member list with one
//! change made, and the forms that add a team or a member.

use std::cmp::Reverse;

use crux::dto::{MemberKind, Team, TeamMember, TeamMemberBody, TeamRole};
use rune::TextEdit;

pub const KINDS: [MemberKind; 4] = [
    MemberKind::User,
    MemberKind::Group,
    MemberKind::Team,
    MemberKind::Rule,
];

pub const ROLES: [TeamRole; 3] = [TeamRole::Member, TeamRole::Maintainer, TeamRole::Owner];

pub fn kind_label(kind: MemberKind) -> &'static str {
    match kind {
        MemberKind::User => "user",
        MemberKind::Group => "group",
        MemberKind::Team => "team",
        MemberKind::Rule => "rule",
    }
}

pub fn role_label(role: TeamRole) -> &'static str {
    match role {
        TeamRole::Member => "member",
        TeamRole::Maintainer => "maintainer",
        TeamRole::Owner => "owner",
    }
}

/// The caller's teams first, then by slug.
pub fn sorted_teams(teams: &[Team]) -> Vec<&Team> {
    let mut sorted: Vec<&Team> = teams.iter().collect();
    sorted.sort_by(|a, b| {
        b.my_role
            .is_some()
            .cmp(&a.my_role.is_some())
            .then_with(|| a.slug.cmp(&b.slug))
    });
    sorted
}

/// Owners first, then by how each is held, then by name.
pub fn sorted_members(members: &[TeamMember]) -> Vec<&TeamMember> {
    let mut sorted: Vec<&TeamMember> = members.iter().collect();
    sorted.sort_by(|a, b| {
        Reverse(a.role)
            .cmp(&Reverse(b.role))
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.member.cmp(&b.member))
    });
    sorted
}

pub fn bodies(members: &[TeamMember]) -> Vec<TeamMemberBody> {
    members
        .iter()
        .map(|m| TeamMemberBody {
            kind: m.kind,
            member: m.member.clone(),
            role: m.role,
        })
        .collect()
}

/// `members` with `added`, in place of a row naming the same kind and member.
pub fn with_member(mut members: Vec<TeamMemberBody>, added: TeamMemberBody) -> Vec<TeamMemberBody> {
    members.retain(|m| !(m.kind == added.kind && m.member == added.member));
    members.push(added);
    members
}

pub fn without_member(
    mut members: Vec<TeamMemberBody>,
    kind: MemberKind,
    member: &str,
) -> Vec<TeamMemberBody> {
    members.retain(|m| !(m.kind == kind && m.member == member));
    members
}

pub fn with_role(
    mut members: Vec<TeamMemberBody>,
    kind: MemberKind,
    member: &str,
    role: TeamRole,
) -> Vec<TeamMemberBody> {
    for m in &mut members {
        if m.kind == kind && m.member == member {
            m.role = role;
        }
    }
    members
}

/// A team keeps at least one user at owner; the controller refuses a list without one.
pub fn keeps_an_owner(members: &[TeamMemberBody]) -> bool {
    members
        .iter()
        .any(|m| m.kind == MemberKind::User && m.role == TeamRole::Owner)
}

/// 2 to 63 of lowercase letters, digits and dashes, not starting with a dash.
pub fn valid_slug(slug: &str) -> bool {
    let plain = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    (2..=63).contains(&slug.len())
        && slug.starts_with(plain)
        && slug.chars().all(|c| plain(c) || c == '-')
}

pub struct TeamForm {
    pub slug: TextEdit,
    pub name: TextEdit,
}

impl Default for TeamForm {
    fn default() -> Self {
        TeamForm {
            slug: TextEdit::new(""),
            name: TextEdit::new(""),
        }
    }
}

impl TeamForm {
    /// The slug and display name to create with, the name defaulting to the slug.
    pub fn ask(&self) -> Result<(String, String), String> {
        let slug = self.slug.text.trim();
        if !valid_slug(slug) {
            return Err("2 to 63 lowercase letters, digits and dashes".into());
        }
        let name = self.name.text.trim();
        let name = if name.is_empty() { slug } else { name };
        Ok((slug.to_string(), name.to_string()))
    }
}

pub struct MemberForm {
    pub kind: MemberKind,
    pub member: TextEdit,
    pub role: TeamRole,
}

impl Default for MemberForm {
    fn default() -> Self {
        MemberForm {
            kind: MemberKind::User,
            member: TextEdit::new(""),
            role: TeamRole::Member,
        }
    }
}

impl MemberForm {
    pub fn ask(&self) -> Option<TeamMemberBody> {
        let member = self.member.text.trim();
        (!member.is_empty()).then(|| TeamMemberBody {
            kind: self.kind,
            member: member.to_string(),
            role: self.role,
        })
    }
}

#[cfg(test)]
mod tests {
    use crux::dto::{MemberKind, Team, TeamMember, TeamMemberBody, TeamRole};
    use rune::TextEdit;

    use crate::teams::{
        MemberForm, TeamForm, bodies, keeps_an_owner, sorted_members, sorted_teams, valid_slug,
        with_member, with_role, without_member,
    };

    fn member(kind: MemberKind, name: &str, role: TeamRole) -> TeamMember {
        TeamMember {
            kind,
            member: name.into(),
            role,
            since: String::new(),
            added_by: None,
            signed_in: false,
        }
    }

    fn body(kind: MemberKind, name: &str, role: TeamRole) -> TeamMemberBody {
        TeamMemberBody {
            kind,
            member: name.into(),
            role,
        }
    }

    fn team(slug: &str, my_role: Option<TeamRole>) -> Team {
        Team {
            slug: slug.into(),
            display_name: slug.into(),
            created_by: None,
            created_at: String::new(),
            members: Vec::new(),
            my_role,
            reachable: true,
        }
    }

    #[test]
    fn my_teams_list_first_and_each_group_by_slug() {
        let teams = [
            team("zeta", None),
            team("beta", Some(TeamRole::Member)),
            team("alpha", None),
            team("omega", Some(TeamRole::Owner)),
        ];
        let slugs: Vec<&str> = sorted_teams(&teams)
            .iter()
            .map(|t| t.slug.as_str())
            .collect();
        assert_eq!(slugs, ["beta", "omega", "alpha", "zeta"]);
    }

    #[test]
    fn members_list_owners_first_then_by_how_held_then_by_name() {
        let members = [
            member(MemberKind::Rule, "email-domain:x.com", TeamRole::Member),
            member(MemberKind::Group, "eng", TeamRole::Owner),
            member(MemberKind::User, "wynn", TeamRole::Member),
            member(MemberKind::User, "zed", TeamRole::Owner),
            member(MemberKind::User, "abe", TeamRole::Owner),
            member(MemberKind::Team, "infra", TeamRole::Maintainer),
        ];
        let order: Vec<&str> = sorted_members(&members)
            .iter()
            .map(|m| m.member.as_str())
            .collect();
        assert_eq!(
            order,
            ["abe", "zed", "eng", "infra", "wynn", "email-domain:x.com"]
        );
    }

    #[test]
    fn a_member_list_takes_one_change_and_keeps_the_rest() {
        let held = bodies(&[
            member(MemberKind::User, "wynn", TeamRole::Owner),
            member(MemberKind::Group, "eng", TeamRole::Member),
        ]);
        let added = with_member(
            held.clone(),
            body(MemberKind::User, "abe", TeamRole::Maintainer),
        );
        assert_eq!(added.len(), 3);
        let replaced = with_member(
            held.clone(),
            body(MemberKind::Group, "eng", TeamRole::Owner),
        );
        assert_eq!(
            replaced,
            [
                body(MemberKind::User, "wynn", TeamRole::Owner),
                body(MemberKind::Group, "eng", TeamRole::Owner),
            ]
        );
        assert_eq!(
            without_member(held.clone(), MemberKind::Group, "eng"),
            [body(MemberKind::User, "wynn", TeamRole::Owner)]
        );
        assert_eq!(
            without_member(held.clone(), MemberKind::User, "eng"),
            held,
            "a group is not removed by a user of the same name"
        );
        assert_eq!(
            with_role(held.clone(), MemberKind::Group, "eng", TeamRole::Maintainer)[1].role,
            TeamRole::Maintainer
        );
    }

    #[test]
    fn only_a_user_at_owner_keeps_a_team_owned() {
        assert!(keeps_an_owner(&[body(
            MemberKind::User,
            "wynn",
            TeamRole::Owner
        )]));
        assert!(!keeps_an_owner(&[
            body(MemberKind::Group, "eng", TeamRole::Owner),
            body(MemberKind::User, "wynn", TeamRole::Maintainer),
        ]));
        assert!(!keeps_an_owner(&[]));
    }

    #[test]
    fn a_slug_is_lowercase_letters_digits_and_dashes_of_2_to_63() {
        for good in ["ab", "infra-2", "9lives", &"a".repeat(63)] {
            assert!(valid_slug(good), "{good}");
        }
        for bad in [
            "a",
            "",
            "-lead",
            "Infra",
            "in fra",
            "in_fra",
            &"a".repeat(64),
        ] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }

    #[test]
    fn the_forms_trim_and_the_team_name_defaults_to_its_slug() {
        let form = TeamForm {
            slug: TextEdit::new(" infra "),
            name: TextEdit::new("  "),
        };
        assert_eq!(form.ask(), Ok(("infra".to_string(), "infra".to_string())));
        let named = TeamForm {
            slug: TextEdit::new("infra"),
            name: TextEdit::new(" Infrastructure "),
        };
        assert_eq!(
            named.ask(),
            Ok(("infra".to_string(), "Infrastructure".to_string()))
        );
        assert!(TeamForm::default().ask().is_err());

        let mut add = MemberForm::default();
        assert_eq!(add.ask(), None);
        add.member = TextEdit::new(" abe ");
        assert_eq!(
            add.ask(),
            Some(body(MemberKind::User, "abe", TeamRole::Member))
        );
    }
}
