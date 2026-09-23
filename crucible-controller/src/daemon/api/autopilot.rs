use crate::api::dto::*;
use crate::api::state::*;
use axum::extract::State;
use axum::response::{IntoResponse, Response};

const NO_FLAG: &str = "no autopilot flag is loaded on this controller";

// --- autopilot flag -----------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/autopilot",
    responses(
        (status = 200, description = "Autopilot flag state: enabled/disabled plus audit trail", body = AutopilotDto)
    )
)]
pub(crate) async fn get_autopilot(State(state): State<ApiState>) -> Response {
    let Some(flag) = state.autopilot.as_ref() else {
        return unavailable(NO_FLAG);
    };
    match flag.read().await {
        Ok(s) => Json(AutopilotDto::from(s)).into_response(),
        Err(e) => AppError(e).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/api/autopilot",
    request_body = AutopilotSetBody,
    responses(
        (status = 200, description = "Autopilot flag updated", body = AutopilotDto),
        (status = 403, description = "Caller is not an admin", body = ErrorBody)
    )
)]
pub(crate) async fn set_autopilot(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    Json(body): Json<AutopilotSetBody>,
) -> Response {
    let Some(flag) = state.autopilot.as_ref() else {
        return unavailable(NO_FLAG);
    };
    match set(&state, flag, identity.as_deref(), &body).await {
        Ok(dto) => Json(dto).into_response(),
        Err(e) => AppError(e).into_response(),
    }
}

async fn set(
    state: &ApiState,
    flag: &crate::daemon::autopilot_flag::AutopilotFlag,
    actor: Option<&str>,
    body: &AutopilotSetBody,
) -> anyhow::Result<AutopilotDto> {
    let prev = flag.read().await?;
    let next = flag.set(body.enabled, actor, &body.reason).await?;
    let from_str = if prev.enabled { "enabled" } else { "disabled" };
    let to_str = if next.enabled { "enabled" } else { "disabled" };
    state
        .db
        .events()
        .append(
            &crate::event_log::Event::now("autopilot", from_str, to_str, Some(&body.reason), None)
                .by(actor),
        )
        .await?;
    Ok(next.into())
}
