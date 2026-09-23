//! `transfer` on a root resource (RFC-0003 C-OWNERSHIP): the owner column of one table changes,
//! only to a principal the caller acts as (or, for a platform administrator, any user or team),
//! and the change is audited with the prior and new owner.

use crate::api::dto::*;
use crate::api::state::*;
use crate::authz::Caller;
use crate::authz::action::{Action, ResourceType, Verb};
use crate::authz::decision::Resource;
use crate::authz::model::Principal;
use crate::authz::owner::decide;
use crate::authz::store;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;

/// The table a governed root resource's owner lives in.
#[derive(Debug, Clone, Copy)]
pub struct Owned {
    pub rtype: ResourceType,
    pub table: &'static str,
    pub id_column: &'static str,
}

pub const PLAYBOOKS: Owned = Owned {
    rtype: ResourceType::Playbook,
    table: "playbooks",
    id_column: "id",
};
pub const PLAYBOOK_DRAFTS: Owned = Owned {
    rtype: ResourceType::PlaybookDraft,
    table: "playbook_drafts",
    id_column: "id",
};
pub const PACK_IMPORTS: Owned = Owned {
    rtype: ResourceType::PackImport,
    table: "pack_imports",
    id_column: "id",
};
pub const MODEL_PROVIDERS: Owned = Owned {
    rtype: ResourceType::ModelProvider,
    table: "model_providers",
    id_column: "id",
};

#[derive(Debug, Deserialize, ToSchema)]
pub struct TransferBody {
    /// The new owner, `user:<login>` or `team:<slug>`.
    pub owner: String,
}

/// What a transfer changed.
#[derive(Debug, Serialize, ToSchema)]
pub struct TransferDto {
    pub id: String,
    pub prior: String,
    pub owner: String,
}

/// The current owner of one row, or `None` when there is no such row.
pub async fn owner_of(
    pool: &sqlx::PgPool,
    owned: Owned,
    id: &str,
) -> anyhow::Result<Option<Principal>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT owner FROM {} WHERE {} = $1",
        owned.table, owned.id_column
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.map(|r| {
        let raw: String = r.try_get("owner")?;
        Principal::parse(&raw).map_err(anyhow::Error::from)
    })
    .transpose()
}

/// Move `id` to `body.owner`: decided as `transfer` against the current owner, the new owner must
/// be one the caller acts as unless they administer the platform, and the change lands with its
/// audit row.
pub async fn transfer(
    state: &ApiState,
    caller: &Caller,
    owned: Owned,
    id: &str,
    body: &TransferBody,
) -> Response {
    let pool = state.db.pool();
    let prior = match owner_of(pool, owned, id).await {
        Ok(Some(owner)) => owner,
        Ok(None) => return not_found(format!("no {} {id:?}", owned.rtype.as_str())),
        Err(e) => return AppError::from(e).into_response(),
    };
    let action = Action {
        resource: owned.rtype,
        verb: Verb::Transfer,
    };
    let resource = Resource::new(owned.rtype, id, prior.clone());
    let decision = match decide(state, caller, action, &resource).await {
        Ok(decision) => decision,
        Err(denied) => return denied.into_response(),
    };
    let next = match Principal::parse(&body.owner) {
        Ok(p) if p.may_own() => p,
        Ok(p) => return unprocessable(format!("{p} may not own a resource")),
        Err(e) => return unprocessable(e.to_string()),
    };
    if next == prior {
        return unprocessable(format!("{id} is already owned by {prior}"));
    }
    if !caller.principals.covers(&next) && !caller.principals.is_platform_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorBody::new(format!(
                "{next} is not a principal you act as"
            ))),
        )
            .into_response();
    }
    let now = crate::clock::now_rfc3339();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return AppError::from(e).into_response(),
    };
    let done = match sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET owner = $2 WHERE {} = $1",
        owned.table, owned.id_column
    )))
    .bind(id)
    .bind(next.to_string())
    .execute(&mut *tx)
    .await
    {
        Ok(done) => done,
        Err(e) => return AppError::from(e).into_response(),
    };
    if done.rows_affected() != 1 {
        return not_found(format!("no {} {id:?}", owned.rtype.as_str()));
    }
    let mut event = caller.audit_event(action, id, true, decision.reason());
    event.prior = Some(serde_json::json!({"owner": prior.to_string()}));
    event.result = Some(serde_json::json!({"owner": next.to_string()}));
    if let Err(e) = store::audit(&mut tx, &event, &now).await {
        return AppError::from(e).into_response();
    }
    if let Err(e) = tx.commit().await {
        return AppError::from(e).into_response();
    }
    Json(TransferDto {
        id: id.to_string(),
        prior: prior.to_string(),
        owner: next.to_string(),
    })
    .into_response()
}

macro_rules! transfer_route {
    ($name:ident, $path:literal, $owned:expr, $what:literal) => {
        #[doc = concat!("`PUT ", $path, "` — transfer ", $what, " to another owner.")]
        #[utoipa::path(
            put,
            path = $path,
            params(("id" = String, Path, description = "Resource id")),
            request_body = TransferBody,
            responses(
                (status = 200, description = "The prior and new owner", body = TransferDto),
                (status = 403, description = "Not allowed to transfer, or the new owner is not acted as", body = ErrorBody),
                (status = 404, description = "No such resource", body = ErrorBody),
                (status = 422, description = "The new owner is malformed or unchanged", body = ErrorBody)
            )
        )]
        pub(crate) async fn $name(
            State(state): State<ApiState>,
            Path(id): Path<String>,
            caller: Caller,
            Json(body): Json<TransferBody>,
        ) -> Response {
            transfer(&state, &caller, $owned, &id, &body).await
        }
    };
}

transfer_route!(
    transfer_playbook,
    "/api/playbooks/{id}/owner",
    PLAYBOOKS,
    "a playbook"
);
transfer_route!(
    transfer_playbook_draft,
    "/api/playbook-drafts/{id}/owner",
    PLAYBOOK_DRAFTS,
    "a draft"
);
transfer_route!(
    transfer_pack_import,
    "/api/playbooks/imports/{id}/owner",
    PACK_IMPORTS,
    "a pack import"
);
transfer_route!(
    transfer_provider,
    "/api/providers/{id}/owner",
    MODEL_PROVIDERS,
    "a model provider"
);
