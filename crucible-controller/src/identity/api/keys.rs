//! The caller's own API keys: mint one, list them, take one back.
//!
//! Every route here needs an issuer subject, because a key hangs off a `users` row and a credential
//! that names nobody the controller has seen sign in has no row to hang from.
//!
//! None of them may be reached with an API key. A key that can mint keys is a key that outlives its
//! own revocation: revoke it and whatever it minted still works. Minting stays a thing a person
//! does in a browser, once, deliberately.

use crate::api::dto::{bad_request, forbidden, not_found};
use crate::api::state::{ApiState, AppError, ErrorBody, Json};
use crate::identity::api_key::ApiKey;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The default lifetime the mint form offers. A key meant to outlive a quarter has to ask.
const DEFAULT_EXPIRY_DAYS: i64 = 90;
/// The longest name worth storing; past this the caller is pasting, not naming.
const MAX_NAME: usize = 120;

/// What a mint asks for.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MintBody {
    /// What to call it, so its owner can tell two keys apart later.
    pub name: String,
    /// Days until it expires. Omitted is [`DEFAULT_EXPIRY_DAYS`]; an explicit `null` never expires.
    #[serde(default, deserialize_with = "expiry_days")]
    pub expires_in_days: Option<Option<i64>>,
}

/// Distinguishes "the field was absent" from "the field was `null`", which mean different
/// lifetimes here and would otherwise collapse into the same `None`.
fn expiry_days<'de, D>(d: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<i64>::deserialize(d).map(Some)
}

/// A freshly minted key. The only response that ever carries `secret`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MintedDto {
    #[serde(flatten)]
    pub key: ApiKey,
    /// The full `crk_…` credential. Shown once — the controller keeps only its digest and cannot
    /// show it again.
    pub secret: String,
    /// Where to point an MCP client, so the page can show a command that works rather than one
    /// with a hole in it. Null on a deployment that exposes no MCP route; the SPA's own origin is
    /// not a usable guess, because `/mcp` rides the machine Route on its own hostname.
    pub mcp_url: Option<String>,
}

fn no_subject() -> Response {
    forbidden("this credential names no issuer subject, so it holds no api keys")
}

/// A key may not manage keys. See the module note.
fn not_from_a_key() -> Response {
    forbidden("api keys cannot manage api keys; sign in to mint or revoke one")
}

#[utoipa::path(
    get,
    path = "/api/keys",
    responses(
        (status = 200, description = "Every api key the caller holds", body = Vec<ApiKey>),
        (status = 403, description = "The caller has no issuer subject, or presented a key", body = ErrorBody)
    )
)]
pub(crate) async fn list_keys(
    State(state): State<ApiState>,
    subject: crate::identity::auth::Subject,
    path: crate::identity::auth::AuthPath,
) -> Result<Response, AppError> {
    if path == crate::identity::auth::AuthPath::ApiKey {
        return Ok(not_from_a_key());
    }
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    Ok(Json(crate::identity::api_key::list(state.db.pool(), sub).await?).into_response())
}

#[utoipa::path(
    post,
    path = "/api/keys",
    request_body = MintBody,
    responses(
        (status = 201, description = "The minted key, including its one-time secret", body = MintedDto),
        (status = 400, description = "The name is empty or too long", body = ErrorBody),
        (status = 403, description = "The caller has no issuer subject, or presented a key", body = ErrorBody)
    )
)]
pub(crate) async fn mint_key(
    State(state): State<ApiState>,
    subject: crate::identity::auth::Subject,
    path: crate::identity::auth::AuthPath,
    Json(body): Json<MintBody>,
) -> Result<Response, AppError> {
    if path == crate::identity::auth::AuthPath::ApiKey {
        return Ok(not_from_a_key());
    }
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Ok(bad_request(format!(
            "an api key needs a name of 1 to {MAX_NAME} characters"
        )));
    }
    let days = match body.expires_in_days {
        None => Some(DEFAULT_EXPIRY_DAYS),
        Some(explicit) => explicit,
    };
    let expires_at = match days {
        None => None,
        Some(days) if days > 0 => Some(expiry_stamp(days)?),
        Some(_) => {
            return Ok(bad_request(
                "an api key's lifetime must be at least a day; send null to never expire",
            ));
        }
    };
    let minted =
        crate::identity::api_key::mint(state.db.pool(), sub, name, expires_at.as_deref()).await?;
    Ok((
        StatusCode::CREATED,
        Json(MintedDto {
            key: minted.key,
            secret: minted.secret,
            mcp_url: configured_mcp_url(),
        }),
    )
        .into_response())
}

/// `CONTROLLER_MCP_URL`: where the hosted MCP surface answers, when the deployment serves it on a
/// hostname of its own.
pub(crate) fn configured_mcp_url() -> Option<String> {
    std::env::var("CONTROLLER_MCP_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
}

/// `now + days`, RFC3339, matching every other stamp in the schema.
fn expiry_stamp(days: i64) -> anyhow::Result<String> {
    let span = jiff::Span::new().try_days(days)?;
    Ok(jiff::Timestamp::now().checked_add(span)?.to_string())
}

#[utoipa::path(
    delete,
    path = "/api/keys/{id}",
    params(("id" = String, Path, description = "The key's public id")),
    responses(
        (status = 204, description = "The key is revoked"),
        (status = 404, description = "The caller holds no live key with that id", body = ErrorBody),
        (status = 403, description = "The caller has no issuer subject, or presented a key", body = ErrorBody)
    )
)]
pub(crate) async fn revoke_key(
    State(state): State<ApiState>,
    subject: crate::identity::auth::Subject,
    path: crate::identity::auth::AuthPath,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    if path == crate::identity::auth::AuthPath::ApiKey {
        return Ok(not_from_a_key());
    }
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    match crate::identity::api_key::revoke(state.db.pool(), sub, &id).await? {
        true => Ok(StatusCode::NO_CONTENT.into_response()),
        false => Ok(not_found("no live api key of yours has that id")),
    }
}
