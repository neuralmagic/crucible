//! View-as: a platform administrator's browser session resolving as another user, read-only. The
//! binding decides `platform:impersonate` before `start` runs; the auth middleware does the rest,
//! answering every request on the session as the snapshot and refusing every write but a stop.

use crate::api::dto::{not_found, unprocessable};
use crate::api::state::{ApiState, AppError, ErrorBody};
use crate::authz::Caller;
use crate::identity::auth::AuthPath;
use crate::identity::session::Impersonation;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use tower_sessions::Session;
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema)]
pub struct StartImpersonationBody {
    /// The login to view as: someone who has signed in to this controller.
    pub login: String,
}

#[utoipa::path(
    post,
    path = "/api/impersonation",
    request_body = StartImpersonationBody,
    responses(
        (status = 200, description = "The session now views as the user, read-only", body = Impersonation),
        (status = 403, description = "The caller may not impersonate", body = ErrorBody),
        (status = 404, description = "No one with that login has signed in", body = ErrorBody),
        (status = 422, description = "Not a browser session, or the caller's own login", body = ErrorBody)
    )
)]
pub(crate) async fn start(
    State(state): State<ApiState>,
    caller: Caller,
    session: Option<Extension<Session>>,
    Json(body): Json<StartImpersonationBody>,
) -> Response {
    let Some(Extension(session)) = session.filter(|_| caller.path == AuthPath::Session) else {
        return unprocessable("viewing as another user needs a signed-in browser session");
    };
    let Some(by) = caller.actor_principals().login().map(str::to_string) else {
        return unprocessable("viewing as another user needs a signed-in browser session");
    };
    let view = match snapshot(state.db.pool(), &by, &body.login).await {
        Ok(view) => view,
        Err(refused) => return refused,
    };
    if let Err(e) = crate::identity::session::start_impersonation(&session, &view).await {
        return AppError::from(anyhow::Error::from(e)).into_response();
    }
    state
        .audit(
            crate::event_log::Event::now(
                &format!("user:{}", view.login),
                "self",
                "viewed-as",
                Some(&format!("{} started viewing as {}", view.by, view.login)),
                None,
            )
            .by(Some(&view.by)),
            "start_impersonation",
        )
        .await;
    Json(view).into_response()
}

/// The view-as snapshot `by` would run under for the login `asked`: that user's subject and the
/// groups their last sign-in stamped.
#[allow(clippy::result_large_err)]
pub(crate) async fn snapshot(
    pool: &sqlx::PgPool,
    by: &str,
    asked: &str,
) -> Result<Impersonation, Response> {
    let login = asked.trim().to_lowercase();
    if login == by {
        return Err(unprocessable("that is your own login"));
    }
    let (sub, groups, groups_at) = match crate::identity::oidc::users::stamped(pool, &login).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return Err(not_found(format!(
                "no one with login {login} has signed in here"
            )));
        }
        Err(e) => return Err(AppError::from(e).into_response()),
    };
    Ok(Impersonation {
        login,
        sub,
        groups,
        groups_at,
        by: by.to_string(),
        started_at: jiff::Timestamp::now(),
    })
}

#[utoipa::path(
    delete,
    path = "/api/impersonation",
    responses((status = 204, description = "The session answers as its own identity again"))
)]
pub(crate) async fn stop(
    State(state): State<ApiState>,
    session: Option<Extension<Session>>,
) -> Response {
    let Some(Extension(session)) = session else {
        return StatusCode::NO_CONTENT.into_response();
    };
    match crate::identity::session::stop_impersonation(&session).await {
        Ok(Some(view)) => {
            state
                .audit(
                    crate::event_log::Event::now(
                        &format!("user:{}", view.login),
                        "viewed-as",
                        "self",
                        Some(&format!("{} stopped viewing as {}", view.by, view.login)),
                        None,
                    )
                    .by(Some(&view.by)),
                    "stop_impersonation",
                )
                .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => AppError::from(anyhow::Error::from(e)).into_response(),
    }
}
