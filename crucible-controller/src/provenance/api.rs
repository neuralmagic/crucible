//! The provenance surface: the public key set (unauthenticated), stored envelopes by digest,
//! statements by commit and by run, and a server-side verification.

use crate::api::dto::{bad_request, not_found, unavailable};
use crate::api::state::{ApiState, AppError, ErrorBody, Json};
use crate::provenance::StoredStatement;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use crucible_contract::provenance::{
    CommitSha, EnvelopeDigest, Expect, KeySet, VerifyStep, verify,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub(crate) const KEYS_PATH: &str = "/api/provenance/keys";

const NO_KEY: &str = "this controller signs no commit provenance: no key is configured";

/// One stored statement.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StatementDto {
    /// `sha256:<hex>` over the envelope's stored bytes.
    pub digest: String,
    pub commit: String,
    pub repository: String,
    pub run_id: String,
    pub iter: i64,
    pub task: String,
    pub keyid: String,
    pub created_at: String,
}

impl From<StoredStatement> for StatementDto {
    fn from(s: StoredStatement) -> Self {
        StatementDto {
            digest: s.digest.to_string(),
            commit: s.commit,
            repository: s.repository,
            run_id: s.run_id,
            iter: s.iter,
            task: s.task,
            keyid: s.keyid,
            created_at: s.created_at,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct VerifyBody {
    /// An envelope digest (`sha256:<hex>`) or a full commit sha.
    pub target: String,
    /// The repository the subject must name, when given.
    pub repository: Option<String>,
}

/// One envelope's verification.
#[derive(Debug, Serialize, ToSchema)]
pub struct VerificationDto {
    pub digest: String,
    pub ok: bool,
    /// The first step that failed: `digest`, `envelope`, `payload_type`, `key_id`, `signature`,
    /// `statement`, or `subject`.
    #[schema(value_type = Option<String>)]
    pub step: Option<VerifyStep>,
    pub detail: Option<String>,
    pub keyid: Option<String>,
    pub run_id: Option<String>,
    pub task: Option<String>,
}

/// What a verification target names.
pub(crate) enum Target {
    Envelope(EnvelopeDigest),
    Commit(CommitSha),
}

impl Target {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.starts_with("sha256:") {
            true => EnvelopeDigest::parse(raw).ok().map(Target::Envelope),
            false => CommitSha::parse(raw).ok().map(Target::Commit),
        }
    }
}

/// The unauthenticated key-set router, merged outside the bearer guard like `/metrics`.
pub(crate) fn keys_router(state: ApiState) -> axum::Router {
    axum::Router::new()
        .route(KEYS_PATH, axum::routing::get(get_keys))
        .with_state(state)
}

async fn get_keys(State(state): State<ApiState>) -> Response {
    match state.provenance.as_ref() {
        Some(signer) => axum::Json(signer.key_set()).into_response(),
        None => unavailable(NO_KEY),
    }
}

/// `GET /api/provenance/envelopes/{digest}` — an envelope's stored bytes, exactly as signed.
#[utoipa::path(
    get,
    path = "/api/provenance/envelopes/{digest}",
    params(("digest" = String, Path, description = "`sha256:<hex>` envelope digest")),
    responses(
        (status = 200, description = "The DSSE envelope, byte for byte as stored", content_type = "application/json", body = Object),
        (status = 400, description = "Not an envelope digest", body = ErrorBody),
        (status = 404, description = "No envelope has that digest", body = ErrorBody)
    )
)]
pub(crate) async fn get_envelope(
    State(state): State<ApiState>,
    Path(digest): Path<String>,
) -> Result<Response, AppError> {
    let Ok(digest) = EnvelopeDigest::parse(&digest) else {
        return Ok(bad_request(format!("{digest:?} is not an envelope digest")));
    };
    Ok(
        match crate::provenance::envelope(state.db.pool(), &digest).await? {
            Some(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
            None => not_found(format!("no envelope {digest}")),
        },
    )
}

/// `GET /api/provenance/commits/{sha}` — every statement whose subject is the commit, newest
/// first. One commit has one statement per run that pushed it.
#[utoipa::path(
    get,
    path = "/api/provenance/commits/{sha}",
    params(("sha" = String, Path, description = "Full commit sha")),
    responses(
        (status = 200, description = "The commit's statements, newest first", body = Vec<StatementDto>),
        (status = 400, description = "Not a full commit sha", body = ErrorBody)
    )
)]
pub(crate) async fn list_commit_statements(
    State(state): State<ApiState>,
    Path(sha): Path<String>,
) -> Result<Response, AppError> {
    let Ok(sha) = CommitSha::parse(&sha) else {
        return Ok(bad_request(format!("{sha:?} is not a full commit sha")));
    };
    let statements = crate::provenance::statements_for_commit(state.db.pool(), sha.as_str())
        .await?
        .into_iter()
        .map(StatementDto::from)
        .collect::<Vec<_>>();
    Ok(Json(statements).into_response())
}

/// `GET /api/runs/{run_id}/provenance` — every statement the run produced.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/provenance",
    params(("run_id" = String, Path, description = "Run identifier")),
    responses(
        (status = 200, description = "The run's statements, oldest first", body = Vec<StatementDto>)
    )
)]
pub(crate) async fn list_run_statements(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Result<Response, AppError> {
    let statements = crate::provenance::statements_for_run(state.db.pool(), &run_id)
        .await?
        .into_iter()
        .map(StatementDto::from)
        .collect::<Vec<_>>();
    Ok(Json(statements).into_response())
}

/// `POST /api/provenance/verify` — check every envelope a digest or commit names against this
/// controller's key set.
#[utoipa::path(
    post,
    path = "/api/provenance/verify",
    request_body = VerifyBody,
    responses(
        (status = 200, description = "One result per envelope", body = Vec<VerificationDto>),
        (status = 400, description = "Not an envelope digest or a full commit sha", body = ErrorBody),
        (status = 404, description = "Nothing is stored for the target", body = ErrorBody),
        (status = 503, description = "No signing key is configured", body = ErrorBody)
    )
)]
pub(crate) async fn verify_target(
    State(state): State<ApiState>,
    Json(body): Json<VerifyBody>,
) -> Result<Response, AppError> {
    let Some(signer) = state.provenance.as_ref() else {
        return Ok(unavailable(NO_KEY));
    };
    let Some(target) = Target::parse(&body.target) else {
        return Ok(bad_request(format!(
            "{:?} is neither an envelope digest nor a full commit sha",
            body.target
        )));
    };
    let pool = state.db.pool();
    let (digests, commit) = match &target {
        Target::Envelope(digest) => (vec![digest.clone()], None),
        Target::Commit(sha) => (
            crate::provenance::statements_for_commit(pool, sha.as_str())
                .await?
                .into_iter()
                .map(|s| s.digest)
                .collect(),
            Some(sha),
        ),
    };
    let keys: KeySet = signer.key_set();
    let mut results = Vec::with_capacity(digests.len());
    for digest in digests {
        let Some(bytes) = crate::provenance::envelope(pool, &digest).await? else {
            continue;
        };
        let checked = verify(
            &bytes,
            &keys,
            Expect {
                digest: Some(&digest),
                commit,
                repository: body.repository.as_deref(),
            },
        );
        results.push(match checked {
            Ok(v) => VerificationDto {
                digest: digest.to_string(),
                ok: true,
                step: None,
                detail: None,
                keyid: Some(v.keyid.to_string()),
                run_id: Some(v.statement.predicate.run),
                task: Some(v.statement.predicate.task),
            },
            Err(f) => VerificationDto {
                digest: digest.to_string(),
                ok: false,
                step: Some(f.step),
                detail: Some(f.detail),
                keyid: None,
                run_id: None,
                task: None,
            },
        });
    }
    if results.is_empty() {
        return Ok(not_found(format!("no statement for {}", body.target)));
    }
    Ok(Json(results).into_response())
}
