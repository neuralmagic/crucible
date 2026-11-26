//! The call routes: a run dispatches, polls, and cancels its calls with its pod token; anyone who
//! may read a run lists its calls with their progress.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Router};
use crucible_contract::calls::{
    CALL_CANCEL_PATH, CALL_INPUT_FILE_PATH, CALL_PATH, CALL_RESULT_FILE_PATH, CALLS_PATH,
    CallRefused, CallState, CallStatus, CallTarget, DispatchRequest, DispatchResponse,
    MAX_CALL_DEPTH, MAX_FILE_PARAM_BYTES, PARAM_FILE_PATH, Pin, PlaybookRef, ProgressGraph,
};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::api::state::{ApiError, ApiState, ErrorBody, Json};
use crate::calls::store::Claimed;
use crate::calls::{Backends, CallCtx, CallError, CallRecord, DispatchSpec, Outcome};
use crate::runs::pod_token::Grant;

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
        .route(PARAM_FILE_PATH, get(crate::launches::param_files::serve))
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
    if let Err(note) = planned(pool, &parent, tree.clone(), request).await? {
        return refused(note);
    }
    if let Err(note) = crate::calls::lock::check_bound(
        &locked.interface,
        request.values.keys().chain(request.files.keys()),
    )
    .and_then(|()| {
        crate::calls::lock::check_values(
            target.kind(),
            &locked.interface,
            &request.values,
            &request.files,
        )
    }) {
        return refused(note);
    }
    for (name, file) in &request.files {
        match crate::calls::store::input_file_size(pool, run, &file.digest).await? {
            Some(size) if size == file.size => {}
            Some(size) => {
                return refused(format!(
                    "parameter {name:?} says {} bytes, but this run uploaded {size}",
                    file.size
                ));
            }
            None => {
                return refused(format!(
                    "parameter {name:?} names {}, which this run did not upload",
                    file.digest
                ));
            }
        }
    }
    if let CallTarget::Playbook { playbook } = target
        && let Some(note) = calling_again(pool, run, &request.alias, playbook).await?
    {
        return refused(note);
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
        let owner = crate::authz::model::Principal::stored(parent.owner.as_deref());
        backend.authorize(&ctx, &owner, target).await?;
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
            let stop = recorded(pool, &id, &started).await?;
            tracing::info!(%run, task = %request.task, alias = %request.alias, call = %id, "call dispatched");
            if stop && let Some(record) = crate::calls::store::get(pool, run, &id.0).await? {
                tracing::info!(%run, call = %id, "call cancelled while it was dispatched; stopping its target run");
                stop_target(&backends, &ctx, &record).await?;
                crate::calls::store::settle(
                    pool,
                    &id,
                    &Outcome::cancelled("cancelled while it was dispatched"),
                    &record.progress,
                )
                .await?;
            }
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

const RECORD_ATTEMPTS: u32 = 4;

async fn recorded(
    pool: &PgPool,
    id: &crucible_contract::calls::CallId,
    started: &crate::calls::Dispatched,
) -> anyhow::Result<bool> {
    let mut attempt = 1;
    loop {
        match crate::calls::store::dispatched(pool, id, started).await {
            Ok(stop) => return Ok(stop),
            Err(e) if attempt < RECORD_ATTEMPTS => {
                tracing::warn!(call = %id, attempt, error = %format!("{e:#}"), "recording a started dispatch failed; trying again");
                tokio::time::sleep(std::time::Duration::from_millis(250) * attempt).await;
                attempt += 1;
            }
            Err(e) => {
                return Err(e.context(format!(
                    "call {id} started {:?} but could not record it",
                    started.handle
                )));
            }
        }
    }
}

async fn planned(
    pool: &PgPool,
    parent: &crate::calls::Parent,
    tree: crucible_contract::pack_tree::PackTree,
    request: &DispatchRequest,
) -> anyhow::Result<Result<(), String>> {
    let variant = crate::launches::store::launch_variant(pool, &parent.launch).await?;
    let compiled = match crate::calls::compile::compile_tree(tree, variant).await {
        Ok(compiled) => compiled,
        Err(CallError::Internal(e)) => return Err(e),
        Err(refused) => return Ok(Err(refused.note())),
    };
    let task = &request.task;
    Ok(match compiled.calls.get(task) {
        None => Err(format!(
            "run {}'s plan has no call task {task:?}",
            parent.run
        )),
        Some(call) if call.alias != request.alias => Err(format!(
            "call task {task:?} calls target {}, not {}",
            call.alias, request.alias
        )),
        Some(call) if call.mapped && request.instance.is_none() => Err(format!(
            "call task {task:?} is mapped, so a dispatch names its instance"
        )),
        Some(call) if !call.mapped && request.instance.is_some() => Err(format!(
            "call task {task:?} is not mapped, so a dispatch names no instance"
        )),
        Some(_) => Ok(()),
    })
}

async fn calling_again(
    pool: &PgPool,
    run: &str,
    alias: &crucible_contract::calls::CallAlias,
    target: &PlaybookRef,
) -> anyhow::Result<Option<String>> {
    let (id, draft) = match target {
        PlaybookRef::Registered(name) => (name.as_str(), false),
        PlaybookRef::Draft(id) => (id.as_str(), true),
    };
    let chain = crate::calls::store::chain(pool, run).await?;
    if !chain.iter().any(|c| c.playbook == id && c.draft == draft) {
        return Ok(None);
    }
    let names: Vec<&str> = chain.iter().rev().map(|c| c.playbook.as_str()).collect();
    Ok(Some(format!(
        "call target {alias} is playbook {id}, which is already in this chain of calling runs \
         ({} -> {id})",
        names.join(" -> ")
    )))
}

const DISPATCH_LOST_AFTER: std::time::Duration = std::time::Duration::from_secs(10 * 60);

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

pub(crate) async fn reread(ctx: &CallCtx, record: CallRecord) -> anyhow::Result<CallRecord> {
    Ok(
        crate::calls::store::get(ctx.db.pool(), &record.parent_run, &record.id.0)
            .await?
            .unwrap_or(record),
    )
}

pub(crate) fn state_of(record: CallRecord) -> Option<CallState> {
    Some(CallState {
        run_number: record.detail.get("run_number").and_then(Value::as_u64),
        links: record
            .detail
            .get(crucible_contract::calls::LINKS_DETAIL_KEY)
            .and_then(|links| serde_json::from_value(links.clone()).ok())
            .unwrap_or_default(),
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
    match found(&state, &run, &call).await {
        Ok(record) => answer(record),
        Err(response) => response,
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

pub(crate) async fn stop_target(
    backends: &Backends,
    ctx: &CallCtx,
    record: &CallRecord,
) -> anyhow::Result<()> {
    if record.handle.is_none() {
        return Ok(());
    }
    let stopped = match backends.get(record.kind) {
        Ok(backend) => backend.cancel(ctx, record).await,
        Err(e) => Err(e),
    };
    match stopped {
        Ok(()) => Ok(()),
        Err(CallError::Internal(e)) => {
            tracing::warn!(call = %record.id, error = %format!("{e:#}"), "stopping a call's target run failed; the refresh loop asks again");
            crate::calls::store::owe_cancel(ctx.db.pool(), &record.id, true).await
        }
        Err(e) => {
            tracing::warn!(call = %record.id, error = %e.note(), "a call's target run cannot be stopped");
            Ok(())
        }
    }
}

pub(crate) async fn cancel_one(
    backends: &Backends,
    ctx: &CallCtx,
    record: CallRecord,
    note: &str,
) -> anyhow::Result<CallRecord> {
    if record.status.ended() {
        return Ok(record);
    }
    stop_target(backends, ctx, &record).await?;
    crate::calls::store::settle(
        ctx.db.pool(),
        &record.id,
        &Outcome::cancelled(note),
        &record.progress,
    )
    .await?;
    reread(ctx, record).await
}

pub(crate) async fn cancel_for_launch(
    db: &crate::client::Db,
    cfg: &crate::config::ControllerCfg,
    key: &str,
) -> anyhow::Result<usize> {
    let backends = Backends::following(cfg);
    let ctx = CallCtx {
        db: db.clone(),
        scratch_dir: cfg.scratch_root().to_path_buf(),
    };
    let open = crate::calls::store::open_for_launch(db.pool(), key).await?;
    let count = open.len();
    let mut failed = Vec::new();
    for record in open {
        let id = record.id.clone();
        if let Err(e) = cancel_one(&backends, &ctx, record, "cancelled with its calling run").await
        {
            tracing::warn!(call = %id, error = %format!("{e:#}"), "cancelling a call with its launch failed");
            failed.push(format!("{id}: {e:#}"));
        }
    }
    match failed.is_empty() {
        true => Ok(count),
        false => Err(CancelsFailed { count, failed }.into()),
    }
}

/// Some of a launch's calls could not be cancelled; the rest were.
#[derive(Debug, thiserror::Error)]
#[error("cancelling {} of {count} calls failed: {}", failed.len(), failed.join("; "))]
pub(crate) struct CancelsFailed {
    count: usize,
    failed: Vec<String>,
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
    let actual = crucible_contract::artifact::content_digest(&body);
    if actual != digest {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the bytes digest to {actual}, not {digest}"),
        );
    }
    match crate::calls::store::put_input_file(state.db.pool(), &run, &digest, &body).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "run {run}'s call input files would pass {} bytes",
                crate::calls::store::MAX_RUN_INPUT_BYTES
            ),
        ),
        Err(e) => ApiError::from(e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use crate::api::state::ApiState;
    use crate::calls::api::*;
    use crate::client::Db;
    use axum::body::Body;
    use axum::http::Request;
    use crucible_contract::calls::{CallAlias, NodeStatus, TargetHandle};
    use sqlx::PgPool;

    async fn refreshed(state: &ApiState) {
        let later = jiff::Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(60))
            .expect("a minute on");
        crate::calls::refresh::refresh_due(&Backends::new(state), &ctx(state), later)
            .await
            .expect("refresh pass");
    }

    const TARGET_WORKFLOW: &str = concat!(
        "params = {\"topic\": {\"type\": \"string\", \"required\": True}}\n",
        "hello = command(name = \"hello\", run = \"echo hello\", emits = {\"greeting\": \"string\"})\n",
        "workflow(type = \"playbook\", tasks = [hello], result = hello)\n",
    );

    const CALLER_MANIFEST: &str =
        "[agent]\n\n[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n";

    const CALLER_WORKFLOW: &str = concat!(
        "params = {}\n",
        "calls = {\"target\": {\"kind\": \"playbook\", \"playbook\": \"draft:target\"}, ",
        "\"filer\": {\"kind\": \"playbook\", \"playbook\": \"draft:filer\"}}\n",
        "hello = command(name = \"hello\", run = \"echo hello\", emits_files = [\"NOTES.md\"])\n",
        "ask = call(name = \"ask\", target = \"target\", params = {\"topic\": \"attention\"})\n",
        "file = call(name = \"file\", target = \"filer\", ",
        "params = {\"notes\": hello.files[\"NOTES.md\"]}, depends_on = [hello])\n",
        "workflow(type = \"playbook\", tasks = [hello, ask, file], result = hello)\n",
    );

    const FILER_WORKFLOW: &str = concat!(
        "params = {\"notes\": {\"type\": \"file\", \"required\": True, \"max_bytes\": 64, ",
        "\"media_type\": [\"text/markdown\"]}}\n",
        "read = command(name = \"read\", run = \"true\")\n",
        "workflow(type = \"playbook\", tasks = [read])\n",
    );

    fn wren() -> crate::authz::model::Principal {
        crate::authz::model::Principal::User("wren".into())
    }

    async fn caller(pool: &PgPool, login: &str) -> crate::authz::Caller {
        crate::authz::resolve_caller(
            pool,
            &crate::identity::auth::Roles::new(vec![], vec![], vec![]),
            Some(login),
            &[],
            crate::identity::auth::AuthPath::Open,
        )
        .await
        .expect("caller")
    }

    fn state(pool: PgPool) -> ApiState {
        ApiState {
            playbook_caps: crate::config::PlaybookCaps {
                max_cost: 10.0,
                max_time: crate::model::MaxTime::parse("2h").expect("cap"),
            },
            scratch_dir: std::env::temp_dir().join(format!("calls-{}", uuid::Uuid::now_v7())),
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

    async fn launched(pool: &PgPool, key: &str, run: &str, draft: &str) {
        let version = crate::playbooks::drafts::latest(pool, draft)
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
                    playbook: draft,
                    repo: "(draft)",
                    title: draft,
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
    }

    async fn calling_run(state: &ApiState, key: &str, run: &str) -> String {
        let pool = state.db.pool();
        draft(pool, "target", target_files()).await;
        draft(
            pool,
            "filer",
            files(crate::playbooks::drafts::SKELETON_MANIFEST, FILER_WORKFLOW),
        )
        .await;
        let wren = caller(pool, "wren").await;
        let mut caller = files(CALLER_MANIFEST, CALLER_WORKFLOW);
        crate::calls::lock::into_files(
            &Backends::new(state),
            &ctx(state),
            &crate::calls::lock::Locking {
                state,
                caller: &wren,
                draft_pack: true,
                draft: Some("caller"),
            },
            &mut caller,
        )
        .await
        .expect("lock written");
        assert!(caller.contains_key("calls.lock"));
        draft(pool, "caller", caller).await;
        launched(pool, key, run, "caller").await;
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
                r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{"name":"hello","kind":"command","required":true}]}"#,
                "\n",
                r#"{"v":1,"kind":"task_started","task":"hello","attempt":1,"estimate_usd":0.0}"#,
                "\n",
                r#"{"v":1,"kind":"task_result","task":"hello","status":"pass","output":{"greeting":"hi"}}"#,
                "\n",
                r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":""}"#,
                "\n",
            )
            .as_bytes(),
        )
        .await
        .expect("session");
        let (_, unrefreshed) = crate::testing::call(&app, get(&poll, &token)).await;
        assert_eq!(
            unrefreshed["status"], "pending",
            "a GET reads the store and settles nothing"
        );
        refreshed(&state).await;
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

        let listed = crate::calls::reader::call_progress(
            State(state.clone()),
            caller(&pool, "wren").await,
            Path("caller-run".to_string()),
        )
        .await;
        let body = axum::body::to_bytes(listed.into_body(), usize::MAX)
            .await
            .expect("body");
        let views: Vec<crucible_contract::calls::CallView> =
            serde_json::from_slice(&body).expect("views");
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

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_lock_refuses_what_it_cannot_pin(pool: PgPool) {
        let state = state(pool.clone());
        let backends = Backends::new(&state);
        let wren = caller(&pool, "wren").await;
        let locking = |draft_pack| crate::calls::lock::Locking {
            state: &state,
            caller: &wren,
            draft_pack,
            draft: None,
        };
        let missing = crate::calls::lock::declared(|path| {
            (path == "crucible.toml")
                .then(|| CALLER_MANIFEST.to_string())
                .or_else(|| (path == "workflow.star").then(|| "calls = {\"gone\": {\"kind\": \"playbook\", \"playbook\": \"draft:nowhere\"}}\n".to_string()))
        })
        .expect("declared");
        let refused = crate::calls::lock::write(&backends, &ctx(&state), &missing, &locking(true))
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
        let refused = crate::calls::lock::write(&backends, &ctx(&state), &head, &locking(false))
            .await
            .expect_err("a registered pack may not pin a draft head");
        assert!(refused.contains("only a draft pack"), "{refused}");
        let lock = crate::calls::lock::write(&backends, &ctx(&state), &head, &locking(true))
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
        let refused = crate::calls::lock::write(&backends, &ctx(&state), &github, &locking(true))
            .await
            .expect_err("no token source");
        assert!(
            refused.contains("CONTROLLER_CALLS_GITHUB_TOKEN"),
            "{refused}"
        );
    }

    async fn digest_of(pool: &PgPool, key: &str, alias: &str) -> String {
        let tree = crate::runs::blob_store::get_pack(pool, &crate::model::sanitize_key(key))
            .await
            .expect("pack")
            .expect("stored");
        let (_, lock) = crate::calls::lock::of_tree(&tree).expect("lock");
        lock.expect("locked").calls[&CallAlias::parse(alias).expect("alias")]
            .interface_digest
            .clone()
    }

    fn with_files(digest: &str, task: &str, alias: &str, files: Value) -> DispatchRequest {
        serde_json::from_value(json!({
            "task": task,
            "attempt": 1,
            "alias": alias,
            "interface_digest": digest,
            "files": files,
        }))
        .expect("request")
    }

    async fn read_as(app: &axum::Router, user: &str, path: &str) -> (StatusCode, String) {
        let req = Request::get(path)
            .header("x-auth-request-user", user)
            .body(Body::empty())
            .expect("request");
        let (status, body) = crate::testing::oneshot_bytes(app, req).await;
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn child_of(pool: &PgPool, handle: &Value) -> (String, String) {
        let run = handle["run"].as_str().expect("playbook handle").to_string();
        let key: String =
            sqlx::query_scalar("SELECT key FROM playbook_launches WHERE reserved_run = $1")
                .bind(&run)
                .fetch_one(pool)
                .await
                .expect("child launch");
        (key, run)
    }

    async fn finished_with(pool: &PgPool, key: &str, run: &str, session: &str) {
        running(pool, key, run, "finished").await;
        crate::runs::blob_store::put_run_session(pool, run, session.as_bytes())
            .await
            .expect("session");
    }

    const PLANNED_HELLO: &str = r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{"name":"hello","kind":"command","required":true}]}"#;

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_calls_progress_is_served_only_to_readers_of_the_calling_run(pool: PgPool) {
        let state = state(pool.clone());
        let digest = calling_run(&state, "playbook:caller:1", "caller-run").await;
        let token = mint_token(&pool, "caller-run").await;
        let pod = run_routes(state.clone());
        let (status, body) = crate::testing::call(
            &pod,
            post(
                CALLS,
                &token,
                &request(&digest, 1, json!({"topic": "attention"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let call = body["call"].as_str().expect("call").to_string();
        let (child, target_run) = child_of(&pool, &body["handle"]).await;
        finished_with(
            &pool,
            &child,
            &target_run,
            &format!(
                "{PLANNED_HELLO}\n{}\n",
                r#"{"v":1,"kind":"task_result","task":"hello","status":"pass","output":{"greeting":"hi"}}"#
            ),
        )
        .await;
        refreshed(&state).await;
        let app = crate::api::router(state.clone());
        let progress = "/api/runs/caller-run/call-progress";

        let (status, seen) = read_as(&app, "wren", progress).await;
        assert_eq!(status, StatusCode::OK, "{seen}");
        let views: Vec<crucible_contract::calls::CallView> =
            serde_json::from_str(&seen).expect("views");
        assert_eq!(views.len(), 1);
        assert!(views[0].state.link.is_some());
        assert!(views[0].state.progress.nodes[0].link.is_some());

        let (status, _) = read_as(&app, "mallory", progress).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "not a reader of the calling run"
        );
        let (status, _) = read_as(&app, "mallory", "/api/runs/nowhere/call-progress").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let log = format!("/api/runs/caller-run/calls/{call}/nodes/hello/log");
        let (status, text) = read_as(&app, "wren", &log).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(text.contains(r#""task":"hello""#), "{text}");
        let (status, _) = read_as(&app, "mallory", &log).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = read_as(
            &app,
            "wren",
            &format!("/api/runs/caller-run/calls/{call}/nodes/nope/log"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, launches) = read_as(&app, "wren", "/api/playbook-runs").await;
        assert_eq!(status, StatusCode::OK, "{launches}");
        let launches: Vec<Value> = serde_json::from_str(&launches).expect("launches");
        let called = launches
            .iter()
            .find(|l| l["key"] == json!(child))
            .expect("the child launch is listed");
        assert_eq!(
            called["called_by"],
            json!({"run": "caller-run", "launch": "playbook:caller:1", "task": "ask", "instance": null})
        );
        let caller_row = launches
            .iter()
            .find(|l| l["key"] == json!("playbook:caller:1"))
            .expect("the caller is listed");
        assert_eq!(caller_row["called_by"], Value::Null);

        sqlx::query("UPDATE playbook_launches SET owner = 'user:ash' WHERE key = $1")
            .bind(&child)
            .execute(&pool)
            .await
            .expect("hand the target run to ash");
        let (status, seen) = read_as(&app, "wren", progress).await;
        assert_eq!(status, StatusCode::OK, "{seen}");
        let views: Vec<crucible_contract::calls::CallView> =
            serde_json::from_str(&seen).expect("views");
        let node = &views[0].state.progress.nodes[0];
        assert_eq!(node.label, "hello");
        assert!(node.status.ended());
        assert_eq!(node.link, None, "no link into a run wren may not read");
        assert_eq!(node.detail, None);
        assert_eq!(views[0].state.link, None);
        let (status, _) = read_as(&app, "wren", &log).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "no log of a run wren may not read"
        );
        let (status, _) = read_as(&app, "ash", progress).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "reading the target is not reading the caller"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_dispatch_must_match_the_plan_and_the_locked_types(pool: PgPool) {
        let state = state(pool.clone());
        let digest = calling_run(&state, "playbook:caller:1", "caller-run").await;
        let filer = digest_of(&pool, "playbook:caller:1", "filer").await;
        let token = mint_token(&pool, "caller-run").await;
        let app = run_routes(state.clone());
        let notes = b"# notes\n".to_vec();
        let notes_digest = crucible_contract::artifact::content_digest(&notes);
        let put = Request::put(format!("/api/runs/caller-run/call-files/{notes_digest}"))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(notes.clone()))
            .expect("request");
        let (status, _) = crate::testing::call(&app, put).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let file = |media: &str, size: usize| json!({"notes": {"digest": notes_digest, "size": size, "path": "NOTES.md", "media_type": media}});
        let mut not_a_call = request(&digest, 1, json!({"topic": "a"}));
        not_a_call.task = "hello".into();
        let mut instanced = request(&digest, 1, json!({"topic": "a"}));
        instanced.instance = Some("x".into());
        let mut wrong_alias =
            with_files(&filer, "ask", "filer", file("text/markdown", notes.len()));
        wrong_alias.task = "ask".into();
        for (refused, says) in [
            (not_a_call, "no call task \"hello\""),
            (wrong_alias, "calls target target, not filer"),
            (instanced, "not mapped"),
            (request(&digest, 1, json!({"topic": 3})), "takes string"),
            (
                with_files(
                    &digest,
                    "ask",
                    "target",
                    json!({"topic": {"digest": notes_digest, "size": notes.len()}}),
                ),
                "not a file parameter",
            ),
            (
                serde_json::from_value(json!({
                    "task": "file", "attempt": 1, "alias": "filer",
                    "interface_digest": filer, "values": {"notes": "text"},
                }))
                .expect("request"),
                "takes a file",
            ),
            (
                with_files(&filer, "file", "filer", file("image/png", notes.len())),
                "takes text/markdown",
            ),
            (
                with_files(&filer, "file", "filer", file("text/markdown", 3)),
                "this run uploaded 8",
            ),
            (
                with_files(&filer, "file", "filer", file("text/markdown", 65)),
                "at most 64 bytes",
            ),
            (
                with_files(
                    &filer,
                    "file",
                    "filer",
                    json!({"notes": {"digest": "sha256:00", "size": 1, "media_type": "text/markdown"}}),
                ),
                "did not upload",
            ),
        ] {
            let (status, body) = crate::testing::call(&app, post(CALLS, &token, &refused)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let note = body["note"].as_str().unwrap_or_default();
            assert!(note.contains(says), "{says:?} in {note}");
        }
        assert!(child_launches(&pool).await.is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_bound_file_reaches_the_target_launch_and_only_its_runs(pool: PgPool) {
        let state = state(pool.clone());
        calling_run(&state, "playbook:caller:1", "caller-run").await;
        let filer = digest_of(&pool, "playbook:caller:1", "filer").await;
        let token = mint_token(&pool, "caller-run").await;
        let app = run_routes(state.clone());
        let notes = b"# notes\n".to_vec();
        let digest = crucible_contract::artifact::content_digest(&notes);
        let put = Request::put(format!("/api/runs/caller-run/call-files/{digest}"))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(notes.clone()))
            .expect("request");
        assert_eq!(
            crate::testing::call(&app, put).await.0,
            StatusCode::NO_CONTENT
        );
        let dispatch = with_files(
            &filer,
            "file",
            "filer",
            json!({"notes": {"digest": digest, "size": notes.len(), "path": "NOTES.md", "media_type": "text/markdown"}}),
        );
        let (status, body) = crate::testing::call(&app, post(CALLS, &token, &dispatch)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (child, target_run) = child_of(&pool, &body["handle"]).await;
        let params: Value =
            sqlx::query_scalar("SELECT params FROM playbook_launches WHERE key = $1")
                .bind(&child)
                .fetch_one(&pool)
                .await
                .expect("params");
        let reference: Value =
            serde_json::from_str(params["notes"].as_str().expect("staged")).expect("reference");
        assert_eq!(reference["digest"], json!(digest));
        assert_eq!(reference["media_type"], json!("text/markdown"));
        let staged = reference["path"].as_str().expect("path");
        assert!(staged.ends_with("/NOTES.md"), "{staged}");
        assert_eq!(std::fs::read(staged).expect("staged on disk"), notes);

        running(&pool, &child, &target_run, "running").await;
        let child_token = mint_token(&pool, &target_run).await;
        let served = format!("/api/runs/{target_run}/param-files/{digest}");
        let (status, bytes) = crate::testing::oneshot_bytes(&app, get(&served, &child_token)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes.as_ref(), notes.as_slice());
        let (status, _) = crate::testing::call(&app, get(&served, &token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "another run's token");
        let (status, _) = crate::testing::call(
            &app,
            get(
                &format!("/api/runs/caller-run/param-files/{digest}"),
                &token,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "the caller's own launch holds no file"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_runs_call_inputs_are_capped(pool: PgPool) {
        let state = state(pool.clone());
        calling_run(&state, "playbook:caller:1", "caller-run").await;
        let half = usize::try_from(crate::calls::store::MAX_RUN_INPUT_BYTES / 2 + 1).expect("size");
        let first = vec![1u8; half];
        let second = vec![2u8; half];
        let d = |b: &[u8]| crucible_contract::artifact::content_digest(b);
        let put = |bytes: &[u8]| {
            let pool = pool.clone();
            let bytes = bytes.to_vec();
            async move {
                crate::calls::store::put_input_file(&pool, "caller-run", &d(&bytes), &bytes)
                    .await
                    .expect("put")
            }
        };
        assert!(put(&first).await);
        assert!(!put(&second).await, "past the cap");
        assert!(put(&first).await, "a file the run holds answers again");
        assert_eq!(
            crate::calls::store::input_file_size(&pool, "caller-run", &d(&second))
                .await
                .expect("size"),
            None
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_call_never_targets_a_playbook_already_in_its_chain(pool: PgPool) {
        let state = state(pool.clone());
        let digest = calling_run(&state, "playbook:caller:1", "caller-run").await;
        let token = mint_token(&pool, "caller-run").await;
        let app = run_routes(state.clone());
        let max_time = crate::model::MaxTime::parse("30m").expect("duration");
        let params = json!({});
        let mut tx = pool.begin().await.expect("tx");
        crate::launches::store::insert_playbook_launch_with(
            &mut tx,
            "playbook:target:0",
            &crate::launches::model::NewPlaybookLaunch {
                choice_snapshots: None,
                playbook: "target",
                repo: "(draft)",
                title: "grand",
                params: &params,
                schema_digest: "sha256:schema",
                max_cost: 1.0,
                max_time: &max_time,
                advance_dedupe: false,
                dedupe_schedule: None,
                origin: crate::model::LaunchOrigin::Draft,
                draft_version: Some(1),
                created_by: Some("wren"),
                owner: Some("user:wren"),
                launcher_groups: None,
                rerun: None,
                dispatch: Default::default(),
            },
            &crate::playbooks::exposure::Extraction::Absent,
        )
        .await
        .expect("grand launch");
        tx.commit().await.expect("commit");
        running(&pool, "playbook:target:0", "grand-run", "running").await;
        sqlx::query(
            "UPDATE playbook_launches SET parent_run = 'grand-run', parent_task = 'up', \
             call_depth = 1 WHERE key = 'playbook:caller:1'",
        )
        .execute(&pool)
        .await
        .expect("chain");
        assert_eq!(
            crate::calls::store::chain(&pool, "caller-run")
                .await
                .expect("chain")
                .into_iter()
                .map(|c| (c.playbook, c.draft))
                .collect::<Vec<_>>(),
            vec![("caller".to_string(), true), ("target".to_string(), true)]
        );
        let (status, body) = crate::testing::call(
            &app,
            post(CALLS, &token, &request(&digest, 1, json!({"topic": "a"}))),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let note = body["note"].as_str().unwrap_or_default();
        assert!(note.contains("already in this chain"), "{note}");
        assert!(note.contains("target -> caller -> target"), "{note}");
        assert!(child_launches(&pool).await.is_empty());
    }

    async fn lock_as(
        state: &ApiState,
        backends: &Backends,
        who: &crate::authz::Caller,
        me: Option<&str>,
        calls: String,
    ) -> Result<Option<crucible_contract::calls::CallsLock>, String> {
        let workflow = format!("{calls}workflow(type = \"playbook\", tasks = [])\n");
        let declared = crate::calls::lock::declared(|path| match path {
            "crucible.toml" => Some(CALLER_MANIFEST.to_string()),
            "workflow.star" => Some(workflow.clone()),
            _ => None,
        })
        .expect("declared");
        crate::calls::lock::write(
            backends,
            &ctx(state),
            &declared,
            &crate::calls::lock::Locking {
                state,
                caller: who,
                draft_pack: true,
                draft: me,
            },
        )
        .await
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_lock_names_only_readable_targets_and_no_draft_cycle(pool: PgPool) {
        let state = state(pool.clone());
        let backends = Backends::new(&state);
        draft(&pool, "target", target_files()).await;
        let ash = caller(&pool, "ash").await;
        let wren = caller(&pool, "wren").await;
        let calling = |draft: &str| {
            format!(
                "calls = {{\"t\": {{\"kind\": \"playbook\", \"playbook\": \"draft:{draft}\"}}}}\n"
            )
        };
        let lock = |who, me, body| lock_as(&state, &backends, who, me, body);
        let (ash, wren) = (&ash, &wren);
        let hidden = lock(ash, None, calling("target"))
            .await
            .expect_err("ash may not read it");
        let absent = lock(ash, None, calling("nowhere"))
            .await
            .expect_err("absent");
        assert_eq!(hidden, "call target t: no draft \"target\"");
        assert_eq!(absent, "call target t: no draft \"nowhere\"");
        assert!(lock(wren, None, calling("target")).await.is_ok());

        let plain = |name: &str| {
            files(
                CALLER_MANIFEST,
                &format!(
                    "{name} = command(name = \"{name}\", run = \"true\")\nworkflow(type = \"playbook\", tasks = [{name}])\n"
                ),
            )
        };
        draft(&pool, "loop-a", plain("a")).await;
        let mut loop_b = files(
            CALLER_MANIFEST,
            &format!(
                "{}b = command(name = \"b\", run = \"true\")\nworkflow(type = \"playbook\", tasks = [b])\n",
                calling("loop-a")
            ),
        );
        crate::calls::lock::into_files(
            &backends,
            &ctx(&state),
            &crate::calls::lock::Locking {
                state: &state,
                caller: wren,
                draft_pack: true,
                draft: Some("loop-b"),
            },
            &mut loop_b,
        )
        .await
        .expect("loop-b locks loop-a");
        draft(&pool, "loop-b", loop_b).await;
        let refused = lock(wren, Some("loop-a"), calling("loop-b"))
            .await
            .expect_err("a cycle back to loop-a");
        assert!(refused.contains("loop-a -> loop-b -> loop-a"), "{refused}");
        let refused = lock(wren, Some("loop-a"), calling("loop-a"))
            .await
            .expect_err("a draft calling itself");
        assert!(refused.contains("loop-a -> loop-a"), "{refused}");
        assert!(lock(wren, Some("other"), calling("loop-b")).await.is_ok());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_target_run_settles_on_its_verdict(pool: PgPool) {
        let state = state(pool.clone());
        let digest = calling_run(&state, "playbook:caller:1", "caller-run").await;
        let token = mint_token(&pool, "caller-run").await;
        let app = run_routes(state.clone());
        let mut settled = Vec::new();
        for (attempt, session) in [
            (
                1,
                format!(
                    "{PLANNED_HELLO}\n{}\n{}\n",
                    r#"{"v":1,"kind":"task_result","task":"hello","status":"fail"}"#,
                    r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":""}"#
                ),
            ),
            (
                2,
                format!(
                    "{PLANNED_HELLO}\n{}\n",
                    r#"{"v":1,"kind":"shutdown","outcome":"complete","reason":"nothing to do"}"#
                ),
            ),
        ] {
            let (status, body) = crate::testing::call(
                &app,
                post(
                    CALLS,
                    &token,
                    &request(&digest, attempt, json!({"topic": "a"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let call = body["call"].as_str().expect("call").to_string();
            let (child, target_run) = child_of(&pool, &body["handle"]).await;
            finished_with(&pool, &child, &target_run, &session).await;
            refreshed(&state).await;
            let (_, polled) =
                crate::testing::call(&app, get(&format!("{CALLS}/{call}"), &token)).await;
            settled.push(serde_json::from_value::<CallState>(polled).expect("state"));
        }
        assert_eq!(settled[0].status, CallStatus::Failed, "{}", settled[0].note);
        assert!(
            settled[0].note.contains("invalid verdict"),
            "{}",
            settled[0].note
        );
        assert!(
            settled[0].note.contains("required task hello settled fail"),
            "{}",
            settled[0].note
        );
        assert_eq!(settled[1].status, CallStatus::Passed, "{}", settled[1].note);
        assert_eq!(settled[1].result, json!({}));
        assert!(
            settled[1].note.contains("result task hello")
                && settled[1].note.contains("never settled"),
            "{}",
            settled[1].note
        );
    }

    const GITHUB_CALLER: &str = concat!(
        "params = {}\n",
        "calls = {\"ci\": {\"kind\": \"github_workflow\", \"repo\": \"acme/app\", ",
        "\"workflow\": \".github/workflows/ci.yml\", \"ref\": \"main\", ",
        "\"outputs\": {\"artifact\": \"result\"}}}\n",
        "hello = command(name = \"hello\", run = \"echo hello\")\n",
        "ci = call(name = \"ci\", target = \"ci\", params = {})\n",
        "workflow(type = \"playbook\", tasks = [hello, ci], result = hello)\n",
    );

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_github_call_needs_an_owner_who_may_write_to_the_repository(pool: PgPool) {
        use crucible_contract::calls::{
            CallsLock, GitRefName, GithubRepo, LOCK_VERSION, LockedCall, TargetInterface,
            WorkflowPath,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/installation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 7,
                "permissions": {"actions": "write", "contents": "read"},
            })))
            .mount(&server)
            .await;
        let in_an_hour = jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() + 3600)
            .expect("an hour on")
            .to_string();
        Mock::given(method("POST"))
            .and(path("/app/installations/7/access_tokens"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({"token": "ghs_repo", "expires_at": in_an_hour})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/collaborators/wren-gh/permission"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"permission": "read", "role_name": "read"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let keys = tempfile::tempdir().expect("dir");
        let state = ApiState {
            calls_github: Some(crate::calls::GithubToken::AppInstallation),
            pack_pr_app: Some(crate::secrets::github_app::GithubAppTokenSource::new(
                "1",
                "2",
                crate::secrets::github_app::testkey::write_test_key(keys.path()),
                server.uri(),
            )),
            github_logins: crate::identity::github_logins::GithubLogins::parse(["wren-gh=wren"]),
            ..state(pool.clone())
        };
        let interface = TargetInterface {
            params: json!({"type": "object", "properties": {}, "required": []}),
            result: vec![],
            result_files: vec![],
            graph: Default::default(),
        };
        let locked = LockedCall::new(
            crucible_contract::calls::Pin::GithubRef {
                repo: GithubRepo::parse("acme/app").expect("repo"),
                workflow: WorkflowPath::parse(".github/workflows/ci.yml").expect("workflow"),
                git_ref: GitRefName::parse("main").expect("ref"),
                commit: "a".repeat(40),
            },
            interface,
        )
        .expect("locked");
        let digest = locked.interface_digest.clone();
        let lock = CallsLock {
            version: LOCK_VERSION,
            calls: BTreeMap::from([(CallAlias::parse("ci").expect("alias"), locked)]),
        };
        let mut caller = files(CALLER_MANIFEST, GITHUB_CALLER);
        caller.insert("calls.lock".into(), lock.render().expect("render"));
        draft(&pool, "gh-caller", caller).await;
        launched(&pool, "playbook:gh-caller:1", "caller-run", "gh-caller").await;
        let token = mint_token(&pool, "caller-run").await;
        let app = run_routes(state.clone());
        let dispatch: DispatchRequest = serde_json::from_value(json!({
            "task": "ci",
            "attempt": 1,
            "alias": "ci",
            "interface_digest": digest,
            "values": {},
        }))
        .expect("request");

        let (status, body) = crate::testing::call(&app, post(CALLS, &token, &dispatch)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let note = body["note"].as_str().expect("note");
        assert!(note.contains("wren-gh has read"), "{note}");
        assert!(note.contains("write, maintain or admin"), "{note}");

        sqlx::query("UPDATE playbook_launches SET owner = 'user:mallory' WHERE key = $1")
            .bind("playbook:gh-caller:1")
            .execute(&pool)
            .await
            .expect("hand the launch to mallory");
        let (status, body) = crate::testing::call(&app, post(CALLS, &token, &dispatch)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let note = body["note"].as_str().expect("note");
        assert!(note.contains("no linked GitHub login"), "{note}");

        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM call_target_runs")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(kept, 0, "a refused dispatch keeps no call");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_call_settles_from_the_run_its_handle_names(pool: PgPool) {
        let state = state(pool.clone());
        let digest = calling_run(&state, "playbook:caller:1", "caller-run").await;
        let token = mint_token(&pool, "caller-run").await;
        let app = run_routes(state.clone());
        let (status, body) = crate::testing::call(
            &app,
            post(CALLS, &token, &request(&digest, 1, json!({"topic": "a"}))),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let call = body["call"].as_str().expect("call").to_string();
        let (child, target_run) = child_of(&pool, &body["handle"]).await;
        finished_with(
            &pool,
            &child,
            &target_run,
            &format!(
                "{PLANNED_HELLO}\n{}\n{}\n",
                r#"{"v":1,"kind":"task_result","task":"hello","status":"pass","output":{"greeting":"mine"}}"#,
                r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":""}"#
            ),
        )
        .await;
        running(&pool, &child, "a-later-run", "running").await;
        refreshed(&state).await;
        let (_, polled) = crate::testing::call(&app, get(&format!("{CALLS}/{call}"), &token)).await;
        let polled: CallState = serde_json::from_value(polled).expect("state");
        assert_eq!(polled.status, CallStatus::Passed, "{}", polled.note);
        assert_eq!(polled.result, json!({"greeting": "mine"}));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_dispatch_it_cannot_record_answers_failing_after_retries(pool: PgPool) {
        let started = crate::calls::Dispatched {
            handle: TargetHandle::Playbook { run: "t-1".into() },
            link: None,
            detail: json!({}),
        };
        let began = std::time::Instant::now();
        let error = recorded(
            &pool,
            &crucible_contract::calls::CallId("unclaimed".into()),
            &started,
        )
        .await
        .expect_err("no claim to record on");
        assert!(
            format!("{error:#}").contains("could not record"),
            "{error:#}"
        );
        assert!(
            began.elapsed() >= std::time::Duration::from_millis(250 * 6),
            "every attempt was made"
        );
    }

    #[test]
    fn a_depth_reads_as_its_place_in_the_chain() {
        assert_eq!(ordinal(1), "first");
        assert_eq!(ordinal(4), "fourth");
        assert_eq!(ordinal(7), "7th");
    }
}
