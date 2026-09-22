use crate::api::dto::*;
use crate::api::state::*;
use crate::playbooks::api::drafts::{DraftCompileDto, draft_refusal};
use crate::playbooks::api::registry::{PackDispatchDto, RegisterAck};
use crate::playbooks::registry::RegisterError;
use crate::secrets::api::PackSecretsDto;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// --- pack import (the preview gate ahead of POST /api/playbooks) ---------------

/// Map a fetch/compile refusal onto the response it deserves. 422 is the caller's URL, ref or
/// path; 502 is the git remote; a moved ref is 409 so the wizard re-previews rather than pinning
/// bytes nobody looked at.
pub(crate) fn import_refusal(err: RegisterError) -> Response {
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

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ImportCandidatesBody {
    /// `owner/repo` or a clone URL.
    repo: String,
    /// Branch or tag. Omitted ⇒ the repo's default branch.
    #[serde(default)]
    git_ref: Option<String>,
}

/// One pack directory an import could register.
#[derive(Debug, Serialize, ToSchema)]
pub struct ImportCandidateDto {
    /// The pack directory inside the repo; empty = the repo root.
    pub path: String,
    /// The `[workflow].file` the pack declares, relative to `path`.
    pub workflow_file: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ImportCandidatesDto {
    /// The commit the ref resolved to. A registration quotes it back as `expected_rev`.
    pub rev: String,
    pub candidates: Vec<ImportCandidateDto>,
}

/// `POST /api/playbooks/import/candidates` — clone a repo at a ref and list the directories that
/// hold a registrable playbook pack. Nothing is stored: this is the first step of the import
/// wizard, and the clone is scratch.
#[utoipa::path(
    post,
    path = "/api/playbooks/import/candidates",
    request_body = ImportCandidatesBody,
    responses(
        (status = 200, description = "The pack directories found at this ref", body = ImportCandidatesDto),
        (status = 403, description = "Caller is not in the operator whitelist", body = ErrorBody),
        (status = 422, description = "Bad repo or ref", body = ErrorBody),
        (status = 502, description = "Cloning the repo failed", body = ErrorBody)
    )
)]
pub(crate) async fn import_candidates(
    State(state): State<ApiState>,
    _identity: crate::identity::session::Identity,
    _operator: crate::identity::auth::OperatorGuard,
    Json(body): Json<ImportCandidatesBody>,
) -> Response {
    if let Some(msg) = require_non_empty(&[("repo", &body.repo)]) {
        return unprocessable(msg);
    }
    let git_ref = match require_git_ref(body.git_ref.as_deref()) {
        Ok(r) => r,
        Err(msg) => {
            return unprocessable(msg);
        }
    };
    let repo = normalize_repo(body.repo.trim()).to_string();
    let git = match crate::playbooks::registry::PackGit::resolve(state.pack_pr_app.as_ref()).await {
        Ok(g) => g,
        Err(e) => return AppError::from(e).into_response(),
    };

    let found = tokio::task::spawn_blocking(move || {
        let checkout = git.fetch_checkout(&repo, git_ref.as_deref())?;
        let candidates = crate::playbooks::registry::enumerate_candidates(&checkout.path());
        Ok::<_, RegisterError>((checkout.rev.clone(), candidates))
    })
    .await;
    let found = match found {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return import_refusal(e),
        Err(e) => {
            return AppError::from(anyhow::Error::new(e).context("joining the import worker"))
                .into_response();
        }
    };

    let (rev, candidates) = found;
    Json(ImportCandidatesDto {
        rev,
        candidates: candidates
            .into_iter()
            .map(|c| ImportCandidateDto {
                path: c.path,
                workflow_file: c.workflow_file,
            })
            .collect(),
    })
    .into_response()
}

// --- durable imports (the row the wizard, a shared link and the rail all read) --

use crate::playbooks::imports::{ImportError, PackImport};

/// Map an import refusal onto the response it deserves, handing the wrapped registration and
/// draft refusals to the mappers those paths already own.
fn import_error(err: ImportError) -> Response {
    match err {
        ImportError::NotFound(msg) => not_found(msg),
        ImportError::Conflict(msg) => {
            (StatusCode::CONFLICT, Json(ErrorBody::new(msg))).into_response()
        }
        ImportError::Register(e) => import_refusal(e),
        ImportError::Draft(e) => draft_refusal(e),
        ImportError::Internal(e) => AppError::from(e).into_response(),
    }
}

/// One proposed import: what was fetched, what the pinned engine made of it, and where it stands.
#[derive(Debug, Serialize, ToSchema)]
pub struct PackImportDto {
    pub id: String,
    pub repo: String,
    pub git_ref: Option<String>,
    pub path: String,
    /// The commit everything below was taken at; the preview is this pack even after the ref moves.
    pub rev: String,
    pub tar_digest: String,
    /// The engine's params JSON Schema; null when the source did not compile.
    pub params_schema: Option<serde_json::Value>,
    pub schema_digest: Option<String>,
    /// The compiled plan as nodes and edges; null when there is none.
    pub graph: Option<crate::playbooks::plan_graph::WorkflowGraphDto>,
    /// The engine's own diagnostics, verbatim.
    pub diagnostics: Vec<String>,
    /// What the proposed pack's agent needs, against what this deployment can dispatch.
    pub dispatch: PackDispatchDto,
    /// The credentials the pack declares, and what the deploy profile already fills. Frozen with
    /// the rest of the preview, so the gate names them before anything is recompiled.
    pub secrets: PackSecretsDto,
    /// What a run of the proposed pack would be allowed to write and reach, and the digest to
    /// compare against the id this registration would bump.
    pub exposure: ExposureDto,
    pub core_rev: String,
    /// `pending`, `registered` or `discarded`.
    pub status: String,
    /// The registry id this import registered as, once it did.
    pub playbook: Option<String>,
    /// The draft seeded from this import's frozen tarball, once one was.
    pub draft_id: Option<String>,
    /// `user:<login>` or `team:<slug>`.
    pub owner: String,
    pub proposed_by: Option<String>,
    pub created_at: String,
    pub resolved_by: Option<String>,
    pub resolved_at: Option<String>,
}

impl PackImportDto {
    fn from_row(
        i: PackImport,
        cap: &crate::playbooks::dispatch::DispatchCapability,
        profile_env: &[String],
        catalog: &[crate::images::model::CatalogImage],
    ) -> Self {
        let dispatch = PackDispatchDto::new(i.agent.as_ref(), cap, catalog);
        let secrets = PackSecretsDto::new(&i.declared_secrets, profile_env);
        PackImportDto {
            id: i.id,
            repo: i.repo,
            git_ref: i.git_ref,
            path: i.path,
            rev: i.rev,
            tar_digest: i.tar_digest,
            params_schema: i.params_schema,
            schema_digest: i.schema_digest,
            graph: i.graph,
            diagnostics: i.diagnostics,
            dispatch,
            secrets,
            exposure: ExposureDto::new(i.exposure, i.exposure_digest),
            core_rev: i.core_rev,
            status: i.status.as_str().to_string(),
            playbook: i.playbook,
            draft_id: i.draft_id,
            owner: i.owner.to_string(),
            proposed_by: i.proposed_by,
            created_at: i.created_at,
            resolved_by: i.resolved_by,
            resolved_at: i.resolved_at,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ProposeImportBody {
    /// The principal to own it: `user:<login>` or `team:<slug>` the caller acts as; absent means
    /// the caller.
    owner: Option<String>,
    /// `owner/repo` or a clone URL.
    repo: String,
    #[serde(default)]
    git_ref: Option<String>,
    /// The pack directory inside the repo; omitted ⇒ the repo root.
    #[serde(default)]
    path: Option<String>,
}

/// `POST /api/playbooks/imports` — fetch a pack at a ref, compile it with the pinned engine, and
/// store both as a pending import. The row is the preview: its id is a link anyone can open, and
/// everything on it is frozen at the commit the fetch resolved to. A source the engine refuses
/// still lands a row carrying that refusal — showing the `file:line:col` error is what the gate
/// is for.
#[utoipa::path(
    post,
    path = "/api/playbooks/imports",
    request_body = ProposeImportBody,
    responses(
        (status = 201, description = "The pending import", body = PackImportDto),
        (status = 403, description = "Caller is not in the operator whitelist", body = ErrorBody),
        (status = 422, description = "Bad repo/ref, or a path that is not a playbook pack", body = ErrorBody),
        (status = 502, description = "Cloning the repo failed", body = ErrorBody)
    )
)]
pub(crate) async fn propose_pack_import(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _operator: crate::identity::auth::OperatorGuard,
    caller: crate::authz::Caller,
    Json(body): Json<ProposeImportBody>,
) -> Response {
    if let Some(msg) = require_non_empty(&[("repo", &body.repo)]) {
        return unprocessable(msg);
    }
    let git_ref = match require_git_ref(body.git_ref.as_deref()) {
        Ok(r) => r,
        Err(msg) => {
            return unprocessable(msg);
        }
    };
    let repo = normalize_repo(body.repo.trim()).to_string();
    let path = body.path.unwrap_or_default().trim().to_string();
    let actor = identity.as_deref();
    let owner = match crate::authz::owner::owner_for_create(
        &state,
        &caller,
        body.owner.as_deref(),
        crate::authz::action::ResourceType::PackImport,
    )
    .await
    {
        Ok(owner) => owner,
        Err(refused) => return refused,
    };

    let git = match crate::playbooks::registry::PackGit::resolve(state.pack_pr_app.as_ref()).await {
        Ok(g) => g,
        Err(e) => return AppError::from(e).into_response(),
    };
    let import = match crate::playbooks::imports::propose(
        state.db.pool(),
        &git,
        crate::playbooks::registry::PackSource {
            repo: &repo,
            git_ref: git_ref.as_deref(),
            path: &path,
            expected_rev: None,
        },
        actor,
        &owner,
    )
    .await
    {
        Ok(i) => i,
        Err(e) => return import_error(e),
    };

    state
        .audit(
            crate::event_log::Event::now(
                &format!("import:{}", import.id),
                "pending",
                "pending",
                Some(&format!(
                    "pack import proposed from {repo} at {} ({} diagnostics)",
                    import.rev,
                    import.diagnostics.len()
                )),
                None,
            )
            .by(actor),
            "propose_pack_import",
        )
        .await;
    let catalog = match crate::playbooks::api::registry::catalog(&state).await {
        Ok(c) => c,
        Err(refusal) => return refusal,
    };
    (
        StatusCode::CREATED,
        Json(PackImportDto::from_row(
            import,
            &state.dispatch,
            &state.profile_secret_env,
            &catalog,
        )),
    )
        .into_response()
}

/// `GET /api/playbooks/imports/{id}` — the preview gate a shared link opens: the form, the graph,
/// the diagnostics and where the proposal stands.
/// The import `id` names, decided for `verb` against its owner; a caller who may not read it is
/// told it does not exist.
#[allow(clippy::result_large_err)]
async fn readable_import(
    state: &ApiState,
    caller: &crate::authz::Caller,
    id: &str,
    verb: crate::authz::action::Verb,
) -> Result<crate::playbooks::imports::PackImport, Response> {
    let row = crate::playbooks::imports::get(state.db.pool(), id)
        .await
        .map_err(|e| AppError::from(e).into_response())?;
    crate::authz::owner::decide_row(
        state,
        caller,
        crate::authz::action::ResourceType::PackImport,
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
    path = "/api/playbooks/imports/{id}",
    params(("id" = String, Path, description = "Import id")),
    responses(
        (status = 200, description = "The import", body = PackImportDto),
        (status = 404, description = "No import with that id", body = ErrorBody)
    )
)]
pub(crate) async fn get_pack_import(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let import = match readable_import(&state, &caller, &id, crate::authz::action::Verb::Read).await
    {
        Ok(import) => import,
        Err(refused) => return Ok(refused),
    };
    let catalog = crate::images::store::list_images(state.db.pool()).await?;
    Ok(Json(PackImportDto::from_row(
        import,
        &state.dispatch,
        &state.profile_secret_env,
        &catalog,
    ))
    .into_response())
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct CompileImportBody {
    /// Values to compile the graph with, `{name: value}`.
    #[serde(default)]
    params: std::collections::BTreeMap<String, String>,
}

/// What a compile of the frozen tarball produced.
#[derive(Debug, Serialize, ToSchema)]
pub struct ImportCompileDto {
    pub params_schema: Option<serde_json::Value>,
    pub schema_digest: Option<String>,
    pub graph: Option<crate::playbooks::plan_graph::WorkflowGraphDto>,
    pub diagnostics: Vec<String>,
    pub dispatch: PackDispatchDto,
    /// The credentials the pack declares, and what the deploy profile already fills.
    pub secrets: PackSecretsDto,
    pub exposure: ExposureDto,
}

/// `POST /api/playbooks/imports/{id}/compile` — recompile the stored pack with parameter values.
/// A pack with required parameters compiles no plan until they are supplied, so the gate asks for
/// this rather than re-fetching the ref; nothing is stored and the bytes stay the frozen ones.
#[utoipa::path(
    post,
    path = "/api/playbooks/imports/{id}/compile",
    params(("id" = String, Path, description = "Import id")),
    request_body = CompileImportBody,
    responses(
        (status = 200, description = "The form, graph and diagnostics for these values", body = ImportCompileDto),
        (status = 403, description = "Caller is not in the operator whitelist", body = ErrorBody),
        (status = 404, description = "No import with that id", body = ErrorBody),
        (status = 422, description = "The stored pack is not compilable", body = ErrorBody)
    )
)]
pub(crate) async fn compile_pack_import(
    State(state): State<ApiState>,
    _operator: crate::identity::auth::OperatorGuard,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<CompileImportBody>,
) -> Response {
    if let Err(refused) =
        readable_import(&state, &caller, &id, crate::authz::action::Verb::Update).await
    {
        return refused;
    }
    let catalog = match crate::playbooks::api::registry::catalog(&state).await {
        Ok(c) => c,
        Err(refusal) => return refusal,
    };
    match crate::playbooks::imports::compile(state.db.pool(), &id, body.params).await {
        Ok(preview) => Json(ImportCompileDto {
            params_schema: preview.params_schema,
            schema_digest: preview.schema_digest,
            graph: preview.graph,
            diagnostics: preview.diagnostics,
            dispatch: PackDispatchDto::new(preview.agent.as_ref(), &state.dispatch, &catalog),
            secrets: PackSecretsDto::new(&preview.declared_secrets, &state.profile_secret_env),
            exposure: ExposureDto::new(preview.exposure, preview.exposure_digest),
        })
        .into_response(),
        Err(e) => import_error(e),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct RegisterImportBody {
    /// The registry id to register under.
    id: String,
    description: String,
}

/// `POST /api/playbooks/imports/{id}/register` — complete a pending import through the one
/// registration path, pinned at the rev it was proposed at, and stamp the row registered. A ref
/// that moved since the proposal is a 409: the bytes nobody looked at are not what gets pinned.
#[utoipa::path(
    post,
    path = "/api/playbooks/imports/{id}/register",
    params(("id" = String, Path, description = "Import id")),
    request_body = RegisterImportBody,
    responses(
        (status = 201, description = "Registered", body = RegisterAck),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody),
        (status = 404, description = "No import with that id", body = ErrorBody),
        (status = 409, description = "The import is already resolved, the ref moved, or a live draft holds the id", body = ErrorBody),
        (status = 422, description = "Bad registry id or description", body = ErrorBody),
        (status = 502, description = "Cloning the repo failed", body = ErrorBody)
    )
)]
pub(crate) async fn register_pack_import(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<RegisterImportBody>,
) -> Response {
    if let Err(refused) =
        readable_import(&state, &caller, &id, crate::authz::action::Verb::Approve).await
    {
        return refused;
    }
    if let Some(msg) = require_non_empty(&[("id", &body.id), ("description", &body.description)]) {
        return unprocessable(msg);
    }
    let actor = identity.as_deref();
    let git = match crate::playbooks::registry::PackGit::resolve(state.pack_pr_app.as_ref()).await {
        Ok(g) => g,
        Err(e) => return AppError::from(e).into_response(),
    };
    let registered = match crate::playbooks::imports::register(
        state.db.pool(),
        &git,
        &id,
        body.id.trim(),
        body.description.trim(),
        actor,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return import_error(e),
    };

    state
        .audit(
            crate::event_log::Event::now(
                &format!("import:{id}"),
                "pending",
                "registered",
                Some(&format!(
                    "pack import registered as playbook {} at {}",
                    registered.id, registered.rev
                )),
                None,
            )
            .by(actor),
            "register_pack_import",
        )
        .await;
    (
        StatusCode::CREATED,
        Json(RegisterAck {
            id: registered.id,
            rev: registered.rev,
            tar_digest: registered.tar_digest,
            schema_digest: registered.schema_digest,
            schema_changed: registered.schema_changed,
            exposure_digest: registered.exposure_digest,
            exposure_changed: registered.exposure_changed,
        }),
    )
        .into_response()
}

/// `POST /api/playbooks/imports/{id}/discard` — refuse a proposal. The row stays for the audit
/// trail and takes no further transitions.
#[utoipa::path(
    post,
    path = "/api/playbooks/imports/{id}/discard",
    params(("id" = String, Path, description = "Import id")),
    responses(
        (status = 200, description = "The discarded import", body = PackImportDto),
        (status = 403, description = "Caller is not in the operator whitelist", body = ErrorBody),
        (status = 404, description = "No import with that id", body = ErrorBody),
        (status = 409, description = "The import is already resolved", body = ErrorBody)
    )
)]
pub(crate) async fn discard_pack_import(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _operator: crate::identity::auth::OperatorGuard,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
) -> Response {
    if let Err(refused) =
        readable_import(&state, &caller, &id, crate::authz::action::Verb::Update).await
    {
        return refused;
    }
    let actor = identity.as_deref();
    let import = match crate::playbooks::imports::discard(state.db.pool(), &id, actor).await {
        Ok(i) => i,
        Err(e) => return import_error(e),
    };
    state
        .audit(
            crate::event_log::Event::now(
                &format!("import:{id}"),
                "pending",
                "discarded",
                Some(&format!("pack import from {} discarded", import.repo)),
                None,
            )
            .by(actor),
            "discard_pack_import",
        )
        .await;
    let catalog = match crate::playbooks::api::registry::catalog(&state).await {
        Ok(c) => c,
        Err(refusal) => return refusal,
    };
    Json(PackImportDto::from_row(
        import,
        &state.dispatch,
        &state.profile_secret_env,
        &catalog,
    ))
    .into_response()
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct DraftFromGitBody {
    /// The principal to own it: `user:<login>` or `team:<slug>` the caller acts as; absent means
    /// the caller.
    owner: Option<String>,
    /// The draft id to open under.
    id: String,
    description: String,
    /// `owner/repo` or a clone URL.
    repo: String,
    #[serde(default)]
    git_ref: Option<String>,
    /// The pack directory inside the repo; omitted ⇒ the repo root.
    #[serde(default)]
    path: Option<String>,
}

/// A draft opened straight from git, with the import row that carries where its bytes came from.
#[derive(Debug, Serialize, ToSchema)]
pub struct DraftFromGitDto {
    pub import_id: String,
    pub repo: String,
    pub git_ref: Option<String>,
    pub path: String,
    /// The commit the fetch resolved to; the draft's origin rev.
    pub rev: String,
    pub draft: DraftCompileDto,
}

/// `POST /api/playbook-drafts/from-git` — fetch a pack at a ref and open it as a draft in one
/// motion. It is the propose-then-open pair, in order: the pending import is what says where the
/// draft's bytes came from, and it stays pending because a draft is a fork of them, not a
/// resolution.
#[utoipa::path(
    post,
    path = "/api/playbook-drafts/from-git",
    request_body = DraftFromGitBody,
    responses(
        (status = 201, description = "The draft's first version, and the import behind it", body = DraftFromGitDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 409, description = "That draft id is already a draft or a registered playbook", body = ErrorBody),
        (status = 422, description = "Bad draft id, repo/ref, or a path that is not a playbook pack", body = ErrorBody),
        (status = 502, description = "Cloning the repo failed", body = ErrorBody)
    )
)]
pub(crate) async fn create_draft_from_git(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Json(body): Json<DraftFromGitBody>,
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
    let repo = normalize_repo(body.repo.trim()).to_string();
    let path = body.path.unwrap_or_default().trim().to_string();
    let draft_id = body.id.trim().to_string();
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

    let git = match crate::playbooks::registry::PackGit::resolve(state.pack_pr_app.as_ref()).await {
        Ok(g) => g,
        Err(e) => return AppError::from(e).into_response(),
    };
    let (import, saved) = match crate::playbooks::imports::propose_as_draft(
        state.db.pool(),
        &git,
        crate::playbooks::registry::PackSource {
            repo: &repo,
            git_ref: git_ref.as_deref(),
            path: &path,
            expected_rev: None,
        },
        &draft_id,
        body.description.trim(),
        actor,
        &owner,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return import_error(e),
    };

    state
        .audit(
            crate::event_log::Event::now(
                &format!("draft:{draft_id}"),
                "created",
                "created",
                Some(&format!(
                    "draft {draft_id} opened from {repo} at {} (import {})",
                    import.rev, import.id
                )),
                None,
            )
            .by(actor),
            "create_draft_from_git",
        )
        .await;
    let catalog = match crate::playbooks::api::registry::catalog(&state).await {
        Ok(c) => c,
        Err(refusal) => return refusal,
    };
    (
        StatusCode::CREATED,
        Json(DraftFromGitDto {
            import_id: import.id,
            repo: import.repo,
            git_ref: import.git_ref,
            path: import.path,
            rev: import.rev,
            draft: DraftCompileDto::from_saved(saved, &state.dispatch, &catalog),
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ImportDraftBody {
    /// The draft id to open under.
    id: String,
    description: String,
}

/// `POST /api/playbooks/imports/{id}/draft` — seed a draft from the import's frozen tarball, so a
/// proposal an agent made becomes something a human edits on the authoring surface. The import
/// stays pending: the draft is a fork of its bytes, not a resolution.
#[utoipa::path(
    post,
    path = "/api/playbooks/imports/{id}/draft",
    params(("id" = String, Path, description = "Import id")),
    request_body = ImportDraftBody,
    responses(
        (status = 201, description = "The draft's first version", body = DraftCompileDto),
        (status = 403, description = "The active policy denies the caller this action", body = ErrorBody),
        (status = 404, description = "No import with that id", body = ErrorBody),
        (status = 409, description = "The import is resolved, or that draft id is taken", body = ErrorBody),
        (status = 422, description = "Bad draft id or description", body = ErrorBody)
    )
)]
pub(crate) async fn draft_from_pack_import(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    caller: crate::authz::Caller,
    Path(id): Path<String>,
    Json(body): Json<ImportDraftBody>,
) -> Response {
    if let Err(refused) =
        readable_import(&state, &caller, &id, crate::authz::action::Verb::Update).await
    {
        return refused;
    }
    if let Some(msg) = require_non_empty(&[("id", &body.id), ("description", &body.description)]) {
        return unprocessable(msg);
    }
    let actor = identity.as_deref();
    let draft_id = body.id.trim().to_string();
    let saved = match crate::playbooks::imports::open_as_draft(
        state.db.pool(),
        &id,
        &draft_id,
        body.description.trim(),
        actor,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return import_error(e),
    };

    state
        .audit(
            crate::event_log::Event::now(
                &format!("draft:{draft_id}"),
                "created",
                "created",
                Some(&format!("draft {draft_id} seeded from pack import {id}")),
                None,
            )
            .by(actor),
            "draft_from_pack_import",
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
