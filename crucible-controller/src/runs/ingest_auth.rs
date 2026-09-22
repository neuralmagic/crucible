//! Auth for the Tier 2 ingest drop-box (Tier 2 ingest). A turn pod authenticates its artifact
//! POST with a projected ServiceAccount token minted for the `crucible-ingest` audience; this module
//! validates that token via the Kubernetes **TokenReview** API and requires three things from the
//! response before a byte is written:
//!
//!   1. the token authenticated at all (`status.authenticated == true`);
//!   2. it was granted the `crucible-ingest` audience (audience-locked → useless against the kube
//!      API or any other service);
//!   3. it belongs to the turn pods' own service account, AND its bound-pod claims
//!      (`authentication.kubernetes.io/pod-name` and `…/pod-uid`) match the pod the request names —
//!      a turn is exactly one pod, so pod-binding *is* turn-scoping. The name alone is not enough:
//!      pod names are deterministic per run and the loop service account can create pods in the
//!      loop namespace, so the UID the controller itself observed on the create response is checked
//!      whenever one is recorded.
//!
//! Nothing is minted, stored, or expired by us: the kubelet rotates the token and the API server
//! invalidates it when the pod dies. The cost is a kube API call per POST, so validations are cached
//! briefly per (token, pod). When the kube API is unreachable we **fail closed** (503) — the turn
//! retries on its own backoff, and the Tier 1 manifest keeps any miss loud.
//!
//! Under hub-spoke dispatch the check is cluster-keyed. A spoke pod's token is issued by its own
//! cluster's API server and can only be TokenReviewed there, and each cluster has its own loop
//! namespace and turn ServiceAccount. The `{pod}` path segment is looked up in the `work_pods`
//! ledger, which has a row for every dispatched pod; the row's cluster selects both the API server
//! the review is created on and the expected (namespace, service account) pair. A pod with no row
//! is validated against the hub, which is the behavior from before spokes existed.
//!
//! This surface lives entirely OUTSIDE the human-facing oauth2-proxy/role stack: it is write-only,
//! never in the OpenAPI-typed SPA client.

use crate::daemon::queue::BoxFuture;
use crate::runs::clusters::{ClusterClients, HUB_CLUSTER};
use crate::runs::workpod::retry::{KubeFailure, classify_chain};
use axum::extract::{FromRef, FromRequestParts, Path};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use k8s_openapi::api::authentication::v1::{
    TokenReview, TokenReviewSpec, TokenReviewStatus, UserInfo,
};
use kube::Api;
use kube::api::PostParams;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

/// How long a successful (token, pod) validation is trusted before another TokenReview is issued.
/// Brief by design — the endpoint sees a handful of POSTs per turn, not a request stream, so a short
/// window collapses the retries of one artifact onto one kube call without meaningfully widening the
/// blast radius of a token the API server has since invalidated.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// How long a rejected (token, pod) is remembered, so repeated invalid tokens trigger at most one
/// TokenReview per TTL. Shorter than the accept TTL because a token that becomes valid shortly
/// after a rejection must not stay rejected for long. Only definitive rejections are cached here,
/// never an unreachable-cluster failure.
const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(30);

/// Maximum remembered rejections, bounding the map's size under many distinct invalid tokens. At
/// the cap, expired entries are dropped and a new rejection is not cached.
const NEGATIVE_CACHE_MAX: usize = 4096;

/// What the turn pods' service account must look like. `namespace` is always required (the token's
/// SA must live in the loop namespace); `name` pins the exact SA when the deployment knows it (the
/// Helm chart sets it), otherwise any SA in the namespace is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedServiceAccount {
    pub(crate) namespace: String,
    pub(crate) name: Option<String>,
}

/// Why a token was rejected — mapped to an HTTP status by the extractor. `Unauthorized` (401) is a
/// definitive "this token is not a valid turn-pod credential for this pod"; `Unavailable` (503) is
/// "the kube API couldn't tell us" (fail closed — the pod retries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestReject {
    Unauthorized(RejectReason),
    Unavailable(String),
}

/// The specific check a rejected token failed, for an honest 401 body + a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// `status.authenticated` was not true.
    Unauthenticated,
    /// The granted audiences did not include `crucible-ingest`.
    Audience,
    /// No user in the review status (an unauthenticated token shape).
    NoUser,
    /// The username was not a `system:serviceaccount:<ns>:<name>` in the expected namespace/name.
    ServiceAccount,
    /// The `authentication.kubernetes.io/pod-name` claim was missing.
    NoPodClaim,
    /// The bound-pod claim did not equal the `{pod}` path segment.
    PodMismatch,
    /// A UID was expected for the pod and the token carries no `…/pod-uid` claim.
    NoUidClaim,
    /// The token's pod-uid claim is not the UID recorded for this pod.
    UidMismatch,
}

impl RejectReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RejectReason::Unauthenticated => "token did not authenticate",
            RejectReason::Audience => "token was not granted the crucible-ingest audience",
            RejectReason::NoUser => "token review returned no user",
            RejectReason::ServiceAccount => "token is not the turn service account",
            RejectReason::NoPodClaim => "token carries no bound-pod claim",
            RejectReason::PodMismatch => "token is bound to a different pod",
            RejectReason::NoUidClaim => "token carries no bound-pod UID claim",
            RejectReason::UidMismatch => "token is bound to a pod with a different UID",
        }
    }
}

/// A cluster's API server could not answer, producing a 503. Distinct from a rejection, and never
/// cached: a transient failure must not reject a valid pod for the negative-cache TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unreachable {
    cluster: String,
    detail: String,
}

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cluster `{}`: {}", self.cluster, self.detail)
    }
}

/// The cluster boundary a validation crosses: a spoke's loop-pod namespace, and the TokenReview
/// itself, both against the pod's home cluster. Production uses [`ClusterKube`]; tests substitute
/// a fake so the routing and accept/reject/fail-closed paths run without a live API server.
pub(crate) trait ClusterTokenReview: Send + Sync {
    /// The namespace a spoke's loop pods run in, declared by its kubeconfig context.
    fn pod_namespace(&self, cluster: &str) -> BoxFuture<Result<String, Unreachable>>;
    /// TokenReview `token` for `audience` against `cluster`'s API server.
    fn review(
        &self,
        cluster: &str,
        token: String,
        audience: String,
    ) -> BoxFuture<Result<TokenReviewStatus, Unreachable>>;
}

/// The production boundary, over the shared per-cluster client registry. A cluster whose client
/// cannot be built (no credentials mounted, unknown name) fails closed like an unreachable API
/// server, never as an accept.
pub(crate) struct ClusterKube {
    clusters: Arc<ClusterClients>,
    hub_namespace: String,
}

impl ClusterKube {
    pub(crate) fn new(clusters: Arc<ClusterClients>, hub_namespace: impl Into<String>) -> Self {
        ClusterKube {
            clusters,
            hub_namespace: hub_namespace.into(),
        }
    }
}

/// Build a cluster-boundary failure, distinguishing a rejected credential from a connectivity
/// failure so a misconfigured spoke is not reported as a network error.
fn unreachable(cluster: &str, what: &str, err: &anyhow::Error) -> Unreachable {
    let detail = match classify_chain(err) {
        Some(KubeFailure::AuthRejected(code)) => {
            format!("{what}: the cluster rejected our credential ({code})")
        }
        Some(KubeFailure::Gone) => format!("{what}: the cluster API answered 404"),
        Some(KubeFailure::Transient) | None => format!("{what}: {err:#}"),
    };
    Unreachable {
        cluster: cluster.to_string(),
        detail,
    }
}

impl ClusterTokenReview for ClusterKube {
    fn pod_namespace(&self, cluster: &str) -> BoxFuture<Result<String, Unreachable>> {
        let clusters = self.clusters.clone();
        let hub_namespace = self.hub_namespace.clone();
        let cluster = cluster.to_string();
        Box::pin(async move {
            clusters
                .pod_namespace(&cluster, &hub_namespace)
                .await
                .map_err(|e| unreachable(&cluster, "resolving the loop namespace", &e))
        })
    }

    fn review(
        &self,
        cluster: &str,
        token: String,
        audience: String,
    ) -> BoxFuture<Result<TokenReviewStatus, Unreachable>> {
        let clusters = self.clusters.clone();
        let cluster = cluster.to_string();
        Box::pin(async move {
            let client = clusters
                .client(&cluster)
                .await
                .map_err(|e| unreachable(&cluster, "building the kube client", &e))?;
            let review = TokenReview {
                metadata: Default::default(),
                spec: TokenReviewSpec {
                    token: Some(token),
                    audiences: Some(vec![audience]),
                },
                status: None,
            };
            let api: Api<TokenReview> = Api::all(client);
            let reviewed = api
                .create(&PostParams::default(), &review)
                .await
                .map_err(|e| unreachable(&cluster, "TokenReview call failed", &e.into()))?;
            reviewed.status.ok_or_else(|| Unreachable {
                cluster,
                detail: "TokenReview returned no status".to_string(),
            })
        })
    }
}

/// A boundary with no cluster: every validation fails closed with 503. Test-only, since
/// [`ClusterKube`] already answers 503 per request when a client cannot be built.
#[cfg(test)]
pub(crate) struct NoClusters;

#[cfg(test)]
impl NoClusters {
    fn no_client(cluster: &str) -> Unreachable {
        Unreachable {
            cluster: cluster.to_string(),
            detail: "the controller has no kube client for TokenReview".to_string(),
        }
    }
}

#[cfg(test)]
impl ClusterTokenReview for NoClusters {
    fn pod_namespace(&self, cluster: &str) -> BoxFuture<Result<String, Unreachable>> {
        let err = Self::no_client(cluster);
        Box::pin(async move { Err(err) })
    }

    fn review(
        &self,
        cluster: &str,
        _token: String,
        _audience: String,
    ) -> BoxFuture<Result<TokenReviewStatus, Unreachable>> {
        let err = Self::no_client(cluster);
        Box::pin(async move { Err(err) })
    }
}

/// The accept cache: each (token, pod) hash expires `ttl` after it is written.
fn accept_cache(ttl: Duration) -> moka::sync::Cache<u64, ()> {
    moka::sync::Cache::builder().time_to_live(ttl).build()
}

/// The reject cache: entries expire `ttl` after write and the map is bounded to
/// [`NEGATIVE_CACHE_MAX`], moka evicting the coldest entries past the cap.
fn reject_cache(ttl: Duration) -> moka::sync::Cache<u64, RejectReason> {
    moka::sync::Cache::builder()
        .max_capacity(NEGATIVE_CACHE_MAX as u64)
        .time_to_live(ttl)
        .build()
}

/// The validator: the cluster boundary, the `work_pods` ledger the pod name is routed through, the
/// expected audience and per-cluster service accounts, and the accept and reject caches.
pub struct IngestValidator {
    kube: Arc<dyn ClusterTokenReview>,
    /// The ledger the `{pod}` segment's home cluster is read from. `None`, and any pod with no
    /// row, resolves to the hub.
    ledger: Option<PgPool>,
    audience: String,
    /// The hub's expected namespace and name. The name is also the fallback for a spoke with no
    /// entry in `spoke_accounts`.
    expected_sa: ExpectedServiceAccount,
    spoke_accounts: BTreeMap<String, String>,
    /// Accept cache: (token, pod) hash → nothing, each entry expiring at [`CACHE_TTL`].
    cache: moka::sync::Cache<u64, ()>,
    /// Reject cache: (token, pod) hash → reason, expiring at [`NEGATIVE_CACHE_TTL`] and bounded to
    /// [`NEGATIVE_CACHE_MAX`] entries (moka evicts under load).
    rejects: moka::sync::Cache<u64, RejectReason>,
}

impl IngestValidator {
    /// A validator over a live cluster boundary. `expected_sa` is the hub's loop namespace and,
    /// optionally, the exact turn service account name; `ledger` resolves each pod's cluster.
    pub(crate) fn new(
        kube: Arc<dyn ClusterTokenReview>,
        ledger: PgPool,
        audience: impl Into<String>,
        expected_sa: ExpectedServiceAccount,
        spoke_accounts: BTreeMap<String, String>,
    ) -> Self {
        IngestValidator {
            kube,
            ledger: Some(ledger),
            audience: audience.into(),
            expected_sa,
            spoke_accounts,
            cache: accept_cache(CACHE_TTL),
            rejects: reject_cache(NEGATIVE_CACHE_TTL),
        }
    }

    /// A validator with no cluster boundary: every validation fails closed with 503. Test-only,
    /// as the production path always uses [`ClusterKube`].
    #[cfg(test)]
    pub(crate) fn unavailable(
        audience: impl Into<String>,
        expected_sa: ExpectedServiceAccount,
    ) -> Self {
        IngestValidator {
            kube: Arc::new(NoClusters),
            ledger: None,
            audience: audience.into(),
            expected_sa,
            spoke_accounts: BTreeMap::new(),
            cache: accept_cache(CACHE_TTL),
            rejects: reject_cache(NEGATIVE_CACHE_TTL),
        }
    }

    /// Pre-seed the accept cache so [`validate`] short-circuits before any kube call. Tests use this
    /// to drive the real ingest router without a live API server (no mock — a genuinely-authorized
    /// token would land the same cache entry the first TokenReview does).
    #[cfg(test)]
    pub(crate) fn preauthorize(&self, token: &str, pod: &str) {
        self.remember(token, &BoundPod::named(pod));
    }

    fn cache_key(token: &str, pod: &BoundPod<'_>) -> u64 {
        // The raw token never lands in the map; a hash of (token, pod, uid) is the key, so a heap
        // dump of the controller doesn't surrender live bearer tokens. The expected UID is part of
        // the key: an entry proven against one UID must never authorize a check against another.
        let mut h = DefaultHasher::new();
        token.hash(&mut h);
        pod.name.hash(&mut h);
        pod.uid.hash(&mut h);
        h.finish()
    }

    fn cached(&self, token: &str, pod: &BoundPod<'_>) -> bool {
        self.cache.get(&Self::cache_key(token, pod)).is_some()
    }

    fn remember(&self, token: &str, pod: &BoundPod<'_>) {
        self.cache.insert(Self::cache_key(token, pod), ());
    }

    /// The remembered rejection for (token, pod, uid), if one is still inside the negative TTL.
    fn cached_reject(&self, token: &str, pod: &BoundPod<'_>) -> Option<RejectReason> {
        self.rejects.get(&Self::cache_key(token, pod))
    }

    fn remember_reject(&self, token: &str, pod: &BoundPod<'_>, reason: RejectReason) {
        self.rejects.insert(Self::cache_key(token, pod), reason);
    }

    /// The cluster `pod` runs on and the UID the controller recorded for it. A pod with no
    /// `work_pods` row resolves to the hub with no UID, the behavior from before spokes existed;
    /// the hub's TokenReview still validates it. A row written before UIDs were recorded also has
    /// none, and is checked by name alone. A ledger error fails closed instead of falling back.
    async fn ledger_pod(&self, pod: &str) -> Result<(String, Option<String>), IngestReject> {
        let Some(pool) = &self.ledger else {
            return Ok((HUB_CLUSTER.to_string(), None));
        };
        match crate::runs::work_pods::get_work_pod(pool, pod).await {
            Ok(Some(row)) => Ok((row.cluster, row.pod_uid)),
            Ok(None) => Ok((HUB_CLUSTER.to_string(), None)),
            Err(e) => Err(IngestReject::Unavailable(format!(
                "work_pods lookup for `{pod}`: {e:#}"
            ))),
        }
    }

    /// What a token from `cluster` must look like: the hub's configured pair, or a spoke's own
    /// namespace from its kubeconfig plus its configured service account.
    async fn expected_for(&self, cluster: &str) -> Result<ExpectedServiceAccount, IngestReject> {
        if cluster == HUB_CLUSTER {
            return Ok(self.expected_sa.clone());
        }
        let namespace = self
            .kube
            .pod_namespace(cluster)
            .await
            .map_err(|u| IngestReject::Unavailable(u.to_string()))?;
        Ok(ExpectedServiceAccount {
            namespace,
            name: self
                .spoke_accounts
                .get(cluster)
                .cloned()
                .or_else(|| self.expected_sa.name.clone()),
        })
    }

    /// Validate `token` for the `{pod}` an ingest POST names, against the cluster and the UID the
    /// `work_pods` ledger recorded for it. `Ok(())` means the POST is authorized; `Err` carries
    /// whether the caller should answer 401 (definitively rejected) or 503 (kube unreachable —
    /// fail closed).
    async fn validate(&self, token: &str, pod: &str) -> Result<(), IngestReject> {
        let (cluster, uid) = self.ledger_pod(pod).await?;
        self.check(
            token,
            &BoundPod {
                name: pod,
                uid: uid.as_deref(),
            },
            &cluster,
        )
        .await
    }

    /// One validation: cache, TokenReview against `cluster`, then the accept/reject matrix.
    async fn check(
        &self,
        token: &str,
        pod: &BoundPod<'_>,
        cluster: &str,
    ) -> Result<(), IngestReject> {
        if token.is_empty() {
            return Err(IngestReject::Unauthorized(RejectReason::Unauthenticated));
        }
        if self.cached(token, pod) {
            return Ok(());
        }
        if let Some(reason) = self.cached_reject(token, pod) {
            return Err(IngestReject::Unauthorized(reason));
        }

        let expected = self.expected_for(cluster).await?;
        let status = self
            .kube
            .review(cluster, token.to_string(), self.audience.clone())
            .await
            .map_err(|u| IngestReject::Unavailable(u.to_string()))?;

        if let Err(reason) = decide(&status, &self.audience, &expected, pod) {
            self.remember_reject(token, pod, reason);
            return Err(IngestReject::Unauthorized(reason));
        }
        self.remember(token, pod);
        Ok(())
    }
}

/// The pod a token must be bound to: the name it was created under and, when the controller
/// recorded one, the UID the API server returned on that create. A `None` UID is a pod the
/// controller never observed a UID for (a row written before UIDs were recorded, a pod with no
/// ledger row at all), which is checked by name alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoundPod<'a> {
    pub(crate) name: &'a str,
    pub(crate) uid: Option<&'a str>,
}

impl<'a> BoundPod<'a> {
    /// A pod with no recorded UID. Only a test builds one directly; production always knows
    /// whether a UID was recorded.
    #[cfg(test)]
    pub(crate) fn named(name: &'a str) -> Self {
        BoundPod { name, uid: None }
    }
}

/// The bound-pod claim key the kube API server stamps into a projected token's TokenReview `extra`.
const POD_NAME_CLAIM: &str = crucible_contract::INGEST_POD_NAME_CLAIM;

/// The bound-pod UID claim, stamped beside the name by every API server that binds a projected
/// token to a pod. The controller observed this UID itself on the create response, so it is the
/// only pod identity a same-named pod cannot forge.
const POD_UID_CLAIM: &str = "authentication.kubernetes.io/pod-uid";

/// The pure validation core over a [`TokenReviewStatus`] — the accept/reject matrix, unit-tested
/// exhaustively without a live API server. Kept separate from [`IngestValidator::validate`] so the
/// kube call is the only untested seam.
fn decide(
    status: &TokenReviewStatus,
    expected_audience: &str,
    expected_sa: &ExpectedServiceAccount,
    pod: &BoundPod<'_>,
) -> Result<(), RejectReason> {
    if status.authenticated != Some(true) {
        return Err(RejectReason::Unauthenticated);
    }
    // The API server echoes the audiences it actually granted; ours must be among them.
    let granted = status.audiences.as_deref().unwrap_or_default();
    if !granted.iter().any(|a| a == expected_audience) {
        return Err(RejectReason::Audience);
    }
    let user = status.user.as_ref().ok_or(RejectReason::NoUser)?;
    check_service_account(user, expected_sa)?;

    let pod_claim = claim(user, POD_NAME_CLAIM).ok_or(RejectReason::NoPodClaim)?;
    if pod_claim != pod.name {
        return Err(RejectReason::PodMismatch);
    }
    if let Some(uid) = pod.uid {
        let uid_claim = claim(user, POD_UID_CLAIM).ok_or(RejectReason::NoUidClaim)?;
        if uid_claim != uid {
            return Err(RejectReason::UidMismatch);
        }
    }
    Ok(())
}

/// One single-valued claim out of a TokenReview's `extra` map.
fn claim<'a>(user: &'a UserInfo, key: &str) -> Option<&'a str> {
    user.extra.as_ref()?.get(key)?.first().map(String::as_str)
}

/// The username of a service-account token is `system:serviceaccount:<namespace>:<name>`. It must be
/// in the expected namespace and, when the deployment pinned one, the exact expected name.
fn check_service_account(
    user: &UserInfo,
    expected: &ExpectedServiceAccount,
) -> Result<(), RejectReason> {
    let username = user
        .username
        .as_deref()
        .ok_or(RejectReason::ServiceAccount)?;
    let rest = username
        .strip_prefix("system:serviceaccount:")
        .ok_or(RejectReason::ServiceAccount)?;
    let (ns, name) = rest.split_once(':').ok_or(RejectReason::ServiceAccount)?;
    if ns != expected.namespace {
        return Err(RejectReason::ServiceAccount);
    }
    if let Some(want) = &expected.name
        && name != want
    {
        return Err(RejectReason::ServiceAccount);
    }
    Ok(())
}

/// The authenticated ingest request: the verified pod (== the token's bound pod == the `{pod}` path
/// segment) and the raw `{kind}` segment the handler parses into an [`crucible_contract::ArtifactKind`].
pub struct IngestAuth {
    pub(crate) pod: String,
    pub(crate) kind: String,
}

impl<S> FromRequestParts<S> for IngestAuth
where
    std::sync::Arc<IngestValidator>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path((pod, kind)) = Path::<(String, String)>::from_request_parts(parts, state)
            .await
            .map_err(|e| e.into_response())?;

        let token = bearer(parts)
            .ok_or_else(|| (StatusCode::UNAUTHORIZED, "missing bearer token").into_response())?;

        let validator = std::sync::Arc::<IngestValidator>::from_ref(state);
        match validator.validate(&token, &pod).await {
            Ok(()) => Ok(IngestAuth { pod, kind }),
            Err(IngestReject::Unauthorized(reason)) => {
                tracing::warn!(pod, reason = reason.as_str(), "ingest token rejected");
                Err((StatusCode::UNAUTHORIZED, reason.as_str()).into_response())
            }
            Err(IngestReject::Unavailable(msg)) => {
                tracing::warn!(pod, error = %msg, "ingest TokenReview unavailable — failing closed");
                Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "token validation unavailable",
                )
                    .into_response())
            }
        }
    }
}

/// Pull the `Authorization: Bearer <token>` value, or `None`.
fn bearer(parts: &Parts) -> Option<String> {
    let raw = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    raw.strip_prefix("Bearer ")
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod test_harness {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A fake cluster boundary: per-cluster namespaces, fixed TokenReview statuses, a set of
    /// clusters whose API is unreachable, and a review-call counter. This is the only faked layer;
    /// the ledger routing, the caches, and the accept/reject checks are the real code, and so is
    /// every caller under test.
    #[derive(Default)]
    pub(crate) struct FakeClusters {
        pub(crate) namespaces: HashMap<String, String>,
        pub(crate) statuses: HashMap<String, TokenReviewStatus>,
        pub(crate) down: Mutex<std::collections::HashSet<String>>,
        pub(crate) reviews: std::sync::atomic::AtomicUsize,
    }

    impl FakeClusters {
        pub(crate) fn reviews(&self) -> usize {
            self.reviews.load(std::sync::atomic::Ordering::SeqCst)
        }

        pub(crate) fn set_down(&self, cluster: &str, down: bool) {
            let mut set = self.down.lock().expect("down lock");
            if down {
                set.insert(cluster.to_string());
            } else {
                set.remove(cluster);
            }
        }

        fn is_down(&self, cluster: &str) -> bool {
            self.down.lock().expect("down lock").contains(cluster)
        }
    }

    impl ClusterTokenReview for FakeClusters {
        fn pod_namespace(&self, cluster: &str) -> BoxFuture<Result<String, Unreachable>> {
            let answer = if self.is_down(cluster) {
                Err(Unreachable {
                    cluster: cluster.to_string(),
                    detail: "connection refused".to_string(),
                })
            } else {
                self.namespaces
                    .get(cluster)
                    .cloned()
                    .ok_or_else(|| Unreachable {
                        cluster: cluster.to_string(),
                        detail: "no credentials".to_string(),
                    })
            };
            Box::pin(async move { answer })
        }

        fn review(
            &self,
            cluster: &str,
            _token: String,
            _audience: String,
        ) -> BoxFuture<Result<TokenReviewStatus, Unreachable>> {
            self.reviews
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let answer = if self.is_down(cluster) {
                Err(Unreachable {
                    cluster: cluster.to_string(),
                    detail: "connection refused".to_string(),
                })
            } else {
                self.statuses
                    .get(cluster)
                    .cloned()
                    .ok_or_else(|| Unreachable {
                        cluster: cluster.to_string(),
                        detail: "no credentials".to_string(),
                    })
            };
            Box::pin(async move { answer })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::ingest_auth::test_harness::FakeClusters;
    use std::collections::{BTreeMap, HashMap};

    fn sa_user(username: &str, pod: Option<&str>) -> UserInfo {
        uid_user(username, pod, None)
    }

    /// A service-account user with both bound-pod claims, either of which may be absent.
    fn uid_user(username: &str, pod: Option<&str>, uid: Option<&str>) -> UserInfo {
        let mut extra: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if let Some(p) = pod {
            extra.insert(POD_NAME_CLAIM.to_string(), vec![p.to_string()]);
        }
        if let Some(u) = uid {
            extra.insert(POD_UID_CLAIM.to_string(), vec![u.to_string()]);
        }
        UserInfo {
            username: Some(username.to_string()),
            extra: if extra.is_empty() { None } else { Some(extra) },
            groups: None,
            uid: None,
        }
    }

    fn status(
        authenticated: bool,
        audiences: &[&str],
        user: Option<UserInfo>,
    ) -> TokenReviewStatus {
        TokenReviewStatus {
            authenticated: Some(authenticated),
            audiences: Some(audiences.iter().map(|a| a.to_string()).collect()),
            error: None,
            user,
        }
    }

    fn expected() -> ExpectedServiceAccount {
        ExpectedServiceAccount {
            namespace: "autoresearch".to_string(),
            name: Some("crucible-turn".to_string()),
        }
    }

    fn good_status(pod: &str) -> TokenReviewStatus {
        status(
            true,
            &["crucible-ingest"],
            Some(sa_user(
                "system:serviceaccount:autoresearch:crucible-turn",
                Some(pod),
            )),
        )
    }

    #[test]
    fn accepts_a_well_formed_pod_bound_ingest_token() {
        assert_eq!(
            decide(
                &good_status("crucible-turn-abc"),
                "crucible-ingest",
                &expected(),
                &BoundPod::named("crucible-turn-abc")
            ),
            Ok(())
        );
    }

    #[test]
    fn rejects_unauthenticated() {
        let s = status(
            false,
            &["crucible-ingest"],
            Some(sa_user(
                "system:serviceaccount:autoresearch:crucible-turn",
                Some("p"),
            )),
        );
        assert_eq!(
            decide(&s, "crucible-ingest", &expected(), &BoundPod::named("p")),
            Err(RejectReason::Unauthenticated)
        );
    }

    #[test]
    fn rejects_wrong_audience() {
        let s = status(
            true,
            &["sts.amazonaws.com"],
            Some(sa_user(
                "system:serviceaccount:autoresearch:crucible-turn",
                Some("p"),
            )),
        );
        assert_eq!(
            decide(&s, "crucible-ingest", &expected(), &BoundPod::named("p")),
            Err(RejectReason::Audience)
        );
    }

    #[test]
    fn rejects_wrong_pod_claim() {
        assert_eq!(
            decide(
                &good_status("pod-A"),
                "crucible-ingest",
                &expected(),
                &BoundPod::named("pod-B")
            ),
            Err(RejectReason::PodMismatch)
        );
    }

    #[test]
    fn rejects_missing_pod_claim() {
        let s = status(
            true,
            &["crucible-ingest"],
            Some(sa_user(
                "system:serviceaccount:autoresearch:crucible-turn",
                None,
            )),
        );
        assert_eq!(
            decide(&s, "crucible-ingest", &expected(), &BoundPod::named("p")),
            Err(RejectReason::NoPodClaim)
        );
    }

    #[test]
    fn rejects_wrong_service_account_name() {
        let s = status(
            true,
            &["crucible-ingest"],
            Some(sa_user(
                "system:serviceaccount:autoresearch:some-other-sa",
                Some("p"),
            )),
        );
        assert_eq!(
            decide(&s, "crucible-ingest", &expected(), &BoundPod::named("p")),
            Err(RejectReason::ServiceAccount)
        );
    }

    #[test]
    fn rejects_wrong_namespace() {
        let s = status(
            true,
            &["crucible-ingest"],
            Some(sa_user(
                "system:serviceaccount:kube-system:crucible-turn",
                Some("p"),
            )),
        );
        assert_eq!(
            decide(&s, "crucible-ingest", &expected(), &BoundPod::named("p")),
            Err(RejectReason::ServiceAccount)
        );
    }

    #[test]
    fn rejects_a_non_service_account_user() {
        let s = status(
            true,
            &["crucible-ingest"],
            Some(sa_user("system:node:worker-1", Some("p"))),
        );
        assert_eq!(
            decide(&s, "crucible-ingest", &expected(), &BoundPod::named("p")),
            Err(RejectReason::ServiceAccount)
        );
    }

    #[test]
    fn rejects_missing_user() {
        let s = status(true, &["crucible-ingest"], None);
        assert_eq!(
            decide(&s, "crucible-ingest", &expected(), &BoundPod::named("p")),
            Err(RejectReason::NoUser)
        );
    }

    #[test]
    fn unpinned_name_accepts_any_sa_in_the_namespace() {
        let any_name = ExpectedServiceAccount {
            namespace: "autoresearch".to_string(),
            name: None,
        };
        let s = status(
            true,
            &["crucible-ingest"],
            Some(sa_user(
                "system:serviceaccount:autoresearch:whatever",
                Some("p"),
            )),
        );
        assert_eq!(
            decide(&s, "crucible-ingest", &any_name, &BoundPod::named("p")),
            Ok(())
        );
    }

    /// The whole point of recording the UID: a pod created with the same deterministic name by
    /// anything other than the controller carries a different UID, and the name check alone would
    /// have accepted it.
    #[test]
    fn a_same_named_pod_with_another_uid_is_refused() {
        let s = status(
            true,
            &["crucible-ingest"],
            Some(uid_user(
                "system:serviceaccount:autoresearch:crucible-turn",
                Some("crucible-run-1"),
                Some("uid-b"),
            )),
        );
        let bound = BoundPod {
            name: "crucible-run-1",
            uid: Some("uid-a"),
        };
        assert_eq!(
            decide(&s, "crucible-ingest", &expected(), &bound),
            Err(RejectReason::UidMismatch)
        );
        assert_eq!(
            decide(
                &s,
                "crucible-ingest",
                &expected(),
                &BoundPod {
                    name: "crucible-run-1",
                    uid: Some("uid-b"),
                }
            ),
            Ok(())
        );
    }

    /// A token with no UID claim cannot prove it is the recorded pod, so an expectation of one is
    /// a rejection rather than a silent fallback to the name.
    #[test]
    fn a_recorded_uid_with_no_uid_claim_is_refused() {
        let s = status(
            true,
            &["crucible-ingest"],
            Some(sa_user(
                "system:serviceaccount:autoresearch:crucible-turn",
                Some("crucible-run-1"),
            )),
        );
        let bound = BoundPod {
            name: "crucible-run-1",
            uid: Some("uid-a"),
        };
        assert_eq!(
            decide(&s, "crucible-ingest", &expected(), &bound),
            Err(RejectReason::NoUidClaim)
        );
    }

    /// Insert a `work_pods` row for `pod` on `cluster`, which is what the validator's ledger
    /// lookup reads.
    async fn dispatched(pool: &sqlx::PgPool, pod: &str, cluster: &str) {
        crate::runs::work_pods::insert_work_pod(
            pool,
            &crate::runs::workpod::NewWorkPod {
                pod_name: pod.to_string(),
                kind: "run".to_string(),
                issue_key: None,
                state: crate::runs::workpod::WorkPodState::Running,
                cost_tag: "turn".to_string(),
                cluster: cluster.to_string(),
            },
        )
        .await
        .expect("insert work pod");
    }

    fn spoke_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(c, sa)| (c.to_string(), sa.to_string()))
            .collect()
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn spoke_pod_validates_against_its_own_cluster(pool: sqlx::PgPool) {
        // The wharf turn service account is in wharf's namespace, a pair the hub expectation
        // (autoresearch/crucible-turn) rejects.
        let wharf_status = status(
            true,
            &["crucible-ingest"],
            Some(sa_user(
                "system:serviceaccount:crucible-loops:wharf-turn",
                Some("crucible-turn-w"),
            )),
        );
        let fake = Arc::new(FakeClusters {
            namespaces: HashMap::from([("wharf".to_string(), "crucible-loops".to_string())]),
            statuses: HashMap::from([
                ("wharf".to_string(), wharf_status.clone()),
                ("hub".to_string(), wharf_status),
            ]),
            ..Default::default()
        });
        dispatched(&pool, "crucible-turn-w", "wharf").await;
        dispatched(&pool, "crucible-turn-h", HUB_CLUSTER).await;

        let v = IngestValidator::new(
            fake.clone(),
            pool.clone(),
            "crucible-ingest",
            expected(),
            spoke_map(&[("wharf", "wharf-turn")]),
        );
        v.validate("tok", "crucible-turn-w")
            .await
            .expect("the spoke's own namespace and service account are accepted");

        // The same token on a hub row is checked against the hub pair and rejected.
        assert_eq!(
            v.validate("tok", "crucible-turn-h").await,
            Err(IngestReject::Unauthorized(RejectReason::ServiceAccount))
        );

        // A spoke with no configured service account falls back to the hub's name, which this
        // token does not match.
        let fallback =
            IngestValidator::new(fake, pool, "crucible-ingest", expected(), BTreeMap::new());
        assert_eq!(
            fallback.validate("tok", "crucible-turn-w").await,
            Err(IngestReject::Unauthorized(RejectReason::ServiceAccount))
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn row_naming_a_cluster_without_credentials_fails_closed(pool: sqlx::PgPool) {
        // The production boundary over a registry with no clusters directory: the client cannot be
        // built, which must fail closed rather than fall back to the hub's expectations.
        dispatched(&pool, "crucible-turn-x", "nowhere").await;
        let v = IngestValidator::new(
            Arc::new(ClusterKube::new(
                Arc::new(ClusterClients::new(None)),
                "autoresearch",
            )),
            pool,
            "crucible-ingest",
            expected(),
            BTreeMap::new(),
        );
        let err = v.validate("tok", "crucible-turn-x").await.unwrap_err();
        assert!(
            matches!(err, IngestReject::Unavailable(ref m) if m.contains("nowhere")),
            "{err:?}"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn unreachable_spoke_is_503_and_is_not_negative_cached(pool: sqlx::PgPool) {
        let fake = Arc::new(FakeClusters {
            namespaces: HashMap::from([("wharf".to_string(), "crucible-loops".to_string())]),
            statuses: HashMap::from([(
                "wharf".to_string(),
                status(
                    true,
                    &["crucible-ingest"],
                    Some(sa_user(
                        "system:serviceaccount:crucible-loops:wharf-turn",
                        Some("crucible-turn-w"),
                    )),
                ),
            )]),
            ..Default::default()
        });
        dispatched(&pool, "crucible-turn-w", "wharf").await;
        let v = IngestValidator::new(
            fake.clone(),
            pool,
            "crucible-ingest",
            expected(),
            spoke_map(&[("wharf", "wharf-turn")]),
        );

        fake.set_down("wharf", true);
        let err = v.validate("tok", "crucible-turn-w").await.unwrap_err();
        assert!(matches!(err, IngestReject::Unavailable(_)), "{err:?}");

        // The 503 was not cached, so the same token is accepted once the spoke answers again.
        fake.set_down("wharf", false);
        v.validate("tok", "crucible-turn-w")
            .await
            .expect("a valid token is accepted after the spoke answers again");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn repeated_invalid_token_triggers_one_token_review_per_negative_ttl(pool: sqlx::PgPool) {
        let fake = Arc::new(FakeClusters {
            namespaces: HashMap::new(),
            statuses: HashMap::from([(
                HUB_CLUSTER.to_string(),
                status(
                    true,
                    &["sts.amazonaws.com"],
                    Some(sa_user(
                        "system:serviceaccount:autoresearch:crucible-turn",
                        Some("crucible-turn-h"),
                    )),
                ),
            )]),
            ..Default::default()
        });
        dispatched(&pool, "crucible-turn-h", HUB_CLUSTER).await;
        let mut v = IngestValidator::new(
            fake.clone(),
            pool,
            "crucible-ingest",
            expected(),
            BTreeMap::new(),
        );
        v.rejects = reject_cache(Duration::from_millis(50));

        for _ in 0..5 {
            assert_eq!(
                v.validate("bad", "crucible-turn-h").await,
                Err(IngestReject::Unauthorized(RejectReason::Audience))
            );
        }
        assert_eq!(
            fake.reviews(),
            1,
            "five rejected requests issue one TokenReview"
        );

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            v.validate("bad", "crucible-turn-h").await,
            Err(IngestReject::Unauthorized(RejectReason::Audience))
        );
        assert_eq!(
            fake.reviews(),
            2,
            "past the TTL the token is reviewed again"
        );
    }

    #[tokio::test]
    async fn no_kube_client_fails_closed_503() {
        let v = IngestValidator::unavailable("crucible-ingest", expected());
        let err = v.validate("some-token", "pod-a").await.unwrap_err();
        assert!(matches!(err, IngestReject::Unavailable(_)));
    }

    /// The drop-box's bound-pod claim is checked against the UID the create response carried, not
    /// the pod name alone: same name, other UID, refused.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn ingest_checks_the_recorded_pod_uid(pool: sqlx::PgPool) {
        let fake = Arc::new(FakeClusters {
            namespaces: HashMap::new(),
            statuses: HashMap::from([(
                HUB_CLUSTER.to_string(),
                status(
                    true,
                    &["crucible-ingest"],
                    Some(uid_user(
                        "system:serviceaccount:autoresearch:crucible-turn",
                        Some("crucible-turn-h"),
                        Some("uid-impostor"),
                    )),
                ),
            )]),
            ..Default::default()
        });
        dispatched(&pool, "crucible-turn-h", HUB_CLUSTER).await;
        crate::runs::work_pods::set_work_pod_uid(&pool, "crucible-turn-h", "uid-real")
            .await
            .expect("record the uid");
        let v = IngestValidator::new(
            fake,
            pool.clone(),
            "crucible-ingest",
            expected(),
            BTreeMap::new(),
        );
        assert_eq!(
            v.validate("tok", "crucible-turn-h").await,
            Err(IngestReject::Unauthorized(RejectReason::UidMismatch)),
            "a same-named pod with another UID is refused"
        );

        // A row with no recorded UID (written before UIDs were, or a pod with no row) still
        // validates by name, so the drop-box keeps working for in-flight pods across the upgrade.
        dispatched(&pool, "crucible-turn-old", HUB_CLUSTER).await;
        let by_name = Arc::new(FakeClusters {
            namespaces: HashMap::new(),
            statuses: HashMap::from([(
                HUB_CLUSTER.to_string(),
                status(
                    true,
                    &["crucible-ingest"],
                    Some(sa_user(
                        "system:serviceaccount:autoresearch:crucible-turn",
                        Some("crucible-turn-old"),
                    )),
                ),
            )]),
            ..Default::default()
        });
        let v = IngestValidator::new(
            by_name,
            pool,
            "crucible-ingest",
            expected(),
            BTreeMap::new(),
        );
        v.validate("tok", "crucible-turn-old")
            .await
            .expect("a row with no recorded UID is checked by name alone");
    }

    #[tokio::test]
    async fn empty_token_is_unauthorized_without_a_kube_call() {
        let v = IngestValidator::unavailable("crucible-ingest", expected());
        let err = v.validate("", "pod-a").await.unwrap_err();
        assert_eq!(
            err,
            IngestReject::Unauthorized(RejectReason::Unauthenticated)
        );
    }
}
