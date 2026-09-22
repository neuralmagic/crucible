use crate::api::dto::*;
use crate::dto::dto;

use crate::api::state::*;

use crate::runs::task_evidence;

use anyhow::Context as _;

use axum::extract::{Path, State};

use axum::response::{IntoResponse, Response};

use serde::Serialize;

use utoipa::ToSchema;

dto! {
    // --- per-task evidence and the run's engine log -------------------------------------------------

    /// One file a task captured, as the panel reads it.
    pub struct EvidenceFileDto: From<f: task_evidence::CapturedFile> {
        pub name: String,
        pub size_bytes: u64,
        /// The file's text; null for a file that is not decodable text or is over the inline cap, which
        /// the panel then lists by name and size alone.
        pub content: Option<String>,
    }
}

/// What one task of a run did: how it ended, what it emitted, and the files it captured.
#[derive(Debug, Serialize, ToSchema)]
pub struct TaskEvidenceDto {
    pub run_id: String,
    pub task: String,
    /// The latest attempt's terminal status; null while the task has none.
    pub status: Option<String>,
    /// The iteration the latest attempt reported in.
    pub iter: Option<i64>,
    pub note: Option<String>,
    /// How many tries the task took, as the run counted them.
    pub attempts: Option<i64>,
    /// Summed across the task's recorded attempts.
    pub cost_usd: Option<f64>,
    pub secs: Option<f64>,
    /// The payload the task emitted, from the run's session log; null for a task that emitted none
    /// and for a run whose session is not stored.
    pub payload: Option<serde_json::Value>,
    pub files: Vec<EvidenceFileDto>,
    /// True when the run is still going and the task has reported nothing terminal — the panel says
    /// so rather than reading the empty evidence as a finished task.
    pub running: bool,
}

/// `GET /api/runs/{run_id}/tasks/{task}/evidence` — one task's result payload, note, timing and
/// captured files. The files are read out of the local run's own `state/files/<task>/` and nowhere
/// else; a pod run's files never reach this machine, so it answers with the result alone. A task
/// the run neither planned nor reported on is a 404, and a run id or task name that could leave the
/// run's directory is a 400.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/tasks/{task}/evidence",
    params(
        ("run_id" = String, Path, description = "Run identifier"),
        ("task" = String, Path, description = "Task name, fan-out instance brackets included")
    ),
    responses(
        (status = 200, description = "The task's evidence", body = TaskEvidenceDto),
        (status = 400, description = "The run id or task name is not a single path segment", body = ErrorBody),
        (status = 404, description = "No such run, or no such task in it", body = ErrorBody)
    )
)]
pub(crate) async fn get_task_evidence(
    State(state): State<ApiState>,
    Path((run_id, task)): Path<(String, String)>,
) -> Result<Response, AppError> {
    if !task_evidence::safe_segment(&run_id) || !task_evidence::safe_segment(&task) {
        return Ok(bad_request(
            "a run id and a task name are each one path segment",
        ));
    }
    let Some(run) = crate::runs::store::get_run(state.db.pool(), &run_id).await? else {
        return Ok(not_found(format!("run not found: {run_id}")));
    };

    let results = crate::runs::task_results::list_task_results(state.db.pool(), &run_id).await?;
    let mine: Vec<&crate::runs::model::TaskResult> =
        results.iter().filter(|r| r.task == task).collect();
    if mine.is_empty() && !planned(state.db.pool(), &run_id, &task).await? {
        return Ok(not_found(format!("no task {task:?} in run {run_id}")));
    }
    let latest = mine.iter().max_by_key(|r| r.iter);

    // The store first, then the log a local run published in its own directory: a run whose
    // session never reached the store still has its evidence on this disk.
    let session = match crate::runs::blob_store::get_run_session(state.db.pool(), &run_id).await? {
        Some(stored) => Some(stored),
        None => task_evidence::local_session(&state.scratch_dir, &run_id).await,
    };
    let from_session = session
        .as_deref()
        .and_then(|s| task_evidence::session_result(s, &task));

    let files = task_evidence::captured_files(&state.scratch_dir, &run_id, &task).await;
    let summed = |pick: fn(&crate::runs::model::TaskResult) -> Option<f64>| {
        let vals: Vec<f64> = mine.iter().filter_map(|r| pick(r)).collect();
        (!vals.is_empty()).then(|| vals.iter().sum())
    };

    Ok(Json(TaskEvidenceDto {
        run_id: run_id.clone(),
        task,
        // The session is the executor's own last word, and can carry a failing attempt after a
        // passing one; the recorded row is the fallback when no session is stored.
        status: from_session
            .as_ref()
            .map(|s| s.status.to_string())
            .or_else(|| latest.map(|r| r.status.clone())),
        iter: latest.map(|r| r.iter),
        note: latest.map(|r| r.note.clone()),
        // The session's own count when it published one; the recorded attempts otherwise.
        attempts: from_session
            .as_ref()
            .and_then(|s| s.attempts)
            .or_else(|| (!mine.is_empty()).then(|| i64::try_from(mine.len()).unwrap_or(i64::MAX))),
        cost_usd: summed(|r| r.cost_usd),
        secs: summed(|r| r.secs),
        payload: from_session.and_then(|s| s.payload),
        files: files.into_iter().map(EvidenceFileDto::from).collect(),
        running: latest.is_none() && run.status == "running",
    })
    .into_response())
}

/// Whether the run's newest admitted plan declares this task. A fan-out instance is never declared,
/// which is why a recorded result counts on its own.
async fn planned(pool: &sqlx::PgPool, run_id: &str, task: &str) -> Result<bool, AppError> {
    let Some(plan) = crate::runs::task_results::latest_run_plan(pool, run_id).await? else {
        return Ok(false);
    };
    let tasks: Vec<crucible_contract::session::PlanTaskWire> =
        serde_json::from_str(&plan.graph_json)
            .with_context(|| format!("decoding the stored task graph for run {run_id}"))?;
    Ok(tasks.iter().any(|t| t.name == task))
}

/// A run's engine output, or where to find it.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunLogDto {
    pub run_id: String,
    /// `local` (a supervised subprocess on this machine) or `pod` (a work pod).
    pub dispatch: String,
    /// The engine output, tail-first-line-aligned and capped; null when this machine holds none.
    pub text: Option<String>,
    /// True when the head of the log was dropped to fit the cap.
    pub truncated: bool,
    /// Where the output lives when the controller holds none: the work pod for a pod run, the run's
    /// own directory for a local run that kept no log.
    pub location: Option<String>,
}

/// `GET /api/runs/{run_id}/log` — a run's engine output: the supervised subprocess for a local run,
/// and for a pod run the live pod while it runs, then the pod log kept at completion, or the
/// ingested session for a run that finished before pod logs were kept.
/// A run whose output is reachable from neither answers with where it lives; only an unknown run
/// is a 404.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/log",
    params(("run_id" = String, Path, description = "Run identifier")),
    responses(
        (status = 200, description = "The run's engine output, or where it lives", body = RunLogDto),
        (status = 400, description = "The run id is not a single path segment", body = ErrorBody),
        (status = 404, description = "Run not found", body = ErrorBody)
    )
)]
pub(crate) async fn get_run_log(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Result<Response, AppError> {
    if !task_evidence::safe_segment(&run_id) {
        return Ok(bad_request("a run id is one path segment"));
    }
    let Some(run) = crate::runs::store::get_run(state.db.pool(), &run_id).await? else {
        return Ok(not_found(format!("run not found: {run_id}")));
    };
    let dispatch = run.dispatch.as_str().to_string();
    let (text, truncated, location) = match run.dispatch {
        crate::runs::model::RunDispatch::Local => {
            match task_evidence::engine_log_tail(&state.scratch_dir, &run_id).await {
                Some((text, truncated)) => (Some(text), truncated, None),
                None => (
                    None,
                    false,
                    Some(format!(
                        "no engine log under {}",
                        task_evidence::run_dir(&state.scratch_dir, &run_id).display()
                    )),
                ),
            }
        }
        crate::runs::model::RunDispatch::Pod => pod_run_log(&state, &run, &run_id).await,
    };
    Ok(Json(RunLogDto {
        run_id,
        dispatch,
        text,
        truncated,
        location,
    })
    .into_response())
}

/// A pod run's output. A running pod is read live; once it goes terminal it is deleted within
/// minutes, so the engine log the completion edge kept is served, and the ingested session for a
/// run that finished before that existed.
async fn pod_run_log(
    state: &ApiState,
    run: &crate::runs::model::Run,
    run_id: &str,
) -> (Option<String>, bool, Option<String>) {
    if run.status == "running"
        && let Some(text) = live_pod_log(state, run).await
    {
        let (tail, truncated) = task_evidence::tail_within_cap(text.as_bytes());
        return (Some(tail), truncated, None);
    }
    match crate::runs::blob_store::get_run_engine_log(state.db.pool(), run_id).await {
        Ok(Some(log)) => {
            let (tail, truncated) = task_evidence::tail_within_cap(log.as_bytes());
            return (Some(tail), truncated, None);
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(run_id, error = %e, "reading the stored engine log failed");
        }
    }
    match crate::runs::blob_store::get_run_session(state.db.pool(), run_id).await {
        Ok(Some(session)) => {
            let (tail, truncated) = task_evidence::tail_within_cap(session.as_bytes());
            (Some(tail), truncated, None)
        }
        Ok(None) => (None, false, Some(pod_location(run, &state.pod_namespace))),
        Err(e) => {
            tracing::warn!(run_id, error = %e, "reading the stored run session failed");
            (None, false, Some(pod_location(run, &state.pod_namespace)))
        }
    }
}

/// The live pod's log on whichever cluster the run's ledger row names. `None` when the run recorded
/// no pod, or the pod is already gone.
async fn live_pod_log(state: &ApiState, run: &crate::runs::model::Run) -> Option<String> {
    let pod = run.pod.as_deref()?;
    let api =
        crate::runs::live::spoke_pod_api(&state.clusters, &run.location, &state.pod_namespace)
            .await
            .ok()?;
    match api.logs(pod, &kube::api::LogParams::default()).await {
        Ok(text) => Some(text),
        Err(e) => {
            tracing::debug!(pod, cluster = run.location.cluster, error = %e, "live pod log unavailable");
            None
        }
    }
}

/// Where a pod run's output lives when neither the pod nor the store has it, named by the cluster
/// and namespace the run actually dispatched to. A run backfilled before the namespace column
/// existed only knows the hub's own namespace.
fn pod_location(run: &crate::runs::model::Run, hub_namespace: &str) -> String {
    let cluster = &run.location.cluster;
    let namespace = match run.location.namespace.as_deref() {
        Some(ns) => ns,
        None if cluster == crate::runs::clusters::HUB_CLUSTER => hub_namespace,
        None => "(unrecorded)",
    };
    match run.pod.as_deref() {
        Some(pod) => format!("pod {pod} in namespace {namespace} on cluster {cluster}"),
        None => format!("namespace {namespace} on cluster {cluster}; this run recorded no pod"),
    }
}

// --- a run's captured files ---------------------------------------------------------------------

/// A run's captured files, as the listing serves them.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunFilesDto {
    pub run_id: String,
    /// Every file the run's tasks captured, ordered by key. Empty for a run that captured nothing
    /// and for one whose files never reached the controller.
    pub files: Vec<crate::runs::run_files::RunFile>,
}

/// `GET /api/runs/{run_id}/files` — every file the run's tasks captured, each named by the task
/// that declared it, that task's fan-out instance where it has one, and the declared path. A pod
/// run answers from the `run-files` bundle adopted at completion; a local run answers off this
/// machine's scratch disk. A task that did not produce what it declared captured nothing, so it is
/// absent rather than listed empty.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/files",
    params(("run_id" = String, Path, description = "Run identifier")),
    responses(
        (status = 200, description = "The run's captured files", body = RunFilesDto),
        (status = 400, description = "The run id is not a single path segment", body = ErrorBody),
        (status = 404, description = "Run not found", body = ErrorBody)
    )
)]
pub(crate) async fn get_run_files(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Result<Response, AppError> {
    if !task_evidence::safe_segment(&run_id) {
        return Ok(bad_request("a run id is one path segment"));
    }
    if crate::runs::store::get_run(state.db.pool(), &run_id)
        .await?
        .is_none()
    {
        return Ok(not_found(format!("run not found: {run_id}")));
    }
    let files = match crate::runs::blob_store::get_run_files(state.db.pool(), &run_id).await? {
        Some(bundle) => crate::runs::run_files::list_bundle(&bundle)?,
        None => crate::runs::run_files::list_local(&state.scratch_dir, &run_id).await,
    };
    Ok(Json(RunFilesDto { run_id, files }).into_response())
}

/// `GET /api/runs/{run_id}/files/{*key}` — the bytes of one captured file, keyed as the listing
/// reports it (`<task>/<declared path>`, fan-out brackets included). Served as an octet stream:
/// a captured file is whatever the task wrote, and guessing a content type off its extension would
/// invite a browser to render a run's own output.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/files/{key}",
    params(
        ("run_id" = String, Path, description = "Run identifier"),
        ("key" = String, Path, description = "The file's key from the listing")
    ),
    responses(
        (status = 200, description = "The file's bytes", content_type = "application/octet-stream"),
        (status = 400, description = "The run id is not a single path segment", body = ErrorBody),
        (status = 404, description = "No such run, or no such captured file in it", body = ErrorBody)
    )
)]
pub(crate) async fn get_run_file(
    State(state): State<ApiState>,
    Path((run_id, key)): Path<(String, String)>,
) -> Result<Response, AppError> {
    if !task_evidence::safe_segment(&run_id) {
        return Ok(bad_request("a run id is one path segment"));
    }
    if crate::runs::store::get_run(state.db.pool(), &run_id)
        .await?
        .is_none()
    {
        return Ok(not_found(format!("run not found: {run_id}")));
    }
    let body = match crate::runs::blob_store::get_run_files(state.db.pool(), &run_id).await? {
        Some(bundle) => crate::runs::run_files::read_bundle(&bundle, &key)?,
        None => crate::runs::run_files::read_local(&state.scratch_dir, &run_id, &key).await,
    };
    let Some(body) = body else {
        return Ok(not_found(format!("run {run_id} captured no file {key:?}")));
    };
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        body,
    )
        .into_response())
}
