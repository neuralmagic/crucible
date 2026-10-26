//! The MCP clients a person has approved through people OAuth, and signing one or all of them out.

use crate::agent_auth::people::clients::ClientId;
use crate::api::dto::bad_request;
use crate::api::dto::not_found;
use crate::api::state::{ApiState, AppError, ErrorBody, Json};
use crate::identity::api::cluster_identities::{no_subject, not_from_a_key};
use crate::identity::auth::{AuthPath, Subject};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// One approval whose refresh token is still live.
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
pub struct McpSignInDto {
    pub client_id: String,
    /// The client's name as the person saw it on the consent page.
    pub client_name: String,
    /// The host that published the client's metadata document; absent for any other client.
    pub client_host: Option<String>,
    pub client_kind: crate::agent_auth::people::clients::ClientKind,
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
                client_host: g.client_id.host().map(str::to_string),
                client_kind: g.client_id.kind(),
                client_id: g.client_id.to_string(),
                client_name: g.client_name,
                resource: g.resource,
                cluster: g.cluster,
                approved_at: stamp(g.approved_at),
                last_used_at: stamp(g.last_used_at),
            })
            .collect(),
    })
    .into_response())
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SignOutQuery {
    /// Sign out only this client; every client when absent.
    pub client_id: Option<String>,
}

#[utoipa::path(
    delete,
    path = "/api/me/mcp-sign-ins",
    params(SignOutQuery),
    responses(
        (status = 200, description = "The approvals are revoked; their clients must sign in again", body = SignedOutDto),
        (status = 400, description = "client_id is not a client identifier", body = ErrorBody),
        (status = 403, description = "The caller has no issuer subject, or presented an api key", body = ErrorBody),
        (status = 404, description = "This controller signs no MCP clients in", body = ErrorBody)
    )
)]
pub(crate) async fn sign_out_mcp_clients(
    State(state): State<ApiState>,
    subject: Subject,
    path: AuthPath,
    Query(query): Query<SignOutQuery>,
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
    let now = crate::clock::unix_secs();
    let client = match query.client_id.as_deref().map(ClientId::parse) {
        None => None,
        Some(Ok(client)) => Some(client),
        Some(Err(e)) => return Ok(bad_request(e.to_string())),
    };
    let revoked = match &client {
        None => people.grants.revoke_subject(sub, now).await,
        Some(client) => people.grants.revoke_client(sub, client, now).await,
    }
    .map_err(anyhow::Error::from)?;
    tracing::info!(
        sub,
        client_id = client.as_ref().map(ClientId::as_str),
        revoked,
        "mcp clients signed out"
    );
    Ok(Json(SignedOutDto { revoked }).into_response())
}
