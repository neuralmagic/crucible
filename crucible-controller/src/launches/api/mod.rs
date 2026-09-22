//! The launches slice's HTTP handlers, mounted by [`crate::api`].

pub(crate) mod emissions;
pub(crate) mod playbook_runs;
pub(crate) mod schedules;
pub(crate) mod watches;

use crate::api::dto::not_found;
use crate::api::state::{ApiState, AppError};
use crate::authz::action::{ResourceType, Verb};
use crate::authz::decision::Resource;
use crate::authz::model::Principal;
use crate::launches::standing::{self, OwnerSnapshot};
use axum::response::{IntoResponse, Response};

/// Look up a standing launch's owner snapshot and decide whether the caller may act on it.
#[allow(clippy::result_large_err)]
pub(crate) async fn authorize_standing(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    verb: Verb,
    missing: &str,
) -> Result<OwnerSnapshot, Response> {
    let snapshot = match standing::owner_snapshot(state.db.pool(), id).await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return Err(not_found(missing)),
        Err(e) => return Err(AppError::from(e).into_response()),
    };
    let existing = Resource::new(
        ResourceType::StandingLaunch,
        id,
        Principal::stored(snapshot.principal.as_deref()),
    );
    crate::authz::owner::decide_on(state, caller, &existing, verb)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(snapshot)
}
