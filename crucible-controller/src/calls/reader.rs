//! The call routes a person reaches: a run's calls with their progress, one progress node's log,
//! and writing a lock for a pack held outside the controller. Each needs read on the calling run
//! or on every playbook target.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use crucible_contract::calls::CallView;
use serde::{Deserialize, Serialize};

use crate::api::state::{ApiError, ApiState, ErrorBody, Json};
use crate::authz::Caller;
use crate::calls::{Backends, CallCtx, CallError, CallRecord};

fn ctx(state: &ApiState) -> CallCtx {
    CallCtx {
        db: state.db.clone(),
        scratch_dir: state.scratch_dir.clone(),
    }
}

fn refuse(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, axum::Json(ErrorBody::new(msg))).into_response()
}

fn no_run(run: &str) -> Response {
    refuse(StatusCode::NOT_FOUND, format!("run not found: {run}"))
}

#[allow(clippy::result_large_err)]
async fn readable_run(state: &ApiState, caller: &Caller, run: &str) -> Result<(), Response> {
    match crate::calls::access::may_read_run(state, caller, run).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(no_run(run)),
        Err(e) => Err(ApiError::from(e).into_response()),
    }
}

async fn as_seen(
    state: &ApiState,
    caller: &Caller,
    mut record: CallRecord,
) -> anyhow::Result<CallRecord> {
    let Some(target) = record
        .handle
        .as_ref()
        .and_then(|h| crate::calls::access::guarded_run(record.kind, h))
    else {
        return Ok(record);
    };
    if crate::calls::access::may_read_run(state, caller, target).await? {
        return Ok(record);
    }
    record.progress = crate::calls::access::labels_only(&record.progress);
    record.link = None;
    Ok(record)
}

#[utoipa::path(
    get,
    path = "/api/runs/{run}/call-progress",
    params(("run" = String, Path, description = "Calling run")),
    responses(
        (status = 200, description = "The run's calls with their progress", body = [Object]),
        (status = 404, description = "No run the caller may read", body = ErrorBody),
        (status = 500, description = "Reading the calls failed", body = ErrorBody)
    )
)]
pub(crate) async fn call_progress(
    State(state): State<ApiState>,
    caller: Caller,
    Path(run): Path<String>,
) -> Response {
    if let Err(refused) = readable_run(&state, &caller, &run).await {
        return refused;
    }
    let pool = state.db.pool();
    let records = match crate::calls::store::list(pool, &run).await {
        Ok(records) => records,
        Err(e) => return ApiError::from(e).into_response(),
    };
    let mut views = Vec::with_capacity(records.len());
    for record in records {
        let record = match as_seen(&state, &caller, record).await {
            Ok(record) => record,
            Err(e) => return ApiError::from(e).into_response(),
        };
        let (task, instance, attempt, alias, kind) = (
            record.task.clone(),
            record.instance.clone(),
            record.attempt,
            record.alias.clone(),
            record.kind,
        );
        if let Some(state) = crate::calls::api::state_of(record) {
            views.push(CallView {
                task,
                instance,
                attempt,
                alias,
                kind,
                state,
            });
        }
    }
    axum::Json(views).into_response()
}

#[utoipa::path(
    get,
    path = "/api/runs/{run}/calls/{call}/nodes/{node}/log",
    params(
        ("run" = String, Path, description = "Calling run"),
        ("call" = String, Path, description = "Call id"),
        ("node" = String, Path, description = "Progress node id, percent-encoded as one segment")
    ),
    responses(
        (status = 200, description = "The node's log", body = String, content_type = "text/plain"),
        (status = 404, description = "No run, call, or node the caller may read", body = ErrorBody)
    )
)]
pub(crate) async fn node_log(
    State(state): State<ApiState>,
    caller: Caller,
    Path((run, call, node)): Path<(String, String, String)>,
) -> Response {
    if let Err(refused) = readable_run(&state, &caller, &run).await {
        return refused;
    }
    let record = match crate::calls::store::get(state.db.pool(), &run, &call).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return refuse(
                StatusCode::NOT_FOUND,
                format!("run {run} has no call {call}"),
            );
        }
        Err(e) => return ApiError::from(e).into_response(),
    };
    let Some(handle) = record.handle.as_ref() else {
        return refuse(
            StatusCode::NOT_FOUND,
            format!("run {run} has no call {call}"),
        );
    };
    if let Some(target) = crate::calls::access::guarded_run(record.kind, handle) {
        match crate::calls::access::may_read_run(&state, &caller, target).await {
            Ok(true) => {}
            Ok(false) => {
                return refuse(
                    StatusCode::NOT_FOUND,
                    format!("call {call} has no node {node:?} you may read"),
                );
            }
            Err(e) => return ApiError::from(e).into_response(),
        }
    }
    if !record.progress.nodes.iter().any(|n| n.id == node) {
        return refuse(
            StatusCode::NOT_FOUND,
            format!("call {call} has no node {node:?}"),
        );
    }
    let backends = Backends::new(&state);
    let read = match backends.get(record.kind) {
        Ok(backend) => backend.node_log(&ctx(&state), &record, &node).await,
        Err(e) => Err(e),
    };
    match read {
        Ok(text) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        Err(CallError::Internal(e)) => ApiError::from(e).into_response(),
        Err(other) => refuse(StatusCode::NOT_FOUND, other.note()),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct LockBody {
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub draft_id: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct LockAnswer {
    pub lock: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/calls/lock",
    request_body = LockBody,
    responses(
        (status = 200, description = "The calls.lock text", body = LockAnswer),
        (status = 422, description = "A target could not be pinned", body = ErrorBody)
    )
)]
pub(crate) async fn write_lock(
    State(state): State<ApiState>,
    caller: Caller,
    Json(body): Json<LockBody>,
) -> Response {
    let calls = match crate::calls::lock::declared(|path| body.files.get(path).cloned()) {
        Ok(calls) => calls,
        Err(note) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, note),
    };
    let locking = crate::calls::locking::Locking {
        state: &state,
        caller: &caller,
        draft_pack: body.draft,
        draft: body.draft_id.as_deref(),
    };
    match crate::calls::locking::write(&Backends::new(&state), &ctx(&state), &calls, &locking).await
    {
        Ok(lock) => match lock.map(|l| l.render()).transpose() {
            Ok(lock) => axum::Json(LockAnswer { lock }).into_response(),
            Err(e) => ApiError::from(anyhow::Error::from(e)).into_response(),
        },
        Err(note) => refuse(StatusCode::UNPROCESSABLE_ENTITY, note),
    }
}

pub(crate) const LOCK_ROUTE: &str = "/api/calls/lock";
