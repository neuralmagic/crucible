use crate::api::dto::{bad_request, unavailable};
use crate::api::state::*;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// --- runtime config overrides (Lane O2) ---------------------------------------

/// The effective controller config: every overridable knob with its resolved value, source
/// (`default`/`env`/`override`), overridable flag, and description.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigDto {
    pub knobs: Vec<crate::daemon::overrides_store::KnobView>,
    /// This build's contract version and every dispatch target's, as last checked.
    pub contract: crate::runs::contract::ContractDto,
    /// Draft-head schedules disabled by this write. Zero on reads and unrelated writes.
    pub draft_head_schedules_disabled: u64,
}

/// The `PUT /api/config/overrides` body: the FULL desired override set plus a required
/// `justification` (recorded in the audit event). Absent knobs mean "not overridden" (fall back to
/// the defaults layer) — the write replaces the whole set, it isn't a partial patch.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ConfigOverridesBody {
    #[serde(flatten)]
    pub overrides: crate::daemon::overrides_store::OverrideSet,
    pub justification: String,
}

fn config_dto(
    state: &ApiState,
    store: &crate::daemon::overrides_store::ConfigStore,
    draft_head_schedules_disabled: u64,
) -> ConfigDto {
    ConfigDto {
        knobs: store.views(),
        contract: state.contracts.snapshot(),
        draft_head_schedules_disabled,
    }
}

#[utoipa::path(
    get,
    path = "/api/config",
    responses(
        (status = 200, description = "Effective config: every knob with value, source, description", body = ConfigDto),
        (status = 503, description = "No override store configured", body = ErrorBody)
    )
)]
pub(crate) async fn get_config(State(state): State<ApiState>) -> Response {
    let Some(store) = state.config.as_ref() else {
        return unavailable("config override store not initialized");
    };
    Json(config_dto(&state, store, 0)).into_response()
}

#[utoipa::path(
    put,
    path = "/api/config/overrides",
    request_body = ConfigOverridesBody,
    responses(
        (status = 200, description = "Overrides written; returns the new effective config", body = ConfigDto),
        (status = 400, description = "The override set failed validation", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 503, description = "No override store configured", body = ErrorBody)
    )
)]
pub(crate) async fn put_config_overrides(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    Json(body): Json<ConfigOverridesBody>,
) -> Response {
    let Some(store) = state.config.clone() else {
        return unavailable("config override store not initialized");
    };
    let justification = body.justification.trim().to_string();
    if justification.is_empty() {
        return bad_request("justification is required and must be non-empty");
    }
    let next = body.overrides;
    if let Err(errs) = next.validate() {
        return bad_request(format!("override validation failed: {}", errs.join("; ")));
    }

    let actor = identity.as_deref();
    let _policy_transition = store.recurring_policy_write().await;
    // Diff against the previous set for the audit trail's changed-keys record.
    let allowed_before = store.effective().allow_draft_head_schedules;
    let prev = store.current();
    let changed = next.changed_keys(&prev);

    if let Err(e) = store.write_overrides(&next).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody::new(format!("writing overrides failed: {e:#}"))),
        )
            .into_response();
    }
    let allowed_after = store.effective().allow_draft_head_schedules;
    let disabled = if allowed_before && !allowed_after {
        match state
            .schedules
            .disable_draft_head_schedules(
                "disabled by administrator recurring-playbook policy change",
            )
            .await
        {
            Ok(count) => count,
            Err(e) => return AppError::from(e).into_response(),
        }
    } else {
        0
    };

    // Audit: who, why, and exactly which knobs moved.
    let policy_change = (allowed_before != allowed_after).then(|| {
        format!(
            "; allow_draft_head_schedules {allowed_before} -> {allowed_after}; disabled {disabled} schedule(s)"
        )
    });
    let mut reason = if changed.is_empty() {
        format!("config override write (no change): {justification}")
    } else {
        format!("config override [{}]: {justification}", changed.join(","))
    };
    if let Some(policy_change) = policy_change {
        reason.push_str(&policy_change);
    }
    if let Err(e) = state
        .audit_required(
            crate::event_log::Event::now("config", "config", "config", Some(&reason), None)
                .by(actor),
        )
        .await
    {
        return AppError::from(e).into_response();
    }

    Json(config_dto(&state, &store, disabled)).into_response()
}

// --- named broker codegen contracts (deploy-pinned) --------------------------

/// The contract NAMES this deploy configured, sorted. Names only: a contract body carries base
/// images and benchmark command lines, and the New Scenario form only ever needs to pick one.
#[derive(Debug, Serialize, ToSchema)]
pub struct BrokerContractsDto {
    pub names: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/api/config/broker-contracts",
    responses(
        (status = 200, description = "The configured broker codegen contract names", body = BrokerContractsDto)
    )
)]
pub(crate) async fn get_broker_contracts(State(state): State<ApiState>) -> Response {
    Json(BrokerContractsDto {
        names: state.broker_contracts.names(),
    })
    .into_response()
}

// --- playbook ceiling caps (deploy-pinned) -----------------------------------

/// The admin bounds a playbook launch's ceilings are checked against, so the launch form can hold
/// its inputs inside them instead of discovering the bound from a 422.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlaybookCapsDto {
    /// The largest per-run cost ceiling a launcher may ask for, in USD.
    pub max_cost: f64,
    /// The longest wall-clock ceiling a launcher may ask for, in the engine's duration grammar.
    pub max_time: String,
}

#[utoipa::path(
    get,
    path = "/api/config/playbook-caps",
    responses(
        (status = 200, description = "The admin caps a playbook launch's ceilings are bounded by", body = PlaybookCapsDto)
    )
)]
pub(crate) async fn get_playbook_caps(State(state): State<ApiState>) -> Response {
    Json(PlaybookCapsDto {
        max_cost: state.playbook_caps.max_cost,
        max_time: state.playbook_caps.max_time.as_str().to_string(),
    })
    .into_response()
}
