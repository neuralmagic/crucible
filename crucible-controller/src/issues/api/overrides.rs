use crate::api::state::*;
use crate::daemon::queue::{IssueKey, Override, OverrideKind};
use axum::Form;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// --- POST handlers (overrides; never write the DB) ---------------------------

/// Accepts either a JSON or a form-urlencoded body for the same route, so the zero-JS debug UI's
/// plain `<form>` posts and JSON API/CLI clients share one handler. `origin` records which
/// encoding arrived, so the handler can answer in kind (JSON ack vs. a PRG redirect).
pub(crate) struct AnyBody<T> {
    value: T,
    origin: BodyOrigin,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyOrigin {
    Json,
    Form,
}

impl<S, T> FromRequest<S> for AnyBody<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_json = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/json"));
        if is_json {
            let Json(value) = Json::<T>::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            Ok(AnyBody {
                value,
                origin: BodyOrigin::Json,
            })
        } else {
            let Form(value) = Form::<T>::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            Ok(AnyBody {
                value,
                origin: BodyOrigin::Form,
            })
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct OverrideAck {
    key: String,
    action: &'static str,
    reason: Option<String>,
    priority: Option<i64>,
    actor: Option<String>,
}

/// A form-origin override answers with a redirect (PRG) back to where a human would look next;
/// a JSON-origin override answers with a 202 + ack, per the frozen "JSON in/out" contract.
fn override_response(origin: BodyOrigin, ack: OverrideAck, redirect_to: &str) -> Response {
    match origin {
        BodyOrigin::Form => Redirect::to(redirect_to).into_response(),
        BodyOrigin::Json => (StatusCode::ACCEPTED, Json(ack)).into_response(),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ParkBody {
    reason: String,
}

fn submit_override(
    state: &ApiState,
    ov: Override,
    action: &'static str,
    redirect_to: &str,
    origin: BodyOrigin,
) -> Response {
    let ack = OverrideAck {
        key: ov.key.0.clone(),
        action,
        reason: ov.reason.clone(),
        priority: ov.priority,
        actor: ov.actor.clone(),
    };
    state.sink.submit(ov);
    override_response(origin, ack, redirect_to)
}

#[utoipa::path(
    post,
    path = "/api/issues/{key}/park",
    params(
        ("key" = String, Path, description = "Issue key (percent-encoded)")
    ),
    request_body = ParkBody,
    responses(
        (status = 202, description = "Park override enqueued", body = OverrideAck),
        (status = 303, description = "Park override enqueued (form POST, redirects to /inbox)"),
        (status = 403, description = "Caller is not an operator", body = ErrorBody)
    )
)]
pub(crate) async fn park_issue(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    identity: crate::identity::session::Identity,
    _operator: crate::identity::auth::OperatorGuard,
    body: AnyBody<ParkBody>,
) -> Response {
    submit_override(
        &state,
        Override {
            key: IssueKey(key),
            kind: OverrideKind::Park,
            reason: Some(body.value.reason),
            priority: None,
            actor: identity.0,
        },
        "park",
        "/inbox",
        body.origin,
    )
}

#[derive(Debug, Deserialize, Default, ToSchema)]
pub(crate) struct UnparkBody {
    #[serde(default)]
    reason: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/issues/{key}/unpark",
    params(
        ("key" = String, Path, description = "Issue key (percent-encoded)")
    ),
    request_body = UnparkBody,
    responses(
        (status = 202, description = "Unpark override enqueued", body = OverrideAck),
        (status = 303, description = "Unpark override enqueued (form POST, redirects to /inbox)"),
        (status = 403, description = "Caller is not an operator", body = ErrorBody)
    )
)]
pub(crate) async fn unpark_issue(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    identity: crate::identity::session::Identity,
    _operator: crate::identity::auth::OperatorGuard,
    body: AnyBody<UnparkBody>,
) -> Response {
    submit_override(
        &state,
        Override {
            key: IssueKey(key),
            kind: OverrideKind::Unpark,
            reason: body.value.reason,
            priority: None,
            actor: identity.0,
        },
        "unpark",
        "/inbox",
        body.origin,
    )
}

#[cfg(feature = "autoresearch")]
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ScopeNowBody {
    justification: String,
    #[serde(default)]
    max_cost: Option<f64>,
}

#[utoipa::path(
    post,
    path = "/api/issues/{key}/scope",
    params(
        ("key" = String, Path, description = "Issue key (percent-encoded)")
    ),
    request_body = ScopeNowBody,
    responses(
        (status = 202, description = "ScopeNow override enqueued", body = OverrideAck),
        (status = 303, description = "ScopeNow override enqueued (form POST, redirects to /)"),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody)
    )
)]
#[cfg(feature = "autoresearch")]
pub(crate) async fn scope_now(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    body: AnyBody<ScopeNowBody>,
) -> Response {
    let justification = body.value.justification;
    submit_override(
        &state,
        Override {
            key: IssueKey(key),
            kind: OverrideKind::ScopeNow {
                justification: justification.clone(),
                max_cost: body.value.max_cost,
            },
            reason: Some(justification),
            priority: None,
            actor: identity.0,
        },
        "scope_now",
        "/",
        body.origin,
    )
}

#[cfg(feature = "autoresearch")]
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct RedispatchBody {
    justification: String,
}

#[utoipa::path(
    post,
    path = "/api/issues/{key}/redispatch",
    params(
        ("key" = String, Path, description = "Issue key (percent-encoded)")
    ),
    request_body = RedispatchBody,
    responses(
        (status = 202, description = "Redispatch override enqueued", body = OverrideAck),
        (status = 303, description = "Redispatch override enqueued (form POST, redirects to /)"),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody)
    )
)]
#[cfg(feature = "autoresearch")]
pub(crate) async fn redispatch_issue(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    body: AnyBody<RedispatchBody>,
) -> Response {
    let justification = body.value.justification;
    submit_override(
        &state,
        Override {
            key: IssueKey(key),
            kind: OverrideKind::Redispatch {
                justification: justification.clone(),
            },
            reason: Some(justification),
            priority: None,
            actor: identity.0,
        },
        "redispatch",
        "/",
        body.origin,
    )
}

#[cfg(feature = "autoresearch")]
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct BumpBody {
    priority: i64,
    #[serde(default)]
    reason: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/issues/{key}/bump",
    params(
        ("key" = String, Path, description = "Issue key (percent-encoded)")
    ),
    request_body = BumpBody,
    responses(
        (status = 202, description = "Priority bump override enqueued", body = OverrideAck),
        (status = 303, description = "Priority bump override enqueued (form POST, redirects to /)"),
        (status = 403, description = "Caller is not an operator", body = ErrorBody)
    )
)]
#[cfg(feature = "autoresearch")]
pub(crate) async fn bump_issue(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    identity: crate::identity::session::Identity,
    _operator: crate::identity::auth::OperatorGuard,
    body: AnyBody<BumpBody>,
) -> Response {
    submit_override(
        &state,
        Override {
            key: IssueKey(key),
            kind: OverrideKind::Bump,
            reason: body.value.reason,
            priority: Some(body.value.priority),
            actor: identity.0,
        },
        "bump",
        "/",
        body.origin,
    )
}
