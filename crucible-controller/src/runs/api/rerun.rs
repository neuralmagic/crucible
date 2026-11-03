//! `GET /api/runs/{run_id}/rerun`: what rerunning a finished playbook run from some of its tasks
//! would reuse and dispatch again.

use crate::api::dto::bad_request;
use crate::api::state::{ApiResult, ApiState, ErrorBody};
use crate::runs::rerun::RerunPreviewDto;
use crate::runs::task_evidence;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RerunQuery {
    /// The tasks to dispatch again, comma-separated. Empty reruns only what did not pass.
    #[serde(default)]
    from: String,
}

impl RerunQuery {
    fn tasks(&self) -> Vec<String> {
        self.from
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// `GET /api/runs/{run_id}/rerun` — preview a rerun of a finished playbook run against the pack
/// the rerun would run: the newest draft save, or the registered pack. Reused tasks keep their
/// output, files and commits; the rest dispatch again.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/rerun",
    params(("run_id" = String, Path, description = "Run identifier"), RerunQuery),
    responses(
        (status = 200, description = "What the rerun would reuse and dispatch", body = RerunPreviewDto),
        (status = 400, description = "The run id is not a single path segment", body = ErrorBody),
        (status = 404, description = "Run not found", body = ErrorBody),
        (status = 422, description = "The run cannot be rerun as asked", body = ErrorBody)
    )
)]
pub(crate) async fn preview_rerun(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    Query(q): Query<RerunQuery>,
) -> ApiResult<Response> {
    if !task_evidence::safe_segment(&run_id) {
        return Ok(bad_request("a run id is one path segment"));
    }
    match crate::runs::rerun::preview(&state.db, &run_id, &q.tasks()).await {
        Ok(preview) => Ok(Json(preview).into_response()),
        Err(refusal) => Err(refusal.into()),
    }
}

/// A launch body's request to rerun a finished run of the same pack instead of starting afresh.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct RerunBody {
    /// The finished run to rerun.
    of: String,
    /// The tasks to dispatch again. Empty reruns only what did not pass.
    #[serde(default)]
    from: Vec<String>,
}

/// Admit a launch's rerun of `playbook`, or the refusal.
pub(crate) async fn admit_launch(
    state: &ApiState,
    playbook: &str,
    draft: bool,
    rerun: Option<&RerunBody>,
) -> ApiResult<Option<crate::launches::model::RerunOf>> {
    let Some(rerun) = rerun else {
        return Ok(None);
    };
    crate::runs::rerun::admit(&state.db, playbook, draft, &rerun.of, &rerun.from).await?;
    Ok(Some(crate::launches::model::RerunOf {
        run: rerun.of.clone(),
        from: rerun.from.clone(),
    }))
}
