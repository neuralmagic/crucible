//! Kubernetes RBAC as the authority for authorize rules: each [`AccessCheck`] becomes a
//! SubjectAccessReview for the caller's Kubernetes identity, and the API server decides.
//!
//! ```text
//!   claims.kube_user ──▶ user        (never a system: identity)
//!   OpenShift Groups ──▶ groups      (or claims.kube_groups), plus system:authenticated
//!   AccessCheck      ──▶ resourceAttributes ──▶ SubjectAccessReview ──▶ status.allowed
//! ```
//!
//! The server keeps acting with its own service account; RBAC only answers "may this person".
//! That account needs `create` on `subjectaccessreviews` and, for OpenShift groups, `list` on
//! `groups.user.openshift.io`.

use crate::guard::{AccessReview, ReviewFuture};
use crate::policy::AccessCheck;
use crate::principal::Principal;
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use kube::api::{Api, ApiResource, DynamicObject, ListParams, PostParams};
use kube::core::GroupVersionKind;
use serde_json::Value;
use std::time::Duration;

/// Claim naming the caller's Kubernetes user.
pub const USER_CLAIM: &str = "kube_user";
/// Claim listing the caller's Kubernetes groups, when groups come from the token.
pub const GROUPS_CLAIM: &str = "kube_groups";

/// Groups every authenticated OpenShift OAuth user carries; RBAC often binds through them.
const AUTHENTICATED: [&str; 2] = ["system:authenticated", "system:authenticated:oauth"];

const ALLOW_TTL: Duration = Duration::from_secs(30);
const GROUPS_TTL: Duration = Duration::from_secs(60);

/// Where a caller's groups come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSource {
    /// `user.openshift.io/v1` Group objects listing the user, read live (cached a minute).
    OpenShift,
    /// The token's `kube_groups` claim, for clusters whose identity provider issues the token.
    Token,
}

#[derive(Debug, thiserror::Error)]
pub enum KubeIdentityError {
    #[error("token carries no {USER_CLAIM} claim")]
    NoUser,
    #[error("{USER_CLAIM} {0:?} is a system identity")]
    SystemUser(String),
    #[error("{GROUPS_CLAIM} must be a list of strings")]
    Groups,
    #[error("listing OpenShift groups: {0}")]
    GroupLookup(#[source] kube::Error),
    #[error("creating the SubjectAccessReview: {0}")]
    Review(#[source] kube::Error),
}

/// Who the caller is to Kubernetes RBAC.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KubeIdentity {
    pub user: String,
    pub groups: Vec<String>,
}

/// The caller's user, from `kube_user`. A `system:` user is refused so no token can stand in for
/// a node, a service account or the cluster itself.
pub fn kube_user(principal: &Principal) -> Result<String, KubeIdentityError> {
    let user = principal
        .claims
        .get(USER_CLAIM)
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .ok_or(KubeIdentityError::NoUser)?;
    if user.starts_with("system:") {
        return Err(KubeIdentityError::SystemUser(user.to_string()));
    }
    Ok(user.to_string())
}

/// Groups from the token's `kube_groups`, without any `system:` group a token could use to claim
/// cluster powers (`system:masters`), plus the authenticated groups.
pub fn token_groups(principal: &Principal) -> Result<Vec<String>, KubeIdentityError> {
    let listed = match principal.claims.get(GROUPS_CLAIM) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|g| {
                g.as_str()
                    .map(str::to_string)
                    .ok_or(KubeIdentityError::Groups)
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(KubeIdentityError::Groups),
    };
    Ok(with_authenticated(
        listed.into_iter().filter(|g| !g.starts_with("system:")),
    ))
}

/// The names of the OpenShift Group objects whose `users` include `user`.
pub fn openshift_groups_of(groups: &[DynamicObject], user: &str) -> Vec<String> {
    groups
        .iter()
        .filter(|g| {
            g.data
                .get("users")
                .and_then(Value::as_array)
                .is_some_and(|users| users.iter().any(|u| u.as_str() == Some(user)))
        })
        .filter_map(|g| g.metadata.name.clone())
        .collect()
}

fn with_authenticated(groups: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut all: Vec<String> = groups.into_iter().collect();
    all.extend(AUTHENTICATED.map(String::from));
    all.sort();
    all.dedup();
    all
}

/// The SubjectAccessReview asking whether `identity` may do what `check` describes.
pub fn review_request(identity: &KubeIdentity, check: &AccessCheck) -> SubjectAccessReview {
    let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
    SubjectAccessReview {
        spec: SubjectAccessReviewSpec {
            user: Some(identity.user.clone()),
            groups: Some(identity.groups.clone()),
            resource_attributes: Some(ResourceAttributes {
                group: Some(check.group.clone()),
                resource: some(&check.resource),
                subresource: some(&check.subresource),
                verb: some(&check.verb),
                namespace: some(&check.namespace),
                name: some(&check.name),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct DecisionKey {
    identity: KubeIdentity,
    group: String,
    resource: String,
    subresource: String,
    verb: String,
    namespace: String,
    name: String,
}

pub struct SubjectAccessReviewer {
    client: kube::Client,
    groups: GroupSource,
    allowed: moka::future::Cache<DecisionKey, ()>,
    openshift_groups: moka::future::Cache<(), std::sync::Arc<Vec<DynamicObject>>>,
}

impl SubjectAccessReviewer {
    pub fn new(client: kube::Client, groups: GroupSource) -> Self {
        Self {
            client,
            groups,
            allowed: moka::future::Cache::builder()
                .time_to_live(ALLOW_TTL)
                .max_capacity(10_000)
                .build(),
            openshift_groups: moka::future::Cache::builder()
                .time_to_live(GROUPS_TTL)
                .build(),
        }
    }

    pub async fn identity(&self, principal: &Principal) -> Result<KubeIdentity, KubeIdentityError> {
        let user = kube_user(principal)?;
        let groups = match self.groups {
            GroupSource::Token => token_groups(principal)?,
            GroupSource::OpenShift => {
                let all = self.all_openshift_groups().await?;
                with_authenticated(openshift_groups_of(&all, &user))
            }
        };
        Ok(KubeIdentity { user, groups })
    }

    async fn all_openshift_groups(
        &self,
    ) -> Result<std::sync::Arc<Vec<DynamicObject>>, KubeIdentityError> {
        if let Some(cached) = self.openshift_groups.get(&()).await {
            return Ok(cached);
        }
        let gvk = GroupVersionKind::gvk("user.openshift.io", "v1", "Group");
        let api: Api<DynamicObject> =
            Api::all_with(self.client.clone(), &ApiResource::from_gvk(&gvk));
        let listed = api
            .list(&ListParams::default())
            .await
            .map_err(KubeIdentityError::GroupLookup)?;
        let groups = std::sync::Arc::new(listed.items);
        self.openshift_groups.insert((), groups.clone()).await;
        Ok(groups)
    }

    /// Allowed decisions are cached for 30 seconds; denials never are, so a new RoleBinding takes
    /// effect on the next call.
    pub async fn allowed(
        &self,
        principal: &Principal,
        check: &AccessCheck,
    ) -> Result<bool, KubeIdentityError> {
        let identity = self.identity(principal).await?;
        let key = DecisionKey {
            identity: identity.clone(),
            group: check.group.clone(),
            resource: check.resource.clone(),
            subresource: check.subresource.clone(),
            verb: check.verb.clone(),
            namespace: check.namespace.clone(),
            name: check.name.clone(),
        };
        if self.allowed.contains_key(&key) {
            return Ok(true);
        }
        let api: Api<SubjectAccessReview> = Api::all(self.client.clone());
        let reviewed = api
            .create(&PostParams::default(), &review_request(&identity, check))
            .await
            .map_err(KubeIdentityError::Review)?;
        let allowed = reviewed.status.is_some_and(|s| s.allowed);
        tracing::debug!(user = %identity.user, rule = %check.rule, verb = %check.verb,
            resource = %check.resource, namespace = %check.namespace, name = %check.name, allowed,
            "subject access review");
        if allowed {
            self.allowed.insert(key, ()).await;
        }
        Ok(allowed)
    }
}

impl AccessReview for SubjectAccessReviewer {
    fn review<'a>(&'a self, principal: &'a Principal, check: &'a AccessCheck) -> ReviewFuture<'a> {
        Box::pin(async move { Ok(self.allowed(principal, check).await?) })
    }
}

#[cfg(test)]
mod tests {
    use crate::kube::*;
    use serde_json::json;

    fn principal(claims: Value) -> Principal {
        match claims {
            Value::Object(m) => Principal::from_claims(m, crate::principal::Callers::Runs).unwrap(),
            other => panic!("{other}"),
        }
    }

    fn check() -> AccessCheck {
        AccessCheck {
            tool: "workload_plan".into(),
            rule: "queue-use".into(),
            message: "m".into(),
            group: "kueue.x-k8s.io".into(),
            resource: "localqueues".into(),
            subresource: String::new(),
            verb: "use".into(),
            namespace: "team-a".into(),
            name: "crucible-e2e".into(),
        }
    }

    #[test]
    fn the_user_comes_from_kube_user_and_never_names_a_system_identity() {
        let p = principal(json!({"sub": "s", "kube_user": "alice"}));
        assert_eq!(kube_user(&p).unwrap(), "alice");
        for bad in [
            json!({"sub": "s"}),
            json!({"sub": "s", "kube_user": ""}),
            json!({"sub": "s", "kube_user": 7}),
        ] {
            assert!(matches!(
                kube_user(&principal(bad)),
                Err(KubeIdentityError::NoUser)
            ));
        }
        let admin = principal(json!({"sub": "s", "kube_user": "system:admin"}));
        assert!(matches!(
            kube_user(&admin),
            Err(KubeIdentityError::SystemUser(_))
        ));
    }

    #[test]
    fn token_groups_drop_system_groups_and_add_the_authenticated_ones() {
        let p =
            principal(json!({"sub": "s", "kube_groups": ["ml-team", "system:masters", "ml-team"]}));
        assert_eq!(
            token_groups(&p).unwrap(),
            [
                "ml-team",
                "system:authenticated",
                "system:authenticated:oauth"
            ]
        );
        let none = principal(json!({"sub": "s"}));
        assert_eq!(token_groups(&none).unwrap(), AUTHENTICATED);
        let bad = principal(json!({"sub": "s", "kube_groups": "ml-team"}));
        assert!(matches!(token_groups(&bad), Err(KubeIdentityError::Groups)));
    }

    #[test]
    fn openshift_groups_are_the_ones_listing_the_user() {
        let group = |name: &str, users: Value| -> DynamicObject {
            serde_json::from_value(json!({
                "apiVersion": "user.openshift.io/v1", "kind": "Group",
                "metadata": {"name": name}, "users": users,
            }))
            .unwrap()
        };
        let all = [
            group("ml-team", json!(["alice", "bob"])),
            group("ops", json!(["carol"])),
            group("empty", Value::Null),
            group("alicia", json!(["alice2"])),
        ];
        assert_eq!(openshift_groups_of(&all, "alice"), ["ml-team"]);
        assert!(openshift_groups_of(&all, "dave").is_empty());
    }

    #[test]
    fn the_review_asks_about_exactly_the_checked_attributes() {
        let identity = KubeIdentity {
            user: "alice".into(),
            groups: vec!["ml-team".into(), "system:authenticated".into()],
        };
        let review = review_request(&identity, &check());
        assert_eq!(
            serde_json::to_value(&review.spec).unwrap(),
            json!({
                "user": "alice",
                "groups": ["ml-team", "system:authenticated"],
                "resourceAttributes": {
                    "group": "kueue.x-k8s.io", "resource": "localqueues", "verb": "use",
                    "namespace": "team-a", "name": "crucible-e2e",
                },
            })
        );
        let mut cluster_wide = check();
        cluster_wide.group.clear();
        cluster_wide.namespace.clear();
        cluster_wide.name.clear();
        let attrs = serde_json::to_value(
            review_request(&identity, &cluster_wide)
                .spec
                .resource_attributes,
        )
        .unwrap();
        assert_eq!(
            attrs,
            json!({"group": "", "resource": "localqueues", "verb": "use"})
        );
    }
}
