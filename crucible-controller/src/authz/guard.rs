//! Coarse role gates for the routes the policy does not decide yet. The role is read from the
//! caller's platform team memberships, so a member added through the teams API holds it exactly
//! like one named in `CONTROLLER_ADMINS` or `CONTROLLER_OPERATORS`.

use crate::api::state::ApiState;
use crate::authz::Caller;
use crate::identity::auth::Role;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

fn forbidden(caller: &Caller, needs: &str) -> Response {
    let who = caller.principals.login().unwrap_or("anonymous");
    let body = serde_json::json!({ "error": format!("{who} is not {needs}") });
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_default(),
    )
        .into_response()
}

/// Admits an owner of the platform administrators team; 403 with a JSON body otherwise.
pub struct AdminGuard;

impl FromRequestParts<ApiState> for AdminGuard {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let caller = Caller::from_request_parts(parts, state).await?;
        match caller.role() {
            Role::Admin => Ok(AdminGuard),
            Role::Operator | Role::Viewer => Err(forbidden(&caller, "a platform administrator")),
        }
    }
}

/// Admits a platform operator or administrator; 403 with a JSON body otherwise.
pub struct OperatorGuard;

impl FromRequestParts<ApiState> for OperatorGuard {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let caller = Caller::from_request_parts(parts, state).await?;
        match caller.role() {
            Role::Admin | Role::Operator => Ok(OperatorGuard),
            Role::Viewer => Err(forbidden(&caller, "a platform operator")),
        }
    }
}
