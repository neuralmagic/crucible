use crate::api::dto::*;

use crate::api::state::*;

use crate::authz::action::{ResourceType, Verb};

use crate::authz::decision::Resource;

use crate::authz::model::Principal;

use crate::launches::webhooks::transform::{Input, Transform};

use crate::launches::webhooks::verify::{MintError, Verifier, VerifierKind};

use crate::launches::webhooks::{Delivery, SaveError, Webhook};

use crate::playbooks::api::registry::{AuthorizedLaunch, authorize_pack};

use crate::playbooks::registry::FieldError;

use axum::extract::{Path, State};

use axum::http::StatusCode;

use axum::response::{IntoResponse, Response};

use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use utoipa::ToSchema;

/// The most launches per hour any webhook may allow.
pub(crate) const MAX_LAUNCHES_PER_HOUR: i32 = 120;

/// The longest any webhook keeps its settled deliveries.
pub(crate) const MAX_RETENTION_DAYS: i32 = 90;

const DEFAULT_RETENTION_DAYS: i32 = 14;

/// One webhook: what it launches, how a delivery proves itself, and how a delivery becomes params.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookDto {
    pub id: String,
    pub playbook: String,
    pub adopted_repo: Option<String>,
    pub adopted_path: Option<String>,
    pub adopted_rev: Option<String>,
    /// The fixed values every launch carries.
    #[schema(value_type = BTreeMap<String, String>)]
    pub params: serde_json::Value,
    /// Param name to the CEL expression that derives it from a delivery.
    #[schema(value_type = BTreeMap<String, String>)]
    pub derive: serde_json::Value,
    pub filter: String,
    pub dedupe: String,
    pub schema_digest: String,
    pub max_cost: f64,
    pub max_time: String,
    /// `path_token` or `hmac_sha256`.
    pub verifier: String,
    /// The header an `hmac_sha256` signature is in.
    pub header: Option<String>,
    pub max_launches_per_hour: i32,
    pub retention_days: i32,
    /// Where deliveries are posted, relative to the delivery surface. A `path_token` webhook's
    /// deliveries add its token as one more segment.
    pub delivery_path: String,
    /// The full delivery URL when this controller knows its public delivery address. A
    /// `path_token` webhook's URL is this plus its token, which is never readable again.
    pub delivery_url: Option<String>,
    pub last_delivery_at: Option<String>,
    pub enabled: bool,
    pub consecutive_failures: i64,
    pub created_by: Option<String>,
    pub owner_principal: Option<String>,
    pub owner_groups_at: Option<String>,
    pub owner_signin_required: bool,
    pub owner_refresh_error: Option<String>,
    pub owner_refresh_at: Option<String>,
    pub dispatch_target: Option<String>,
    pub agent_provider: Option<String>,
    pub agent_model: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<Webhook> for WebhookDto {
    fn from(w: Webhook) -> Self {
        let c = w.core;
        WebhookDto {
            delivery_path: format!("/hooks/{}", c.id),
            delivery_url: None,
            id: c.id,
            playbook: c.playbook,
            adopted_repo: c.adopted_repo,
            adopted_path: c.adopted_path,
            adopted_rev: c.adopted_rev,
            params: c.params,
            derive: w.derive,
            filter: w.filter,
            dedupe: w.dedupe,
            schema_digest: c.schema_digest,
            max_cost: c.max_cost,
            max_time: c.max_time,
            verifier: w.verifier.as_str().to_string(),
            header: w.header,
            max_launches_per_hour: w.max_launches_per_hour,
            retention_days: w.retention_days,
            last_delivery_at: w.last_delivery_at,
            enabled: c.enabled,
            consecutive_failures: c.consecutive_failures,
            created_by: c.created_by,
            owner_principal: c.owner_principal,
            owner_groups_at: c.owner_groups_at,
            owner_signin_required: c.owner_signin_required,
            owner_refresh_error: c.owner_refresh_error,
            owner_refresh_at: c.owner_refresh_at,
            dispatch_target: c.dispatch_target,
            agent_provider: c.agent_provider,
            agent_model: c.agent_model,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

/// A webhook with the secret a sender is configured with. The secret is never readable again.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookSecretDto {
    pub webhook: WebhookDto,
    pub secret: String,
    /// The full delivery URL, token included for `path_token`, when this controller knows its
    /// public delivery address.
    pub delivery_url: Option<String>,
}

/// One recorded delivery and how it settled.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookDeliveryDto {
    pub id: String,
    pub received_at: String,
    /// `pending`, `launched`, `filtered`, `duplicate`, `throttled`, or `failed`.
    pub outcome: String,
    pub reason: Option<String>,
    pub dedupe_key: Option<String>,
    pub launch_key: Option<String>,
    pub settled_at: Option<String>,
    /// The recorded headers; none that carries a credential is ever recorded.
    #[schema(value_type = Object)]
    pub headers: serde_json::Value,
    /// The body as recorded, when it is UTF-8.
    pub body: Option<String>,
    /// The body as recorded, base64, when it is not UTF-8.
    pub body_base64: Option<String>,
}

impl From<Delivery> for WebhookDeliveryDto {
    fn from(d: Delivery) -> Self {
        use base64::Engine;
        let (body, body_base64) = match String::from_utf8(d.body) {
            Ok(text) => (Some(text), None),
            Err(e) => (
                None,
                Some(base64::engine::general_purpose::STANDARD.encode(e.into_bytes())),
            ),
        };
        WebhookDeliveryDto {
            id: d.id,
            received_at: d.received_at,
            outcome: d.outcome.unwrap_or_else(|| "pending".to_string()),
            reason: d.reason,
            dedupe_key: d.dedupe_key,
            launch_key: d.launch_key,
            settled_at: d.settled_at,
            headers: d.headers,
            body,
            body_base64,
        }
    }
}

/// A webhook's trigger and the authorization it launches with. The same body creates a webhook
/// and replaces one.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct WebhookBody {
    /// The principal to own it: `user:<login>` or `team:<slug>` the caller acts as; absent means
    /// the caller.
    owner: Option<String>,
    /// The registry id every launch runs. Webhooks target adopted revisions only.
    playbook: String,
    /// `path_token` or `hmac_sha256`. Fixed at creation.
    verifier: String,
    /// The header an `hmac_sha256` signature is in (`x-hub-signature-256`). Fixed at creation.
    #[serde(default)]
    header: Option<String>,
    /// A CEL bool over `body`, `headers`, `delivery`, and `received_at`; absent matches every
    /// delivery.
    #[serde(default = "match_all")]
    filter: String,
    /// A CEL string or integer a delivery launches under at most once.
    dedupe: String,
    /// Param name to the CEL expression that derives it. Every name must be a param the playbook
    /// declares and must not also be in `params`.
    #[serde(default)]
    derive: BTreeMap<String, String>,
    /// The fixed param values, validated against the pack's stored schema and frozen.
    #[serde(default)]
    params: BTreeMap<String, String>,
    max_launches_per_hour: i32,
    #[serde(default = "default_retention")]
    retention_days: i32,
    max_cost: f64,
    max_time: String,
    /// The schema digest the form was rendered against, refused when the pack has moved on.
    #[serde(default)]
    schema_digest: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    dispatch_target: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

fn match_all() -> String {
    "true".to_string()
}

fn default_retention() -> i32 {
    DEFAULT_RETENTION_DAYS
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct WebhookEnabledBody {
    enabled: bool,
}

/// A transform and a sample delivery to run it on.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct WebhookPreviewBody {
    #[serde(default = "match_all")]
    filter: String,
    dedupe: String,
    #[serde(default)]
    derive: BTreeMap<String, String>,
    /// The sample body, as the sender would post it.
    #[schema(value_type = Value)]
    sample: serde_json::Value,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// One transform result: the value, or why the delivery would settle failed.
#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewResultDto {
    #[schema(value_type = Option<Value>)]
    pub value: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl<T: Into<serde_json::Value>> From<Result<T, String>> for PreviewResultDto {
    fn from(r: Result<T, String>) -> Self {
        match r {
            Ok(v) => PreviewResultDto {
                value: Some(v.into()),
                error: None,
            },
            Err(e) => PreviewResultDto {
                value: None,
                error: Some(e),
            },
        }
    }
}

/// What a transform makes of a sample delivery. Nothing is stored.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookPreviewDto {
    pub filter: PreviewResultDto,
    pub dedupe: PreviewResultDto,
    pub derive: PreviewResultDto,
}

/// A starting point for a webhook form.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookPresetDto {
    pub id: String,
    pub title: String,
    pub verifier: String,
    pub header: Option<String>,
    pub filter: String,
    pub dedupe: String,
    pub derive: BTreeMap<String, String>,
}

fn field_error(field: &str, message: impl Into<String>) -> FieldError {
    FieldError {
        field: field.to_string(),
        message: message.into(),
    }
}

fn derive_map(derive: &BTreeMap<String, String>) -> serde_json::Map<String, serde_json::Value> {
    derive
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect()
}

/// A webhook as the API answers it, its delivery URL filled in from this controller's public
/// delivery address.
fn dto(state: &ApiState, w: Webhook) -> WebhookDto {
    let mut dto = WebhookDto::from(w);
    dto.delivery_url = state
        .hooks_public_url
        .as_deref()
        .map(|base| format!("{}{}", base.trim_end_matches('/'), dto.delivery_path));
    dto
}

fn delivery_url(state: &ApiState, id: &str, kind: VerifierKind, secret: &str) -> Option<String> {
    let base = state.hooks_public_url.as_deref()?.trim_end_matches('/');
    Some(match kind {
        VerifierKind::PathToken => format!("{base}/hooks/{id}/{secret}"),
        _ => format!("{base}/hooks/{id}"),
    })
}

fn save_refusal(e: SaveError) -> Response {
    match e {
        SaveError::Mint(e @ MintError::NoCredentialKey(_)) => {
            invalid_fields(vec![field_error("verifier", e.to_string())])
        }
        e @ SaveError::VerifierFixed => {
            invalid_fields(vec![field_error("verifier", e.to_string())])
        }
        other => AppError::from(anyhow::Error::new(other)).into_response(),
    }
}

/// Everything a save stores besides ownership, validated.
struct AuthorizedWebhook {
    launch: AuthorizedLaunch,
    verifier: Verifier,
    derive: serde_json::Map<String, serde_json::Value>,
}

#[allow(clippy::result_large_err)]
fn check_verifier(body: &WebhookBody) -> Result<Verifier, Vec<FieldError>> {
    let kind = VerifierKind::parse(&body.verifier)
        .map_err(|e| vec![field_error("verifier", e.to_string())])?;
    let header = match (kind.signs(), body.header.as_deref()) {
        (true, Some(raw)) => {
            let name = raw.trim().to_ascii_lowercase();
            axum::http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                vec![field_error(
                    "header",
                    format!("{raw:?} is not a header name"),
                )]
            })?;
            Some(name)
        }
        (true, None) => {
            return Err(vec![field_error(
                "header",
                format!("the {} verifier reads a named header", kind.as_str()),
            )]);
        }
        (false, Some(_)) => {
            return Err(vec![field_error(
                "header",
                format!("the {} verifier reads no header", kind.as_str()),
            )]);
        }
        (false, None) => None,
    };
    Ok(Verifier { kind, header })
}

/// Validate a webhook body: the verifier, the bounds, and the transform here, the launch half
/// through the same [`authorize_pack`] a form POST, a schedule, and a watch use.
#[allow(clippy::result_large_err)]
async fn authorize_webhook(
    state: &ApiState,
    caller: &crate::authz::Caller,
    body: &WebhookBody,
) -> Result<AuthorizedWebhook, Response> {
    let mut refused = Vec::new();
    let verifier = match check_verifier(body) {
        Ok(v) => Some(v),
        Err(e) => {
            refused.extend(e);
            None
        }
    };
    if !(1..=MAX_LAUNCHES_PER_HOUR).contains(&body.max_launches_per_hour) {
        refused.push(field_error(
            "max_launches_per_hour",
            format!("must be between 1 and {MAX_LAUNCHES_PER_HOUR}"),
        ));
    }
    if !(1..=MAX_RETENTION_DAYS).contains(&body.retention_days) {
        refused.push(field_error(
            "retention_days",
            format!("must be between 1 and {MAX_RETENTION_DAYS}"),
        ));
    }
    let derive = derive_map(&body.derive);
    if let Err(e) = Transform::compile(&body.filter, &body.dedupe, &derive) {
        refused.extend(e);
    }
    for name in body.derive.keys() {
        if body.params.contains_key(name) {
            refused.push(field_error(
                &format!("derive.{name}"),
                "is also given a fixed value in params",
            ));
        }
    }
    let (pack, schema, max_time) = authorize_pack(
        state,
        caller,
        &body.playbook,
        body.max_cost,
        &body.max_time,
        body.schema_digest.as_deref(),
    )
    .await?;
    let mut relaxed = schema.clone();
    for name in body.derive.keys() {
        if schema.pointer(&format!("/properties/{name}")).is_none() {
            refused.push(field_error(
                &format!("derive.{name}"),
                format!("the playbook declares no param named {name}"),
            ));
        }
        if let Some(required) = relaxed.get_mut("required").and_then(|r| r.as_array_mut()) {
            required.retain(|r| r.as_str() != Some(name.as_str()));
        }
    }
    let params = match crate::playbooks::registry::validate_params(&relaxed, &body.params) {
        Ok(params) => Some(params),
        Err(e) => {
            refused.extend(e);
            None
        }
    };
    match (verifier, params) {
        (Some(verifier), Some(params)) if refused.is_empty() => Ok(AuthorizedWebhook {
            launch: AuthorizedLaunch {
                pack,
                params,
                max_cost: body.max_cost,
                max_time,
            },
            verifier,
            derive,
        }),
        _ => Err(invalid_fields(refused)),
    }
}

fn new_webhook<'a>(
    authorized: &'a AuthorizedWebhook,
    body: &'a WebhookBody,
    saver: &'a crate::playbooks::api::saver::Saver,
) -> crate::launches::webhooks::NewWebhook<'a> {
    crate::launches::webhooks::NewWebhook {
        standing: crate::launches::standing::NewStanding {
            playbook: &authorized.launch.pack.id,
            target_kind: "adopted",
            eligible_draft_version: None,
            params: &authorized.launch.params,
            schema_digest: &authorized.launch.pack.schema_digest,
            max_cost: authorized.launch.max_cost,
            max_time: &authorized.launch.max_time,
            advance_dedupe: false,
            enabled: body.enabled,
            created_by: saver.actor.as_deref(),
            owner_principal: saver.owner_principal.as_deref(),
            owner_groups: Some(&saver.groups),
            dispatch_target: Some(&saver.dispatch_target),
            agent_provider: saver.provider.as_deref(),
            agent_model: saver.model.as_deref(),
        },
        verifier: &authorized.verifier,
        filter: &body.filter,
        dedupe: &body.dedupe,
        derive: &authorized.derive,
        max_launches_per_hour: body.max_launches_per_hour,
        retention_days: body.retention_days,
    }
}

fn resource_of(w: &Webhook) -> Resource {
    Resource::new(
        ResourceType::StandingLaunch,
        &w.core.id,
        Principal::stored(w.core.owner_principal.as_deref()),
    )
}

#[allow(clippy::result_large_err)]
async fn audit(
    state: &ApiState,
    id: &str,
    from: &str,
    to: &str,
    reason: &str,
    actor: Option<&str>,
) -> Result<(), Response> {
    state
        .audit_required(
            crate::event_log::Event::now(
                &crate::model::Trigger::Webhook.event_key(id),
                from,
                to,
                Some(reason),
                None,
            )
            .by(actor),
        )
        .await
        .map_err(|e| AppError::from(e).into_response())
}

fn state_of(enabled: bool) -> &'static str {
    if enabled { "enabled" } else { "disabled" }
}

/// Create a webhook and mint its secret. The secret is in this response and nowhere else.
#[utoipa::path(
    post,
    path = "/api/webhooks",
    request_body = WebhookBody,
    responses(
        (status = 201, description = "Webhook stored with its one-time secret", body = WebhookSecretDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No playbook with that id", body = ErrorBody),
        (status = 409, description = "The form was rendered against a schema this playbook no longer serves", body = ErrorBody),
        (status = 422, description = "The verifier, bounds, transform, or parameter values were refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn create_webhook(
    State(state): State<ApiState>,
    groups: crate::identity::auth::Groups,
    caller: crate::authz::Caller,
    axum::Json(body): axum::Json<WebhookBody>,
) -> Response {
    let authorized = match authorize_webhook(&state, &caller, &body).await {
        Ok(ok) => ok,
        Err(refusal) => return refusal,
    };
    let saver = match crate::playbooks::api::saver::resolve_saver(
        &state,
        &caller,
        &groups,
        crate::playbooks::api::saver::Ownership::Snapshot {
            asked: body.owner.as_deref(),
        },
        crate::playbooks::api::saver::SaverRequest {
            agent: authorized.launch.pack.agent.clone(),
            dispatch_target: body.dispatch_target.as_deref(),
            provider: body.provider.as_deref(),
            model: body.model.as_deref(),
        },
    )
    .await
    {
        Ok(s) => s,
        Err(refusal) => return refusal,
    };
    if let Err(denied) = crate::authz::owner::decide_create(
        &state,
        &caller,
        ResourceType::StandingLaunch,
        &Principal::stored(saver.owner_principal.as_deref()),
    )
    .await
    {
        return denied.into_response();
    }
    if let Err(denied) =
        crate::playbooks::api::registry::decide_launch(&state, &caller, &authorized.launch.pack)
            .await
    {
        return denied;
    }
    let created = match crate::launches::webhooks::create(
        state.db.pool(),
        state.credential_keys.as_deref(),
        &new_webhook(&authorized, &body, &saver),
    )
    .await
    {
        Ok(created) => created,
        Err(e) => return save_refusal(e),
    };
    let webhook = created.webhook;
    let reason = format!(
        "playbook {} on {} deliveries as revision {}",
        webhook.core.playbook,
        webhook.verifier.as_str(),
        webhook.core.adopted_rev.as_deref().unwrap_or("?")
    );
    if let Err(refusal) = audit(
        &state,
        &webhook.core.id,
        "new",
        state_of(webhook.core.enabled),
        &reason,
        saver.actor.as_deref(),
    )
    .await
    {
        return refusal;
    }
    let url = delivery_url(&state, &webhook.core.id, webhook.verifier, &created.secret);
    (
        StatusCode::CREATED,
        Json(WebhookSecretDto {
            webhook: dto(&state, webhook),
            secret: created.secret,
            delivery_url: url,
        }),
    )
        .into_response()
}

/// `GET /api/webhooks` — the webhooks the caller may read, enabled first.
#[utoipa::path(
    get,
    path = "/api/webhooks",
    responses((status = 200, description = "Webhooks", body = Vec<WebhookDto>))
)]
pub(crate) async fn list_webhooks(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
) -> Result<Json<Vec<WebhookDto>>, AppError> {
    let rows =
        crate::launches::webhooks::list(state.db.pool(), crate::launches::webhooks::LIST_LIMIT)
            .await?;
    let readable = crate::authz::owner::readable(
        &state,
        &caller,
        ResourceType::StandingLaunch,
        rows,
        resource_of,
    )
    .await?;
    Ok(Json(readable.into_iter().map(|w| dto(&state, w)).collect()))
}

/// `GET /api/webhooks/presets` — starting points for a webhook form.
#[utoipa::path(
    get,
    path = "/api/webhooks/presets",
    responses((status = 200, description = "Presets", body = Vec<WebhookPresetDto>))
)]
pub(crate) async fn list_webhook_presets() -> Json<Vec<WebhookPresetDto>> {
    Json(
        crate::launches::webhooks::presets::PRESETS
            .iter()
            .map(|p| WebhookPresetDto {
                id: p.id.to_string(),
                title: p.title.to_string(),
                verifier: p.verifier.as_str().to_string(),
                header: p.header.map(str::to_string),
                filter: p.filter.to_string(),
                dedupe: p.dedupe.to_string(),
                derive: p
                    .derive
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            })
            .collect(),
    )
}

#[allow(clippy::result_large_err)]
async fn readable_webhook(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    verb: Verb,
) -> Result<Webhook, Response> {
    let row = crate::launches::webhooks::get(state.db.pool(), id)
        .await
        .map_err(|e| AppError::from(e).into_response())?;
    crate::authz::owner::decide_row(
        state,
        caller,
        ResourceType::StandingLaunch,
        id,
        verb,
        row,
        |w| Principal::stored(w.core.owner_principal.as_deref()),
    )
    .await
    .map_err(IntoResponse::into_response)
}

/// `GET /api/webhooks/{id}` — one webhook, the source an edit form prefills from.
#[utoipa::path(
    get,
    path = "/api/webhooks/{id}",
    params(("id" = String, Path, description = "Webhook id")),
    responses(
        (status = 200, description = "The webhook", body = WebhookDto),
        (status = 404, description = "No webhook with that id", body = ErrorBody)
    )
)]
pub(crate) async fn get_webhook(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Response {
    match readable_webhook(&state, &caller, &id, Verb::Read).await {
        Ok(w) => Json(dto(&state, w)).into_response(),
        Err(refusal) => refusal,
    }
}

/// Replace a webhook: the launch, the transform, and the bounds. The verifier and its secret stay;
/// a body naming another verifier is refused.
#[utoipa::path(
    put,
    path = "/api/webhooks/{id}",
    params(("id" = String, Path, description = "Webhook id")),
    request_body = WebhookBody,
    responses(
        (status = 200, description = "The webhook as stored", body = WebhookDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No webhook (or no playbook) with that id", body = ErrorBody),
        (status = 409, description = "The form was rendered against a schema this playbook no longer serves", body = ErrorBody),
        (status = 422, description = "The verifier, bounds, transform, or parameter values were refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn update_webhook(
    State(state): State<ApiState>,
    groups: crate::identity::auth::Groups,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<WebhookBody>,
) -> Response {
    let snapshot = match crate::launches::api::authorize_standing(
        &state,
        &caller,
        &id,
        Verb::Update,
        &format!("no webhook {id:?}"),
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(refusal) => return refusal,
    };
    let prior = match crate::launches::webhooks::get(state.db.pool(), &id).await {
        Ok(Some(prior)) => prior,
        Ok(None) => return not_found(format!("no webhook {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    };
    let authorized = match authorize_webhook(&state, &caller, &body).await {
        Ok(ok) => ok,
        Err(refusal) => return refusal,
    };
    let saver = match crate::playbooks::api::saver::resolve_saver(
        &state,
        &caller,
        &groups,
        crate::playbooks::api::saver::Ownership::Keep {
            principal: snapshot.principal.as_deref(),
            groups: snapshot.groups.as_ref(),
        },
        crate::playbooks::api::saver::SaverRequest {
            agent: authorized.launch.pack.agent.clone(),
            dispatch_target: body.dispatch_target.as_deref(),
            provider: body.provider.as_deref(),
            model: body.model.as_deref(),
        },
    )
    .await
    {
        Ok(s) => s,
        Err(refusal) => return refusal,
    };
    let firing_changed = prior.core.playbook != authorized.launch.pack.id
        || prior.core.adopted_rev.as_deref() != Some(authorized.launch.pack.rev.as_str())
        || prior.core.params != authorized.launch.params
        || prior.core.dispatch_target.as_deref() != Some(saver.dispatch_target.as_str())
        || prior.core.agent_provider != saver.provider
        || prior.core.agent_model != saver.model
        || prior.filter != body.filter
        || prior.dedupe != body.dedupe
        || prior.derive != serde_json::Value::Object(authorized.derive.clone())
        || prior.max_launches_per_hour != body.max_launches_per_hour;
    if firing_changed
        && let Err(denied) =
            crate::playbooks::api::registry::decide_launch(&state, &caller, &authorized.launch.pack)
                .await
    {
        return denied;
    }
    if prior.verifier() != authorized.verifier {
        return save_refusal(SaveError::VerifierFixed);
    }
    let webhook = match crate::launches::webhooks::update(
        state.db.pool(),
        &id,
        &new_webhook(&authorized, &body, &saver),
    )
    .await
    {
        Ok(Some(updated)) => updated,
        Ok(None) => return not_found(format!("no webhook {id:?}")),
        Err(e) => return save_refusal(e),
    };
    match crate::launches::webhooks::expire_stale_owner_parks(state.db.pool(), &id).await {
        Ok(expired) if !expired.is_empty() => {
            tracing::info!(webhook = %id, expired = expired.len(), "webhooks: re-save expired stale-owner parks");
        }
        Ok(_) => {}
        Err(e) => return AppError::from(e).into_response(),
    }
    if !webhook.core.enabled
        && let Err(e) = crate::launches::webhooks::settle_pending(
            state.db.pool(),
            &id,
            "the webhook is disabled",
        )
        .await
    {
        return AppError::from(e).into_response();
    }
    let reason = format!(
        "playbook {} on {} deliveries",
        webhook.core.playbook,
        webhook.verifier.as_str()
    );
    if let Err(refusal) = audit(
        &state,
        &id,
        "edited",
        state_of(webhook.core.enabled),
        &reason,
        saver.actor.as_deref(),
    )
    .await
    {
        return refusal;
    }
    Json(dto(&state, webhook)).into_response()
}

/// Delete a webhook. Its deliveries and consumed keys go with it; the launches it made stay.
#[utoipa::path(
    delete,
    path = "/api/webhooks/{id}",
    params(("id" = String, Path, description = "Webhook id")),
    responses(
        (status = 204, description = "Deleted; its delivery address answers not found"),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No webhook with that id", body = ErrorBody)
    )
)]
pub(crate) async fn delete_webhook(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Response {
    if let Err(refusal) = readable_webhook(&state, &caller, &id, Verb::Delete).await {
        return refusal;
    }
    if let Err(e) =
        crate::launches::webhooks::settle_pending(state.db.pool(), &id, "the webhook is deleted")
            .await
    {
        return AppError::from(e).into_response();
    }
    match crate::launches::standing::delete(state.db.pool(), &id).await {
        Ok(true) => {}
        Ok(false) => return not_found(format!("no webhook {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    }
    if let Err(refusal) = audit(
        &state,
        &id,
        "enabled",
        "deleted",
        "webhook deleted",
        identity.as_deref(),
    )
    .await
    {
        return refusal;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Enable or disable a webhook without re-authorizing it. Enabling clears the failure count;
/// disabling settles every pending delivery failed.
#[utoipa::path(
    post,
    path = "/api/webhooks/{id}/enabled",
    params(("id" = String, Path, description = "Webhook id")),
    request_body = WebhookEnabledBody,
    responses(
        (status = 200, description = "The webhook as stored", body = WebhookDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No webhook with that id", body = ErrorBody)
    )
)]
pub(crate) async fn set_webhook_enabled(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<WebhookEnabledBody>,
) -> Response {
    if let Err(refusal) = readable_webhook(&state, &caller, &id, Verb::Update).await {
        return refusal;
    }
    let prior =
        match crate::launches::standing::set_enabled(state.db.pool(), &id, body.enabled).await {
            Ok(Some(prior)) => prior,
            Ok(None) => return not_found(format!("no webhook {id:?}")),
            Err(e) => return AppError::from(e).into_response(),
        };
    if !body.enabled
        && let Err(e) = crate::launches::webhooks::settle_pending(
            state.db.pool(),
            &id,
            "the webhook is disabled",
        )
        .await
    {
        return AppError::from(e).into_response();
    }
    if prior != body.enabled
        && let Err(refusal) = audit(
            &state,
            &id,
            state_of(prior),
            state_of(body.enabled),
            "by request",
            identity.as_deref(),
        )
        .await
    {
        return refusal;
    }
    match crate::launches::webhooks::get(state.db.pool(), &id).await {
        Ok(Some(w)) => Json(dto(&state, w)).into_response(),
        Ok(None) => not_found(format!("no webhook {id:?}")),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// Replace a webhook's secret. Deliveries signed or addressed with the old one are refused from
/// now on.
#[utoipa::path(
    post,
    path = "/api/webhooks/{id}/secret",
    params(("id" = String, Path, description = "Webhook id")),
    responses(
        (status = 200, description = "The webhook with its new one-time secret", body = WebhookSecretDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No webhook with that id", body = ErrorBody),
        (status = 422, description = "This controller cannot keep the verifier's secret", body = ValidationErrorBody)
    )
)]
pub(crate) async fn rotate_webhook_secret(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Response {
    if let Err(refusal) = readable_webhook(&state, &caller, &id, Verb::Update).await {
        return refusal;
    }
    let secret = match crate::launches::webhooks::rotate_secret(
        state.db.pool(),
        state.credential_keys.as_deref(),
        &id,
    )
    .await
    {
        Ok(Some(secret)) => secret,
        Ok(None) => return not_found(format!("no webhook {id:?}")),
        Err(e) => return save_refusal(e),
    };
    if let Err(refusal) = audit(
        &state,
        &id,
        "secret",
        "replaced",
        "secret replaced",
        identity.as_deref(),
    )
    .await
    {
        return refusal;
    }
    match crate::launches::webhooks::get(state.db.pool(), &id).await {
        Ok(Some(w)) => {
            let url = delivery_url(&state, &id, w.verifier, &secret);
            Json(WebhookSecretDto {
                webhook: dto(&state, w),
                secret,
                delivery_url: url,
            })
            .into_response()
        }
        Ok(None) => not_found(format!("no webhook {id:?}")),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// A page of a webhook's deliveries.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct DeliveriesQuery {
    /// Only deliveries recorded before this delivery id: the last id of the previous page.
    #[serde(default)]
    before: Option<String>,
    /// Page size, at most 200.
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/webhooks/{id}/deliveries` — a webhook's recorded deliveries, newest first, a page at
/// a time.
#[utoipa::path(
    get,
    path = "/api/webhooks/{id}/deliveries",
    params(("id" = String, Path, description = "Webhook id"), DeliveriesQuery),
    responses(
        (status = 200, description = "Recorded deliveries", body = Vec<WebhookDeliveryDto>),
        (status = 404, description = "No webhook with that id", body = ErrorBody)
    )
)]
pub(crate) async fn list_webhook_deliveries(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    axum::extract::Query(page): axum::extract::Query<DeliveriesQuery>,
) -> Response {
    if let Err(refusal) = readable_webhook(&state, &caller, &id, Verb::Read).await {
        return refusal;
    }
    let limit = page
        .limit
        .unwrap_or(crate::launches::webhooks::LIST_LIMIT)
        .clamp(1, crate::launches::webhooks::LIST_LIMIT);
    match crate::launches::webhooks::deliveries(state.db.pool(), &id, page.before.as_deref(), limit)
        .await
    {
        Ok(rows) => Json(
            rows.into_iter()
                .map(WebhookDeliveryDto::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// Run a transform on a sample delivery and say what each part makes of it. Nothing is stored.
#[utoipa::path(
    post,
    path = "/api/webhooks/preview",
    request_body = WebhookPreviewBody,
    responses(
        (status = 200, description = "What the transform makes of the sample", body = WebhookPreviewDto),
        (status = 422, description = "An expression was refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn preview_webhook(axum::Json(body): axum::Json<WebhookPreviewBody>) -> Response {
    let transform = match Transform::compile(&body.filter, &body.dedupe, &derive_map(&body.derive))
    {
        Ok(t) => t,
        Err(refused) => return invalid_fields(refused),
    };
    let headers: serde_json::Map<String, serde_json::Value> = body
        .headers
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), serde_json::Value::String(v)))
        .collect();
    let evaluated = transform.evaluate(&Input {
        delivery: "preview",
        body: &body.sample,
        headers: &headers,
        received_at: &crate::clock::now_rfc3339(),
    });
    Json(WebhookPreviewDto {
        filter: evaluated.filter.into(),
        dedupe: evaluated.dedupe.into(),
        derive: evaluated.params.map(serde_json::Value::Object).into(),
    })
    .into_response()
}

/// A body to check: the expressions of a transform, with no sample.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct WebhookCheckBody {
    #[serde(default = "match_all")]
    filter: String,
    #[serde(default)]
    dedupe: String,
    #[serde(default)]
    derive: BTreeMap<String, String>,
}

/// One refusal of one expression. `line` and `column` are 1-based, present when the parser named a
/// position; without them the refusal is about the whole expression.
#[derive(Debug, Serialize, ToSchema)]
pub struct CelDiagnosticDto {
    /// `filter`, `dedupe`, or `derive.<param>`.
    pub field: String,
    pub message: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookCheckDto {
    /// Every refusal a save would answer with; empty when the transform compiles.
    pub diagnostics: Vec<CelDiagnosticDto>,
}

/// Check a transform's expressions the way a save does, and say where each refusal is. Nothing is
/// evaluated or stored.
#[utoipa::path(
    post,
    path = "/api/webhooks/check",
    request_body = WebhookCheckBody,
    responses((status = 200, description = "The refusals a save would answer with", body = WebhookCheckDto))
)]
pub(crate) async fn check_webhook(
    axum::Json(body): axum::Json<WebhookCheckBody>,
) -> Json<WebhookCheckDto> {
    let diagnostics = crate::launches::webhooks::transform::diagnose(
        &body.filter,
        &body.dedupe,
        &derive_map(&body.derive),
    )
    .into_iter()
    .map(|d| CelDiagnosticDto {
        field: d.field,
        message: d.message,
        line: d.at.map(|(line, _)| line),
        column: d.at.map(|(_, column)| column),
    })
    .collect();
    Json(WebhookCheckDto { diagnostics })
}

/// A function an editor offers, and whether it is called on a target (`s.startsWith(p)`).
#[derive(Debug, Serialize, ToSchema)]
pub struct CelFunctionDto {
    pub name: String,
    pub member: bool,
}

/// What a transform may name: its variables, the functions the interpreter defines, and the macros.
#[derive(Debug, Serialize, ToSchema)]
pub struct CelLanguageDto {
    pub variables: Vec<String>,
    pub functions: Vec<CelFunctionDto>,
    pub macros: Vec<String>,
}

/// `GET /api/webhooks/cel` — what a webhook transform may name.
#[utoipa::path(
    get,
    path = "/api/webhooks/cel",
    responses((status = 200, description = "Variables, functions, and macros", body = CelLanguageDto))
)]
pub(crate) async fn webhook_cel_language() -> Json<CelLanguageDto> {
    use crate::launches::webhooks::transform::{FUNCTIONS, MACROS, VARIABLES};
    Json(CelLanguageDto {
        variables: VARIABLES.iter().map(|v| v.to_string()).collect(),
        functions: FUNCTIONS
            .iter()
            .map(|(name, member)| CelFunctionDto {
                name: name.to_string(),
                member: *member,
            })
            .collect(),
        macros: MACROS.iter().map(|m| m.to_string()).collect(),
    })
}
