//! The guard on the controller's HTTP API + debug UI. A request authenticates by session cookie or
//! by bearer. With no issuer and no expected token the guard is open: every request lands as
//! `CONTROLLER_DEV_IDENTITY`, and [`crate::Surface::bind`] forces a loopback bind, since an open
//! guard has no business answering the world. A controller with none of the three refuses to start.
//!
//! Which credential proved the caller decides who they may claim to be:
//!
//! ```text
//!   crucible_session cookie -> a login this controller ran -> claims off the session
//!   CONTROLLER_API_TOKEN    -> machine caller -> configured identity, no groups
//!   crk_ api key            -> its owner -> login and stored groups (require_api_key)
//! ```
//!
//! The `X-Auth-Request-*` family is dropped off every request before anything else runs, so nothing
//! a client wrote can reach a handler on any path.
//!
//! What a handler sees is [`Resolved`], stamped as a request extension by [`require_auth`]. The
//! [`Identity`] and [`Groups`] extractors read that extension; they fall back to the asserted
//! headers only where the middleware never ran, which is the unit tests and nothing the network
//! can reach.

#![allow(clippy::disallowed_macros)]

use axum::extract::{FromRequestParts, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Serialize;
use std::convert::Infallible;
use std::sync::Arc;

use crate::identity::api_key::constant_time_eq;
use crate::identity::session::{Identity, Resolved, asserted_groups};

/// The prefix of the header family the guard drops off every request.
const ASSERTED_PREFIX: &str = "x-auth-request-";

/// The issuer's stable subject for the caller, when the credential that proved them carries one:
/// a session and an api key do, the static-token and open paths do not. It is the key everything
/// durable about a person hangs off, so a route that stores or reads one asks for this rather than
/// deriving it from a login.
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
/// seeing an empty group list. And only the paths [`AuthPath::may_own`] names stand for a person
/// whose standing is confirmed, so a downgraded or impersonated session owns nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPath {
    /// A session this controller minted at `/auth/callback`.
    Session,
    /// The same session after a definitively refused group refresh. The login stands; the groups
    /// are gone and so is every role they or the login carried, until the user signs in again.
    DowngradedSession,
    /// `CONTROLLER_API_TOKEN`: a machine caller, with at most a configured identity and no groups.
    StaticToken,
    /// An opaque `crk_…` key its owner minted, carrying their login and the groups their `users`
    /// row holds.
    ApiKey,
    /// No issuer and no expected token: the guard is off, every request is the dev identity, and
    /// the surface is loopback-bound.
    Open,
    /// An administrator's session viewing as another user: that user's login and the groups their
    /// last sign-in stamped, read-only.
    Impersonated,
}

impl AuthPath {
    /// Whether the caller's group list is the issuer's word. False means "no groups were checked",
    /// not "this caller is in no groups".
    pub fn carries_groups(self) -> bool {
        matches!(
            self,
            AuthPath::Session | AuthPath::ApiKey | AuthPath::Impersonated
        )
    }

    /// Whether this credential may own a secret or a schedule at all.
    pub fn may_own(self) -> bool {
        !matches!(self, AuthPath::DowngradedSession | AuthPath::Impersonated)
    }

    /// The path's name as an audit row spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthPath::Session => "session",
            AuthPath::DowngradedSession => "downgraded_session",
            AuthPath::StaticToken => "static_token",
            AuthPath::ApiKey => "api_key",
            AuthPath::Open => "open",
            AuthPath::Impersonated => "impersonated",
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

/// What the auth middleware checks against: the static machine token, its configured identity,
/// the dev identity an open guard lands everyone as, and the issuer sessions are proved against.
pub(crate) struct BearerGuard {
    pub(crate) expected: Option<SharedToken>,
    pub(crate) static_identity: Option<HeaderValue>,
    /// `CONTROLLER_DEV_IDENTITY`: the login every request lands as while the guard is open.
    pub(crate) dev_identity: Option<HeaderValue>,
    pub(crate) oidc: Option<Arc<crate::identity::oidc::OidcProvider>>,
    /// The offline credential a live session's groups are re-read through. `None` — no issuer, no
    /// mounted key — leaves a session's groups exactly as its login stamped them.
    pub(crate) refresh: Option<Arc<crate::identity::oidc::credentials::OwnerRefresh>>,
    /// How stale a session's groups may get before the next request re-reads them.
    pub(crate) refresh_after: std::time::Duration,
}

impl Default for BearerGuard {
    fn default() -> Self {
        BearerGuard {
            expected: None,
            static_identity: None,
            dev_identity: None,
            oidc: None,
            refresh: None,
            refresh_after: crate::identity::oidc::users::DEFAULT_SESSION_GROUP_REFRESH,
        }
    }
}

impl BearerGuard {
    /// Read the credentials off the environment. A guard that could name nobody — no issuer, no
    /// static token, no dev identity — refuses to build, so an unconfigured deployment fails
    /// closed rather than starting open.
    pub(crate) fn from_env(
        oidc: Option<Arc<crate::identity::oidc::OidcProvider>>,
        users: Option<sqlx::PgPool>,
    ) -> anyhow::Result<Self> {
        let mut guard = BearerGuard::new(
            expected_token(),
            std::env::var("CONTROLLER_API_TOKEN_IDENTITY").ok(),
        )?;
        guard.oidc = oidc.clone();
        let keys = crate::identity::oidc::credentials::CredentialKeys::from_env()?;
        guard.refresh = users.and_then(|pool| {
            crate::identity::oidc::credentials::OwnerRefresh::from_parts(pool, oidc, keys)
        });
        guard.refresh_after = crate::identity::oidc::users::session_group_refresh_interval();
        if guard.oidc.is_none()
            && std::env::var("CONTROLLER_AUTH_MODE")
                .is_ok_and(|mode| mode.trim().eq_ignore_ascii_case("native"))
        {
            anyhow::bail!(
                "CONTROLLER_AUTH_MODE=native without CONTROLLER_OIDC_ISSUER: there is no identity provider to run a login against"
            );
        }
        guard
            .with_dev_identity(std::env::var("CONTROLLER_DEV_IDENTITY").ok())?
            .named_or_closed()
    }

    /// Land every request on an open guard as `login`. Errors on a closed guard.
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

    /// Refuse an open guard with no dev identity: it would admit every request as nobody.
    pub(crate) fn named_or_closed(self) -> anyhow::Result<Self> {
        if self.is_open() && self.dev_identity.is_none() {
            anyhow::bail!(
                "no identity is configured: set CONTROLLER_OIDC_ISSUER for logins, CONTROLLER_API_TOKEN for machine callers, or CONTROLLER_DEV_IDENTITY to run an open local dev controller"
            );
        }
        Ok(self)
    }

    /// Reject an identity the header can't carry at boot instead of per request.
    pub(crate) fn new(
        expected: Option<SharedToken>,
        static_identity: Option<String>,
    ) -> anyhow::Result<Self> {
        let static_identity = static_identity
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|v| {
                HeaderValue::from_str(&v).map_err(|_| {
                    anyhow::anyhow!("CONTROLLER_API_TOKEN_IDENTITY is not a valid header value")
                })
            })
            .transpose()?;
        Ok(BearerGuard {
            expected,
            static_identity,
            ..BearerGuard::default()
        })
    }

    /// No expected token and no issuer: the guard admits everything, which is why
    /// [`crate::Surface::bind`] binds loopback. An issuer alone is enough to keep it closed — that
    /// deployment has logins.
    pub(crate) fn is_open(&self) -> bool {
        self.expected.is_none() && self.oidc.is_none()
    }
}

/// Middleware: 401 any request that doesn't carry a credential the guard recognizes. The whole
/// `X-Auth-Request-*` family is dropped before anything else runs. With no expected token and no
/// issuer, every request passes as `CONTROLLER_DEV_IDENTITY`, and the surface is loopback-bound.
///
/// The credentials: the session cookie this controller minted (its claims come off the session,
/// and a non-GET additionally needs a same-origin `Sec-Fetch-Site` or `Origin`, because the cookie
/// is `SameSite=Lax` and every tenant on the cluster's apps wildcard is same-site), and the static
/// token, which gets the configured `CONTROLLER_API_TOKEN_IDENTITY` (no identity configured =
/// anonymous). An api key is checked by [`require_api_key`] before this guard sees it. A session
/// store that cannot answer is 500, and an unreachable issuer during a group refresh keeps the
/// groups the session had.
pub(crate) async fn require_auth(
    State(guard): State<Arc<BearerGuard>>,
    mut req: Request,
    next: Next,
) -> Response {
    for name in asserted_headers(req.headers()) {
        req.headers_mut().remove(&name);
    }

    if guard.is_open() {
        let resolved = match &guard.dev_identity {
            Some(value) => match value.to_str() {
                Ok(name) => Resolved::named(name),
                Err(_) => return unauthorized(),
            },
            None => Resolved::anonymous(),
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
        // No bearer: a browser authenticates by the cookie its login set, which needs an issuer.
        if guard.oidc.is_none() {
            return unauthorized();
        }
        let session = req.extensions().get::<tower_sessions::Session>().cloned();
        return match native_session(session.clone()).await {
            Ok(Some(claims)) => {
                if !is_safe(req.method()) && !same_origin(&req) {
                    return cross_origin();
                }
                let claims = match &session {
                    Some(session) => refresh_session_groups(&guard, session, claims).await,
                    None => claims,
                };
                if !claims.downgraded
                    && let Some(session) = &session
                {
                    match viewing_as(session, &claims.login).await {
                        Ok(Some(view)) => {
                            if !is_safe(req.method()) && !stops_viewing(&req) {
                                return read_only_view(&view);
                            }
                            let resolved = Resolved {
                                user: Some(view.login.clone()),
                                groups: view.groups.clone(),
                                sub: Some(view.sub.clone()),
                            };
                            req.extensions_mut().insert(view);
                            admit(&mut req, AuthPath::Impersonated, resolved);
                            return next.run(req).await;
                        }
                        Ok(None) => {}
                        Err(e) => return session_error(&e),
                    }
                }
                let path = if claims.downgraded {
                    AuthPath::DowngradedSession
                } else {
                    AuthPath::Session
                };
                let resolved = Resolved {
                    user: Some(claims.login),
                    groups: claims.groups,
                    sub: Some(claims.sub),
                };
                admit(&mut req, path, resolved);
                next.run(req).await
            }
            Ok(None) if wants_page(&req) => login_redirect(&req),
            Ok(None) => unauthorized(),
            Err(response) => response,
        };
    };

    if guard
        .expected
        .as_ref()
        .is_some_and(|e| e.matches(&presented))
    {
        let resolved = match &guard.static_identity {
            Some(value) => match value.to_str() {
                Ok(name) => Resolved::named(name),
                Err(_) => return unauthorized(),
            },
            None => Resolved::anonymous(),
        };
        admit(&mut req, AuthPath::StaticToken, resolved);
        return next.run(req).await;
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
    if presented_bearer(req.headers()).is_some_and(crate::runs::pod_token::looks_like_token) {
        return key_refused("a pod token authenticates only its own evidence routes");
    }
    if presented_bearer(req.headers()).is_some_and(crate::identity::api_key::looks_like_key) {
        let keys = KeyGuard {
            pool: both.pool,
            refresh: both.guard.refresh.clone(),
        };
        return require_api_key(State(keys), req, next).await;
    }
    require_auth(State(both.guard), req, next).await
}

/// The caller's bearer, whose prefix picks the guard. A malformed or revoked key still lands on
/// the key guard, so it is told what is wrong with it.
fn presented_bearer(headers: &header::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
}

/// What [`require_api_key`] checks a key against: the pool it is stored in, and the offline
/// credential its owner's stale groups are re-read through.
#[derive(Clone)]
pub struct KeyGuard {
    pub(crate) pool: sqlx::PgPool,
    pub(crate) refresh: Option<Arc<crate::identity::oidc::credentials::OwnerRefresh>>,
}

/// Middleware: admit a request only on an API key its owner minted.
///
/// Deliberately not part of [`require_auth`]. The surfaces this guards are reached by agents
/// holding one long-lived credential, and none of the human paths — the session cookie, the static
/// machine token — mean anything there. Keeping the
/// two guards apart means a bug in either cannot widen the other, and it is the same shape
/// `/metrics` and the run evidence routes already use: merged outside the human guard, carrying
/// their own credential check.
///
/// What it stamps is what every handler already reads, so nothing downstream needs to know an API
/// key was involved: the owner's login, their subject, and the groups their `users` row holds.
pub async fn require_api_key(
    State(KeyGuard { pool, refresh }): State<KeyGuard>,
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
    let authenticated =
        match crate::identity::api_key::verify(&pool, refresh.as_deref(), presented).await {
            Ok(authenticated) => authenticated,
            Err(refusal) => {
                tracing::debug!(reason = %refusal, "api key refused");
                return key_refused(&refusal.to_string());
            }
        };

    // The client wrote no identity that survives: every X-Auth-Request-* header goes, exactly as
    // `require_auth` drops them, so the key is the only thing naming this caller.
    for name in asserted_headers(req.headers()) {
        req.headers_mut().remove(&name);
    }
    let resolved = Resolved {
        user: Some(authenticated.login),
        groups: authenticated.groups,
        sub: Some(authenticated.sub),
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
    if !crate::identity::oidc::users::stale(&claims.groups_at, now, guard.refresh_after) {
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
/// The live view-as this session's administrator started, if any. An expired one, or one another
/// login started on this session, is dropped rather than honored.
async fn viewing_as(
    session: &tower_sessions::Session,
    login: &str,
) -> Result<Option<crate::identity::session::Impersonation>, tower_sessions::session::Error> {
    let Some(view) = crate::identity::session::impersonation(session).await? else {
        return Ok(None);
    };
    if view.by == login && !view.expired(jiff::Timestamp::now()) {
        return Ok(Some(view));
    }
    crate::identity::session::stop_impersonation(session).await?;
    Ok(None)
}

/// The one write a view-as session may make: stopping the view.
fn stops_viewing(req: &Request) -> bool {
    req.method() == Method::DELETE && req.uri().path() == "/api/impersonation"
}

fn read_only_view(view: &crate::identity::session::Impersonation) -> Response {
    let body = serde_json::json!({
        "error": format!(
            "viewing as {} is read-only; stop viewing as them to make changes",
            view.login
        )
    });
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_default(),
    )
        .into_response()
}

fn session_error(e: &tower_sessions::session::Error) -> Response {
    tracing::error!(error = %e, "reading the session's view-as slot");
    (StatusCode::INTERNAL_SERVER_ERROR, "session store error").into_response()
}

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

fn cross_origin() -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"status":"error","error":"a cookie-authenticated write must come from this origin"}"#,
    )
        .into_response()
}

fn asserted_headers(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .keys()
        .filter(|name| name.as_str().starts_with(ASSERTED_PREFIX))
        .cloned()
        .collect()
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

    /// The admin login whitelist (normalized: trimmed, lowercased, de-blanked). Read-only view for
    /// the `/admin` access panel (`GET /api/access`); the guards decide from platform team
    /// membership, not this.
    pub(crate) fn admins(&self) -> &[String] {
        &self.admins
    }

    /// The operator login whitelist (normalized). Same read-only-projection role as [`Roles::admins`].
    pub(crate) fn operators(&self) -> &[String] {
        &self.operators
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
    fn operator_is_not_admin() {
        let roles = Roles::new(vec![], vec!["bob".to_string()], vec![]);
        assert_eq!(
            roles.role(&identity("bob"), &Groups(Vec::new())),
            Role::Operator
        );
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

        /// Asserted headers on a static-token request name nobody: the request still lands as the
        /// configured identity, with no groups.
        #[tokio::test]
        async fn the_static_token_ignores_every_asserted_header() {
            let (status, body) = call(app(pinned()), req(Some("Bearer s3cr3t"), &CLAIMS)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "crucible-cd|");
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
        /// a hand-maintained list of names, on a closed guard and an open one alike.
        #[tokio::test]
        async fn the_whole_asserted_header_family_is_dropped() {
            let asserted = [
                ("x-auth-request-user", "admin"),
                ("x-auth-request-preferred-username", "admin"),
                ("x-auth-request-access-token", "gho_whatever"),
            ];
            let open = BearerGuard::default()
                .with_dev_identity(Some("wren".into()))
                .expect("an open guard takes a dev identity");
            for (guard, bearer) in [(static_only(), Some("Bearer s3cr3t")), (open, None)] {
                let (status, body) = call(header_echo_app(guard), req(bearer, &asserted)).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(body, "", "no x-auth-request-* header reaches the handler");
            }
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

            let (status, body) =
                call(path_echo_app(pinned()), req(Some("Bearer s3cr3t"), &[])).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "StaticToken");

            // No issuer and no expected token: the guard is off, and the surface is loopback-bound.
            let (status, body) = call(path_echo_app(BearerGuard::default()), req(None, &[])).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "Open");
        }

        /// A cluster token or a JWT is no credential here: anything but the static token, a
        /// session, or an api key is 401.
        #[tokio::test]
        async fn a_foreign_bearer_is_401() {
            for bearer in ["Bearer sha256~mine", "Bearer aGVhZGVy.cGF5bG9hZA.c2ln"] {
                let (status, _) = call(app(static_only()), req(Some(bearer), &CLAIMS)).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer}");
            }
        }

        #[tokio::test]
        async fn a_missing_or_malformed_bearer_is_401() {
            for header in [None, Some("s3cr3t"), Some("Bearer "), Some("Bearer wrong")] {
                let (status, _) = call(app(static_only()), req(header, &[])).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "header {header:?}");
            }
        }

        #[test]
        fn an_identity_the_header_cannot_carry_refuses_to_build() {
            let refusal = match BearerGuard::new(token("s3cr3t"), Some("bad\nname".into())) {
                Ok(_) => panic!("expected a refusal"),
                Err(e) => e.to_string(),
            };
            assert!(refusal.contains("not a valid header value"), "{refusal}");
        }

        #[test]
        fn a_blank_configured_identity_is_no_identity() {
            let guard = BearerGuard::new(token("s3cr3t"), Some("  ".into())).expect("guard builds");
            assert!(guard.static_identity.is_none());
            assert!(!guard.is_open());
            assert!(
                BearerGuard::new(None, None)
                    .expect("guard builds")
                    .is_open()
            );
        }

        /// No issuer, no static token, no dev identity: a guard that would admit every request as
        /// nobody refuses to build, so a deployment that forgot its issuer does not start open.
        #[test]
        fn an_open_guard_without_a_dev_identity_refuses_to_build() {
            let refusal = match BearerGuard::default().named_or_closed() {
                Ok(_) => panic!("expected a refusal"),
                Err(e) => e.to_string(),
            };
            assert!(
                refusal.contains("CONTROLLER_OIDC_ISSUER")
                    && refusal.contains("CONTROLLER_DEV_IDENTITY"),
                "{refusal}"
            );
            assert!(static_only().named_or_closed().is_ok());
            assert!(
                BearerGuard::default()
                    .with_dev_identity(Some("wren".into()))
                    .and_then(BearerGuard::named_or_closed)
                    .is_ok()
            );
        }

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

    /// A deployment with an issuer: identity is a cookie this controller minted or an api key, and
    /// nothing a client wrote in the `X-Auth-Request-*` family is read on any path.
    mod native {
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
                    "/api/impersonation",
                    axum::routing::delete(|session: tower_sessions::Session| async move {
                        crate::identity::session::stop_impersonation(&session)
                            .await
                            .expect("stop");
                        "stopped"
                    }),
                )
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
                .route(
                    "/view-as/{variant}",
                    get(
                        |session: tower_sessions::Session,
                         axum::extract::Path(variant): axum::extract::Path<String>| async move {
                            let now = jiff::Timestamp::now();
                            let (by, started_at) = match variant.as_str() {
                                "stale" => ("alice", now - jiff::SignedDuration::from_hours(2)),
                                "foreign" => ("mallory", now),
                                _ => ("alice", now),
                            };
                            crate::identity::session::start_impersonation(
                                &session,
                                &crate::identity::session::Impersonation {
                                    login: "reed".to_string(),
                                    sub: "sub-reed".to_string(),
                                    groups: vec!["/groups/mlr".to_string()],
                                    groups_at: Some(now.to_string()),
                                    by: by.to_string(),
                                    started_at,
                                },
                            )
                            .await
                            .expect("start");
                            "viewing"
                        },
                    ),
                )
                .layer(crate::identity::session::layer(store, false))
        }

        fn native_guard() -> BearerGuard {
            let mut guard =
                BearerGuard::new(SharedToken::new("s3cr3t"), Some("crucible-cd".into()))
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

        async fn probe(app: &axum::Router, cookie: &str) -> String {
            let (status, body, _) = call(
                app,
                HttpRequest::get("/probe")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body
        }

        async fn view_as(app: &axum::Router, cookie: &str, variant: &str) {
            let (status, _, _) = call(
                app,
                HttpRequest::get(format!("/view-as/{variant}"))
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }

        /// A session viewing as someone resolves every request as their snapshot, refuses every
        /// write but the stop, and answers as its own login again once stopped.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn a_view_as_session_answers_as_the_target_and_refuses_writes(pool: PgPool) {
            let app = app(&pool, native_guard()).await;
            let cookie = sign_in(&app).await;
            view_as(&app, &cookie, "live").await;
            assert_eq!(probe(&app, &cookie).await, "reed|/groups/mlr|Impersonated");

            let (status, body, _) = call(
                &app,
                HttpRequest::post("/write")
                    .header(header::COOKIE, &cookie)
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(body.contains("viewing as reed is read-only"), "{body}");

            let (status, body, _) = call(
                &app,
                HttpRequest::delete("/api/impersonation")
                    .header(header::COOKIE, &cookie)
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(probe(&app, &cookie).await, "alice|/groups/team-x|Session");
        }

        /// A snapshot past its hour, or one another login left on the session, is dropped: the
        /// session answers as its own login.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn a_stale_or_foreign_view_as_is_dropped(pool: PgPool) {
            let app = app(&pool, native_guard()).await;
            for variant in ["stale", "foreign"] {
                let cookie = sign_in(&app).await;
                view_as(&app, &cookie, variant).await;
                assert_eq!(
                    probe(&app, &cookie).await,
                    "alice|/groups/team-x|Session",
                    "{variant}"
                );
                assert_eq!(
                    probe(&app, &cookie).await,
                    "alice|/groups/team-x|Session",
                    "{variant}: dropped, not just skipped once"
                );
            }
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

        /// No path reads a header a client could have written.
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

        /// An issuer alone keeps the guard closed even with no API token set, and refuses a dev
        /// identity.
        #[test]
        fn an_issuer_alone_keeps_the_guard_closed() {
            let mut guard = BearerGuard::new(None, None).expect("guard builds");
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

        /// A bearer that is neither the static token nor an api key is refused on a deployment with
        /// logins: device-flow JWTs and cluster tokens are no credentials here.
        #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
        async fn a_jwt_or_cluster_token_bearer_is_401(pool: PgPool) {
            let app = app(&pool, native_guard()).await;
            for bearer in ["Bearer aGVhZGVy.cGF5bG9hZA.c2ln", "Bearer sha256~mine"] {
                let (status, _, _) = call(
                    &app,
                    HttpRequest::get("/probe")
                        .header(header::AUTHORIZATION, bearer)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer}");
            }
        }
    }
}
