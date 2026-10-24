//! The MCP clients a person has approved through people OAuth, and signing them all out.

use crate::api::dto::not_found;
use crate::api::state::{ApiState, AppError, ErrorBody, Json};
use crate::identity::api::cluster_identities::{no_subject, not_from_a_key};
use crate::identity::auth::{AuthPath, Subject};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// One approval whose refresh token is still live.
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
pub struct McpSignInDto {
    pub resource: String,
    pub cluster: String,
    pub approved_at: String,
    pub last_used_at: String,
}

/// Whether this controller signs MCP clients in, and the caller's live approvals when it does.
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
pub struct McpSignInsDto {
    pub enabled: bool,
    pub sign_ins: Vec<McpSignInDto>,
}

#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
pub struct SignedOutDto {
    /// How many approvals were revoked.
    pub revoked: u64,
}

fn stamp(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|s| jiff::Timestamp::from_second(s).ok())
        .map_or_else(String::new, |t| t.to_string())
}

#[utoipa::path(
    get,
    path = "/api/me/mcp-sign-ins",
    responses(
        (status = 200, description = "The caller's live MCP client approvals", body = McpSignInsDto),
        (status = 403, description = "The caller has no issuer subject", body = ErrorBody)
    )
)]
pub(crate) async fn list_mcp_sign_ins(
    State(state): State<ApiState>,
    subject: Subject,
) -> Result<Response, AppError> {
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let Some(people) = state.people_issuer.as_ref().and_then(|i| i.people.as_ref()) else {
        return Ok(Json(McpSignInsDto {
            enabled: false,
            sign_ins: Vec::new(),
        })
        .into_response());
    };
    let live = people
        .grants
        .live(sub, crate::clock::unix_secs())
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Json(McpSignInsDto {
        enabled: true,
        sign_ins: live
            .into_iter()
            .map(|g| McpSignInDto {
                resource: g.resource,
                cluster: g.cluster,
                approved_at: stamp(g.approved_at),
                last_used_at: stamp(g.last_used_at),
            })
            .collect(),
    })
    .into_response())
}

#[utoipa::path(
    delete,
    path = "/api/me/mcp-sign-ins",
    responses(
        (status = 200, description = "Every approval is revoked; clients must sign in again", body = SignedOutDto),
        (status = 403, description = "The caller has no issuer subject, or presented an api key", body = ErrorBody),
        (status = 404, description = "This controller signs no MCP clients in", body = ErrorBody)
    )
)]
pub(crate) async fn sign_out_mcp_clients(
    State(state): State<ApiState>,
    subject: Subject,
    path: AuthPath,
) -> Result<Response, AppError> {
    if path == AuthPath::ApiKey {
        return Ok(not_from_a_key());
    }
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let Some(people) = state.people_issuer.as_ref().and_then(|i| i.people.as_ref()) else {
        return Ok(not_found("this controller signs no MCP clients in"));
    };
    let revoked = people
        .grants
        .revoke_subject(sub, crate::clock::unix_secs())
        .await
        .map_err(anyhow::Error::from)?;
    tracing::info!(sub, revoked, "mcp clients signed out");
    Ok(Json(SignedOutDto { revoked }).into_response())
}
