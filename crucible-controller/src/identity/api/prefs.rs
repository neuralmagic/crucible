//! Per-user UI preferences, in two homes. `/api/prefs` is session state ([`crate::identity::session`]) that
//! lapses with the inactivity window; `/api/prefs/editor` is a document in [`crate::identity::user_prefs`],
//! keyed by the identity the session is bound to, so a fresh browser reads it back. Both bodies
//! are opaque to the server, and the size cap is the `DefaultBodyLimit` layer on the routes,
//! applied before the JSON is buffered or parsed.

use crate::api::state::{ApiState, AppError, Json};
use crate::identity::session::BoundSession;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The session slot the blob lives under.
const PREFS_KEY: &str = "prefs";
/// Request-body cap for `PUT /api/prefs`: a session row is not a document store.
pub(crate) const MAX_PREFS_BYTES: usize = 16 * 1024;

/// An opaque client-owned preferences blob. The server enforces only the size cap.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PrefsDto {
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub prefs: serde_json::Value,
}

#[utoipa::path(
    get,
    path = "/api/prefs",
    responses(
        (status = 200, description = "The caller's session-stored UI preferences; `{}` when none were ever saved", body = PrefsDto)
    )
)]
pub(crate) async fn get_prefs(bound: BoundSession) -> Result<Json<PrefsDto>, AppError> {
    let prefs = bound
        .get::<serde_json::Value>(PREFS_KEY)
        .await?
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(Json(PrefsDto { prefs }))
}

#[utoipa::path(
    put,
    path = "/api/prefs",
    request_body = PrefsDto,
    responses(
        (status = 200, description = "Preferences stored in the caller's session", body = PrefsDto),
        (status = 403, description = "No identity on the request — prefs are per-user"),
        (status = 413, description = "Request body exceeds the 16 KiB cap")
    )
)]
pub(crate) async fn put_prefs(
    mut bound: BoundSession,
    Json(body): Json<PrefsDto>,
) -> Result<Response, AppError> {
    if bound.identity().is_none() {
        return Ok((
            StatusCode::FORBIDDEN,
            "prefs need an authenticated identity",
        )
            .into_response());
    }
    bound.insert(PREFS_KEY, &body.prefs).await?;
    Ok(Json(body).into_response())
}

/// An editor-preferences document. Same opaque-blob contract as [`PrefsDto`], but stored against
/// the caller's identity in `user_prefs`, so a fresh browser reads back what was set.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct EditorPrefsDto {
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub prefs: serde_json::Value,
}

async fn get_document(
    state: &ApiState,
    bound: &BoundSession,
    kind: &str,
) -> Result<serde_json::Value, AppError> {
    let Some(identity) = bound.identity() else {
        return Ok(serde_json::json!({}));
    };
    Ok(
        crate::identity::user_prefs::get(state.db.pool(), &identity.user, kind)
            .await?
            .unwrap_or_else(|| serde_json::json!({})),
    )
}

async fn put_document(
    state: &ApiState,
    bound: &BoundSession,
    kind: &str,
    doc: &serde_json::Value,
) -> Result<Option<Response>, AppError> {
    let Some(identity) = bound.identity() else {
        return Ok(Some(
            (
                StatusCode::FORBIDDEN,
                format!("{kind} prefs need an authenticated identity"),
            )
                .into_response(),
        ));
    };
    crate::identity::user_prefs::put(state.db.pool(), &identity.user, kind, doc).await?;
    Ok(None)
}

#[utoipa::path(
    get,
    path = "/api/prefs/editor",
    responses(
        (status = 200, description = "The caller's stored editor preferences; `{}` when none were ever saved", body = EditorPrefsDto)
    )
)]
pub(crate) async fn get_editor_prefs(
    State(state): State<ApiState>,
    bound: BoundSession,
) -> Result<Json<EditorPrefsDto>, AppError> {
    let prefs = get_document(&state, &bound, crate::identity::user_prefs::EDITOR_KIND).await?;
    Ok(Json(EditorPrefsDto { prefs }))
}

#[utoipa::path(
    put,
    path = "/api/prefs/editor",
    request_body = EditorPrefsDto,
    responses(
        (status = 200, description = "Preferences stored against the caller's identity", body = EditorPrefsDto),
        (status = 403, description = "No identity on the request — prefs are per-user"),
        (status = 413, description = "Request body exceeds the 16 KiB cap")
    )
)]
pub(crate) async fn put_editor_prefs(
    State(state): State<ApiState>,
    bound: BoundSession,
    Json(body): Json<EditorPrefsDto>,
) -> Result<Response, AppError> {
    if let Some(refusal) = put_document(
        &state,
        &bound,
        crate::identity::user_prefs::EDITOR_KIND,
        &body.prefs,
    )
    .await?
    {
        return Ok(refusal);
    }
    Ok(Json(body).into_response())
}

/// The pickers' document: what a user starred and how they last filtered, stored against their
/// identity like the editor's settings so it follows them to a fresh browser.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PickerPrefsDto {
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub prefs: serde_json::Value,
}

#[utoipa::path(
    get,
    path = "/api/prefs/pickers",
    responses(
        (status = 200, description = "The caller's stored picker preferences; `{}` when none were ever saved", body = PickerPrefsDto)
    )
)]
pub(crate) async fn get_picker_prefs(
    State(state): State<ApiState>,
    bound: BoundSession,
) -> Result<Json<PickerPrefsDto>, AppError> {
    let prefs = get_document(&state, &bound, crate::identity::user_prefs::PICKERS_KIND).await?;
    Ok(Json(PickerPrefsDto { prefs }))
}

#[utoipa::path(
    put,
    path = "/api/prefs/pickers",
    request_body = PickerPrefsDto,
    responses(
        (status = 200, description = "Preferences stored against the caller's identity", body = PickerPrefsDto),
        (status = 403, description = "No identity on the request — prefs are per-user"),
        (status = 413, description = "Request body exceeds the 16 KiB cap")
    )
)]
pub(crate) async fn put_picker_prefs(
    State(state): State<ApiState>,
    bound: BoundSession,
    Json(body): Json<PickerPrefsDto>,
) -> Result<Response, AppError> {
    if let Some(refusal) = put_document(
        &state,
        &bound,
        crate::identity::user_prefs::PICKERS_KIND,
        &body.prefs,
    )
    .await?
    {
        return Ok(refusal);
    }
    Ok(Json(body).into_response())
}
