use crate::api::dto::*;
use crate::dto::dto;

use crate::api::state::*;

use crate::daemon::queue::IssueKey;

use crate::playbooks::api::registry::{
    PackDispatchDto, PlaybookLaunchAck, UNREADABLE_MANIFEST, check_ceilings, undispatchable,
};

use crate::playbooks::co_draft::CoDraft;

use crate::playbooks::drafts::{
    DraftError, DraftFiles, DraftOrigin, DraftRow, DraftSummary, DraftVersionRow, SavedVersion,
    StaleBase,
};

use axum::extract::{Path, Query, State};

use axum::http::{StatusCode, header};

use axum::response::{IntoResponse, Response};

use serde::{Deserialize, Serialize};

use utoipa::ToSchema;

dto! {
    // --- draft packs (the authoring studio: create, save, launch, graduate) --------

    /// A stale-base refusal, as the studio and the MCP client read it: the message plus the version
    /// that overtook the save, so the writer can fetch exactly that version and merge onto it.
    pub struct StaleBaseBody: From<stale: StaleBase> {
        pub error: String = stale.to_string(),
        pub base_version: i64,
        pub current_version: i64,
        pub saved_by: Option<String>,
        pub saved_at: String,
    }
}

/// Map a draft refusal onto the response it deserves.
pub(crate) fn draft_refusal(err: DraftError) -> Response {
    match err {
        DraftError::NotFound(msg) => not_found(msg),
        DraftError::Conflict(msg) => {
            (StatusCode::CONFLICT, Json(ErrorBody::new(msg))).into_response()
        }
        DraftError::StaleBase(stale) => {
            (StatusCode::CONFLICT, Json(StaleBaseBody::from(stale))).into_response()
        }
        DraftError::Invalid(msg) => unprocessable(msg),
        DraftError::Push(msg) => {
            (StatusCode::BAD_GATEWAY, Json(ErrorBody::new(msg))).into_response()
        }
        DraftError::Internal(e) => AppError::from(e).into_response(),
    }
}

/// What a draft is based on: the registered pack it was templated from at the rev it was taken
/// at, or the import row whose frozen tarball seeded it. `moved` is the whole point of carrying
/// the rev — a registry re-pin means the draft is now behind its own origin, which is what the
/// studio offers a rebase against.
#[derive(Debug, Serialize, ToSchema)]
pub struct DraftOriginDto {
    /// `playbook` or `import`.
    pub kind: String,
    pub playbook: Option<String>,
    pub import_id: Option<String>,
    /// Where that pack lives, and the graduation PR target this draft pre-fills.
    pub repo: Option<String>,
    pub path: Option<String>,
    /// The rev the draft was seeded from.
    pub rev: Option<String>,
    /// The rev that pack serves now.
    pub current_rev: Option<String>,
    pub moved: bool,
}

impl From<DraftOrigin> for DraftOriginDto {
    fn from(o: DraftOrigin) -> Self {
        let moved = o.moved();
        DraftOriginDto {
            kind: o.kind.as_str().to_string(),
            playbook: o.playbook,
            import_id: o.import_id,
            repo: o.repo,
            path: o.path,
            rev: o.rev,
            current_rev: o.current_rev,
            moved,
        }
    }
}

/// One draft as the drafts rail lists it. Deliberately its own route: a draft is not registered,
/// and mixing it into `GET /api/playbooks` would make the registry lie about what can be launched
/// from a pin.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlaybookDraftDto {
    pub id: String,
    pub description: String,
    /// What this draft is based on; null when it started from the skeleton.
    pub origin: Option<DraftOriginDto>,
    /// Where graduation exported this draft, and the PR it opened.
    pub graduation_repo: Option<String>,
    pub graduation_path: Option<String>,
    pub graduation_pr_url: Option<String>,
    /// Set once the graduated pack was imported: the draft is read-only from then on.
    pub retired_at: Option<String>,
    /// The newest save; 0 when nothing is saved yet.
    pub latest_version: i64,
    /// True when the newest save compiled, so the draft has a form and a graph to launch from.
    pub compiles: bool,
    pub diagnostics: i64,
    /// `user:<login>` or `team:<slug>`.
    pub owner: String,
    /// What the caller may do with this draft under the policy set in force.
    pub actions: Vec<crate::authz::action::Verb>,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl PlaybookDraftDto {
    fn new(s: DraftSummary, actions: Vec<crate::authz::action::Verb>) -> Self {
        let DraftRow {
            id,
            description,
            origin,
            graduation_repo,
            graduation_path,
            graduation_pr_url,
            retired_at,
            owner,
            created_by,
            created_at,
            updated_at,
        } = s.draft;
        PlaybookDraftDto {
            id,
            description,
            origin: origin.map(DraftOriginDto::from),
            graduation_repo,
            graduation_path,
            graduation_pr_url,
            retired_at,
            latest_version: s.latest_version,
            compiles: s.compiles,
            diagnostics: s.diagnostics as i64,
            owner: owner.to_string(),
            actions,
            created_by,
            created_at,
            updated_at,
        }
    }
}

dto! {
    /// One save in a draft's history.
    pub struct DraftVersionDto: From<v: DraftVersionRow> {
        pub version: i64,
        pub tar_digest: String,
        /// Null when this save did not compile.
        pub schema_digest: Option<String>,
        pub diagnostics: i64 = v.diagnostics.len() as i64,
        pub core_rev: String,
        pub created_by: Option<String>,
        pub created_at: String,
    }
}

/// A draft with its save history.
#[derive(Debug, Serialize, ToSchema)]
pub struct PlaybookDraftDetail {
    #[serde(flatten)]
    pub draft: PlaybookDraftDto,
    pub versions: Vec<DraftVersionDto>,
}

/// What one compile-on-save produced. Diagnostics are the payload, not an error: a save that did
/// not compile still answers 200 with the version it stored and the anchors the editor pins.
#[derive(Debug, Serialize, ToSchema)]
pub struct DraftCompileDto {
    pub version: i64,
    /// Who saved this version: the studio's operator, or the agent that saved over MCP.
    pub saved_by: Option<String>,
    pub saved_at: String,
    pub params_schema: Option<serde_json::Value>,
    pub schema_digest: Option<String>,
    pub graph: Option<crate::playbooks::plan_graph::WorkflowGraphDto>,
    /// Everything wrong with this save, each carrying whether it stopped the compile or only the
    /// dispatch: a save with `compile` diagnostics has nothing to launch, one with only
    /// `dispatch` diagnostics compiled and would still die at agent spawn here.
    pub diagnostics: Vec<crate::playbooks::drafts::Diagnostic>,
    /// What this save's agent needs, against what this deployment can dispatch.
    pub dispatch: PackDispatchDto,
}

impl DraftCompileDto {
    pub(crate) fn from_saved(
        s: SavedVersion,
        cap: &crate::playbooks::dispatch::DispatchCapability,
        catalog: &[crate::images::model::CatalogImage],
    ) -> Self {
        let dispatch = PackDispatchDto::new(s.agent.as_ref(), cap, catalog);
        let mut diagnostics = s.diagnostics;
        diagnostics.extend(crate::playbooks::drafts::dispatch_diagnostics(
            s.graph.as_ref(),
            s.agent.as_ref(),
            cap,
        ));
        DraftCompileDto {
            version: s.version,
            saved_by: s.saved_by,
            saved_at: s.saved_at,
            params_schema: s.params_schema,
            schema_digest: s.schema_digest,
            graph: s.graph,
            diagnostics,
            dispatch,
        }
    }
}

dto! {
    /// A stored version's files, as the editor's tab set. The version and its saver are the base a
    /// writer edits from: the next save carries them back as `base_version`.
    pub struct DraftFilesDto: From<f: DraftFiles> {
        pub version: i64,
        pub saved_by: Option<String>,
        pub saved_at: String,
        pub diagnostics: Vec<crate::playbooks::drafts::Diagnostic>,
        /// `{path: content}`, every file the pack holds.
        pub files: std::collections::BTreeMap<String, String>,
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct CreateDraftBody {
    /// The principal to own it: `user:<login>` or `team:<slug>` the caller acts as; absent means
    /// the caller.
    owner: Option<String>,
    /// The draft id: a lowercase slug, in the same launch-key namespace as a registry id.
    id: String,
    description: String,
    /// A registered playbook to seed version 1 from. Omitted ⇒ a minimal skeleton.
    #[serde(default)]
    template: Option<String>,
    /// Revision rendered by the inspector; cloning refuses if the registry moved meanwhile.
    #[serde(default)]
    template_rev: Option<String>,
    /// Pack digest rendered by the inspector; protects same-revision repoints.
    #[serde(default)]
    template_digest: Option<String>,
}

/// `POST /api/playbook-drafts` — create a draft and store its first version. With `template`, the
/// draft opens on a copy of that registered pack's files; without one, on a skeleton that compiles.
#[utoipa::path(
    post,
    path = "/api/playbook-drafts",
    request_body = CreateDraftBody,
    responses(
        (status = 201, description = "Draft created with version 1", body = DraftCompileDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No playbook with that template id", body = ErrorBody),
        (status = 409, description = "That id is already a draft or a registered playbook", body = ErrorBody),
        (status = 422, description = "Bad id or description", body = ErrorBody)
    )
)]
pub(crate) async fn create_playbook_draft(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Json(body): Json<CreateDraftBody>,
) -> Response {
    if let Some(msg) = require_non_empty(&[("id", &body.id), ("description", &body.description)]) {
        return unprocessable(msg);
    }
    let id = body.id.trim().to_string();
    let template = body
        .template
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let actor = identity.as_deref();
    let owner = match crate::authz::owner::owner_for_create(
        &state,
        &caller,
        body.owner.as_deref(),
        crate::authz::action::ResourceType::PlaybookDraft,
    )
    .await
    {
        Ok(owner) => owner,
        Err(refused) => return refused,
    };
    let seed = match template.as_deref() {
        Some(t) => crate::playbooks::drafts::DraftSeed::Template {
            id: t,
            expected_rev: body.template_rev.as_deref(),
            expected_digest: body.template_digest.as_deref(),
        },
        None => crate::playbooks::drafts::DraftSeed::Skeleton,
    };
    let saved = match crate::playbooks::drafts::create(
        state.db.pool(),
        &id,
        body.description.trim(),
        seed,
        actor,
        &owner,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return draft_refusal(e),
    };

    state
        .audit(
            crate::event_log::Event::now(
                &format!("draft:{id}"),
                "created",
                "created",
                Some(&match template.as_deref() {
                    Some(t) => format!("draft {id} seeded from playbook {t}"),
                    None => format!("draft {id} created"),
                }),
                None,
            )
            .by(actor),
            "create_playbook_draft",
        )
        .await;
    let catalog = match crate::playbooks::api::registry::catalog(&state).await {
        Ok(c) => c,
        Err(refusal) => return refusal,
    };
    (
        StatusCode::CREATED,
        Json(DraftCompileDto::from_saved(
            saved,
            &state.dispatch,
            &catalog,
        )),
    )
        .into_response()
}

/// The draft `id` names, decided for `verb` against its owner; a caller who may not read it is
/// told it does not exist.
#[allow(clippy::result_large_err)]
async fn readable_draft(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    verb: crate::authz::action::Verb,
) -> Result<crate::playbooks::drafts::DraftRow, Response> {
    let row = crate::playbooks::drafts::get(state.db.pool(), id)
        .await
        .map_err(|e| AppError::from(e).into_response())?;
    crate::authz::owner::decide_row(
        state,
        caller,
        crate::authz::action::ResourceType::PlaybookDraft,
        id,
        verb,
        row,
        |r| r.owner.clone(),
    )
    .await
    .map_err(IntoResponse::into_response)
}

/// `GET /api/playbook-drafts` — the drafts rail: what is being authored, and whether it compiles.
#[utoipa::path(
    get,
    path = "/api/playbook-drafts",
    responses((status = 200, description = "Draft packs", body = Vec<PlaybookDraftDto>))
)]
pub(crate) async fn list_playbook_drafts(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
) -> Result<Json<Vec<PlaybookDraftDto>>, AppError> {
    let rows = crate::playbooks::drafts::list(state.db.pool()).await?;
    let rows = crate::authz::owner::readable_with_actions(
        &state,
        &caller,
        crate::authz::action::ResourceType::PlaybookDraft,
        rows,
        |r| {
            crate::authz::decision::Resource::new(
                crate::authz::action::ResourceType::PlaybookDraft,
                &r.draft.id,
                r.draft.owner.clone(),
            )
        },
    )
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(row, actions)| PlaybookDraftDto::new(row, actions))
            .collect(),
    ))
}

/// `GET /api/playbook-drafts/{id}` — one draft with its save history.
#[utoipa::path(
    get,
    path = "/api/playbook-drafts/{id}",
    params(("id" = String, Path, description = "Draft id")),
    responses(
        (status = 200, description = "The draft and its versions", body = PlaybookDraftDetail),
        (status = 404, description = "No draft with that id", body = ErrorBody)
    )
)]
pub(crate) async fn get_playbook_draft(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let draft = match readable_draft(&state, &caller, &id, crate::authz::action::Verb::Read).await {
        Ok(draft) => draft,
        Err(refused) => return Ok(refused),
    };
    let actions = crate::authz::owner::actions_on(
        &state,
        &caller,
        crate::authz::decision::Resource::new(
            crate::authz::action::ResourceType::PlaybookDraft,
            &id,
            draft.owner.clone(),
        ),
    )
    .await?;
    let versions = crate::playbooks::drafts::versions(state.db.pool(), &id).await?;
    let latest = versions.first();
    let summary = DraftSummary {
        latest_version: latest.map_or(0, |v| v.version),
        compiles: latest.is_some_and(|v| v.schema_digest.is_some()),
        diagnostics: latest.map_or(0, |v| v.diagnostics.len()),
        draft,
    };
    Ok(Json(PlaybookDraftDetail {
        draft: PlaybookDraftDto::new(summary, actions),
        versions: versions.into_iter().map(DraftVersionDto::from).collect(),
    })
    .into_response())
}

#[derive(Debug, Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct VersionQuery {
    /// Which save to read; omitted ⇒ the newest.
    #[serde(default)]
    version: Option<i64>,
}

/// `GET /api/playbook-drafts/{id}/files` — a stored version's files, the editor's tab set.
#[utoipa::path(
    get,
    path = "/api/playbook-drafts/{id}/files",
    params(("id" = String, Path, description = "Draft id"), VersionQuery),
    responses(
        (status = 200, description = "The version's files", body = DraftFilesDto),
        (status = 404, description = "No draft or no such version", body = ErrorBody),
        (status = 422, description = "The stored pack holds a file the editor cannot show", body = ErrorBody)
    )
)]
pub(crate) async fn get_playbook_draft_files(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Query(query): Query<VersionQuery>,
) -> Response {
    if let Err(refused) =
        readable_draft(&state, &caller, &id, crate::authz::action::Verb::Read).await
    {
        return refused;
    }
    match crate::playbooks::drafts::files(state.db.pool(), &id, query.version).await {
        Ok(Some(files)) => Json(DraftFilesDto::from(files)).into_response(),
        Ok(None) => not_found(format!("no draft {id:?} at that version")),
        Err(e) => draft_refusal(e),
    }
}

/// `GET /api/playbook-drafts/{id}/preview` — what a stored version compiled to. The studio's first
/// paint reads it, so opening a draft shows the same form, graph and diagnostics the last save did.
#[utoipa::path(
    get,
    path = "/api/playbook-drafts/{id}/preview",
    params(("id" = String, Path, description = "Draft id"), VersionQuery),
    responses(
        (status = 200, description = "The version's form, graph and diagnostics", body = DraftCompileDto),
        (status = 404, description = "No draft or no such version", body = ErrorBody)
    )
)]
pub(crate) async fn get_playbook_draft_preview(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Query(query): Query<VersionQuery>,
) -> Result<Response, AppError> {
    if let Err(refused) =
        readable_draft(&state, &caller, &id, crate::authz::action::Verb::Read).await
    {
        return Ok(refused);
    }
    let catalog = crate::images::store::list_images(state.db.pool()).await?;
    match crate::playbooks::drafts::preview(state.db.pool(), &id, query.version).await? {
        Some(saved) => Ok(Json(DraftCompileDto::from_saved(
            saved,
            &state.dispatch,
            &catalog,
        ))
        .into_response()),
        None => Ok(not_found(format!("no draft {id:?} at that version"))),
    }
}

/// The origin pack's files as they stand now: the left-hand side of the studio's rebase diff.
#[derive(Debug, Serialize, ToSchema)]
pub struct DraftOriginFilesDto {
    /// `playbook` or `import`.
    pub kind: String,
    /// The pack these files were read from: a registry id, or an import id.
    pub reference: String,
    /// The rev they are at.
    pub rev: Option<String>,
    pub files: std::collections::BTreeMap<String, String>,
}

/// `GET /api/playbook-drafts/{id}/origin/files` — the draft's origin pack as it stands now. The
/// studio diffs it against the buffers when the origin re-pinned under the draft.
#[utoipa::path(
    get,
    path = "/api/playbook-drafts/{id}/origin/files",
    params(("id" = String, Path, description = "Draft id")),
    responses(
        (status = 200, description = "The origin pack's files", body = DraftOriginFilesDto),
        (status = 404, description = "No draft, no origin, or an origin that no longer exists", body = ErrorBody),
        (status = 422, description = "The origin pack holds a file the editor cannot show", body = ErrorBody)
    )
)]
pub(crate) async fn get_playbook_draft_origin_files(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Response {
    if let Err(refused) =
        readable_draft(&state, &caller, &id, crate::authz::action::Verb::Read).await
    {
        return refused;
    }
    match crate::playbooks::drafts::origin_files(state.db.pool(), &id).await {
        Ok(Some(origin)) => Json(DraftOriginFilesDto {
            kind: origin.kind.as_str().to_string(),
            reference: origin.reference,
            rev: origin.rev,
            files: origin.files,
        })
        .into_response(),
        Ok(None) => not_found(format!("draft {id:?} has no origin to rebase onto")),
        Err(e) => draft_refusal(e),
    }
}

/// `GET /api/playbook-drafts/{id}/tarball` — one save as the bytes it stored. The same pack the
/// launch path materializes, handed to whoever wants it in their own editor or their own CI.
#[utoipa::path(
    get,
    path = "/api/playbook-drafts/{id}/tarball",
    params(("id" = String, Path, description = "Draft id"), VersionQuery),
    responses(
        (status = 200, description = "The version's pack tarball", content_type = "application/gzip"),
        (status = 404, description = "No draft or no such version", body = ErrorBody)
    )
)]
pub(crate) async fn get_playbook_draft_tarball(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Query(query): Query<VersionQuery>,
) -> Result<Response, AppError> {
    if let Err(refused) =
        readable_draft(&state, &caller, &id, crate::authz::action::Verb::Read).await
    {
        return Ok(refused);
    }
    let Some((version, tar_gz)) =
        crate::playbooks::drafts::tarball(state.db.pool(), &id, query.version).await?
    else {
        return Ok(not_found(format!("no draft {id:?} at that version")));
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"{}-v{version}.tar.gz\"",
                    tarball_name(&id)
                ),
            ),
        ],
        tar_gz,
    )
        .into_response())
}

/// A draft id is a validated slug, but the download names a file with it, so anything that is not
/// one is flattened rather than trusted into a header.
fn tarball_name(id: &str) -> String {
    let name: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() {
        "draft".to_string()
    } else {
        name
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct SaveDraftBody {
    /// The whole pack, `{path: content}`. A save is the full tree, so a file dropped from the map
    /// is a file dropped from the pack.
    files: std::collections::BTreeMap<String, String>,
    /// The version this save edited from. A base that is no longer the newest save is refused with
    /// the version that overtook it; omitted appends blind.
    #[serde(default)]
    base_version: Option<i64>,
}

/// `POST /api/playbook-drafts/{id}/versions` — compile-on-save. The file map is tarred, compiled
/// with the pinned engine, and stored as the next version whether or not it compiled: the
/// diagnostics come back with their `file:line:col` anchors so the editor can pin them, and the
/// form and graph come back for the previews beside it. `base_version` is the version the writer
/// edited from: another editor's save landing first refuses this one with that version instead of
/// overwriting it.
#[utoipa::path(
    post,
    path = "/api/playbook-drafts/{id}/versions",
    params(("id" = String, Path, description = "Draft id")),
    request_body = SaveDraftBody,
    responses(
        (status = 200, description = "The version stored, and what the engine made of it", body = DraftCompileDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No draft with that id", body = ErrorBody),
        (status = 409, description = "A newer save overtook `base_version`, or the draft retired when its graduated pack was imported", body = StaleBaseBody),
        (status = 422, description = "A file path or size the draft cannot hold", body = ErrorBody)
    )
)]
pub(crate) async fn save_playbook_draft(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<SaveDraftBody>,
) -> Response {
    if let Err(refused) =
        readable_draft(&state, &caller, &id, crate::authz::action::Verb::Update).await
    {
        return refused;
    }
    let catalog = match crate::playbooks::api::registry::catalog(&state).await {
        Ok(c) => c,
        Err(refusal) => return refusal,
    };
    match crate::playbooks::drafts::save_version(
        state.db.pool(),
        &id,
        body.files,
        identity.as_deref(),
        body.base_version,
    )
    .await
    {
        Ok(saved) => Json(DraftCompileDto::from_saved(
            saved,
            &state.dispatch,
            &catalog,
        ))
        .into_response(),
        Err(e) => draft_refusal(e),
    }
}

/// `DELETE /api/playbook-drafts/{id}` — drop a draft and every version it holds.
#[utoipa::path(
    delete,
    path = "/api/playbook-drafts/{id}",
    params(("id" = String, Path, description = "Draft id")),
    responses(
        (status = 204, description = "Draft deleted"),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No draft with that id", body = ErrorBody)
    )
)]
pub(crate) async fn delete_playbook_draft(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    if let Err(refused) =
        readable_draft(&state, &caller, &id, crate::authz::action::Verb::Delete).await
    {
        return Ok(refused);
    }
    if !crate::playbooks::drafts::delete(state.db.pool(), &id).await? {
        return Ok(not_found(format!("no draft {id:?}")));
    }
    state
        .audit(
            crate::event_log::Event::now(
                &format!("draft:{id}"),
                "deleted",
                "deleted",
                Some(&format!("draft {id} deleted")),
                None,
            )
            .by(identity.as_deref()),
            "delete_playbook_draft",
        )
        .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct LaunchDraftBody {
    #[serde(default)]
    params: std::collections::BTreeMap<String, String>,
    max_cost: f64,
    max_time: String,
    /// The schema digest the form was rendered against. A save that landed since is a 409.
    #[serde(default)]
    schema_digest: Option<String>,
    /// Which cluster to dispatch onto, from `GET /api/dispatch-targets`. Absent selects the
    /// controller's configured default.
    #[serde(default)]
    dispatch_target: Option<String>,
    /// Which registered inference provider this test-fire runs against, from
    /// `GET /api/config/providers`. Absent resolves through the configured defaults at dispatch.
    #[serde(default)]
    provider: Option<String>,
    /// The model to ask that provider for. Free text; absent takes the provider's own default.
    #[serde(default)]
    model: Option<String>,
}

/// `POST /api/playbook-drafts/{id}/launch` — test-fire a draft. The values are validated against
/// the newest save's stored schema and the ceilings are bounded by the same admin caps a
/// registered launch is held to: a draft launch is a launch, not a bypass.
#[utoipa::path(
    post,
    path = "/api/playbook-drafts/{id}/launch",
    params(("id" = String, Path, description = "Draft id")),
    request_body = LaunchDraftBody,
    responses(
        (status = 201, description = "Launch adopted; the run dispatches on the next reconcile", body = PlaybookLaunchAck),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No draft with that id", body = ErrorBody),
        (status = 409, description = "A save landed under the launcher", body = ErrorBody),
        (status = 422, description = "The newest save never compiled, or values/ceilings were refused", body = ValidationErrorBody)
    )
)]
pub(crate) async fn launch_playbook_draft(
    State(state): State<ApiState>,
    groups: crate::identity::auth::Groups,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<LaunchDraftBody>,
) -> Response {
    let draft = match readable_draft(&state, &caller, &id, crate::authz::action::Verb::Launch).await
    {
        Ok(d) => d,
        Err(refused) => return refused,
    };
    if draft.retired_at.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(ErrorBody::new(format!(
                "draft {id} retired when its graduated pack was imported; launch the registered pack"
            ))),
        )
            .into_response();
    }
    let latest = match crate::playbooks::drafts::latest(state.db.pool(), &id).await {
        Ok(Some(l)) => l,
        Ok(None) => return not_found(format!("no draft {id:?}")),
        Err(e) => return AppError::from(e).into_response(),
    };
    let (Some(schema), Some(schema_digest)) = (latest.params_schema, latest.schema_digest) else {
        return invalid_fields(vec![crate::playbooks::registry::FieldError {
            field: String::new(),
            message: format!(
                "version {} of draft {id} did not compile, so there is no form to launch it with",
                latest.version
            ),
        }]);
    };
    if let Some(asked) = body.schema_digest.as_deref()
        && asked != schema_digest
    {
        return (
            StatusCode::CONFLICT,
            Json(ErrorBody::new(format!(
                "draft {id} now serves schema {schema_digest}, not {asked}; reload the form"
            ))),
        )
            .into_response();
    }
    if let Some(refusal) = match latest.agent.as_ref() {
        Some(agent) => state.dispatch.refusal(agent),
        None => Some(UNREADABLE_MANIFEST.to_string()),
    } {
        return undispatchable(refusal);
    }
    let max_time = match check_ceilings(&state.playbook_caps, body.max_cost, &body.max_time) {
        Ok(t) => t,
        Err(field) => return invalid_fields(vec![field]),
    };
    let params = match crate::playbooks::registry::validate_params(&schema, &body.params) {
        Ok(v) => v,
        Err(fields) => return invalid_fields(fields),
    };

    let saver = match crate::playbooks::api::saver::resolve_saver(
        &state,
        &caller,
        &groups,
        crate::playbooks::api::saver::Ownership::Session,
        crate::playbooks::api::saver::SaverRequest {
            agent: latest.agent.clone(),
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
    let image = match crate::playbooks::api::registry::authorize_image(
        &state,
        latest.agent.as_ref(),
        saver.provider.as_deref(),
        draft.graduation_repo.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        Err(refusal) => return refusal,
    };
    let actor = saver.actor.as_deref();
    let dispatch_target = saver.dispatch_target.clone();
    let key = format!("playbook:{id}:{}", uuid::Uuid::now_v7());
    let title = format!("draft {id}: {}", draft.description);
    let launch = crate::launches::model::NewPlaybookLaunch {
        playbook: &id,
        repo: draft.graduation_repo.as_deref().unwrap_or("(draft)"),
        title: &title,
        params: &params,
        schema_digest: &schema_digest,
        max_cost: body.max_cost,
        max_time: &max_time,
        advance_dedupe: false,
        dedupe_schedule: None,
        origin: crate::model::LaunchOrigin::Draft,
        draft_version: Some(latest.version),
        created_by: actor,
        launcher_groups: None,
    };
    // A draft's content changes on every save, so the disclosure is recomputed from the version
    // being launched and lands in the same transaction that adopts it, before anything executes.
    let exposure =
        match crate::playbooks::drafts::exposure_of(state.db.pool(), &id, latest.version).await {
            Ok(Some(extraction)) => extraction,
            Ok(None) => return not_found(format!("no draft {id:?}")),
            Err(crate::playbooks::drafts::DraftError::Invalid(message)) => {
                return invalid_fields(vec![crate::playbooks::registry::FieldError {
                    field: String::new(),
                    message,
                }]);
            }
            Err(e) => return AppError::from(anyhow::Error::new(e)).into_response(),
        };
    let exposure_dto = match ExposureDto::try_from(&exposure) {
        Ok(dto) => dto,
        Err(e) => return AppError::from(e).into_response(),
    };

    use crate::launches::store::AdoptPlaybookOutcome;
    match crate::launches::store::adopt_draft_launch(state.db.pool(), &key, &launch, &exposure)
        .await
    {
        Ok(AdoptPlaybookOutcome::Adopted) => {
            if let Err(refusal) =
                crate::playbooks::api::saver::pin_dispatch(&state, &key, &saver).await
            {
                return refusal;
            }
        }
        Ok(AdoptPlaybookOutcome::UnknownPlaybook) => return not_found(format!("no draft {id:?}")),
        Ok(AdoptPlaybookOutcome::SchemaDrifted { current }) => {
            return (
                StatusCode::CONFLICT,
                Json(ErrorBody::new(format!(
                    "draft {id} was saved while the launch was being authorized (now {current}); \
                     reload the form"
                ))),
            )
                .into_response();
        }
        Err(e) => return AppError::from(e).into_response(),
    }

    state
        .audit(
            crate::event_log::Event::now(
                &key,
                "new",
                "new",
                Some(&format!(
                    "draft {id} launched at version {} ({} params, max_cost {}, max_time {max_time})",
                    latest.version,
                    body.params.len(),
                    body.max_cost
                )),
                None,
            )
            .by(actor),
            "launch_playbook_draft",
        )
        .await;
    state.queue.enqueue_urgent(IssueKey(key.clone()));

    (
        StatusCode::CREATED,
        Json(PlaybookLaunchAck {
            key,
            playbook: id,
            params: body.params,
            schema_digest,
            max_cost: body.max_cost,
            max_time: max_time.as_str().to_string(),
            advance_dedupe: false,
            dedupe_schedule: None,
            dispatch_target,
            provider: saver.provider,
            model: saver.model,
            actor: saver.actor,
            exposure: Some(exposure_dto),
            image,
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct GraduateDraftBody {
    /// `owner/repo` the export PR opens against.
    repo: String,
    /// The pack directory inside that repo; omitted ⇒ the repo root.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GraduateAck {
    pub pr_url: String,
}

/// `POST /api/playbook-drafts/{id}/graduate` — export a draft as a PR. The newest compiling
/// version is pushed as a branch pair under `path`, and the draft retires by itself once that
/// merged pack is imported from the same repo/path.
#[utoipa::path(
    post,
    path = "/api/playbook-drafts/{id}/graduate",
    params(("id" = String, Path, description = "Draft id")),
    request_body = GraduateDraftBody,
    responses(
        (status = 200, description = "The export PR", body = GraduateAck),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody),
        (status = 404, description = "No draft with that id", body = ErrorBody),
        (status = 409, description = "Already graduated; the body carries the open PR", body = ErrorBody),
        (status = 422, description = "Bad repo/path, or nothing that compiled to export", body = ErrorBody),
        (status = 502, description = "The push or the PR open failed", body = ErrorBody)
    )
)]
pub(crate) async fn graduate_playbook_draft(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<GraduateDraftBody>,
) -> Response {
    if let Err(refused) =
        readable_draft(&state, &caller, &id, crate::authz::action::Verb::Approve).await
    {
        return refused;
    }
    if let Some(msg) = require_non_empty(&[("repo", &body.repo)]) {
        return unprocessable(msg);
    }
    let repo = normalize_repo(body.repo.trim()).to_string();
    let path = body.path.unwrap_or_default().trim().to_string();
    let token =
        match crate::runs::engine::resolve_pack_pr_token_for(state.pack_pr_app.as_ref()).await {
            Ok(t) => t,
            Err(e) => return AppError::from(e).into_response(),
        };

    let actor = identity.as_deref();
    let url =
        match crate::playbooks::drafts::graduate(state.db.pool(), &id, &repo, &path, token).await {
            Ok(url) => url,
            Err(e) => return draft_refusal(e),
        };

    state
        .audit(
            crate::event_log::Event::now(
                &format!("draft:{id}"),
                "graduated",
                "graduated",
                Some(&format!("draft {id} exported to {repo} as {url}")),
                None,
            )
            .by(actor),
            "graduate_playbook_draft",
        )
        .await;
    Json(GraduateAck { pr_url: url }).into_response()
}

// --- co-drafting with a local agent ------------------------------------------

/// The base URL the co-draft handoff is rendered against: the deploy's own
/// `CONTROLLER_PUBLIC_URL` when it set one, else the host this request arrived on, so a laptop
/// deployment that configures nothing still hands out commands that reach it.
fn co_draft(state: &ApiState, headers: &axum::http::HeaderMap) -> CoDraft {
    let base = state
        .public_url
        .clone()
        .unwrap_or_else(|| request_base(headers));
    CoDraft::new(
        &base,
        crate::identity::api::keys::configured_mcp_url().as_deref(),
    )
}

/// A host the caller asserted ends up inside commands a human pastes into a shell, so anything
/// that is not a plain host:port is dropped rather than echoed.
fn request_base(headers: &axum::http::HeaderMap) -> String {
    let read = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let scheme = match read("x-forwarded-proto") {
        Some("https") => "https",
        _ => "http",
    };
    let host = read("x-forwarded-host")
        .or_else(|| read(header::HOST.as_str()))
        .filter(|h| {
            h.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '[' | ']'))
        })
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

/// `GET /api/playbooks/drafts/skill` — the co-draft skill file, generated with this deployment's
/// own URL in it. Downloaded by a human and dropped into their agent's skills directory; the agent
/// reads it and knows how to hold this controller's draft tools.
#[utoipa::path(
    get,
    path = "/api/playbooks/drafts/skill",
    responses((status = 200, description = "The generated SKILL.md", content_type = "text/markdown"))
)]
pub(crate) async fn get_co_draft_skill(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "text/markdown; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"{}\"",
                    crate::playbooks::co_draft::SKILL_FILENAME
                ),
            ),
        ],
        co_draft(&state, &headers).skill_markdown(),
    )
        .into_response()
}

/// One setup step as the drafts-page hint lists it.
#[derive(Debug, Serialize, ToSchema)]
pub struct CoDraftStepDto {
    pub label: String,
    pub commands: Vec<String>,
}

/// What the drafts page needs to render the co-draft hint: the same commands the skill file
/// carries, so a human and their agent are never told two different things.
#[derive(Debug, Serialize, ToSchema)]
pub struct CoDraftDto {
    /// This controller's own external base URL, as every command below spells it.
    pub url: String,
    /// Where the generated skill file downloads from.
    pub skill_url: String,
    pub steps: Vec<CoDraftStepDto>,
    pub example_prompt: String,
}

/// `GET /api/playbooks/drafts/co-draft` — the setup the hint above the create form shows.
#[utoipa::path(
    get,
    path = "/api/playbooks/drafts/co-draft",
    responses((status = 200, description = "The MCP setup for this deployment", body = CoDraftDto))
)]
pub(crate) async fn get_co_draft(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let co = co_draft(&state, &headers);
    Json(CoDraftDto {
        url: co.base().to_string(),
        skill_url: co.skill_url(),
        steps: co
            .setup()
            .into_iter()
            .map(|step| CoDraftStepDto {
                label: step.label,
                commands: step.commands,
            })
            .collect(),
        example_prompt: co.example_prompt(),
    })
    .into_response()
}
