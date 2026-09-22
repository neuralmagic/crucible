#![allow(clippy::disallowed_macros)]

use crate::api::dto::*;
use crate::dto::dto;

use crate::api::state::*;

use crate::playbooks::providers::{
    DefaultScope, DispatchDefault, Endpoint, InferenceProtocol, ModelProvider, ProviderKind,
    ProviderSecretRef, WorkloadClass,
};

use axum::extract::{Path, Query, State};

use axum::http::StatusCode;

use axum::response::{IntoResponse, Response};

use serde::{Deserialize, Serialize};

use utoipa::ToSchema;

dto! {
    // --- what a launch picker reads (any authenticated caller) --------------------

    /// One enabled provider, as a picker needs it: what to call it, which service it is, and which
    /// models it offers. The credential reference is not here — only an administrator has business
    /// knowing which secret pays for a provider.
    pub struct ProviderDto: From<p: ModelProvider> {
        /// The agent CLI a dispatch to this provider runs: claude, hermes, codex, opencode, or pi.
        pub harness: String = crate::playbooks::providers::harness_name(p.harness()).to_string(),
        pub id: String,
        pub display_name: String,
        pub kind: ProviderKind,
        pub models: Vec<String>,
        pub default_model: String,
        /// Where a custom provider is reached; null for a kind reached at its service's own address.
        pub endpoint: Option<String> = p.endpoint.as_ref().map(|e| e.url.clone()),
        /// The API a custom provider speaks; null for the other kinds, whose kind says.
        pub protocol: Option<InferenceProtocol> = p.endpoint.as_ref().map(|e| e.protocol),
        /// `user:<login>` or `team:<slug>`.
        pub owner: String = p.owner.to_string(),
    }
}

dto! {
    /// Where a scope's dispatches of one workload class go when the launch names nothing.
    pub struct DispatchDefaultDto: From<d: DispatchDefault> {
        pub scope_kind: DefaultScope,
        /// The domain name, empty for the platform-wide row.
        pub scope_ref: String,
        pub workload_class: WorkloadClass,
        pub provider: String = d.provider_id,
        /// Null takes the provider's own default model.
        pub model: Option<String>,
    }
}

/// What the launch forms render their provider/model pickers from: the providers a launch may
/// choose, and the defaults it would inherit by choosing nothing. Both empty is the zero-config
/// deployment, where the forms show no picker at all.
#[derive(Debug, Serialize, ToSchema)]
pub struct DispatchProvidersDto {
    pub providers: Vec<ProviderDto>,
    pub defaults: Vec<DispatchDefaultDto>,
}

/// The subset of `rows` that `caller` may read.
async fn readable_providers(
    state: &ApiState,
    caller: &crate::authz::Caller,
    rows: Vec<ModelProvider>,
) -> anyhow::Result<Vec<ModelProvider>> {
    crate::authz::owner::readable(
        state,
        caller,
        crate::authz::action::ResourceType::ModelProvider,
        rows,
        |r| {
            crate::authz::decision::Resource::new(
                crate::authz::action::ResourceType::ModelProvider,
                &r.id,
                r.owner.clone(),
            )
        },
    )
    .await
}

/// The enabled providers `caller` may read: what a launch form offers and what a launch may pin.
async fn launchable(
    state: &ApiState,
    caller: &crate::authz::Caller,
) -> anyhow::Result<Vec<ModelProvider>> {
    let enabled = crate::playbooks::providers::list(state.db.pool(), true).await?;
    readable_providers(state, caller, enabled).await
}

#[utoipa::path(
    get,
    path = "/api/config/providers",
    responses(
        (status = 200, description = "The enabled providers a launch may pick, and the defaults it inherits", body = DispatchProvidersDto)
    )
)]
pub(crate) async fn get_dispatch_providers(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
) -> Response {
    let providers = match launchable(&state, &caller).await {
        Ok(rows) => rows,
        Err(e) => return AppError::from(e).into_response(),
    };
    let defaults = match crate::playbooks::providers::list_defaults(state.db.pool()).await {
        Ok(rows) => rows,
        Err(e) => return AppError::from(e).into_response(),
    };
    Json(DispatchProvidersDto {
        providers: providers.into_iter().map(ProviderDto::from).collect(),
        defaults: defaults.into_iter().map(DispatchDefaultDto::from).collect(),
    })
    .into_response()
}

dto! {
    // --- the registry itself (admin) ---------------------------------------------

    /// A registration in full, including the credential it spends and whether it is offered at launch.
    pub struct ProviderDetailDto: From<p: ModelProvider> {
        /// The agent CLI a dispatch to this provider runs: claude, hermes, codex, opencode, or pi.
        pub harness: String = crate::playbooks::providers::harness_name(p.harness()).to_string(),
        /// The harness the registration named; null runs the default for the kind or protocol.
        pub harness_override: Option<String> = p.harness.map(|h| crate::playbooks::providers::harness_name(h).to_string()),
        pub id: String,
        pub display_name: String,
        pub kind: ProviderKind,
        pub models: Vec<String>,
        pub default_model: String,
        /// The secrets-registry entry the key lives under; null means the deploy profile's ambient
        /// credentials.
        pub secret_name: Option<String> = p.secret.as_ref().map(|s| s.name.clone()),
        /// The principal whose keyspace holds `secret_name`. Null exactly when `secret_name` is.
        pub secret_owner: Option<String> = p.secret.as_ref().map(|s| s.owner.to_string()),
        /// Where a custom provider is reached; null for a kind reached at its service's own address.
        pub endpoint: Option<String> = p.endpoint.as_ref().map(|e| e.url.clone()),
        /// The API a custom provider speaks; null for the other kinds.
        pub protocol: Option<InferenceProtocol> = p.endpoint.as_ref().map(|e| e.protocol),
        pub enabled: bool,
        pub owner: String = p.owner.to_string(),
        pub created_by: String,
        pub created_at: String,
        pub updated_at: String,
    }
}

/// Everything about a registration except its id, which a POST carries in the body and a PUT in
/// the path.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ProviderBody {
    display_name: String,
    kind: ProviderKind,
    /// The curated list offered at launch. Empty takes the kind's own list; a launch may still
    /// name a model outside it.
    #[serde(default)]
    models: Vec<String>,
    /// Absent takes the kind's default model.
    #[serde(default)]
    default_model: Option<String>,
    /// A secrets-registry name of kind `inference_api_key`. Absent runs on the deploy profile's
    /// ambient credentials, which is what every Vertex provider does.
    #[serde(default)]
    secret_name: Option<String>,
    /// The principal (`user:<login>` / `group:<path>`) whose keyspace holds `secret_name`. Absent
    /// is fine while exactly one owner has registered that name; the stored reference always
    /// carries the owner, so a later same-named secret elsewhere cannot move or shadow it.
    #[serde(default)]
    secret_owner: Option<String>,
    /// Where a custom provider is reached: an absolute http(s) URL, the base the harness's API
    /// paths are appended to. Required for the custom kind, refused for the others.
    #[serde(default)]
    endpoint: Option<String>,
    /// The API a custom provider speaks at that endpoint. Required with `endpoint`.
    #[serde(default)]
    protocol: Option<InferenceProtocol>,
    /// The agent CLI to run the models under (claude, hermes, codex, opencode, or pi), one of
    /// those that speak the kind's service or the custom endpoint's protocol. Absent runs the
    /// default: Claude Code for anthropic, vertex, and messages; Codex for openai and responses;
    /// opencode for chat_completions.
    #[serde(default)]
    harness: Option<String>,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct RegisterProviderBody {
    /// The principal to own it: `user:<login>` or `team:<slug>` the caller acts as; absent means
    /// the caller.
    owner: Option<String>,
    /// The slug every pinned column and API path refers to: lowercase letters, digits, and dashes.
    id: String,
    #[serde(flatten)]
    provider: ProviderBody,
}

/// Upper bound on a provider slug. It rides an issue column and an API path; a longer one is a
/// caller mistake, not an identifier.
const PROVIDER_ID_MAX_LEN: usize = 64;

/// Validate a provider slug. Narrow on purpose: the id is a path segment and a stored pin, so it
/// stays in the vocabulary a URL and a form option can carry unescaped.
fn require_provider_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty() {
        return Err("id must be non-empty".to_string());
    }
    if id.len() > PROVIDER_ID_MAX_LEN {
        return Err(format!(
            "id must be at most {PROVIDER_ID_MAX_LEN} characters"
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("id may only contain lowercase letters, digits, and '-'".to_string());
    }
    Ok(id.to_string())
}

/// What a registration's fields become once checked: the strings the store binds.
struct CheckedProvider {
    display_name: String,
    kind: ProviderKind,
    models: Vec<String>,
    default_model: Option<String>,
    secret: Option<ProviderSecretRef>,
    endpoint: Option<Endpoint>,
    harness: Option<crucible::manifest::Harness>,
    enabled: bool,
}

/// Check the harness a registration names against what its kind or protocol can run. Absent is
/// the default; an unknown name or one the service cannot be reached through is refused with the
/// list that would be accepted.
fn check_harness(
    kind: ProviderKind,
    protocol: Option<InferenceProtocol>,
    raw: Option<&str>,
) -> Result<Option<crucible::manifest::Harness>, String> {
    let Some(raw) = raw.map(str::trim).filter(|h| !h.is_empty()) else {
        return Ok(None);
    };
    let allowed = crate::playbooks::providers::allowed_harnesses(kind, protocol);
    match crate::playbooks::providers::parse_harness(raw) {
        Some(harness) if allowed.contains(&harness) => Ok(Some(harness)),
        Some(_) => Err(format!(
            "harness: a {} provider{} runs under one of {}, not {raw}",
            kind.as_str(),
            protocol.map_or(String::new(), |p| format!(" speaking {}", p.as_str())),
            crate::playbooks::providers::harness_list(allowed)
        )),
        None => Err(format!(
            "harness: {raw:?} is not a harness (claude, hermes, codex, opencode, or pi)"
        )),
    }
}

/// Check the endpoint half of a registration against its kind: a custom provider needs both the
/// address and the API, and any other kind is reached at its service's own address.
fn check_endpoint(
    kind: ProviderKind,
    endpoint: Option<&str>,
    protocol: Option<InferenceProtocol>,
) -> Result<Option<Endpoint>, String> {
    let endpoint = endpoint.map(str::trim).filter(|e| !e.is_empty());
    match (kind, endpoint, protocol) {
        (ProviderKind::Custom, Some(url), Some(protocol)) => Ok(Some(Endpoint {
            url: crate::playbooks::providers::check_endpoint(url)
                .map_err(|msg| format!("endpoint: {msg}"))?,
            protocol,
        })),
        (ProviderKind::Custom, None, _) => {
            Err("a custom provider needs an endpoint, the base URL it is reached at".to_string())
        }
        (ProviderKind::Custom, Some(_), None) => Err(
            "a custom provider needs a protocol: messages, chat_completions, or responses"
                .to_string(),
        ),
        (_, None, None) => Ok(None),
        (kind, _, _) => Err(format!(
            "a {} provider is reached at its service's own address and takes no endpoint or \
             protocol; register a custom provider to name one",
            kind.as_str()
        )),
    }
}

/// Check a registration's own fields, then its credential reference against the secrets registry.
/// A provider whose key does not exist, or is not an inference key, would refuse at its first
/// dispatch; refusing it here means an administrator learns at registration instead of a launcher
/// learning hours later.
async fn check_provider(
    pool: &sqlx::PgPool,
    id: &str,
    body: ProviderBody,
) -> Result<Result<CheckedProvider, String>, AppError> {
    let display_name = body.display_name.trim().to_string();
    if display_name.is_empty() {
        return Ok(Err("display_name must be non-empty".to_string()));
    }
    let mut models = Vec::with_capacity(body.models.len());
    for model in &body.models {
        match crate::playbooks::providers::check_model_name(model) {
            Ok(m) => models.push(m),
            Err(msg) => return Ok(Err(format!("models: {msg}"))),
        }
    }
    let default_model = match body.default_model.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => match crate::playbooks::providers::check_model_name(raw) {
            Ok(m) => Some(m),
            Err(msg) => return Ok(Err(format!("default_model: {msg}"))),
        },
    };
    if default_model.is_none() && body.kind.default_model().is_none() {
        return Ok(Err(format!(
            "a {} provider serves whatever the operator loaded, so it has to name its \
             default_model",
            body.kind.as_str()
        )));
    }
    let endpoint = match check_endpoint(body.kind, body.endpoint.as_deref(), body.protocol) {
        Ok(endpoint) => endpoint,
        Err(msg) => return Ok(Err(msg)),
    };
    let harness = match check_harness(
        body.kind,
        endpoint.as_ref().map(|e| e.protocol),
        body.harness.as_deref(),
    ) {
        Ok(harness) => harness,
        Err(msg) => return Ok(Err(msg)),
    };
    let secret = match body.secret_name.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => {
            match check_provider_secret(
                pool,
                id,
                body.kind,
                endpoint.as_ref(),
                raw,
                body.secret_owner.as_deref(),
            )
            .await?
            {
                Ok(secret) => Some(secret),
                Err(msg) => return Ok(Err(msg)),
            }
        }
    };
    Ok(Ok(CheckedProvider {
        display_name,
        kind: body.kind,
        models,
        default_model,
        secret,
        endpoint,
        harness,
        enabled: body.enabled,
    }))
}

/// Pin down which keyspace a named credential lives in. An explicit `secret_owner` is taken as
/// given; without one, the name must be held by exactly one owner, because a bare name is not a
/// reference the registry can answer.
async fn resolve_secret_owner(
    pool: &sqlx::PgPool,
    name: &crate::secrets::SecretName,
    asked: Option<&str>,
) -> Result<Result<crate::authz::model::Principal, String>, AppError> {
    if let Some(raw) = asked.map(str::trim).filter(|o| !o.is_empty()) {
        return Ok(crate::authz::model::Principal::parse(raw)
            .map_err(|e| format!("secret_owner {raw:?} is not a principal: {e}")));
    }
    let rows = crate::secrets::store::find_by_name(pool, name).await?;
    match rows.len() {
        1 => Ok(Ok(rows[0].owner.clone())),
        0 => Ok(Err(format!(
            "secret_name {name} names no secret in the registry"
        ))),
        _ => {
            let owners = rows
                .iter()
                .map(|r| r.owner.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Ok(Err(format!(
                "secret_name {name} is registered by {owners}; name the one this provider spends \
                 in secret_owner"
            )))
        }
    }
}

/// Check the credential a registration names by asking the dispatch-time resolver whether it would
/// take it. A registration whose key the dispatch would refuse is refused here instead, so an
/// administrator learns at registration rather than a launcher learning hours later — and the two
/// answers cannot drift, because there is only one of them.
async fn check_provider_secret(
    pool: &sqlx::PgPool,
    id: &str,
    kind: ProviderKind,
    endpoint: Option<&Endpoint>,
    raw: &str,
    owner: Option<&str>,
) -> Result<Result<ProviderSecretRef, String>, AppError> {
    let name = match crate::secrets::SecretName::parse(raw) {
        Ok(name) => name,
        Err(e) => {
            return Ok(Err(format!(
                "secret_name {raw:?} is not a secret name: {e}"
            )));
        }
    };
    let mut prospective = ModelProvider {
        id: id.to_string(),
        owner: crate::authz::model::Principal::platform(),
        display_name: String::new(),
        kind,
        models: Vec::new(),
        default_model: kind.default_model().unwrap_or_default().to_string(),
        secret: None,
        endpoint: endpoint.cloned(),
        harness: None,
        enabled: true,
        created_by: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    // A provider with nowhere to put a key is refused before the registry is consulted: there is
    // no owner to disambiguate against, and the reference is wrong whatever it names.
    if prospective.api_key_env().is_none() {
        return Ok(Err(format!(
            "a {} provider authenticates with the deploy profile's ambient credentials and must \
             not name a secret",
            kind.as_str()
        )));
    }
    let owner = match resolve_secret_owner(pool, &name, owner).await? {
        Ok(owner) => owner,
        Err(msg) => return Ok(Err(msg)),
    };
    let secret = ProviderSecretRef {
        name: name.as_str().to_string(),
        owner,
    };
    prospective.secret = Some(secret.clone());
    match crate::secrets::launch::resolve_provider_secret(pool, &prospective).await? {
        Ok(_) => Ok(Ok(secret)),
        Err(refusal) => Ok(Err(refusal.to_string())),
    }
}

fn new_provider<'a>(
    id: &'a str,
    checked: &'a CheckedProvider,
    actor: &'a str,
    owner: crate::authz::model::Principal,
) -> crate::playbooks::providers::NewProvider<'a> {
    crate::playbooks::providers::NewProvider {
        id,
        display_name: &checked.display_name,
        kind: checked.kind,
        models: &checked.models,
        default_model: checked.default_model.as_deref(),
        secret: checked.secret.as_ref(),
        endpoint: checked.endpoint.as_ref(),
        harness: checked.harness,
        enabled: checked.enabled,
        created_by: actor,
        owner,
    }
}

#[allow(clippy::result_large_err)]
async fn store_provider(
    state: &ApiState,
    id: &str,
    checked: CheckedProvider,
    actor: &str,
    owner: crate::authz::model::Principal,
) -> Result<ModelProvider, Response> {
    crate::playbooks::providers::upsert(state.db.pool(), &new_provider(id, &checked, actor, owner))
        .await
        .map_err(|e| AppError::from(e).into_response())?;
    read_back(state, id).await
}

#[allow(clippy::result_large_err)]
async fn read_back(state: &ApiState, id: &str) -> Result<ModelProvider, Response> {
    crate::playbooks::providers::get(state.db.pool(), id)
        .await
        .map_err(|e| AppError::from(e).into_response())?
        .ok_or_else(|| {
            AppError::from(anyhow::anyhow!(
                "provider {id} vanished between its write and its read back"
            ))
            .into_response()
        })
}

/// The provider `id` names, decided for `verb` against its owner; a caller who may not read it is
/// told it does not exist.
#[allow(clippy::result_large_err)]
async fn readable_provider(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    verb: crate::authz::action::Verb,
) -> Result<ModelProvider, Response> {
    let row = crate::playbooks::providers::get(state.db.pool(), id)
        .await
        .map_err(|e| AppError::from(e).into_response())?;
    crate::authz::owner::decide_row(
        state,
        caller,
        crate::authz::action::ResourceType::ModelProvider,
        id,
        verb,
        row,
        |r| r.owner.clone(),
    )
    .await
    .map_err(IntoResponse::into_response)
}

#[utoipa::path(
    get,
    path = "/api/providers",
    responses(
        (status = 200, description = "Every registration, disabled ones included", body = Vec<ProviderDetailDto>),
        (status = 403, description = "Caller is not an admin", body = ErrorBody)
    )
)]
pub(crate) async fn list_providers(
    State(state): State<ApiState>,
    _admin: crate::identity::auth::AdminGuard,
    caller: crate::authz::Caller,
) -> Response {
    let rows = match crate::playbooks::providers::list(state.db.pool(), false).await {
        Ok(rows) => rows,
        Err(e) => return AppError::from(e).into_response(),
    };
    match readable_providers(&state, &caller, rows).await {
        Ok(rows) => Json(
            rows.into_iter()
                .map(ProviderDetailDto::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/api/providers",
    request_body = RegisterProviderBody,
    responses(
        (status = 201, description = "Provider registered", body = ProviderDetailDto),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 409, description = "A provider is already registered under that id", body = ErrorBody),
        (status = 422, description = "The id, the model list, or the named secret was refused", body = ErrorBody)
    )
)]
pub(crate) async fn register_provider(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    caller: crate::authz::Caller,
    Json(body): Json<RegisterProviderBody>,
) -> Response {
    let id = match require_provider_id(&body.id) {
        Ok(id) => id,
        Err(msg) => return unprocessable(msg),
    };
    let checked = match check_provider(state.db.pool(), &id, body.provider).await {
        Ok(Ok(checked)) => checked,
        Ok(Err(msg)) => return unprocessable(msg),
        Err(e) => return e.into_response(),
    };
    let actor = identity.as_deref().unwrap_or("unknown");
    let owner = match crate::authz::owner::owner_for_create(
        &state,
        &caller,
        body.owner.as_deref(),
        crate::authz::action::ResourceType::ModelProvider,
    )
    .await
    {
        Ok(owner) => owner,
        Err(refused) => return refused,
    };
    // The existence check IS the write, so two concurrent registrations of one id cannot both
    // decide they created it and silently replace one another.
    match crate::playbooks::providers::insert(
        state.db.pool(),
        &new_provider(&id, &checked, actor, owner.clone()),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                Json(ErrorBody::new(format!(
                    "a provider is already registered as {id}; PUT it to change the registration"
                ))),
            )
                .into_response();
        }
        Err(e) => return AppError::from(e).into_response(),
    }
    let stored = match read_back(&state, &id).await {
        Ok(stored) => stored,
        Err(refusal) => return refusal,
    };
    state
        .audit(
            crate::event_log::Event::now(
                &format!("provider:{id}"),
                "absent",
                "registered",
                Some(&format!(
                    "model provider {id} registered ({})",
                    stored.kind.as_str()
                )),
                None,
            )
            .by(Some(actor)),
            "register_provider",
        )
        .await;
    (StatusCode::CREATED, Json(ProviderDetailDto::from(stored))).into_response()
}

#[utoipa::path(
    put,
    path = "/api/providers/{id}",
    params(("id" = String, Path, description = "Provider slug")),
    request_body = ProviderBody,
    responses(
        (status = 200, description = "Registration replaced", body = ProviderDetailDto),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No provider with that id", body = ErrorBody),
        (status = 422, description = "The model list or the named secret was refused", body = ErrorBody)
    )
)]
pub(crate) async fn update_provider(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    caller: crate::authz::Caller,
    Json(body): Json<ProviderBody>,
) -> Response {
    let existing =
        match readable_provider(&state, &caller, &id, crate::authz::action::Verb::Update).await {
            Ok(row) => row,
            Err(refused) => return refused,
        };
    let checked = match check_provider(state.db.pool(), &id, body).await {
        Ok(Ok(checked)) => checked,
        Ok(Err(msg)) => return unprocessable(msg),
        Err(e) => return e.into_response(),
    };
    let actor = identity.as_deref().unwrap_or("unknown");
    // The registration keeps whoever created it: an edit is not a change of authorship.
    let stored = match store_provider(
        &state,
        &id,
        checked,
        &existing.created_by,
        existing.owner.clone(),
    )
    .await
    {
        Ok(stored) => stored,
        Err(refusal) => return refusal,
    };
    state
        .audit(
            crate::event_log::Event::now(
                &format!("provider:{id}"),
                "registered",
                if stored.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                Some(&format!(
                    "model provider {id} re-registered ({})",
                    stored.kind.as_str()
                )),
                None,
            )
            .by(Some(actor)),
            "update_provider",
        )
        .await;
    Json(ProviderDetailDto::from(stored)).into_response()
}

#[utoipa::path(
    delete,
    path = "/api/providers/{id}",
    params(("id" = String, Path, description = "Provider slug")),
    responses(
        (status = 204, description = "Deregistered, along with every default naming it"),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No provider with that id", body = ErrorBody),
        (status = 409, description = "Issues or schedules still pin it", body = ErrorBody)
    )
)]
pub(crate) async fn delete_provider(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    caller: crate::authz::Caller,
) -> Response {
    if let Err(refused) =
        readable_provider(&state, &caller, &id, crate::authz::action::Verb::Delete).await
    {
        return refused;
    }
    // A pin outlives the registration: the row keeps naming the id, every dispatch of it then
    // fails at resolution, and nothing can clear it but re-registering the same slug. Disabling
    // is the way to retire a provider that work still names.
    match crate::playbooks::providers::usage(state.db.pool(), &id).await {
        Ok(usage) if usage.is_empty() => {}
        Ok(usage) => return (StatusCode::CONFLICT, Json(in_use_body(&id, &usage))).into_response(),
        Err(e) => return AppError::from(e).into_response(),
    }
    match crate::playbooks::providers::delete(state.db.pool(), &id).await {
        Ok(true) => {}
        Ok(false) => return not_found(format!("no provider {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    }
    let actor = identity.as_deref();
    state
        .audit(
            crate::event_log::Event::now(
                &format!("provider:{id}"),
                "registered",
                "deleted",
                Some(&format!("model provider {id} deregistered")),
                None,
            )
            .by(actor),
            "delete_provider",
        )
        .await;
    StatusCode::NO_CONTENT.into_response()
}

/// What still pins a provider, as the refusal a `DELETE` returns.
fn in_use_body(id: &str, usage: &crate::playbooks::providers::ProviderUsage) -> ErrorBody {
    let sample = |names: &[String], total: i64| {
        let listed = names.join(", ");
        match total > names.len() as i64 {
            true => format!("{listed}, and {} more", total - names.len() as i64),
            false => listed,
        }
    };
    let mut held = Vec::new();
    if usage.issue_count > 0 {
        held.push(format!(
            "{} issue(s) ({})",
            usage.issue_count,
            sample(&usage.issues, usage.issue_count)
        ));
    }
    if usage.schedule_count > 0 {
        held.push(format!(
            "{} schedule(s) ({})",
            usage.schedule_count,
            sample(&usage.schedules, usage.schedule_count)
        ));
    }
    ErrorBody::new(format!(
        "provider {id} is still pinned by {}; disable it instead, or clear those pins first",
        held.join(" and ")
    ))
}

// --- the defaults a launch inherits (admin) ----------------------------------

/// Point one scope's dispatches of one workload class at a provider.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct DispatchDefaultBody {
    scope_kind: DefaultScope,
    /// The domain name a `domain` default covers. Absent, or empty, for the platform-wide row.
    #[serde(default)]
    scope_ref: Option<String>,
    workload_class: WorkloadClass,
    /// The registered provider id every unpinned dispatch of this class takes.
    provider: String,
    /// Absent takes the provider's own default model.
    #[serde(default)]
    model: Option<String>,
}

/// Which default a `DELETE` clears.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub(crate) struct DispatchDefaultQuery {
    scope_kind: String,
    #[serde(default)]
    scope_ref: Option<String>,
    workload_class: String,
}

/// The scope half of a default, checked: a domain default names a domain, and the platform row's
/// reference is empty because the platform is not a place.
///
/// A domain is spelled `owner/repo`, because that is the name
/// [`crate::playbooks::providers::resolve_for_issue`] matches a default against. Any other spelling would
/// store a default no dispatch could ever inherit.
fn require_scope_ref(scope_kind: DefaultScope, scope_ref: Option<&str>) -> Result<String, String> {
    let scope_ref = scope_ref.map(str::trim).unwrap_or_default();
    match scope_kind {
        DefaultScope::Platform => {
            if !scope_ref.is_empty() {
                return Err(format!(
                    "a platform default covers everything and takes no scope_ref (got {scope_ref:?})"
                ));
            }
            Ok(String::new())
        }
        DefaultScope::Domain => {
            if scope_ref.is_empty() {
                return Err("a domain default must name the domain in scope_ref".to_string());
            }
            let mut halves = scope_ref.split('/');
            let shaped = matches!(
                (halves.next(), halves.next(), halves.next()),
                (Some(owner), Some(repo), None) if !owner.is_empty() && !repo.is_empty()
            ) && !scope_ref.chars().any(char::is_whitespace);
            if !shaped {
                return Err(format!(
                    "a domain is the repository a dispatch carries, spelled owner/repo (got \
                     {scope_ref:?})"
                ));
            }
            Ok(scope_ref.to_string())
        }
    }
}

#[utoipa::path(
    put,
    path = "/api/config/dispatch-defaults",
    request_body = DispatchDefaultBody,
    responses(
        (status = 200, description = "The default now in force", body = DispatchDefaultDto),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 422, description = "The scope reference or the named provider was refused", body = ErrorBody)
    )
)]
pub(crate) async fn put_dispatch_default(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    Json(body): Json<DispatchDefaultBody>,
) -> Response {
    let scope_ref = match require_scope_ref(body.scope_kind, body.scope_ref.as_deref()) {
        Ok(scope_ref) => scope_ref,
        Err(msg) => return unprocessable(msg),
    };
    let provider_id = body.provider.trim().to_string();
    match crate::playbooks::providers::get(state.db.pool(), &provider_id).await {
        Ok(Some(provider)) if provider.enabled => {}
        // A default is a standing choice for work that named nothing; pointing one at a provider
        // nobody may pick would refuse every dispatch that inherited it.
        Ok(Some(_)) => {
            return unprocessable(format!(
                "provider {provider_id:?} is disabled and cannot be a default"
            ));
        }
        Ok(None) => {
            return unprocessable(format!("provider {provider_id:?} is not registered"));
        }
        Err(e) => return AppError::from(e).into_response(),
    }
    let model = match body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        None => None,
        Some(raw) => match crate::playbooks::providers::check_model_name(raw) {
            Ok(m) => Some(m),
            Err(msg) => return unprocessable(format!("model: {msg}")),
        },
    };
    let row = DispatchDefault {
        scope_kind: body.scope_kind,
        scope_ref,
        workload_class: body.workload_class,
        provider_id,
        model,
    };
    if let Err(e) = crate::playbooks::providers::set_default(state.db.pool(), &row).await {
        return AppError::from(e).into_response();
    }
    state
        .audit(
            crate::event_log::Event::now(
                "dispatch-defaults",
                "config",
                "config",
                Some(&format!(
                    "{} {} dispatches of {:?} now default to provider {}",
                    row.scope_kind.as_str(),
                    row.scope_ref,
                    row.workload_class.as_str(),
                    row.provider_id
                )),
                None,
            )
            .by(identity.as_deref()),
            "put_dispatch_default",
        )
        .await;
    Json(DispatchDefaultDto::from(row)).into_response()
}

#[utoipa::path(
    delete,
    path = "/api/config/dispatch-defaults",
    params(DispatchDefaultQuery),
    responses(
        (status = 204, description = "Cleared; the scope inherits again"),
        (status = 400, description = "The scope kind or workload class did not parse", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "That scope had no default for that class", body = ErrorBody),
        (status = 422, description = "The scope reference was refused", body = ErrorBody)
    )
)]
pub(crate) async fn delete_dispatch_default(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    Query(q): Query<DispatchDefaultQuery>,
) -> Response {
    let scope_kind = match DefaultScope::parse(&q.scope_kind) {
        Ok(scope_kind) => scope_kind,
        Err(e) => return bad_request(e.to_string()),
    };
    let class = match WorkloadClass::parse(&q.workload_class) {
        Ok(class) => class,
        Err(e) => return bad_request(e.to_string()),
    };
    let scope_ref = match require_scope_ref(scope_kind, q.scope_ref.as_deref()) {
        Ok(scope_ref) => scope_ref,
        Err(msg) => return unprocessable(msg),
    };
    match crate::playbooks::providers::clear_default(state.db.pool(), scope_kind, &scope_ref, class)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return not_found(format!(
                "{} {scope_ref:?} has no {} default",
                scope_kind.as_str(),
                class.as_str()
            ));
        }
        Err(e) => return AppError::from(e).into_response(),
    }
    state
        .audit(
            crate::event_log::Event::now(
                "dispatch-defaults",
                "config",
                "config",
                Some(&format!(
                    "{} {scope_ref} dispatches of {:?} inherit again",
                    scope_kind.as_str(),
                    class.as_str()
                )),
                None,
            )
            .by(identity.as_deref()),
            "delete_dispatch_default",
        )
        .await;
    StatusCode::NO_CONTENT.into_response()
}

// --- what a launch pins on its row -------------------------------------------

/// The provider and model a launch chose, as its row will store them. Both `None` is a launch that
/// chose nothing, which resolves through the defaults at dispatch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AgentPin {
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
}

/// Why a launch's provider choice was refused, and which field carries the mistake. The field name
/// is what a form highlights; the message is what a human reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PinRefusal {
    pub(crate) field: &'static str,
    pub(crate) message: String,
}

impl PinRefusal {
    fn new(field: &'static str, message: impl Into<String>) -> PinRefusal {
        PinRefusal {
            field,
            message: message.into(),
        }
    }

    /// The refusal as the field-level body the playbook and schedule forms render.
    pub(crate) fn field_error(self) -> crate::playbooks::registry::FieldError {
        crate::playbooks::registry::FieldError {
            field: self.field.to_string(),
            message: self.message,
        }
    }
}

/// Resolve what a launch asked to run against. A model is only meaningful alongside the provider
/// that serves it, so a lone model is refused rather than silently dropped; the model itself is
/// free text, because a curated list is a suggestion and a launcher may know a newer name.
///
/// A disabled provider is refused here even though [`crate::playbooks::providers::resolve_dispatch`] would
/// still resolve it: an administrator turning one off means no NEW work goes there, while work
/// already pinned to it keeps running.
pub(crate) async fn require_agent_pin(
    state: &ApiState,
    caller: &crate::authz::Caller,
    provider: Option<&str>,
    model: Option<&str>,
) -> anyhow::Result<Result<AgentPin, PinRefusal>> {
    let model = match model {
        None => None,
        Some(raw) => {
            let m = raw.trim();
            if m.is_empty() {
                return Ok(Err(PinRefusal::new(
                    "model",
                    "model must be non-empty when present (omit it for the provider's default \
                     model)",
                )));
            }
            match crate::playbooks::providers::check_model_name(m) {
                Ok(m) => Some(m),
                Err(message) => return Ok(Err(PinRefusal::new("model", message))),
            }
        }
    };
    let Some(raw) = provider else {
        if model.is_some() {
            return Ok(Err(PinRefusal::new(
                "provider",
                "model names a model but no provider; a model belongs to the provider that serves \
                 it",
            )));
        }
        return Ok(Ok(AgentPin::default()));
    };
    let id = raw.trim();
    if id.is_empty() {
        return Ok(Err(PinRefusal::new(
            "provider",
            "provider must be non-empty when present (omit it to take the configured default)",
        )));
    }
    let enabled = launchable(state, caller).await?;
    let Some(provider) = enabled.iter().find(|p| p.id == id) else {
        let known = match enabled.is_empty() {
            true => "none are enabled".to_string(),
            false => enabled
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        };
        let registered = crate::playbooks::providers::get(state.db.pool(), id).await?;
        let disabled = readable_providers(state, caller, registered.into_iter().collect()).await?;
        let why = match disabled.is_empty() {
            false => format!("provider {id:?} is disabled"),
            true => format!("provider {id:?} is not registered on this controller"),
        };
        return Ok(Err(PinRefusal::new(
            "provider",
            format!("{why} (enabled: {known})"),
        )));
    };
    Ok(Ok(AgentPin {
        provider: Some(provider.id.clone()),
        model,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_slug_stays_in_the_url_vocabulary() {
        assert_eq!(
            require_provider_id(" plat-openai ").expect("valid"),
            "plat-openai"
        );
        for bad in [
            "",
            "   ",
            "Plat",
            "plat_openai",
            "plat/openai",
            "plat openai",
        ] {
            assert!(require_provider_id(bad).is_err(), "{bad:?}");
        }
        assert!(require_provider_id(&"a".repeat(PROVIDER_ID_MAX_LEN + 1)).is_err());
    }

    #[test]
    fn a_default_names_a_domain_only_when_it_is_one() {
        assert_eq!(
            require_scope_ref(DefaultScope::Platform, None).expect("valid"),
            ""
        );
        assert_eq!(
            require_scope_ref(DefaultScope::Domain, Some(" org/vllm ")).expect("valid"),
            "org/vllm"
        );
        assert!(require_scope_ref(DefaultScope::Platform, Some("org/vllm")).is_err());
        assert!(require_scope_ref(DefaultScope::Domain, Some("  ")).is_err());
        assert!(require_scope_ref(DefaultScope::Domain, None).is_err());
        // A domain is the repo a dispatch carries; anything else configures a default that no
        // dispatch would ever match.
        for bad in ["vllm", "org/vllm/sub", "/vllm", "org/", "org name/vllm"] {
            assert!(
                require_scope_ref(DefaultScope::Domain, Some(bad)).is_err(),
                "{bad:?}"
            );
        }
    }
}
