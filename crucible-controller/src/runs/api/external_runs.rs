//! `PUT /api/runs/{run_id}/session` — ingest (or re-ingest) the session log of an external run:
//! one the controller did NOT dispatch (a harness-launched loop pod). The pod-bound drop-box
//! can't serve these — the pod and its projected token are gone by the time a human decides the
//! run is worth keeping — so this is the operator approval: admin-gated, ledgered, and a re-upload
//! REPLACES the run in place (same patch-not-skip contract as `POST /api/emissions/run`).
//!
//! The body is the raw session NDJSON (the wrapper's SESSION dump / `state/session.jsonl`).
//! It lands in the artifact store as the run's session ([`crate::runs::blob_store::put_run_session`])
//! and folds through the same [`crate::runs::ingest::ingest_session`] the completion edge uses, so the
//! run appears in the SPA (`/runs/{run_id}`) exactly like a dispatched one — minus a scope, which
//! it never had.

use crate::api::dto::{bad_gateway, conflict, require_non_empty, unprocessable};
use crate::api::state::ErrorBody;
use crate::api::state::{ApiState, Json};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ExternalRunQuery {
    /// Human-authorized write: why this run deserves a ledger row.
    justification: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ExternalRunAck {
    /// True when an existing row was dropped and re-ingested (a re-upload).
    replaced: bool,
    /// Folded outcome facts, straight from the parsed session.
    status: String,
    best_score: Option<f64>,
    pr_links: usize,
}

/// Body cap: matches the wrapper's own session-dump ceiling; a bigger body is a 413 upstream
/// (axum's body limit is configured at router build).
#[utoipa::path(
    put,
    path = "/api/runs/{run_id}/session",
    request_body(content = String, content_type = "application/x-ndjson"),
    params(
        ("run_id", description = "The run id the session belongs to"),
        ("justification" = String, Query, description = "Why this external run is being ingested")
    ),
    responses(
        (status = 201, description = "Run ingested (or replaced) and visible in the SPA", body = ExternalRunAck),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody),
        (status = 409, description = "The run belongs to a scope (controller-dispatched); external replace is refused", body = ErrorBody),
        (status = 422, description = "run_id/justification/body must be non-empty", body = ErrorBody)
    )
)]
pub(crate) async fn put_external_run_session(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    AxPath(run_id): AxPath<String>,
    Query(q): Query<ExternalRunQuery>,
    body: String,
) -> Response {
    let run_id = run_id.trim().to_string();
    if let Some(msg) = require_non_empty(&[
        ("run_id", &run_id),
        ("justification", &q.justification),
        ("body", &body),
    ]) {
        return unprocessable(msg);
    }

    // A dispatched run is the pod-watch's to own; replacing it from the outside would fork
    // authority over the same row. Only scope-less (external) rows are replaceable.
    let replaced = match crate::runs::store::get_run(state.db.pool(), &run_id).await {
        Ok(Some(run)) => match run.scope {
            Some(scope) => {
                return conflict(format!(
                    "run {run_id} is controller-dispatched (scope {scope}); external replace refused"
                ));
            }
            None => {
                if let Err(e) =
                    crate::runs::store::delete_run_cascade(state.db.pool(), &run_id).await
                {
                    return bad_gateway(format!("dropping the prior row for replace: {e:#}"));
                }
                true
            }
        },
        Ok(None) => false,
        Err(e) => return bad_gateway(format!("looking up run {run_id}: {e:#}")),
    };

    // Evidence first, fold second: the session lands in the store before any row points at it.
    if let Err(e) =
        crate::runs::blob_store::put_run_session(state.db.pool(), &run_id, body.as_bytes()).await
    {
        return bad_gateway(format!("storing the session: {e:#}"));
    }
    let session_uri = crate::runs::blob_store::run_session_uri(&run_id);

    let parsed = match crate::runs::ingest::ingest_session(
        &state.db,
        &crate::runs::ingest::IngestTarget {
            run_id: &run_id,
            scope_id: None,
            issue: None,
            pod: None,
            session_uri: &session_uri,
        },
        &body,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return bad_gateway(format!("ingesting the session: {e:#}")),
    };

    let actor = identity.as_deref().unwrap_or("unknown");
    state
        .audit(
            crate::event_log::Event::now(
                &format!("run:{run_id}"),
                "ingest",
                "ingest",
                Some(&format!(
                    "external run session {}: {}",
                    if replaced { "replaced" } else { "ingested" },
                    q.justification.trim()
                )),
                None,
            )
            .by(Some(actor)),
            "external run ingest",
        )
        .await;

    (
        StatusCode::CREATED,
        Json(ExternalRunAck {
            replaced,
            status: parsed.outcome.unwrap_or_else(|| "incomplete".to_string()),
            best_score: parsed.best_score,
            pr_links: parsed.pr_links.len(),
        }),
    )
        .into_response()
}
