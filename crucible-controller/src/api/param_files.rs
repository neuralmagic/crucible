//! The routes that take a launcher's file parameter uploads and serve a run its launch's files
//! when they are not on its disk.

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use crucible_contract::calls::MAX_FILE_PARAM_BYTES;

use crate::api::state::{ApiError, ApiState, ErrorBody};
use crate::launches::param_files::{MAX_UPLOADS_BYTES, of_run, put_upload};
use crate::runs::pod_token::Grant;

fn refuse(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorBody::new(msg))).into_response()
}

pub(crate) async fn serve(
    Extension(grant): Extension<Grant>,
    State(state): State<ApiState>,
    UrlPath((run, digest)): UrlPath<(String, String)>,
) -> Response {
    if !grant.may_write_run(&run) {
        return refuse(
            StatusCode::FORBIDDEN,
            "this pod token reaches file parameters only of its own run",
        );
    }
    match of_run(state.db.pool(), &run, &digest).await {
        Ok(Some(bytes)) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Ok(None) => refuse(
            StatusCode::NOT_FOUND,
            format!("run {run}'s launch holds no file {digest}"),
        ),
        Err(e) => ApiError::from(e).into_response(),
    }
}

/// What an upload stored.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct UploadAck {
    pub digest: String,
    pub size: u64,
}

#[utoipa::path(
    put,
    path = "/api/param-files/{digest}",
    params(("digest" = String, Path, description = "sha256:<hex> of the body")),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 201, description = "Stored", body = UploadAck),
        (status = 401, description = "No identity to own the upload", body = ErrorBody),
        (status = 413, description = "Past the per-file or per-launcher cap", body = ErrorBody),
        (status = 422, description = "The bytes do not digest to the path's digest", body = ErrorBody)
    )
)]
pub(crate) async fn put(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    UrlPath(digest): UrlPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let Some(uploader) = caller.subject_principal().map(|p| p.to_string()) else {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "an upload needs an identity to own it",
        );
    };
    let actual = crucible_contract::artifact::content_digest(&body);
    if actual != digest {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the bytes digest to {actual}, not {digest}"),
        );
    }
    match put_upload(state.db.pool(), &uploader, &digest, &body).await {
        Ok(true) => (
            StatusCode::CREATED,
            Json(UploadAck {
                digest,
                size: u64::try_from(body.len()).unwrap_or(u64::MAX),
            }),
        )
            .into_response(),
        Ok(false) => refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("your uploads would pass {MAX_UPLOADS_BYTES} bytes; launch or wait a day"),
        ),
        Err(e) => ApiError::from(e).into_response(),
    }
}

pub(crate) fn upload_limit() -> axum::extract::DefaultBodyLimit {
    axum::extract::DefaultBodyLimit::max(
        usize::try_from(MAX_FILE_PARAM_BYTES).unwrap_or(usize::MAX),
    )
}
