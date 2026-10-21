//! `GET /api/runs/{run_id}/rerun`: what rerunning a finished playbook run from some of its tasks
//! would reuse and dispatch again.

use crate::api::dto::{bad_request, not_found, unprocessable};
use crate::api::state::{ApiState, AppError, ErrorBody};
use crate::runs::rerun::{RerunPreviewDto, RerunRefusal};
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

/// The answer a refusal gives: a missing run is a 404, anything the engine would refuse a 422.
pub(crate) fn refused(refusal: RerunRefusal) -> Result<Response, AppError> {
    match refusal {
        RerunRefusal::NoRun(_) => Ok(not_found(refusal.to_string())),
        RerunRefusal::Internal(e) => Err(AppError::from(e)),
        other => Ok(unprocessable(other.to_string())),
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
) -> Result<Response, AppError> {
    if !task_evidence::safe_segment(&run_id) {
        return Ok(bad_request("a run id is one path segment"));
    }
    match crate::runs::rerun::preview(&state.db, &run_id, &q.tasks()).await {
        Ok(preview) => Ok(Json(preview).into_response()),
        Err(refusal) => refused(refusal),
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

/// Admit a launch's rerun of `playbook`, or the response that refuses it. Only a controller that
/// runs playbooks on its own machine can lay a parent's evidence out for the engine.
pub(crate) async fn admit_launch(
    state: &ApiState,
    playbook: &str,
    draft: bool,
    rerun: Option<&RerunBody>,
) -> Result<Option<crate::launches::model::RerunOf>, Box<Response>> {
    let Some(rerun) = rerun else {
        return Ok(None);
    };
    if !state.dispatch.is_local() {
        return Err(Box::new(unprocessable(
            "this controller runs playbooks in pods, which cannot rerun yet",
        )));
    }
    match crate::runs::rerun::admit(&state.db, playbook, draft, &rerun.of, &rerun.from).await {
        Ok(()) => Ok(Some(crate::launches::model::RerunOf {
            run: rerun.of.clone(),
            from: rerun.from.clone(),
        })),
        Err(refusal) => Err(Box::new(match refused(refusal) {
            Ok(response) => response,
            Err(e) => e.into_response(),
        })),
    }
}
