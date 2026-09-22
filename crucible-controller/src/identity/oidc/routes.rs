//! The controller-owned login, callback, and logout routes.
//!
//! They are mounted INSIDE the session layer and OUTSIDE the auth guard: signing in is what a
//! caller with no credential does, so a guard that demanded one first would be a loop.
//!
//! Both modes answer here. In proxy mode `/auth/login` and `/auth/logout` are thin redirects onto
//! the sidecar's own endpoints and `/auth/callback` is a no-op, so the SPA can hold one set of URLs
//! across the flip and back.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use serde::Deserialize;
use std::sync::Arc;
use tower_sessions::Session;

use crate::identity::api_key::constant_time_eq;
use crate::identity::auth::AuthMode;
use crate::identity::oidc::{OidcError, OidcProvider};

/// What the routes need: the mode, the issuer (native), the pool the `users` row is written to, and
/// the sidecar's path prefix (proxy).
#[derive(Clone)]
pub struct AuthState {
    pub mode: AuthMode,
    pub oidc: Option<Arc<OidcProvider>>,
    pub pool: sqlx::PgPool,
    /// The chart-mounted key the offline credential is sealed under. `None` — no key mounted —
    /// stores no credential, and scheduled launches stay on the schedule-row snapshot.
    pub credential_keys: Option<Arc<crate::identity::oidc::credentials::CredentialKeys>>,
    /// `auth.proxyPrefix` — where the sidecar's `/start` and `/sign_out` live. Empty is legal: a
    /// deployment whose registered redirect URI is a bare `/callback` runs with no prefix.
    pub proxy_prefix: String,
}

impl AuthState {
    fn native(&self) -> Option<Arc<OidcProvider>> {
        self.oidc.clone().filter(|_| self.mode == AuthMode::Native)
    }
}

pub fn router(state: AuthState) -> Router {
    let mut router = Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", get(logout));
    if let Some(path) = state
        .oidc
        .as_ref()
        .and_then(|provider| provider.callback_path())
        .filter(|path| path != "/auth/callback")
    {
        router = router.route(&path, get(callback));
    }
    router.with_state(state)
}

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    /// Where to land after signing in. Same-origin absolute paths only.
    rd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

impl CallbackQuery {
    /// No authorization response at all. The registered redirect URI is the only URI the issuer
    /// accepts as a `post_logout_redirect_uri`, so a bare hit here is the browser coming back from
    /// logout, not a login to refuse.
    fn is_empty(&self) -> bool {
        self.code.is_none() && self.state.is_none() && self.error.is_none()
    }
}

/// The post-login destination, refused unless it is a path on this origin. Anything else — an
/// absolute URL, a protocol-relative `//host`, a backslash the browser normalizes to one — would
/// make the callback an open redirect.
fn safe_redirect(raw: Option<String>) -> String {
    let candidate = raw.unwrap_or_default();
    let ok = candidate.starts_with('/')
        && !candidate.starts_with("//")
        && !candidate.starts_with("/\\")
        && !candidate.contains(['\r', '\n']);
    if ok { candidate } else { "/".to_string() }
}

fn refused(status: StatusCode, why: impl std::fmt::Display) -> Response {
    let body = serde_json::json!({ "status": "error", "error": why.to_string() });
    (status, axum::Json(body)).into_response()
}

fn from_oidc(e: OidcError) -> Response {
    match e {
        OidcError::Unavailable(why) => {
            tracing::warn!(error = %why, "the identity provider could not be reached");
            refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "the identity provider could not be reached; retry",
            )
        }
        OidcError::Rejected(why) => {
            tracing::warn!(reason = %why, "login refused");
            refused(StatusCode::UNAUTHORIZED, why)
        }
    }
}

fn session_error(context: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "{context}");
    refused(StatusCode::INTERNAL_SERVER_ERROR, "session store error")
}

/// Start a login. Proxy mode hands the browser to the sidecar; native mode builds the
/// authorization request, parks its PKCE verifier, nonce, and state on the session, and redirects.
async fn login(
    State(state): State<AuthState>,
    session: Session,
    Query(query): Query<LoginQuery>,
) -> Response {
    let redirect_to = safe_redirect(query.rd);
    let Some(oidc) = state.native() else {
        let encoded = urlencode(&redirect_to);
        return Redirect::to(&format!("{}/start?rd={encoded}", state.proxy_prefix)).into_response();
    };
    let (url, flow) = match oidc.authorize(redirect_to).await {
        Ok(ok) => ok,
        Err(e) => return from_oidc(e),
    };
    if let Err(e) = crate::identity::session::start_login(&session, &flow).await {
        return session_error("parking the login flow", e);
    }
    Redirect::to(&url).into_response()
}

/// Finish a login: the state must match the one this browser started with, the code is exchanged
/// with the PKCE verifier, and the ID token is validated before any session exists.
async fn callback(
    State(state): State<AuthState>,
    session: Session,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(oidc) = state.native() else {
        // Proxy mode owns its own callback under the sidecar's prefix; this route is inert.
        return Redirect::to("/").into_response();
    };
    if query.is_empty() {
        return Redirect::to("/").into_response();
    }
    // Taken unconditionally: an authorization response, good or bad, consumes the request it
    // answers, so a code can never be replayed against a second one.
    let flow = match crate::identity::session::take_login(&session).await {
        Ok(flow) => flow,
        Err(e) => return session_error("reading the login flow", e),
    };
    if let Some(error) = query.error {
        let detail = query.error_description.unwrap_or_default();
        return refused(
            StatusCode::UNAUTHORIZED,
            format!("the identity provider refused the login: {error} {detail}").trim_end(),
        );
    }
    let Some(flow) = flow else {
        return refused(
            StatusCode::BAD_REQUEST,
            "this callback answers no login this browser started",
        );
    };
    let (Some(code), Some(returned_state)) = (query.code, query.state) else {
        return refused(StatusCode::BAD_REQUEST, "the callback carried no code");
    };
    if !constant_time_eq(returned_state.as_bytes(), flow.state.as_bytes()) {
        return refused(
            StatusCode::UNAUTHORIZED,
            "the callback's state does not match the login this browser started",
        );
    }
    let exchanged = match oidc.exchange(code, &flow).await {
        Ok(ok) => ok,
        Err(e) => return from_oidc(e),
    };
    let crate::identity::oidc::Exchanged {
        claims,
        refresh_token,
        id_token,
    } = exchanged;
    let now = jiff::Timestamp::now();
    if let Err(e) = record_login(&state, &claims, refresh_token.as_deref(), now).await {
        tracing::error!(error = %format!("{e:#}"), "recording the login");
        return refused(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the login could not be recorded",
        );
    }
    // A fresh login clears whatever a failed refresh left on this owner's schedules: the whole
    // point of signing in again is that the credential works.
    if let Err(e) = crate::launches::standing::clear_owner_signin(&state.pool, &claims.login).await
    {
        tracing::warn!(error = %format!("{e:#}"), "clearing the owner's sign-in flags");
    }
    let native = crate::identity::session::NativeClaims {
        sub: claims.sub,
        login: claims.login,
        email: claims.email,
        groups: claims.groups,
        groups_at: now.to_string(),
        downgraded: false,
    };
    if let Err(e) =
        crate::identity::session::establish_native(&session, &native, Some(&id_token)).await
    {
        return session_error("establishing the session", e);
    }
    Redirect::to(&flow.redirect_to).into_response()
}

/// The user row, the groups its claim carried, and the offline credential, in one transaction.
/// Either all of it lands or none does: a `users` row with no credential is a user whose schedules
/// silently stop following live groups, and one with stale groups is an API key answering for a
/// membership its owner no longer holds.
async fn record_login(
    state: &AuthState,
    claims: &crate::identity::oidc::VerifiedClaims,
    refresh_token: Option<&str>,
    now: jiff::Timestamp,
) -> anyhow::Result<()> {
    use anyhow::Context;
    let mut tx = state
        .pool
        .begin()
        .await
        .context("opening the login write")?;
    crate::identity::oidc::users::record_login_on(
        &mut tx,
        &claims.sub,
        &claims.login,
        claims.email.as_deref(),
        now,
    )
    .await?;
    crate::identity::oidc::users::record_groups_on(&mut tx, &claims.sub, &claims.groups, now)
        .await?;
    if let (Some(keys), Some(token)) = (state.credential_keys.as_deref(), refresh_token) {
        crate::identity::oidc::credentials::upsert(&mut tx, keys, &claims.sub, token, now).await?;
    }
    tx.commit().await.context("committing the login write")
}

/// End a session here, then at the issuer. Proxy mode hands the browser to the sidecar's sign-out.
async fn logout(State(state): State<AuthState>, session: Session) -> Response {
    let Some(oidc) = state.native() else {
        return Redirect::to(&format!("{}/sign_out", state.proxy_prefix)).into_response();
    };
    let hint = match crate::identity::session::id_token(&session).await {
        Ok(hint) => hint,
        Err(e) => return session_error("reading the session's ID token", e),
    };
    if let Err(e) = crate::identity::session::end(&session).await {
        return session_error("ending the session", e);
    }
    let endpoint = match oidc.end_session_endpoint().await {
        Ok(endpoint) => endpoint,
        // The local session is already gone; an unreachable issuer must not leave the browser on an
        // error page for a logout that, locally, succeeded.
        Err(e) => {
            tracing::warn!(error = %e, "no end_session_endpoint for logout");
            None
        }
    };
    let Some(endpoint) = endpoint else {
        return Redirect::to("/").into_response();
    };
    let separator = if endpoint.contains('?') { '&' } else { '?' };
    let mut url = format!(
        "{endpoint}{separator}client_id={}",
        urlencode(&oidc.cfg().client_id)
    );
    if let Some(hint) = &hint {
        url.push_str(&format!("&id_token_hint={}", urlencode(hint)));
    }
    if let Some(post) = &oidc.cfg().post_logout_redirect {
        url.push_str(&format!("&post_logout_redirect_uri={}", urlencode(post)));
    }
    Redirect::to(&url).into_response()
}

fn urlencode(raw: &str) -> String {
    percent_encoding::utf8_percent_encode(raw, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The post-login destination is the one attacker-influenced value the callback echoes, so it
    /// is a same-origin path or it is `/`.
    #[test]
    fn only_a_same_origin_path_survives_the_redirect_check() {
        for good in ["/", "/issues", "/runs/run-1?tab=flow#top"] {
            assert_eq!(safe_redirect(Some(good.to_string())), good);
        }
        for bad in [
            "https://evil.example.com/",
            "//evil.example.com/",
            "/\\evil.example.com",
            "issues",
            "",
            "/ok\nLocation: https://evil.example.com",
        ] {
            assert_eq!(
                safe_redirect(Some(bad.to_string())),
                "/",
                "{bad:?} must not be a redirect target"
            );
        }
        assert_eq!(safe_redirect(None), "/");
    }
}
