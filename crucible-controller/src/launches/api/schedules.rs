use crate::api::dto::*;
use crate::dto::dto;

use crate::api::state::*;

use crate::launches::schedules::Schedule;

use crate::clock::stamp;
use crate::launches::schedules::cron::CronSpec;

use crate::playbooks::api::registry::{
    AuthorizedLaunch, UNREADABLE_MANIFEST, authorize_launch, check_ceilings, undispatchable,
};

use axum::extract::{Path, State};

use axum::http::StatusCode;

use axum::response::{IntoResponse, Response};

use serde::{Deserialize, Serialize};

use utoipa::ToSchema;

dto! {
    /// One recurring schedule: what it launches, when it recurs, and how its last firings went.
    pub struct ScheduleDto: From<s: Schedule> {
        pub id: String,
        pub playbook: String,
        /// `adopted` is an immutable Git revision; `draft_head` follows successful draft saves only
        /// when the administrator enables that development policy.
        pub target_kind: String,
        pub adopted_repo: Option<String>,
        pub adopted_path: Option<String>,
        pub adopted_rev: Option<String>,
        pub eligible_draft_version: Option<i64>,
        /// The values every firing launches.
        #[schema(value_type = Object)]
        pub params: serde_json::Value,
        pub schema_digest: String,
        pub max_cost: f64,
        pub max_time: String,
        /// Whether a firing may advance the dedupe state.
        pub advance_dedupe: bool,
        /// The recurring-input cursor, when this schedule declares one.
        pub cursor: Option<CursorDto> = s.cursor.clone().map(CursorDto::from),
        /// What the last successful firing left a result-path cursor at; null until one lands, and
        /// then the next firing passes it as `cursor.param`. Read-only: only a completed run writes
        /// it. Null on a run-file cursor, whose value is described by `cursor_file` instead.
        pub cursor_value: Option<String> = match &s.cursor {
            Some(crate::launches::model::CursorSpec::File { .. }) => None,
            _ => s.cursor_value.clone(),
        },
        /// The stored file of a run-file cursor: its size and digest, not its body. Null until a
        /// firing lands one.
        pub cursor_file: Option<CursorFileDto> = match &s.cursor {
            Some(crate::launches::model::CursorSpec::File { .. }) => {
                s.cursor_value.as_deref().map(CursorFileDto::of)
            }
            _ => None,
        },
        pub cursor_updated_at: Option<String>,
        pub cron_expr: String,
        /// The IANA zone the expression is read in.
        pub tz: String,
        pub enabled: bool,
        /// When it fires next, UTC. Null while disabled, while a sweep holds it claimed, or when the
        /// expression has no further occurrence.
        pub next_due_at: Option<String>,
        pub last_fired_at: Option<String>,
        /// Firings that did not launch since the last one that did.
        pub consecutive_failures: i64,
        pub created_by: Option<String>,
        /// The principal this schedule's firings launch under, and when its group snapshot was last
        /// taken. Null on a schedule saved before ownership was recorded, which parks any firing whose
        /// scope binds secrets.
        pub owner_principal: Option<String>,
        pub owner_groups_at: Option<String>,
        /// The fire-time group refresh, for the schedules view: whether the owner has to sign in again
        /// before this schedule can fire, why the last refresh did not land (transient failures
        /// included), and when it was tried.
        pub owner_signin_required: bool,
        pub owner_refresh_error: Option<String>,
        pub owner_refresh_at: Option<String>,
        /// The inference provider every firing pins on its issue; null resolves through the configured
        /// defaults at dispatch.
        pub agent_provider: Option<String>,
        /// The model pinned alongside it; null takes the resolved provider's default.
        pub agent_model: Option<String>,
        pub created_at: String,
        pub updated_at: String,
    }
}

dto! {
    /// A schedule's cursor: what a finished firing leaves behind and how the next firing receives
    /// it. A `$.task.field` result path is passed as `param`; the pack opts in by declaring that
    /// param with an empty default. A `task/file` run-file key is written into the pack at `path`
    /// before the run starts; the pack ships its own default at that path and declares it as a
    /// `[[workspace.inject]]`. A pack that ignores either behaves exactly as it does with no cursor.
    #[derive(Clone, Deserialize)]
    pub struct CursorDto: From<c: crate::launches::model::CursorSpec> {
        /// A dotted path into the run result (`$.scan.newest_created_at`), or the key of a captured
        /// file as `GET /api/runs/{run_id}/files` lists it (`rollup/STATE.json`).
        pub from: String = c.source().to_string(),
        /// The param a result-path cursor is passed as. Refused on a run-file cursor.
        #[serde(default)]
        pub param: Option<String> = c.param().map(str::to_string),
        /// The pack-relative path a run-file cursor is written to. Refused on a result-path cursor.
        #[serde(default)]
        pub path: Option<String> = c.path().map(str::to_string),
    }
}

/// The stored file of a run-file cursor, described without its body.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CursorFileDto {
    pub bytes: u64,
    /// `sha256:<hex>` of the stored bytes.
    pub digest: String,
}

impl CursorFileDto {
    fn of(value: &str) -> Self {
        CursorFileDto {
            bytes: value.len() as u64,
            digest: crate::launches::schedules::content_digest(value.as_bytes()),
        }
    }
}

/// A schedule's recurrence and the authorization it launches with. The same body creates a
/// schedule and replaces one: a PUT is the whole schedule, so an edit can never leave half the
/// old authorization behind.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ScheduleBody {
    /// The principal to own it: `user:<login>` or `team:<slug>` the caller acts as; absent means
    /// the caller.
    owner: Option<String>,
    /// The registry id every firing launches.
    playbook: String,
    /// `adopted` freezes the registered Git revision; `draft_head` is the development lane and
    /// resolves the latest successful save at each firing.
    #[serde(default = "adopted_target")]
    target_kind: String,
    /// The param values, validated against the pack's stored schema and frozen.
    #[serde(default)]
    params: std::collections::BTreeMap<String, String>,
    max_cost: f64,
    max_time: String,
    /// A five-field cron expression (`0 6 * * MON-FRI`).
    cron_expr: String,
    /// The IANA zone the expression is read in. Absent ⇒ `UTC`.
    #[serde(default = "utc")]
    tz: String,
    /// The schema digest the form was rendered against, refused when the pack has moved on.
    #[serde(default)]
    schema_digest: Option<String>,
    /// Whether a firing advances the dedupe state. Absent ⇒ true: a schedule is the surface that
    /// state belongs to.
    #[serde(default = "yes")]
    advance_dedupe: bool,
    /// Absent ⇒ enabled.
    #[serde(default = "yes")]
    enabled: bool,
    /// Which cluster every firing dispatches onto, from `GET /api/dispatch-targets`. Absent
    /// selects the controller's configured default.
    #[serde(default)]
    dispatch_target: Option<String>,
    /// Which registered inference provider every firing's agent runs against, from
    /// `GET /api/config/providers`. Absent resolves through the configured defaults at dispatch.
    #[serde(default)]
    provider: Option<String>,
    /// The model to ask that provider for. Free text; absent takes the provider's own default.
    #[serde(default)]
    model: Option<String>,
    /// The recurring-input cursor. Absent leaves every firing launching `params` unchanged;
    /// changing it clears the stored value, since a different field is a different cursor.
    #[serde(default)]
    cursor: Option<CursorDto>,
}

fn utc() -> String {
    "UTC".to_string()
}

fn adopted_target() -> String {
    "adopted".to_string()
}

fn yes() -> bool {
    true
}

/// How many firings a preview returns by default, and the most it will return.
const PREVIEW_DEFAULT: usize = 5;

const PREVIEW_MAX: usize = 25;

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct SchedulePreviewBody {
    cron_expr: String,
    #[serde(default = "utc")]
    tz: String,
    /// How many firings to return (default 5, capped at 25).
    #[serde(default)]
    count: Option<usize>,
}

/// The next firings of an expression, as UTC instants.
#[derive(Debug, Serialize, ToSchema)]
pub struct SchedulePreviewDto {
    pub cron_expr: String,
    pub tz: String,
    /// The next firings, UTC, soonest first. Empty when the expression matches nothing ahead.
    pub firings: Vec<String>,
}

/// Validate a schedule body: the cron half here, the launch half through the same
/// [`authorize_launch`] the form POST and the one-shots use.
#[allow(clippy::result_large_err)]
async fn authorize_schedule(
    state: &ApiState,
    caller: &crate::authz::Caller,
    body: &ScheduleBody,
) -> Result<
    (
        AuthorizedLaunch,
        CronSpec,
        Option<crate::launches::model::CursorSpec>,
        Option<i64>,
    ),
    Response,
> {
    let spec = CronSpec::parse(&body.cron_expr, &body.tz).map_err(|e| invalid_fields(vec![e]))?;
    let cursor = body
        .cursor
        .as_ref()
        .map(|c| {
            crate::launches::model::CursorSpec::parse(
                &c.from,
                c.param.as_deref(),
                c.path.as_deref(),
            )
        })
        .transpose()
        .map_err(|(field, message)| {
            invalid_fields(vec![crate::playbooks::registry::FieldError {
                field: field.to_string(),
                message,
            }])
        })?;
    if body.target_kind == "adopted" {
        let authorized = authorize_launch(
            state,
            caller,
            &body.playbook,
            &body.params,
            body.max_cost,
            &body.max_time,
            body.schema_digest.as_deref(),
        )
        .await?;
        return Ok((authorized, spec, cursor, None));
    }
    if body.target_kind != "draft_head" {
        return Err(invalid_fields(vec![
            crate::playbooks::registry::FieldError {
                field: "target_kind".to_string(),
                message: "must be adopted or draft_head".to_string(),
            },
        ]));
    }
    let allowed = state
        .config
        .as_ref()
        .is_some_and(|store| store.effective().allow_draft_head_schedules);
    if !allowed {
        return Err(invalid_fields(vec![
            crate::playbooks::registry::FieldError {
                field: "target_kind".to_string(),
                message: "draft-head schedules are disabled by administrator policy".to_string(),
            },
        ]));
    }
    let draft = match crate::playbooks::drafts::get(state.db.pool(), &body.playbook).await {
        Ok(Some(d)) if d.retired_at.is_none() => d,
        Ok(Some(_)) => return Err(conflict("the draft retired after graduation")),
        Ok(None) => return Err(not_found(format!("no draft {:?}", body.playbook))),
        Err(e) => return Err(AppError::from(e).into_response()),
    };
    let latest = match crate::playbooks::drafts::latest(state.db.pool(), &body.playbook).await {
        Ok(Some(v)) => v,
        Ok(None) => return Err(not_found(format!("no draft {:?}", body.playbook))),
        Err(e) => return Err(AppError::from(e).into_response()),
    };
    let (Some(schema), Some(schema_digest)) = (latest.params_schema, latest.schema_digest) else {
        return Err(invalid_fields(vec![
            crate::playbooks::registry::FieldError {
                field: "playbook".to_string(),
                message: format!("draft version {} did not compile", latest.version),
            },
        ]));
    };
    if let Some(asked) = body.schema_digest.as_deref()
        && asked != schema_digest
    {
        return Err(conflict("the draft schema changed; reload the form"));
    }
    if let Some(refusal) = match latest.agent.as_ref() {
        Some(agent) => state.dispatch.refusal(agent),
        None => Some(UNREADABLE_MANIFEST.to_string()),
    } {
        return Err(undispatchable(refusal));
    }
    let max_time = check_ceilings(&state.playbook_caps, body.max_cost, &body.max_time)
        .map_err(|field| invalid_fields(vec![field]))?;
    let params = crate::playbooks::registry::validate_params(&schema, &body.params)
        .map_err(invalid_fields)?;
    let version = latest.version;
    let pack = crate::playbooks::registry::PlaybookRow {
        exposure_digest: None,
        id: draft.id.clone(),
        description: draft.description,
        source: match draft.graduation_repo {
            Some(repo) => crate::playbooks::registry::PlaybookSource::Git {
                repo,
                git_ref: None,
                path: draft.graduation_path.unwrap_or_default(),
            },
            None => crate::playbooks::registry::PlaybookSource::Draft {
                draft: draft.id.clone(),
                version,
            },
        },
        rev: format!("draft-v{version}"),
        tar_digest: String::new(),
        schema_digest,
        agent: latest.agent,
        core_rev: crate::playbooks::registry::core_rev().unwrap_or_default(),
        owner: draft.owner,
        created_by: draft.created_by,
        created_at: draft.created_at,
        updated_at: draft.updated_at,
    };
    Ok((
        AuthorizedLaunch {
            pack,
            params,
            max_cost: body.max_cost,
            max_time,
        },
        spec,
        cursor,
        Some(version),
    ))
}

/// Schedule a playbook: validate it now, exactly as an immediate launch is validated, and store it
/// with the expression the sweep fires it on. Every firing inserts an ordinary launch row.
#[utoipa::path(
    post,
    path = "/api/schedules",
    request_body = ScheduleBody,
    responses(
        (status = 201, description = "Schedule stored; it fires on its next due window", body = ScheduleDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No playbook with that id", body = ErrorBody),
        (status = 409, description = "The form was rendered against a schema this playbook no longer serves", body = ErrorBody),
        (status = 422, description = "Parameter values, ceilings, or the cron expression were refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn create_schedule(
    State(state): State<ApiState>,
    groups: crate::identity::auth::Groups,
    caller: crate::authz::Caller,
    Json(body): Json<ScheduleBody>,
) -> Response {
    let _policy_operation = match state.config.as_ref() {
        Some(store) => Some(store.recurring_policy_read().await),
        None => None,
    };
    let (authorized, spec, cursor, draft_version) =
        match authorize_schedule(&state, &caller, &body).await {
            Ok(ok) => ok,
            Err(refusal) => return refusal,
        };
    // The target is resolved against the saver, for the same reason the groups are snapshotted: a
    // firing has no session, so this save is the only authorization its dispatch will ever have.
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
    if let Err(refusal) = crate::playbooks::api::registry::authorize_image(
        &state,
        authorized.pack.agent.as_ref(),
        saver.provider.as_deref(),
        authorized.pack.source.repo(),
    )
    .await
    {
        return refusal;
    }
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
    let stored = state
        .schedules
        .create(
            &crate::launches::schedules::NewSchedule {
                standing: crate::launches::standing::NewStanding {
                    playbook: &authorized.pack.id,
                    target_kind: &body.target_kind,
                    eligible_draft_version: draft_version,
                    params: &authorized.params,
                    schema_digest: &authorized.pack.schema_digest,
                    max_cost: authorized.max_cost,
                    max_time: &authorized.max_time,
                    advance_dedupe: body.advance_dedupe,
                    enabled: body.enabled,
                    created_by: actor,
                    owner_principal: saver.owner_principal.as_deref(),
                    owner_groups: Some(&saver.groups),
                    dispatch_target: Some(&saver.dispatch_target),
                    agent_provider: saver.provider.as_deref(),
                    agent_model: saver.model.as_deref(),
                },
                cursor: cursor.as_ref(),
                spec: &spec,
            },
            jiff::Timestamp::now(),
        )
        .await;
    let stored = match stored {
        Ok(s) => s,
        Err(e) => return AppError::from(e).into_response(),
    };

    let audit_key = format!("schedule:{}", stored.id);
    let audit_reason = format!(
        "playbook {} scheduled on {:?} ({}) as {} {}",
        stored.playbook,
        stored.cron_expr,
        stored.tz,
        stored.target_kind,
        stored
            .adopted_rev
            .as_deref()
            .map(|rev| format!("revision {rev}"))
            .unwrap_or_else(|| format!(
                "draft version {}",
                stored.eligible_draft_version.unwrap_or_default()
            ))
    );
    if let Err(e) = state
        .audit_required(
            crate::event_log::Event::now(
                &audit_key,
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

    (StatusCode::CREATED, Json(ScheduleDto::from(stored))).into_response()
}

/// `GET /api/schedules` — every schedule, enabled and soonest-due first.
#[utoipa::path(
    get,
    path = "/api/schedules",
    responses((status = 200, description = "Playbook schedules", body = Vec<ScheduleDto>))
)]
pub(crate) async fn list_schedules(
    State(state): State<ApiState>,
) -> Result<Json<Vec<ScheduleDto>>, AppError> {
    let rows = state
        .schedules
        .list(crate::launches::schedules::LIST_LIMIT)
        .await?;
    Ok(Json(rows.into_iter().map(ScheduleDto::from).collect()))
}

/// `GET /api/schedules/{id}` — one schedule, the source an edit form prefills from.
#[utoipa::path(
    get,
    path = "/api/schedules/{id}",
    params(("id" = String, Path, description = "Schedule id")),
    responses(
        (status = 200, description = "The schedule", body = ScheduleDto),
        (status = 404, description = "No schedule with that id", body = ErrorBody)
    )
)]
pub(crate) async fn get_schedule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    match state.schedules.get(&id).await? {
        Some(row) => Ok(Json(ScheduleDto::from(row)).into_response()),
        None => Ok(not_found(format!("no schedule {id:?}"))),
    }
}

/// Replace a schedule: the values, the ceilings, the expression, and whether it is enabled. The
/// next firing is recomputed from now and the failure count starts over, so re-enabling an
/// auto-disabled schedule is an ordinary PUT.
#[utoipa::path(
    put,
    path = "/api/schedules/{id}",
    params(("id" = String, Path, description = "Schedule id")),
    request_body = ScheduleBody,
    responses(
        (status = 200, description = "The schedule as stored", body = ScheduleDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No schedule (or no playbook) with that id", body = ErrorBody),
        (status = 409, description = "The form was rendered against a schema this playbook no longer serves", body = ErrorBody),
        (status = 422, description = "Parameter values, ceilings, or the cron expression were refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn update_schedule(
    State(state): State<ApiState>,
    groups: crate::identity::auth::Groups,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<ScheduleBody>,
) -> Response {
    let _policy_operation = match state.config.as_ref() {
        Some(store) => Some(store.recurring_policy_read().await),
        None => None,
    };
    if body.target_kind == "draft_head"
        && body.enabled
        && !state
            .config
            .as_ref()
            .is_some_and(|store| store.effective().allow_draft_head_schedules)
        && matches!(
            state.schedules.get(&id).await,
            Ok(Some(crate::launches::schedules::Schedule { target_kind, .. })) if target_kind == "draft_head"
        )
    {
        return conflict("cannot enable a draft-head schedule while adopted-only policy is active");
    }
    let snapshot = match crate::launches::api::authorize_standing(
        &state,
        &caller,
        &id,
        crate::authz::action::Verb::Update,
        &format!("no schedule {id:?}"),
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(refusal) => return refusal,
    };
    let prior = match state.schedules.get(&id).await {
        Ok(row) => row,
        Err(e) => return AppError::from(e).into_response(),
    };
    let (authorized, spec, cursor, draft_version) =
        match authorize_schedule(&state, &caller, &body).await {
            Ok(ok) => ok,
            Err(refusal) => return refusal,
        };
    // The target is resolved against the saver, for the same reason the groups are snapshotted: a
    // firing has no session, so this save is the only authorization its dispatch will ever have.
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
    if let Err(refusal) = crate::playbooks::api::registry::authorize_image(
        &state,
        authorized.pack.agent.as_ref(),
        saver.provider.as_deref(),
        authorized.pack.source.repo(),
    )
    .await
    {
        return refusal;
    }
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
    let stored = state
        .schedules
        .update(
            &id,
            &crate::launches::schedules::NewSchedule {
                standing: crate::launches::standing::NewStanding {
                    playbook: &authorized.pack.id,
                    target_kind: &body.target_kind,
                    eligible_draft_version: draft_version,
                    params: &authorized.params,
                    schema_digest: &authorized.pack.schema_digest,
                    max_cost: authorized.max_cost,
                    max_time: &authorized.max_time,
                    advance_dedupe: body.advance_dedupe,
                    enabled: body.enabled,
                    created_by: actor,
                    owner_principal: saver.owner_principal.as_deref(),
                    owner_groups: Some(&saver.groups),
                    dispatch_target: Some(&saver.dispatch_target),
                    agent_provider: saver.provider.as_deref(),
                    agent_model: saver.model.as_deref(),
                },
                cursor: cursor.as_ref(),
                spec: &spec,
            },
            jiff::Timestamp::now(),
        )
        .await;
    let stored = match stored {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(format!("no schedule {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    };
    // The re-save re-owned the row, so the firings that parked under the old snapshot will never
    // launch.
    let expired = match state.schedules.expire_stale_owner_parks(&id).await {
        Ok(keys) => keys,
        Err(e) => return AppError::from(e).into_response(),
    };
    if !expired.is_empty() {
        tracing::info!(schedule = %id, expired = expired.len(), "schedules: re-save expired stale-owner parks");
    }

    let audit_key = format!("schedule:{id}");
    let audit_reason = format!(
        "playbook {} rescheduled on {:?} ({}) from {} to {}",
        stored.playbook,
        stored.cron_expr,
        stored.tz,
        prior
            .as_ref()
            .and_then(|s| s.adopted_rev.as_deref())
            .unwrap_or("draft head"),
        stored.adopted_rev.as_deref().unwrap_or("draft head")
    );
    if let Err(e) = state
        .audit_required(
            crate::event_log::Event::now(
                &audit_key,
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

    Json(ScheduleDto::from(stored)).into_response()
}

/// Delete a schedule so it stops firing. The launches it already fired are ordinary runs and stay.
#[utoipa::path(
    delete,
    path = "/api/schedules/{id}",
    params(("id" = String, Path, description = "Schedule id")),
    responses(
        (status = 204, description = "Deleted; it will not fire again"),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No schedule with that id", body = ErrorBody)
    )
)]
pub(crate) async fn delete_schedule(
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
        &format!("no schedule {id:?}"),
    )
    .await
    {
        return refusal;
    }
    match state.schedules.delete(&id).await {
        Ok(true) => {}
        Ok(false) => return not_found(format!("no schedule {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    }
    state
        .audit(
            crate::event_log::Event::now(
                &format!("schedule:{id}"),
                "enabled",
                "deleted",
                Some("schedule deleted"),
                None,
            )
            .by(identity.as_deref()),
            "delete_schedule",
        )
        .await;
    StatusCode::NO_CONTENT.into_response()
}

/// What an expression fires at next, computed server-side. The launch form's schedule toggle
/// renders this, so the timezone a launcher sees is the timezone the sweep will use — there is no
/// second interpretation in the browser.
#[utoipa::path(
    post,
    path = "/api/schedules/preview",
    request_body = SchedulePreviewBody,
    responses(
        (status = 200, description = "The next firings, UTC", body = SchedulePreviewDto),
        (status = 422, description = "The expression or the zone was refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn preview_schedule(Json(body): Json<SchedulePreviewBody>) -> Response {
    let spec = match CronSpec::parse(&body.cron_expr, &body.tz) {
        Ok(spec) => spec,
        Err(field) => return invalid_fields(vec![field]),
    };
    let count = body.count.unwrap_or(PREVIEW_DEFAULT).min(PREVIEW_MAX);
    let firings = spec
        .next_firings(jiff::Timestamp::now(), count)
        .into_iter()
        .map(stamp)
        .collect();
    Json(SchedulePreviewDto {
        cron_expr: spec.expr().to_string(),
        tz: spec.tz_name().to_string(),
        firings,
    })
    .into_response()
}
