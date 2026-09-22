use crate::api::dto::*;
use crate::dto::dto;

use crate::api::state::*;

use crate::launches::tracker::TrackerKind;

use crate::launches::watches::Watch;

use crate::playbooks::api::registry::{AuthorizedLaunch, authorize_pack};

use axum::extract::{Path, State};

use axum::http::StatusCode;

use axum::response::{IntoResponse, Response};

use serde::{Deserialize, Serialize};

use utoipa::ToSchema;

dto! {
    /// One tracker watch: what it launches, what it sweeps, and how its last sweeps went.
    pub struct WatchDto: From<w: Watch> {
        pub id: String,
        pub playbook: String,
        pub adopted_repo: Option<String>,
        pub adopted_path: Option<String>,
        pub adopted_rev: Option<String>,
        /// The values every launch carries, besides the item's key.
        #[schema(value_type = Object)]
        pub params: serde_json::Value,
        pub schema_digest: String,
        pub max_cost: f64,
        pub max_time: String,
        /// The tracker swept (`jira`).
        pub tracker: String,
        /// The query in the tracker's native language.
        pub query: String,
        /// The playbook param a matching item's identifier is passed as.
        pub key_param: String,
        /// The update time the next sweep searches from, RFC 3339. Items updated before it never
        /// launch automatically.
        pub watermark: String,
        pub enabled: bool,
        pub last_swept_at: Option<String>,
        pub last_launched_at: Option<String>,
        /// Sweeps that did not launch since the last one that did.
        pub consecutive_failures: i64,
        pub created_by: Option<String>,
        /// The principal this watch's launches run under, and when its group snapshot was last taken.
        pub owner_principal: Option<String>,
        pub owner_groups_at: Option<String>,
        /// The fire-time group refresh, for the watches view: whether the owner has to sign in again
        /// before this watch can launch, why the last refresh did not land, and when it was tried.
        pub owner_signin_required: bool,
        pub owner_refresh_error: Option<String>,
        pub owner_refresh_at: Option<String>,
        pub dispatch_target: Option<String>,
        pub agent_provider: Option<String>,
        pub agent_model: Option<String>,
        pub created_at: String,
        pub updated_at: String,
    }
}

dto! {
    /// One item a watch launched.
    pub struct WatchHitDto: From<h: crate::launches::watches::Hit> {
        pub item_id: String,
        /// The item's update time when it was launched, RFC 3339.
        pub item_updated_at: String,
        pub launch_key: String,
        pub launched_at: String,
    }
}

/// A watch's trigger and the authorization it launches with. The same body creates a watch and
/// replaces one: a PUT is the whole watch, so an edit can never leave half the old authorization
/// behind.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct WatchBody {
    /// The principal to own it: `user:<login>` or `team:<slug>` the caller acts as; absent means
    /// the caller.
    owner: Option<String>,
    /// The registry id every launch runs. Watches target adopted revisions only.
    playbook: String,
    /// The tracker to sweep, from `GET /api/watches/trackers`.
    tracker: String,
    /// The query in the tracker's native language (JQL for Jira). Ordering is the sweep's.
    query: String,
    /// The playbook param the matching item's identifier is passed as. Must be a declared
    /// string param; it is overlaid on `params` at each launch.
    key_param: String,
    /// The remaining param values, validated against the pack's stored schema and frozen.
    #[serde(default)]
    params: std::collections::BTreeMap<String, String>,
    max_cost: f64,
    max_time: String,
    /// The schema digest the form was rendered against, refused when the pack has moved on.
    #[serde(default)]
    schema_digest: Option<String>,
    /// An explicit watermark, RFC 3339: items updated at or after it launch. Absent starts at now
    /// on create and keeps the stored watermark on replace.
    #[serde(default)]
    since: Option<String>,
    /// Absent ⇒ enabled.
    #[serde(default = "yes")]
    enabled: bool,
    /// Which cluster every launch dispatches onto, from `GET /api/dispatch-targets`. Absent
    /// selects the controller's configured default.
    #[serde(default)]
    dispatch_target: Option<String>,
    /// Which registered inference provider every launch's agent runs against. Absent resolves
    /// through the configured defaults at dispatch.
    #[serde(default)]
    provider: Option<String>,
    /// The model to ask that provider for. Free text; absent takes the provider's own default.
    #[serde(default)]
    model: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct WatchEnabledBody {
    enabled: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct WatchPreviewBody {
    tracker: String,
    query: String,
    /// Count items updated at or after this RFC 3339 instant; absent counts the whole query.
    #[serde(default)]
    since: Option<String>,
}

/// What a query matches right now, so a creator sees what a watch would sweep before saving it.
#[derive(Debug, Serialize, ToSchema)]
pub struct WatchPreviewDto {
    pub tracker: String,
    pub query: String,
    /// How many items match, at or after `since` when given.
    pub matched: usize,
    /// The first matching item identifiers, oldest-updated first (at most 25).
    pub sample: Vec<String>,
}

/// The trackers a watch may name here.
#[derive(Debug, Serialize, ToSchema)]
pub struct TrackersDto {
    pub trackers: Vec<String>,
}

const PREVIEW_SAMPLE: usize = 25;

fn field_error(field: &str, message: impl Into<String>) -> crate::playbooks::registry::FieldError {
    crate::playbooks::registry::FieldError {
        field: field.to_string(),
        message: message.into(),
    }
}

/// Resolve the tracker a body names against what this deployment has credentials for.
#[allow(clippy::result_large_err)]
fn tracker_for(
    state: &ApiState,
    name: &str,
) -> Result<
    (
        TrackerKind,
        std::sync::Arc<dyn crate::launches::tracker::TrackerSearch>,
    ),
    Response,
> {
    let kind = TrackerKind::parse(name)
        .map_err(|e| invalid_fields(vec![field_error("tracker", e.to_string())]))?;
    let trackers = state.trackers();
    match trackers.get(kind) {
        Some(tracker) => Ok((kind, tracker.clone())),
        None => Err(invalid_fields(vec![field_error(
            "tracker",
            format!("this controller has no {name} credentials configured"),
        )])),
    }
}

#[allow(clippy::result_large_err)]
fn check_since(since: Option<&str>) -> Result<Option<String>, Response> {
    let Some(raw) = since else {
        return Ok(None);
    };
    let ts: jiff::Timestamp = raw
        .parse()
        .map_err(|e| invalid_fields(vec![field_error("since", format!("not RFC 3339: {e}"))]))?;
    Ok(Some(ts.to_string()))
}

/// The pack's schema with the key param taken off `required`, so the stored params (everything
/// but the key) validate now while the key itself is overlaid per item at launch. The key must be
/// a declared string param: a pack that does not declare it refuses the watch now rather than at
/// its first hit.
fn schema_without_key(
    schema: &serde_json::Value,
    key_param: &str,
) -> Result<serde_json::Value, crate::playbooks::registry::FieldError> {
    let declared = schema
        .pointer(&format!("/properties/{key_param}"))
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str());
    match declared {
        Some("string") => {}
        Some(other) => {
            return Err(field_error(
                "key_param",
                format!("{key_param} is declared as {other}, not a string"),
            ));
        }
        None => {
            return Err(field_error(
                "key_param",
                format!("the pack declares no param named {key_param}"),
            ));
        }
    }
    let mut relaxed = schema.clone();
    if let Some(required) = relaxed.get_mut("required").and_then(|r| r.as_array_mut()) {
        required.retain(|r| r.as_str() != Some(key_param));
    }
    Ok(relaxed)
}

/// Validate a watch body: the trigger half here, the launch half through the same
/// [`authorize_pack`] the form POST and the schedules use.
#[allow(clippy::result_large_err)]
async fn authorize_watch(
    state: &ApiState,
    caller: &crate::authz::Caller,
    body: &WatchBody,
) -> Result<(AuthorizedLaunch, TrackerKind, Option<String>), Response> {
    let (kind, tracker) = tracker_for(state, &body.tracker)?;
    tracker
        .check_query(&body.query)
        .map_err(|e| invalid_fields(vec![field_error("query", e.to_string())]))?;
    let since = check_since(body.since.as_deref())?;
    if body.params.contains_key(&body.key_param) {
        return Err(invalid_fields(vec![field_error(
            "key_param",
            "is also given a fixed value in params; the sweep overlays it per item",
        )]));
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
    let relaxed =
        schema_without_key(&schema, &body.key_param).map_err(|f| invalid_fields(vec![f]))?;
    let params = crate::playbooks::registry::validate_params(&relaxed, &body.params)
        .map_err(invalid_fields)?;
    Ok((
        AuthorizedLaunch {
            pack,
            params,
            max_cost: body.max_cost,
            max_time,
        },
        kind,
        since,
    ))
}

/// Watch a tracker query: validate it now, exactly as an immediate launch is validated, and store
/// it with the query the sweep runs. Every hit inserts an ordinary launch row.
#[utoipa::path(
    post,
    path = "/api/watches",
    request_body = WatchBody,
    responses(
        (status = 201, description = "Watch stored; it sweeps on the next discovery tick", body = WatchDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No playbook with that id", body = ErrorBody),
        (status = 409, description = "The form was rendered against a schema this playbook no longer serves", body = ErrorBody),
        (status = 422, description = "Parameter values, ceilings, the tracker, or the query were refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn create_watch(
    State(state): State<ApiState>,
    groups: crate::identity::auth::Groups,
    caller: crate::authz::Caller,
    Json(body): Json<WatchBody>,
) -> Response {
    let (authorized, kind, since) = match authorize_watch(&state, &caller, &body).await {
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
            agent: authorized.pack.agent.clone(),
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
        crate::authz::action::ResourceType::StandingLaunch,
        &crate::authz::model::Principal::stored(saver.owner_principal.as_deref()),
    )
    .await
    {
        return denied.into_response();
    }
    if let Err(denied) =
        crate::playbooks::api::registry::decide_launch(&state, &caller, &authorized.pack).await
    {
        return denied;
    }
    let actor = saver.actor.as_deref();
    let stored = crate::launches::watches::create(
        state.db.pool(),
        &crate::launches::watches::NewWatch {
            standing: crate::launches::standing::NewStanding {
                playbook: &authorized.pack.id,
                target_kind: "adopted",
                eligible_draft_version: None,
                params: &authorized.params,
                schema_digest: &authorized.pack.schema_digest,
                max_cost: authorized.max_cost,
                max_time: &authorized.max_time,
                advance_dedupe: false,
                enabled: body.enabled,
                created_by: actor,
                owner_principal: saver.owner_principal.as_deref(),
                owner_groups: Some(&saver.groups),
                dispatch_target: Some(&saver.dispatch_target),
                agent_provider: saver.provider.as_deref(),
                agent_model: saver.model.as_deref(),
            },
            tracker: kind,
            query: &body.query,
            key_param: &body.key_param,
            since: since.as_deref(),
        },
        jiff::Timestamp::now(),
    )
    .await;
    let stored = match stored {
        Ok(w) => w,
        Err(e) => return AppError::from(e).into_response(),
    };
    let audit_reason = format!(
        "playbook {} watching {} {:?} from {} as revision {}",
        stored.playbook,
        stored.tracker,
        stored.query,
        stored.watermark,
        stored.adopted_rev.as_deref().unwrap_or("?")
    );
    if let Err(e) = state
        .audit_required(
            crate::event_log::Event::now(
                &crate::model::Trigger::Watch.event_key(&stored.id),
                "new",
                if stored.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                Some(&audit_reason),
                None,
            )
            .by(actor),
        )
        .await
    {
        return AppError::from(e).into_response();
    }
    (StatusCode::CREATED, Json(WatchDto::from(stored))).into_response()
}

/// `GET /api/watches` — every watch, enabled first.
#[utoipa::path(
    get,
    path = "/api/watches",
    responses((status = 200, description = "Tracker watches", body = Vec<WatchDto>))
)]
pub(crate) async fn list_watches(
    State(state): State<ApiState>,
) -> Result<Json<Vec<WatchDto>>, AppError> {
    let rows =
        crate::launches::watches::list(state.db.pool(), crate::launches::watches::LIST_LIMIT)
            .await?;
    Ok(Json(rows.into_iter().map(WatchDto::from).collect()))
}

/// `GET /api/watches/trackers` — the trackers a watch may name on this controller.
#[utoipa::path(
    get,
    path = "/api/watches/trackers",
    responses((status = 200, description = "Configured trackers", body = TrackersDto))
)]
pub(crate) async fn list_watch_trackers(State(state): State<ApiState>) -> Json<TrackersDto> {
    Json(TrackersDto {
        trackers: state
            .trackers()
            .configured()
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect(),
    })
}

/// `GET /api/watches/{id}` — one watch, the source an edit form prefills from.
#[utoipa::path(
    get,
    path = "/api/watches/{id}",
    params(("id" = String, Path, description = "Watch id")),
    responses(
        (status = 200, description = "The watch", body = WatchDto),
        (status = 404, description = "No watch with that id", body = ErrorBody)
    )
)]
pub(crate) async fn get_watch(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    match crate::launches::watches::get(state.db.pool(), &id).await? {
        Some(row) => Ok(Json(WatchDto::from(row)).into_response()),
        None => Ok(not_found(format!("no watch {id:?}"))),
    }
}

/// Replace a watch: the values, the ceilings, the query, and whether it is enabled. The failure
/// count starts over, so re-enabling an auto-disabled watch is an ordinary PUT. The watermark stays
/// where the sweep left it unless `since` is given.
#[utoipa::path(
    put,
    path = "/api/watches/{id}",
    params(("id" = String, Path, description = "Watch id")),
    request_body = WatchBody,
    responses(
        (status = 200, description = "The watch as stored", body = WatchDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No watch (or no playbook) with that id", body = ErrorBody),
        (status = 409, description = "The form was rendered against a schema this playbook no longer serves", body = ErrorBody),
        (status = 422, description = "Parameter values, ceilings, the tracker, or the query were refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn update_watch(
    State(state): State<ApiState>,
    groups: crate::identity::auth::Groups,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<WatchBody>,
) -> Response {
    let snapshot = match crate::launches::api::authorize_standing(
        &state,
        &caller,
        &id,
        crate::authz::action::Verb::Update,
        &format!("no watch {id:?}"),
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(refusal) => return refusal,
    };
    let prior = match crate::launches::watches::get(state.db.pool(), &id).await {
        Ok(row) => row,
        Err(e) => return AppError::from(e).into_response(),
    };
    let (authorized, kind, since) = match authorize_watch(&state, &caller, &body).await {
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
            agent: authorized.pack.agent.clone(),
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
    let firing_changed = prior.as_ref().is_none_or(|p| {
        p.playbook != authorized.pack.id
            || p.params != authorized.params
            || p.dispatch_target.as_deref() != Some(saver.dispatch_target.as_str())
            || p.agent_provider != saver.provider
            || p.agent_model != saver.model
    });
    if firing_changed
        && let Err(denied) =
            crate::playbooks::api::registry::decide_launch(&state, &caller, &authorized.pack).await
    {
        return denied;
    }
    let actor = saver.actor.as_deref();
    let stored = crate::launches::watches::update(
        state.db.pool(),
        &id,
        &crate::launches::watches::NewWatch {
            standing: crate::launches::standing::NewStanding {
                playbook: &authorized.pack.id,
                target_kind: "adopted",
                eligible_draft_version: None,
                params: &authorized.params,
                schema_digest: &authorized.pack.schema_digest,
                max_cost: authorized.max_cost,
                max_time: &authorized.max_time,
                advance_dedupe: false,
                enabled: body.enabled,
                created_by: actor,
                owner_principal: saver.owner_principal.as_deref(),
                owner_groups: Some(&saver.groups),
                dispatch_target: Some(&saver.dispatch_target),
                agent_provider: saver.provider.as_deref(),
                agent_model: saver.model.as_deref(),
            },
            tracker: kind,
            query: &body.query,
            key_param: &body.key_param,
            since: since.as_deref(),
        },
    )
    .await;
    let stored = match stored {
        Ok(Some(w)) => w,
        Ok(None) => return not_found(format!("no watch {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    };
    // The re-save re-owned the row, so the launches that parked under the old snapshot will never
    // run.
    match crate::launches::watches::expire_stale_owner_parks(state.db.pool(), &id).await {
        Ok(expired) if !expired.is_empty() => {
            tracing::info!(watch = %id, expired = expired.len(), "watches: re-save expired stale-owner parks");
        }
        Ok(_) => {}
        Err(e) => return AppError::from(e).into_response(),
    }
    let audit_reason = format!(
        "playbook {} rewatching {} {:?} from {}",
        stored.playbook, stored.tracker, stored.query, stored.watermark
    );
    if let Err(e) = state
        .audit_required(
            crate::event_log::Event::now(
                &crate::model::Trigger::Watch.event_key(&id),
                "edited",
                if stored.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                Some(&audit_reason),
                None,
            )
            .by(actor),
        )
        .await
    {
        return AppError::from(e).into_response();
    }
    Json(WatchDto::from(stored)).into_response()
}

/// Delete a watch so it stops sweeping. The launches it already made are ordinary runs and stay.
#[utoipa::path(
    delete,
    path = "/api/watches/{id}",
    params(("id" = String, Path, description = "Watch id")),
    responses(
        (status = 204, description = "Deleted; it will not sweep again"),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No watch with that id", body = ErrorBody)
    )
)]
pub(crate) async fn delete_watch(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Response {
    if let Err(refusal) = crate::launches::api::authorize_standing(
        &state,
        &caller,
        &id,
        crate::authz::action::Verb::Delete,
        &format!("no watch {id:?}"),
    )
    .await
    {
        return refusal;
    }
    match crate::launches::watches::get(state.db.pool(), &id).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(format!("no watch {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    }
    match crate::launches::watches::delete(state.db.pool(), &id).await {
        Ok(true) => {}
        Ok(false) => return not_found(format!("no watch {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    }
    state
        .audit(
            crate::event_log::Event::now(
                &crate::model::Trigger::Watch.event_key(&id),
                "enabled",
                "deleted",
                Some("watch deleted"),
                None,
            )
            .by(identity.as_deref()),
            "delete_watch",
        )
        .await;
    StatusCode::NO_CONTENT.into_response()
}

/// Enable or disable a watch without re-authorizing it. Enabling clears the failure count.
#[utoipa::path(
    post,
    path = "/api/watches/{id}/enabled",
    params(("id" = String, Path, description = "Watch id")),
    request_body = WatchEnabledBody,
    responses(
        (status = 200, description = "The watch as stored", body = WatchDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No watch with that id", body = ErrorBody)
    )
)]
pub(crate) async fn set_watch_enabled(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<WatchEnabledBody>,
) -> Response {
    if let Err(refusal) = crate::launches::api::authorize_standing(
        &state,
        &caller,
        &id,
        crate::authz::action::Verb::Update,
        &format!("no watch {id:?}"),
    )
    .await
    {
        return refusal;
    }
    let prior =
        match crate::launches::watches::set_enabled(state.db.pool(), &id, body.enabled).await {
            Ok(Some(prior)) => prior,
            Ok(None) => return not_found(format!("no watch {id:?}")),
            Err(e) => return AppError::from(e).into_response(),
        };
    let state_of = |on: bool| if on { "enabled" } else { "disabled" };
    if prior != body.enabled {
        state
            .audit(
                crate::event_log::Event::now(
                    &crate::model::Trigger::Watch.event_key(&id),
                    state_of(prior),
                    state_of(body.enabled),
                    Some("by request"),
                    None,
                )
                .by(identity.as_deref()),
                "set_watch_enabled",
            )
            .await;
    }
    match crate::launches::watches::get(state.db.pool(), &id).await {
        Ok(Some(w)) => Json(WatchDto::from(w)).into_response(),
        Ok(None) => not_found(format!("no watch {id:?}")),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// `GET /api/watches/{id}/hits` — the items a watch has launched, newest first.
#[utoipa::path(
    get,
    path = "/api/watches/{id}/hits",
    params(("id" = String, Path, description = "Watch id")),
    responses(
        (status = 200, description = "Launched items", body = Vec<WatchHitDto>),
        (status = 404, description = "No watch with that id", body = ErrorBody)
    )
)]
pub(crate) async fn list_watch_hits(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    if crate::launches::watches::get(state.db.pool(), &id)
        .await?
        .is_none()
    {
        return Ok(not_found(format!("no watch {id:?}")));
    }
    let hits =
        crate::launches::watches::hits(state.db.pool(), &id, crate::launches::watches::LIST_LIMIT)
            .await?;
    Ok(Json(hits.into_iter().map(WatchHitDto::from).collect::<Vec<_>>()).into_response())
}

/// Forget that a watch launched an item, so the next sweep that finds it launches it again. This
/// is the needs-info round trip: amend the item, reset it here, and the watch picks it up.
#[utoipa::path(
    delete,
    path = "/api/watches/{id}/hits/{item}",
    params(
        ("id" = String, Path, description = "Watch id"),
        ("item" = String, Path, description = "The tracker's item identifier (`PROJ-123`)")
    ),
    responses(
        (status = 204, description = "Reset; the item is unseen again"),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No watch with that id, or it never launched that item", body = ErrorBody)
    )
)]
pub(crate) async fn reset_watch_hit(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path((id, item)): Path<(String, String)>,
) -> Response {
    if let Err(refusal) = crate::launches::api::authorize_standing(
        &state,
        &caller,
        &id,
        crate::authz::action::Verb::Update,
        &format!("no watch {id:?}"),
    )
    .await
    {
        return refusal;
    }
    match crate::launches::watches::reset_hit(state.db.pool(), &id, &item).await {
        Ok(true) => {}
        Ok(false) => return not_found(format!("watch {id:?} never launched {item:?}")),
        Err(e) => return AppError::from(e).into_response(),
    }
    let note = format!("{item} reset; the next sweep may launch it again");
    state
        .audit(
            crate::event_log::Event::now(
                &crate::model::Trigger::Watch.event_key(&id),
                "hit",
                "reset",
                Some(&note),
                Some(&item),
            )
            .by(identity.as_deref()),
            "reset_watch_hit",
        )
        .await;
    StatusCode::NO_CONTENT.into_response()
}

/// What a query matches right now, computed against the real tracker, so a creator sees what the
/// watch would sweep before saving it.
#[utoipa::path(
    post,
    path = "/api/watches/preview",
    request_body = WatchPreviewBody,
    responses(
        (status = 200, description = "The matching items", body = WatchPreviewDto),
        (status = 422, description = "The tracker or the query was refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn preview_watch(
    State(state): State<ApiState>,
    Json(body): Json<WatchPreviewBody>,
) -> Response {
    let (kind, tracker) = match tracker_for(&state, &body.tracker) {
        Ok(t) => t,
        Err(refusal) => return refusal,
    };
    if let Err(e) = tracker.check_query(&body.query) {
        return invalid_fields(vec![field_error("query", e.to_string())]);
    }
    let since = match check_since(body.since.as_deref()) {
        Ok(s) => s,
        Err(refusal) => return refusal,
    };
    let hits = match tracker.search(&body.query, since.as_deref()).await {
        Ok(hits) => hits,
        Err(e) => return invalid_fields(vec![field_error("query", format!("{e:#}"))]),
    };
    Json(WatchPreviewDto {
        tracker: kind.as_str().to_string(),
        query: body.query,
        matched: hits.len(),
        sample: hits
            .iter()
            .take(PREVIEW_SAMPLE)
            .map(|h| h.id.clone())
            .collect(),
    })
    .into_response()
}
