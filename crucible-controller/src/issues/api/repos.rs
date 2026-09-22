use crate::api::dto::*;

use crate::api::state::*;

use crate::issues::repo_ref::{RepoRef, RepoWhitelist};

use axum::extract::{Path, State};

use axum::http::StatusCode;

use axum::response::{IntoResponse, Response};

use serde::{Deserialize, Serialize};

use utoipa::ToSchema;

// --- repo watch-set (Lane O3) --------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct AddRepoBody {
    /// `org/name`, e.g. `neuralmagic/llm-d`.
    repo: String,
    justification: String,
}

/// The ack body for a pause/resume/unwatch action.
#[derive(Debug, Serialize, ToSchema)]
pub struct RepoActionAck {
    pub repo: String,
    pub action: &'static str,
}

/// Validate `raw` against the format check, then the org whitelist (Lane O3's first two steps —
/// see [`crate::issues::repo_ref`]'s module doc). Returns the parsed [`RepoRef`] or the specific rejection
/// message the `POST /api/repos` 422 body should carry.
fn validate_repo_ref(raw: &str, whitelist: &RepoWhitelist) -> Result<RepoRef, String> {
    let repo_ref: RepoRef = raw
        .parse()
        .map_err(|e: crate::issues::repo_ref::RepoRefParseError| format!("invalid repo: {e}"))?;
    whitelist
        .check(&repo_ref)
        .map_err(|e| format!("{e}: {}", repo_ref.org))?;
    Ok(repo_ref)
}

#[utoipa::path(
    post,
    path = "/api/repos",
    request_body = AddRepoBody,
    responses(
        (status = 201, description = "Repo added and watched", body = RepoHealthDto),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 409, description = "Repo is already watched", body = ErrorBody),
        (status = 422, description = "Format, whitelist, or GitHub-existence check failed", body = ErrorBody)
    )
)]
pub(crate) async fn add_repo(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    Json(body): Json<AddRepoBody>,
) -> Response {
    let justification = body.justification.trim().to_string();
    if justification.is_empty() {
        return unprocessable("justification is required and must be non-empty");
    }

    // Step 1 + 2: format, then the org whitelist (sync, no I/O).
    let repo_ref = match validate_repo_ref(body.repo.trim(), &state.repo_whitelist) {
        Ok(r) => r,
        Err(msg) => {
            return unprocessable(msg);
        }
    };
    let repo = repo_ref.as_repo_string();

    // Step 3: the repo must actually exist on GitHub — the same client/metric path every other
    // GitHub fetch in the controller uses.
    let exists = crate::issues::triage::repo_exists(&repo).await;
    if let Some(m) = state.db.metrics() {
        m.record_github(exists.is_ok());
    }
    match exists {
        Ok(true) => {}
        Ok(false) => {
            return unprocessable(format!("{repo} does not exist on GitHub"));
        }
        Err(e) => {
            return unprocessable(format!("could not verify {repo} exists on GitHub: {e:#}"));
        }
    }

    let actor = identity.as_deref();
    match crate::issues::repo_watch::insert_watched_repo(state.db.pool(), &repo, actor).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                Json(ErrorBody::new(format!("{repo} is already watched"))),
            )
                .into_response();
        }
        Err(e) => return AppError::from(e).into_response(),
    }
    state
        .audit(
            crate::event_log::Event::now(&repo, "unwatched", "watched", Some(&justification), None)
                .by(actor),
            "add_repo",
        )
        .await;

    match crate::issues::store::repo_health(state.db.pool()).await {
        Ok(health) => {
            let dto = health
                .into_iter()
                .find(|r| r.repo == repo)
                .map(RepoHealthDto::from);
            (StatusCode::CREATED, Json(dto)).into_response()
        }
        Err(e) => AppError::from(e).into_response(),
    }
}

/// Shared body for pause/resume/unwatch: look the repo up (404 if unknown), apply `f`, audit-log
/// the transition, and ack.
async fn repo_transition(
    state: &ApiState,
    repo: &str,
    action: &'static str,
    from: &'static str,
    to: &'static str,
    identity: &crate::identity::session::Identity,
    apply: impl std::future::Future<Output = Result<bool, anyhow::Error>>,
) -> Response {
    match apply.await {
        Ok(true) => {
            state
                .audit(
                    crate::event_log::Event::now(repo, from, to, None, None)
                        .by(identity.as_deref()),
                    action,
                )
                .await;
            (
                StatusCode::OK,
                Json(RepoActionAck {
                    repo: repo.to_string(),
                    action,
                }),
            )
                .into_response()
        }
        Ok(false) => not_found(format!("{repo} is not a known repo")),
        Err(e) => AppError::from(e).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/api/repos/{repo}/pause",
    params(("repo" = String, Path, description = "org/name repo (percent-encoded)")),
    responses(
        (status = 200, description = "Repo paused (skipped by discovery, still watched)", body = RepoActionAck),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "Repo is not known to the controller", body = ErrorBody)
    )
)]
pub(crate) async fn pause_repo(
    State(state): State<ApiState>,
    Path(repo): Path<String>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
) -> Response {
    repo_transition(
        &state,
        &repo,
        "pause",
        "watching",
        "paused",
        &identity,
        crate::issues::repo_watch::set_repo_paused(state.db.pool(), &repo, true),
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/repos/{repo}/resume",
    params(("repo" = String, Path, description = "org/name repo (percent-encoded)")),
    responses(
        (status = 200, description = "Repo resumed (discovery picks it up again)", body = RepoActionAck),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "Repo is not known to the controller", body = ErrorBody)
    )
)]
pub(crate) async fn resume_repo(
    State(state): State<ApiState>,
    Path(repo): Path<String>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
) -> Response {
    repo_transition(
        &state,
        &repo,
        "resume",
        "paused",
        "watching",
        &identity,
        crate::issues::repo_watch::set_repo_paused(state.db.pool(), &repo, false),
    )
    .await
}

#[utoipa::path(
    delete,
    path = "/api/repos/{repo}",
    params(("repo" = String, Path, description = "org/name repo (percent-encoded)")),
    responses(
        (status = 200, description = "Repo unwatched (row + its issues kept, discovery stops)", body = RepoActionAck),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "Repo is not known to the controller", body = ErrorBody)
    )
)]
pub(crate) async fn unwatch_repo(
    State(state): State<ApiState>,
    Path(repo): Path<String>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
) -> Response {
    repo_transition(
        &state,
        &repo,
        "unwatch",
        "watched",
        "unwatched",
        &identity,
        crate::issues::repo_watch::set_repo_watched(state.db.pool(), &repo, false),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/repos",
    responses(
        (status = 200, description = "Per-repository triage health: issue counts by status and upstream poll watermarks", body = Vec<RepoHealthDto>)
    )
)]
pub(crate) async fn get_repos(
    State(state): State<ApiState>,
) -> Result<Json<Vec<RepoHealthDto>>, AppError> {
    let health = crate::issues::store::repo_health(state.db.pool()).await?;
    Ok(Json(health.into_iter().map(RepoHealthDto::from).collect()))
}
