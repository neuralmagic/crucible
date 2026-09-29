//! Explain: how the active policy set decides one action for one user who has signed in, with the
//! rules that decided it. The user is resolved from the groups their last sign-in stamped, the
//! same snapshot viewing as them runs under, so the answer is what they would get now.

use crate::api::dto::{not_found, unprocessable};
use crate::api::state::{ApiState, AppError, ErrorBody};
use crate::authz::action::{Action, ResourceType};
use crate::authz::api::TeamMembershipDto;
use crate::authz::decision::Resource;
use crate::authz::transfer::{MODEL_PROVIDERS, Owned, PACK_IMPORTS, PLAYBOOK_DRAFTS, PLAYBOOKS};
use crate::identity::auth::AuthPath;
use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ExplainQuery {
    /// The login to decide for: someone who has signed in to this controller.
    pub login: String,
    /// The action, `<resource>:<verb>`.
    pub action: String,
    /// The resource's id. A playbook, draft, import, or provider is decided against its owner;
    /// anything else, or no id, against the platform.
    pub resource: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExplanationDto {
    pub login: String,
    pub action: String,
    pub resource: String,
    pub owner: String,
    pub allowed: bool,
    /// The `@id`s of the policies that decided, or why nothing did.
    pub rules: Vec<String>,
    /// The digest of the active policy set that decided.
    pub policy: String,
    pub groups: Vec<String>,
    /// When `groups` was last the issuer's word.
    pub groups_at: Option<String>,
    pub teams: Vec<TeamMembershipDto>,
}

fn owned(rtype: ResourceType) -> Option<Owned> {
    [PLAYBOOKS, PLAYBOOK_DRAFTS, PACK_IMPORTS, MODEL_PROVIDERS]
        .into_iter()
        .find(|o| o.rtype == rtype)
}

#[utoipa::path(
    get,
    path = "/api/authz/explain",
    params(ExplainQuery),
    responses(
        (status = 200, description = "The decision and the rules that made it", body = ExplanationDto),
        (status = 403, description = "The caller may not decide for other users", body = ErrorBody),
        (status = 404, description = "No such signed-in user, or no such resource", body = ErrorBody),
        (status = 422, description = "The action is not in the vocabulary", body = ErrorBody)
    )
)]
pub(crate) async fn explain(
    State(state): State<ApiState>,
    Query(q): Query<ExplainQuery>,
) -> Response {
    let action = match Action::parse(q.action.trim()) {
        Ok(action) => action,
        Err(e) => return unprocessable(e.to_string()),
    };
    let login = q.login.trim().to_lowercase();
    let pool = state.db.pool();
    let (_, groups, groups_at) = match crate::identity::oidc::users::stamped(pool, &login).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(format!("no one with login {login} has signed in here")),
        Err(e) => return AppError::from(e).into_response(),
    };
    let caller = match crate::authz::resolve_caller(
        pool,
        &state.roles,
        Some(&login),
        &groups,
        AuthPath::Impersonated,
    )
    .await
    {
        Ok(caller) => caller,
        Err(e) => return AppError::from(e).into_response(),
    };
    let id = q
        .resource
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let resource = match (owned(action.resource), id) {
        (Some(table), Some(id)) => match crate::authz::transfer::owner_of(pool, table, id).await {
            Ok(Some(owner)) => Resource::new(action.resource, id, owner),
            Ok(None) => return not_found(format!("no {} {id:?}", action.resource.as_str())),
            Err(e) => return AppError::from(e).into_response(),
        },
        (_, id) => Resource::platform(action.resource, id.unwrap_or(action.resource.as_str())),
    };
    let resource = match crate::authz::granted::attach(pool, &caller.principals, resource).await {
        Ok(resource) => resource,
        Err(e) => return AppError::from(e).into_response(),
    };
    let decision = crate::authz::owner::decision(&state, &caller, action.verb, &resource);
    Json(ExplanationDto {
        login,
        action: action.to_string(),
        resource: resource.id.clone(),
        owner: resource.owner.to_string(),
        allowed: decision.allowed,
        rules: decision.rules,
        policy: state.policy.current().digest().to_string(),
        groups,
        groups_at,
        teams: caller
            .principals
            .teams()
            .iter()
            .map(|(team, membership)| TeamMembershipDto {
                team: team.clone(),
                membership: membership.clone(),
            })
            .collect(),
    })
    .into_response()
}
