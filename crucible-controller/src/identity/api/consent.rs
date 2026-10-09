//! The authorization request parked on the caller's session: what the consent page shows (which
//! client asks for which resource and scopes, and who the caller would act as on its cluster), and
//! the decision that answers it with the client's loopback redirect.

use crate::agent_auth::people::{
    self, Approved, CLIENT_ID, CODE_TTL_SECS, ClusterUser, CodeGrant, PENDING_KEY, Pending,
};
use crate::api::dto::{bad_request, conflict, forbidden, not_found, unavailable};
use crate::api::state::{ApiState, AppError, ErrorBody, Json};
use crate::identity::api::cluster_identities::{
    ClusterIdentityDto, identity_of, person_row, rules,
};
use crate::identity::api::kube_user::no_subject;
use crate::identity::api_key::constant_time_eq;
use crate::identity::auth::{AuthPath, Subject};
use crate::identity::cluster_identity::Person;
use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use forge::fleet::ClusterIdentity;
use serde::{Deserialize, Serialize};
use tower_sessions::Session;
use utoipa::{IntoParams, ToSchema};

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConsentQuery {
    /// The parked request's id, from `/consent?id=`.
    pub id: String,
}

/// What the person is asked to approve.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConsentDto {
    pub id: String,
    pub client_id: String,
    pub resource: String,
    pub scopes: Vec<String>,
    /// The login the token's `sub` will name.
    pub login: String,
    /// The caller's username on the resource's cluster, and how to link it when there is none.
    pub identity: ClusterIdentityDto,
}

#[utoipa::path(
    get,
    path = "/api/oauth/consent",
    params(ConsentQuery),
    responses(
        (status = 200, description = "The request waiting on the caller's decision", body = ConsentDto),
        (status = 403, description = "The caller has no issuer subject", body = ErrorBody),
        (status = 404, description = "Nothing is pending under this id for the caller", body = ErrorBody)
    )
)]
pub(crate) async fn get_consent(
    State(state): State<ApiState>,
    subject: Subject,
    session: Session,
    Query(query): Query<ConsentQuery>,
) -> Result<Response, AppError> {
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let pending: Option<Pending> = session
        .get(PENDING_KEY)
        .await
        .context("reading the parked authorization request")?;
    let Some(pending) =
        pending.filter(|p| p.sub == sub && constant_time_eq(p.id.as_bytes(), query.id.as_bytes()))
    else {
        return Ok(not_found(
            "no authorization request is waiting under this id; start again from your client",
        ));
    };
    let Some((login, email)) = person_row(state.db.pool(), sub).await? else {
        return Ok(no_subject());
    };
    let r = &pending.request;
    let rule = rules(&state)
        .await?
        .into_iter()
        .find(|(c, _)| c == &r.cluster)
        .map(|(_, rule)| rule)
        .unwrap_or(ClusterIdentity::Link);
    let person = Person {
        sub,
        login: &login,
        email: email.as_deref(),
    };
    let identity = identity_of(&state, &r.cluster, &rule, person).await?;
    Ok(Json(ConsentDto {
        id: pending.id.clone(),
        client_id: CLIENT_ID.to_string(),
        resource: r.resource.clone(),
        scopes: r.scopes.clone(),
        login,
        identity,
    })
    .into_response())
}

/// The person's answer to a parked authorization request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    Deny,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DecisionBody {
    /// The parked request's id, from `/consent?id=`.
    pub id: String,
    pub decision: Verdict,
}

/// Where to send the browser: the client's loopback callback, with a code or an error.
#[derive(Debug, Serialize, ToSchema)]
pub struct DecisionDto {
    pub redirect_url: String,
}

fn send_back(redirect_url: String) -> Response {
    let mut response = Json(DecisionDto { redirect_url }).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[utoipa::path(
    post,
    path = "/api/oauth/consent",
    request_body = DecisionBody,
    responses(
        (status = 200, description = "Navigate here: the client's callback with a code, or with the error the decision sends back", body = DecisionDto),
        (status = 400, description = "The cluster's rule gives no username for this login", body = ErrorBody),
        (status = 403, description = "Not a browser session, or no issuer subject", body = ErrorBody),
        (status = 404, description = "This controller issues no tokens to people", body = ErrorBody),
        (status = 409, description = "Nothing is pending under this id, or the cluster needs a link first", body = ErrorBody)
    )
)]
pub(crate) async fn decide_consent(
    State(state): State<ApiState>,
    subject: Subject,
    path: AuthPath,
    session: Session,
    Json(body): Json<DecisionBody>,
) -> Result<Response, AppError> {
    let Some(auth) = state.people_issuer.as_ref() else {
        return Ok(not_found("this controller issues no tokens to people"));
    };
    let Some(issued) = auth.people.as_ref() else {
        return Ok(not_found("this controller issues no tokens to people"));
    };
    if path != AuthPath::Session {
        return Ok(forbidden(
            "approving a client needs a signed-in browser session",
        ));
    }
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let pending: Option<Pending> = session
        .remove(PENDING_KEY)
        .await
        .context("taking the parked authorization request")?;
    let Some(pending) =
        pending.filter(|p| p.sub == sub && constant_time_eq(p.id.as_bytes(), body.id.as_bytes()))
    else {
        return Ok(conflict(
            "this approval is no longer pending; start again from your client",
        ));
    };
    let Some((login, email)) = person_row(state.db.pool(), sub).await? else {
        return Ok(no_subject());
    };
    let issuer = auth.issuer.issuer().as_str();
    let r = pending.request.clone();
    let back = |error: &str, description: &str| {
        send_back(r.redirect.with(&[
            ("error", error),
            ("error_description", description),
            ("state", r.state.as_str()),
            ("iss", issuer),
        ]))
    };
    if body.decision == Verdict::Deny {
        return Ok(back("access_denied", "the person declined"));
    }
    let approved = Approved {
        sub: sub.to_string(),
        login: login.clone(),
        email,
        resource: r.resource.clone(),
        cluster: r.cluster.clone(),
        scopes: r.scopes.clone(),
    };
    match people::kube_user(auth, &approved).await? {
        ClusterUser::Known(_) => {}
        ClusterUser::Unlinked => {
            session
                .insert(PENDING_KEY, &pending)
                .await
                .context("parking the authorization request again")?;
            return Ok(conflict(format!(
                "link your username on {} first",
                r.cluster
            )));
        }
        ClusterUser::Unavailable(why) => return Ok(bad_request(why)),
    }
    let now = crate::clock::unix_secs();
    let grant = CodeGrant {
        approved,
        redirect: r.redirect.clone(),
        challenge: r.challenge.clone(),
        expires_at: now.saturating_add(CODE_TTL_SECS),
    };
    match issued.grants.issue_code(&grant).await {
        Ok(code) => {
            tracing::info!(login, resource = r.resource, "authorization code issued");
            Ok(send_back(r.redirect.with(&[
                ("code", code.as_str()),
                ("state", r.state.as_str()),
                ("iss", issuer),
            ])))
        }
        Err(e) => {
            tracing::error!(error = %e, "storing an authorization code");
            Ok(unavailable("the approval could not be stored; retry"))
        }
    }
}
