//! The launches slice's HTTP handlers, mounted by [`crate::api`].

pub(crate) mod emissions;
pub(crate) mod playbook_runs;
pub(crate) mod schedules;
pub(crate) mod watches;
pub(crate) mod webhooks;

use crate::api::dto::{conflict, not_found};
use crate::api::state::{ApiState, AppError};
use crate::authz::action::{ResourceType, Verb};
use crate::authz::decision::Resource;
use crate::authz::model::Principal;
use crate::launches::standing::{self, OwnerSnapshot, Unadoptable};
use axum::response::{IntoResponse, Response};

/// The response for a standing-launch save the store refused: a registry row that left or moved
/// since the endpoint authorized it is a 404 or a 409, anything else a 500.
pub(crate) fn save_failed(e: anyhow::Error) -> Response {
    match e.downcast_ref::<Unadoptable>() {
        Some(gone @ Unadoptable::Gone { .. }) => not_found(gone.to_string()),
        Some(moved @ Unadoptable::Repinned { .. }) => conflict(moved.to_string()),
        None => AppError::from(e).into_response(),
    }
}

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

#[cfg(test)]
mod tests {
    use crate::launches::api::save_failed;
    use crate::launches::standing::Unadoptable;
    use anyhow::Context as _;
    use axum::http::StatusCode;

    /// A refusal keeps its status through the context the stores wrap it in.
    #[test]
    fn a_moved_or_missing_registry_row_is_a_client_refusal() {
        let wrapped = |refusal: Unadoptable| {
            Err::<(), _>(refusal)
                .context("create schedule")
                .expect_err("refused")
        };
        let repinned = save_failed(wrapped(Unadoptable::Repinned {
            playbook: "survey".to_string(),
            rev: "def456".to_string(),
        }));
        assert_eq!(repinned.status(), StatusCode::CONFLICT);
        let gone = save_failed(wrapped(Unadoptable::Gone {
            playbook: "survey".to_string(),
        }));
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
        let other = save_failed(anyhow::Error::msg("connection reset"));
        assert_eq!(other.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
