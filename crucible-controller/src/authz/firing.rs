//! The decision a standing launch takes when it fires (RFC-0003 C-DECISION): the owner is the
//! subject, its memberships are resolved now, a team owner fires at `maintainer`, and a refusal
//! is audited before the firing parks.

#![allow(clippy::disallowed_macros)]

use crate::authz::action::{Action, ResourceType, Verb};
use crate::authz::decision::{Decision, Resource, Subject};
use crate::authz::model::{Principal, Principals};
use crate::authz::policy::ActivePolicy;
use crate::authz::store::{self, AuditEvent};
use anyhow::{Context, Result};
use sqlx::PgPool;

/// The subject a firing runs as. A user owner acts with the groups the last save or refresh
/// snapshotted and the teams those reach; a team owner acts as the team at `maintainer`.
pub async fn subject_for_owner(
    pool: &PgPool,
    owner: &Principal,
    groups: Option<&serde_json::Value>,
) -> Result<Subject> {
    match owner {
        Principal::Team(slug) => Ok(Subject::team_firing(slug)),
        Principal::User(login) => {
            let groups: Vec<String> = groups
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let proves_groups = !groups.is_empty();
            let teams =
                crate::authz::resolve::teams_for(pool, Some(login), &groups, proves_groups).await?;
            let principals = Principals::new(Some(login), &groups).with_teams(teams);
            Subject::of(&principals, proves_groups)
                .context("a user owner always resolves to a subject")
        }
        other => anyhow::bail!("{other} cannot own a standing launch"),
    }
}

/// Decide `standing_launch:launch` for the owner of standing launch `id`. A denial is audited
/// with the owner as actor and `firing` as the credential path.
pub async fn decide_firing(
    pool: &PgPool,
    policy: &ActivePolicy,
    id: &str,
    owner_principal: Option<&str>,
    groups: Option<&serde_json::Value>,
    now: jiff::Timestamp,
) -> Result<Decision> {
    let owner = crate::authz::model::Principal::stored(owner_principal);
    let resource = Resource::new(ResourceType::StandingLaunch, id, owner.clone());
    let action = Action {
        resource: ResourceType::StandingLaunch,
        verb: Verb::Launch,
    };
    let decision = match subject_for_owner(pool, &owner, groups).await {
        Ok(subject) => policy
            .current()
            .authorize(&subject, action, &resource, now.as_second()),
        Err(e) => Decision::denied(format!("unresolvable-owner: {e}")),
    };
    if !decision.allowed {
        let mut conn = pool.acquire().await?;
        store::audit(
            &mut conn,
            &AuditEvent {
                actor: owner.to_string(),
                subject: None,
                auth_path: "firing".to_string(),
                action: action.to_string(),
                resource_type: ResourceType::StandingLaunch.as_str().to_string(),
                resource_id: id.to_string(),
                allowed: false,
                rule: decision.reason(),
                prior: None,
                result: None,
            },
            &now.to_string(),
        )
        .await?;
    }
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::model::TeamSlug;
    use sqlx::PgPool;

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_user_owner_fires_as_themselves_and_a_team_owner_at_maintainer(pool: PgPool) {
        let policy = ActivePolicy::default_set().expect("default");
        let now = jiff::Timestamp::now();
        let d = decide_firing(&pool, &policy, "s1", Some("user:alice"), None, now)
            .await
            .expect("decide");
        assert!(d.allowed, "{}", d.reason());
        assert_eq!(d.rules, vec!["user-owner-all"]);

        let d = decide_firing(&pool, &policy, "s2", Some("team:llm-d"), None, now)
            .await
            .expect("decide");
        assert!(d.allowed, "{}", d.reason());
        assert!(
            d.rules.contains(&"team-member-read-launch".to_string()),
            "{}",
            d.reason()
        );
        let subject = subject_for_owner(
            &pool,
            &Principal::Team(TeamSlug::parse("llm-d").expect("slug")),
            None,
        )
        .await
        .expect("subject");
        assert_eq!(subject.tags["team:llm-d"], "maintainer");

        // A row that predates ownership is the platform's and fires as that team.
        let d = decide_firing(&pool, &policy, "s0", None, None, now)
            .await
            .expect("decide");
        assert!(d.allowed, "{}", d.reason());
        // An owner no subject can be built for is refused, and the refusal is on the trail.
        let d = decide_firing(&pool, &policy, "s3", Some("run:r1"), None, now)
            .await
            .expect("decide");
        assert!(!d.allowed);
        let trail = store::audit_for(&pool, "standing_launch", "s3", 5)
            .await
            .expect("trail");
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].auth_path, "firing");
        assert_eq!(trail[0].actor, "run:r1");
        assert!(
            store::audit_for(&pool, "standing_launch", "s1", 5)
                .await
                .expect("trail")
                .is_empty()
        );
    }
}
