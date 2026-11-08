//! Kubernetes RBAC as the authority for access checks: each [`AccessCheck`] becomes a
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

use crate::access::{AccessCheck, AccessReview, ReviewError, ReviewFuture};
use crate::principal::{Callers, Principal};
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use kube::api::{Api, ApiResource, DynamicObject, ListParams, PostParams};
use kube::core::GroupVersionKind;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Claim naming the caller's Kubernetes user.
pub const USER_CLAIM: &str = "kube_user";
/// The prefix of every Kubernetes identity the crucible issuer mints for runs.
pub const CRUCIBLE_PREFIX: &str = "crucible:";
/// Claim listing the caller's Kubernetes groups, when groups come from the token.
pub const GROUPS_CLAIM: &str = "kube_groups";

/// The group every authenticated user carries; RBAC often binds through it.
const AUTHENTICATED: &str = "system:authenticated";
/// The group OpenShift adds for a user who signed in through its OAuth server.
const OPENSHIFT_OAUTH: &str = "system:authenticated:oauth";

const ALLOW_TTL: Duration = Duration::from_secs(30);
const GROUPS_TTL: Duration = Duration::from_secs(15);
/// Attempts at one review. A retry is taken only when the API server did not answer.
const REVIEW_ATTEMPTS: u32 = 2;
const REVIEW_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Where a caller's groups come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSource {
    /// `user.openshift.io/v1` Group objects listing the user, read live (cached 15 seconds).
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
    #[error("{USER_CLAIM} {user:?} carries the reserved {prefix} prefix")]
    ReservedUser { user: String, prefix: &'static str },
    #[error("a run's {USER_CLAIM} must carry the {prefix} prefix")]
    NotARun { user: String, prefix: &'static str },
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
/// a node, a service account or the cluster itself. A person's user may not carry the `crucible:`
/// prefix and a run's must.
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
    match principal.callers {
        Callers::People if user.starts_with(CRUCIBLE_PREFIX) => {
            return Err(KubeIdentityError::ReservedUser {
                user: user.to_string(),
                prefix: CRUCIBLE_PREFIX,
            });
        }
        Callers::Runs if !user.starts_with(CRUCIBLE_PREFIX) => {
            return Err(KubeIdentityError::NotARun {
                user: user.to_string(),
                prefix: CRUCIBLE_PREFIX,
            });
        }
        _ => {}
    }
    Ok(user.to_string())
}

/// Groups from the token's `kube_groups`, plus `system:authenticated`, without `system:` groups
/// and, for a person, without `crucible:` groups.
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
    let reserved = |g: &str| {
        g.starts_with("system:")
            || (principal.callers == Callers::People && g.starts_with(CRUCIBLE_PREFIX))
    };
    Ok(with_authenticated(
        listed.into_iter().filter(|g| !reserved(g)),
        &[AUTHENTICATED],
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

/// `user`'s groups when they come from OpenShift Group objects: those listing the user, plus the
/// groups OpenShift gives every user signed in through its OAuth server.
pub fn openshift_groups(groups: &[DynamicObject], user: &str) -> Vec<String> {
    with_authenticated(
        openshift_groups_of(groups, user),
        &[AUTHENTICATED, OPENSHIFT_OAUTH],
    )
}

fn with_authenticated(groups: impl IntoIterator<Item = String>, implied: &[&str]) -> Vec<String> {
    let mut all: Vec<String> = groups.into_iter().collect();
    all.extend(implied.iter().map(|g| g.to_string()));
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
    openshift_groups: Mutex<Option<GroupCache>>,
}

/// The Group objects last listed, and when.
struct GroupCache {
    at: Instant,
    items: Arc<Vec<DynamicObject>>,
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
            openshift_groups: Mutex::new(None),
        }
    }

    /// The caller's Kubernetes identity: a run's groups from its token, a person's from the
    /// configured source.
    pub async fn identity(&self, principal: &Principal) -> Result<KubeIdentity, KubeIdentityError> {
        let user = kube_user(principal)?;
        let groups = match (principal.callers, self.groups) {
            (Callers::Runs, _) => token_groups(principal)?,
            (Callers::People, GroupSource::Token) => token_groups(principal)?,
            (Callers::People, GroupSource::OpenShift) => {
                let all = self.all_openshift_groups().await?;
                openshift_groups(&all, &user)
            }
        };
        Ok(KubeIdentity { user, groups })
    }

    async fn all_openshift_groups(&self) -> Result<Arc<Vec<DynamicObject>>, KubeIdentityError> {
        let mut cached = self.openshift_groups.lock().await;
        if let Some(groups) = cached.as_ref()
            && groups.at.elapsed() < GROUPS_TTL
        {
            return Ok(Arc::clone(&groups.items));
        }
        let gvk = GroupVersionKind::gvk("user.openshift.io", "v1", "Group");
        let api: Api<DynamicObject> =
            Api::all_with(self.client.clone(), &ApiResource::from_gvk(&gvk));
        let listed = api
            .list(&ListParams::default())
            .await
            .map_err(KubeIdentityError::GroupLookup)?;
        let items = Arc::new(listed.items);
        *cached = Some(GroupCache {
            at: Instant::now(),
            items: Arc::clone(&items),
        });
        Ok(items)
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
        let allowed = self.create_review(&identity, check).await?;
        tracing::debug!(user = %identity.user, rule = %check.rule, verb = %check.verb,
            resource = %check.resource, namespace = %check.namespace, name = %check.name, allowed,
            "subject access review");
        if allowed {
            self.allowed.insert(key, ()).await;
        }
        Ok(allowed)
    }

    /// Ask the API server, once more when it failed to answer at all.
    async fn create_review(
        &self,
        identity: &KubeIdentity,
        check: &AccessCheck,
    ) -> Result<bool, KubeIdentityError> {
        let api: Api<SubjectAccessReview> = Api::all(self.client.clone());
        let request = review_request(identity, check);
        let mut attempt = 1;
        loop {
            match api.create(&PostParams::default(), &request).await {
                Ok(reviewed) => return Ok(reviewed.status.is_some_and(|s| s.allowed)),
                Err(error) if attempt < REVIEW_ATTEMPTS && unanswered(&error) => {
                    tracing::warn!(error = %error, attempt, user = %identity.user,
                        rule = %check.rule, namespace = %check.namespace,
                        "subject access review unanswered; retrying");
                    attempt += 1;
                    tokio::time::sleep(REVIEW_RETRY_DELAY).await;
                }
                Err(error) => {
                    tracing::warn!(error = %error, user = %identity.user, rule = %check.rule,
                        namespace = %check.namespace, "subject access review failed; refusing");
                    return Err(KubeIdentityError::Review(error));
                }
            }
        }
    }
}

/// Whether the API server failed to answer the review: unreachable, overloaded, or erroring.
fn unanswered(error: &kube::Error) -> bool {
    match error {
        kube::Error::Service(_) | kube::Error::HyperError(_) => true,
        kube::Error::Api(status) => status.code >= 500 || status.code == 429,
        _ => false,
    }
}

impl AccessReview for SubjectAccessReviewer {
    fn review<'a>(&'a self, principal: &'a Principal, check: &'a AccessCheck) -> ReviewFuture<'a> {
        Box::pin(async move { Ok(self.allowed(principal, check).await?) })
    }
}

impl From<KubeIdentityError> for ReviewError {
    fn from(error: KubeIdentityError) -> Self {
        match error {
            KubeIdentityError::NoUser
            | KubeIdentityError::SystemUser(_)
            | KubeIdentityError::ReservedUser { .. }
            | KubeIdentityError::NotARun { .. }
            | KubeIdentityError::Groups => Self::Identity(Box::new(error)),
            KubeIdentityError::GroupLookup(_) | KubeIdentityError::Review(_) => {
                Self::Unavailable(Box::new(error))
            }
        }
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
            or_verbs: Vec::new(),
            namespace: "team-a".into(),
            name: "crucible-e2e".into(),
        }
    }

    #[test]
    fn the_user_comes_from_kube_user_and_never_names_a_system_identity() {
        let p = principal(json!({"sub": "s", "kube_user": "crucible:run:x"}));
        assert_eq!(kube_user(&p).unwrap(), "crucible:run:x");
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
    fn token_groups_drop_system_groups_and_add_only_system_authenticated() {
        let p =
            principal(json!({"sub": "s", "kube_groups": ["ml-team", "system:masters", "ml-team"]}));
        assert_eq!(
            token_groups(&p).unwrap(),
            ["ml-team", "system:authenticated"],
            "an identity provider's token is not an OpenShift OAuth sign-in"
        );
        let none = principal(json!({"sub": "s"}));
        assert_eq!(token_groups(&none).unwrap(), [AUTHENTICATED]);
        let bad = principal(json!({"sub": "s", "kube_groups": "ml-team"}));
        assert!(matches!(token_groups(&bad), Err(KubeIdentityError::Groups)));
    }

    fn person(claims: Value) -> Principal {
        match claims {
            Value::Object(m) => {
                Principal::from_claims(m, crate::principal::Callers::People).unwrap()
            }
            other => panic!("{other}"),
        }
    }

    /// A person's user may not carry the `crucible:` prefix and a run's must.
    #[test]
    fn the_user_prefix_is_refused_both_ways() {
        let run = principal(json!({"sub": "s", "kube_user": "crucible:run:crucible-run-x"}));
        assert_eq!(kube_user(&run).unwrap(), "crucible:run:crucible-run-x");
        let bare = principal(json!({"sub": "s", "kube_user": "alice"}));
        assert!(matches!(
            kube_user(&bare),
            Err(KubeIdentityError::NotARun { .. })
        ));
        let someone = person(json!({"sub": "s", "kube_user": "alice"}));
        assert_eq!(kube_user(&someone).unwrap(), "alice");
        let smuggler = person(json!({"sub": "s", "kube_user": "crucible:run:crucible-run-x"}));
        assert!(matches!(
            kube_user(&smuggler),
            Err(KubeIdentityError::ReservedUser { .. })
        ));
    }

    /// A person's token drops crucible: groups; a run's keeps them, since they are its scope.
    #[test]
    fn crucible_groups_are_a_runs_scope_and_never_a_persons() {
        let run = principal(json!({
            "sub": "s", "kube_user": "crucible:run:crucible-run-x",
            "kube_groups": ["crucible:runs", "crucible:team:ml", "system:masters"],
        }));
        assert_eq!(
            token_groups(&run).unwrap(),
            ["crucible:runs", "crucible:team:ml", "system:authenticated"]
        );
        let person = person(json!({
            "sub": "s", "kube_user": "alice",
            "kube_groups": ["crucible:runs", "crucible:team:ml"],
        }));
        assert_eq!(
            token_groups(&person).unwrap(),
            ["system:authenticated"],
            "a person's issuer cannot hand out a run's scope"
        );
    }

    #[tokio::test]
    async fn a_runs_groups_come_from_its_token_even_on_an_openshift_group_source() {
        let (client, _) = scripted(vec![]);
        let reviewer = SubjectAccessReviewer::new(client, GroupSource::OpenShift);
        let run = principal(json!({
            "sub": "s", "kube_user": "crucible:run:crucible-run-x",
            "kube_groups": ["crucible:runs", "crucible:team:ml"],
        }));
        assert_eq!(
            reviewer.identity(&run).await.unwrap(),
            KubeIdentity {
                user: "crucible:run:crucible-run-x".into(),
                groups: vec![
                    "crucible:runs".into(),
                    "crucible:team:ml".into(),
                    "system:authenticated".into()
                ],
            },
            "the Group objects are never listed for a run"
        );
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
        assert_eq!(
            openshift_groups(&all, "alice"),
            ["ml-team", AUTHENTICATED, OPENSHIFT_OAUTH]
        );
        assert_eq!(
            openshift_groups(&all, "dave"),
            [AUTHENTICATED, OPENSHIFT_OAUTH]
        );
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

    /// A kube client answering each request with the next scripted `(status, body)`, counting how
    /// many it served.
    fn scripted(answers: Vec<(u16, Value)>) -> (kube::Client, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let service = tower::service_fn(move |_request: http::Request<kube::client::Body>| {
            let counter = Arc::clone(&counter);
            let answers = answers.clone();
            async move {
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let (code, body) = answers[index.min(answers.len() - 1)].clone();
                let response = http::Response::builder()
                    .status(code)
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(http_body_util::Full::new(bytes::Bytes::from(
                        body.to_string(),
                    )))
                    .expect("a response");
                Ok::<_, std::convert::Infallible>(response)
            }
        });
        (kube::Client::new(service, "default"), seen)
    }

    fn answer(allowed: bool) -> Value {
        json!({
            "apiVersion": "authorization.k8s.io/v1",
            "kind": "SubjectAccessReview",
            "metadata": {},
            "spec": {},
            "status": {"allowed": allowed},
        })
    }

    fn failure() -> Value {
        json!({
            "apiVersion": "v1",
            "kind": "Status",
            "status": "Failure",
            "message": "etcdserver: request timed out",
            "code": 500,
        })
    }

    fn alice() -> Principal {
        person(json!({"sub": "s", "kube_user": "alice"}))
    }

    #[tokio::test]
    async fn a_review_the_api_server_could_not_answer_is_asked_again() {
        let (client, seen) = scripted(vec![(500, failure()), (200, answer(true))]);
        let reviewer = SubjectAccessReviewer::new(client, GroupSource::Token);
        assert!(reviewer.allowed(&alice(), &check()).await.unwrap());
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_denial_is_an_answer_and_is_not_asked_again() {
        let (client, seen) = scripted(vec![(200, answer(false))]);
        let reviewer = SubjectAccessReviewer::new(client, GroupSource::Token);
        assert!(!reviewer.allowed(&alice(), &check()).await.unwrap());
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_review_the_api_server_forbids_asking_is_not_asked_again() {
        let (client, seen) = scripted(vec![(
            403,
            json!({"apiVersion": "v1", "kind": "Status", "status": "Failure",
                "message": "cannot create subjectaccessreviews", "code": 403}),
        )]);
        let reviewer = SubjectAccessReviewer::new(client, GroupSource::Token);
        assert!(matches!(
            reviewer.allowed(&alice(), &check()).await,
            Err(KubeIdentityError::Review(_))
        ));
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
