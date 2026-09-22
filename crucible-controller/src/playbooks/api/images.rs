//! `POST /api/images/rank`: the catalog ranked for one pack's requirements. Lives beside the
//! playbook handlers because ranking is the preflight's matcher applied to the catalog.
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use utoipa::ToSchema;

use crate::api::dto::bad_request;
use crate::api::state::{ApiState, AppError, ErrorBody, Json};

/// What a pack asks of its image, as the picker sends it.
#[derive(Debug, serde::Deserialize, ToSchema)]
pub struct RankImagesBody {
    /// `[agent.requires]`: predicate -> version range.
    #[serde(default)]
    pub requires: std::collections::BTreeMap<String, String>,
    /// `[agent.prefers]`: predicate -> version range that orders compatible images.
    #[serde(default)]
    pub prefers: std::collections::BTreeMap<String, String>,
    /// The harness the pack declares or the launch will pin; absent is the engine default.
    #[serde(default)]
    pub harness: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/images/rank",
    request_body = RankImagesBody,
    responses(
        (status = 200, description = "The catalog ranked for these requirements: compatible images slimmest first with the promoted default marked, excluded images with their unsatisfied predicates, and unverified images", body = crate::playbooks::preflight::RankedCatalog),
        (status = 400, description = "Unknown harness", body = ErrorBody)
    )
)]
pub(crate) async fn rank_images(
    State(state): State<ApiState>,
    Json(body): Json<RankImagesBody>,
) -> Result<Response, AppError> {
    let harness = match body.harness.as_deref() {
        None => Default::default(),
        Some(name) => match crate::playbooks::providers::parse_harness(name) {
            Some(h) => h,
            None => {
                return Ok(bad_request(format!(
                    "unknown harness {name:?} (claude, hermes, codex, opencode or pi)"
                )));
            }
        },
    };
    let catalog = crate::images::store::list_images(state.db.pool()).await?;
    Ok(Json(crate::playbooks::preflight::rank(
        &catalog,
        &body.requires,
        &body.prefers,
        harness,
    ))
    .into_response())
}
