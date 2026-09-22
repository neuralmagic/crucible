//! Approval-queue and scope-evidence reads: what is waiting on a human and the trail behind it.

use crate::api::dto::*;
use crate::api::state::*;
#[cfg(feature = "autoresearch")]
use axum::extract::Path;
use axum::extract::State;
#[cfg(feature = "autoresearch")]
use axum::response::{IntoResponse, Response};

#[utoipa::path(
    get,
    path = "/api/approvals",
    responses(
        (status = 200, description = "Approval queue: scope packs awaiting approval, pending pack imports, and kept-candidate PRs awaiting review", body = ApprovalsDto)
    )
)]
pub(crate) async fn get_approvals(
    State(state): State<ApiState>,
) -> Result<Json<ApprovalsDto>, AppError> {
    let imports = crate::playbooks::imports::pending(state.db.pool()).await?;
    #[cfg(feature = "autoresearch")]
    let (awaiting, prs) = if state.autoresearch {
        (
            crate::issues::store::awaiting_approval_scopes(state.db.pool()).await?,
            crate::runs::store::kept_candidate_prs(state.db.pool()).await?,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    #[cfg(not(feature = "autoresearch"))]
    let (awaiting, prs): (
        Vec<crate::issues::model::AwaitingApproval>,
        Vec<crate::issues::model::KeptPr>,
    ) = (Vec::new(), Vec::new());
    Ok(Json(ApprovalsDto {
        awaiting_approval: awaiting
            .into_iter()
            .map(AwaitingApprovalDto::from)
            .collect(),
        pending_imports: imports.into_iter().map(PendingImportDto::from).collect(),
        kept_prs: prs.into_iter().map(KeptPrDto::from).collect(),
    }))
}

/// `GET /api/approvals/{scope_id}/evidence`: the measurement provenance for one scope pack, so the human
/// at the approval can see the full refine trail before approving. Reads `SCOPE.md` out of the scope
/// issue's stored pack tarball ([`crate::playbooks::packs::read_pack_file`]) and parses the fenced round trail
/// out of it via [`crate::issues::refine_trail::extract_trail`]. Missing or trail-less packs answer with an
/// empty `rounds` list, never an error — only an unknown scope id 404s.
#[cfg(feature = "autoresearch")]
#[utoipa::path(
    get,
    path = "/api/approvals/{scope_id}/evidence",
    params(
        ("scope_id" = i64, Path, description = "Scope id (the `scope_id` field from GET /api/approvals)")
    ),
    responses(
        (status = 200, description = "The scope's check outcome and refine round trail", body = crate::issues::refine_trail::ScopeEvidenceDto),
        (status = 404, description = "Scope not found", body = ErrorBody)
    )
)]
pub(crate) async fn get_scope_evidence(
    State(state): State<ApiState>,
    Path(scope_id): Path<i64>,
) -> Result<Response, AppError> {
    let Some(scope) = crate::issues::store::get_scope_by_id(state.db.pool(), scope_id).await?
    else {
        return Ok(not_found(format!("scope not found: {scope_id}")));
    };
    let rounds =
        match crate::playbooks::packs::read_pack_file(state.db.pool(), &scope.issue, "SCOPE.md")
            .await?
        {
            Some(scope_md) => crate::issues::refine_trail::extract_trail(&scope_md),
            None => Vec::new(),
        };
    Ok(Json(crate::issues::refine_trail::ScopeEvidenceDto {
        scope_id,
        check_outcome: scope.check_outcome,
        rounds,
    })
    .into_response())
}

/// `GET /api/issues/{key}/scope-report`: the latest scope turn's structured report — per-stage
/// pass/fail, the refine round trail, cost, and the work pod that ran it. Stored verbatim by the
/// reconcile on every turn that produced a report (success or failure), so a parked issue keeps
/// its evidence where the ScopeProgress page can render it instead of the flattened park reason.
#[cfg(feature = "autoresearch")]
#[utoipa::path(
    get,
    path = "/api/issues/{key}/scope-report",
    params(
        ("key" = String, Path, description = "Issue key (percent-encoded)")
    ),
    responses(
        (status = 200, description = "The latest structured scope report for the issue", body = crate::issues::refine_trail::ScopeReportDto),
        (status = 404, description = "No scope report recorded for the issue", body = ErrorBody)
    )
)]
pub(crate) async fn get_scope_report(
    State(state): State<ApiState>,
    Path(key): Path<String>,
) -> Result<Response, AppError> {
    let Some(row) = crate::issues::store::latest_scope_report(state.db.pool(), &key).await? else {
        return Ok(not_found(format!("no scope report for: {key}")));
    };
    Ok(Json(crate::issues::refine_trail::ScopeReportDto::from_row(row)).into_response())
}

/// `GET /api/issues/{key}/scope-transcript`: the latest scope turn's preserved agent transcript —
/// the session NDJSON the propose/refine/adversary turns streamed (round-delimited `note` lines,
/// nested agent events), stored gzipped by the reconcile and decompressed here for the SPA's
/// session renderer. Absent for pre-feature turns and turns that delivered no transcript.
#[cfg(feature = "autoresearch")]
#[utoipa::path(
    get,
    path = "/api/issues/{key}/scope-transcript",
    params(
        ("key" = String, Path, description = "Issue key (percent-encoded)")
    ),
    responses(
        (status = 200, description = "The latest scope turn's session transcript, as NDJSON (one SessionEvent per line)", body = String, content_type = "application/x-ndjson"),
        (status = 404, description = "No transcript recorded for the issue", body = ErrorBody)
    )
)]
pub(crate) async fn get_scope_transcript(
    State(state): State<ApiState>,
    Path(key): Path<String>,
) -> Result<Response, AppError> {
    use anyhow::Context as _;
    use std::io::Read as _;
    let Some(row) = crate::issues::store::latest_scope_transcript(state.db.pool(), &key).await?
    else {
        return Ok(not_found(format!("no scope transcript for: {key}")));
    };
    let mut ndjson = String::new();
    flate2::read::GzDecoder::new(row.transcript_gz.as_slice())
        .read_to_string(&mut ndjson)
        .context("decompressing the stored scope transcript")?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
        ndjson,
    )
        .into_response())
}
