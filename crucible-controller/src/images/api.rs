use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use crucible_capability::CapabilityDoc;
use serde::Serialize;
use utoipa::ToSchema;

use crate::api::state::{ApiState, AppError, ErrorBody, Json};
use crate::dto::dto;
use crate::images::model::{CatalogImage, RepositoryStatus};

dto! {
    /// One catalogued sandbox image: a digest in a watched repository.
    #[derive(Clone, PartialEq)]
    pub struct CatalogImageDto: From<i: CatalogImage> {
        /// The repository's last path segment.
        pub name: String = i.name().to_string(),
        /// The image carries a capability document the controller could read.
        pub verified: bool = i.capabilities.is_some(),
        /// Repository reference without tag or digest.
        pub repository: String,
        pub digest: String,
        /// Channel tags currently pointing at this digest.
        pub tags: Vec<String>,
        pub arches: Vec<String>,
        /// The image config's `created` timestamp, when present.
        pub created_at: Option<String>,
        /// The capability document off the image config; absent on an unverified image.
        pub capabilities: Option<CapabilityDoc>,
        /// sha256 of the capability label as stamped.
        pub capability_digest: Option<String>,
        pub intro_digest: Option<String>,
        pub first_seen: String,
        pub last_seen: String,
    }
}

dto! {
    /// A watched repository's last poll.
    pub struct CatalogRepositoryDto: From<r: RepositoryStatus> {
        pub repository: String,
        pub last_polled: String,
        pub last_ok: Option<String>,
        pub last_error: Option<String>,
    }
}

/// The catalog: every image plus each repository's poll state.
#[derive(Debug, Serialize, ToSchema)]
pub struct CatalogDto {
    pub images: Vec<CatalogImageDto>,
    pub repositories: Vec<CatalogRepositoryDto>,
}

#[utoipa::path(
    get,
    path = "/api/images",
    responses(
        (status = 200, description = "The image catalog: cached rows plus each watched repository's last poll", body = CatalogDto)
    )
)]
pub(crate) async fn list_images(State(state): State<ApiState>) -> Result<Response, AppError> {
    let images = crate::images::store::list_images(state.db.pool()).await?;
    let repositories = crate::images::store::list_repositories(state.db.pool()).await?;
    Ok(Json(CatalogDto {
        images: images.into_iter().map(Into::into).collect(),
        repositories: repositories.into_iter().map(Into::into).collect(),
    })
    .into_response())
}

#[utoipa::path(
    post,
    path = "/api/images/refresh",
    responses(
        (status = 202, description = "A catalog sweep was requested; the watcher runs it now"),
        (status = 403, description = "Caller is not an admin", body = ErrorBody)
    )
)]
pub(crate) async fn refresh_images(
    State(state): State<ApiState>,
    _admin: crate::identity::auth::AdminGuard,
) -> Response {
    state.images_refresh.notify_one();
    StatusCode::ACCEPTED.into_response()
}
