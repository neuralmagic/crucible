//! The guard on the controller's HTTP API + debug UI. A request authenticates by session cookie
//! or by bearer; with no expected token and no issuer configured the guard is off (an operator
//! who trusts the network they bound to) and [`crate::serve`] forces a loopback bind, since an
//! open guard has no business answering the world.
//!
//! Which credential proved the caller decides who they may claim to be:
//!
//! ```text
//!   proxy mode
//!     CONTROLLER_PROXY_TOKEN  -> the SSO edge   -> only the headers the edge injects are kept
//!   native mode
//!     crucible_session cookie -> a login this controller ran -> claims off the session
//!     a Keycloak access token -> validated against the issuer's JWKS -> claims off the token
//!   both
//!     CONTROLLER_API_TOKEN    -> machine caller -> configured identity, no groups
//!     any other bearer        -> cluster token  -> users/~ identity, no groups
//! ```
//!
//! In native mode the `X-Auth-Request-*` family is dropped off every request before anything else
//! runs, so nothing a client wrote can reach a handler on any path.
//!
//! What a handler sees is [`Resolved`], stamped as a request extension by [`require_auth`]. The
//! [`Identity`] and [`Groups`] extractors read that extension; they fall back to the asserted
//! headers only where the middleware never ran, which is the unit tests and nothing the network
//! can reach.

#![allow(clippy::disallowed_macros)]

use axum::extract::{FromRef, FromRequestParts, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;

use crate::identity::api_key::constant_time_eq;
use crate::identity::session::IdentitySource;
use crate::identity::session::{Identity, Resolved, asserted_groups, asserted_user};

/// Which identity model the human surface runs. One release carries both so production can flip and
/// flip back; the proxy arm is deleted in the release after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// Identity is the oauth2-proxy sidecar's `X-Auth-Request-*` assertion.
    #[default]
    Proxy,
    /// Identity is a session this controller minted, or a JWT it validated.
    Native,
}

impl AuthMode {
    /// `CONTROLLER_AUTH_MODE`. Unset or unrecognized is `proxy`, the shape every existing deploy
    /// already runs.
    pub fn from_env() -> Self {
        match std::env::var("CONTROLLER_AUTH_MODE")
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str()
        {
            "native" => AuthMode::Native,
            _ => AuthMode::Proxy,
        }
    }
}

/// The header the guard stamps the proven identity into, and the prefix of the family it drops.
const IDENTITY_HEADER: HeaderName = HeaderName::from_static("x-auth-request-user");
const ASSERTED_PREFIX: &str = "x-auth-request-";

/// The `X-Auth-Request-*` headers oauth2-proxy overwrites on every proxied request, and so the only
/// ones that survive the proxy path — the edge forwards any other client header untouched. A
/// deployment's proxy must inject every name listed here.
const EDGE_ASSERTED_HEADERS: [&str; 3] = [
    "x-auth-request-user",
    "x-auth-request-email",
    "x-auth-request-groups",
];

/// The issuer's stable subject for the caller, when the credential that proved them carries one:
/// a native session and a validated bearer JWT do, the header, static-token, and cluster-token
/// paths do not. It is the key everything durable about a person hangs off, so a route that stores
/// or reads one asks for this rather than deriving it from a login.
pub struct Subject(pub(crate) Option<String>);

impl Subject {
    pub(crate) fn as_deref(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Subject {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Subject(
            parts
                .extensions
                .get::<Resolved>()
                .and_then(|r| r.sub.clone()),
        ))
    }
}

/// The groups asserted for the caller. Normalized like the role lists: trimmed, lowercased, and
/// otherwise kept verbatim, so an ownership check can compare exact full paths. Empty on every path
/// [`AuthPath::carries_groups`] does not name, because no credential was checked against a group
/// claim there — which is not the same as the caller being in no groups.
pub struct Groups(pub(crate) Vec<String>);

impl<S: Send + Sync> FromRequestParts<S> for Groups {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(resolved) = parts.extensions.get::<Resolved>() {
            return Ok(Groups(resolved.groups.clone()));
        }
        Ok(Groups(asserted_groups(&parts.headers)))
    }
}

/// Which credential authenticated the request. [`require_auth`] stamps it as a request
/// extension, so a handler can hold a rule to the path that proved the caller rather than
/// inferring one from which headers happen to be present.
///
/// Two distinctions matter. Only the paths [`AuthPath::carries_groups`] names hold validated group
/// claims, so a route that acts on group membership refuses the others outright instead of silently
/// seeing an empty group list. And only the paths [`AuthPath::may_own`] names name a person the
/// controller knows, so a cluster token whose OpenShift username is nobody's SSO login owns nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPath {
    /// The SSO edge's own token: identity and groups are the edge's assertion.
    Edge,
    /// A session this controller minted at `/auth/callback`.
    Session,
    /// The same session after a definitively refused group refresh. The login stands; the groups
    /// are gone and so is every role they or the login carried, until the user signs in again.
    DowngradedSession,
    /// A Keycloak access token the controller validated against the issuer's JWKS.
    Jwt,
    /// `CONTROLLER_API_TOKEN`: a machine caller, with at most a configured identity and no groups.
    StaticToken,
    /// Any other bearer, resolved to a username by the cluster's `users/~`, and matching a `users`
    /// row. No groups.
    ClusterToken,
    /// A valid cluster token whose OpenShift username matches no `users` row, so it names nobody
    /// this controller has an SSO identity for.
    UnknownClusterToken,
    /// An opaque `crk_…` key its owner minted, carrying their login and the groups their last
    /// login stamped. It authenticates only on the MCP surface, which runs its own guard.
    ApiKey,
    /// No expected token configured — the guard is off and the surface is loopback-bound.
    Open,
}

impl AuthPath {
    /// Whether the caller's group list is the issuer's word. False means "no groups were checked",
    /// not "this caller is in no groups".
    pub fn carries_groups(self) -> bool {
        matches!(
            self,
            AuthPath::Edge | AuthPath::Session | AuthPath::Jwt | AuthPath::ApiKey
        )
    }

    /// Whether this credential may own a secret or a schedule at all.
    pub fn may_own(self) -> bool {
        !matches!(
            self,
            AuthPath::UnknownClusterToken | AuthPath::DowngradedSession
        )
    }

    /// The path's name as an audit row spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthPath::Edge => "edge",
            AuthPath::Session => "session",
            AuthPath::DowngradedSession => "downgraded_session",
            AuthPath::Jwt => "jwt",
            AuthPath::StaticToken => "static_token",
            AuthPath::ClusterToken => "cluster_token",
            AuthPath::UnknownClusterToken => "unknown_cluster_token",
            AuthPath::ApiKey => "api_key",
            AuthPath::Open => "open",
        }
    }

    /// Whether the caller holds any role beyond read. False on a downgraded session whatever the
    /// admin and operator lists say: the point of the downgrade is that nothing about this caller's
    /// standing has been confirmed since the refresh was refused.
    pub fn holds_roles(self) -> bool {
        self != AuthPath::DowngradedSession
    }
}

impl<S: Send + Sync> FromRequestParts<S> for AuthPath {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<AuthPath>()
            .copied()
            .unwrap_or(AuthPath::Open))
    }
}

/// A shared bearer the guard compares presented credentials against. Only constructible from a
/// non-empty value, so "configured" and "configured to the empty string" can't be confused.
#[derive(Debug, Clone)]
pub(crate) struct SharedToken(String);

impl SharedToken {
    pub(crate) fn new(raw: impl Into<String>) -> Option<Self> {
        let raw = raw.into();
        (!raw.is_empty()).then_some(SharedToken(raw))
    }

    fn from_env(var: &str) -> Option<Self> {
        std::env::var(var).ok().and_then(SharedToken::new)
    }

    fn matches(&self, presented: &str) -> bool {
        constant_time_eq(presented.as_bytes(), self.0.as_bytes())
    }
}

/// The expected token from `CONTROLLER_API_TOKEN` (`None`/empty = guard off).
pub(crate) fn expected_token() -> Option<SharedToken> {
    SharedToken::from_env("CONTROLLER_API_TOKEN")
}

/// How stale a live session's group list may get before the next request re-reads it from the
/// owner's offline credential.
const DEFAULT_SESSION_GROUP_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);

/// `CONTROLLER_SESSION_GROUP_REFRESH_MINUTES`. Unset, unparseable, or zero is
/// [`DEFAULT_SESSION_GROUP_REFRESH`] — a deployment must not be able to turn the check into a
/// per-request round trip against the issuer by typo.
fn session_group_refresh_interval() -> std::time::Duration {
    std::env::var("CONTROLLER_SESSION_GROUP_REFRESH_MINUTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|m| *m > 0)
        .map(|m| std::time::Duration::from_secs(m * 60))
        .unwrap_or(DEFAULT_SESSION_GROUP_REFRESH)
}

/// What the auth middleware checks against: the mode, the SSO edge's own token, the static machine
/// token, the cluster-token path that resolves any OTHER bearer to an OpenShift username via
/// `users/~`, and (native mode) the issuer the sessions and JWT bearers are proved against.
pub(crate) struct BearerGuard {
    pub(crate) mode: AuthMode,
    pub(crate) expected: Option<SharedToken>,
    pub(crate) proxy: Option<SharedToken>,
    pub(crate) static_identity: Option<HeaderValue>,
    /// `CONTROLLER_DEV_IDENTITY`: the login every request lands as while the guard is open. Only
    /// constructible on an open guard, so it never names a caller on a routable bind.
    pub(crate) dev_identity: Option<HeaderValue>,
    pub(crate) kube: Option<crate::identity::kube_user::KubeUserAuth>,
    pub(crate) oidc: Option<Arc<crate::identity::oidc::OidcProvider>>,
    /// The pool the `users` table is read through, to tell a cluster token that names a known SSO
    /// login from one that names nobody. `None` leaves every cluster token unlinked.
    pub(crate) users: Option<sqlx::PgPool>,
    /// The offline credential a live session's groups are re-read through. `None` — no issuer, no
    /// mounted key — leaves a session's groups exactly as its login stamped them.
    pub(crate) refresh: Option<Arc<crate::identity::oidc::credentials::OwnerRefresh>>,
    /// How stale a session's groups may get before the next request re-reads them.
    pub(crate) refresh_after: std::time::Duration,
}

impl Default for BearerGuard {
    fn default() -> Self {
        BearerGuard {
            mode: AuthMode::default(),
            expected: None,
            proxy: None,
            static_identity: None,
            dev_identity: None,
            kube: None,
            oidc: None,
            users: None,
            refresh: None,
            refresh_after: DEFAULT_SESSION_GROUP_REFRESH,
        }
    }
}

impl BearerGuard {
    /// Read the credentials off the environment.
    pub(crate) fn from_env(
        kube: Option<crate::identity::kube_user::KubeUserAuth>,
        oidc: Option<Arc<crate::identity::oidc::OidcProvider>>,
        users: Option<sqlx::PgPool>,
    ) -> anyhow::Result<Self> {
        let mut guard = BearerGuard::new(
            expected_token(),
            SharedToken::from_env("CONTROLLER_PROXY_TOKEN"),
            std::env::var("CONTROLLER_API_TOKEN_IDENTITY").ok(),
            kube,
            AuthMode::from_env(),
        )?;
        guard.oidc = oidc.clone();
        guard.users = users.clone();
        let keys = crate::identity::oidc::credentials::CredentialKeys::from_env()?;
        guard.refresh = users.and_then(|pool| {
            crate::identity::oidc::credentials::OwnerRefresh::from_parts(pool, oidc, keys)
        });
        guard.refresh_after = session_group_refresh_interval();
        if guard.mode == AuthMode::Native && guard.oidc.is_none() {
            anyhow::bail!(
                "CONTROLLER_AUTH_MODE=native without CONTROLLER_OIDC_ISSUER: native mode has no identity provider to run a login against"
            );
        }
        guard.with_dev_identity(std::env::var("CONTROLLER_DEV_IDENTITY").ok())
    }

    /// Land every request on an open guard as `login`. Refused on a closed guard: a deployment
    /// with a token or an issuer has real callers to tell apart, and would hand this name to all
    /// of them.
    pub(crate) fn with_dev_identity(mut self, login: Option<String>) -> anyhow::Result<Self> {
        let Some(login) = login
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
        else {
            return Ok(self);
        };
        if !self.is_open() {
            anyhow::bail!(
                "CONTROLLER_DEV_IDENTITY is set on a guarded deployment (CONTROLLER_API_TOKEN or CONTROLLER_OIDC_ISSUER is configured), so every caller would land as {login}"
            );
        }
        self.dev_identity =
            Some(HeaderValue::from_str(&login).map_err(|_| {
                anyhow::anyhow!("CONTROLLER_DEV_IDENTITY is not a valid header value")
            })?);
        Ok(self)
    }

    /// Refuse the combinations that would silently hand a machine caller the edge's power to name
    /// a person, and reject an identity the header can't carry at boot instead of per request.
    pub(crate) fn new(
        expected: Option<SharedToken>,
        proxy: Option<SharedToken>,
        static_identity: Option<String>,
        kube: Option<crate::identity::kube_user::KubeUserAuth>,
        mode: AuthMode,
    ) -> anyhow::Result<Self> {
        if mode == AuthMode::Native && proxy.is_some() {
            anyhow::bail!(
                "CONTROLLER_PROXY_TOKEN is set in native mode, where there is no SSO edge to hold it and no header assertion is trusted"
            );
        }
        match (&expected, &proxy) {
            (None, Some(_)) => anyhow::bail!(
                "CONTROLLER_PROXY_TOKEN is set without CONTROLLER_API_TOKEN, so the guard is off and every request would be trusted to name itself"
            ),
            (Some(api), Some(proxy)) if proxy.matches(&api.0) => anyhow::bail!(
                "CONTROLLER_PROXY_TOKEN equals CONTROLLER_API_TOKEN, so any machine caller could assert an identity and its groups"
            ),
            _ => {}
        }
        let static_identity = static_identity
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|v| {
                HeaderValue::from_str(&v).map_err(|_| {
                    anyhow::anyhow!("CONTROLLER_API_TOKEN_IDENTITY is not a valid header value")
                })
            })
            .transpose()?;
        if static_identity.is_some() && proxy.is_none() && mode == AuthMode::Proxy {
            anyhow::bail!(
                "CONTROLLER_API_TOKEN_IDENTITY is set without CONTROLLER_PROXY_TOKEN, so an SSO edge still presenting the API token upstream would land every logged-in user as that identity"
            );
        }
        Ok(BearerGuard {
            mode,
            expected,
            proxy,
            static_identity,
            dev_identity: None,
            kube,
            oidc: None,
            users: None,
            refresh: None,
            refresh_after: DEFAULT_SESSION_GROUP_REFRESH,
        })
    }

    /// No expected token and no issuer: the guard admits everything, which is why [`crate::serve`]
    /// binds loopback. An issuer alone is enough to keep it closed — that deployment has logins.
    pub(crate) fn is_open(&self) -> bool {
        self.expected.is_none() && self.oidc.is_none()
    }

    /// The identity model this deployment runs.
    pub(crate) fn mode(&self) -> AuthMode {
        self.mode
    }

    /// The session source a request authenticated in this mode is bound under.
    fn source(&self) -> IdentitySource {
        match self.mode() {
            AuthMode::Proxy => IdentitySource::Proxy,
            AuthMode::Native => IdentitySource::Native,
        }
    }

    /// Whether the cluster-token username names somebody who has signed in through the issuer. In
    /// proxy mode there are no logins to check against, so every cluster token stays linked and the
    /// path behaves exactly as it did before native mode existed.
    async fn cluster_token_path(&self, user: &str) -> AuthPath {
        if self.mode() == AuthMode::Proxy {
            return AuthPath::ClusterToken;
        }
        let Some(pool) = &self.users else {
            return AuthPath::UnknownClusterToken;
        };
        match crate::identity::oidc::users::is_known_login(pool, user).await {
            Ok(true) => AuthPath::ClusterToken,
            Ok(false) => AuthPath::UnknownClusterToken,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "users lookup failed; the cluster token owns nothing this request");
                AuthPath::UnknownClusterToken
            }
        }
    }
}

/// Middleware: 401 any request that doesn't carry a credential the guard recognizes. With no
/// expected token and no issuer, every request passes, as `CONTROLLER_DEV_IDENTITY` when one is
/// set, and the surface is loopback-bound.
///
/// Which credential it is decides who the caller may claim to be.
///
/// In native mode the whole `X-Auth-Request-*` family is dropped before anything else runs, and the
/// credentials are: the session cookie this controller minted (its claims come off the session, and
/// a non-GET additionally needs a same-origin `Sec-Fetch-Site` or `Origin`, because the cookie is
/// `SameSite=Lax` and every tenant on the cluster's apps wildcard is same-site), a Keycloak access
/// token validated against the issuer's JWKS, the static token, and a cluster token.
///
/// In proxy mode the SSO edge's own token (`CONTROLLER_PROXY_TOKEN`) is the only one that may carry
/// `X-Auth-Request-*`, and only the [`EDGE_ASSERTED_HEADERS`] the edge overwrites after a login.
///
/// On both: the static token gets the configured `CONTROLLER_API_TOKEN_IDENTITY` (no identity
/// configured = anonymous); a cluster token gets the name the API server returned for it
/// (`users/~`). Both drop every `X-Auth-Request-*` header the client wrote first, groups included —
/// nothing that reaches a handler is caller-asserted. A static-token request that arrives wearing
/// edge assertions while an identity is pinned is 401: that is an edge presenting the wrong bearer,
/// not the machine caller. Anything the API server rejects is 401; an API server that can't answer
/// is 503, fail closed, and so is an unreachable issuer.
pub(crate) async fn require_auth(
    State(guard): State<Arc<BearerGuard>>,
    mut req: Request,
    next: Next,
) -> Response {
    let native = guard.mode() == AuthMode::Native;
    if native {
        for name in asserted_headers(req.headers()) {
            req.headers_mut().remove(&name);
        }
    }
    let source = guard.source();

    if guard.is_open() {
        let resolved = match &guard.dev_identity {
            Some(value) => {
                let Ok(name) = value.to_str().map(str::to_string) else {
                    return unauthorized();
                };
                stamp_identity(req.headers_mut(), Some(value.clone()));
                Resolved::named(source, name)
            }
            None if native => Resolved::anonymous(source),
            None => resolved_from_headers(req.headers(), source),
        };
        admit(&mut req, AuthPath::Open, resolved);
        return next.run(req).await;
    }

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .map(str::to_string);

    let Some(presented) = presented else {
        // No bearer. A native deployment's browsers authenticate by the cookie this controller set.
        if !native {
            return unauthorized();
        }
        let session = req.extensions().get::<tower_sessions::Session>().cloned();
        return match native_session(session.clone()).await {
            Ok(Some(claims)) => {
                if !is_safe(req.method()) && !same_origin(&req) {
                    return cross_origin();
                }
                let claims = match session {
                    Some(session) => refresh_session_groups(&guard, &session, claims).await,
                    None => claims,
                };
                let path = if claims.downgraded {
                    AuthPath::DowngradedSession
                } else {
                    AuthPath::Session
                };
                let resolved = Resolved {
                    user: Some(claims.login),
                    groups: claims.groups,
                    sub: Some(claims.sub),
                    source,
                };
                admit(&mut req, path, resolved);
                next.run(req).await
            }
            Ok(None) if wants_page(&req) => login_redirect(&req),
            Ok(None) => unauthorized(),
            Err(response) => response,
        };
    };

    if guard.proxy.as_ref().is_some_and(|p| p.matches(&presented)) {
        keep_only_edge_asserted(req.headers_mut());
        let resolved = resolved_from_headers(req.headers(), source);
        admit(&mut req, AuthPath::Edge, resolved);
        return next.run(req).await;
    }
    if guard
        .expected
        .as_ref()
        .is_some_and(|e| e.matches(&presented))
    {
        if guard.static_identity.is_some() && has_asserted_header(req.headers()) {
            return unauthorized();
        }
        stamp_identity(req.headers_mut(), guard.static_identity.clone());
        let resolved = match &guard.static_identity {
            Some(value) => match value.to_str() {
                Ok(name) => Resolved::named(source, name),
                Err(_) => return unauthorized(),
            },
            None => Resolved::anonymous(source),
        };
        admit(&mut req, AuthPath::StaticToken, resolved);
        return next.run(req).await;
    }

    // A JWT is the issuer's word and never the cluster's, so it is never handed to `users/~`.
    if native && looks_like_jwt(&presented) {
        let Some(oidc) = &guard.oidc else {
            return unauthorized();
        };
        return match oidc.verify_access_token(&presented).await {
            Ok(claims) => {
                stamp_identity(req.headers_mut(), None);
                let resolved = Resolved {
                    user: Some(claims.login),
                    groups: claims.groups,
                    sub: Some(claims.sub),
                    source,
                };
                admit(&mut req, AuthPath::Jwt, resolved);
                next.run(req).await
            }
            Err(crate::identity::oidc::OidcError::Unavailable(why)) => {
                tracing::warn!(error = %why, "jwt bearer check failed closed");
                issuer_unavailable()
            }
            Err(crate::identity::oidc::OidcError::Rejected(why)) => {
                tracing::debug!(reason = %why, "jwt bearer refused");
                unauthorized()
            }
        };
    }

    if let Some(kube) = &guard.kube {
        use crate::identity::kube_user::LookupError;
        match kube.lookup(&presented).await {
            Ok(user) => {
                let Ok(value) = HeaderValue::from_str(&user) else {
                    // A username the header can't carry can't become an identity; fail closed.
                    return unauthorized();
                };
                stamp_identity(req.headers_mut(), Some(value));
                let path = guard.cluster_token_path(&user).await;
                admit(&mut req, path, Resolved::named(source, user));
                return next.run(req).await;
            }
            Err(LookupError::Unauthorized) => return unauthorized(),
            Err(LookupError::Unavailable(why)) => {
                tracing::warn!(error = %why, "cluster-token bearer check failed closed");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"status":"error","error":"the cluster API could not verify the bearer; retry"}"#,
                )
                    .into_response();
            }
        }
    }
    unauthorized()
}

/// What [`require_auth_or_api_key`] needs to run either guard. Which one runs is decided by the
/// credential the caller presented, never by the route.
#[derive(Clone)]
pub(crate) struct HumanOrKeyGuard {
    pub(crate) guard: Arc<BearerGuard>,
    pub(crate) pool: sqlx::PgPool,
}

/// Middleware: admit a request on the human credential set, or on a minted API key.
///
/// A `crk_` bearer goes to [`require_api_key`] and nowhere else. It never falls back to the human
/// guard, so a wrong or revoked key is refused rather than retried as some weaker identity — the
/// fallback is the only way this could widen anything, so there isn't one. Anything else reaches
/// [`require_auth`] exactly as before. Neither guard's logic moves; this only chooses between them,
/// which is what lets an agent hold one long-lived credential for both the MCP surface and the API.
pub(crate) async fn require_auth_or_api_key(
    State(both): State<HumanOrKeyGuard>,
    req: Request,
    next: Next,
) -> Response {
    if presented_api_key(req.headers()) {
        return require_api_key(State(both.pool), req, next).await;
    }
    require_auth(State(both.guard), req, next).await
}

/// Whether the caller's bearer even claims to be an API key, which is what picks the guard. A
/// malformed or revoked key still lands on the key guard, so it is told what is wrong with it.
fn presented_api_key(headers: &header::HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(crate::identity::api_key::looks_like_key)
}

/// Middleware: admit a request only on an API key its owner minted.
///
/// Deliberately not part of [`require_auth`]. The surfaces this guards are reached by agents
/// holding one long-lived credential, and none of the human paths — the session cookie, the edge's
/// header assertions, the static machine token, a cluster token — mean anything there. Keeping the
/// two guards apart means a bug in either cannot widen the other, and it is the same shape
/// `/metrics` and the ingest drop-box already use: merged outside the human guard,
/// carrying its own credential check.
///
/// What it stamps is what every handler already reads, so nothing downstream needs to know an API
/// key was involved: the owner's login, their subject, and the groups their last login recorded.
pub async fn require_api_key(
    State(pool): State<sqlx::PgPool>,
    mut req: Request,
    next: Next,
) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty());
    let Some(presented) = presented else {
        return key_refused("this surface needs an api key as its bearer");
    };
    let authenticated = match crate::identity::api_key::verify(&pool, presented).await {
        Ok(authenticated) => authenticated,
        Err(refusal) => {
            tracing::debug!(reason = %refusal, "api key refused");
            return key_refused(&refusal.to_string());
        }
    };

    // The client wrote no identity that survives: every X-Auth-Request-* header goes, exactly as
    // native mode drops them, so the key is the only thing naming this caller.
    for name in asserted_headers(req.headers()) {
        req.headers_mut().remove(&name);
    }
    let resolved = Resolved {
        user: Some(authenticated.login),
        groups: authenticated.groups,
        sub: Some(authenticated.sub),
        source: IdentitySource::Native,
    };
    admit(&mut req, AuthPath::ApiKey, resolved);

    // Stamped after the response, off the request path: a key is entitled to its request whether or
    // not the bookkeeping write lands.
    let response = next.run(req).await;
    tokio::spawn(async move { crate::identity::api_key::touch(&pool, &authenticated.id).await });
    response
}

/// A refusal naming what is wrong with the presented key. Unlike [`unauthorized`], it says which
/// of the four things happened, because all four are the caller's to fix and none of them tell an
/// attacker anything they could not learn by presenting the key again.
fn key_refused(why: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"status": "error", "error": why}).to_string(),
    )
        .into_response()
}

/// Stamp what the guard proved onto the request. The two extensions always travel together: a
/// handler that sees one and not the other would be reading half a decision.
fn admit(req: &mut Request, path: AuthPath, resolved: Resolved) {
    req.extensions_mut().insert(path);
    req.extensions_mut().insert(resolved);
}

/// The identity an oauth2-proxy edge asserted, read off the headers it injects.
fn resolved_from_headers(headers: &HeaderMap, source: IdentitySource) -> Resolved {
    Resolved {
        user: asserted_user(headers),
        groups: asserted_groups(headers),
        sub: None,
        source,
    }
}

/// Re-read a live session's groups from its owner's offline credential, at most once per
/// [`BearerGuard::refresh_after`].
///
/// Three answers, and they are deliberately not symmetric:
/// * the issuer answered — the session takes the groups it just asserted;
/// * the issuer could not be reached — the session keeps the groups it had, and the attempt is
///   stamped so an outage costs one round trip per interval and not one per request;
/// * the issuer refused, or there is no credential to spend — the session is downgraded: its
///   groups are dropped and every role it carried is gone until the user signs in again.
async fn refresh_session_groups(
    guard: &BearerGuard,
    session: &tower_sessions::Session,
    claims: crate::identity::session::NativeClaims,
) -> crate::identity::session::NativeClaims {
    use crate::identity::oidc::credentials::RefreshOutcome;
    let Some(refresh) = &guard.refresh else {
        return claims;
    };
    if claims.downgraded {
        return claims;
    }
    let now = jiff::Timestamp::now();
    if !stale(&claims.groups_at, now, guard.refresh_after) {
        return claims;
    }
    let mut next = claims.clone();
    next.groups_at = now.to_string();
    match refresh.refresh(&claims.sub).await {
        Ok(RefreshOutcome::Claims(fresh)) => next.groups = fresh.groups,
        Ok(RefreshOutcome::Absent) => {
            tracing::info!(login = %claims.login, "session downgraded: no offline credential to re-read groups from");
            next.groups = Vec::new();
            next.downgraded = true;
        }
        Err(crate::identity::oidc::OidcError::Unavailable(why)) => {
            tracing::warn!(login = %claims.login, error = %why, "session group refresh deferred");
        }
        Err(crate::identity::oidc::OidcError::Rejected(why)) => {
            tracing::warn!(login = %claims.login, reason = %why, "session downgraded: the group refresh was refused");
            next.groups = Vec::new();
            next.downgraded = true;
        }
    }
    if let Err(e) = crate::identity::session::restamp_native(session, &next).await {
        // The session row is unchanged, so the next request tries again. What this request sees is
        // still the fresher answer.
        tracing::error!(error = %e, "restamping the refreshed session claims");
    }
    next
}

/// Whether a stamped instant is older than `max_age`. An unparseable or missing stamp is stale:
/// a session whose groups have no provenance has to prove them again.
fn stale(at: &str, now: jiff::Timestamp, max_age: std::time::Duration) -> bool {
    let Ok(at) = at.parse::<jiff::Timestamp>() else {
        return true;
    };
    let Ok(span) = jiff::SignedDuration::try_from(max_age) else {
        return true;
    };
    match at.checked_add(span) {
        Ok(expires) => expires <= now,
        Err(_) => true,
    }
}

/// The native claims on the request's session. A store that cannot answer is a 500, never a 401:
/// the SPA reads 401 as "sign in again", and a database blip must not log everybody out.
#[allow(clippy::result_large_err)]
async fn native_session(
    session: Option<tower_sessions::Session>,
) -> Result<Option<crate::identity::session::NativeClaims>, Response> {
    let Some(session) = session else {
        return Ok(None);
    };
    crate::identity::session::native_claims(&session)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "loading the session identity");
            (StatusCode::INTERNAL_SERVER_ERROR, "session store error").into_response()
        })
}

/// Whether the method only reads. The CSRF check applies to everything else.
fn is_safe(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Whether a cookie-authenticated write came from this origin.
///
/// `Sec-Fetch-Site: same-origin` is the answer every current browser gives for the SPA's own
/// fetches. `same-site` is deliberately NOT accepted: the cluster's apps wildcard makes every other
/// tenant same-site. Without the metadata header at all (a non-browser client), an `Origin` that
/// matches the request's own authority is the fallback, and a request carrying neither is refused.
fn same_origin(req: &Request) -> bool {
    let header_str = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    if let Some(site) = header_str("sec-fetch-site") {
        return site.eq_ignore_ascii_case("same-origin");
    }
    let Some(origin) = header_str("origin") else {
        return false;
    };
    let Some(authority) = header_str("x-forwarded-host").or_else(|| header_str("host")) else {
        return false;
    };
    // The scheme is stripped rather than checked: the deployment terminates TLS at a route, so the
    // request's own scheme is http whatever the browser used.
    let origin_authority = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    origin_authority.eq_ignore_ascii_case(authority)
}

/// Whether the presented bearer is shaped like a JWT (three non-empty dot-separated segments).
/// A cluster token (`sha256~…`) never is, which is what keeps the two bearer paths from trying
/// each other's credentials.
pub(crate) fn looks_like_jwt(token: &str) -> bool {
    let mut segments = token.split('.');
    let three = [segments.next(), segments.next(), segments.next()];
    segments.next().is_none()
        && three
            .iter()
            .all(|s| s.is_some_and(|s| !s.is_empty() && !s.contains('~')))
}

fn cross_origin() -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"status":"error","error":"a cookie-authenticated write must come from this origin"}"#,
    )
        .into_response()
}

fn issuer_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"status":"error","error":"the identity provider could not verify the bearer; retry"}"#,
    )
        .into_response()
}

fn has_asserted_header(headers: &HeaderMap) -> bool {
    headers
        .keys()
        .any(|name| name.as_str().starts_with(ASSERTED_PREFIX))
}

fn asserted_headers(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .keys()
        .filter(|name| name.as_str().starts_with(ASSERTED_PREFIX))
        .cloned()
        .collect()
}

/// Drop every `X-Auth-Request-*` header the client wrote, then stamp the identity this path proved.
fn stamp_identity(headers: &mut HeaderMap, identity: Option<HeaderValue>) {
    for name in asserted_headers(headers) {
        headers.remove(&name);
    }
    if let Some(value) = identity {
        headers.insert(IDENTITY_HEADER, value);
    }
}

/// Drop the asserted headers the edge does not inject: those reach the upstream exactly as the
/// client wrote them.
fn keep_only_edge_asserted(headers: &mut HeaderMap) {
    for name in asserted_headers(headers) {
        if !EDGE_ASSERTED_HEADERS.contains(&name.as_str()) {
            headers.remove(&name);
        }
    }
}

/// A browser asking for a page rather than a client asking for data: a safe method, outside
/// `/api`, negotiating HTML. Those get sent to sign in; everything else keeps the 401 the SPA and
/// CLI clients already act on.
fn wants_page(req: &Request) -> bool {
    is_safe(req.method())
        && !req.uri().path().starts_with("/api")
        && req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("text/html"))
}

fn login_redirect(req: &Request) -> Response {
    let rd = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let encoded: String = url::form_urlencoded::byte_serialize(rd.as_bytes()).collect();
    Redirect::to(&format!("/auth/login?rd={encoded}")).into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"status":"error","error":"missing or wrong controller bearer token"}"#,
    )
        .into_response()
}

/// The two-tier role a caller holds. Admins are implicitly operators — any route gated on
/// `Role::Operator` passes for both. Kept as a closed enum (not a raw string) so the whoami wire
/// type and the guards can't drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Operator,
    Viewer,
}

/// The role whitelists: GitHub logins allowed to hit admin- or operator-gated routes. Shared as
/// `State` so individual routes or middleware can opt into the check. Empty list ⇒ locked-closed
/// (nobody holds that role). The admin list wins over the operator list — a login in both is an
/// admin, not merely an operator.
#[derive(Debug, Clone)]
pub struct Roles {
    admins: Arc<Vec<String>>,
    operators: Arc<Vec<String>>,
    /// IdP groups whose members hold `Operator` — write access by team membership instead of a
    /// hand-maintained login list. Matched against [`Groups`] verbatim or by the asserted
    /// group's last `/`-segment, so a configured `team-x` matches an IdP-shaped `/groups/team-x`.
    operator_groups: Arc<Vec<String>>,
}

fn normalize(logins: Vec<String>) -> Vec<String> {
    logins
        .into_iter()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .collect()
}

impl Roles {
    pub fn new(admins: Vec<String>, operators: Vec<String>, operator_groups: Vec<String>) -> Self {
        Roles {
            admins: Arc::new(normalize(admins)),
            operators: Arc::new(normalize(operators)),
            operator_groups: Arc::new(normalize(operator_groups)),
        }
    }

    /// The role for an identity: `Admin` if their login is in the admin list (wins), else
    /// `Operator` if in the operator list or any asserted group is a configured operator group,
    /// else `Viewer` (including anonymous callers). Group membership alone never grants admin.
    pub(crate) fn role(&self, identity: &Identity, groups: &Groups) -> Role {
        let Some(user) = identity.as_deref() else {
            return Role::Viewer;
        };
        if self.admins.iter().any(|a| a.eq_ignore_ascii_case(user)) {
            Role::Admin
        } else if self.operators.iter().any(|o| o.eq_ignore_ascii_case(user))
            || groups.0.iter().any(|g| self.is_operator_group(g))
        {
            Role::Operator
        } else {
            Role::Viewer
        }
    }

    /// Whether one asserted group grants operator: exact match, or the group's last `/`-segment
    /// (IdPs assert path-shaped groups like `/groups/team-x`; operators configure `team-x`).
    ///
    /// The tail match lives here and nowhere else. [`Groups`] keeps the full asserted paths, so a
    /// check that asks "does this caller own `group:/groups/team-x`" compares whole paths and can
    /// never be widened by a configured short name.
    fn is_operator_group(&self, asserted: &str) -> bool {
        let tail = asserted.rsplit('/').next().unwrap_or(asserted);
        self.operator_groups
            .iter()
            .any(|cfg| cfg == asserted || cfg == tail)
    }

    fn is_admin(&self, identity: &Identity) -> bool {
        // Admin never comes from a group, so no Groups needed on this path.
        self.role(identity, &Groups(Vec::new())) == Role::Admin
    }

    /// The admin login whitelist (normalized: trimmed, lowercased, de-blanked). Read-only view for
    /// the `/admin` access panel (`GET /api/access`) — the guards decide with `is_admin`, not this.
    pub(crate) fn admins(&self) -> &[String] {
        &self.admins
    }

    /// The operator login whitelist (normalized). Same read-only-projection role as [`Roles::admins`].
    pub(crate) fn operators(&self) -> &[String] {
        &self.operators
    }

    fn is_operator(&self, identity: &Identity, groups: &Groups) -> bool {
        matches!(self.role(identity, groups), Role::Admin | Role::Operator)
    }
}

fn forbidden(who: &str, tier: &str) -> Response {
    let body = serde_json::json!({
        "error": format!("{who} is not in the {tier} whitelist")
    });
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_default(),
    )
        .into_response()
}

/// An axum extractor that asserts the caller is an admin. 403 with JSON body when not. Pair with
/// `Roles` in state; any route that extracts `AdminGuard` is admin-gated. Money + config routes
/// (ScopeNow, the autopilot kill switch) stay admin-only.
pub struct AdminGuard;

impl<S: Send + Sync> FromRequestParts<S> for AdminGuard
where
    Roles: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Ok(identity) = Identity::from_request_parts(parts, state).await;
        let Ok(path) = AuthPath::from_request_parts(parts, state).await;
        let roles = Roles::from_ref(state);
        if path.holds_roles() && roles.is_admin(&identity) {
            Ok(AdminGuard)
        } else {
            let who = identity.as_deref().unwrap_or("anonymous");
            Err(forbidden(who, "admin"))
        }
    }
}

/// An axum extractor that asserts the caller is an operator or an admin. 403 with JSON body when
/// not. Pair with `Roles` in state; curation routes (park/unpark/bump) are operator-gated.
pub struct OperatorGuard;

impl<S: Send + Sync> FromRequestParts<S> for OperatorGuard
where
    Roles: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Ok(identity) = Identity::from_request_parts(parts, state).await;
        let Ok(groups) = Groups::from_request_parts(parts, state).await;
        let Ok(path) = AuthPath::from_request_parts(parts, state).await;
        let roles = Roles::from_ref(state);
        if path.holds_roles() && roles.is_operator(&identity, &groups) {
            Ok(OperatorGuard)
        } else {
            let who = identity.as_deref().unwrap_or("anonymous");
            Err(forbidden(who, "operator"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_groups_grant_operator_never_admin() {
        let roles = Roles::new(vec!["will".into()], vec![], vec!["platform-devs".into()]);
        let id = Identity(Some("alice".into()));
        let member = Groups(vec!["platform-devs".into()]);
        assert_eq!(roles.role(&id, &member), Role::Operator);
        // IdPs assert path-shaped groups; the configured tail matches.
        let pathy = Groups(vec!["/groups/platform-devs".into()]);
        assert_eq!(roles.role(&id, &pathy), Role::Operator);
        let outsider = Groups(vec!["/groups/other-team".into()]);
        assert_eq!(roles.role(&id, &outsider), Role::Viewer);
        // Group membership never elevates to admin; the admin list still wins for its user.
        assert_eq!(
            roles.role(&Identity(Some("will".into())), &member),
            Role::Admin
        );
        // Anonymous stays viewer regardless of asserted groups.
        assert_eq!(roles.role(&Identity(None), &member), Role::Viewer);
    }

    /// The tail match is an operator-role convenience. What an ownership check reads is the
    /// extracted list, and that keeps whole paths — trimmed and lowercased, never shortened.
    #[tokio::test]
    async fn extracted_groups_keep_whole_paths() {
        let req = axum::http::Request::builder()
            .header("x-auth-request-groups", " /Groups/Team-X , ,/groups/sre ")
            .body(())
            .expect("request");
        let (mut parts, _) = req.into_parts();
        let Ok(groups) = Groups::from_request_parts(&mut parts, &()).await;
        assert_eq!(groups.0, vec!["/groups/team-x", "/groups/sre"]);

        let roles = Roles::new(vec![], vec![], vec!["team-x".into()]);
        assert_eq!(
            roles.role(&Identity(Some("alice".into())), &groups),
            Role::Operator
        );
        assert!(
            !groups.0.iter().any(|g| g == "team-x"),
            "the configured short name must not appear in the caller's groups"
        );
    }

    #[test]
    fn shared_token_needs_a_value_and_matches_exactly() {
        assert!(SharedToken::new("").is_none(), "empty is not configured");
        let want = SharedToken::new("s3cr3t").expect("token");
        assert!(want.matches("s3cr3t"));
        assert!(!want.matches("wrong"));
        assert!(!want.matches("s3cr3t2"), "prefix match is not a match");
        assert!(!want.matches(""));
    }

    #[tokio::test]
    async fn identity_prefers_user_header_and_ignores_blank_values() {
        let extract = |req: axum::http::Request<()>| async move {
            let (mut parts, _) = req.into_parts();
            Identity::from_request_parts(&mut parts, &())
                .await
                .expect("infallible")
        };

        let req = axum::http::Request::builder()
            .header("x-auth-request-user", "wren")
            .header("x-auth-request-email", "me@wren.com")
            .body(())
            .expect("request");
        assert_eq!(extract(req).await.as_deref(), Some("wren"));

        let req = axum::http::Request::builder()
            .header("x-auth-request-user", "  ")
            .header("x-auth-request-email", "me@wren.com")
            .body(())
            .expect("request");
        assert_eq!(
            extract(req).await.as_deref(),
            Some("me@wren.com"),
            "blank user falls back to email"
        );

        let req = axum::http::Request::builder().body(()).expect("request");
        assert!(extract(req).await.as_deref().is_none());
    }

    #[test]
    fn constant_time_eq_matches_plain_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    fn identity(user: &str) -> Identity {
        Identity(Some(user.to_string()))
    }

    #[test]
    fn admin_list_wins_over_operator_list() {
        let roles = Roles::new(vec!["alice".to_string()], vec!["alice".to_string()], vec![]);
        assert_eq!(
            roles.role(&identity("alice"), &Groups(Vec::new())),
            Role::Admin
        );
    }

    #[test]
    fn admin_implies_operator() {
        let roles = Roles::new(vec!["alice".to_string()], vec![], vec![]);
        assert!(roles.is_operator(&identity("alice"), &Groups(Vec::new())));
        assert!(roles.is_admin(&identity("alice")));
    }

    #[test]
    fn operator_is_not_admin() {
        let roles = Roles::new(vec![], vec!["bob".to_string()], vec![]);
        assert_eq!(
            roles.role(&identity("bob"), &Groups(Vec::new())),
            Role::Operator
        );
        assert!(roles.is_operator(&identity("bob"), &Groups(Vec::new())));
        assert!(!roles.is_admin(&identity("bob")));
    }

    #[test]
    fn unknown_and_anonymous_are_viewers() {
        let roles = Roles::new(vec!["alice".to_string()], vec!["bob".to_string()], vec![]);
        assert_eq!(
            roles.role(&identity("mallory"), &Groups(Vec::new())),
            Role::Viewer
        );
        assert_eq!(
            roles.role(&Identity(None), &Groups(Vec::new())),
            Role::Viewer
        );
    }

    #[test]
    fn empty_lists_lock_closed() {
        let roles = Roles::new(vec![], vec![], vec![]);
        assert_eq!(
            roles.role(&identity("alice"), &Groups(Vec::new())),
            Role::Viewer
        );
        assert!(!roles.is_admin(&identity("alice")));
        assert!(!roles.is_operator(&identity("alice"), &Groups(Vec::new())));
    }

    #[test]
    fn login_match_is_case_insensitive() {
        let roles = Roles::new(vec![], vec!["Bob".to_string()], vec![]);
        assert_eq!(
            roles.role(&identity("bob"), &Groups(Vec::new())),
            Role::Operator
        );
    }

    mod bearer_guard {
        use super::*;
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use axum::routing::get;
        use tower::util::ServiceExt;

        /// A router whose one handler echoes the Identity and Groups the middleware let through,
        /// so each test asserts exactly what the role lists and ownership checks would later see.
        fn app(guard: BearerGuard) -> axum::Router {
            axum::Router::new()
                .route(
                    "/probe",
                    get(|identity: Identity, groups: Groups| async move {
                        format!(
                            "{}|{}",
                            identity.as_deref().unwrap_or("anonymous"),
                            groups.0.join(",")
                        )
                    }),
                )
                .layer(axum::middleware::from_fn_with_state(
                    Arc::new(guard),
                    require_auth,
                ))
        }

        /// The same router, echoing the asserted header NAMES that survived instead of the values.
        fn header_echo_app(guard: BearerGuard) -> axum::Router {
            axum::Router::new()
                .route(
                    "/probe",
                    get(|headers: axum::http::HeaderMap| async move {
                        headers
                            .keys()
                            .map(|k| k.as_str().to_string())
                            .filter(|k| k.starts_with(ASSERTED_PREFIX))
                            .collect::<Vec<_>>()
                            .join(",")
                    }),
                )
                .layer(axum::middleware::from_fn_with_state(
                    Arc::new(guard),
                    require_auth,
                ))
        }

        /// A stand-in API server: one good token, everything else 401.
        pub(super) async fn kube_auth() -> crate::identity::kube_user::KubeUserAuth {
            let app = axum::Router::new().route(
                "/apis/user.openshift.io/v1/users/~",
                get(|headers: axum::http::HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default();
                    if auth == "Bearer sha256~mine" {
                        (
                            StatusCode::OK,
                            r#"{"metadata":{"name":"wynn"}}"#.to_string(),
                        )
                    } else {
                        (StatusCode::UNAUTHORIZED, String::new())
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let url = format!("http://{}", listener.local_addr().expect("addr"));
            tokio::spawn(async move {
                axum::serve(listener, app).await.expect("serve");
            });
            crate::identity::kube_user::KubeUserAuth::new(
                &url,
                std::path::Path::new("/nonexistent"),
            )
            .expect("client")
        }

        async fn call(app: axum::Router, req: HttpRequest<Body>) -> (StatusCode, String) {
            let res = app.oneshot(req).await.expect("infallible");
            let status = res.status();
            let body = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .expect("body");
            (status, String::from_utf8_lossy(&body).into_owned())
        }

        /// Every asserted header a client could write, on one request.
        fn req(auth_header: Option<&str>, asserted: &[(&str, &str)]) -> HttpRequest<Body> {
            let mut b = HttpRequest::get("/probe");
            if let Some(a) = auth_header {
                b = b.header("authorization", a);
            }
            for (name, value) in asserted {
                b = b.header(*name, *value);
            }
            b.body(Body::empty()).expect("request")
        }

        fn token(raw: &str) -> Option<SharedToken> {
            SharedToken::new(raw)
        }

        fn static_only() -> BearerGuard {
            BearerGuard {
                expected: token("s3cr3t"),
                ..BearerGuard::default()
            }
        }

        const CLAIMS: [(&str, &str); 3] = [
            ("x-auth-request-user", "admin"),
            ("x-auth-request-email", "admin@example.com"),
            ("x-auth-request-groups", "/groups/finance,/groups/sre"),
        ];

        fn pinned() -> BearerGuard {
            BearerGuard {
                expected: token("s3cr3t"),
                proxy: token("edge"),
                static_identity: Some(HeaderValue::from_static("crucible-cd")),
                ..BearerGuard::default()
            }
        }

        /// The static token is a machine credential: it names nobody, so the request lands with the
        /// configured identity and NO groups.
        #[tokio::test]
        async fn the_static_token_carries_the_configured_identity_and_no_groups() {
            let (status, body) = call(app(pinned()), req(Some("Bearer s3cr3t"), &[])).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "crucible-cd|");
        }

        /// An edge presenting the static token instead of its own (a skewed proxy, or a client
        /// imitating one) would otherwise land every logged-in human as the pinned machine
        /// identity, whatever role that name holds. 401 instead.
        #[tokio::test]
        async fn a_pinned_identity_refuses_a_request_wearing_edge_assertions() {
            for asserted in CLAIMS {
                let (status, _) =
                    call(app(pinned()), req(Some("Bearer s3cr3t"), &[asserted])).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "header {}", asserted.0);
            }
        }

        /// With no identity configured the static token is anonymous — a viewer, not whoever the
        /// caller typed into the header.
        #[tokio::test]
        async fn the_static_token_without_a_configured_identity_is_anonymous() {
            let (status, body) =
                call(app(static_only()), req(Some("Bearer s3cr3t"), &CLAIMS)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "anonymous|");
        }

        /// A header the guard doesn't know about yet must not survive either: the family goes, not
        /// a hand-maintained list of three names.
        #[tokio::test]
        async fn the_whole_asserted_header_family_is_dropped() {
            let guard = BearerGuard {
                expected: token("s3cr3t"),
                ..BearerGuard::default()
            };
            let (status, body) = call(
                header_echo_app(guard),
                req(
                    Some("Bearer s3cr3t"),
                    &[
                        ("x-auth-request-preferred-username", "admin"),
                        ("x-auth-request-access-token", "gho_whatever"),
                    ],
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "", "no x-auth-request-* header reaches the handler");
        }

        /// Each credential stamps the path it authenticated on, which is what lets a route hold a
        /// rule to a path rather than guessing from the headers it can see. The static token's
        /// stamp is the one the secrets registry refuses `group:` owners on.
        #[tokio::test]
        async fn every_path_stamps_which_credential_authenticated() {
            fn path_echo_app(guard: BearerGuard) -> axum::Router {
                axum::Router::new()
                    .route(
                        "/probe",
                        get(|path: AuthPath| async move { format!("{path:?}") }),
                    )
                    .layer(axum::middleware::from_fn_with_state(
                        Arc::new(guard),
                        require_auth,
                    ))
            }

            let cases = [
                ("Bearer edge", "Edge"),
                ("Bearer s3cr3t", "StaticToken"),
                ("Bearer sha256~mine", "ClusterToken"),
            ];
            for (bearer, expected) in cases {
                let guard = BearerGuard {
                    kube: Some(kube_auth().await),
                    ..pinned()
                };
                let (status, body) = call(path_echo_app(guard), req(Some(bearer), &[])).await;
                assert_eq!(status, StatusCode::OK, "{bearer}");
                assert_eq!(body, expected, "{bearer}");
            }

            // No expected token: the guard is off, and the surface is loopback-bound.
            let (status, body) = call(path_echo_app(BearerGuard::default()), req(None, &[])).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "Open");
        }

        /// The SSO edge's own token is the one credential that may name a person, because the edge
        /// overwrites the identity headers after a login.
        #[tokio::test]
        async fn the_proxy_token_keeps_the_edges_assertion() {
            let (status, body) = call(app(pinned()), req(Some("Bearer edge"), &CLAIMS)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "admin|/groups/finance,/groups/sre");
        }

        /// oauth2-proxy overwrites only the headers it is configured to inject and forwards the
        /// rest of the client's request untouched, so anything outside that set is still the
        /// client's word on the proxy path.
        #[tokio::test]
        async fn the_proxy_token_drops_headers_the_edge_does_not_inject() {
            let (status, body) = call(
                header_echo_app(pinned()),
                req(
                    Some("Bearer edge"),
                    &[
                        ("x-auth-request-user", "alice"),
                        ("x-auth-request-preferred-username", "admin"),
                        ("x-auth-request-access-token", "gho_whatever"),
                    ],
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "x-auth-request-user");
        }

        #[tokio::test]
        async fn without_kube_auth_a_foreign_bearer_is_401() {
            let (status, _) = call(app(static_only()), req(Some("Bearer sha256~mine"), &[])).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn a_missing_or_malformed_bearer_is_401() {
            for header in [None, Some("s3cr3t"), Some("Bearer "), Some("Bearer wrong")] {
                let (status, _) = call(app(static_only()), req(header, &[])).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "header {header:?}");
            }
        }

        #[tokio::test]
        async fn a_cluster_token_resolves_to_the_api_servers_identity() {
            let guard = BearerGuard {
                expected: token("s3cr3t"),
                kube: Some(kube_auth().await),
                ..BearerGuard::default()
            };
            let (status, body) = call(app(guard), req(Some("Bearer sha256~mine"), &[])).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "wynn|", "identity comes from users/~, not the client");
        }

        /// The whole point of stripping: a valid cluster token must not carry someone ELSE's
        /// asserted name — or any groups — past the guard.
        #[tokio::test]
        async fn a_cluster_token_cannot_smuggle_an_identity_or_groups() {
            let guard = BearerGuard {
                expected: token("s3cr3t"),
                kube: Some(kube_auth().await),
                ..BearerGuard::default()
            };
            let (status, body) = call(app(guard), req(Some("Bearer sha256~mine"), &CLAIMS)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "wynn|");
        }

        #[tokio::test]
        async fn a_rejected_cluster_token_is_401() {
            let guard = BearerGuard {
                expected: token("s3cr3t"),
                kube: Some(kube_auth().await),
                ..BearerGuard::default()
            };
            let (status, _) = call(app(guard), req(Some("Bearer sha256~stolen"), &[])).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn an_unreachable_api_server_fails_closed_as_503() {
            let guard = BearerGuard {
                expected: token("s3cr3t"),
                kube: Some(
                    crate::identity::kube_user::KubeUserAuth::new(
                        "http://127.0.0.1:9",
                        std::path::Path::new("/nonexistent"),
                    )
                    .expect("client"),
                ),
                ..BearerGuard::default()
            };
            let (status, _) = call(app(guard), req(Some("Bearer sha256~mine"), &[])).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        }

        /// A proxy token equal to the API token would make the whole distinction a no-op, and a
        /// proxy token with the guard off would trust every caller to name itself. Neither boots.
        #[test]
        fn misconfigured_credentials_refuse_to_build() {
            // `BearerGuard` holds live credentials and so has no Debug; unwrap the refusal by hand.
            let refusal = |guard: anyhow::Result<BearerGuard>| match guard {
                Ok(_) => panic!("expected a refusal"),
                Err(e) => e.to_string(),
            };
            assert!(
                refusal(BearerGuard::new(
                    token("same"),
                    token("same"),
                    None,
                    None,
                    AuthMode::Proxy
                ))
                .contains("equals CONTROLLER_API_TOKEN")
            );
            assert!(
                refusal(BearerGuard::new(
                    None,
                    token("edge"),
                    None,
                    None,
                    AuthMode::Proxy
                ))
                .contains("without CONTROLLER_API_TOKEN")
            );
            assert!(
                refusal(BearerGuard::new(
                    token("s3cr3t"),
                    token("edge"),
                    Some("bad\nname".into()),
                    None,
                    AuthMode::Proxy
                ))
                .contains("not a valid header value")
            );
            // A pinned identity with no edge token to tell the edge apart is the skew that lands
            // every logged-in user on that identity.
            assert!(
                refusal(BearerGuard::new(
                    token("s3cr3t"),
                    None,
                    Some("crucible-cd".into()),
                    None,
                    AuthMode::Proxy
                ))
                .contains("without CONTROLLER_PROXY_TOKEN")
            );
        }

        #[test]
        fn a_blank_configured_identity_is_no_identity() {
            let guard = BearerGuard::new(
                token("s3cr3t"),
                None,
                Some("  ".into()),
                None,
                AuthMode::Proxy,
            )
            .expect("guard builds");
            assert!(guard.static_identity.is_none());
            assert!(!guard.is_open());
            assert!(
                BearerGuard::new(None, None, None, None, AuthMode::Proxy)
                    .expect("guard builds")
                    .is_open()
            );
        }

        /// Guard off is the loopback-only trusted-network mode: no credential to sort callers by,
        /// so headers pass exactly as they arrive.
        #[tokio::test]
        async fn no_expected_token_still_means_open() {
            let (status, body) = call(app(BearerGuard::default()), req(None, &[])).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "anonymous|");
        }

        /// A dev identity names every open-guard caller, and whatever the client asserted is
        /// dropped first, so a browser and a curl that writes the edge's headers land the same.
        #[tokio::test]
        async fn an_open_guard_with_a_dev_identity_names_every_caller() {
            let guard = BearerGuard::default()
                .with_dev_identity(Some(" wren ".into()))
                .expect("an open guard takes a dev identity");
            let app = app(guard);
            let (status, body) = call(app.clone(), req(None, &[])).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "wren|");
            let (status, body) = call(app, req(Some("Bearer anything"), &CLAIMS)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "wren|");
        }

        #[test]
        fn a_dev_identity_is_refused_on_a_guarded_deployment() {
            let refusal = match static_only().with_dev_identity(Some("wren".into())) {
                Ok(_) => panic!("expected a refusal"),
                Err(e) => e.to_string(),
            };
            assert!(refusal.contains("guarded deployment"), "{refusal}");
        }

        #[test]
        fn a_blank_dev_identity_is_none_and_a_bad_one_fails_boot() {
            let guard = BearerGuard::default()
                .with_dev_identity(Some("  ".into()))
                .expect("blank is unset");
            assert!(guard.dev_identity.is_none());
            assert!(
                BearerGuard::default()
                    .with_dev_identity(Some("wr\nen".into()))
                    .is_err()
            );
        }
    }

    /// Native mode: identity is a cookie this controller minted or a JWT it validated, and nothing
    /// a client wrote in the `X-Auth-Request-*` family is read on any path.
    mod native {
        use super::bearer_guard::kube_auth;
        use super::*;
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use axum::routing::{get, post};
        use sqlx::PgPool;
        use tower::util::ServiceExt;

        /// An agent holding a minted key reaches the API with it, the same credential that reaches
        /// the MCP surface. Nothing about the route decides this; the bearer does.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn a_minted_key_authenticates_on_the_human_surface(pool: PgPool) {
            let now = jiff::Timestamp::now();
            crate::identity::oidc::users::record_login(&pool, "sub-alice", "alice", None, now)
                .await
                .expect("login");
            let minted = crate::identity::api_key::mint(&pool, "sub-alice", "laptop", None)
                .await
                .expect("mint");
            let app = app(&pool, native_guard()).await;

            let res = app
                .clone()
                .oneshot(
                    HttpRequest::get("/probe")
                        .header(header::AUTHORIZATION, format!("Bearer {}", minted.secret))
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("infallible");
            assert_eq!(res.status(), StatusCode::OK);
            let body = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .expect("body");
            let body = String::from_utf8(body.to_vec()).expect("utf8");
            assert!(body.starts_with("alice|"), "{body}");
            assert!(
                body.contains("ApiKey"),
                "the key path is what admitted it: {body}"
            );
        }

        /// A key that does not verify is refused as a key. Falling back to the human guard would
        /// hand a revoked key whatever weaker identity that guard would have allowed.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn a_bad_key_is_refused_rather_than_retried_as_a_weaker_identity(pool: PgPool) {
            let app = app(&pool, native_guard()).await;
            let res = app
                .oneshot(
                    HttpRequest::get("/probe")
                        .header(header::AUTHORIZATION, "Bearer crk_nope_nope")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("infallible");
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        }

        /// The guarded surface plus an unguarded `/establish` that mints a native session, mounted
        /// exactly as `human_router` mounts them: sessions outside the guard, login routes beside it.
        /// A browser that lands on the app with no session is sent to sign in, carrying where it
        /// was going; API calls and non-HTML clients keep the 401 the SPA and CLIs act on.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn an_anonymous_page_load_is_sent_to_login(pool: PgPool) {
            let app = app(&pool, native_guard()).await;
            let get = |path: &str, accept: &str| {
                HttpRequest::get(path)
                    .header(header::ACCEPT, accept)
                    .body(Body::empty())
                    .expect("request")
            };
            for (path, want) in [
                ("/", "/auth/login?rd=%2F"),
                (
                    "/runs/abc?tab=flow",
                    "/auth/login?rd=%2Fruns%2Fabc%3Ftab%3Dflow",
                ),
            ] {
                let res = app
                    .clone()
                    .oneshot(get(path, "text/html,application/xhtml+xml"))
                    .await
                    .expect("infallible");
                assert_eq!(res.status(), StatusCode::SEE_OTHER, "{path}");
                assert_eq!(
                    res.headers()
                        .get(header::LOCATION)
                        .and_then(|v| v.to_str().ok()),
                    Some(want),
                    "{path}"
                );
            }
            for (path, accept) in [
                ("/api/whoami", "text/html"),
                ("/probe", "application/json"),
                ("/probe", "*/*"),
            ] {
                let (status, _, _) = call(&app, get(path, accept)).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {accept}");
            }
            let (status, _, _) = call(
                &app,
                HttpRequest::post("/write")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "a write never redirects");
        }

        async fn app(pool: &PgPool, guard: BearerGuard) -> axum::Router {
            let store = crate::identity::session::store(pool)
                .await
                .expect("session store");
            axum::Router::new()
                .route(
                    "/probe",
                    get(
                        |identity: Identity, groups: Groups, path: AuthPath| async move {
                            format!(
                                "{}|{}|{path:?}",
                                identity.as_deref().unwrap_or("anonymous"),
                                groups.0.join(",")
                            )
                        },
                    ),
                )
                .route("/write", post(|| async { "written" }))
                .route(
                    "/headers",
                    get(|headers: HeaderMap| async move {
                        headers
                            .keys()
                            .map(|k| k.as_str().to_string())
                            .filter(|k| k.starts_with(ASSERTED_PREFIX))
                            .collect::<Vec<_>>()
                            .join(",")
                    }),
                )
                .layer(axum::middleware::from_fn_with_state(
                    HumanOrKeyGuard {
                        guard: Arc::new(guard),
                        pool: pool.clone(),
                    },
                    require_auth_or_api_key,
                ))
                .route(
                    "/establish",
                    get(|session: tower_sessions::Session| async move {
                        crate::identity::session::establish_native(
                            &session,
                            &crate::identity::session::NativeClaims {
                                sub: "sub-alice".to_string(),
                                login: "alice".to_string(),
                                email: Some("alice@example.com".to_string()),
                                groups: vec!["/groups/team-x".to_string()],
                                groups_at: jiff::Timestamp::now().to_string(),
                                downgraded: false,
                            },
                            None,
                        )
                        .await
                        .expect("establish");
                        "signed in"
                    }),
                )
                .layer(crate::identity::session::layer(store, false))
        }

        fn native_guard() -> BearerGuard {
            let mut guard = BearerGuard::new(
                SharedToken::new("s3cr3t"),
                None,
                Some("crucible-cd".into()),
                None,
                AuthMode::Native,
            )
            .expect("guard builds");
            // An issuer the guard never reaches in these tests; its presence is what makes the
            // deployment a native one rather than an open one.
            guard.oidc = Some(Arc::new(
                crate::identity::oidc::OidcProvider::new(crate::identity::oidc::OidcCfg {
                    issuer: "http://127.0.0.1:9/realms/none".to_string(),
                    client_id: "crucible-controller".to_string(),
                    client_secret: None,
                    redirect_url: "http://localhost/auth/callback".to_string(),
                    scopes: vec!["openid".to_string()],
                    device_client_id: Some("crucible-cli".to_string()),
                    post_logout_redirect: None,
                })
                .expect("provider"),
            ));
            guard
        }

        async fn call(
            app: &axum::Router,
            req: HttpRequest<Body>,
        ) -> (StatusCode, String, Option<String>) {
            let res = app.clone().oneshot(req).await.expect("infallible");
            let status = res.status();
            let cookie = res
                .headers()
                .get(header::SET_COOKIE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.split(';').next().unwrap_or_default().to_string());
            let body = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .expect("body");
            (status, String::from_utf8_lossy(&body).into_owned(), cookie)
        }

        async fn sign_in(app: &axum::Router) -> String {
            let (status, _, cookie) = call(
                app,
                HttpRequest::get("/establish")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            cookie.expect("the login set a session cookie")
        }

        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn a_session_cookie_authenticates_and_carries_its_validated_claims(pool: PgPool) {
            let app = app(&pool, native_guard()).await;
            let cookie = sign_in(&app).await;
            let (status, body, _) = call(
                &app,
                HttpRequest::get("/probe")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "alice|/groups/team-x|Session");

            // No cookie, no bearer, and the guard is closed.
            let (status, _, _) = call(
                &app,
                HttpRequest::get("/probe")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }

        /// The `SameSite=Lax` cookie rides along on a cross-site write, and every other tenant on
        /// the cluster's apps wildcard is same-SITE, so only same-ORIGIN counts.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn a_cookie_authenticated_write_needs_a_same_origin_request(pool: PgPool) {
            let app = app(&pool, native_guard()).await;
            let cookie = sign_in(&app).await;
            let write = |extra: Option<(&'static str, &'static str)>| {
                let mut b = HttpRequest::post("/write").header(header::COOKIE, &cookie);
                if let Some((name, value)) = extra {
                    b = b.header(name, value);
                }
                b.body(Body::empty()).expect("request")
            };

            let (status, _, _) = call(&app, write(Some(("sec-fetch-site", "same-origin")))).await;
            assert_eq!(status, StatusCode::OK);

            for refused in [
                None,
                Some(("sec-fetch-site", "cross-site")),
                Some(("sec-fetch-site", "same-site")),
                Some(("sec-fetch-site", "none")),
                Some(("origin", "https://evil.example.com")),
            ] {
                let (status, _, _) = call(&app, write(refused)).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{refused:?}");
            }

            // Without the metadata header, an Origin that matches the request's own authority is
            // the fallback a non-browser client gets.
            let (status, _, _) = call(
                &app,
                HttpRequest::post("/write")
                    .header(header::COOKIE, &cookie)
                    .header(header::HOST, "crucible.example.com")
                    .header(header::ORIGIN, "https://crucible.example.com")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);

            // A read never needs it: the cookie cannot be used to change anything by reading.
            let (status, _, _) = call(
                &app,
                HttpRequest::get("/probe")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }

        /// The whole point of native mode: no path reads a header a client could have written.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn asserted_headers_are_ignored_on_every_native_path(pool: PgPool) {
            let app = app(&pool, native_guard()).await;
            let cookie = sign_in(&app).await;
            let spoofed = [
                ("x-auth-request-user", "root"),
                ("x-auth-request-email", "root@example.com"),
                ("x-auth-request-groups", "/groups/admins"),
                ("x-auth-request-preferred-username", "root"),
            ];
            let with_spoof = |mut b: axum::http::request::Builder| {
                for (name, value) in spoofed {
                    b = b.header(name, value);
                }
                b.body(Body::empty()).expect("request")
            };

            // The static token path: the configured identity, never the header's.
            let (status, body, _) = call(
                &app,
                with_spoof(
                    HttpRequest::get("/probe").header(header::AUTHORIZATION, "Bearer s3cr3t"),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "crucible-cd||StaticToken");

            // The session path: the session's claims, never the header's.
            let (status, body, _) = call(
                &app,
                with_spoof(HttpRequest::get("/probe").header(header::COOKIE, &cookie)),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "alice|/groups/team-x|Session");

            // And nothing in the family reaches a handler at all.
            let (status, body, _) = call(
                &app,
                with_spoof(HttpRequest::get("/headers").header(header::COOKIE, &cookie)),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "");
        }

        /// A cluster token proves an OpenShift username, which need not be an SSO login. A `user:`
        /// principal is an SSO login, so a name nobody has signed in under owns nothing.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn a_cluster_token_owns_nothing_until_its_login_has_signed_in(pool: PgPool) {
            let guard = BearerGuard {
                kube: Some(kube_auth().await),
                users: Some(pool.clone()),
                ..native_guard()
            };
            let app = app(&pool, guard).await;
            let probe = || {
                HttpRequest::get("/probe")
                    .header(header::AUTHORIZATION, "Bearer sha256~mine")
                    .body(Body::empty())
                    .expect("request")
            };

            let (status, body, _) = call(&app, probe()).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                body, "wynn||UnknownClusterToken",
                "nobody has signed in as wynn"
            );

            crate::identity::oidc::users::record_login(
                &pool,
                "sub-wynn",
                "wynn",
                None,
                jiff::Timestamp::now(),
            )
            .await
            .expect("login");
            let (status, body, _) = call(&app, probe()).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "wynn||ClusterToken");
        }

        /// A deployment with no issuer configured cannot validate a JWT, so it must not fall
        /// through to handing that JWT to the cluster's `users/~` as if it were a cluster token.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn a_jwt_bearer_without_an_issuer_is_401_and_never_a_cluster_token(pool: PgPool) {
            let guard = BearerGuard {
                oidc: None,
                kube: Some(kube_auth().await),
                ..native_guard()
            };
            let app = app(&pool, guard).await;
            let (status, _, _) = call(
                &app,
                HttpRequest::get("/probe")
                    .header(header::AUTHORIZATION, "Bearer aGVhZGVy.cGF5bG9hZA.c2ln")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }

        /// Criterion: `Identity` and `Groups` keep their shape, so every role check answers the
        /// same in both modes. The real `/api/whoami` is the probe — same logins, same groups, same
        /// role lists, one identity model each, and the only field allowed to differ is `mode`.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn every_role_check_answers_the_same_in_both_modes(pool: PgPool) {
            struct NoSink;
            impl crate::daemon::queue::OverrideSink for NoSink {
                fn submit(&self, _ov: crate::daemon::queue::Override) {}
            }
            let state = |mode: AuthMode| crate::api::state::ApiState {
                roles: Roles::new(
                    vec!["alice".to_string()],
                    vec!["bob".to_string()],
                    vec!["team-x".to_string()],
                ),
                auth_mode: mode,
                ..crate::api::state::ApiState::test(
                    crate::client::Db::new(pool.clone()),
                    Arc::new(NoSink),
                )
            };

            let proxy_app = crate::api::router(state(AuthMode::Proxy)).layer(
                axum::middleware::from_fn_with_state(
                    Arc::new(
                        BearerGuard::new(
                            SharedToken::new("s3cr3t"),
                            SharedToken::new("edge"),
                            None,
                            None,
                            AuthMode::Proxy,
                        )
                        .expect("guard builds"),
                    ),
                    require_auth,
                ),
            );

            let native_app = |login: &'static str, groups: Vec<String>| {
                let pool = pool.clone();
                let state = state(AuthMode::Native);
                async move {
                    let store = crate::identity::session::store(&pool)
                        .await
                        .expect("session store");
                    crate::api::router(state)
                        .layer(axum::middleware::from_fn_with_state(
                            Arc::new(native_guard()),
                            require_auth,
                        ))
                        .route(
                            "/establish",
                            get(move |session: tower_sessions::Session| async move {
                                crate::identity::session::establish_native(
                                    &session,
                                    &crate::identity::session::NativeClaims {
                                        sub: format!("sub-{login}"),
                                        login: login.to_string(),
                                        email: None,
                                        groups,
                                        groups_at: jiff::Timestamp::now().to_string(),
                                        downgraded: false,
                                    },
                                    None,
                                )
                                .await
                                .expect("establish");
                                "signed in"
                            }),
                        )
                        .layer(crate::identity::session::layer(store, false))
                }
            };

            for (login, groups) in [
                ("alice", ""),
                ("bob", ""),
                ("mallory", ""),
                ("mallory", "/groups/team-x"),
                ("alice", "/groups/team-x"),
            ] {
                let mut edge = HttpRequest::get("/api/whoami")
                    .header(header::AUTHORIZATION, "Bearer edge")
                    .header("x-auth-request-user", login);
                if !groups.is_empty() {
                    edge = edge.header("x-auth-request-groups", groups);
                }
                let (status, proxy_body, _) =
                    call(&proxy_app, edge.body(Body::empty()).expect("request")).await;
                assert_eq!(status, StatusCode::OK);

                let group_list: Vec<String> = groups
                    .split(',')
                    .map(|g| g.trim().to_string())
                    .filter(|g| !g.is_empty())
                    .collect();
                let app = native_app(login, group_list).await;
                let cookie = {
                    let (status, _, cookie) = call(
                        &app,
                        HttpRequest::get("/establish")
                            .body(Body::empty())
                            .expect("request"),
                    )
                    .await;
                    assert_eq!(status, StatusCode::OK);
                    cookie.expect("session cookie")
                };
                let (status, native_body, _) = call(
                    &app,
                    HttpRequest::get("/api/whoami")
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await;
                assert_eq!(status, StatusCode::OK);

                let mut proxy: serde_json::Value =
                    serde_json::from_str(&proxy_body).expect("proxy whoami");
                let mut native: serde_json::Value =
                    serde_json::from_str(&native_body).expect("native whoami");
                assert_eq!(proxy["mode"], serde_json::json!("proxy"));
                assert_eq!(native["mode"], serde_json::json!("native"));
                proxy["mode"] = serde_json::Value::Null;
                native["mode"] = serde_json::Value::Null;
                assert_eq!(
                    proxy, native,
                    "{login} with groups {groups:?} must resolve identically in both modes"
                );
            }
        }

        /// The two modes never share a credential: native mode has no SSO edge to hold a proxy
        /// token, and an issuer alone keeps the guard closed even with no API token set.
        #[test]
        fn native_mode_refuses_a_proxy_token_and_stays_closed_without_an_api_token() {
            let refusal = match BearerGuard::new(
                SharedToken::new("s3cr3t"),
                SharedToken::new("edge"),
                None,
                None,
                AuthMode::Native,
            ) {
                Ok(_) => panic!("expected a refusal"),
                Err(e) => e.to_string(),
            };
            assert!(refusal.contains("no SSO edge"), "{refusal}");

            let mut guard =
                BearerGuard::new(None, None, None, None, AuthMode::Native).expect("guard builds");
            assert!(
                guard.is_open(),
                "no issuer and no token is the loopback shape"
            );
            guard.oidc = native_guard().oidc;
            assert!(
                !guard.is_open(),
                "a deployment with logins is never the open shape"
            );
            assert!(
                guard.with_dev_identity(Some("wren".into())).is_err(),
                "an issuer alone refuses a dev identity"
            );
        }
    }
}
