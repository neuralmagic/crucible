//! The blueprint surface (RFC-0006): marking a registered playbook as one, making it public, the
//! catalog, and starting a draft from one.

use crate::api::dto::*;
use crate::api::state::*;
use crate::authz::action::{Action, ResourceType, Verb};
use crate::authz::decision::Resource;
use crate::playbooks::blueprints::{Placeholder, PlaceholderReason, barred};
use crate::playbooks::registry::PlaybookRow;
use crate::secrets::manifest::DeclaredSecret;
use crate::secrets::store::{self, BindingRow, SecretRow};
use crate::secrets::{AuditAction, ProjectionKind, ScopeKind, SecretMode};
use anyhow::Context as _;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Whether a registered playbook is a blueprint, and whether it is public.
#[derive(Debug, Serialize, ToSchema)]
pub struct BlueprintStateDto {
    pub id: String,
    pub blueprint: bool,
    pub public: bool,
}

/// A declared secret a draft started from a blueprint still has to bind.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlaceholderDto {
    pub declared_name: String,
    pub projection_kind: Option<ProjectionKind>,
    pub projection: Option<String>,
    pub reason: PlaceholderReason,
}

impl From<Placeholder> for PlaceholderDto {
    fn from(p: Placeholder) -> Self {
        let (projection_kind, projection) = p.projection.unzip();
        PlaceholderDto {
            declared_name: p.declared_name.to_string(),
            projection_kind,
            projection,
            reason: p.reason,
        }
    }
}

/// One secret a blueprint declares, as it would reach the caller.
#[derive(Debug, Serialize, ToSchema)]
pub struct BlueprintSecretDto {
    pub declared_name: String,
    /// Bound for the caller by carrying; otherwise a placeholder for `reason`.
    pub carries: bool,
    pub reason: Option<PlaceholderReason>,
}

/// One catalog entry.
#[derive(Debug, Serialize, ToSchema)]
pub struct BlueprintDto {
    pub id: String,
    pub description: String,
    /// `user:<login>` or `team:<slug>`.
    pub owner: String,
    pub rev: String,
    pub public: bool,
    pub secrets: Vec<BlueprintSecretDto>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct UseBlueprintBody {
    /// The new draft's id: a lowercase slug no draft or registered playbook holds.
    id: String,
    /// Absent ⇒ the blueprint's.
    #[serde(default)]
    description: Option<String>,
    /// The principal to own the draft: `user:<login>` or `team:<slug>` the caller acts as; absent
    /// means the caller.
    #[serde(default)]
    owner: Option<String>,
    /// The revision the caller inspected; refused when the blueprint has moved since.
    #[serde(default)]
    expected_rev: Option<String>,
    /// The caller's own Vault token, to carry bindings of reference-mode secrets. Never stored.
    #[serde(default)]
    vault_token: Option<String>,
}

/// A draft started from a blueprint: its first version, the bindings it carried, and the
/// placeholders it has to fill.
#[derive(Debug, Serialize, ToSchema)]
pub struct BlueprintUsedDto {
    pub id: String,
    pub version: i64,
    pub bindings: Vec<crate::secrets::api::SecretBindingDto>,
    pub placeholders: Vec<PlaceholderDto>,
}

/// Whether a plan may read Vault: a use checks what it carries, the catalog only previews it.
#[derive(Debug, Clone, Copy)]
enum Mode<'a> {
    Preview,
    Use { vault_token: Option<&'a str> },
}

/// What a use of a blueprint would do for one caller.
struct Plan {
    carried: Vec<(BindingRow, SecretRow)>,
    placeholders: Vec<Placeholder>,
}

/// Whether `binding` carries to `caller`, or why not.
#[allow(clippy::result_large_err)]
async fn carries(
    state: &ApiState,
    caller: &crate::authz::Caller,
    blueprint: &PlaybookRow,
    binding: &BindingRow,
    secret: &SecretRow,
    mode: Mode<'_>,
) -> Result<Result<(), PlaceholderReason>, Response> {
    if let Some(reason) = barred(blueprint, binding, secret) {
        return Ok(Err(reason));
    }
    let mut resource = Resource::new(ResourceType::Secret, &secret.id, secret.owner.clone());
    resource.origin_owner = Some(blueprint.owner.clone());
    let in_team = caller
        .subject()
        .is_some_and(|subject| resource.owner_role(&subject).is_some());
    if !in_team {
        return Ok(Err(PlaceholderReason::NotInTeam));
    }
    if !crate::authz::owner::may(state, caller, Verb::Bind, &resource) {
        return Ok(Err(PlaceholderReason::PolicyDenied));
    }
    match mode {
        Mode::Preview if secret.mode == SecretMode::Reference => {
            Ok(Err(PlaceholderReason::Unverified))
        }
        Mode::Preview => Ok(Ok(())),
        Mode::Use { vault_token } => {
            match crate::secrets::api::check_bindable(state, secret, vault_token).await {
                Ok(()) => Ok(Ok(())),
                Err(refusal) if refusal.is_unavailable() => Err(refusal.into_response()),
                Err(_) => Ok(Err(PlaceholderReason::Unverified)),
            }
        }
    }
}

#[allow(clippy::result_large_err)]
async fn plan(
    state: &ApiState,
    caller: &crate::authz::Caller,
    blueprint: &PlaybookRow,
    declared: &[DeclaredSecret],
    mode: Mode<'_>,
) -> Result<Plan, Response> {
    let bound = store::bindings_for_scope(state.db.pool(), ScopeKind::Playbook, &blueprint.id)
        .await
        .map_err(|e| AppError::from(e).into_response())?;
    let mut carried = Vec::new();
    let mut refused = Vec::new();
    for (binding, secret) in bound {
        match carries(state, caller, blueprint, &binding, &secret, mode).await? {
            Ok(()) => carried.push((binding, secret)),
            Err(reason) => refused.push((binding, reason)),
        }
    }
    let placeholders = declared
        .iter()
        .filter(|d| !carried.iter().any(|(b, _)| b.declared_name == d.name))
        .map(
            |d| match refused.iter().find(|(b, _)| b.declared_name == d.name) {
                Some((binding, reason)) => Placeholder {
                    declared_name: d.name.clone(),
                    projection: Some((binding.projection_kind, binding.projection.clone())),
                    reason: *reason,
                },
                None => Placeholder {
                    declared_name: d.name.clone(),
                    projection: d.projection.clone(),
                    reason: PlaceholderReason::Unbound,
                },
            },
        )
        .collect();
    Ok(Plan {
        carried,
        placeholders,
    })
}

/// The blueprint `id`, decided for `verb`. A playbook that is not a blueprint is not found.
#[allow(clippy::result_large_err)]
async fn readable_blueprint(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    verb: Verb,
) -> Result<PlaybookRow, Response> {
    let row = crate::playbooks::api::registry::readable_playbook(state, caller, id, verb).await?;
    if !row.blueprint {
        return Err(not_found(format!("no blueprint {id:?}")));
    }
    Ok(row)
}

fn state_of(row: &PlaybookRow) -> BlueprintStateDto {
    BlueprintStateDto {
        id: row.id.clone(),
        blueprint: row.blueprint,
        public: row.public,
    }
}

/// `PUT /api/playbooks/{id}/blueprint` — offer a registered playbook as a blueprint. Refused
/// unless the caller may bind every secret whose binding would then carry.
#[utoipa::path(
    put,
    path = "/api/playbooks/{id}/blueprint",
    params(("id" = String, Path, description = "Registry id")),
    responses(
        (status = 200, description = "The playbook is a blueprint", body = BlueprintStateDto),
        (status = 403, description = "The caller may not update it, or may not bind a secret that would carry", body = ErrorBody),
        (status = 404, description = "No playbook with that id", body = ErrorBody)
    )
)]
pub(crate) async fn mark_blueprint(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let mut row = match crate::playbooks::api::registry::readable_playbook(
        &state,
        &caller,
        &id,
        Verb::Update,
    )
    .await
    {
        Ok(row) => row,
        Err(refused) => return Ok(refused),
    };
    let mut named = Vec::new();
    let mut unnamed = 0;
    for (binding, secret) in
        store::bindings_for_scope(state.db.pool(), ScopeKind::Playbook, &id).await?
    {
        if barred(&row, &binding, &secret).is_some() {
            continue;
        }
        let resource = Resource::new(ResourceType::Secret, &secret.id, secret.owner.clone());
        if crate::authz::owner::may(&state, &caller, Verb::Bind, &resource) {
            continue;
        }
        if crate::authz::owner::may(&state, &caller, Verb::Read, &resource) {
            named.push(secret.name.to_string());
        } else {
            unnamed += 1;
        }
    }
    if !named.is_empty() || unnamed > 0 {
        let mut lacking = named;
        if unnamed > 0 {
            lacking.push(format!("{unnamed} more"));
        }
        return Ok((
            StatusCode::FORBIDDEN,
            Json(ErrorBody::new(format!(
                "marking {id} would carry secrets you may not bind: {}",
                lacking.join(", ")
            ))),
        )
            .into_response());
    }
    crate::playbooks::registry::set_blueprint(state.db.pool(), &id, true).await?;
    row.blueprint = true;
    state
        .audit(
            crate::event_log::Event::now(
                &format!("playbook:{id}"),
                "blueprint",
                "marked",
                Some(&format!("playbook {id} marked as a blueprint")),
                None,
            )
            .by(identity.as_deref()),
            "mark_blueprint",
        )
        .await;
    Ok(Json(state_of(&row)).into_response())
}

/// `DELETE /api/playbooks/{id}/blueprint` — stop offering a playbook as a blueprint, which also
/// makes it private. Drafts already started from it keep what they carried.
#[utoipa::path(
    delete,
    path = "/api/playbooks/{id}/blueprint",
    params(("id" = String, Path, description = "Registry id")),
    responses(
        (status = 200, description = "The playbook is not a blueprint", body = BlueprintStateDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No playbook with that id", body = ErrorBody)
    )
)]
pub(crate) async fn unmark_blueprint(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let mut row = match crate::playbooks::api::registry::readable_playbook(
        &state,
        &caller,
        &id,
        Verb::Update,
    )
    .await
    {
        Ok(row) => row,
        Err(refused) => return Ok(refused),
    };
    crate::playbooks::registry::set_blueprint(state.db.pool(), &id, false).await?;
    row.blueprint = false;
    row.public = false;
    state
        .audit(
            crate::event_log::Event::now(
                &format!("playbook:{id}"),
                "blueprint",
                "unmarked",
                Some(&format!("playbook {id} is no longer a blueprint")),
                None,
            )
            .by(identity.as_deref()),
            "unmark_blueprint",
        )
        .await;
    Ok(Json(state_of(&row)).into_response())
}

async fn set_public(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    public: bool,
) -> Result<Response, AppError> {
    let Some(mut row) = crate::playbooks::registry::get(state.db.pool(), id).await? else {
        return Ok(not_found(format!("no playbook {id:?}")));
    };
    let decision =
        match crate::authz::owner::decide_on(state, caller, &row.resource(), Verb::Share).await {
            Ok(decision) => decision,
            Err(refused) => return Ok(refused.into_response()),
        };
    let now = crate::clock::now_rfc3339();
    let mut tx = state.db.pool().begin().await?;
    let Some(prior) = crate::playbooks::registry::set_public(&mut tx, id, public).await? else {
        return Ok(conflict(format!("playbook {id} is not a blueprint")));
    };
    let action = Action {
        resource: ResourceType::Playbook,
        verb: Verb::Share,
    };
    let mut audit = caller.audit_event(action, id, true, decision.reason());
    audit.prior = Some(serde_json::json!({ "public": prior }));
    audit.result = Some(serde_json::json!({ "public": public }));
    crate::authz::store::audit(&mut tx, &audit, &now).await?;
    tx.commit().await?;
    row.public = public;
    Ok(Json(state_of(&row)).into_response())
}

/// `PUT /api/playbooks/{id}/blueprint/public` — let every user read a blueprint.
#[utoipa::path(
    put,
    path = "/api/playbooks/{id}/blueprint/public",
    params(("id" = String, Path, description = "Registry id")),
    responses(
        (status = 200, description = "The blueprint is public", body = BlueprintStateDto),
        (status = 403, description = "The caller may not share it", body = ErrorBody),
        (status = 404, description = "No playbook with that id", body = ErrorBody),
        (status = 409, description = "The playbook is not a blueprint", body = ErrorBody)
    )
)]
pub(crate) async fn publish_blueprint(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    set_public(&state, &caller, &id, true).await
}

/// `DELETE /api/playbooks/{id}/blueprint/public` — make a blueprint private again.
#[utoipa::path(
    delete,
    path = "/api/playbooks/{id}/blueprint/public",
    params(("id" = String, Path, description = "Registry id")),
    responses(
        (status = 200, description = "The blueprint is private", body = BlueprintStateDto),
        (status = 403, description = "The caller may not share it", body = ErrorBody),
        (status = 404, description = "No playbook with that id", body = ErrorBody),
        (status = 409, description = "The playbook is not a blueprint", body = ErrorBody)
    )
)]
pub(crate) async fn unpublish_blueprint(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    set_public(&state, &caller, &id, false).await
}

/// `GET /api/blueprints` — the blueprints the caller may read, each with what its secrets would
/// do for them.
#[utoipa::path(
    get,
    path = "/api/blueprints",
    responses((status = 200, description = "The catalog", body = Vec<BlueprintDto>))
)]
pub(crate) async fn list_blueprints(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
) -> Result<Response, AppError> {
    let rows = crate::playbooks::registry::list_blueprints(state.db.pool()).await?;
    let rows = crate::authz::owner::readable(&state, &caller, ResourceType::Playbook, rows, |r| {
        r.resource()
    })
    .await?;
    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let declared = crate::playbooks::blueprints::declared(state.db.pool(), &row).await?;
        let plan = match plan(&state, &caller, &row, &declared, Mode::Preview).await {
            Ok(plan) => plan,
            Err(refused) => return Ok(refused),
        };
        let secrets = declared
            .iter()
            .map(|d| {
                let reason = plan
                    .placeholders
                    .iter()
                    .find(|p| p.declared_name == d.name)
                    .map(|p| p.reason);
                BlueprintSecretDto {
                    declared_name: d.name.to_string(),
                    carries: reason.is_none(),
                    reason,
                }
            })
            .collect();
        entries.push(BlueprintDto {
            id: row.id,
            description: row.description,
            owner: row.owner.to_string(),
            rev: row.rev,
            public: row.public,
            secrets,
        });
    }
    Ok(Json(entries).into_response())
}

/// `POST /api/blueprints/{id}/use` — start a draft of the caller's own from a blueprint. The
/// owning team's secrets carry for its members; every other declared secret becomes a
/// placeholder. A refused use leaves no draft and no binding.
#[utoipa::path(
    post,
    path = "/api/blueprints/{id}/use",
    params(("id" = String, Path, description = "Blueprint id")),
    request_body = UseBlueprintBody,
    responses(
        (status = 201, description = "The new draft", body = BlueprintUsedDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No blueprint with that id", body = ErrorBody),
        (status = 409, description = "The new id is taken, or the blueprint moved from the expected revision", body = ErrorBody),
        (status = 422, description = "Bad id or description", body = ErrorBody),
        (status = 503, description = "No Vault client to check a carried secret", body = ErrorBody)
    )
)]
pub(crate) async fn use_blueprint(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<UseBlueprintBody>,
) -> Result<Response, AppError> {
    if let Some(msg) = require_non_empty(&[("id", &body.id)]) {
        return Ok(unprocessable(msg));
    }
    let blueprint = match readable_blueprint(&state, &caller, &id, Verb::Read).await {
        Ok(row) => row,
        Err(refused) => return Ok(refused),
    };
    if let Some(expected) = body.expected_rev.as_deref()
        && expected != blueprint.rev
    {
        return Ok(conflict(format!(
            "blueprint {id} moved from revision {expected} to {}; reload before using it",
            blueprint.rev
        )));
    }
    let owner = match crate::authz::owner::owner_for_create(
        &state,
        &caller,
        body.owner.as_deref(),
        ResourceType::PlaybookDraft,
    )
    .await
    {
        Ok(owner) => owner,
        Err(refused) => return Ok(refused),
    };
    let declared = crate::playbooks::blueprints::declared(state.db.pool(), &blueprint).await?;
    let mode = Mode::Use {
        vault_token: body.vault_token.as_deref(),
    };
    let plan = match plan(&state, &caller, &blueprint, &declared, mode).await {
        Ok(plan) => plan,
        Err(refused) => return Ok(refused),
    };

    let to = body.id.trim().to_string();
    let description = body
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or(&blueprint.description);
    let digest = blueprint.tree_digest.as_ref().map(ToString::to_string);
    let actor = identity.as_deref();
    let saved = match crate::playbooks::drafts::create(
        state.db.pool(),
        &to,
        description,
        crate::playbooks::drafts::DraftSeed::Template {
            id: &id,
            expected_rev: Some(&blueprint.rev),
            expected_digest: digest.as_deref(),
        },
        actor,
        &owner,
    )
    .await
    {
        Ok(saved) => saved,
        Err(e) => return Ok(crate::playbooks::api::drafts::draft_refusal(e)),
    };
    let bindings = match carry(&state, &caller, &id, &to, &plan).await {
        Ok(bindings) => bindings,
        Err(e) => {
            if let Err(cleanup) = crate::playbooks::drafts::delete(
                state.db.pool(),
                &to,
                caller.actor(),
                caller.subject_label(),
            )
            .await
            {
                tracing::error!(draft = %to, error = %cleanup, "removing a draft whose blueprint use failed");
            }
            return Ok(match e {
                e @ store::StoreError::Duplicate { .. } => conflict(e.to_string()),
                store::StoreError::Internal(e) => AppError::from(e).into_response(),
            });
        }
    };
    state
        .audit(
            crate::event_log::Event::now(
                &format!("draft:{to}"),
                "created",
                "created",
                Some(&format!("draft {to} started from blueprint {id}")),
                None,
            )
            .by(actor),
            "use_blueprint",
        )
        .await;
    Ok((
        StatusCode::CREATED,
        Json(BlueprintUsedDto {
            id: to,
            version: saved.version,
            bindings: bindings
                .into_iter()
                .map(crate::secrets::api::SecretBindingDto::from)
                .collect(),
            placeholders: plan
                .placeholders
                .iter()
                .cloned()
                .map(PlaceholderDto::from)
                .collect(),
        }),
    )
        .into_response())
}

/// Store the plan's carried bindings, each audited as a bind, and its placeholders, in one
/// transaction.
async fn carry(
    state: &ApiState,
    caller: &crate::authz::Caller,
    blueprint: &str,
    draft: &str,
    plan: &Plan,
) -> Result<Vec<BindingRow>, store::StoreError> {
    let mut tx = state
        .db
        .pool()
        .begin()
        .await
        .context("opening the carry transaction")?;
    let mut stored = Vec::with_capacity(plan.carried.len());
    for (binding, secret) in &plan.carried {
        let id = uuid::Uuid::now_v7().to_string();
        let row = store::insert_carried_binding(
            &mut tx,
            &store::NewBinding {
                id: &id,
                secret_id: &secret.id,
                scope_kind: ScopeKind::Playbook,
                scope_id: draft,
                projection_kind: binding.projection_kind,
                projection: &binding.projection,
                declared_name: &binding.declared_name,
                pack_rev: None,
                schema_digest: None,
                created_by: caller.actor_principals().login(),
            },
            store::Origin {
                blueprint,
                binding: &binding.id,
            },
        )
        .await?;
        let detail = format!(
            "playbook {draft} as {} {}, carried from blueprint {blueprint}",
            row.projection_kind.as_str(),
            row.projection
        );
        store::audit(
            &mut tx,
            &store::NewAudit {
                secret_id: Some(&secret.id),
                secret_name: secret.name.as_str(),
                owner: &secret.owner,
                action: AuditAction::Bind,
                actor: caller.actor(),
                subject: caller.subject_label(),
                detail: Some(&detail),
            },
        )
        .await?;
        stored.push(row);
    }
    crate::playbooks::blueprints::insert_placeholders(&mut tx, draft, &plan.placeholders).await?;
    tx.commit().await.context("committing the carry")?;
    Ok(stored)
}

#[cfg(test)]
mod tests;
