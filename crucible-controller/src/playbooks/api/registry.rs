use crate::api::dto::*;
use crate::api::state::*;
use crate::daemon::queue::IssueKey;
use crate::playbooks::registry::{PlaybookRow, PlaybookSource, RegisterError, RegisterPlaybook};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// --- playbook registry (admin-gated registration, open reads) ------------------

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct RegisterPlaybookBody {
    /// The principal to own it: `user:<login>` or `team:<slug>` the caller acts as; absent means
    /// the caller.
    owner: Option<String>,
    /// The registry id: a lowercase slug, the name every launch of this pack is keyed by.
    id: String,
    description: String,
    /// `owner/repo` or a clone URL.
    repo: String,
    /// Branch or tag to pin the pack at. Omitted ⇒ the repo's default branch.
    #[serde(default)]
    git_ref: Option<String>,
    /// The pack directory inside the repo. Omitted ⇒ the repo root.
    #[serde(default)]
    path: Option<String>,
    /// The commit an import preview was taken against. When the ref has moved since, the
    /// registration is refused rather than pinning bytes nobody previewed.
    #[serde(default)]
    expected_rev: Option<String>,
    /// The exposure digest the caller reviewed. Re-registering an id whose declared exposure has
    /// changed is refused (409, naming both digests) unless this quotes the new one back.
    #[serde(default)]
    accept_exposure_digest: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct RegisterAck {
    pub(crate) id: String,
    /// The commit the pack is pinned at, or the tarball digest of a draft-sourced pack.
    pub(crate) rev: String,
    pub(crate) tar_digest: String,
    pub(crate) schema_digest: String,
    /// True when re-registering an existing id changed the launch form.
    pub(crate) schema_changed: bool,
    /// The digest of the exposure this revision discloses; null for an absent-legacy revision.
    pub(crate) exposure_digest: Option<String>,
    /// True when re-registering an existing id changed the declared exposure.
    pub(crate) exposure_changed: bool,
}

impl From<crate::playbooks::registry::Registered> for RegisterAck {
    fn from(r: crate::playbooks::registry::Registered) -> Self {
        RegisterAck {
            id: r.id,
            rev: r.rev,
            tar_digest: r.tar_digest,
            schema_digest: r.schema_digest,
            schema_changed: r.schema_changed,
            exposure_digest: r.exposure_digest,
            exposure_changed: r.exposure_changed,
        }
    }
}

/// The response a refused registration earns.
pub(crate) fn register_refusal(err: RegisterError) -> Response {
    match err {
        RegisterError::Invalid(msg) | RegisterError::Compile(msg) => unprocessable(msg),
        RegisterError::Conflict(msg) => {
            (StatusCode::CONFLICT, Json(ErrorBody::new(msg))).into_response()
        }
        RegisterError::Fetch(msg) => {
            (StatusCode::BAD_GATEWAY, Json(ErrorBody::new(msg))).into_response()
        }
        RegisterError::RevMoved(current) => (
            StatusCode::CONFLICT,
            Json(ErrorBody::new(format!(
                "the ref moved to {current} since the preview; preview again before registering"
            ))),
        )
            .into_response(),
        e @ RegisterError::ExposureChanged { .. } => {
            (StatusCode::CONFLICT, Json(ErrorBody::new(e.to_string()))).into_response()
        }
        RegisterError::Internal(e) => AppError::from(e).into_response(),
    }
}

/// The substrate a pack's agent asks for, and whether this deployment can give it. Rides every
/// surface a pack is looked at from — the registry list, the import review, a draft's save — so
/// nobody has to launch to find out.
#[derive(Debug, Serialize, ToSchema)]
pub struct PackDispatchDto {
    /// The pack's `[agent]` backend; null when its manifest did not parse.
    pub backend: Option<String>,
    /// The sandbox image the backend runs in, when it declares one.
    pub sandbox_image: Option<String>,
    /// The `[agent]` harness the manifest names, when it names one.
    pub harness: Option<String>,
    /// `[agent.requires]`: predicate -> version range the image must satisfy.
    pub requires: std::collections::BTreeMap<String, String>,
    /// `[agent.prefers]`: predicate -> version range that ranks compatible images.
    pub prefers: std::collections::BTreeMap<String, String>,
    /// `[agent] allow_unverified_image`: the pack opts into launching on an image the catalog
    /// cannot vouch for.
    pub allow_unverified_image: bool,
    /// `[agent.resources]`: what the sandbox is scheduled with.
    pub resources: SandboxResourcesDto,
    /// The capability preflight of the declared image against the catalog, with the manifest's
    /// own harness standing in for the one a launch will pin.
    pub image: crate::playbooks::preflight::ImagePreflight,
    /// True when this deployment can dispatch that backend.
    pub dispatchable: bool,
    /// Why it cannot, verbatim; null when it can.
    pub refusal: Option<String>,
    /// True when a launch here runs as a supervised subprocess on the controller's machine rather
    /// than a work pod.
    pub local_mode: bool,
}

/// `[agent.resources]` as the sandbox is scheduled with it; zero and nulls are no request.
#[derive(Debug, Serialize, ToSchema)]
pub struct SandboxResourcesDto {
    pub gpus: u32,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    /// Node labels the sandbox must land on, as the pack wrote them.
    pub node_selector: std::collections::BTreeMap<String, String>,
}

impl From<&crucible::manifest::SandboxResources> for SandboxResourcesDto {
    fn from(r: &crucible::manifest::SandboxResources) -> Self {
        SandboxResourcesDto {
            gpus: r.gpus,
            cpu: r.cpu.as_ref().map(|q| q.as_str().to_string()),
            memory: r.memory.as_ref().map(|q| q.as_str().to_string()),
            node_selector: r.node_selector.clone(),
        }
    }
}

impl PackDispatchDto {
    pub(crate) fn new(
        agent: Option<&crate::playbooks::dispatch::PackAgent>,
        cap: &crate::playbooks::dispatch::DispatchCapability,
        catalog: &[crate::images::model::CatalogImage],
    ) -> Self {
        let Some(agent) = agent else {
            return PackDispatchDto {
                backend: None,
                sandbox_image: None,
                harness: None,
                requires: Default::default(),
                prefers: Default::default(),
                allow_unverified_image: false,
                resources: SandboxResourcesDto::from(
                    &crucible::manifest::SandboxResources::default(),
                ),
                image: Default::default(),
                dispatchable: false,
                refusal: Some(UNREADABLE_MANIFEST.to_string()),
                local_mode: cap.is_local(),
            };
        };
        let refusal = cap.refusal(agent);
        PackDispatchDto {
            backend: Some(agent.backend.clone()),
            sandbox_image: agent.sandbox_image.clone(),
            harness: agent.harness.clone(),
            requires: agent.requires.clone(),
            prefers: agent.prefers.clone(),
            allow_unverified_image: agent.allow_unverified_image,
            resources: SandboxResourcesDto::from(&agent.resources),
            image: crate::playbooks::preflight::preflight(agent, None, catalog),
            dispatchable: refusal.is_none(),
            refusal,
            local_mode: cap.is_local(),
        }
    }
}

/// The image catalog a request's dispatch notices are computed against.
#[allow(clippy::result_large_err)]
pub(crate) async fn catalog(
    state: &ApiState,
) -> Result<Vec<crate::images::model::CatalogImage>, Response> {
    crate::images::store::list_images(state.db.pool())
        .await
        .map_err(|e| AppError::from(e).into_response())
}

/// Decide `launch` on a pack against its owner, for every surface that fires it: the form POST,
/// one-shots, watches and schedules.
#[allow(clippy::result_large_err)]
pub(crate) async fn decide_launch(
    state: &ApiState,
    caller: &crate::authz::Caller,
    pack: &PlaybookRow,
) -> Result<(), Response> {
    crate::authz::owner::decide_on(
        state,
        caller,
        &crate::authz::decision::Resource::new(
            crate::authz::action::ResourceType::Playbook,
            &pack.id,
            pack.owner.clone(),
        ),
        crate::authz::action::Verb::Launch,
    )
    .await
    .map(|_| ())
    .map_err(IntoResponse::into_response)
}

/// The capability preflight of a launch: the harness it resolves (its provider pin, else the
/// scope's dispatch default) against the pack's image, over the catalog. `Err` is the 422 to
/// answer with; `Ok` carries the warnings a launch proceeds under.
#[allow(clippy::result_large_err)]
pub(crate) async fn authorize_image(
    state: &ApiState,
    agent: Option<&crate::playbooks::dispatch::PackAgent>,
    provider: Option<&str>,
    domain: Option<&str>,
) -> Result<crate::playbooks::preflight::ImagePreflight, Response> {
    let Some(agent) = agent else {
        return Ok(Default::default());
    };
    let resolved = crate::playbooks::preflight::resolve_harness(state.db.pool(), provider, domain)
        .await
        .map_err(|e| AppError::from(e).into_response())?;
    let catalog = catalog(state).await?;
    let verdict = crate::playbooks::preflight::preflight(agent, resolved.as_ref(), &catalog);
    if verdict.refused() {
        return Err(refused(
            "the pack's sandbox image fails the capability preflight",
            verdict
                .refusals
                .iter()
                .map(|message| crate::playbooks::registry::FieldError {
                    field: "sandbox_image".to_string(),
                    message: message.clone(),
                })
                .collect(),
        ));
    }
    Ok(verdict)
}

/// One registered playbook: what it is, where it is pinned, and the digest of the launch form the
/// pinned engine extracted from it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlaybookDto {
    pub id: String,
    pub description: String,
    pub source: PlaybookSourceDto,
    /// The git commit, or the tarball digest of a draft-sourced pack.
    pub rev: String,
    pub tar_digest: String,
    pub schema_digest: String,
    /// The engine pin the stored schema was extracted with.
    pub core_rev: String,
    /// The digest of this revision's declared exposure; null for an absent-legacy revision.
    pub exposure_digest: Option<String>,
    /// What the pack's agent needs, against what this deployment can dispatch.
    pub dispatch: PackDispatchDto,
    /// `user:<login>` or `team:<slug>`.
    pub owner: String,
    /// What the caller may do with this playbook under the policy set in force.
    pub actions: Vec<crate::authz::action::Verb>,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Where a registered pack came from.
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlaybookSourceDto {
    Git {
        repo: String,
        /// The branch/tag the pack was fetched at; null = the repo's default branch.
        git_ref: Option<String>,
        /// The pack directory inside the repo; empty = the repo root.
        path: String,
    },
    /// Published straight from a draft version.
    Draft { draft: String, version: i64 },
}

impl From<PlaybookSource> for PlaybookSourceDto {
    fn from(source: PlaybookSource) -> Self {
        match source {
            PlaybookSource::Git {
                repo,
                git_ref,
                path,
            } => PlaybookSourceDto::Git {
                repo,
                git_ref,
                path,
            },
            PlaybookSource::Draft { draft, version } => PlaybookSourceDto::Draft { draft, version },
        }
    }
}

impl PlaybookDto {
    pub(crate) fn from_row(
        r: PlaybookRow,
        actions: Vec<crate::authz::action::Verb>,
        cap: &crate::playbooks::dispatch::DispatchCapability,
        catalog: &[crate::images::model::CatalogImage],
    ) -> Self {
        let dispatch = PackDispatchDto::new(r.agent.as_ref(), cap, catalog);
        PlaybookDto {
            id: r.id,
            description: r.description,
            source: r.source.into(),
            rev: r.rev,
            tar_digest: r.tar_digest,
            exposure_digest: r.exposure_digest,
            schema_digest: r.schema_digest,
            core_rev: r.core_rev,
            dispatch,
            owner: r.owner.to_string(),
            actions,
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// One registered playbook together with the exact pinned files a launch or clone will consume.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlaybookDetailDto {
    #[serde(flatten)]
    pub playbook: PlaybookDto,
    pub files: std::collections::BTreeMap<String, String>,
}

/// Register a playbook pack, or re-pin one already registered. The controller clones the repo at
/// `git_ref`, tars the pack subtree, and runs the pinned engine's `plan params` against the pack's
/// declared workflow source; the resulting JSON Schema is what the launch form and ask validation
/// read. A pack whose source does not compile registers nothing and gets the engine's compile error
/// back — the form can never fail to render later.
#[utoipa::path(
    post,
    path = "/api/playbooks",
    request_body = RegisterPlaybookBody,
    responses(
        (status = 201, description = "Playbook registered (or re-pinned)", body = RegisterAck),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody),
        (status = 409, description = "The ref moved since the preview this registration quotes, or a live draft holds the id", body = ErrorBody),
        (status = 422, description = "Bad id/description/ref/path, or the pack's workflow source did not compile", body = ErrorBody),
        (status = 502, description = "Cloning the pack repo failed", body = ErrorBody)
    )
)]
pub(crate) async fn register_playbook(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    caller: crate::authz::Caller,
    Json(body): Json<RegisterPlaybookBody>,
) -> Response {
    if let Some(msg) = require_non_empty(&[
        ("id", &body.id),
        ("description", &body.description),
        ("repo", &body.repo),
    ]) {
        return unprocessable(msg);
    }
    let git_ref = match require_git_ref(body.git_ref.as_deref()) {
        Ok(r) => r,
        Err(msg) => {
            return unprocessable(msg);
        }
    };
    let actor = identity.as_deref();
    let owner = match crate::authz::owner::owner_for_create(
        &state,
        &caller,
        body.owner.as_deref(),
        crate::authz::action::ResourceType::Playbook,
    )
    .await
    {
        Ok(owner) => owner,
        Err(refused) => return refused,
    };
    let req = RegisterPlaybook {
        id: body.id.trim().to_string(),
        owner,
        description: body.description.trim().to_string(),
        repo: normalize_repo(body.repo.trim()).to_string(),
        git_ref,
        path: body.path.unwrap_or_default().trim().to_string(),
        expected_rev: body
            .expected_rev
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty()),
        accept_exposure_digest: body
            .accept_exposure_digest
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty()),
    };

    let git = match crate::playbooks::registry::PackGit::resolve(state.pack_pr_app.as_ref()).await {
        Ok(g) => g,
        Err(e) => return AppError::from(e).into_response(),
    };
    let registered =
        match crate::playbooks::registry::register(state.db.pool(), &git, req, actor).await {
            Ok(r) => r,
            Err(e) => return register_refusal(e),
        };

    state
        .audit(
            crate::event_log::Event::now(
                &format!("playbook:{}", registered.id),
                "registered",
                "registered",
                Some(&format!(
                    "playbook pinned at {} ({})",
                    registered.rev, registered.schema_digest
                )),
                None,
            )
            .by(actor),
            "register_playbook",
        )
        .await;

    (StatusCode::CREATED, Json(RegisterAck::from(registered))).into_response()
}

/// The registered playbook `id` names, decided for `verb` against its owner; a caller who may not
/// read it is told it does not exist.
#[allow(clippy::result_large_err)]
pub(crate) async fn readable_playbook(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    verb: crate::authz::action::Verb,
) -> Result<crate::playbooks::registry::PlaybookRow, Response> {
    let row = crate::playbooks::registry::get(state.db.pool(), id)
        .await
        .map_err(|e| AppError::from(e).into_response())?;
    crate::authz::owner::decide_row(
        state,
        caller,
        crate::authz::action::ResourceType::Playbook,
        id,
        verb,
        row,
        |r| r.owner.clone(),
    )
    .await
    .map_err(IntoResponse::into_response)
}

/// `GET /api/playbooks` — the registry: what can be launched, pinned where, with which form.
#[utoipa::path(
    get,
    path = "/api/playbooks",
    responses((status = 200, description = "Registered playbooks", body = Vec<PlaybookDto>))
)]
pub(crate) async fn list_playbooks(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
) -> Result<Json<Vec<PlaybookDto>>, AppError> {
    let rows = crate::playbooks::registry::list(state.db.pool()).await?;
    let rows = crate::authz::owner::readable_with_actions(
        &state,
        &caller,
        crate::authz::action::ResourceType::Playbook,
        rows,
        |r| {
            crate::authz::decision::Resource::new(
                crate::authz::action::ResourceType::Playbook,
                &r.id,
                r.owner.clone(),
            )
        },
    )
    .await?;
    let catalog = crate::images::store::list_images(state.db.pool()).await?;
    Ok(Json(
        rows.into_iter()
            .map(|(r, actions)| PlaybookDto::from_row(r, actions, &state.dispatch, &catalog))
            .collect(),
    ))
}

/// `GET /api/playbooks/{id}` — inspect the exact Git-pinned pack before launching or cloning it.
#[utoipa::path(
    get,
    path = "/api/playbooks/{id}",
    params(("id" = String, Path, description = "Registry id")),
    responses(
        (status = 200, description = "Registered playbook and its pinned files", body = PlaybookDetailDto),
        (status = 404, description = "No playbook with that id", body = ErrorBody)
    )
)]
pub(crate) async fn get_playbook(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let row = match readable_playbook(&state, &caller, &id, crate::authz::action::Verb::Read).await
    {
        Ok(row) => row,
        Err(refused) => return Ok(refused),
    };
    let Some(files) = crate::playbooks::registry::files(state.db.pool(), &id).await? else {
        return Ok(not_found(format!("no playbook {id:?}")));
    };
    let actions = crate::authz::owner::actions_on(
        &state,
        &caller,
        crate::authz::decision::Resource::new(
            crate::authz::action::ResourceType::Playbook,
            &id,
            row.owner.clone(),
        ),
    )
    .await?;
    let catalog = crate::images::store::list_images(state.db.pool()).await?;
    Ok(Json(PlaybookDetailDto {
        playbook: PlaybookDto::from_row(row, actions, &state.dispatch, &catalog),
        files,
    })
    .into_response())
}

/// `GET /api/playbooks/{id}/schema` — the stored params JSON Schema, verbatim as the engine printed
/// it. The SPA's launch form and the launch endpoint's validation read this one document.
#[utoipa::path(
    get,
    path = "/api/playbooks/{id}/schema",
    params(("id" = String, Path, description = "Registry id")),
    responses(
        (status = 200, description = "The pack's params JSON Schema", body = Object),
        (status = 404, description = "No playbook with that id", body = ErrorBody)
    )
)]
pub(crate) async fn get_playbook_schema(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    if let Err(refused) =
        readable_playbook(&state, &caller, &id, crate::authz::action::Verb::Read).await
    {
        return Ok(refused);
    }
    match crate::playbooks::registry::schema(state.db.pool(), &id).await? {
        Some(schema) => Ok(Json(schema).into_response()),
        None => Ok(not_found(format!("no playbook {id:?}"))),
    }
}

// --- launch (the one launch path: form, schedule sweep, and one-shots all land here) ----------

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct LaunchPlaybookBody {
    /// The param values, `{name: value}`, validated against the pack's stored schema.
    #[serde(default)]
    params: std::collections::BTreeMap<String, String>,
    /// The launcher's per-run cost ceiling in USD, bounded by `CONTROLLER_PLAYBOOK_MAX_COST_USD`.
    max_cost: f64,
    /// The launcher's wall-clock ceiling (`90s`, `30m`, `2h`), bounded by
    /// `CONTROLLER_PLAYBOOK_MAX_TIME`.
    max_time: String,
    /// The schema digest the form was rendered against. When it no longer matches the stored one,
    /// a pin bump moved the form under the launcher and the launch is refused.
    #[serde(default)]
    schema_digest: Option<String>,
    /// Opt this run into advancing the schedule dedupe state (the cursor, the seen-set). Absent =
    /// false: an ad-hoc experiment must not eat the next scheduled sweep's inputs.
    #[serde(default)]
    advance_dedupe: bool,
    /// The schedule whose dedupe state this run may advance, which must run the same playbook.
    /// Absent advances nothing, so `advance_dedupe` alone is inert.
    #[serde(default)]
    dedupe_schedule: Option<String>,
    /// Which cluster to dispatch onto, from `GET /api/dispatch-targets`. Absent selects the
    /// controller's configured default, which is what a form that never showed the choice sends.
    #[serde(default)]
    dispatch_target: Option<String>,
    /// Which registered inference provider this run's agent runs against, from
    /// `GET /api/config/providers`. Absent resolves through the configured defaults at dispatch.
    #[serde(default)]
    provider: Option<String>,
    /// The model to ask that provider for. Free text; absent takes the provider's own default.
    #[serde(default)]
    model: Option<String>,
}

/// What a launch landed: the issue key the run is tracked under, and the values it was authorized
/// with, read back the way the dispatch will read them.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlaybookLaunchAck {
    pub key: String,
    pub playbook: String,
    pub params: std::collections::BTreeMap<String, String>,
    pub schema_digest: String,
    pub max_cost: f64,
    pub max_time: String,
    pub advance_dedupe: bool,
    /// The schedule whose dedupe state this run may advance; null when none was named.
    pub dedupe_schedule: Option<String>,
    /// The cluster this launch was authorized to dispatch onto, resolved from the caller's
    /// eligible set.
    pub dispatch_target: String,
    /// The inference provider pinned on the row, echoed back; null resolves through the configured
    /// defaults at dispatch.
    pub provider: Option<String>,
    /// The model pinned alongside it; null takes the resolved provider's default.
    pub model: Option<String>,
    pub actor: Option<String>,
    /// What this launch is allowed to write and reach, recorded on the launch row before anything
    /// executed. Absent on a registered launch, whose disclosure is the registry row's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure: Option<ExposureDto>,
    /// The capability preflight this launch passed: the resolved digest, what it matched, and
    /// the warnings it proceeds under (an unverified-image override among them).
    pub image: crate::playbooks::preflight::ImagePreflight,
}

/// What a pack whose manifest never parsed is refused with. A launch would fail at the engine's
/// first read of it, so it is refused here instead.
pub(crate) const UNREADABLE_MANIFEST: &str =
    "the pack's crucible.toml did not parse, so what substrate its agent needs is unknown";

/// A launch this deployment has no substrate for. Refused here, at launch, rather than discovered
/// at the bottom of a failed reconcile: the pack is fine, this deployment is what cannot run it.
pub(crate) fn undispatchable(refusal: String) -> Response {
    refused(
        "this deployment cannot dispatch the pack's agent backend",
        vec![crate::playbooks::registry::FieldError {
            field: String::new(),
            message: refusal,
        }],
    )
}

/// Check the launcher's ceilings against the admin caps. Ceilings are the launcher's — the pack
/// declares none — so this is the only bound on them.
pub(crate) fn check_ceilings(
    caps: &crate::config::PlaybookCaps,
    max_cost: f64,
    max_time: &str,
) -> Result<crate::model::MaxTime, crate::playbooks::registry::FieldError> {
    let refuse = |field: &str, message: String| crate::playbooks::registry::FieldError {
        field: field.to_string(),
        message,
    };
    if !max_cost.is_finite() || max_cost <= 0.0 {
        return Err(refuse(
            "max_cost",
            "max_cost must be a positive number of USD".to_string(),
        ));
    }
    if max_cost > caps.max_cost {
        return Err(refuse(
            "max_cost",
            format!(
                "max_cost {max_cost} is above this controller's cap of {}",
                caps.max_cost
            ),
        ));
    }
    let max_time = crate::model::MaxTime::parse(max_time).map_err(|m| refuse("max_time", m))?;
    if max_time.secs() > caps.max_time.secs() {
        return Err(refuse(
            "max_time",
            format!(
                "max_time {max_time} is above this controller's cap of {}",
                caps.max_time
            ),
        ));
    }
    Ok(max_time)
}

/// Launch a registered playbook: validate the supplied values against the pack's stored schema,
/// bound the launcher's ceilings against the admin caps, and adopt an issue whose
/// `playbook_launches` row carries both. The row lands at `new` with no scope turn ahead of it —
/// the pack already exists, and this validated POST is the human authorization the scope/approval
/// gates would otherwise stand in for.
#[utoipa::path(
    post,
    path = "/api/playbooks/{id}/launch",
    params(("id" = String, Path, description = "Registry id")),
    request_body = LaunchPlaybookBody,
    responses(
        (status = 201, description = "Launch adopted; the run dispatches on the next reconcile", body = PlaybookLaunchAck),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No playbook with that id", body = ErrorBody),
        (status = 409, description = "The form was rendered against a schema this playbook no longer serves", body = ErrorBody),
        (status = 422, description = "Parameter values or launcher ceilings were refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn launch_playbook(
    State(state): State<ApiState>,
    groups: crate::identity::auth::Groups,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<LaunchPlaybookBody>,
) -> Response {
    let authorized = match authorize_launch(
        &state,
        &caller,
        &id,
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
    let pack = authorized.pack;
    if let Err(refused) = decide_launch(&state, &caller, &pack).await {
        return refused;
    }
    let dedupe_schedule =
        match dedupe_target(&state, &pack.id, body.dedupe_schedule.as_deref()).await {
            Ok(target) => target,
            Err(refusal) => return refusal,
        };
    let saver = match crate::playbooks::api::saver::resolve_saver(
        &state,
        &caller,
        &groups,
        crate::playbooks::api::saver::Ownership::Session,
        crate::playbooks::api::saver::SaverRequest {
            agent: pack.agent.clone(),
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
    let image = match authorize_image(
        &state,
        pack.agent.as_ref(),
        saver.provider.as_deref(),
        pack.source.repo(),
    )
    .await
    {
        Ok(v) => v,
        Err(refusal) => return refusal,
    };
    let actor = saver.actor.as_deref();
    let dispatch_target = saver.dispatch_target.clone();
    let key = format!("playbook:{id}:{}", uuid::Uuid::now_v7());
    let launch = crate::launches::model::NewPlaybookLaunch {
        playbook: &pack.id,
        repo: pack.source.launch_repo(),
        title: &pack.description,
        params: &authorized.params,
        schema_digest: &pack.schema_digest,
        max_cost: authorized.max_cost,
        max_time: &authorized.max_time,
        advance_dedupe: body.advance_dedupe,
        dedupe_schedule: dedupe_schedule.as_deref(),
        origin: crate::model::LaunchOrigin::Manual,
        draft_version: None,
        created_by: actor,
        launcher_groups: Some(&saver.groups),
    };
    use crate::launches::store::AdoptPlaybookOutcome;
    match crate::launches::store::adopt_playbook_launch(state.db.pool(), &key, &launch).await {
        Ok(AdoptPlaybookOutcome::Adopted) => {
            // The issue row the adopt just wrote is what every later dispatch reads its cluster
            // off, so the pin lands before the launch is enqueued.
            if let Err(refusal) =
                crate::playbooks::api::saver::pin_dispatch(&state, &key, &saver).await
            {
                return refusal;
            }
        }
        Ok(AdoptPlaybookOutcome::UnknownPlaybook) => {
            return not_found(format!("no playbook {id:?}"));
        }
        Ok(AdoptPlaybookOutcome::SchemaDrifted { current }) => {
            return (
                StatusCode::CONFLICT,
                Json(ErrorBody::new(format!(
                    "playbook {id} was re-registered while the launch was being authorized \
                     (schema is now {current}); reload the form"
                ))),
            )
                .into_response();
        }
        Err(e) => return AppError::from(e).into_response(),
    }

    let max_time = authorized.max_time;
    state
        .audit(
            crate::event_log::Event::now(
                &key,
                "new",
                "new",
                Some(&format!(
                    "playbook {id} launched at {} ({} params, max_cost {}, max_time {max_time})",
                    pack.rev,
                    body.params.len(),
                    body.max_cost
                )),
                None,
            )
            .by(actor),
            "launch_playbook",
        )
        .await;
    state.queue.enqueue_urgent(IssueKey(key.clone()));

    (
        StatusCode::CREATED,
        Json(PlaybookLaunchAck {
            key,
            playbook: pack.id,
            params: body.params,
            schema_digest: pack.schema_digest,
            max_cost: authorized.max_cost,
            max_time: max_time.as_str().to_string(),
            advance_dedupe: body.advance_dedupe,
            dedupe_schedule,
            dispatch_target,
            provider: saver.provider,
            model: saver.model,
            actor: saver.actor,
            exposure: None,
            image,
        }),
    )
        .into_response()
}

/// What a launch is authorized to run: the registry row it names, the values the pack's stored
/// schema accepted, and the ceilings the admin caps allowed. The immediate launch and the deferred
/// one-shot both come through here, so there is exactly one place values are enforced and exactly
/// one 422 shape to answer with.
pub(crate) struct AuthorizedLaunch {
    pub pack: PlaybookRow,
    /// The validated `{name: value}` object as stored.
    pub params: serde_json::Value,
    pub max_cost: f64,
    pub max_time: crate::model::MaxTime,
}

/// Resolve the schedule an ad-hoc launch may advance the dedupe state of. A schedule that does not
/// exist, or that runs a different pack, is a field-level refusal: advancing another playbook's
/// cursor from this run's result would poison a recurrence nobody asked to touch.
#[allow(clippy::result_large_err)]
pub(crate) async fn dedupe_target(
    state: &ApiState,
    playbook: &str,
    schedule: Option<&str>,
) -> Result<Option<String>, Response> {
    let Some(id) = schedule.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let refuse = |message: String| {
        invalid_fields(vec![crate::playbooks::registry::FieldError {
            field: "dedupe_schedule".to_string(),
            message,
        }])
    };
    match state.schedules.get(id).await {
        Ok(Some(s)) if s.playbook == playbook => Ok(Some(s.id)),
        Ok(Some(s)) => Err(refuse(format!(
            "schedule {id} runs playbook {}, not {playbook}",
            s.playbook
        ))),
        Ok(None) => Err(refuse(format!("no schedule {id:?}"))),
        Err(e) => Err(AppError::from(e).into_response()),
    }
}

/// The registry half of a launch authorization: the pack, the schema its params are validated
/// against, and the bounded ceilings. `Err` is the response to answer with: 404 for an
/// unregistered id, 409 for a form rendered against a schema the pack no longer serves, 422 for
/// ceilings.
#[allow(clippy::result_large_err)]
pub(crate) async fn authorize_pack(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    max_cost: f64,
    max_time: &str,
    schema_digest: Option<&str>,
) -> Result<(PlaybookRow, serde_json::Value, crate::model::MaxTime), Response> {
    let pack = readable_playbook(state, caller, id, crate::authz::action::Verb::Read).await?;
    if let Some(asked) = schema_digest
        && asked != pack.schema_digest
    {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorBody::new(format!(
                "playbook {id} now serves schema {}, not {asked}; reload the form",
                pack.schema_digest
            ))),
        )
            .into_response());
    }
    if let Some(refusal) = match pack.agent.as_ref() {
        Some(agent) => state.dispatch.refusal(agent),
        None => Some(UNREADABLE_MANIFEST.to_string()),
    } {
        return Err(undispatchable(refusal));
    }
    let max_time = match check_ceilings(&state.playbook_caps, max_cost, max_time) {
        Ok(t) => t,
        Err(field) => return Err(invalid_fields(vec![field])),
    };
    let schema = match crate::playbooks::registry::schema(state.db.pool(), id).await {
        Ok(Some(s)) => s,
        Ok(None) => return Err(not_found(format!("no playbook {id:?}"))),
        Err(e) => return Err(AppError::from(e).into_response()),
    };
    Ok((pack, schema, max_time))
}

/// Validate one launch request against the registry: [`authorize_pack`], then the values.
#[allow(clippy::result_large_err)]
pub(crate) async fn authorize_launch(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    params: &std::collections::BTreeMap<String, String>,
    max_cost: f64,
    max_time: &str,
    schema_digest: Option<&str>,
) -> Result<AuthorizedLaunch, Response> {
    let (pack, schema, max_time) =
        authorize_pack(state, caller, id, max_cost, max_time, schema_digest).await?;
    let params = match crate::playbooks::registry::validate_params(&schema, params) {
        Ok(v) => v,
        Err(fields) => return Err(invalid_fields(fields)),
    };
    Ok(AuthorizedLaunch {
        pack,
        params,
        max_cost,
        max_time,
    })
}
