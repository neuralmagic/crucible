//! Per-resource shares (RFC-0003 C-SHARING): a grantee holds a share role on one resource until
//! a not-after time. The shares on a resource are part of its read; granting needs `share` and
//! every action the role confers; revoking needs `share` or platform administration.

#![allow(clippy::disallowed_macros)]

use crate::api::dto::*;
use crate::api::state::*;
use crate::authz::Caller;
use crate::authz::action::{Action, Verb};
use crate::authz::decision::{Resource, ShareRole};
use crate::authz::granted::{attach, confers, expired};
use crate::authz::model::Principal;
use crate::authz::owner::may;
use crate::authz::store::{self, AuditEvent, ShareRow};
use crate::authz::transfer::{MODEL_PROVIDERS, Owned, PACK_IMPORTS, PLAYBOOK_DRAFTS, PLAYBOOKS};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// One share as the API serves it.
#[derive(Debug, Serialize, ToSchema)]
pub struct ShareDto {
    pub grantee: String,
    pub role: ShareRole,
    pub not_after: Option<String>,
    /// Whether `not_after` has passed: the share lists, and allows nothing.
    pub expired: bool,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl ShareDto {
    fn from_row(row: ShareRow, now: jiff::Timestamp) -> Option<Self> {
        let role = ShareRole::parse(&row.role).ok()?;
        Some(ShareDto {
            expired: expired(row.not_after.as_deref(), now),
            grantee: row.grantee,
            role,
            not_after: row.not_after,
            created_by: row.created_by,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ShareBody {
    pub role: ShareRole,
    /// RFC 3339; absent means the share does not expire.
    #[serde(default)]
    pub not_after: Option<String>,
}

#[allow(clippy::result_large_err)]
async fn resource_of(
    state: &ApiState,
    caller: &Caller,
    owned: Owned,
    id: &str,
) -> Result<Resource, Response> {
    let owner = crate::authz::transfer::owner_of(state.db.pool(), owned, id)
        .await
        .map_err(|e| AppError::from(e).into_response())?
        .ok_or_else(|| not_found(format!("no {} {id:?}", owned.rtype.as_str())))?;
    attach(
        state.db.pool(),
        &caller.principals,
        Resource::new(owned.rtype, id, owner),
    )
    .await
    .map_err(|e| AppError::from(e).into_response())
}

fn event(caller: &Caller, owned: Owned, id: &str, rule: String) -> AuditEvent {
    let action = Action {
        resource: owned.rtype,
        verb: Verb::Share,
    };
    caller.audit_event(action, id, true, rule)
}

fn share_json(row: &ShareRow) -> serde_json::Value {
    serde_json::json!({"grantee": row.grantee, "role": row.role, "not_after": row.not_after})
}

/// The shares on a resource the caller may read.
pub async fn list(state: &ApiState, caller: &Caller, owned: Owned, id: &str) -> Response {
    let resource = match resource_of(state, caller, owned, id).await {
        Ok(r) => r,
        Err(refusal) => return refusal,
    };
    if let Err(refused) = crate::authz::owner::decide_on(state, caller, &resource, Verb::Read).await
    {
        return refused.into_response();
    }
    let rows = match store::shares_of(state.db.pool(), owned.rtype.as_str(), id).await {
        Ok(rows) => rows,
        Err(e) => return AppError::from(e).into_response(),
    };
    let now = jiff::Timestamp::now();
    let shares: Vec<ShareDto> = rows
        .into_iter()
        .filter_map(|row| ShareDto::from_row(row, now))
        .collect();
    Json(shares).into_response()
}

/// Grant or change a share: decided as `share`, and refused when the role confers an action the
/// sharer does not hold on the resource.
pub async fn grant(
    state: &ApiState,
    caller: &Caller,
    owned: Owned,
    id: &str,
    grantee: &str,
    body: &ShareBody,
) -> Response {
    let resource = match resource_of(state, caller, owned, id).await {
        Ok(r) => r,
        Err(refusal) => return refusal,
    };
    let decision = match crate::authz::owner::decide_on(state, caller, &resource, Verb::Share).await
    {
        Ok(decision) => decision,
        Err(refused) => return refused.into_response(),
    };
    let grantee = match Principal::parse(grantee) {
        Ok(p) if matches!(p, Principal::User(_) | Principal::Team(_)) => p,
        Ok(p) => return unprocessable(format!("{p} cannot hold a share")),
        Err(e) => return unprocessable(e.to_string()),
    };
    if grantee == resource.owner {
        return unprocessable(format!("{grantee} owns {id}"));
    }
    if let Some(not_after) = body.not_after.as_deref()
        && not_after.parse::<jiff::Timestamp>().is_err()
    {
        return unprocessable(format!(
            "not_after {not_after:?} is not an RFC 3339 timestamp"
        ));
    }
    let lacking: Vec<String> = confers(body.role)
        .iter()
        .copied()
        .filter(|verb| owned.rtype.defines(*verb))
        .filter(|verb| !may(state, caller, *verb, &resource))
        .map(|verb| {
            Action {
                resource: owned.rtype,
                verb,
            }
            .to_string()
        })
        .collect();
    if !lacking.is_empty() {
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorBody::new(format!(
                "a {} share confers {}, which you do not hold on {id}",
                body.role.as_str(),
                lacking.join(", ")
            ))),
        )
            .into_response();
    }
    let now = crate::clock::now_rfc3339();
    let row = ShareRow {
        resource_type: owned.rtype.as_str().to_string(),
        resource_id: id.to_string(),
        grantee: grantee.to_string(),
        role: body.role.as_str().to_string(),
        not_after: body.not_after.clone(),
        created_by: caller.actor().map(|p| p.to_string()),
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    let mut tx = match state.db.pool().begin().await {
        Ok(tx) => tx,
        Err(e) => return AppError::from(e).into_response(),
    };
    let prior = match store::upsert_share(&mut tx, &row).await {
        Ok(prior) => prior,
        Err(e) => return AppError::from(e).into_response(),
    };
    let mut audit = event(caller, owned, id, decision.reason());
    audit.prior = prior.as_ref().map(share_json);
    audit.result = Some(share_json(&row));
    if let Err(e) = store::audit(&mut tx, &audit, &now).await {
        return AppError::from(e).into_response();
    }
    if let Err(e) = tx.commit().await {
        return AppError::from(e).into_response();
    }
    let status = if prior.is_some() {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    match ShareDto::from_row(row, jiff::Timestamp::now()) {
        Some(dto) => (status, Json(dto)).into_response(),
        None => {
            AppError::from(anyhow::anyhow!("the share's role did not read back")).into_response()
        }
    }
}

/// Revoke a share: `share` on the resource, or platform administration.
pub async fn revoke(
    state: &ApiState,
    caller: &Caller,
    owned: Owned,
    id: &str,
    grantee: &str,
) -> Response {
    let resource = match resource_of(state, caller, owned, id).await {
        Ok(r) => r,
        Err(refusal) => return refusal,
    };
    let rule = if caller.principals.is_platform_admin() {
        "platform-admin-all".to_string()
    } else {
        match crate::authz::owner::decide_on(state, caller, &resource, Verb::Share).await {
            Ok(decision) => decision.reason(),
            Err(refused) => return refused.into_response(),
        }
    };
    let mut tx = match state.db.pool().begin().await {
        Ok(tx) => tx,
        Err(e) => return AppError::from(e).into_response(),
    };
    let prior = match store::delete_share(&mut tx, owned.rtype.as_str(), id, grantee).await {
        Ok(Some(prior)) => prior,
        Ok(None) => return not_found(format!("no share of {id} with {grantee}")),
        Err(e) => return AppError::from(e).into_response(),
    };
    let now = crate::clock::now_rfc3339();
    let mut audit = event(caller, owned, id, rule);
    audit.prior = Some(share_json(&prior));
    if let Err(e) = store::audit(&mut tx, &audit, &now).await {
        return AppError::from(e).into_response();
    }
    if let Err(e) = tx.commit().await {
        return AppError::from(e).into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

macro_rules! share_routes {
    ($list:ident, $grant:ident, $revoke:ident, $base:literal, $owned:expr, $what:literal) => {
        #[doc = concat!("`GET ", $base, "/shares` — the shares on ", $what, ".")]
        #[utoipa::path(
            get,
            path = concat!($base, "/shares"),
            params(("id" = String, Path, description = "Resource id")),
            responses(
                (status = 200, description = "The shares, expired ones included", body = Vec<ShareDto>),
                (status = 404, description = "No such resource", body = ErrorBody)
            )
        )]
        pub(crate) async fn $list(
            State(state): State<ApiState>,
            Path(id): Path<String>,
            caller: Caller,
        ) -> Response {
            list(&state, &caller, $owned, &id).await
        }

        #[doc = concat!("`PUT ", $base, "/shares/{grantee}` — grant or change a share on ", $what, ".")]
        #[utoipa::path(
            put,
            path = concat!($base, "/shares/{grantee}"),
            params(
                ("id" = String, Path, description = "Resource id"),
                ("grantee" = String, Path, description = "`user:<login>` or `team:<slug>`")
            ),
            request_body = ShareBody,
            responses(
                (status = 200, description = "The share, changed", body = ShareDto),
                (status = 201, description = "The share, granted", body = ShareDto),
                (status = 403, description = "Not allowed to share, or the role confers more than the sharer holds", body = ErrorBody),
                (status = 404, description = "No such resource", body = ErrorBody),
                (status = 422, description = "The grantee, role, or not-after is malformed", body = ErrorBody)
            )
        )]
        pub(crate) async fn $grant(
            State(state): State<ApiState>,
            Path((id, grantee)): Path<(String, String)>,
            caller: Caller,
            Json(body): Json<ShareBody>,
        ) -> Response {
            grant(&state, &caller, $owned, &id, &grantee, &body).await
        }

        #[doc = concat!("`DELETE ", $base, "/shares/{grantee}` — revoke a share on ", $what, ".")]
        #[utoipa::path(
            delete,
            path = concat!($base, "/shares/{grantee}"),
            params(
                ("id" = String, Path, description = "Resource id"),
                ("grantee" = String, Path, description = "`user:<login>` or `team:<slug>`")
            ),
            responses(
                (status = 204, description = "Revoked"),
                (status = 403, description = "Not allowed to share", body = ErrorBody),
                (status = 404, description = "No such resource or share", body = ErrorBody)
            )
        )]
        pub(crate) async fn $revoke(
            State(state): State<ApiState>,
            Path((id, grantee)): Path<(String, String)>,
            caller: Caller,
        ) -> Response {
            revoke(&state, &caller, $owned, &id, &grantee).await
        }
    };
}

share_routes!(
    list_playbook_shares,
    grant_playbook_share,
    revoke_playbook_share,
    "/api/playbooks/{id}",
    PLAYBOOKS,
    "a playbook"
);
share_routes!(
    list_playbook_draft_shares,
    grant_playbook_draft_share,
    revoke_playbook_draft_share,
    "/api/playbook-drafts/{id}",
    PLAYBOOK_DRAFTS,
    "a draft"
);
share_routes!(
    list_pack_import_shares,
    grant_pack_import_share,
    revoke_pack_import_share,
    "/api/playbooks/imports/{id}",
    PACK_IMPORTS,
    "a pack import"
);
share_routes!(
    list_provider_shares,
    grant_provider_share,
    revoke_provider_share,
    "/api/providers/{id}",
    MODEL_PROVIDERS,
    "a model provider"
);
