//! Decision requests (RFC-0002 C-HUMAN-DECISION, C-DECISION-EVIDENCE). A run opens and polls its
//! requests on the pod ingest surface; a person allowed `run:approve` lists, reads, and answers
//! them on the human API.

pub(crate) mod store;

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use crucible_contract::decision::{Label, QuestionId};
use crucible_contract::decision_request::{OpenRequest, RequestStatus, SubmitAnswer};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use utoipa::ToSchema;

use crate::api::state::{ApiState, AppError, ErrorBody};
use crate::authz::Caller;
use crate::authz::action::{Action, ResourceType, Verb};
use crate::authz::decision::Resource;
use crate::authz::model::Principal;
use crate::event_log::Event;
use crate::runs::ingest_auth::{LOCAL_POD_PREFIX, PodAuth};
use crate::runs::ingest_drop::IngestState;

/// The longest note an answer may carry, in bytes. Overridden by `CONTROLLER_DECISION_NOTE_MAX`.
const DEFAULT_NOTE_MAX: usize = 4096;

fn note_max() -> usize {
    std::env::var("CONTROLLER_DECISION_NOTE_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_NOTE_MAX)
}

/// The run-facing routes, mounted on the ingest router beside the artifact drop-box.
pub(crate) fn ingest_routes() -> Router<IngestState> {
    Router::new()
        .route("/api/pods/{pod}/decisions", post(open_request))
        .route("/api/pods/{pod}/decisions/{id}", get(poll_request))
}

/// The run a verified pod belongs to, and that run's launch.
async fn pod_run(pool: &PgPool, pod: &str) -> anyhow::Result<Option<(String, Option<String>)>> {
    if pod.starts_with(LOCAL_POD_PREFIX) {
        let row = sqlx::query(
            "SELECT t.run_id, r.issue FROM local_run_tokens t \
             LEFT JOIN runs r ON r.run_id = t.run_id WHERE t.pod = $1",
        )
        .bind(pod)
        .fetch_optional(pool)
        .await?;
        return Ok(row.map(|r| (r.get("run_id"), r.get("issue"))));
    }
    let Some(key): Option<String> =
        sqlx::query_scalar("SELECT issue_key FROM work_pods WHERE pod_name = $1")
            .bind(pod)
            .fetch_optional(pool)
            .await?
            .flatten()
    else {
        return Ok(None);
    };
    let runs: Vec<String> = sqlx::query_scalar("SELECT run_id FROM runs WHERE issue = $1")
        .bind(&key)
        .fetch_all(pool)
        .await?;
    Ok(runs
        .into_iter()
        .find(|run| crate::runs::workpod::run_pod_name(run) == pod)
        .map(|run| (run, Some(key))))
}

fn refuse(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorBody::new(msg))).into_response()
}

async fn open_request(
    auth: PodAuth,
    State(state): State<IngestState>,
    Json(request): Json<OpenRequest>,
) -> Response {
    let pool = state.db.pool();
    match request.evidence.digest() {
        Ok(digest) if digest == request.evidence_digest => {}
        Ok(_) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                "evidence_digest does not match the evidence",
            );
        }
        Err(e) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
    }
    if request.questions.is_empty() {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a decision request asks at least one question",
        );
    }
    let (run_id, launch) = match pod_run(pool, &auth.pod).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return refuse(
                StatusCode::NOT_FOUND,
                format!("no run for pod {}", auth.pod),
            );
        }
        Err(e) => return AppError::from(e).into_response(),
    };
    let (decision, opened) = match store::open(pool, &run_id, launch.as_deref(), &request).await {
        Ok(found) => found,
        Err(e) => return AppError::from(e).into_response(),
    };
    if opened {
        publish(&state.db, &decision, "open").await;
    }
    Json(decision.state()).into_response()
}

async fn poll_request(
    auth: PodAuth,
    State(state): State<IngestState>,
    Path((_pod, id)): Path<(String, String)>,
) -> Response {
    let pool = state.db.pool();
    let run_id = match pod_run(pool, &auth.pod).await {
        Ok(Some((run_id, _))) => run_id,
        Ok(None) => {
            return refuse(
                StatusCode::NOT_FOUND,
                format!("no run for pod {}", auth.pod),
            );
        }
        Err(e) => return AppError::from(e).into_response(),
    };
    match store::get(pool, &id).await {
        Ok(Some(d)) if d.run_id == run_id => Json(d.state()).into_response(),
        Ok(_) => refuse(StatusCode::NOT_FOUND, format!("no decision request {id}")),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// One live event per transition, keyed by the launch so a launch's page refreshes with it.
async fn publish(db: &crate::Db, d: &store::Decision, to: &str) {
    let key = d.launch_key.as_deref().unwrap_or(&d.run_id);
    if let Err(e) = db
        .events()
        .append(&Event::now(key, "decision", to, Some(&d.task), Some(&d.id)))
        .await
    {
        tracing::warn!(id = %d.id, error = format!("{e:#}"), "recording the decision event failed");
    }
}

/// A run reached a terminal state: its open requests are withdrawn and a local run's ingest
/// credential stops working.
pub(crate) async fn run_ended(db: &crate::Db, run_id: &str) {
    match store::withdraw_run(db.pool(), run_id).await {
        Ok(withdrawn) => {
            for d in &withdrawn {
                publish(db, d, "withdrawn").await;
            }
        }
        Err(e) => {
            tracing::warn!(%run_id, error = format!("{e:#}"), "withdrawing the run's decision requests failed")
        }
    }
    if let Err(e) = sqlx::query("DELETE FROM local_run_tokens WHERE run_id = $1")
        .bind(run_id)
        .execute(db.pool())
        .await
    {
        tracing::warn!(%run_id, error = %e, "revoking the local run's ingest credential failed");
    }
}

/// The owner a run's approvals are decided against: the principal its spend is attributed to,
/// which is the user who launched it, else the platform.
async fn run_owner(pool: &PgPool, run_id: &str) -> anyhow::Result<Principal> {
    let attributed: Option<String> =
        sqlx::query_scalar("SELECT attributed_to FROM runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(Principal::stored(attributed.as_deref()))
}

async fn may_answer(state: &ApiState, caller: &Caller, d: &store::Decision) -> bool {
    let Ok(owner) = run_owner(state.db.pool(), &d.run_id).await else {
        return false;
    };
    let resource = Resource::new(ResourceType::Run, &d.run_id, owner);
    crate::authz::owner::decide_on(state, caller, &resource, Verb::Approve)
        .await
        .is_ok()
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DecisionSummaryDto {
    pub id: String,
    pub run_id: String,
    pub task: String,
    pub launch_key: Option<String>,
    pub opened_at: String,
    pub expires_at: String,
    /// The question ids, in order.
    pub questions: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DecisionsDto {
    /// Open requests the caller may answer, newest first.
    pub open: Vec<DecisionSummaryDto>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DecisionQuestionDto {
    pub id: String,
    pub instructions: String,
    /// The labels an answer may give.
    pub labels: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DecisionFileDto {
    pub path: String,
    pub media_type: String,
    pub base64: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DecisionInputDto {
    pub task: String,
    pub status: String,
    #[schema(value_type = Option<Object>)]
    pub output: Option<serde_json::Value>,
    pub files: Vec<DecisionFileDto>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DecisionGatedDto {
    pub name: String,
    pub kind: String,
    pub question: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DecisionAnswerDto {
    pub labels: BTreeMap<String, String>,
    pub decided_by: String,
    pub decided_at: String,
    pub note: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DecisionDto {
    pub id: String,
    pub run_id: String,
    pub task: String,
    pub launch_key: Option<String>,
    /// `open`, `answered`, `expired`, or `withdrawn`.
    pub state: String,
    pub opened_at: String,
    pub expires_at: String,
    pub questions: Vec<DecisionQuestionDto>,
    pub inputs: Vec<DecisionInputDto>,
    pub spent_usd: f64,
    pub elapsed_secs: u64,
    pub max_cost_usd: f64,
    pub max_time_secs: Option<u64>,
    pub gated: Vec<DecisionGatedDto>,
    /// The rendered review, CommonMark.
    pub review: Option<String>,
    pub evidence_digest: String,
    pub answer: Option<DecisionAnswerDto>,
    /// Whether the caller may answer it.
    pub can_answer: bool,
}

fn state_name(status: &RequestStatus) -> &'static str {
    match status {
        RequestStatus::Open => "open",
        RequestStatus::Answered { .. } => "answered",
        RequestStatus::Expired => "expired",
        RequestStatus::Withdrawn => "withdrawn",
    }
}

fn dto(d: store::Decision, can_answer: bool) -> DecisionDto {
    let answer = match &d.status {
        RequestStatus::Answered { answer } => Some(DecisionAnswerDto {
            labels: answer
                .labels
                .iter()
                .map(|(q, l)| (q.to_string(), l.to_string()))
                .collect(),
            decided_by: answer.decided_by.clone(),
            decided_at: answer.decided_at.clone(),
            note: answer.note.clone(),
        }),
        _ => None,
    };
    DecisionDto {
        state: state_name(&d.status).to_owned(),
        questions: d
            .questions
            .iter()
            .map(|(id, q)| DecisionQuestionDto {
                id: id.to_string(),
                instructions: q.instructions.clone(),
                labels: q.labels().iter().map(Label::to_string).collect(),
            })
            .collect(),
        inputs: d
            .evidence
            .inputs
            .into_iter()
            .map(|(task, input)| DecisionInputDto {
                task,
                status: input.status,
                output: input.output,
                files: input
                    .files
                    .into_iter()
                    .map(|f| DecisionFileDto {
                        path: f.path,
                        media_type: f.media_type,
                        base64: f.base64,
                    })
                    .collect(),
            })
            .collect(),
        spent_usd: d.evidence.run.spent_usd,
        elapsed_secs: d.evidence.run.elapsed_secs,
        max_cost_usd: d.evidence.run.max_cost_usd,
        max_time_secs: d.evidence.run.max_time_secs,
        gated: d
            .evidence
            .gated
            .into_iter()
            .map(|g| DecisionGatedDto {
                name: g.name,
                kind: g.kind,
                question: g.question.to_string(),
                labels: g.labels.iter().map(Label::to_string).collect(),
            })
            .collect(),
        review: d.evidence.review,
        evidence_digest: d.evidence_digest,
        answer,
        can_answer: can_answer && matches!(d.status, RequestStatus::Open),
        id: d.id,
        run_id: d.run_id,
        task: d.task,
        launch_key: d.launch_key,
        opened_at: d.opened_at,
        expires_at: d.expires_at,
    }
}

/// `GET /api/decisions` — the open requests the caller may answer.
#[utoipa::path(
    get,
    path = "/api/decisions",
    responses((status = 200, description = "Open requests the caller may answer", body = DecisionsDto))
)]
pub(crate) async fn list_decisions(State(state): State<ApiState>, caller: Caller) -> Response {
    let open = match store::list_open(state.db.pool()).await {
        Ok(open) => open,
        Err(e) => return AppError::from(e).into_response(),
    };
    let mut answerable = Vec::new();
    for d in open {
        if may_answer(&state, &caller, &d).await {
            answerable.push(DecisionSummaryDto {
                questions: d.questions.keys().map(QuestionId::to_string).collect(),
                id: d.id,
                run_id: d.run_id,
                task: d.task,
                launch_key: d.launch_key,
                opened_at: d.opened_at,
                expires_at: d.expires_at,
            });
        }
    }
    Json(DecisionsDto { open: answerable }).into_response()
}

/// `GET /api/decisions/{id}` — one request with its evidence.
#[utoipa::path(
    get,
    path = "/api/decisions/{id}",
    params(("id" = String, Path, description = "Decision request id")),
    responses(
        (status = 200, description = "The request and its evidence", body = DecisionDto),
        (status = 404, description = "No such request", body = ErrorBody)
    )
)]
pub(crate) async fn get_decision(
    State(state): State<ApiState>,
    caller: Caller,
    Path(id): Path<String>,
) -> Response {
    match store::get(state.db.pool(), &id).await {
        Ok(Some(d)) => {
            let can = may_answer(&state, &caller, &d).await;
            Json(dto(d, can)).into_response()
        }
        Ok(None) => refuse(StatusCode::NOT_FOUND, format!("no decision request {id}")),
        Err(e) => AppError::from(e).into_response(),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct DecisionAnswerBody {
    /// Question id to the chosen label.
    pub labels: BTreeMap<String, String>,
    /// The digest of the evidence the answer was made on.
    pub evidence_digest: String,
    pub note: Option<String>,
}

/// Every question gets exactly one of its declared labels, never `uncertain`, and nothing else.
fn checked(
    d: &store::Decision,
    body: &DecisionAnswerBody,
) -> Result<BTreeMap<QuestionId, Label>, String> {
    if let Some(extra) = body
        .labels
        .keys()
        .find(|q| !d.questions.keys().any(|id| id.as_str() == q.as_str()))
    {
        return Err(format!("the request asks no question {extra:?}"));
    }
    let mut labels = BTreeMap::new();
    for (id, question) in &d.questions {
        let Some(raw) = body.labels.get(id.as_str()) else {
            return Err(format!("no label for {id}"));
        };
        let label = Label::new(raw.clone()).map_err(|e| e.to_string())?;
        if label.is_uncertain() || !question.resolves_to(&label) {
            return Err(format!("{id} has no label {raw:?}"));
        }
        labels.insert(id.clone(), label);
    }
    Ok(labels)
}

/// `POST /api/decisions/{id}/answer` — answer an open request.
#[utoipa::path(
    post,
    path = "/api/decisions/{id}/answer",
    params(("id" = String, Path, description = "Decision request id")),
    request_body = DecisionAnswerBody,
    responses(
        (status = 200, description = "The request, answered", body = DecisionDto),
        (status = 403, description = "The caller may not approve this run", body = ErrorBody),
        (status = 404, description = "No such request", body = ErrorBody),
        (status = 409, description = "The request is closed, or the evidence changed", body = ErrorBody),
        (status = 422, description = "The labels or the note were refused", body = ErrorBody)
    )
)]
pub(crate) async fn answer_decision(
    State(state): State<ApiState>,
    caller: Caller,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<DecisionAnswerBody>,
) -> Response {
    let pool = state.db.pool();
    let d = match store::get(pool, &id).await {
        Ok(Some(d)) => d,
        Ok(None) => return refuse(StatusCode::NOT_FOUND, format!("no decision request {id}")),
        Err(e) => return AppError::from(e).into_response(),
    };
    let owner = match run_owner(pool, &d.run_id).await {
        Ok(owner) => owner,
        Err(e) => return AppError::from(e).into_response(),
    };
    let resource = Resource::new(ResourceType::Run, &d.run_id, owner);
    let decision =
        match crate::authz::owner::decide_on(&state, &caller, &resource, Verb::Approve).await {
            Ok(decision) => decision,
            Err(refused) => return refused.into_response(),
        };
    let labels = match checked(&d, &body) {
        Ok(labels) => labels,
        Err(msg) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, msg),
    };
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    if note.is_some_and(|n| n.len() > note_max()) {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the note is longer than {} bytes", note_max()),
        );
    }
    let decided_by = caller.actor_label();
    let submitted = SubmitAnswer {
        labels: labels.clone(),
        evidence_digest: body.evidence_digest.clone(),
        note: note.map(str::to_owned),
    };
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return AppError::from(anyhow::Error::from(e)).into_response(),
    };
    let recorded = match store::answer(
        &mut tx,
        &id,
        &submitted.labels,
        &submitted.evidence_digest,
        &decided_by,
        submitted.note.as_deref(),
    )
    .await
    {
        Ok(recorded) => recorded,
        Err(e) => return AppError::from(e).into_response(),
    };
    match recorded {
        Ok(true) => {
            let action = Action {
                resource: ResourceType::Run,
                verb: Verb::Approve,
            };
            let mut event = caller.audit_event(action, d.run_id.clone(), true, decision.reason());
            event.result = serde_json::to_value(&submitted).ok();
            let now = crate::clock::now_rfc3339();
            if let Err(e) = crate::authz::store::audit(&mut tx, &event, &now).await {
                return AppError::from(e).into_response();
            }
            if let Err(e) = tx.commit().await {
                return AppError::from(anyhow::Error::from(e)).into_response();
            }
        }
        Ok(false) => {}
        Err(store::Refused::StaleEvidence) => {
            return refuse(
                StatusCode::CONFLICT,
                "the evidence changed since it was shown; reload it",
            );
        }
        Err(store::Refused::Closed(state_now)) => {
            return refuse(StatusCode::CONFLICT, format!("the request is {state_now}"));
        }
    }
    match store::get(pool, &id).await {
        Ok(Some(answered)) => {
            if recorded == Ok(true) {
                publish(&state.db, &answered, "answered").await;
            }
            Json(dto(answered, false)).into_response()
        }
        Ok(None) => refuse(StatusCode::NOT_FOUND, format!("no decision request {id}")),
        Err(e) => AppError::from(e).into_response(),
    }
}

#[cfg(test)]
mod tests;
