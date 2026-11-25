//! The call routes: a run dispatches, polls, and cancels its calls with its pod token; anyone who
//! may read a run lists its calls with their progress.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Router};
use crucible_contract::calls::{
    CALL_CANCEL_PATH, CALL_INPUT_FILE_PATH, CALL_PATH, CALL_RESULT_FILE_PATH, CALLS_PATH, CallKind,
    CallRefused, CallState, CallStatus, CallView, DispatchRequest, DispatchResponse,
    MAX_CALL_DEPTH, MAX_FILE_PARAM_BYTES, NodeStatus, Pin, ProgressGraph,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;
use sqlx::PgPool;

use crate::api::state::{ApiError, ApiState, ErrorBody, Json};
use crate::calls::store::Claimed;
use crate::calls::{Backends, CallCtx, CallError, CallRecord, DispatchSpec, Outcome, Polled};
use crate::runs::pod_token::Grant;

/// The routes a run reaches with its own pod token.
pub(crate) fn run_routes(state: ApiState) -> Router {
    let db = state.db.clone();
    let file_cap = usize::try_from(MAX_FILE_PARAM_BYTES).unwrap_or(usize::MAX);
    Router::new()
        .route(CALLS_PATH, post(dispatch))
        .route(CALL_PATH, get(poll))
        .route(CALL_CANCEL_PATH, post(cancel))
        .route(CALL_RESULT_FILE_PATH, get(result_file))
        .route(
            CALL_INPUT_FILE_PATH,
            put(input_file).layer(axum::extract::DefaultBodyLimit::max(file_cap)),
        )
        .layer(axum::middleware::from_fn_with_state(
            db,
            crate::decisions::require_run_token,
        ))
        .with_state(state)
}

fn refuse(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, axum::Json(ErrorBody::new(msg))).into_response()
}

fn not_this_run() -> Response {
    refuse(
        StatusCode::FORBIDDEN,
        "this pod token reaches calls only for its own run",
    )
}

fn ctx(state: &ApiState) -> CallCtx {
    CallCtx {
        db: state.db.clone(),
        scratch_dir: state.scratch_dir.clone(),
    }
}

/// Why a dispatch answered without a target run.
#[derive(Debug)]
enum NotDispatched {
    Refused(String),
    /// Another request holds the identity and has not finished dispatching.
    InFlight,
    Internal(anyhow::Error),
}

impl From<CallError> for NotDispatched {
    fn from(e: CallError) -> Self {
        match e {
            CallError::Internal(e) => NotDispatched::Internal(e),
            other => NotDispatched::Refused(other.note()),
        }
    }
}

impl From<anyhow::Error> for NotDispatched {
    fn from(e: anyhow::Error) -> Self {
        NotDispatched::Internal(e)
    }
}

fn answered(record: &CallRecord) -> Result<DispatchResponse, NotDispatched> {
    match &record.handle {
        Some(handle) => Ok(DispatchResponse {
            call: record.id.clone(),
            handle: handle.clone(),
            link: record.link.clone(),
        }),
        None => Err(NotDispatched::InFlight),
    }
}

async fn dispatch(
    Extension(grant): Extension<Grant>,
    State(state): State<ApiState>,
    Path(run): Path<String>,
    Json(request): Json<DispatchRequest>,
) -> Response {
    if !grant.may_write_run(&run) {
        return not_this_run();
    }
    match dispatch_call(&state, &run, &request).await {
        Ok(response) => axum::Json(response).into_response(),
        Err(NotDispatched::Refused(note)) => {
            tracing::info!(%run, task = %request.task, alias = %request.alias, %note, "call refused");
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(CallRefused { note }),
            )
                .into_response()
        }
        Err(NotDispatched::InFlight) => refuse(
            StatusCode::CONFLICT,
            "this call is being dispatched by another request; retry",
        ),
        Err(NotDispatched::Internal(e)) => ApiError::from(e).into_response(),
    }
}

async fn dispatch_call(
    state: &ApiState,
    run: &str,
    request: &DispatchRequest,
) -> Result<DispatchResponse, NotDispatched> {
    let pool = state.db.pool();
    let refused = |note: String| Err(NotDispatched::Refused(note));
    let Some(parent) = crate::calls::store::parent(pool, run).await? else {
        return refused(format!("run {run} is not a playbook launch's run"));
    };
    if parent.depth.saturating_add(1) >= MAX_CALL_DEPTH {
        return refused(format!(
            "run {run} is already the {} in a chain of calling runs, and may call no further",
            ordinal(parent.depth.saturating_add(1))
        ));
    }
    let slug = crate::model::sanitize_key(&parent.launch);
    let Some(tree) = crate::runs::blob_store::get_pack(pool, &slug).await? else {
        return refused(format!("launch {} stored no pack", parent.launch));
    };
    let (calls, lock) = match crate::calls::lock::of_tree(&tree) {
        Ok(found) => found,
        Err(note) => return refused(note),
    };
    let (Some(target), Some(locked)) = (
        calls.get(&request.alias),
        lock.as_ref().and_then(|l| l.calls.get(&request.alias)),
    ) else {
        return refused(format!(
            "the pack declares no call target {}",
            request.alias
        ));
    };
    if request.interface_digest != locked.interface_digest {
        return refused(format!(
            "the call was compiled against interface {} but calls.lock records {}",
            request.interface_digest, locked.interface_digest
        ));
    }
    if matches!(locked.pin, Pin::DraftHead { .. }) && !parent.draft {
        return refused(format!(
            "call target {} pins a draft's head, which only a draft pack may do",
            request.alias
        ));
    }
    if let Err(note) = crate::calls::lock::check_bound(
        &locked.interface,
        request.values.keys().chain(request.files.keys()),
    ) {
        return refused(note);
    }
    for (name, file) in &request.files {
        if !crate::calls::store::has_input_file(pool, run, &file.digest).await? {
            return refused(format!(
                "file parameter {name:?} names {}, which this run did not upload",
                file.digest
            ));
        }
    }
    let bound = json!({
        "values": request.values,
        "files": request
            .files
            .iter()
            .map(|(name, f)| (name.clone(), json!({"digest": f.digest, "size": f.size})))
            .collect::<BTreeMap<_, _>>(),
    });
    if let Some(live) = crate::calls::store::live_earlier(
        pool,
        run,
        &request.task,
        request.instance.as_deref(),
        request.attempt,
    )
    .await?
    {
        tracing::info!(%run, task = %request.task, attempt = request.attempt, call = %live.id, "call already running under an earlier attempt");
        return answered(&live);
    }
    let claimed = crate::calls::store::claim(
        pool,
        &crate::calls::store::NewCall {
            parent_run: run,
            task: &request.task,
            instance: request.instance.as_deref(),
            attempt: request.attempt,
            alias: &request.alias,
            target,
            interface: &locked.interface,
            bound,
            budget: request.budget,
        },
    )
    .await?;
    let id = match claimed {
        Claimed::Existing(record) => return held(pool, &record).await,
        Claimed::Fresh(id) => id,
    };
    let backends = Backends::new(state);
    let ctx = ctx(state);
    let checked = async {
        let backend = backends.get(target.kind())?;
        if locked.pin.moves() {
            let now = backend.interface(&ctx, target).await?;
            let digest = now
                .interface
                .digest()
                .map_err(|e| CallError::Internal(e.into()))?;
            if digest != locked.interface_digest {
                return Err(CallError::Refused(format!(
                    "the interface of call target {} changed since calls.lock was written: {}",
                    request.alias,
                    crate::calls::lock::drift(&locked.interface, &now.interface)
                )));
            }
        }
        Ok(backend)
    }
    .await;
    let backend = match checked {
        Ok(backend) => backend,
        Err(e) => {
            crate::calls::store::release(pool, &id).await?;
            return Err(e.into());
        }
    };
    let started = backend
        .dispatch(
            &ctx,
            &DispatchSpec {
                call: &id,
                parent: &parent,
                task: &request.task,
                instance: request.instance.as_deref(),
                alias: &request.alias,
                target,
                locked,
                values: &request.values,
                files: &request.files,
                budget: request.budget,
            },
        )
        .await;
    match started {
        Ok(started) => {
            crate::calls::store::dispatched(pool, &id, &started).await?;
            tracing::info!(%run, task = %request.task, alias = %request.alias, call = %id, "call dispatched");
            Ok(DispatchResponse {
                call: id,
                handle: started.handle,
                link: started.link,
            })
        }
        Err(CallError::Internal(e)) => {
            let note = format!(
                "dispatching failed after it may have started a target this controller does not \
                 follow: {e:#}"
            );
            crate::calls::store::settle(
                pool,
                &id,
                &Outcome::failed(note.clone()),
                &ProgressGraph::default(),
            )
            .await?;
            tracing::warn!(%run, task = %request.task, alias = %request.alias, call = %id, %note, "call dispatch failed");
            Err(NotDispatched::Refused(note))
        }
        Err(e) => {
            crate::calls::store::release(pool, &id).await?;
            Err(e.into())
        }
    }
}

/// How long a claimed call may go without a recorded target before its dispatch counts as lost.
const DISPATCH_LOST_AFTER: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Answer a dispatch whose identity another request already holds.
async fn held(pool: &PgPool, record: &CallRecord) -> Result<DispatchResponse, NotDispatched> {
    if record.handle.is_some() {
        return answered(record);
    }
    if record.status.ended() {
        return Err(NotDispatched::Refused(record.note.clone()));
    }
    let claimed: jiff::Timestamp = record.created_at.parse().map_err(|e| {
        NotDispatched::Internal(
            anyhow::Error::new(e).context(format!("call {} created_at", record.id)),
        )
    })?;
    let lost = jiff::Timestamp::now()
        .duration_since(claimed)
        .unsigned_abs()
        > DISPATCH_LOST_AFTER;
    if !lost {
        return Err(NotDispatched::InFlight);
    }
    let note = "this controller lost the dispatch before it recorded a target; a target may have \
                started that it does not follow"
        .to_string();
    crate::calls::store::settle(
        pool,
        &record.id,
        &Outcome::failed(note.clone()),
        &ProgressGraph::default(),
    )
    .await?;
    Err(NotDispatched::Refused(note))
}

fn ordinal(n: u32) -> String {
    match n {
        1 => "first".to_string(),
        2 => "second".to_string(),
        3 => "third".to_string(),
        4 => "fourth".to_string(),
        n => format!("{n}th"),
    }
}

/// Whether a call's own wall clock ran out. A playbook target run carries the same ceiling
/// itself, so only other kinds are cut off here.
fn past_wall_clock(record: &CallRecord, now: jiff::Timestamp) -> Option<u64> {
    if record.kind == CallKind::Playbook {
        return None;
    }
    let secs = record.budget.and_then(|b| b.wall_clock_secs)?;
    let started: jiff::Timestamp = record.created_at.parse().ok()?;
    let elapsed = now.duration_since(started).as_secs();
    (u64::try_from(elapsed).unwrap_or(0) > secs).then_some(secs)
}

/// Bring a dispatched, unended call up to date with its target run.
pub(crate) async fn refresh(
    backends: &Backends,
    ctx: &CallCtx,
    record: CallRecord,
) -> anyhow::Result<CallRecord> {
    if record.status.ended() || record.handle.is_none() {
        return Ok(record);
    }
    let pool = ctx.db.pool();
    let backend = match backends.get(record.kind) {
        Ok(backend) => backend,
        Err(e) => {
            crate::calls::store::settle(
                pool,
                &record.id,
                &Outcome::failed(e.note()),
                &record.progress,
            )
            .await?;
            return reread(ctx, record).await;
        }
    };
    if let Some(secs) = past_wall_clock(&record, jiff::Timestamp::now()) {
        if let Err(e) = backend.cancel(ctx, &record).await {
            tracing::warn!(call = %record.id, error = %e.note(), "cancelling a call past its wall clock failed");
        }
        crate::calls::store::settle(
            pool,
            &record.id,
            &Outcome::failed(format!(
                "the target run ran past the call's wall clock of {secs}s"
            )),
            &record.progress,
        )
        .await?;
        return reread(ctx, record).await;
    }
    match backend.poll(ctx, &record).await {
        Ok(Polled {
            progress,
            outcome: Some(outcome),
        }) => {
            crate::calls::store::settle(pool, &record.id, &outcome, &progress).await?;
        }
        Ok(Polled {
            progress,
            outcome: None,
        }) => {
            let status = match progress
                .nodes
                .iter()
                .any(|n| n.status != NodeStatus::Pending)
            {
                true => CallStatus::Running,
                false => record.status,
            };
            crate::calls::store::progressed(pool, &record.id, status, &progress).await?;
        }
        Err(CallError::Internal(e)) => {
            tracing::warn!(call = %record.id, error = %format!("{e:#}"), "following a call's target run failed; its last state stands");
        }
        Err(refused) => {
            if let Err(e) = backend.cancel(ctx, &record).await {
                tracing::warn!(call = %record.id, error = %e.note(), "cancelling a call whose target run cannot be followed failed");
            }
            crate::calls::store::settle(
                pool,
                &record.id,
                &Outcome::failed(refused.note()),
                &record.progress,
            )
            .await?;
        }
    }
    reread(ctx, record).await
}

async fn reread(ctx: &CallCtx, record: CallRecord) -> anyhow::Result<CallRecord> {
    Ok(
        crate::calls::store::get(ctx.db.pool(), &record.parent_run, &record.id.0)
            .await?
            .unwrap_or(record),
    )
}

fn state_of(record: CallRecord) -> Option<CallState> {
    Some(CallState {
        handle: record.handle?,
        call: record.id,
        status: record.status,
        note: record.note,
        result: record.result,
        result_files: record.result_files,
        progress: record.progress,
        link: record.link,
    })
}

#[allow(clippy::result_large_err)]
async fn found(state: &ApiState, run: &str, call: &str) -> Result<CallRecord, Response> {
    match crate::calls::store::get(state.db.pool(), run, call).await {
        Ok(Some(record)) if record.handle.is_some() => Ok(record),
        Ok(_) => Err(refuse(
            StatusCode::NOT_FOUND,
            format!("run {run} has no call {call}"),
        )),
        Err(e) => Err(ApiError::from(e).into_response()),
    }
}

fn answer(record: CallRecord) -> Response {
    match state_of(record) {
        Some(state) => axum::Json(state).into_response(),
        None => refuse(StatusCode::CONFLICT, "the call is still being dispatched"),
    }
}

async fn poll(
    Extension(grant): Extension<Grant>,
    State(state): State<ApiState>,
    Path((run, call)): Path<(String, String)>,
) -> Response {
    if !grant.may_write_run(&run) {
        return not_this_run();
    }
    let record = match found(&state, &run, &call).await {
        Ok(record) => record,
        Err(response) => return response,
    };
    match refresh(&Backends::new(&state), &ctx(&state), record).await {
        Ok(record) => answer(record),
        Err(e) => ApiError::from(e).into_response(),
    }
}

async fn cancel(
    Extension(grant): Extension<Grant>,
    State(state): State<ApiState>,
    Path((run, call)): Path<(String, String)>,
) -> Response {
    if !grant.may_write_run(&run) {
        return not_this_run();
    }
    let record = match found(&state, &run, &call).await {
        Ok(record) => record,
        Err(response) => return response,
    };
    let ctx = ctx(&state);
    match cancel_one(
        &Backends::new(&state),
        &ctx,
        record,
        "cancelled by its calling run",
    )
    .await
    {
        Ok(record) => answer(record),
        Err(e) => ApiError::from(e).into_response(),
    }
}

/// Stop a call's target run and settle the call cancelled.
pub(crate) async fn cancel_one(
    backends: &Backends,
    ctx: &CallCtx,
    record: CallRecord,
    note: &str,
) -> anyhow::Result<CallRecord> {
    if record.status.ended() {
        return Ok(record);
    }
    match backends.get(record.kind) {
        Ok(backend) => {
            if let Err(e) = backend.cancel(ctx, &record).await {
                tracing::warn!(call = %record.id, error = %e.note(), "cancelling a call's target run failed");
            }
        }
        Err(e) => {
            tracing::warn!(call = %record.id, error = %e.note(), "no backend to cancel a call's target run")
        }
    }
    crate::calls::store::settle(
        ctx.db.pool(),
        &record.id,
        &Outcome {
            status: CallStatus::Cancelled,
            note: note.to_string(),
            result: Value::Null,
            result_files: Vec::new(),
        },
        &record.progress,
    )
    .await?;
    reread(ctx, record).await
}

/// Cancel every open call of the runs of launch `key`: the launch is being cancelled.
pub(crate) async fn cancel_for_launch(
    db: &crate::client::Db,
    cfg: &crate::config::ControllerCfg,
    key: &str,
) -> anyhow::Result<usize> {
    let backends = Backends::following(cfg.calls_github_token);
    let ctx = CallCtx {
        db: db.clone(),
        scratch_dir: cfg.scratch_root().to_path_buf(),
    };
    let open = crate::calls::store::open_for_launch(db.pool(), key).await?;
    let count = open.len();
    for record in open {
        cancel_one(&backends, &ctx, record, "cancelled with its calling run").await?;
    }
    Ok(count)
}

async fn result_file(
    Extension(grant): Extension<Grant>,
    State(state): State<ApiState>,
    Path((run, call, path)): Path<(String, String, String)>,
) -> Response {
    if !grant.may_write_run(&run) {
        return not_this_run();
    }
    let record = match found(&state, &run, &call).await {
        Ok(record) => record,
        Err(response) => return response,
    };
    if record.status != CallStatus::Passed {
        return refuse(
            StatusCode::CONFLICT,
            "the call has not passed, so it has no result files",
        );
    }
    let backends = Backends::new(&state);
    let read = match backends.get(record.kind) {
        Ok(backend) => backend.result_file(&ctx(&state), &record, &path).await,
        Err(e) => Err(e),
    };
    match read {
        Ok(Some(bytes)) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Ok(None) => refuse(
            StatusCode::NOT_FOUND,
            format!("the call returned no file {path:?}"),
        ),
        Err(CallError::Internal(e)) => ApiError::from(e).into_response(),
        Err(other) => refuse(StatusCode::NOT_FOUND, other.note()),
    }
}

async fn input_file(
    Extension(grant): Extension<Grant>,
    State(state): State<ApiState>,
    Path((run, digest)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    if !grant.may_write_run(&run) {
        return not_this_run();
    }
    let actual = format!("sha256:{:x}", sha2::Sha256::digest(&body));
    if actual != digest {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the bytes digest to {actual}, not {digest}"),
        );
    }
    match crate::calls::store::put_input_file(state.db.pool(), &run, &digest, &body).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => ApiError::from(e).into_response(),
    }
}

/// `GET /api/runs/{run}/call-progress`: every call the run dispatched, each brought up to date.
#[utoipa::path(
    get,
    path = "/api/runs/{run}/call-progress",
    params(("run" = String, Path, description = "Calling run")),
    responses(
        (status = 200, description = "The run's calls with their progress", body = [Object]),
        (status = 500, description = "Reading the calls failed", body = ErrorBody)
    )
)]
pub(crate) async fn call_progress(
    State(state): State<ApiState>,
    Path(run): Path<String>,
) -> Response {
    let pool = state.db.pool();
    let records = match crate::calls::store::list(pool, &run).await {
        Ok(records) => records,
        Err(e) => return ApiError::from(e).into_response(),
    };
    let backends = Backends::new(&state);
    let ctx = ctx(&state);
    let mut views = Vec::with_capacity(records.len());
    for record in records {
        let record = match refresh(&backends, &ctx, record.clone()).await {
            Ok(fresh) => fresh,
            Err(e) => {
                tracing::warn!(call = %record.id, error = format!("{e:#}"), "refreshing a call for a reader failed");
                record
            }
        };
        let (task, instance, attempt, alias, kind) = (
            record.task.clone(),
            record.instance.clone(),
            record.attempt,
            record.alias.clone(),
            record.kind,
        );
        if let Some(state) = state_of(record) {
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

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct LockBody {
    /// The pack's `crucible.toml` and workflow sources, by path.
    pub files: BTreeMap<String, String>,
    /// Whether the pack is a draft, which alone may pin a draft's head.
    #[serde(default)]
    pub draft: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct LockAnswer {
    /// The `calls.lock` text; null when the manifest declares no calls.
    pub lock: Option<String>,
}

/// `POST /api/calls/lock`: the lock a pack's `calls` blocks resolve to now, for a pack written
/// outside the controller.
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
    Json(body): Json<LockBody>,
) -> Response {
    let calls = match crate::calls::lock::declared(|path| body.files.get(path).cloned()) {
        Ok(calls) => calls,
        Err(note) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, note),
    };
    match crate::calls::lock::write(&Backends::new(&state), &ctx(&state), &calls, body.draft).await
    {
        Ok(lock) => match lock.map(|l| l.render()).transpose() {
            Ok(lock) => axum::Json(LockAnswer { lock }).into_response(),
            Err(e) => ApiError::from(anyhow::Error::from(e)).into_response(),
        },
        Err(note) => refuse(StatusCode::UNPROCESSABLE_ENTITY, note),
    }
}

/// The route the human API writes a lock on.
pub(crate) const LOCK_ROUTE: &str = "/api/calls/lock";

#[cfg(test)]
mod tests {
    use crate::api::state::ApiState;
    use crate::calls::api::*;
    use crate::client::Db;
    use axum::body::Body;
    use axum::http::Request;
    use crucible_contract::calls::{CallAlias, TargetHandle};
    use sqlx::PgPool;

    const TARGET_WORKFLOW: &str = concat!(
        "params = {\"topic\": {\"type\": \"string\", \"required\": True}}\n",
        "hello = command(name = \"hello\", run = \"echo hello\", emits = {\"greeting\": \"string\"})\n",
        "workflow(type = \"playbook\", tasks = [hello], result = hello)\n",
    );

    const CALLER_MANIFEST: &str =
        "[agent]\n\n[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n";

    const CALLER_WORKFLOW: &str = concat!(
        "params = {}\n",
        "calls = {\"target\": {\"kind\": \"playbook\", \"playbook\": \"draft:target\"}}\n",
        "hello = command(name = \"hello\", run = \"echo hello\")\n",
        "workflow(type = \"playbook\", tasks = [hello], result = hello)\n",
    );

    fn wren() -> crate::authz::model::Principal {
        crate::authz::model::Principal::User("wren".into())
    }

    fn state(pool: PgPool) -> ApiState {
        ApiState {
            playbook_caps: crate::config::PlaybookCaps {
                max_cost: 10.0,
                max_time: crate::model::MaxTime::parse("2h").expect("cap"),
            },
            ..ApiState::test(Db::new(pool))
        }
    }

    async fn draft(pool: &PgPool, id: &str, files: BTreeMap<String, String>) {
        crate::playbooks::drafts::create(
            pool,
            id,
            id,
            crate::playbooks::drafts::DraftSeed::Skeleton,
            Some("wren"),
            &wren(),
        )
        .await
        .expect("create draft");
        let saved = crate::playbooks::drafts::save_version(pool, id, files, Some("wren"), None)
            .await
            .expect("save");
        assert!(
            saved.schema_digest.is_some(),
            "{id} compiles: {:?}",
            saved.diagnostics
        );
    }

    fn files(manifest: &str, workflow: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("crucible.toml".to_string(), manifest.to_string()),
            ("workflow.star".to_string(), workflow.to_string()),
        ])
    }

    fn target_files() -> BTreeMap<String, String> {
        files(crate::playbooks::drafts::SKELETON_MANIFEST, TARGET_WORKFLOW)
    }

    /// The caller draft, saved with the lock its `[calls]` resolves to, launched as `key` with one
    /// running run `run`. Returns the interface digest the lock records.
    async fn calling_run(state: &ApiState, key: &str, run: &str) -> String {
        let pool = state.db.pool();
        draft(pool, "target", target_files()).await;
        let mut caller = files(CALLER_MANIFEST, CALLER_WORKFLOW);
        crate::calls::lock::into_files(&Backends::new(state), &ctx(state), &mut caller)
            .await
            .expect("lock written");
        assert!(caller.contains_key("calls.lock"));
        draft(pool, "caller", caller).await;
        let version = crate::playbooks::drafts::latest(pool, "caller")
            .await
            .expect("latest")
            .expect("saved")
            .version;
        let max_time = crate::model::MaxTime::parse("30m").expect("duration");
        let params = json!({});
        let mut tx = pool.begin().await.expect("tx");
        assert!(
            crate::launches::store::insert_playbook_launch_with(
                &mut tx,
                key,
                &crate::launches::model::NewPlaybookLaunch {
                    choice_snapshots: None,
                    playbook: "caller",
                    repo: "(draft)",
                    title: "caller",
                    params: &params,
                    schema_digest: "sha256:schema",
                    max_cost: 1.0,
                    max_time: &max_time,
                    advance_dedupe: false,
                    dedupe_schedule: None,
                    origin: crate::model::LaunchOrigin::Draft,
                    draft_version: Some(version),
                    created_by: Some("wren"),
                    owner: Some("user:wren"),
                    launcher_groups: None,
                    rerun: None,
                    dispatch: Default::default(),
                },
                &crate::playbooks::exposure::Extraction::Absent,
            )
            .await
            .expect("insert launch")
        );
        tx.commit().await.expect("commit");
        running(pool, key, run, "running").await;
        let tree = crate::runs::blob_store::get_pack(pool, &crate::model::sanitize_key(key))
            .await
            .expect("pack")
            .expect("stored");
        let (_, lock) = crate::calls::lock::of_tree(&tree).expect("lock");
        lock.expect("locked").calls[&alias()]
            .interface_digest
            .clone()
    }

    async fn running(pool: &PgPool, key: &str, run: &str, status: &str) {
        crate::runs::store::insert_run(
            pool,
            &crate::NewRun {
                run_id: run.to_string(),
                scope: None,
                issue: Some(key.to_string()),
                identity_digest: None,
                status: status.to_string(),
                pod: None,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await
        .expect("run");
    }

    fn alias() -> CallAlias {
        CallAlias::parse("target").expect("alias")
    }

    fn request(digest: &str, attempt: u32, values: Value) -> DispatchRequest {
        serde_json::from_value(json!({
            "task": "ask",
            "attempt": attempt,
            "alias": "target",
            "interface_digest": digest,
            "values": values,
        }))
        .expect("request")
    }

    async fn mint_token(pool: &PgPool, run: &str) -> String {
        crate::runs::pod_token::mint(
            pool,
            &crate::runs::pod_token::Subject::Run(run.to_string()),
            None,
            std::time::Duration::from_secs(3600),
        )
        .await
        .expect("token")
    }

    fn post(path: &str, token: &str, body: &DispatchRequest) -> Request<Body> {
        Request::post(path)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).expect("body")))
            .expect("request")
    }

    fn get(path: &str, token: &str) -> Request<Body> {
        Request::get(path)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("request")
    }

    type ChildRow = (String, Option<String>, i32, Option<String>);

    async fn child_launches(pool: &PgPool) -> Vec<ChildRow> {
        sqlx::query_as(
            "SELECT key, parent_task, call_depth, reserved_run FROM playbook_launches \
             WHERE parent_run = 'caller-run' ORDER BY created_at, key",
        )
        .fetch_all(pool)
        .await
        .expect("children")
    }

    const CALLS: &str = "/api/runs/caller-run/calls";

    /// A call dispatches one target run per identity: a resumed caller re-POSTing the same task,
    /// instance and attempt gets the same call and handle back, and nothing new is launched.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_dispatch_is_idempotent_on_its_identity(pool: PgPool) {
        let state = state(pool.clone());
        let digest = calling_run(&state, "playbook:caller:1", "caller-run").await;
        let token = mint_token(&pool, "caller-run").await;
        let app = run_routes(state.clone());
        let first = request(&digest, 1, json!({"topic": "attention"}));

        let (status, body) = crate::testing::call(&app, post(CALLS, &token, &first)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let answered: DispatchResponse = serde_json::from_value(body).expect("response");
        let TargetHandle::Playbook { run: target_run } = &answered.handle else {
            panic!("a playbook target answers a playbook handle");
        };
        let (status, again) = crate::testing::call(&app, post(CALLS, &token, &first)).await;
        assert_eq!(status, StatusCode::OK, "{again}");
        let again: DispatchResponse = serde_json::from_value(again).expect("response");
        assert_eq!(again, answered, "the same identity answers the same call");
        let children = child_launches(&pool).await;
        assert_eq!(children.len(), 1, "one target launch: {children:?}");
        let (child_key, task, depth, reserved) = &children[0];
        assert_eq!(task.as_deref(), Some("ask"));
        assert_eq!(*depth, 1);
        assert_eq!(reserved.as_ref(), Some(target_run));
        assert_eq!(
            crate::calls::store::reserved_run(&pool, child_key)
                .await
                .expect("reserved"),
            Some(target_run.clone()),
            "the target launch's first dispatch takes the run the handle names"
        );
        let bound: Value = sqlx::query_scalar("SELECT bound FROM call_target_runs WHERE id = $1")
            .bind(&answered.call.0)
            .fetch_one(&pool)
            .await
            .expect("bound");
        assert_eq!(bound["values"], json!({"topic": "attention"}));
        let params: Value =
            sqlx::query_scalar("SELECT params FROM playbook_launches WHERE key = $1")
                .bind(child_key)
                .fetch_one(&pool)
                .await
                .expect("params");
        assert_eq!(params["topic"], "attention");

        let second = request(&digest, 2, json!({"topic": "x"}));
        let (status, live) = crate::testing::call(&app, post(CALLS, &token, &second)).await;
        assert_eq!(status, StatusCode::OK, "{live}");
        assert_eq!(
            live["call"],
            json!(answered.call.0),
            "a later attempt while the first call still runs gets that call"
        );
        assert_eq!(child_launches(&pool).await.len(), 1);

        crate::calls::store::settle(
            &pool,
            &answered.call,
            &crate::calls::Outcome::failed("target failed"),
            &crucible_contract::calls::ProgressGraph::default(),
        )
        .await
        .expect("settle");
        let third = request(&digest, 3, json!({"topic": "x"}));
        let (status, third) = crate::testing::call(&app, post(CALLS, &token, &third)).await;
        assert_eq!(status, StatusCode::OK, "{third}");
        assert_ne!(
            third["call"],
            json!(answered.call.0),
            "an attempt after the call ended is another call"
        );
        assert_eq!(child_launches(&pool).await.len(), 2);

        running(&pool, "playbook:caller:1", "other-run", "running").await;
        let other = mint_token(&pool, "other-run").await;
        let (status, _) = crate::testing::call(&app, post(CALLS, &other, &first)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    /// A dispatch the controller lost before it recorded a target is refused once it is stale,
    /// and a later attempt starts afresh instead of waiting on it forever.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_lost_dispatch_settles_failing_instead_of_holding_its_identity(pool: PgPool) {
        let state = state(pool.clone());
        let digest = calling_run(&state, "playbook:caller:1", "caller-run").await;
        let token = mint_token(&pool, "caller-run").await;
        let app = run_routes(state.clone());
        let first = request(&digest, 1, json!({"topic": "attention"}));
        let (status, body) = crate::testing::call(&app, post(CALLS, &token, &first)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let call = body["call"].as_str().expect("call").to_owned();

        sqlx::query("UPDATE call_target_runs SET handle = NULL WHERE id = $1")
            .bind(&call)
            .execute(&pool)
            .await
            .expect("unrecord");
        let (status, body) = crate::testing::call(&app, post(CALLS, &token, &first)).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a fresh claim is in flight: {body}"
        );

        sqlx::query(
            "UPDATE call_target_runs SET created_at = '2020-01-01T00:00:00Z' WHERE id = $1",
        )
        .bind(&call)
        .execute(&pool)
        .await
        .expect("age");
        let (status, body) = crate::testing::call(&app, post(CALLS, &token, &first)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            body["note"]
                .as_str()
                .is_some_and(|n| n.contains("lost the dispatch")),
            "{body}"
        );
        let status: String =
            sqlx::query_scalar("SELECT status FROM call_target_runs WHERE id = $1")
                .bind(&call)
                .fetch_one(&pool)
                .await
                .expect("status");
        assert_eq!(status, "failed");
        let (status, body) = crate::testing::call(&app, post(CALLS, &token, &first)).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the settled identity keeps answering its note: {body}"
        );

        let second = request(&digest, 2, json!({"topic": "attention"}));
        let (status, body) = crate::testing::call(&app, post(CALLS, &token, &second)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_ne!(body["call"], json!(call));
    }

    /// What the controller refuses settles the call failing with a note, and creates nothing.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_refused_dispatch_creates_no_target_run(pool: PgPool) {
        let state = state(pool.clone());
        let digest = calling_run(&state, "playbook:caller:1", "caller-run").await;
        let token = mint_token(&pool, "caller-run").await;
        let app = run_routes(state.clone());
        for (refused, says) in [
            (
                request("sha256:other", 1, json!({"topic": "a"})),
                "compiled against",
            ),
            (
                request(&digest, 1, json!({})),
                "required parameter \"topic\"",
            ),
            (
                request(&digest, 1, json!({"topic": "a", "extra": 1})),
                "no parameter \"extra\"",
            ),
        ] {
            let (status, body) = crate::testing::call(&app, post(CALLS, &token, &refused)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let note = body["note"].as_str().unwrap_or_default();
            assert!(note.contains(says), "{note}");
        }
        sqlx::query("UPDATE playbook_launches SET call_depth = 3 WHERE key = 'playbook:caller:1'")
            .execute(&pool)
            .await
            .expect("depth");
        let deep = request(&digest, 1, json!({"topic": "a"}));
        let (status, body) = crate::testing::call(&app, post(CALLS, &token, &deep)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            body["note"].as_str().unwrap_or_default().contains("fourth"),
            "{body}"
        );
        let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM call_target_runs")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(calls, 0);
        assert!(child_launches(&pool).await.is_empty());
    }

    /// A call follows its target run: pending on the locked graph before it runs, passed with the
    /// result task's output once it finishes, and cancelled when its caller cancels it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_call_follows_its_target_run_to_its_outcome(pool: PgPool) {
        let state = state(pool.clone());
        let digest = calling_run(&state, "playbook:caller:1", "caller-run").await;
        let token = mint_token(&pool, "caller-run").await;
        let app = run_routes(state.clone());
        let dispatch = |attempt| request(&digest, attempt, json!({"topic": "attention"}));
        let (_, first) = crate::testing::call(&app, post(CALLS, &token, &dispatch(1))).await;
        let call = first["call"].as_str().expect("call").to_string();
        let poll = format!("{CALLS}/{call}");

        let (status, polled) = crate::testing::call(&app, get(&poll, &token)).await;
        assert_eq!(status, StatusCode::OK, "{polled}");
        let polled: CallState = serde_json::from_value(polled).expect("state");
        assert_eq!(polled.status, CallStatus::Pending);
        assert_eq!(polled.progress.nodes.len(), 1);
        assert_eq!(polled.progress.nodes[0].id, "hello");
        assert_eq!(polled.progress.nodes[0].status, NodeStatus::Pending);

        let TargetHandle::Playbook { run: target_run } = polled.handle.clone() else {
            panic!("playbook handle");
        };
        let child: String =
            sqlx::query_scalar("SELECT key FROM playbook_launches WHERE reserved_run = $1")
                .bind(&target_run)
                .fetch_one(&pool)
                .await
                .expect("child");
        running(&pool, &child, &target_run, "finished").await;
        crate::runs::blob_store::put_run_session(
            &pool,
            &target_run,
            concat!(
                r#"{"v":1,"kind":"task_started","task":"hello","attempt":1,"estimate_usd":0.0}"#,
                "\n",
                r#"{"v":1,"kind":"task_result","task":"hello","status":"pass","output":{"greeting":"hi"}}"#,
                "\n",
            )
            .as_bytes(),
        )
        .await
        .expect("session");
        let (_, passed) = crate::testing::call(&app, get(&poll, &token)).await;
        let passed: CallState = serde_json::from_value(passed).expect("state");
        assert_eq!(passed.status, CallStatus::Passed, "{}", passed.note);
        assert_eq!(passed.result, json!({"greeting": "hi"}));
        assert_eq!(
            passed.progress.nodes[0].status,
            NodeStatus::Settled(crucible_contract::session::TaskStatus::Pass)
        );

        let (_, second) = crate::testing::call(&app, post(CALLS, &token, &dispatch(2))).await;
        let second_call = second["call"].as_str().expect("call").to_string();
        let cancel = Request::post(format!("{CALLS}/{second_call}/cancel"))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("request");
        let (status, cancelled) = crate::testing::call(&app, cancel).await;
        assert_eq!(status, StatusCode::OK, "{cancelled}");
        assert_eq!(cancelled["status"], "cancelled");
        let stashed: Vec<String> =
            sqlx::query_scalar("SELECT key FROM pending_overrides WHERE kind = 'cancel'")
                .fetch_all(&pool)
                .await
                .expect("overrides");
        assert_eq!(
            stashed.len(),
            1,
            "the cancelled call's target launch is cancelled"
        );

        let listed = call_progress(State(state.clone()), Path("caller-run".to_string())).await;
        let body = axum::body::to_bytes(listed.into_body(), usize::MAX)
            .await
            .expect("body");
        let views: Vec<CallView> = serde_json::from_slice(&body).expect("views");
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].state.status, CallStatus::Passed);
        assert_eq!(views[1].state.status, CallStatus::Cancelled);

        let children = crate::calls::store::children(&pool, "caller-run")
            .await
            .expect("children");
        assert_eq!(children.len(), 2);
        assert_eq!(
            crate::calls::store::called_from(&pool, &target_run)
                .await
                .expect("parent"),
            Some(crate::calls::store::CalledFrom {
                run: "caller-run".into(),
                task: "ask".into(),
                instance: None,
            })
        );
    }

    /// A lock pins only what can be resolved, and a draft's head only from a draft pack.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_lock_refuses_what_it_cannot_pin(pool: PgPool) {
        let state = state(pool.clone());
        let backends = Backends::new(&state);
        let missing = crate::calls::lock::declared(|path| {
            (path == "crucible.toml")
                .then(|| CALLER_MANIFEST.to_string())
                .or_else(|| (path == "workflow.star").then(|| "calls = {\"gone\": {\"kind\": \"playbook\", \"playbook\": \"draft:nowhere\"}}\n".to_string()))
        })
        .expect("declared");
        let refused = crate::calls::lock::write(&backends, &ctx(&state), &missing, true)
            .await
            .expect_err("unresolvable");
        assert!(refused.starts_with("call target gone"), "{refused}");

        draft(&pool, "target", target_files()).await;
        let head = crate::calls::lock::declared(|path| {
            (path == "crucible.toml")
                .then(|| CALLER_MANIFEST.to_string())
                .or_else(|| (path == "workflow.star").then(|| "calls = {\"target\": {\"kind\": \"playbook\", \"playbook\": \"draft:target\"}}\n".to_string()))
        })
        .expect("declared");
        let refused = crate::calls::lock::write(&backends, &ctx(&state), &head, false)
            .await
            .expect_err("a registered pack may not pin a draft head");
        assert!(refused.contains("only a draft pack"), "{refused}");
        let lock = crate::calls::lock::write(&backends, &ctx(&state), &head, true)
            .await
            .expect("a draft pack may")
            .expect("declared calls lock");
        let locked = &lock.calls[&alias()];
        assert_eq!(
            locked.pin,
            Pin::DraftHead {
                draft: "target".into()
            }
        );
        assert_eq!(locked.interface.result[0].field, "greeting");
        let github = crate::calls::lock::declared(|path| {
            (path == "crucible.toml")
                .then(|| CALLER_MANIFEST.to_string())
                .or_else(|| {
                    (path == "workflow.star").then(|| {
                        concat!(
                "calls = {\"ci\": {\"kind\": \"github_workflow\", \"repo\": \"acme/app\", ",
                "\"workflow\": \".github/workflows/ci.yml\", \"ref\": \"main\", ",
                "\"outputs\": {\"artifact\": \"result\"}}}\n",
            ).to_string()
                    })
                })
        })
        .expect("declared");
        let refused = crate::calls::lock::write(&backends, &ctx(&state), &github, true)
            .await
            .expect_err("no token source");
        assert!(
            refused.contains("CONTROLLER_CALLS_GITHUB_TOKEN"),
            "{refused}"
        );
    }

    fn record(kind: CallKind, created_at: &str) -> CallRecord {
        CallRecord {
            id: crucible_contract::calls::CallId("c".into()),
            parent_run: "r".into(),
            task: "t".into(),
            instance: None,
            attempt: 1,
            alias: alias(),
            kind,
            target: crucible_contract::calls::CallTarget::Playbook {
                playbook: crucible_contract::calls::PlaybookRef::Draft("d".into()),
            },
            interface: crucible_contract::calls::TargetInterface {
                params: json!({}),
                result: vec![],
                result_files: vec![],
                graph: Default::default(),
            },
            handle: None,
            detail: json!({}),
            budget: Some(crucible_contract::calls::CallBudget {
                cost_usd: None,
                wall_clock_secs: Some(60),
            }),
            status: CallStatus::Running,
            note: String::new(),
            result: Value::Null,
            result_files: vec![],
            progress: Default::default(),
            link: None,
            created_at: created_at.to_string(),
        }
    }

    #[test]
    fn a_call_past_its_wall_clock_is_found_only_for_kinds_without_their_own() {
        let now: jiff::Timestamp = "2026-11-25T10:02:00Z".parse().expect("now");
        assert_eq!(
            past_wall_clock(
                &record(CallKind::GithubWorkflow, "2026-11-25T10:00:00Z"),
                now
            ),
            Some(60)
        );
        assert_eq!(
            past_wall_clock(
                &record(CallKind::GithubWorkflow, "2026-11-25T10:01:30Z"),
                now
            ),
            None
        );
        assert_eq!(
            past_wall_clock(&record(CallKind::Playbook, "2026-11-25T10:00:00Z"), now),
            None,
            "a playbook target run holds the same ceiling itself"
        );
    }

    #[test]
    fn a_depth_reads_as_its_place_in_the_chain() {
        assert_eq!(ordinal(1), "first");
        assert_eq!(ordinal(4), "fourth");
        assert_eq!(ordinal(7), "7th");
    }
}
