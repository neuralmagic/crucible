use crate::api::dto::*;

use crate::api::state::*;

use crate::builds::model::{BuildBackendKind, BuildListRow, BuildQuery, BuildRow, BuildState};
use crate::issues::model::Issue;
use crate::model::Status;

use crate::wire_enum::parse_opt;

use axum::extract::{Path, Query, State};

use axum::http::StatusCode;

use axum::response::{IntoResponse, Response};

use serde::{Deserialize, Serialize};

use utoipa::ToSchema;

// --- builds (declarative image builds) ---------------------------------------

/// One declared image build (`builds` row) joined to the issue it blocks. The `building` reconcile
/// state's ledger, surfaced read-only: what it builds (`image`/`tag`), which backend runs it, the
/// dispatch identity being polled, and the terminal outcome (a pinned `digest_ref` on success, an
/// `evidence_url` build-log pointer on failure/timeout). `state`/`backend` are the canonical strings.
#[derive(Debug, Serialize, ToSchema)]
pub struct BuildDto {
    pub id: i64,
    /// The scope (pack) this build blocks; null for a build the controller recorded without a scope.
    pub scope: Option<i64>,
    /// The issue this build blocks (via `builds.scope → scopes.issue`), or null when it has no scope.
    pub issue_key: Option<String>,
    /// The issue's repo (`owner/repo`), or null when the build has no scope/issue.
    pub repo: Option<String>,
    /// The `[build.<name>]` key.
    pub name: String,
    pub image: String,
    pub tag: String,
    pub context_digest: String,
    /// `cluster` | `github-actions`.
    pub backend: String,
    /// `pending` | `dispatched` | `succeeded` | `failed` | `timed-out`.
    pub state: String,
    /// The cluster Job name / GitHub Actions run id reconcile polls; null until dispatched.
    pub dispatch_id: Option<String>,
    /// The pinned `image@sha256:…` on success; null until succeeded.
    pub digest_ref: Option<String>,
    /// The build-log pointer parked as evidence on failure/timeout (a GH Actions run URL or a pod-log
    /// reference); null on the happy path. The failure-evidence link renders this.
    pub evidence_url: Option<String>,
    pub dispatch_attempts: i64,
    pub timeout_secs: i64,
    pub created_at: String,
    pub dispatched_at: Option<String>,
    pub finished_at: Option<String>,
}

impl BuildDto {
    fn from_row(row: BuildRow, issue_key: Option<String>, repo: Option<String>) -> Self {
        BuildDto {
            id: row.id,
            scope: row.scope,
            issue_key,
            repo,
            name: row.name,
            image: row.image,
            tag: row.tag,
            context_digest: row.context_digest,
            backend: row.backend.as_str().to_string(),
            state: row.state.as_str().to_string(),
            dispatch_id: row.dispatch_id,
            digest_ref: row.digest_ref,
            evidence_url: row.evidence_url,
            dispatch_attempts: row.dispatch_attempts,
            timeout_secs: row.timeout_secs,
            created_at: row.created_at,
            dispatched_at: row.dispatched_at,
            finished_at: row.finished_at,
        }
    }
}

impl From<BuildListRow> for BuildDto {
    fn from(r: BuildListRow) -> Self {
        BuildDto::from_row(r.row, r.issue_key, r.repo)
    }
}

const BUILDS_LIST_DEFAULT: i64 = 50;

const BUILDS_LIST_MAX: i64 = 500;

/// The raw `GET /api/builds` query string: `state=`/`backend=`/`issue=` filters, `limit`/`offset`.
#[derive(Debug, Deserialize, Default)]
pub(crate) struct BuildsQuery {
    state: Option<String>,
    backend: Option<String>,
    issue: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

impl BuildsQuery {
    /// Parse into the strong [`BuildQuery`], or a bad filter value's message (a 400, not a 500).
    /// `state`/`backend` go through the closed-enum parsers so an unknown value is rejected up front
    /// rather than silently matching nothing.
    fn into_model(self) -> Result<BuildQuery, String> {
        let state = parse_opt::<BuildState>(self.state.as_deref())?;
        let backend = parse_opt::<BuildBackendKind>(self.backend.as_deref())?;
        let limit = self
            .limit
            .unwrap_or(BUILDS_LIST_DEFAULT)
            .clamp(0, BUILDS_LIST_MAX);
        let offset = self.offset.unwrap_or(0).max(0);
        Ok(BuildQuery {
            state,
            backend,
            issue: self.issue.filter(|s| !s.is_empty()),
            limit,
            offset,
        })
    }
}

/// `GET /api/builds` — the declarative-builds ledger: filter by `state`/`backend`/`issue`, page with
/// `limit`/`offset`. Newest first (build id descending).
#[utoipa::path(
    get,
    path = "/api/builds",
    params(
        ("state" = Option<String>, Query, description = "Filter by build state (pending|dispatched|succeeded|failed|timed-out)"),
        ("backend" = Option<String>, Query, description = "Filter by backend (cluster|github-actions)"),
        ("issue" = Option<String>, Query, description = "Filter to one issue key (owner/repo#N)"),
        ("limit" = Option<i64>, Query, description = "Max builds to return (default 50, max 500)"),
        ("offset" = Option<i64>, Query, description = "Rows to skip for paging (default 0)"),
    ),
    responses(
        (status = 200, description = "Builds ledger rows, newest first", body = Vec<BuildDto>),
        (status = 400, description = "Invalid query parameters", body = ErrorBody)
    )
)]
pub(crate) async fn list_builds(
    State(state): State<ApiState>,
    Query(q): Query<BuildsQuery>,
) -> Result<Response, AppError> {
    let query = match q.into_model() {
        Ok(q) => q,
        Err(msg) => {
            return Ok(bad_request(msg));
        }
    };
    let rows = crate::builds::store::list_builds_page(state.db.pool(), &query).await?;
    let dtos: Vec<BuildDto> = rows.into_iter().map(BuildDto::from).collect();
    Ok(Json(dtos).into_response())
}

/// `GET /api/issues/{key}/builds` — every build declared for one issue (via its scopes), oldest
/// first. A 404 for an unknown issue so the client can distinguish "no builds" from "no issue".
#[utoipa::path(
    get,
    path = "/api/issues/{key}/builds",
    params(
        ("key" = String, Path, description = "Issue key (owner/repo#N)")
    ),
    responses(
        (status = 200, description = "The issue's builds, oldest first", body = Vec<BuildDto>),
        (status = 404, description = "Issue not found", body = ErrorBody)
    )
)]
pub(crate) async fn list_issue_builds(
    State(state): State<ApiState>,
    Path(key): Path<String>,
) -> Result<Response, AppError> {
    let Some(issue) = crate::issues::store::get_issue(state.db.pool(), &key).await? else {
        return Ok(not_found(format!("issue not found: {key}")));
    };
    let rows = crate::builds::store::builds_for_issue(state.db.pool(), &key).await?;
    let dtos: Vec<BuildDto> = rows
        .into_iter()
        .map(|row| BuildDto::from_row(row, Some(issue.key.clone()), Some(issue.repo.clone())))
        .collect();
    Ok(Json(dtos).into_response())
}

/// The ack `POST /api/builds/{id}/rebuild` returns: the reset build plus the issue it narrated onto
/// (`None` for a build with no scope).
#[derive(Debug, Serialize, ToSchema)]
pub struct RebuildAck {
    pub id: i64,
    pub name: String,
    /// The issue this build blocks (via `builds.scope -> scopes.issue`), or null when it has none.
    pub issue_key: Option<String>,
    pub actor: Option<String>,
}

/// `POST /api/builds/{id}/rebuild` — admin force-rebuild: reset a terminal build row (`succeeded` /
/// `failed` / `timed-out`) back to `pending`, clearing its dispatch identity, digest, and evidence.
/// The reset row is picked up by the EXISTING `building` reconcile — no parallel dispatch path: if
/// the build's failure is what parked its issue, this unparks it back to `building` so
/// [`crate::builds::lifecycle::drive_scope_builds`] re-drives the dispatch on the next pass; if the issue is
/// still `building` (a sibling build still in flight) the reset row alone is enough, since
/// `drive_scope_builds` treats a `pending` row with no `dispatch_id` as needing dispatch.
/// `reconcile_building` only ever runs for `Status::Building`, so a reset row is a 409 refusal
/// (not a reset) whenever nothing will re-drive it: the build has no scope, its issue has already
/// moved past `building`, or the issue is `parked` for a reason other than this build's own
/// failure. A stranded `pending` row would otherwise never get GC'd (`adopt_builds` only reaps
/// `dispatched`) and would permanently consume a `build_pod_cap` slot. Rebuilding an in-flight
/// build (`pending`/`dispatched`) is also a 409 — force-rebuild only ever replaces a terminal
/// outcome, never races a live dispatch.
#[utoipa::path(
    post,
    path = "/api/builds/{id}/rebuild",
    params(
        ("id" = i64, Path, description = "Build row id")
    ),
    responses(
        (status = 202, description = "Build reset to pending; the existing `building` reconcile re-drives it", body = RebuildAck),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "Build not found", body = ErrorBody),
        (status = 409, description = "Build is in flight, or its issue would never re-drive the reset row", body = ErrorBody)
    )
)]
pub(crate) async fn rebuild_build(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
) -> Result<Response, AppError> {
    let Some(build) = crate::builds::store::get_build(state.db.pool(), id).await? else {
        return Ok(not_found(format!("build not found: {id}")));
    };
    if !build.state.is_terminal() {
        return Ok(rebuild_conflict(&format!(
            "build `{}` is {} (in flight) — rebuild only applies to a terminal build",
            build.name,
            build.state.as_str()
        )));
    }

    // Only reset a row the `building` reconcile will actually re-drive (see the doc comment
    // above): the issue must already be `building`, or `parked` on THIS build's own failure (which
    // `rebuild_narrate` below unparks back to `building`). Anything else — no scope, a missing
    // scope/issue row, or an issue that has moved on — would leave the reset row stuck at
    // `pending` forever, leaking a `build_pod_cap` slot with no GC to reclaim it.
    let Some(scope_id) = build.scope else {
        return Ok(rebuild_conflict(&format!(
            "build `{}` has no scope — nothing would re-drive a reset",
            build.name
        )));
    };
    let Some(scope) = crate::issues::store::get_scope_by_id(state.db.pool(), scope_id).await?
    else {
        return Ok(rebuild_conflict(&format!(
            "build `{}`'s scope no longer exists — nothing would re-drive a reset",
            build.name
        )));
    };
    let Some(issue) = crate::issues::store::get_issue(state.db.pool(), &scope.issue).await? else {
        return Ok(rebuild_conflict(&format!(
            "build `{}`'s issue `{}` no longer exists — nothing would re-drive a reset",
            build.name, scope.issue
        )));
    };
    let parked_on_this_build = issue.status == Status::Parked
        && issue
            .park_reason()
            .is_some_and(|r| r.is_image_build_failure());
    if issue.status != Status::Building && !parked_on_this_build {
        return Ok(rebuild_conflict(&format!(
            "build `{}`'s issue `{}` is `{}`, past `building` — nothing would re-drive a reset",
            build.name,
            issue.key,
            issue.status.as_str()
        )));
    }

    if !crate::builds::store::reset_build_for_rebuild(state.db.pool(), id).await? {
        // Raced into pending/dispatched between the read above and this CAS.
        return Ok(rebuild_conflict(&format!(
            "build `{}` started dispatching before the rebuild landed",
            build.name
        )));
    }

    let actor = identity.as_deref().map(str::to_string);
    rebuild_narrate(
        &state,
        &issue,
        &build,
        parked_on_this_build,
        actor.as_deref(),
    )
    .await?;

    // Kick the daemon's reconcile pass so the reset row — and, if this build's failure had parked
    // the issue, the just-unparked issue — re-drives now instead of waiting for the next
    // discovery-triggered pass.
    state.reconcile_now.notify_one();

    Ok((
        StatusCode::ACCEPTED,
        Json(RebuildAck {
            id: build.id,
            name: build.name,
            issue_key: Some(issue.key),
            actor,
        }),
    )
        .into_response())
}

fn rebuild_conflict(message: &str) -> Response {
    conflict(message)
}

/// Narrate a forced rebuild onto the blocked issue's feed. When this build's own failure is what
/// parked the issue (`parked_on_this_build`, computed by the caller from `parked_reason` parsing
/// to [`crate::model::ParkReason::ImageBuildFailed`]), unpark it back to `building` so reconcile
/// re-drives; otherwise log a same-state note (the issue is already `building` with a sibling
/// build still in flight — the only other case the caller admits).
async fn rebuild_narrate(
    state: &ApiState,
    issue: &Issue,
    build: &BuildRow,
    parked_on_this_build: bool,
    actor: Option<&str>,
) -> Result<(), AppError> {
    let reason = format!("forced rebuild: build `{}` reset by admin", build.name);
    if parked_on_this_build {
        crate::issues::transitions::unpark(
            state.db.pool(),
            state.db.events(),
            &issue.key,
            Some(&reason),
            actor,
        )
        .await?;
    } else {
        state
            .db
            .events()
            .append(
                &crate::event_log::Event::now(
                    &issue.key,
                    issue.status.as_str(),
                    issue.status.as_str(),
                    Some(&reason),
                    None,
                )
                .by(actor),
            )
            .await?;
    }
    Ok(())
}
