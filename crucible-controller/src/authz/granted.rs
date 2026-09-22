//! The shares a principal set holds (RFC-0003 C-SHARING), as the decision reads them: the best
//! unexpired role per resource, evaluated against the controller's clock.

use crate::authz::action::{ResourceType, Verb};
use crate::authz::decision::{Resource, ShareRole};
use crate::authz::model::Principals;
use crate::authz::store;
use std::collections::BTreeMap;

/// The actions a share role confers, before the resource type's own vocabulary narrows them.
pub fn confers(role: ShareRole) -> &'static [Verb] {
    match role {
        ShareRole::Viewer => &[Verb::Read],
        ShareRole::Launcher => &[Verb::Read, Verb::Launch],
        ShareRole::Editor => &[Verb::Read, Verb::Update, Verb::Launch, Verb::Bind],
    }
}

pub fn expired(not_after: Option<&str>, now: jiff::Timestamp) -> bool {
    match not_after.and_then(|t| t.parse::<jiff::Timestamp>().ok()) {
        Some(until) => until <= now,
        None => false,
    }
}

/// The best unexpired share role `principals` hold per resource id of one type.
pub async fn held(
    pool: &sqlx::PgPool,
    principals: &Principals,
    rtype: ResourceType,
) -> anyhow::Result<BTreeMap<String, ShareRole>> {
    let grantees: Vec<String> = principals.all().iter().map(|p| p.to_string()).collect();
    let rows = store::shares_granted_to(pool, rtype.as_str(), &grantees).await?;
    let now = jiff::Timestamp::now();
    let mut best: BTreeMap<String, ShareRole> = BTreeMap::new();
    for row in rows {
        if expired(row.not_after.as_deref(), now) {
            continue;
        }
        let Ok(role) = ShareRole::parse(&row.role) else {
            continue;
        };
        let entry = best.entry(row.resource_id).or_insert(role);
        *entry = (*entry).max(role);
    }
    Ok(best)
}

/// `resource` with the best unexpired share the caller holds on it.
pub async fn attach(
    pool: &sqlx::PgPool,
    principals: &Principals,
    resource: Resource,
) -> anyhow::Result<Resource> {
    let mut resource = resource;
    resource.share = held(pool, principals, resource.rtype)
        .await?
        .remove(&resource.id);
    Ok(resource)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_confers_its_actions_and_expiry_is_by_the_clock() {
        assert_eq!(confers(ShareRole::Viewer), &[Verb::Read]);
        assert_eq!(confers(ShareRole::Launcher), &[Verb::Read, Verb::Launch]);
        assert_eq!(
            confers(ShareRole::Editor),
            &[Verb::Read, Verb::Update, Verb::Launch, Verb::Bind]
        );
        let now = "2026-09-14T00:00:00Z".parse().expect("now");
        assert!(!expired(None, now));
        assert!(!expired(Some("2026-09-15T00:00:00Z"), now));
        assert!(expired(Some("2026-09-14T00:00:00Z"), now));
        assert!(expired(Some("2026-09-13T00:00:00Z"), now));
    }
}
