//! The secrets registry's HTTP surface: register, rotate, delete, bind, unbind, list, audit.
//!
//! One rule shapes every handler here: **no route returns a value**. A registration takes one,
//! writes it to Vault inside the request, and drops it; nothing that comes back out of this module
//! — a DTO, an error body, an audit row, a log line — can carry one, because no response type has
//! a field to put one in.
//!
//! The second rule is that authorization is decided here, on the server, from
//! [`crate::authz::model::Principals`] built out of the identity and groups the bearer guard proved.
//! The SPA's own filtering is convenience.

#![allow(clippy::disallowed_macros)]

use crate::api::dto::*;
use crate::dto::dto;

use crate::api::state::*;

use crate::secrets::store::{self, BindingRow, SecretRow};

use crate::secrets::vault::{KvData, VaultClient, VaultToken};

use crate::authz::model::Principal;
use crate::secrets::{
    AuditAction, ConsumerClass, ProjectionKind, ScopeKind, SecretKind, SecretMode, SecretName,
    SecretValue, Visibility,
};

use axum::extract::{Path, Query, State};

use axum::http::StatusCode;

use axum::response::{IntoResponse, Response};

use serde::{Deserialize, Serialize};

use utoipa::ToSchema;

/// How many audit rows one request returns.
const AUDIT_LIMIT: i64 = 200;

dto! {
    /// One registered secret, as the UI and the API see it. Metadata only.
    pub struct SecretDto: From<s: SecretRow> {
        pub id: String,
        pub name: String = s.name.to_string(),
        /// `user:<login>` or `group:<group path>`.
        pub owner: String = s.owner.to_string(),
        pub kind: SecretKind,
        pub visibility: Visibility,
        pub consumer: ConsumerClass,
        pub mode: SecretMode,
        /// Where the bytes live: a path under the registry's mount, or the registered `vault://` URL.
        /// A location, never a value.
        pub vault_path: String,
        /// The KV version the last write landed at; null for a reference, whose versions belong to the
        /// path's owner.
        pub current_version: Option<i64>,
        pub created_by: Option<String>,
        pub created_at: String,
        pub updated_at: String,
    }
}

dto! {
    /// One binding.
    pub struct SecretBindingDto: From<b: BindingRow> {
        pub id: String,
        pub secret_id: String,
        pub scope_kind: ScopeKind,
        pub scope_id: String,
        pub projection_kind: ProjectionKind,
        pub projection: String,
        /// The manifest-declared name this binding satisfies.
        pub declared_name: String = b.declared_name.to_string(),
        pub pack_rev: Option<String>,
        pub schema_digest: Option<String>,
        pub created_by: Option<String>,
        pub created_at: String,
    }
}

/// A secret with what it is bound to — the detail page's read.
#[derive(Debug, Serialize, ToSchema)]
pub struct SecretDetailDto {
    #[serde(flatten)]
    pub secret: SecretDto,
    pub bindings: Vec<SecretBindingDto>,
}

dto! {
    /// One line of the audit trail.
    pub struct SecretAuditDto: From<a: store::AuditRow> {
        pub id: i64,
        pub secret_id: Option<String>,
        pub secret_name: String,
        pub owner: String,
        pub action: AuditAction,
        /// The acting principal; null only where there is none (a hub read on its own behalf).
        pub actor: Option<String>,
        pub detail: Option<String>,
        pub at: String,
    }
}

/// One credential a pack's manifest declares, as the preview gate shows it.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeclaredSecretDto {
    pub name: String,
    pub kind: SecretKind,
    /// How the pack expects the value, when the manifest said: `env` or `file`.
    pub projection_kind: Option<ProjectionKind>,
    /// The environment variable name or file path that projection names.
    pub projection: Option<String>,
}

/// The preview gate's secrets section: what the pack declares, and where that collides with what
/// the deploy profile already supplies.
#[derive(Debug, Serialize, ToSchema)]
pub struct PackSecretsDto {
    pub declared: Vec<DeclaredSecretDto>,
    /// One line per declared name the profile's `[[secret_env]]` also fills. Both would write the
    /// same variable; the binding is what wins.
    pub warnings: Vec<String>,
}

impl PackSecretsDto {
    pub(crate) fn new(
        declared: &[crate::secrets::manifest::DeclaredSecret],
        profile_env: &[String],
    ) -> Self {
        let warnings = crate::secrets::manifest::conflicting_names(declared, profile_env)
            .into_iter()
            .map(|name| {
                format!(
                    "the deploy profile's secret_env already fills {name}; the binding wins once this pack is registered"
                )
            })
            .collect();
        PackSecretsDto {
            declared: declared
                .iter()
                .map(|d| DeclaredSecretDto {
                    name: d.name.to_string(),
                    kind: d.kind,
                    projection_kind: d.projection.as_ref().map(|(kind, _)| *kind),
                    projection: d.projection.as_ref().map(|(_, value)| value.clone()),
                })
                .collect(),
            warnings,
        }
    }
}

/// Register a secret. Deserialize only: the type has no `Serialize` and no `Debug`, so neither the
/// value nor the caller's Vault token can be echoed into a response, a log line, or a span.
#[derive(Deserialize, ToSchema)]
pub struct RegisterSecretBody {
    pub name: String,
    /// `user:self` (the default), `user:<your login>`, or a `group:` in your validated claims.
    #[serde(default)]
    pub owner: Option<String>,
    pub kind: SecretKind,
    /// Absent ⇒ `broker_only`.
    #[serde(default)]
    pub visibility: Option<Visibility>,
    /// Absent ⇒ `run`, except a kubeconfig, which defaults to `hub`.
    #[serde(default)]
    pub consumer: Option<ConsumerClass>,
    /// The bytes, written straight to Vault and dropped with the request. Exactly one of this,
    /// `reference`, and `mint`.
    #[serde(default)]
    pub value: Option<String>,
    /// A `vault://<mount>/<path>#<key>` pointer at a path someone else owns.
    #[serde(default)]
    pub reference: Option<String>,
    /// A minter name (`github-app`): nothing is stored, and the bytes are issued at every
    /// dispatch instead.
    #[serde(default)]
    pub mint: Option<String>,
    /// The registrant's own Vault token, used for one verification read of `reference` and then
    /// dropped. Required in reference mode; never stored.
    #[serde(default)]
    pub vault_token: Option<String>,
}

/// An inference key's bytes are read back as [`crate::secrets::credentials::Credentials`] at every
/// dispatch, so a value that would not parse then is refused now, while someone is watching.
fn check_inference_value(kind: SecretKind, value: Option<&SecretValue>) -> Option<Response> {
    if kind != SecretKind::InferenceApiKey {
        return None;
    }
    let raw = value.map(SecretValue::expose).unwrap_or_default();
    match crate::secrets::credentials::Credentials::parse(raw) {
        Ok(_) => None,
        Err(e) => Some(unprocessable(format!("an inference_api_key value is {e}"))),
    }
}

/// Rotate: the new bytes and nothing else.
#[derive(Deserialize, ToSchema)]
pub struct RotateSecretBody {
    pub value: String,
}

/// Transfer: the principal that takes the secret over.
#[derive(Deserialize, ToSchema)]
pub struct TransferSecretBody {
    /// `user:self`, `user:<your login>`, or a `group:` in your validated claims.
    pub owner: String,
}

/// Bind a secret to a scope.
#[derive(Deserialize, ToSchema)]
pub struct BindSecretBody {
    pub scope_kind: ScopeKind,
    pub scope_id: String,
    /// The manifest-declared name this binding satisfies. Absent ⇒ the secret's own name.
    #[serde(default)]
    pub declared_name: Option<String>,
    pub projection_kind: ProjectionKind,
    /// The environment variable name or the file path the value is written to.
    pub projection: String,
    /// What the pack looked like when the binding was made.
    #[serde(default)]
    pub pack_rev: Option<String>,
    #[serde(default)]
    pub schema_digest: Option<String>,
    /// The binder's own Vault token, for the verification read a reference-mode secret needs on
    /// every bind. Never stored.
    #[serde(default)]
    pub vault_token: Option<String>,
}

/// The registry needs Vault for every write. Without one configured the deployment is not a
/// registry host (local mode, or a cluster with no Vault reach), and says so rather than storing
/// metadata that points at nothing.
#[allow(clippy::result_large_err)]
fn vault(state: &ApiState) -> Result<&VaultClient, Response> {
    state.vault.as_deref().ok_or_else(|| {
        unavailable("this deployment has no Vault client, so the secrets registry is off")
    })
}

/// The secret `id` names, decided for `verb` against its owner. A caller who may not read it is
/// told it does not exist (RFC-0003 C-READ-SCOPING).
#[allow(clippy::result_large_err)]
async fn owned(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    verb: crate::authz::action::Verb,
) -> Result<SecretRow, Response> {
    let row = store::get(state.db.pool(), id)
        .await
        .map_err(|e| AppError::from(e).into_response())?
        .ok_or_else(|| not_found(format!("no secret {id:?}")))?;
    let resource = crate::authz::decision::Resource::new(
        crate::authz::action::ResourceType::Secret,
        id,
        row.owner.clone(),
    );
    crate::authz::owner::decide_on(state, caller, &resource, verb)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(row)
}

/// The verification read a reference performs: one KV read of the pointed-at path with the
/// caller's own Vault token. The token is borrowed here and dropped with the request; so are the
/// bytes the read returns, once they have been asked the agent-visible question.
#[allow(clippy::result_large_err)]
async fn verify_reference(
    client: &VaultClient,
    raw_reference: &str,
    token: Option<&str>,
    visibility: Visibility,
) -> Result<String, Response> {
    let reference = client
        .reference(raw_reference)
        .map_err(|e| unprocessable(e.to_string()))?;
    let token = token
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            unprocessable(
                "a reference is verified with your own Vault token; supply vault_token".to_string(),
            )
        })?;
    let token = VaultToken::new(token);
    let value = client
        .read_reference(&token, &reference)
        .await
        .map_err(|e| forbidden(format!("the reference did not verify: {e}")))?;
    if let Some(value) = value {
        crate::secrets::check_agent_visible_value(visibility, &SecretValue::new(value))
            .map_err(|e| unprocessable(e.to_string()))?;
    }
    Ok(reference.to_string())
}

/// Prove a hub-consumed kubeconfig can actually run work before it is registered as a dispatch
/// target. `Some` is the refusal to return verbatim; `None` is anything that is not a cluster
/// credential, which needs no probe.
async fn probe_dispatch_target(
    client: &crate::secrets::vault::VaultClient,
    kind: crate::secrets::SecretKind,
    consumer: ConsumerClass,
    mode: SecretMode,
    value: Option<&SecretValue>,
    vault_path: &str,
) -> Option<Response> {
    if kind != crate::secrets::SecretKind::Kubeconfig || consumer != ConsumerClass::Hub {
        return None;
    }
    let yaml = match (mode, value) {
        (SecretMode::Managed, Some(v)) => v.expose().to_string(),
        // A reference's bytes belong to someone else's path; read them the way a dispatch will,
        // with the hub's own identity, so the probe tests what will actually be used.
        _ => {
            let reference = match client.reference(vault_path) {
                Ok(reference) => reference,
                Err(e) => return Some(unprocessable(e.to_string())),
            };
            match client.read_reference_as_hub(&reference).await {
                Ok(read) => read
                    .data
                    .get(reference.key())
                    .unwrap_or_default()
                    .to_string(),
                Err(e) => {
                    return Some(unavailable(format!(
                        "the referenced kubeconfig is unreadable: {e}"
                    )));
                }
            }
        }
    };
    crate::runs::dispatch_target::probe(&yaml)
        .await
        .err()
        .map(|e| {
            unprocessable(format!(
                "this kubeconfig cannot serve as a dispatch target: {e}"
            ))
        })
}

/// `POST /api/secrets` — register a secret by value, by reference, or by mint.
///
/// By value, the hub is the writer: the bytes go to `crucible/<owner>/<name>` and the row records
/// the version Vault assigned. By reference, nothing is written: the caller's own Vault token
/// performs one verification read of the pointed-at path, and the row stores the pointer. By mint,
/// nothing is stored and nothing is verified: the row names the minter, and every dispatch that
/// binds it gets a credential issued then, with that minter's own lifetime.
/// Registration is synchronous — a Vault that cannot be written fails the request rather than
/// queueing anything.
#[utoipa::path(
    post,
    path = "/api/secrets",
    request_body = RegisterSecretBody,
    responses(
        (status = 201, description = "The secret's metadata", body = SecretDto),
        (status = 403, description = "Not an operator, or the owner is not in the caller's claims", body = ErrorBody),
        (status = 409, description = "The owner already has a secret with that name", body = ErrorBody),
        (status = 422, description = "The name, kind, visibility, or reference was refused", body = ErrorBody),
        (status = 503, description = "No Vault client, or Vault could not be written", body = ErrorBody)
    )
)]
pub(crate) async fn register_secret(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Json(body): Json<RegisterSecretBody>,
) -> Response {
    let owner = match crate::authz::owner::owner_for_create(
        &state,
        &caller,
        body.owner.as_deref(),
        crate::authz::action::ResourceType::Secret,
    )
    .await
    {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    let name = match SecretName::parse(&body.name) {
        Ok(name) => name,
        Err(e) => return unprocessable(e.to_string()),
    };
    let kind = body.kind;
    let visibility = body
        .visibility
        .unwrap_or(crate::secrets::Visibility::BrokerOnly);
    let consumer = body.consumer.unwrap_or_else(|| kind.default_consumer());
    if let Err(e) = crate::secrets::check_shape(kind, visibility, consumer) {
        return unprocessable(e.to_string());
    }

    let value = body.value.map(SecretValue::new);
    let named = [
        value.is_some(),
        body.reference.is_some(),
        body.mint.is_some(),
    ]
    .into_iter()
    .filter(|named| *named)
    .count();
    if named != 1 {
        return unprocessable(
            "register a secret with a value, with a reference, or with a mint — exactly one",
        );
    }
    if let Some(refusal) = check_inference_value(kind, value.as_ref()) {
        return refusal;
    }
    let mint = match body.mint.as_deref() {
        Some(raw) => match crate::secrets::minter::Minter::parse_name(raw) {
            Ok(minter) => Some(minter),
            Err(e) => return unprocessable(e.to_string()),
        },
        None => None,
    };
    // A minted secret stores no bytes, so its registration needs no Vault client.
    let (mode, vault_path, client) = match mint {
        Some(minter) => {
            if let Err(e) = crate::secrets::check_mint(kind, consumer) {
                return unprocessable(e.to_string());
            }
            (SecretMode::Minted, minter.uri(), None)
        }
        None => {
            let client = match vault(&state) {
                Ok(client) => client,
                Err(refusal) => return refusal,
            };
            let (mode, path) = if let Some(reference) = body.reference.as_deref() {
                match verify_reference(client, reference, body.vault_token.as_deref(), visibility)
                    .await
                {
                    Ok(pointer) => (SecretMode::Reference, pointer),
                    Err(refusal) => return refusal,
                }
            } else {
                // The exclusivity check above leaves exactly one, so this is the value branch.
                let Some(value) = value.as_ref().filter(|v| !v.is_empty()) else {
                    return unprocessable("a registered value cannot be empty");
                };
                if let Err(e) = crate::secrets::check_agent_visible_value(visibility, value) {
                    return unprocessable(e.to_string());
                }
                match crate::secrets::managed_path(&owner, &name) {
                    Ok(path) => (SecretMode::Managed, path.as_str().to_string()),
                    Err(e) => return unprocessable(e.to_string()),
                }
            };
            (mode, path, Some(client))
        }
    };

    if let Some(client) = client
        && let Some(refusal) =
            probe_dispatch_target(client, kind, consumer, mode, value.as_ref(), &vault_path).await
    {
        return refusal;
    }

    let id = uuid::Uuid::now_v7().to_string();
    let mut tx = match state.db.pool().begin().await {
        Ok(tx) => tx,
        Err(e) => return AppError::from(e).into_response(),
    };
    let stored = store::insert(
        &mut tx,
        &store::NewSecret {
            id: &id,
            name: &name,
            owner: &owner,
            kind,
            visibility,
            consumer,
            mode,
            vault_path: &vault_path,
            current_version: None,
            created_by: caller.principals.login(),
        },
    )
    .await;
    let mut stored = match stored {
        Ok(row) => row,
        Err(e @ store::StoreError::Duplicate { .. }) => return conflict(e.to_string()),
        Err(store::StoreError::Internal(e)) => return AppError::from(e).into_response(),
    };

    // The Vault write rides inside the transaction: a write that fails rolls the row back, so a
    // registration never survives as metadata pointing at bytes that were never stored.
    if let (SecretMode::Managed, Some(value), Some(client)) = (mode, value.as_ref(), client) {
        let path = match crate::secrets::vault::VaultPath::parse(&vault_path) {
            Ok(path) => path,
            Err(e) => return unprocessable(e.to_string()),
        };
        let data = KvData::new().with(crate::secrets::VALUE_KEY, value.expose());
        let version = match client.put(&path, &data).await.map(i64::try_from) {
            Ok(Ok(version)) => version,
            Ok(Err(e)) => {
                return AppError::from(anyhow::anyhow!(
                    "Vault returned a version that does not fit the version column: {e}"
                ))
                .into_response();
            }
            Err(e) => return unavailable(format!("the value was not written to Vault: {e}")),
        };
        if let Err(e) = store::set_version(&mut tx, &id, version).await {
            return AppError::from(e).into_response();
        }
        stored.current_version = Some(version);
    }

    if let Err(e) = store::audit(
        &mut tx,
        &store::NewAudit {
            secret_id: Some(&id),
            secret_name: name.as_str(),
            owner: &owner,
            action: AuditAction::Register,
            actor: caller.principals.user(),
            detail: Some(&format!("{} {}", mode.as_str(), kind.as_str())),
        },
    )
    .await
    {
        return AppError::from(e).into_response();
    }
    if let Err(e) = tx.commit().await {
        return AppError::from(e).into_response();
    }
    (StatusCode::CREATED, Json(SecretDto::from(stored))).into_response()
}

/// `GET /api/secrets` — the metadata of every secret the caller owns or is a member of the owner
/// of. `?all=true` widens it to the whole registry for an admin, which is still metadata: no route
/// on this surface returns a value.
#[utoipa::path(
    get,
    path = "/api/secrets",
    params(("all" = Option<bool>, Query, description = "Admins only: every secret, not just the caller's")),
    responses(
        (status = 200, description = "Secret metadata", body = Vec<SecretDto>),
        (status = 403, description = "Not an operator", body = ErrorBody)
    )
)]
pub(crate) async fn list_secrets(
    State(state): State<ApiState>,
    Query(q): Query<ListSecretsQuery>,
    caller: crate::authz::Caller,
) -> Result<Json<Vec<SecretDto>>, AppError> {
    let owners = if q.all.unwrap_or(false) && caller.principals.is_platform_admin() {
        None
    } else {
        Some(caller.principals.all())
    };
    let rows = store::list(state.db.pool(), owners.as_deref()).await?;
    Ok(Json(rows.into_iter().map(SecretDto::from).collect()))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListSecretsQuery {
    all: Option<bool>,
}

/// `GET /api/secrets/{id}` — one secret's metadata and its bindings.
#[utoipa::path(
    get,
    path = "/api/secrets/{id}",
    params(("id" = String, Path, description = "Secret id")),
    responses(
        (status = 200, description = "The secret and its bindings", body = SecretDetailDto),
        (status = 403, description = "The secret is owned by a principal the caller does not hold", body = ErrorBody),
        (status = 404, description = "No secret with that id", body = ErrorBody)
    )
)]
pub(crate) async fn get_secret(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    caller: crate::authz::Caller,
) -> Response {
    let row = match owned(&state, &caller, &id, crate::authz::action::Verb::Read).await {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let bindings = match store::bindings_for_secret(state.db.pool(), &id).await {
        Ok(bindings) => bindings,
        Err(e) => return AppError::from(e).into_response(),
    };
    Json(SecretDetailDto {
        secret: SecretDto::from(row),
        bindings: bindings.into_iter().map(SecretBindingDto::from).collect(),
    })
    .into_response()
}

/// `POST /api/secrets/{id}/rotate` — write a new KV version. The row's current version advances,
/// so the next pod start resolves the new bytes; a pod already running keeps what it holds.
#[utoipa::path(
    post,
    path = "/api/secrets/{id}/rotate",
    params(("id" = String, Path, description = "Secret id")),
    request_body = RotateSecretBody,
    responses(
        (status = 200, description = "The secret's metadata, at its new version", body = SecretDto),
        (status = 403, description = "The secret is owned by a principal the caller does not hold", body = ErrorBody),
        (status = 404, description = "No secret with that id", body = ErrorBody),
        (status = 422, description = "A reference has no version to rotate", body = ErrorBody),
        (status = 503, description = "No Vault client, or Vault could not be written", body = ErrorBody)
    )
)]
pub(crate) async fn rotate_secret(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    caller: crate::authz::Caller,
    Json(body): Json<RotateSecretBody>,
) -> Response {
    let mut row = match owned(&state, &caller, &id, crate::authz::action::Verb::Rotate).await {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    match row.mode {
        SecretMode::Reference => {
            return unprocessable(crate::secrets::PolicyError::RotateReference.to_string());
        }
        SecretMode::Minted => {
            return unprocessable(crate::secrets::PolicyError::RotateMinted.to_string());
        }
        SecretMode::Managed => {}
    }
    let client = match vault(&state) {
        Ok(client) => client,
        Err(refusal) => return refusal,
    };
    let value = SecretValue::new(body.value);
    if value.is_empty() {
        return unprocessable("a rotated value cannot be empty");
    }
    if let Some(refusal) = check_inference_value(row.kind, Some(&value)) {
        return refusal;
    }
    if let Err(e) = crate::secrets::check_agent_visible_value(row.visibility, &value) {
        return unprocessable(e.to_string());
    }
    let path = match crate::secrets::managed_path(&row.owner, &row.name) {
        Ok(path) => path,
        Err(e) => return unprocessable(e.to_string()),
    };
    let data = KvData::new().with(crate::secrets::VALUE_KEY, value.expose());
    let version = match client.put(&path, &data).await.map(i64::try_from) {
        Ok(Ok(version)) => version,
        Ok(Err(e)) => {
            return AppError::from(anyhow::anyhow!(
                "Vault returned a version that does not fit the version column: {e}"
            ))
            .into_response();
        }
        Err(e) => return unavailable(format!("the value was not written to Vault: {e}")),
    };

    let mut tx = match state.db.pool().begin().await {
        Ok(tx) => tx,
        Err(e) => return AppError::from(e).into_response(),
    };
    if let Err(e) = store::set_version(&mut tx, &id, version).await {
        return AppError::from(e).into_response();
    }
    if let Err(e) = store::audit(
        &mut tx,
        &store::NewAudit {
            secret_id: Some(&id),
            secret_name: row.name.as_str(),
            owner: &row.owner,
            action: AuditAction::Rotate,
            actor: caller.principals.user(),
            detail: Some(&format!("version {version}")),
        },
    )
    .await
    {
        return AppError::from(e).into_response();
    }
    if let Err(e) = tx.commit().await {
        return AppError::from(e).into_response();
    }
    // A cached client built on the old bytes would keep working; drop it so the next dispatch
    // through this target picks the rotation up.
    evict_target(&state, &id).await;
    row.current_version = Some(version);
    Json(SecretDto::from(row)).into_response()
}

/// `PUT /api/secrets/{id}/owner` — hand the secret to another principal the caller also holds.
///
/// A managed secret's bytes live under its owner's path, so the current version is copied to the
/// new owner's path and the old path is destroyed once the row has moved; earlier versions stay
/// behind and go with the old path. A reference or a minted secret only changes owner. Bindings
/// and grants name the secret by id and follow it; a model provider names it by owner and name and
/// is re-pointed.
#[utoipa::path(
    put,
    path = "/api/secrets/{id}/owner",
    params(("id" = String, Path, description = "Secret id")),
    request_body = TransferSecretBody,
    responses(
        (status = 200, description = "The secret's metadata under its new owner", body = SecretDto),
        (status = 403, description = "The caller holds neither the current owner nor the new one", body = ErrorBody),
        (status = 404, description = "No secret with that id", body = ErrorBody),
        (status = 409, description = "The new owner already has a secret with that name", body = ErrorBody),
        (status = 422, description = "The owner is malformed, or is already the owner", body = ErrorBody),
        (status = 503, description = "No Vault client, or Vault could not be read or written", body = ErrorBody)
    )
)]
pub(crate) async fn transfer_secret(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    caller: crate::authz::Caller,
    Json(body): Json<TransferSecretBody>,
) -> Response {
    let row = match owned(&state, &caller, &id, crate::authz::action::Verb::Transfer).await {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let to = match body.owner.trim() {
        "self" | "user:self" => match caller.principals.user() {
            Some(user) => user.clone(),
            None => return forbidden("an anonymous caller has no principal to transfer to"),
        },
        raw => match Principal::parse(raw) {
            Ok(to) if to.may_own() => to,
            Ok(to) => return unprocessable(format!("{to} may not own a secret")),
            Err(e) => return unprocessable(e.to_string()),
        },
    };
    if !caller.principals.covers(&to) && !caller.principals.is_platform_admin() {
        return forbidden(format!("{to} is not a principal you act as"));
    }
    if to == row.owner {
        return unprocessable(format!("{} is already owned by {to}", row.name));
    }

    let moved = match row.mode {
        SecretMode::Managed => {
            let client = match vault(&state) {
                Ok(client) => client,
                Err(refusal) => return refusal,
            };
            let from_path = match crate::secrets::managed_path(&row.owner, &row.name) {
                Ok(path) => path,
                Err(e) => return unprocessable(e.to_string()),
            };
            let to_path = match crate::secrets::managed_path(&to, &row.name) {
                Ok(path) => path,
                Err(e) => return unprocessable(e.to_string()),
            };
            let current = match client.get(&from_path, None).await {
                Ok(current) => current,
                Err(e) => return unavailable(format!("the value could not be read: {e}")),
            };
            let version = match client.put(&to_path, &current.data).await.map(i64::try_from) {
                Ok(Ok(version)) => version,
                Ok(Err(e)) => {
                    return AppError::from(anyhow::anyhow!(
                        "Vault returned a version that does not fit the version column: {e}"
                    ))
                    .into_response();
                }
                Err(e) => return unavailable(format!("the value was not written to Vault: {e}")),
            };
            Some((client, from_path, to_path, version))
        }
        SecretMode::Reference | SecretMode::Minted => None,
    };
    let (vault_path, version) = match &moved {
        Some((_, _, to_path, version)) => (to_path.as_str().to_string(), Some(*version)),
        None => (row.vault_path.clone(), row.current_version),
    };

    let mut tx = match state.db.pool().begin().await {
        Ok(tx) => tx,
        Err(e) => return AppError::from(e).into_response(),
    };
    let stored = store::transfer(
        &mut tx,
        &id,
        &row.owner,
        &row.name,
        &to,
        &vault_path,
        version,
    )
    .await;
    let stored = match stored {
        Ok(stored) => stored,
        Err(e @ store::StoreError::Duplicate { .. }) => return conflict(e.to_string()),
        Err(store::StoreError::Internal(e)) => return AppError::from(e).into_response(),
    };
    if let Err(e) = store::audit(
        &mut tx,
        &store::NewAudit {
            secret_id: Some(&id),
            secret_name: row.name.as_str(),
            owner: &to,
            action: AuditAction::Transfer,
            actor: caller.principals.user(),
            detail: Some(&format!("from {}", row.owner)),
        },
    )
    .await
    {
        return AppError::from(e).into_response();
    }
    if let Err(e) = tx.commit().await {
        return AppError::from(e).into_response();
    }
    if let Some((client, from_path, _, _)) = moved
        && let Err(e) = client.delete_all(&from_path).await
    {
        tracing::warn!(secret = %id, path = %from_path, error = %e, "the old copy of a transferred secret was not removed from Vault");
    }
    Json(SecretDto::from(stored)).into_response()
}

/// `DELETE /api/secrets/{id}` — remove the metadata and destroy the managed bytes. Refused while
/// anything is bound to it: a delete that silently broke a scope's launches would be discovered at
/// the next dispatch, not here.
#[utoipa::path(
    delete,
    path = "/api/secrets/{id}",
    params(("id" = String, Path, description = "Secret id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "The secret is owned by a principal the caller does not hold", body = ErrorBody),
        (status = 404, description = "No secret with that id", body = ErrorBody),
        (status = 409, description = "The secret still has bindings", body = ErrorBody),
        (status = 503, description = "No Vault client, or Vault could not be reached", body = ErrorBody)
    )
)]
pub(crate) async fn delete_secret(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    caller: crate::authz::Caller,
) -> Response {
    let row = match owned(&state, &caller, &id, crate::authz::action::Verb::Delete).await {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let bindings = match store::bindings_for_secret(state.db.pool(), &id).await {
        Ok(bindings) => bindings,
        Err(e) => return AppError::from(e).into_response(),
    };
    if !bindings.is_empty() {
        let scopes = bindings
            .iter()
            .map(|b| format!("{} {}", b.scope_kind.as_str(), b.scope_id))
            .collect::<Vec<_>>()
            .join(", ");
        return conflict(format!(
            "{} is still bound to {scopes}; unbind it first",
            row.name
        ));
    }
    let mut tx = match state.db.pool().begin().await {
        Ok(tx) => tx,
        Err(e) => return AppError::from(e).into_response(),
    };
    if let Err(e) = store::delete(&mut tx, &id).await {
        return AppError::from(e).into_response();
    }
    if let Err(e) = store::audit(
        &mut tx,
        &store::NewAudit {
            secret_id: Some(&id),
            secret_name: row.name.as_str(),
            owner: &row.owner,
            action: AuditAction::Delete,
            actor: caller.principals.user(),
            detail: Some(row.mode.as_str()),
        },
    )
    .await
    {
        return AppError::from(e).into_response();
    }
    // Managed bytes are the registry's to destroy; a reference's are not, and a minted has none.
    if row.mode == SecretMode::Managed {
        let client = match vault(&state) {
            Ok(client) => client,
            Err(refusal) => return refusal,
        };
        let path = match crate::secrets::managed_path(&row.owner, &row.name) {
            Ok(path) => path,
            Err(e) => return unprocessable(e.to_string()),
        };
        if let Err(e) = client.delete_all(&path).await {
            return unavailable(format!("the value was not removed from Vault: {e}"));
        }
    }
    if let Err(e) = tx.commit().await {
        return AppError::from(e).into_response();
    }
    // The registry row was the target's only existence; drop the client it backed so a run cannot
    // be dispatched through credentials that are no longer registered.
    evict_target(&state, &id).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Forget any cached kube client for the dispatch target this secret backs. A no-op for a secret
/// that never was one — the name simply is not in the cache.
async fn evict_target(state: &ApiState, secret_id: &str) {
    state
        .clusters
        .evict(&crate::runs::clusters::personal_name(secret_id))
        .await;
}

/// `POST /api/secrets/{id}/bindings` — attach a secret to a repo, playbook, or domain.
///
/// A reference-mode secret is re-verified here with the binder's own Vault token: binding is the
/// act that exposes the path to a run, so the person doing it proves they can read it too.
#[utoipa::path(
    post,
    path = "/api/secrets/{id}/bindings",
    params(("id" = String, Path, description = "Secret id")),
    request_body = BindSecretBody,
    responses(
        (status = 201, description = "The binding", body = SecretBindingDto),
        (status = 403, description = "Not an owner member, or the reference did not verify", body = ErrorBody),
        (status = 404, description = "No secret with that id", body = ErrorBody),
        (status = 409, description = "That scope already binds this declared name or projection", body = ErrorBody),
        (status = 422, description = "The projection is not one this kind can take", body = ErrorBody),
        (status = 503, description = "No Vault client", body = ErrorBody)
    )
)]
pub(crate) async fn bind_secret(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    caller: crate::authz::Caller,
    Json(body): Json<BindSecretBody>,
) -> Response {
    let row = match owned(&state, &caller, &id, crate::authz::action::Verb::Bind).await {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    if let Err(e) = crate::secrets::check_projection(row.kind, body.projection_kind) {
        return unprocessable(e.to_string());
    }
    let declared = match body.declared_name.as_deref() {
        Some(raw) => match SecretName::parse(raw) {
            Ok(name) => name,
            Err(e) => return unprocessable(e.to_string()),
        },
        None => row.name.clone(),
    };
    let scope_id = body.scope_id.trim();
    let projection = body.projection.trim();
    if scope_id.is_empty() || projection.is_empty() {
        return unprocessable("a binding needs a scope id and a projection");
    }
    if row.mode == SecretMode::Reference {
        let client = match vault(&state) {
            Ok(client) => client,
            Err(refusal) => return refusal,
        };
        if let Err(refusal) = verify_reference(
            client,
            &row.vault_path,
            body.vault_token.as_deref(),
            row.visibility,
        )
        .await
        {
            return refusal;
        }
    }
    // The value stored now, not whatever the registration saw. A reference was asked the same
    // question by its re-verification read above; a minted secret stores no bytes.
    if row.visibility == Visibility::AgentVisible && row.mode == SecretMode::Managed {
        let client = match vault(&state) {
            Ok(client) => client,
            Err(refusal) => return refusal,
        };
        let value = match crate::secrets::read::current_value(client, &row).await {
            Ok((value, _)) => SecretValue::new(value),
            Err(e) => return unavailable(format!("the bound value could not be checked: {e}")),
        };
        if let Err(e) = crate::secrets::check_agent_visible_value(row.visibility, &value) {
            return unprocessable(e.to_string());
        }
    }

    let binding_id = uuid::Uuid::now_v7().to_string();
    let mut tx = match state.db.pool().begin().await {
        Ok(tx) => tx,
        Err(e) => return AppError::from(e).into_response(),
    };
    let stored = store::insert_binding(
        &mut tx,
        &store::NewBinding {
            id: &binding_id,
            secret_id: &id,
            scope_kind: body.scope_kind,
            scope_id,
            projection_kind: body.projection_kind,
            projection,
            declared_name: &declared,
            pack_rev: body.pack_rev.as_deref(),
            schema_digest: body.schema_digest.as_deref(),
            created_by: caller.principals.login(),
        },
    )
    .await;
    let stored = match stored {
        Ok(stored) => stored,
        Err(e @ store::StoreError::Duplicate { .. }) => return conflict(e.to_string()),
        Err(store::StoreError::Internal(e)) => return AppError::from(e).into_response(),
    };
    if let Err(e) = store::audit(
        &mut tx,
        &store::NewAudit {
            secret_id: Some(&id),
            secret_name: row.name.as_str(),
            owner: &row.owner,
            action: AuditAction::Bind,
            actor: caller.principals.user(),
            detail: Some(&format!(
                "{} {} as {} {}",
                stored.scope_kind.as_str(),
                stored.scope_id,
                stored.projection_kind.as_str(),
                stored.projection
            )),
        },
    )
    .await
    {
        return AppError::from(e).into_response();
    }
    if let Err(e) = tx.commit().await {
        return AppError::from(e).into_response();
    }
    (StatusCode::CREATED, Json(SecretBindingDto::from(stored))).into_response()
}

/// `DELETE /api/secrets/{id}/bindings/{binding_id}` — detach a secret from a scope.
#[utoipa::path(
    delete,
    path = "/api/secrets/{id}/bindings/{binding_id}",
    params(
        ("id" = String, Path, description = "Secret id"),
        ("binding_id" = String, Path, description = "Binding id")
    ),
    responses(
        (status = 204, description = "Unbound"),
        (status = 403, description = "Not an owner member", body = ErrorBody),
        (status = 404, description = "No such secret or binding", body = ErrorBody)
    )
)]
pub(crate) async fn unbind_secret(
    State(state): State<ApiState>,
    Path((id, binding_id)): Path<(String, String)>,
    caller: crate::authz::Caller,
) -> Response {
    let row = match owned(&state, &caller, &id, crate::authz::action::Verb::Bind).await {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let binding = match store::get_binding(state.db.pool(), &binding_id).await {
        Ok(Some(binding)) if binding.secret_id == id => binding,
        Ok(_) => return not_found(format!("no binding {binding_id:?} on secret {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    };

    let mut tx = match state.db.pool().begin().await {
        Ok(tx) => tx,
        Err(e) => return AppError::from(e).into_response(),
    };
    match store::delete_binding(&mut tx, &binding_id).await {
        Ok(true) => {}
        Ok(false) => return not_found(format!("no binding {binding_id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    }
    if let Err(e) = store::audit(
        &mut tx,
        &store::NewAudit {
            secret_id: Some(&id),
            secret_name: row.name.as_str(),
            owner: &row.owner,
            action: AuditAction::Unbind,
            actor: caller.principals.user(),
            detail: Some(&format!(
                "{} {}",
                binding.scope_kind.as_str(),
                binding.scope_id
            )),
        },
    )
    .await
    {
        return AppError::from(e).into_response();
    }
    if let Err(e) = tx.commit().await {
        return AppError::from(e).into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /api/secrets/{id}/audit` — every recorded action on one secret, newest first.
#[utoipa::path(
    get,
    path = "/api/secrets/{id}/audit",
    params(("id" = String, Path, description = "Secret id")),
    responses(
        (status = 200, description = "The audit trail", body = Vec<SecretAuditDto>),
        (status = 403, description = "The secret is owned by a principal the caller does not hold", body = ErrorBody),
        (status = 404, description = "No secret with that id", body = ErrorBody)
    )
)]
pub(crate) async fn list_secret_audit(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    caller: crate::authz::Caller,
) -> Response {
    if let Err(refusal) = owned(&state, &caller, &id, crate::authz::action::Verb::Read).await {
        return refusal;
    }
    match store::audit_for_secret(state.db.pool(), &id, AUDIT_LIMIT).await {
        Ok(rows) => Json(
            rows.into_iter()
                .map(SecretAuditDto::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

#[cfg(test)]
mod tests;
