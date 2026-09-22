//! The caller's own offline credential: what it is doing, and how to take it away.
//!
//! It is the one thing in the registry a user can destroy for themselves. Revoking is deliberate,
//! so it does not wait on the schedule-row snapshot's TTL: the schedules whose scopes bind secrets
//! are marked as needing sign-in and park on their next firing.

use crate::api::state::{ApiState, AppError, ErrorBody, Json};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// What a settings page shows about the caller's credential. Never the token, and never anything
/// that could be turned back into one.
#[derive(Debug, Serialize, ToSchema)]
pub struct CredentialDto {
    /// Whether a credential is stored at all. False means scheduled launches under this user's
    /// team secrets fall back to the schedule-row snapshot and park once it ages out.
    pub present: bool,
    /// When it last produced a token, RFC3339. `null` until a scheduled launch or a session
    /// refresh has spent it.
    pub refreshed_at: Option<String>,
    /// Why the last refresh was refused, if one was.
    pub last_error: Option<String>,
    pub failures: i64,
}

fn no_subject() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorBody::new(
            "this credential names no issuer subject, so it owns no offline credential",
        )),
    )
        .into_response()
}

#[utoipa::path(
    get,
    path = "/api/credentials/me",
    responses(
        (status = 200, description = "The caller's offline credential state", body = CredentialDto),
        (status = 403, description = "The caller has no issuer subject", body = ErrorBody)
    )
)]
pub(crate) async fn get_credential(
    State(state): State<ApiState>,
    subject: crate::identity::auth::Subject,
) -> Result<Response, AppError> {
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let status = crate::identity::oidc::credentials::status(state.db.pool(), sub).await?;
    Ok(Json(match status {
        Some(status) => CredentialDto {
            present: true,
            refreshed_at: status.refreshed_at,
            last_error: status.last_error,
            failures: status.failures,
        },
        None => CredentialDto {
            present: false,
            refreshed_at: None,
            last_error: None,
            failures: 0,
        },
    })
    .into_response())
}

/// What a revoke did, so the settings page can say how many schedules it stopped.
#[derive(Debug, Serialize, ToSchema)]
pub struct RevokedDto {
    /// Whether there was a credential to revoke.
    pub revoked: bool,
    /// How many of the caller's schedules now need a sign-in before they fire again.
    pub schedules_parked: i64,
}

#[utoipa::path(
    delete,
    path = "/api/credentials/me",
    responses(
        (status = 200, description = "The credential is gone and the caller's schedules need a sign-in", body = RevokedDto),
        (status = 403, description = "The caller has no issuer subject", body = ErrorBody),
        (status = 503, description = "No credential key is mounted, so nothing was ever stored", body = ErrorBody)
    )
)]
pub(crate) async fn revoke_credential(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    subject: crate::identity::auth::Subject,
) -> Result<Response, AppError> {
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let Some(keys) = state.credential_keys.as_deref() else {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody::new(
                "this deployment mounts no credential key, so it stores no offline credential",
            )),
        )
            .into_response());
    };
    let token = crate::identity::oidc::credentials::take(state.db.pool(), keys, sub).await?;
    if let (Some(token), Some(oidc)) = (token.as_deref(), state.oidc.as_deref()) {
        oidc.revoke(token).await;
    }
    let login = identity.as_deref().unwrap_or_default().to_string();
    let parked = crate::launches::standing::require_owner_signin(
        state.db.pool(),
        &login,
        "its owner revoked their offline credential",
    )
    .await?;
    Ok(Json(RevokedDto {
        revoked: token.is_some(),
        schedules_parked: i64::try_from(parked).unwrap_or(i64::MAX),
    })
    .into_response())
}
