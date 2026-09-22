use crate::api::state::*;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

// --- manual reconcile -----------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ReconcileAck {
    actor: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/reconcile",
    responses(
        (status = 202, description = "Reconcile pass triggered: the daemon runs a discovery sweep plus a full re-enqueue of non-terminal issues asynchronously (this request does not wait for it). Repeat triggers while a pass runs coalesce into one.", body = ReconcileAck),
        (status = 403, description = "Caller is not an admin", body = ErrorBody)
    )
)]
pub(crate) async fn trigger_reconcile(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
) -> Result<Response, AppError> {
    state.reconcile_now.notify_one();
    // One line on the synthetic `reconcile` key (the `rerank`/`autopilot` keys' precedent).
    state
        .db
        .events()
        .append(
            &crate::event_log::Event::now(
                "reconcile",
                "requested",
                "triggered",
                Some("manual reconcile: discovery sweep + full re-enqueue of non-terminal issues"),
                None,
            )
            .by(identity.as_deref()),
        )
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ReconcileAck { actor: identity.0 }),
    )
        .into_response())
}
