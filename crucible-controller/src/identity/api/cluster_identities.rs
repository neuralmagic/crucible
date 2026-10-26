//! The caller's Kubernetes username on every cluster the deployment knows, and the routes that
//! link, relink, and unlink it. A rule cluster's name is read off the person's SSO login and
//! cannot be changed here; a link cluster's name is only ever that cluster's own answer.

use crate::api::dto::{bad_request, conflict, not_found, unavailable};
use crate::api::state::{ApiState, AppError, ErrorBody, Json};
use crate::identity::auth::{AuthPath, Subject};
use crate::identity::cluster_identity::{self, Person, Resolution, Source};
use crate::identity::cluster_link::{Callback, LinkFlowError};
use crate::identity::kube_links::{self, LinkError, LinkSource};
use anyhow::Context;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use forge::fleet::{ClusterIdentity, SsoClaim};
use serde::{Deserialize, Serialize};
use tower_sessions::Session;
use utoipa::ToSchema;

/// A cluster token whose owner the cluster's API server names.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkBody {
    pub cluster_token: String,
}

pub(crate) fn no_subject() -> Response {
    crate::api::dto::forbidden(
        "this credential names no issuer subject, so it has no user row to link",
    )
}

pub(crate) fn not_from_a_key() -> Response {
    crate::api::dto::forbidden(
        "api keys cannot change their owner's kubernetes username; sign in to link one",
    )
}

/// The SSO claim a rule cluster's usernames come from.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimDto {
    Login,
    Email,
}

/// How a cluster learns who a person is.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IdentityRuleDto {
    /// The username is the person's SSO claim, behind `prefix` when set.
    Claim {
        claim: ClaimDto,
        prefix: Option<String>,
    },
    /// Each person proves their username to the cluster. `oauth` is whether the cluster's OAuth
    /// server can do it; without it the person pastes a token.
    Link { oauth: bool },
}

/// Where the username shown came from.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum IdentitySourceDto {
    /// Read off the SSO login by the cluster's rule.
    Rule,
    /// The cluster's OAuth server signed the person in.
    OpenshiftOauth,
    /// The cluster's TokenReview accepted a token the person pasted.
    TokenReview,
    /// A platform administrator set it; the event log records who.
    Override,
}

/// One cluster's identity for the caller.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct ClusterIdentityDto {
    pub cluster: String,
    pub rule: IdentityRuleDto,
    /// `None` on a link cluster nobody has linked yet.
    pub kube_user: Option<String>,
    pub source: Option<IdentitySourceDto>,
    /// When a link cluster's username was last proven.
    pub linked_at: Option<String>,
}

/// An in-app path a finished link lands on: absolute, same-origin, and short.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(try_from = "String")]
#[schema(value_type = String)]
pub struct ReturnTo(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("return_to {0:?} is not a same-origin path like /consent?id=...")]
pub struct NotSameOrigin(String);

impl TryFrom<String> for ReturnTo {
    type Error = NotSameOrigin;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let ok = raw.starts_with('/')
            && !raw.starts_with("//")
            && raw.len() <= 2048
            && !raw.contains('#')
            && !raw
                .chars()
                .any(|c| c == '\\' || c.is_control() || c.is_whitespace());
        match ok {
            true => Ok(Self(raw)),
            false => Err(NotSameOrigin(raw)),
        }
    }
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkStartBody {
    /// Where the callback sends the browser afterwards; Settings when absent.
    pub return_to: Option<ReturnTo>,
}

/// Where to send the browser to link a cluster through its OAuth server.
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
pub struct LinkStartDto {
    pub authorize_url: String,
}

impl From<&ClusterIdentity> for IdentityRuleDto {
    fn from(rule: &ClusterIdentity) -> Self {
        match rule {
            ClusterIdentity::Link => Self::Link { oauth: false },
            ClusterIdentity::Claim(c) => Self::Claim {
                claim: match c.claim {
                    SsoClaim::Login => ClaimDto::Login,
                    SsoClaim::Email => ClaimDto::Email,
                },
                prefix: (!c.prefix.is_empty()).then(|| c.prefix.clone()),
            },
        }
    }
}

impl From<Source> for IdentitySourceDto {
    fn from(source: Source) -> Self {
        match source {
            Source::Rule => Self::Rule,
            Source::Link(LinkSource::OpenshiftOauth) => Self::OpenshiftOauth,
            Source::Link(LinkSource::TokenReview) => Self::TokenReview,
            Source::Override => Self::Override,
        }
    }
}

/// The login and email the caller's `users` row recorded, `None` without one.
pub(crate) async fn person_row(
    pool: &sqlx::PgPool,
    sub: &str,
) -> anyhow::Result<Option<(String, Option<String>)>> {
    sqlx::query_as("SELECT login, email FROM users WHERE sub = $1")
        .bind(sub)
        .fetch_optional(pool)
        .await
        .context("reading the caller's login")
}

/// Every cluster the deploy profile knows, with its rule. Without a profile only the hub, as `link`.
pub(crate) async fn rules(state: &ApiState) -> anyhow::Result<Vec<(String, ClusterIdentity)>> {
    let profile = state.profile().await?;
    Ok(match profile {
        Some(p) => p
            .cluster_identities()
            .into_iter()
            .map(|(name, rule)| (name.to_string(), rule.clone()))
            .collect(),
        None => vec![(
            crate::runs::clusters::HUB_CLUSTER.to_string(),
            ClusterIdentity::Link,
        )],
    })
}

pub(crate) async fn identity_of(
    state: &ApiState,
    cluster: &str,
    rule: &ClusterIdentity,
    person: Person<'_>,
) -> anyhow::Result<ClusterIdentityDto> {
    let mut dto_rule = IdentityRuleDto::from(rule);
    if let IdentityRuleDto::Link { oauth } = &mut dto_rule {
        *oauth = state
            .cluster_links
            .as_ref()
            .is_some_and(|links| links.offers(cluster));
    }
    let resolved = cluster_identity::resolve(state.db.pool(), cluster, rule, person).await?;
    let (kube_user, source, linked_at) = match resolved {
        Resolution::Known {
            user,
            source,
            linked_at,
        } => (
            Some(user.as_str().to_string()),
            Some(IdentitySourceDto::from(source)),
            linked_at,
        ),
        Resolution::Unlinked | Resolution::Unavailable(_) => (None, None, None),
    };
    Ok(ClusterIdentityDto {
        cluster: cluster.to_string(),
        rule: dto_rule,
        kube_user,
        source,
        linked_at,
    })
}

/// The caller, and the rule of the `link` cluster they named, or the refusal to send instead.
struct LinkTarget {
    sub: String,
    login: String,
    email: Option<String>,
    rule: ClusterIdentity,
}

impl LinkTarget {
    fn person(&self) -> Person<'_> {
        Person {
            sub: &self.sub,
            login: &self.login,
            email: self.email.as_deref(),
        }
    }
}

async fn link_target(
    state: &ApiState,
    subject: &Subject,
    cluster: &str,
) -> Result<Result<LinkTarget, Response>, AppError> {
    let Some(sub) = subject.as_deref() else {
        return Ok(Err(no_subject()));
    };
    let Some((login, email)) = person_row(state.db.pool(), sub).await? else {
        return Ok(Err(no_subject()));
    };
    let Some((_, rule)) = rules(state).await?.into_iter().find(|(c, _)| c == cluster) else {
        return Ok(Err(not_found(format!(
            "cluster {cluster:?} is not one the deploy profile knows"
        ))));
    };
    if let ClusterIdentity::Claim(c) = &rule {
        let claim = match c.claim {
            SsoClaim::Login => "login",
            SsoClaim::Email => "email",
        };
        return Ok(Err(conflict(format!(
            "cluster {cluster} takes usernames from your SSO {claim}; there is nothing to link"
        ))));
    }
    Ok(Ok(LinkTarget {
        sub: sub.to_string(),
        login,
        email,
        rule,
    }))
}

#[utoipa::path(
    get,
    path = "/api/me/cluster-identities",
    responses(
        (status = 200, description = "The caller's username on every cluster the deploy profile knows", body = [ClusterIdentityDto]),
        (status = 403, description = "The caller has no issuer subject", body = ErrorBody)
    )
)]
pub(crate) async fn list_my_cluster_identities(
    State(state): State<ApiState>,
    subject: Subject,
) -> Result<Response, AppError> {
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let Some((login, email)) = person_row(state.db.pool(), sub).await? else {
        return Ok(no_subject());
    };
    let person = Person {
        sub,
        login: &login,
        email: email.as_deref(),
    };
    let mut out = Vec::new();
    for (cluster, rule) in rules(&state).await? {
        out.push(identity_of(&state, &cluster, &rule, person).await?);
    }
    Ok(Json(out).into_response())
}

/// Whether this controller links cluster usernames at all: it serves people's tokens, or links
/// clusters through OAuth. Without either, the list is empty and the writes are not served.
fn link_refused(e: LinkError) -> Result<Response, AppError> {
    match e {
        LinkError::Unreachable { cluster, detail } => {
            tracing::warn!(
                cluster,
                error = detail,
                "linking a kubernetes username failed closed"
            );
            Ok(unavailable("the cluster could not verify the token; retry"))
        }
        LinkError::Store(e) => Err(e.into()),
        e => Ok(bad_request(e.to_string())),
    }
}

#[utoipa::path(
    put,
    path = "/api/me/cluster-identities/{cluster}",
    request_body = LinkBody,
    params(("cluster" = String, Path, description = "The cluster the token was issued by")),
    responses(
        (status = 200, description = "The cluster's TokenReview named the token's owner; it is now the caller's username there", body = ClusterIdentityDto),
        (status = 400, description = "The cluster refused the token, or it names a system identity", body = ErrorBody),
        (status = 403, description = "The caller has no issuer subject, or presented an api key", body = ErrorBody),
        (status = 404, description = "The deploy profile does not know the cluster", body = ErrorBody),
        (status = 409, description = "The cluster's usernames come from the SSO login", body = ErrorBody),
        (status = 503, description = "The cluster could not be asked", body = ErrorBody)
    )
)]
pub(crate) async fn link_by_token(
    State(state): State<ApiState>,
    subject: Subject,
    path: AuthPath,
    Path(cluster): Path<String>,
    Json(body): Json<LinkBody>,
) -> Result<Response, AppError> {
    if path == AuthPath::ApiKey {
        return Ok(not_from_a_key());
    }
    let target = match link_target(&state, &subject, &cluster).await? {
        Ok(t) => t,
        Err(refusal) => return Ok(refusal),
    };
    if let Err(e) = kube_links::link(
        state.db.pool(),
        state.reviewer.as_ref(),
        &target.sub,
        &cluster,
        &body.cluster_token,
        LinkSource::TokenReview,
    )
    .await
    {
        return link_refused(e);
    }
    let dto = identity_of(&state, &cluster, &target.rule, target.person()).await?;
    Ok(Json(dto).into_response())
}

#[utoipa::path(
    delete,
    path = "/api/me/cluster-identities/{cluster}",
    params(("cluster" = String, Path, description = "The cluster to forget the caller's username on")),
    responses(
        (status = 204, description = "The link is gone"),
        (status = 403, description = "The caller has no issuer subject, or presented an api key", body = ErrorBody),
        (status = 404, description = "The deploy profile does not know the cluster, or nothing was linked", body = ErrorBody),
        (status = 409, description = "The cluster's usernames come from the SSO login", body = ErrorBody)
    )
)]
pub(crate) async fn unlink(
    State(state): State<ApiState>,
    subject: Subject,
    path: AuthPath,
    Path(cluster): Path<String>,
) -> Result<Response, AppError> {
    if path == AuthPath::ApiKey {
        return Ok(not_from_a_key());
    }
    let target = match link_target(&state, &subject, &cluster).await? {
        Ok(t) => t,
        Err(refusal) => return Ok(refusal),
    };
    if !kube_links::unlink(state.db.pool(), &target.sub, &cluster).await? {
        return Ok(not_found(format!("nothing is linked on cluster {cluster}")));
    }
    tracing::info!(sub = target.sub, cluster, "kubernetes username unlinked");
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn browser_only() -> Response {
    crate::api::dto::forbidden("linking through a cluster's sign-in needs a browser session")
}

#[utoipa::path(
    post,
    path = "/api/me/cluster-identities/{cluster}/link",
    request_body = LinkStartBody,
    params(("cluster" = String, Path, description = "The cluster to sign in to")),
    responses(
        (status = 200, description = "Send the browser here; the cluster sends it back to the callback", body = LinkStartDto),
        (status = 403, description = "Not a browser session, or no issuer subject", body = ErrorBody),
        (status = 404, description = "The deploy profile does not know the cluster, or it has no OAuth link", body = ErrorBody),
        (status = 409, description = "The cluster's usernames come from the SSO login", body = ErrorBody),
        (status = 503, description = "The cluster's OAuth metadata could not be read", body = ErrorBody)
    )
)]
pub(crate) async fn start_oauth_link(
    State(state): State<ApiState>,
    subject: Subject,
    path: AuthPath,
    session: Session,
    Path(cluster): Path<String>,
    Json(body): Json<LinkStartBody>,
) -> Result<Response, AppError> {
    if path != AuthPath::Session {
        return Ok(browser_only());
    }
    let target = match link_target(&state, &subject, &cluster).await? {
        Ok(t) => t,
        Err(refusal) => return Ok(refusal),
    };
    let Some(links) = state.cluster_links.as_ref() else {
        return Ok(not_found(
            "this controller links no cluster through OAuth; paste a token instead",
        ));
    };
    let (url, mut pending) = match links
        .start(&target.sub, &cluster, jiff::Timestamp::now())
        .await
    {
        Ok(started) => started,
        Err(e @ (LinkFlowError::UnknownCluster { .. } | LinkFlowError::NotOffered { .. })) => {
            return Ok(not_found(e.to_string()));
        }
        Err(e) => {
            tracing::warn!(cluster, error = %e, "starting a cluster link failed");
            return Ok(unavailable(e.to_string()));
        }
    };
    pending.return_to = body.return_to.map(|r| r.0);
    crate::identity::session::start_cluster_link(&session, &pending)
        .await
        .context("parking the cluster link")?;
    Ok(Json(LinkStartDto {
        authorize_url: url.to_string(),
    })
    .into_response())
}

fn land(page: Option<&str>, outcome: &[(&str, &str)]) -> Response {
    let page = page.unwrap_or("/settings");
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(outcome)
        .finish();
    let sep = if page.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{page}{sep}{query}")).into_response()
}

#[utoipa::path(
    get,
    path = "/oauth/cluster-link/{cluster}/callback",
    params(("cluster" = String, Path, description = "The cluster whose sign-in is answering")),
    responses(
        (status = 303, description = "Back to Settings with `linked=<cluster>` or `link_error=<code>`")
    )
)]
pub(crate) async fn oauth_link_callback(
    State(state): State<ApiState>,
    subject: Subject,
    session: Session,
    Path(cluster): Path<String>,
    Query(callback): Query<Callback>,
) -> Result<Response, AppError> {
    let refused = |page: Option<&str>, e: LinkFlowError| {
        tracing::info!(cluster, reason = %e, "cluster link refused");
        land(page, &[("link_error", e.code())])
    };
    let Some(links) = state.cluster_links.as_ref() else {
        return Ok(refused(None, LinkFlowError::NoFlow));
    };
    let pending = crate::identity::session::take_cluster_link(&session)
        .await
        .context("reading the parked cluster link")?;
    let return_to = pending.as_ref().and_then(|p| p.return_to.clone());
    let page = return_to.as_deref();
    let failed = |e: LinkFlowError| Ok(refused(page, e));
    let Some(sub) = subject.as_deref() else {
        return failed(LinkFlowError::NoFlow);
    };
    let token = match links
        .finish(pending, sub, &cluster, callback, jiff::Timestamp::now())
        .await
    {
        Ok(token) => token,
        Err(e) => return failed(e),
    };
    let linked = kube_links::link(
        state.db.pool(),
        state.reviewer.as_ref(),
        sub,
        &cluster,
        token.expose(),
        LinkSource::OpenshiftOauth,
    )
    .await;
    drop(token);
    match linked {
        Ok(_) => Ok(land(page, &[("linked", &cluster)])),
        Err(LinkError::Store(e)) => Err(e.into()),
        Err(LinkError::Unreachable { cluster, detail }) => {
            tracing::warn!(
                cluster,
                error = detail,
                "reviewing a cluster link failed closed"
            );
            Ok(land(page, &[("link_error", "unavailable")]))
        }
        Err(e) => {
            tracing::info!(cluster, reason = %e, "cluster link review refused");
            Ok(land(page, &[("link_error", "review")]))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::state::ApiState;
    use crate::identity::auth::AuthPath;
    use crate::identity::cluster_link::testing::{EXTERNAL, Fleet, authorize_in_browser, fleet};
    use crate::identity::kube_links::testing::Clusters;
    use crate::identity::session::{IdentitySource, Resolved};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct NoSink;
    impl crate::daemon::queue::OverrideSink for NoSink {
        fn submit(&self, _ov: crate::daemon::queue::Override) {}
    }

    const SUB: &str = "sub-alice";

    /// The hub's usernames are SSO logins, k8s's are prefixed emails, ocp links through
    /// its OAuth server.
    fn profile() -> String {
        format!(
            r#"
[cluster]
loop_namespace = "crucible-system"
rig_namespace = "rig"
service_account = "crucible-spoke"
supervisor_image = "registry.example.com/openshell-supervisor:latest"
identity = {{ claim = "login" }}
[image]
loop = "registry.example.com/crucible-loop:latest"
pull_secret = "quay-pull"
[clusters.ocp]
kubeconfig_secret = "ocp-kubeconfig"
identity = "link"
[clusters.k8s]
kubeconfig_secret = "k8s-kubeconfig"
identity = {{ claim = "email", prefix = "oidc:" }}
[cluster_link]
external_url = "{EXTERNAL}"
openshift = ["ocp"]
"#
        )
    }

    struct Harness {
        pool: PgPool,
        state: ApiState,
        store: tower_sessions_sqlx_store::PostgresStore,
        ocp_base: String,
        _dir: tempfile::TempDir,
    }

    struct Reply {
        status: StatusCode,
        body: Value,
        location: Option<String>,
        cookie: Option<String>,
    }

    async fn harness(pool: PgPool) -> Harness {
        crate::identity::oidc::users::record_login(
            &pool,
            SUB,
            "alice",
            Some("alice@example.com"),
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
        let Fleet { links, ocp, dir } = fleet(true).await;
        let path = dir.path().join("profile.toml");
        std::fs::write(&path, profile()).unwrap();
        let mut tokens: Vec<(String, String, String)> = (1..=5)
            .map(|n| {
                (
                    "ocp".to_string(),
                    format!("sha256~for-code-{n}"),
                    "casey".to_string(),
                )
            })
            .collect();
        tokens.push(("ocp".into(), "sha256~pasted".into(), "casey-pasted".into()));
        let reviewer = Clusters::with(
            &tokens
                .iter()
                .map(|(c, t, u)| (c.as_str(), t.as_str(), u.as_str()))
                .collect::<Vec<_>>(),
        );
        let state = ApiState {
            deploy_profile: Some(path),
            reviewer: Arc::new(reviewer),
            cluster_links: Some(Arc::new(links)),
            identities: true,
            ..ApiState::test(crate::Db::new(pool.clone()), Arc::new(NoSink))
        };
        let store = crate::identity::session::store(&pool).await.unwrap();
        Harness {
            pool,
            state,
            store,
            ocp_base: ocp.base,
            _dir: dir,
        }
    }

    impl Harness {
        /// The routes as `human_router` serves them to alice, admitted on `path`.
        fn app(&self, path: AuthPath) -> axum::Router {
            let resolved = Resolved {
                user: Some("alice".to_string()),
                groups: Vec::new(),
                sub: Some(SUB.to_string()),
                source: IdentitySource::Native,
            };
            crate::api::router(self.state.clone())
                .layer(axum::middleware::from_fn(
                    move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                        let resolved = resolved.clone();
                        async move {
                            req.extensions_mut().insert(path);
                            req.extensions_mut().insert(resolved);
                            next.run(req).await
                        }
                    },
                ))
                .layer(crate::identity::session::layer(self.store.clone(), false))
        }

        async fn send(
            &self,
            path: AuthPath,
            method: &str,
            uri: &str,
            cookie: Option<&str>,
            body: Option<Value>,
        ) -> Reply {
            let mut req = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(cookie) = cookie {
                req = req.header(header::COOKIE, cookie);
            }
            let req = req
                .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                .unwrap();
            let res = self.app(path).oneshot(req).await.unwrap();
            let header_of = |name| {
                res.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            let location = header_of(header::LOCATION);
            let cookie =
                header_of(header::SET_COOKIE).and_then(|c| c.split(';').next().map(str::to_string));
            let status = res.status();
            let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap();
            Reply {
                status,
                body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                location,
                cookie,
            }
        }

        async fn identities(&self) -> HashMap<String, Value> {
            let reply = self
                .send(
                    AuthPath::Session,
                    "GET",
                    "/api/me/cluster-identities",
                    None,
                    None,
                )
                .await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
            reply
                .body
                .as_array()
                .unwrap()
                .iter()
                .map(|i| (i["cluster"].as_str().unwrap().to_string(), i.clone()))
                .collect()
        }

        /// Start a ocp link in a fresh browser session and sign in at the cluster: the
        /// session cookie and the callback query the cluster sent the browser back with.
        async fn signed_in_at_ocp(&self) -> (String, String) {
            self.signed_in_at_ocp_from(json!({})).await
        }

        async fn signed_in_at_ocp_from(&self, body: Value) -> (String, String) {
            let started = self
                .send(
                    AuthPath::Session,
                    "POST",
                    "/api/me/cluster-identities/ocp/link",
                    None,
                    Some(body),
                )
                .await;
            assert_eq!(started.status, StatusCode::OK, "{}", started.body);
            let cookie = started.cookie.expect("the link is parked on a session");
            let url = url::Url::parse(started.body["authorize_url"].as_str().unwrap()).unwrap();
            assert!(url.as_str().starts_with(&self.ocp_base));
            let callback = authorize_in_browser(&url).await;
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("code", callback.code.as_deref().unwrap())
                .append_pair("state", callback.state.as_deref().unwrap())
                .finish();
            (cookie, query)
        }

        async fn put_token(&self, path: AuthPath, cluster: &str, token: &str) -> Reply {
            let uri = format!("/api/me/cluster-identities/{cluster}");
            let body = json!({"cluster_token": token});
            self.send(path, "PUT", &uri, None, Some(body)).await
        }

        async fn unlink(&self, path: AuthPath, cluster: &str) -> StatusCode {
            let uri = format!("/api/me/cluster-identities/{cluster}");
            self.send(path, "DELETE", &uri, None, None).await.status
        }

        async fn start(&self, path: AuthPath, cluster: &str) -> StatusCode {
            let uri = format!("/api/me/cluster-identities/{cluster}/link");
            self.send(path, "POST", &uri, None, Some(json!({})))
                .await
                .status
        }

        async fn callback(&self, cluster: &str, cookie: &str, query: &str) -> Reply {
            self.send(
                AuthPath::Session,
                "GET",
                &format!("/oauth/cluster-link/{cluster}/callback?{query}"),
                Some(cookie),
                None,
            )
            .await
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn every_cluster_shows_its_rule_and_the_name_it_resolves_to(pool: PgPool) {
        let h = harness(pool).await;
        let ids = h.identities().await;
        assert_eq!(ids.len(), 3);
        assert_eq!(
            ids["hub"],
            json!({"cluster": "hub", "rule": {"kind": "claim", "claim": "login", "prefix": null},
                   "kube_user": "alice", "source": "rule", "linked_at": null})
        );
        assert_eq!(
            ids["k8s"],
            json!({"cluster": "k8s",
                   "rule": {"kind": "claim", "claim": "email", "prefix": "oidc:"},
                   "kube_user": "oidc:alice@example.com", "source": "rule", "linked_at": null})
        );
        assert_eq!(
            ids["ocp"],
            json!({"cluster": "ocp", "rule": {"kind": "link", "oauth": true},
                   "kube_user": null, "source": null, "linked_at": null})
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn signing_in_at_the_cluster_links_the_name_it_reviews_and_keeps_no_token(pool: PgPool) {
        let h = harness(pool).await;
        let (cookie, query) = h.signed_in_at_ocp().await;
        let done = h.callback("ocp", &cookie, &query).await;
        assert_eq!(done.status, StatusCode::SEE_OTHER);
        assert_eq!(done.location.as_deref(), Some("/settings?linked=ocp"));
        let ocp = &h.identities().await["ocp"];
        assert_eq!(ocp["kube_user"], "casey");
        assert_eq!(ocp["source"], "openshift-oauth");
        assert!(ocp["linked_at"].is_string());
        let stored: Vec<String> = sqlx::query_scalar(
            "SELECT concat_ws(' ', sub, cluster, kube_user, source, linked_at) FROM kube_identity_links",
        )
        .fetch_all(&h.pool)
        .await
        .unwrap();
        assert!(stored.iter().all(|r| !r.contains("sha256~")), "{stored:?}");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_callback_with_a_forged_state_links_nothing(pool: PgPool) {
        let h = harness(pool).await;
        let (cookie, query) = h.signed_in_at_ocp().await;
        let forged = query.replace("state=", "state=forged");
        let done = h.callback("ocp", &cookie, &forged).await;
        assert_eq!(
            done.location.as_deref(),
            Some("/settings?link_error=state-mismatch")
        );
        assert_eq!(h.identities().await["ocp"]["kube_user"], Value::Null);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_callback_on_another_clusters_path_links_nothing(pool: PgPool) {
        let h = harness(pool).await;
        let (cookie, query) = h.signed_in_at_ocp().await;
        let done = h.callback("k8s", &cookie, &query).await;
        assert_eq!(
            done.location.as_deref(),
            Some("/settings?link_error=wrong-cluster")
        );
        let ids = h.identities().await;
        assert_eq!(ids["ocp"]["kube_user"], Value::Null);
        assert_eq!(ids["k8s"]["source"], "rule");
    }

    /// The parked link is spent by its first callback, so replaying the redirect finds nothing.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_replayed_callback_finds_no_link_to_finish(pool: PgPool) {
        let h = harness(pool).await;
        let (cookie, query) = h.signed_in_at_ocp().await;
        let first = h.callback("ocp", &cookie, &query).await;
        assert_eq!(first.location.as_deref(), Some("/settings?linked=ocp"));
        h.send(
            AuthPath::Session,
            "DELETE",
            "/api/me/cluster-identities/ocp",
            None,
            None,
        )
        .await;
        let replay = h.callback("ocp", &cookie, &query).await;
        assert_eq!(
            replay.location.as_deref(),
            Some("/settings?link_error=no-flow")
        );
        assert_eq!(h.identities().await["ocp"]["kube_user"], Value::Null);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_callback_without_a_started_link_links_nothing(pool: PgPool) {
        let h = harness(pool).await;
        let done = h
            .callback("ocp", "crucible_session=none", "code=code-1&state=x")
            .await;
        assert_eq!(
            done.location.as_deref(),
            Some("/settings?link_error=no-flow")
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn unlinking_forgets_the_cluster_and_only_a_link_cluster_unlinks(pool: PgPool) {
        let h = harness(pool).await;
        let (cookie, query) = h.signed_in_at_ocp().await;
        h.callback("ocp", &cookie, &query).await;
        let unlinked = |cluster| h.unlink(AuthPath::Session, cluster);
        assert_eq!(unlinked("ocp").await, StatusCode::NO_CONTENT);
        assert_eq!(h.identities().await["ocp"]["kube_user"], Value::Null);
        assert_eq!(unlinked("ocp").await, StatusCode::NOT_FOUND);
        assert_eq!(unlinked("k8s").await, StatusCode::CONFLICT);
        assert_eq!(unlinked("elsewhere").await, StatusCode::NOT_FOUND);
        assert_eq!(
            h.unlink(AuthPath::ApiKey, "ocp").await,
            StatusCode::FORBIDDEN
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_pasted_token_links_through_the_clusters_token_review(pool: PgPool) {
        let h = harness(pool).await;
        let linked = h.put_token(AuthPath::Session, "ocp", "sha256~pasted").await;
        assert_eq!(linked.status, StatusCode::OK, "{}", linked.body);
        assert_eq!(linked.body["kube_user"], "casey-pasted");
        assert_eq!(linked.body["source"], "token-review");
        for (path, cluster, token, want) in [
            (
                AuthPath::Session,
                "ocp",
                "sha256~nope",
                StatusCode::BAD_REQUEST,
            ),
            (
                AuthPath::Session,
                "k8s",
                "sha256~pasted",
                StatusCode::CONFLICT,
            ),
            (
                AuthPath::Session,
                "elsewhere",
                "sha256~pasted",
                StatusCode::NOT_FOUND,
            ),
            (
                AuthPath::ApiKey,
                "ocp",
                "sha256~pasted",
                StatusCode::FORBIDDEN,
            ),
        ] {
            let reply = h.put_token(path, cluster, token).await;
            assert_eq!(reply.status, want, "{cluster} {token}: {}", reply.body);
        }
        assert_eq!(h.identities().await["ocp"]["kube_user"], "casey-pasted");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_cluster_sign_in_starts_only_from_a_browser_on_a_link_cluster(pool: PgPool) {
        let h = harness(pool).await;
        assert_eq!(
            h.start(AuthPath::ApiKey, "ocp").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(h.start(AuthPath::Jwt, "ocp").await, StatusCode::FORBIDDEN);
        assert_eq!(
            h.start(AuthPath::Session, "k8s").await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            h.start(AuthPath::Session, "hub").await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            h.start(AuthPath::Session, "elsewhere").await,
            StatusCode::NOT_FOUND
        );
    }

    /// The administrator's override is the hub's username and reads as theirs, not the rule's.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_administrators_override_shows_as_such_on_the_hub(pool: PgPool) {
        let h = harness(pool).await;
        crate::identity::kube_identity::set_override(
            &h.pool,
            "alice",
            Some(&crate::identity::kube_identity::KubeUser::parse("alice-admin").unwrap()),
        )
        .await
        .unwrap();
        let ids = h.identities().await;
        assert_eq!(ids["hub"]["kube_user"], "alice-admin");
        assert_eq!(ids["hub"]["source"], "override");
        assert_eq!(ids["k8s"]["source"], "rule");
    }

    /// Linking from the consent page lands back on it, with the outcome appended.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_link_started_elsewhere_lands_back_where_it_started(pool: PgPool) {
        let h = harness(pool).await;
        let (cookie, query) = h
            .signed_in_at_ocp_from(json!({"return_to": "/consent?id=abc"}))
            .await;
        let done = h.callback("ocp", &cookie, &query).await;
        assert_eq!(done.location.as_deref(), Some("/consent?id=abc&linked=ocp"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_return_path_off_this_origin_is_refused(pool: PgPool) {
        let h = harness(pool).await;
        for bad in [
            "//evil.example/x",
            "https://evil.example/",
            "/\\evil.example",
            "settings",
            "/a b",
            "/x#frag",
        ] {
            let reply = h
                .send(
                    AuthPath::Session,
                    "POST",
                    "/api/me/cluster-identities/ocp/link",
                    None,
                    Some(json!({"return_to": bad})),
                )
                .await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad}: {}",
                reply.body
            );
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn without_people_oauth_or_cluster_links_nothing_is_offered(pool: PgPool) {
        let mut h = harness(pool).await;
        h.state = h.state.clone().with_cluster_links(None);
        assert!(!h.state.identities);
        let listed = h
            .send(
                AuthPath::Session,
                "GET",
                "/api/me/cluster-identities",
                None,
                None,
            )
            .await;
        assert_eq!(listed.status, StatusCode::NOT_FOUND);
        assert_eq!(
            h.put_token(AuthPath::Session, "ocp", "sha256~pasted")
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            h.unlink(AuthPath::Session, "ocp").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            h.start(AuthPath::Session, "ocp").await,
            StatusCode::NOT_FOUND
        );
    }

    /// Without people OAuth there is nothing to sign out, and the page is told so.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn mcp_sign_ins_are_off_without_people_oauth(pool: PgPool) {
        let h = harness(pool).await;
        let listed = h
            .send(AuthPath::Session, "GET", "/api/me/mcp-sign-ins", None, None)
            .await;
        assert_eq!(listed.status, StatusCode::OK);
        assert_eq!(listed.body, json!({"enabled": false, "sign_ins": []}));
        let out = h
            .send(
                AuthPath::Session,
                "DELETE",
                "/api/me/mcp-sign-ins",
                None,
                None,
            )
            .await;
        assert_eq!(out.status, StatusCode::NOT_FOUND);
    }
}
