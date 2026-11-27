//! Whether a project member may run a pipeline on a ref, by GitLab's protected-ref rules: on a
//! protected branch they must be allowed to push or merge, on a protected tag to create it.

use crate::gitlab::model::{ProtectedBranch, ProtectedTag, RefAccess};

const NO_ONE: u32 = 0;

/// The member a pipeline would run for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Runner {
    pub user_id: u64,
    pub access_level: u32,
}

pub fn refused_by(
    git_ref: &str,
    runner: Runner,
    branches: &[ProtectedBranch],
    tags: &[ProtectedTag],
) -> Option<String> {
    let (name, branch, tag) = match (
        git_ref.strip_prefix("refs/heads/"),
        git_ref.strip_prefix("refs/tags/"),
    ) {
        (Some(name), _) => (name, true, false),
        (_, Some(name)) => (name, false, true),
        _ => (git_ref, true, true),
    };
    let branch_rules = branches
        .iter()
        .filter(|_| branch)
        .filter(|b| matches(&b.name, name))
        .find(|b| {
            !b.push_access_levels
                .iter()
                .chain(&b.merge_access_levels)
                .any(|a| allows(a, runner))
        })
        .map(|b| format!("protected branch {:?}", b.name));
    branch_rules.or_else(|| {
        tags.iter()
            .filter(|_| tag)
            .filter(|t| matches(&t.name, name))
            .find(|t| !t.create_access_levels.iter().any(|a| allows(a, runner)))
            .map(|t| format!("protected tag {:?}", t.name))
    })
}

fn allows(access: &RefAccess, runner: Runner) -> bool {
    match (access.user_id, access.group_id, access.deploy_key_id) {
        (Some(user), _, _) => user == runner.user_id,
        (None, None, None) => access
            .access_level
            .is_some_and(|level| level != NO_ONE && runner.access_level >= level),
        _ => false,
    }
}

fn matches(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return false;
    };
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use crate::gitlab::access::*;

    fn level(l: u32) -> RefAccess {
        RefAccess {
            access_level: Some(l),
            user_id: None,
            group_id: None,
            deploy_key_id: None,
        }
    }

    fn user(id: u64) -> RefAccess {
        RefAccess {
            access_level: Some(40),
            user_id: Some(id),
            group_id: None,
            deploy_key_id: None,
        }
    }

    fn group() -> RefAccess {
        RefAccess {
            access_level: Some(30),
            user_id: None,
            group_id: Some(3),
            deploy_key_id: None,
        }
    }

    fn branch(name: &str, push: Vec<RefAccess>, merge: Vec<RefAccess>) -> ProtectedBranch {
        ProtectedBranch {
            name: name.into(),
            push_access_levels: push,
            merge_access_levels: merge,
        }
    }

    const DEV: Runner = Runner {
        user_id: 7,
        access_level: 30,
    };
    const MAINTAINER: Runner = Runner {
        user_id: 8,
        access_level: 40,
    };

    #[test]
    fn wildcards_match_like_gitlabs() {
        assert!(matches("main", "main"));
        assert!(!matches("main", "mainline"));
        assert!(matches("release-*", "release-1.2"));
        assert!(matches("*-stable", "16-stable"));
        assert!(matches("production/*", "production/eu/1"));
        assert!(matches("a*b*c", "aXbYc"));
        assert!(!matches("a*b*c", "aXcYb"));
        assert!(matches("*", "anything"));
        assert!(!matches("ab*ba", "aba"));
    }

    #[test]
    fn an_unprotected_ref_is_open_to_any_member() {
        let branches = [branch("main", vec![level(40)], vec![level(40)])];
        assert_eq!(refused_by("feature", DEV, &branches, &[]), None);
    }

    #[test]
    fn a_protected_branch_needs_push_or_merge() {
        let maintainers = [branch("main", vec![level(40)], vec![level(40)])];
        assert_eq!(
            refused_by("main", DEV, &maintainers, &[]).as_deref(),
            Some("protected branch \"main\"")
        );
        assert_eq!(refused_by("main", MAINTAINER, &maintainers, &[]), None);
        assert_eq!(
            refused_by("refs/heads/main", DEV, &maintainers, &[]).as_deref(),
            Some("protected branch \"main\"")
        );
        let devs_merge = [branch("main", vec![level(40)], vec![level(30)])];
        assert_eq!(refused_by("main", DEV, &devs_merge, &[]), None);
    }

    #[test]
    fn no_one_groups_and_other_users_allow_no_one_else() {
        let no_one = [branch("main", vec![level(0)], vec![level(0)])];
        assert!(refused_by("main", MAINTAINER, &no_one, &[]).is_some());
        let named = [branch("main", vec![user(7)], vec![group()])];
        assert_eq!(refused_by("main", DEV, &named, &[]), None);
        assert!(refused_by("main", MAINTAINER, &named, &[]).is_some());
    }

    #[test]
    fn every_matching_rule_must_allow() {
        let both = [
            branch("release-*", vec![level(30)], vec![]),
            branch("release-1", vec![level(40)], vec![]),
        ];
        assert_eq!(
            refused_by("release-1", DEV, &both, &[]).as_deref(),
            Some("protected branch \"release-1\"")
        );
        assert_eq!(refused_by("release-2", DEV, &both, &[]), None);
    }

    #[test]
    fn a_protected_tag_needs_create() {
        let tags = [ProtectedTag {
            name: "v*".into(),
            create_access_levels: vec![level(40)],
        }];
        assert_eq!(
            refused_by("v1.0", DEV, &[], &tags).as_deref(),
            Some("protected tag \"v*\"")
        );
        assert_eq!(refused_by("v1.0", MAINTAINER, &[], &tags), None);
        let branches = [branch("v*", vec![level(40)], vec![])];
        assert_eq!(refused_by("refs/tags/v1.0", DEV, &branches, &[]), None);
        assert!(refused_by("refs/heads/v1.0", DEV, &[], &tags).is_none());
    }
}
