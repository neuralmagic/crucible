use crate::api::dto::*;
use crate::dto::dto;

use crate::api::state::*;

use crate::launches::model::PlaybookRun;

use crate::launches::one_shots::{CancelOutcome, NewOneShot, OneShot};

use crate::playbooks::api::registry::authorize_launch;

use axum::extract::{Path, State};

use axum::http::StatusCode;

use axum::response::{IntoResponse, Response};

use serde::{Deserialize, Serialize};

use utoipa::ToSchema;

// --- the one-shot runs surface (ad-hoc launches; a schedule's firings are the schedules') ------

/// One ad-hoc launch: the authorization it froze (a relaunch prefills from `params` and the
/// ceilings), what the run did, and what it cost.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlaybookRunDto {
    /// The issue key the run is tracked under.
    pub key: String,
    pub playbook: String,
    /// The registry row's description; null when the playbook was deregistered since.
    pub description: Option<String>,
    /// The values exactly as launched — the prefill source.
    #[schema(value_type = Object)]
    pub params: serde_json::Value,
    /// The schema digest the values were validated against.
    pub schema_digest: String,
    /// The digest the playbook serves now; null when it is no longer registered.
    pub current_schema_digest: Option<String>,
    /// True when the pack has been re-pinned onto a different form since this launch, so a
    /// prefill of these values may no longer satisfy it.
    pub schema_drifted: bool,
    pub max_cost: f64,
    pub max_time: String,
    /// Whether this run was opted into advancing the schedule dedupe state.
    pub advance_dedupe: bool,
    /// `manual` (a form POST), `deferred` (a one-shot's `fire_at` came due), `schedule` (a
    /// firing) or `draft` (a studio test-fire).
    pub origin: String,
    /// The draft version this launch froze; null when `playbook` names a registered pack.
    pub draft_version: Option<i64>,
    /// The schedule this launch belongs to: the one that fired it, or the one whose cursor it was
    /// opted into advancing.
    pub schedule: Option<String>,
    /// The launch issue's status: the run's outcome once it is terminal.
    pub status: String,
    pub parked_reason: Option<String>,
    /// When the park was a secrets refusal, its detail: the declared name this launch's scope has
    /// no binding for, or the bound secret the launcher does not own. Null for every other park.
    pub secrets_refusal: Option<String>,
    /// The inference provider this launch pinned, from `GET /api/config/providers`; null resolves
    /// through the configured defaults at dispatch.
    pub agent_provider: Option<String>,
    /// The model pinned alongside it; null takes the provider's default.
    pub agent_model: Option<String>,
    /// Summed spend of this launch's runs; null until one books cost.
    pub cost_usd: Option<f64>,
    /// How many runs this launch has dispatched.
    pub runs: i64,
    /// Task attempts across those runs that ended `transport`: lost to infrastructure, not to a
    /// verdict. Non-zero on a `done` launch means advisory tasks never ran.
    pub transport_losses: i64,
    pub created_by: Option<String>,
    pub created_at: String,
}

impl From<PlaybookRun> for PlaybookRunDto {
    fn from(r: PlaybookRun) -> Self {
        let schema_drifted = r
            .current_schema_digest
            .as_deref()
            .is_some_and(|current| current != r.schema_digest);
        let secrets_refusal = r.parked_reason.as_deref().and_then(|reason| {
            crate::model::ParkReason::parse(reason)
                .secrets_refusal()
                .map(str::to_string)
        });
        PlaybookRunDto {
            key: r.key,
            playbook: r.playbook,
            description: r.description,
            params: r.params,
            schema_digest: r.schema_digest,
            current_schema_digest: r.current_schema_digest,
            schema_drifted,
            max_cost: r.max_cost,
            max_time: r.max_time,
            advance_dedupe: r.advance_dedupe,
            origin: r.origin.as_str().to_string(),
            draft_version: r.draft_version,
            schedule: r.schedule,
            status: r.status.as_str().to_string(),
            parked_reason: r.parked_reason,
            secrets_refusal,
            agent_provider: r.agent_provider,
            agent_model: r.agent_model,
            cost_usd: r.cost_usd,
            runs: r.runs,
            transport_losses: r.transport_losses,
            created_by: r.created_by,
            created_at: r.created_at,
        }
    }
}

/// How many launches the runs list returns.
const RUNS_LIMIT: i64 = 200;

/// `GET /api/playbook-runs` — the one-shot runs section: every ad-hoc launch with its frozen
/// snapshot, ceilings, outcome, and cost, newest first. A schedule's firings are left out; they
/// belong to the schedules surface, and downstream the rows are identical anyway.
#[utoipa::path(
    get,
    path = "/api/playbook-runs",
    responses((status = 200, description = "Ad-hoc playbook launches", body = Vec<PlaybookRunDto>))
)]
pub(crate) async fn list_playbook_runs(
    State(state): State<ApiState>,
) -> Result<Json<Vec<PlaybookRunDto>>, AppError> {
    let rows =
        crate::launches::store::list_playbook_runs(state.db.pool(), None, RUNS_LIMIT).await?;
    Ok(Json(rows.into_iter().map(PlaybookRunDto::from).collect()))
}

/// Where a launch's engine is. A launch is authorized, then dispatched; nothing else stands
/// between the two, so these three states are the whole lifecycle a reader needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DispatchState {
    /// Authorized, waiting for the reconcile sweep to pick it up.
    Pending,
    /// Every attempt so far failed, and no run exists.
    Failed,
    /// A run exists (or existed): the engine got started.
    Dispatched,
}

/// A launch's dispatch, first-class: whether the engine ever started and, when an attempt failed,
/// the error that stopped it. The failure survives a later success — a run that took three tries
/// to leave the ground is worth saying so.
#[derive(Debug, Serialize, ToSchema)]
pub struct LaunchDispatchDto {
    pub state: DispatchState,
    /// The newest failed attempt's error chain, or its reason when it carried no chain.
    pub failure: Option<String>,
    pub failed_at: Option<String>,
    /// How many attempts failed.
    pub failures: i64,
}

/// One run a launch dispatched.
#[derive(Debug, Serialize, ToSchema)]
pub struct LaunchRunDto {
    pub run_id: String,
    pub status: String,
    /// `pod` (a work pod) or `local` (a supervised subprocess on the controller's machine).
    pub dispatch: String,
    /// The cluster the run was dispatched onto: `hub` or a spoke name.
    pub cluster: String,
    /// The namespace that cluster resolved the pod into; null for a local run or one whose
    /// location predates the columns and could not be recovered.
    pub namespace: Option<String>,
    pub pod: Option<String>,
    pub cost_usd: Option<f64>,
}

/// One launch in full: the frozen authorization, where its dispatch got to, and the runs it
/// produced. The launch view renders this; a relaunch prefills from `launch` alone.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlaybookLaunchDetailDto {
    pub launch: PlaybookRunDto,
    /// Whether the source this launch names still exists — the draft for a draft launch, the
    /// registry row otherwise. False means the origin link has nowhere to go.
    pub source_exists: bool,
    pub dispatch: LaunchDispatchDto,
    /// Every run this launch dispatched, newest first.
    pub runs: Vec<LaunchRunDto>,
}

/// `GET /api/playbook-runs/{key}` — one launch: its frozen snapshot (the source a relaunch
/// prefills the launch form from), its dispatch state, and its runs. Reads a launch of any origin:
/// relaunching a scheduled firing as an ad-hoc run is a legitimate thing to want.
#[utoipa::path(
    get,
    path = "/api/playbook-runs/{key}",
    params(("key" = String, Path, description = "The launch's issue key")),
    responses(
        (status = 200, description = "The launch, its dispatch and its runs", body = PlaybookLaunchDetailDto),
        (status = 404, description = "No launch under that key", body = ErrorBody)
    )
)]
pub(crate) async fn get_playbook_run(
    State(state): State<ApiState>,
    Path(key): Path<String>,
) -> Result<Response, AppError> {
    let rows = crate::launches::store::list_playbook_runs(state.db.pool(), Some(&key), 1).await?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(not_found(format!("no playbook launch {key:?}")));
    };
    let source_exists = match row.draft_version {
        Some(_) => crate::playbooks::drafts::get(state.db.pool(), &row.playbook)
            .await?
            .is_some(),
        None => crate::playbooks::registry::get(state.db.pool(), &row.playbook)
            .await?
            .is_some(),
    };
    let runs = crate::launches::store::list_runs_for_launch(state.db.pool(), &key).await?;
    let dispatch = dispatch_state(&state.db.events().read_for_key(&key).await?, &runs);
    let runs = runs
        .into_iter()
        .map(|r| LaunchRunDto {
            run_id: r.run_id,
            status: r.status,
            dispatch: r.dispatch.as_str().to_string(),
            cluster: r.location.cluster,
            namespace: r.location.namespace,
            pod: r.pod,
            cost_usd: r.cost_usd,
        })
        .collect();
    Ok(Json(PlaybookLaunchDetailDto {
        launch: PlaybookRunDto::from(row),
        source_exists,
        dispatch,
        runs,
    })
    .into_response())
}

/// Fold the launch's history into its dispatch state: a run means the engine started, and the
/// failed attempts are the events [`crate::runs::launch::PLAYBOOK_DISPATCH_FAILED`] stamps.
fn dispatch_state(
    events: &[crate::event_log::EventRecord],
    runs: &[crate::runs::model::Run],
) -> LaunchDispatchDto {
    let failed: Vec<&crate::event_log::EventRecord> = events
        .iter()
        .filter(|e| e.reason.as_deref() == Some(crate::runs::launch::PLAYBOOK_DISPATCH_FAILED))
        .collect();
    let last = failed.last();
    let state = if !runs.is_empty() {
        DispatchState::Dispatched
    } else if last.is_some() {
        DispatchState::Failed
    } else {
        DispatchState::Pending
    };
    LaunchDispatchDto {
        state,
        failure: last.and_then(|e| e.evidence.clone().or_else(|| e.reason.clone())),
        failed_at: last.map(|e| e.ts.clone()),
        failures: i64::try_from(failed.len()).unwrap_or(i64::MAX),
    }
}

dto! {
    // --- deferred one-shots (run once at a time; not a degenerate schedule) ------------------------

    /// One deferred one-shot: what it will launch, when, and what it launched if it already has.
    pub struct OneShotDto: From<r: OneShot> {
        pub id: String,
        pub playbook: String,
        #[schema(value_type = Object)]
        pub params: serde_json::Value,
        pub schema_digest: String,
        pub max_cost: f64,
        pub max_time: String,
        pub advance_dedupe: bool,
        /// The schedule whose dedupe state the firing may advance; null when none was named.
        pub dedupe_schedule: Option<String>,
        /// The instant it fires, normalized to UTC.
        pub fire_at: String,
        /// `pending`, `fired`, or `canceled`.
        pub status: String = r.status.as_str().to_string(),
        /// The launch the firing minted; null until it fires.
        pub fired_key: Option<String>,
        pub fired_at: Option<String>,
        pub created_by: Option<String>,
        /// The principal the firing launches under, and whether they must sign in again first.
        pub owner_principal: Option<String>,
        pub owner_signin_required: bool,
        pub dispatch_target: Option<String>,
        pub agent_provider: Option<String>,
        pub agent_model: Option<String>,
        pub created_at: String,
    }
}

/// A one-shot as the list serves it: the row, and what the caller may do with it.
#[derive(Debug, Serialize, ToSchema)]
pub struct OneShotView {
    #[serde(flatten)]
    pub one_shot: OneShotDto,
    pub actions: Vec<crate::authz::action::Verb>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct CreateOneShotBody {
    /// The principal to own it: `user:<login>` or `team:<slug>` the caller acts as; absent means
    /// the caller.
    owner: Option<String>,
    /// The registry id to launch when the time comes.
    playbook: String,
    /// The param values, validated now against the pack's stored schema and frozen.
    #[serde(default)]
    params: std::collections::BTreeMap<String, String>,
    max_cost: f64,
    max_time: String,
    /// When to fire, as an RFC3339 instant. Any offset is accepted and stored as UTC.
    fire_at: String,
    /// The schema digest the form was rendered against, refused when the pack has moved on.
    #[serde(default)]
    schema_digest: Option<String>,
    /// Opt the firing into advancing the schedule dedupe state. Absent = false.
    #[serde(default)]
    advance_dedupe: bool,
    /// The schedule whose dedupe state the firing may advance, which must run the same playbook.
    /// Absent advances nothing, so `advance_dedupe` alone is inert.
    #[serde(default)]
    dedupe_schedule: Option<String>,
    /// Which cluster the firing dispatches onto, from `GET /api/dispatch-targets`. Absent selects
    /// the controller's configured default.
    #[serde(default)]
    dispatch_target: Option<String>,
    /// Which registered inference provider the firing's agent runs against. Absent resolves
    /// through the configured defaults at dispatch.
    #[serde(default)]
    provider: Option<String>,
    /// The model to ask that provider for. Free text; absent takes the provider's own default.
    #[serde(default)]
    model: Option<String>,
}

/// Defer a launch: validate it now, exactly as an immediate launch is validated, and store it to
/// fire once at `fire_at`. Nothing is adopted until the sweep claims the row, and the values it
/// launches are the ones frozen here.
#[utoipa::path(
    post,
    path = "/api/one-shots",
    request_body = CreateOneShotBody,
    responses(
        (status = 201, description = "One-shot scheduled to fire once", body = OneShotDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No playbook with that id", body = ErrorBody),
        (status = 409, description = "The form was rendered against a schema this playbook no longer serves", body = ErrorBody),
        (status = 422, description = "Parameter values, ceilings, or fire_at were refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn create_one_shot(
    State(state): State<ApiState>,
    groups: crate::identity::auth::Groups,
    caller: crate::authz::Caller,
    Json(body): Json<CreateOneShotBody>,
) -> Response {
    let fire_at = match crate::launches::one_shots::normalize_fire_at(&body.fire_at) {
        Ok(t) => t,
        Err(message) => {
            return invalid_fields(vec![crate::playbooks::registry::FieldError {
                field: "fire_at".to_string(),
                message,
            }]);
        }
    };
    if fire_at <= crate::clock::now_rfc3339() {
        return invalid_fields(vec![crate::playbooks::registry::FieldError {
            field: "fire_at".to_string(),
            message: format!("fire_at {fire_at} is in the past; launch it now instead"),
        }]);
    }
    let authorized = match authorize_launch(
        &state,
        &caller,
        &body.playbook,
        &body.params,
        body.max_cost,
        &body.max_time,
        body.schema_digest.as_deref(),
    )
    .await
    {
        Ok(a) => a,
        Err(refusal) => return refusal,
    };

    let dedupe_schedule = match crate::playbooks::api::registry::dedupe_target(
        &state,
        &authorized.pack.id,
        body.dedupe_schedule.as_deref(),
    )
    .await
    {
        Ok(target) => target,
        Err(refusal) => return refusal,
    };

    // A one-shot fires with no session, so this save owns it to the saver and snapshots their
    // groups, exactly as a schedule save does.
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
    let stored = crate::launches::one_shots::create(
        state.db.pool(),
        &NewOneShot {
            standing: crate::launches::standing::NewStanding {
                playbook: &authorized.pack.id,
                target_kind: "adopted",
                eligible_draft_version: None,
                params: &authorized.params,
                schema_digest: &authorized.pack.schema_digest,
                max_cost: authorized.max_cost,
                max_time: &authorized.max_time,
                advance_dedupe: body.advance_dedupe,
                enabled: true,
                created_by: actor,
                owner_principal: saver.owner_principal.as_deref(),
                owner_groups: Some(&saver.groups),
                dispatch_target: Some(&saver.dispatch_target),
                agent_provider: saver.provider.as_deref(),
                agent_model: saver.model.as_deref(),
            },
            dedupe_schedule: dedupe_schedule.as_deref(),
            fire_at: &fire_at,
        },
    )
    .await;
    let stored = match stored {
        Ok(Some(s)) => s,
        Ok(None) => {
            return not_found(format!(
                "playbook {:?} was deregistered while the one-shot was being authorized",
                authorized.pack.id
            ));
        }
        Err(e) => return AppError::from(e).into_response(),
    };

    state
        .audit(
            crate::event_log::Event::now(
                &format!("playbook:{}", authorized.pack.id),
                "one-shot",
                "one-shot",
                Some(&format!("one-shot {} deferred to {fire_at}", stored.id)),
                None,
            )
            .by(actor),
            "create_one_shot",
        )
        .await;

    (StatusCode::CREATED, Json(OneShotDto::from(stored))).into_response()
}

/// `GET /api/one-shots` — pending one-shots first (soonest due first), then the fired and canceled
/// ones with the launch each produced.
#[utoipa::path(
    get,
    path = "/api/one-shots",
    responses((status = 200, description = "The deferred one-shots the caller may read", body = Vec<OneShotView>))
)]
pub(crate) async fn list_one_shots(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
) -> Result<Json<Vec<OneShotView>>, AppError> {
    let rows =
        crate::launches::one_shots::list(state.db.pool(), crate::launches::one_shots::LIST_LIMIT)
            .await?;
    let rows = crate::authz::owner::readable_with_actions(
        &state,
        &caller,
        crate::authz::action::ResourceType::StandingLaunch,
        rows,
        |r| {
            crate::authz::decision::Resource::new(
                crate::authz::action::ResourceType::StandingLaunch,
                &r.id,
                crate::authz::model::Principal::stored(r.owner_principal.as_deref()),
            )
        },
    )
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(row, actions)| OneShotView {
                one_shot: OneShotDto::from(row),
                actions,
            })
            .collect(),
    ))
}

/// Cancel a pending one-shot so it never fires. A one-shot the sweep already claimed is a run in
/// flight — 409, and the row keeps saying `fired`.
#[utoipa::path(
    delete,
    path = "/api/one-shots/{id}",
    params(("id" = String, Path, description = "One-shot id")),
    responses(
        (status = 200, description = "Canceled; it will not fire", body = OneShotDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No one-shot with that id", body = ErrorBody),
        (status = 409, description = "It already fired (or was already canceled)", body = ErrorBody)
    )
)]
pub(crate) async fn cancel_one_shot(
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
        &format!("no one-shot {id:?}"),
    )
    .await
    {
        return refusal;
    }
    match crate::launches::one_shots::cancel(state.db.pool(), &id).await {
        Ok(CancelOutcome::Canceled) => {}
        Ok(CancelOutcome::NotPending(status)) => {
            return (
                StatusCode::CONFLICT,
                Json(ErrorBody::new(format!(
                    "one-shot {id} is {}, not pending",
                    status.as_str()
                ))),
            )
                .into_response();
        }
        Ok(CancelOutcome::Unknown) => return not_found(format!("no one-shot {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    }
    let canceled = match crate::launches::one_shots::get(state.db.pool(), &id).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(format!("no one-shot {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    };

    state
        .audit(
            crate::event_log::Event::now(
                &format!("playbook:{}", canceled.playbook),
                "one-shot",
                "one-shot",
                Some(&format!("one-shot {id} canceled")),
                None,
            )
            .by(identity.as_deref()),
            "cancel_one_shot",
        )
        .await;

    Json(OneShotDto::from(canceled)).into_response()
}
