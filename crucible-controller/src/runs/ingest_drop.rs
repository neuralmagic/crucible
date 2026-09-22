//! The Tier 2 ingest drop-box: `POST /api/pods/{pod}/artifacts/{kind}` streams the raw gzipped
//! artifact body into the artifact chunk tables ([`crate::runs::blob_store`]) and records a POINTER +
//! digest in the ledger — the ledger row stays a pointer at the evidence. It is a **data-plane
//! evidence drop**, the network equivalent of the S3 bucket: pods still never touch the DB beyond
//! this approval, and the fold that turns evidence into `scopes`/`runs` rows still runs in the
//! controller's own reconcile transaction.
//!
//! The route lives OUTSIDE the human-facing oauth2-proxy/role stack and outside the OpenAPI-typed
//! SPA surface — write-only, never in the generated client. Its auth is the pod-bound TokenReview
//! extractor ([`crate::runs::ingest_auth::IngestAuth`]).
//!
//! Three approval invariants:
//!   * **caps** — a body over the per-kind cap ([`ArtifactKind::max_bytes`]) is a 413 with the limit
//!     in the JSON body; the stream is abandoned the instant it crosses the cap (no unbounded read).
//!   * **content-addressed dedup** — the body is sha256'd as it streams; a re-POST of a digest the
//!     drop-box already holds returns 200 with `stored:false` and leaves the pointer untouched
//!     (at-least-once delivery, idempotent approval — the Temporal lesson).
//!   * **atomic publish** — the chunks land in one transaction, so a torn upload never leaves a
//!     half-written artifact the fold trusts.

use crate::client::Db;
use crate::runs::blob_store::{ArtifactOwner, PutArtifactError};
use crate::runs::ingest_auth::{IngestAuth, IngestValidator};
use crate::runs::model::NewPodArtifact;
use axum::body::Body;
use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use crucible_contract::{ArtifactKind, IngestError, IngestResponse};
use std::sync::Arc;

/// The state the ingest routes carry: the ledger (pointer rows + the chunk store) and the
/// TokenReview validator (the extractor pulls it via [`FromRef`]).
#[derive(Clone)]
pub struct IngestState {
    pub(crate) db: Db,
    pub(crate) validator: Arc<IngestValidator>,
}

impl FromRef<IngestState> for Arc<IngestValidator> {
    fn from_ref(state: &IngestState) -> Self {
        state.validator.clone()
    }
}

/// The ingest router. Mounted by [`crate::serve`] *after* the bearer-guard layer (like `/metrics`),
/// so it is exempt from the human oauth2-proxy stack and governed only by its own TokenReview auth.
pub(crate) fn router(state: IngestState) -> Router {
    Router::new()
        .route("/api/pods/{pod}/artifacts/{kind}", post(ingest_artifact))
        .with_state(state)
}

/// Stream one artifact into the drop-box. The pod is already TokenReview-verified by [`IngestAuth`];
/// the kind is parsed here so an unknown `{kind}` is an honest 404 rather than a router miss.
async fn ingest_artifact(
    auth: IngestAuth,
    State(state): State<IngestState>,
    body: Body,
) -> Response {
    let Ok(kind) = auth.kind.parse::<ArtifactKind>() else {
        return err(
            StatusCode::NOT_FOUND,
            format!("unknown artifact kind: {}", auth.kind),
            None,
        );
    };

    // The pod is TokenReview-verified, but a path segment still must never carry a traversal or
    // separator component before it becomes a storage key — defense in depth on top of the auth.
    if !crate::runs::task_evidence::safe_segment(&auth.pod) {
        return err(
            StatusCode::BAD_REQUEST,
            format!("unsafe pod segment: {}", auth.pod),
            None,
        );
    }

    match store(&state, &auth.pod, kind, body).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(StoreError::TooLarge { limit }) => err(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "artifact exceeds the {} cap of {limit} bytes",
                kind.as_str()
            ),
            Some(limit),
        ),
        Err(StoreError::Body(msg)) => {
            tracing::warn!(pod = %auth.pod, kind = kind.as_str(), error = %msg, "ingest body read failed");
            err(
                StatusCode::BAD_REQUEST,
                format!("reading the body: {msg}"),
                None,
            )
        }
        Err(StoreError::Io(msg)) => {
            tracing::error!(pod = %auth.pod, kind = kind.as_str(), error = %msg, "ingest storage failed");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to persist the artifact".to_string(),
                None,
            )
        }
    }
}

/// Why storing an artifact failed, mapped to a status by the handler.
enum StoreError {
    /// The body crossed the per-kind cap; `limit` rides the 413 body.
    TooLarge { limit: u64 },
    /// The client's body stream errored mid-flight.
    Body(String),
    /// A storage failure on our side.
    Io(String),
}

/// Stream `body` into the chunk store, hashing + capping as it arrives, dedup by digest against
/// the existing pointer row, and record the pointer. The heavy lifting behind [`ingest_artifact`],
/// split out so tests can exercise the storage contract directly.
async fn store(
    state: &IngestState,
    pod: &str,
    kind: ArtifactKind,
    body: Body,
) -> Result<IngestResponse, StoreError> {
    let owner = ArtifactOwner::PodEvidence {
        pod: pod.to_string(),
    };
    let stored = crate::runs::blob_store::put_artifact(
        state.db.pool(),
        &owner,
        kind.as_str(),
        kind.max_bytes(),
        body.into_data_stream(),
    )
    .await
    .map_err(|e| match e {
        PutArtifactError::TooLarge { limit } => StoreError::TooLarge { limit },
        PutArtifactError::Source(msg) => StoreError::Body(msg),
        PutArtifactError::Store(e) => StoreError::Io(format!("{e:#}")),
    })?;

    // Content-addressed dedup: a re-POST of the digest the pointer already names is a no-op approval
    // answer (the rewrite above landed identical bytes).
    match crate::runs::work_pods::pod_artifact(state.db.pool(), pod, kind.as_str()).await {
        Ok(Some(existing)) if existing.digest == stored.digest => {
            return Ok(IngestResponse {
                kind,
                digest: stored.digest,
                bytes: stored.bytes,
                stored: false,
            });
        }
        Ok(_) => {}
        Err(e) => return Err(StoreError::Io(format!("dedup lookup: {e:#}"))),
    }

    crate::runs::work_pods::record_pod_artifact(
        state.db.pool(),
        &NewPodArtifact {
            pod: pod.to_string(),
            kind: kind.as_str().to_string(),
            digest: stored.digest.clone(),
            bytes: i64::try_from(stored.bytes)
                .map_err(|_| StoreError::Io("artifact byte count exceeds i64".to_string()))?,
        },
    )
    .await
    .map_err(|e| StoreError::Io(format!("recording pointer: {e:#}")))?;

    Ok(IngestResponse {
        kind,
        digest: stored.digest,
        bytes: stored.bytes,
        stored: true,
    })
}

/// Build a typed [`IngestError`] JSON response.
fn err(status: StatusCode, message: String, limit_bytes: Option<u64>) -> Response {
    (
        status,
        Json(IngestError {
            error: message,
            limit_bytes,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::ingest_auth::ExpectedServiceAccount;
    use axum::body::to_bytes;
    use axum::http::Request;
    use crucible_contract::content_digest;
    use sqlx::PgPool;
    use tower::ServiceExt;

    /// A ready-to-serve ingest router whose validator trusts `(token, pod)` via the accept cache, so
    /// the real handler runs end-to-end with no live kube API (no mock — a genuine TokenReview lands
    /// the identical cache entry).
    fn app(pool: PgPool, token: &str, pod: &str) -> Router {
        let db = Db::new(pool);
        let validator = IngestValidator::unavailable(
            "crucible-ingest",
            ExpectedServiceAccount {
                namespace: "autoresearch".to_string(),
                name: Some("crucible-turn".to_string()),
            },
        );
        validator.preauthorize(token, pod);
        router(IngestState {
            db,
            validator: Arc::new(validator),
        })
    }

    fn post_req(pod: &str, kind: &str, token: &str, body: Vec<u8>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/api/pods/{pod}/artifacts/{kind}"))
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::from(body))
            .expect("request")
    }

    async fn json_body<T: serde::de::DeserializeOwned>(resp: Response) -> T {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn stores_then_dedups_by_digest(pool: PgPool) {
        let payload = b"gzip-bytes-pretend".to_vec();
        let expect_digest = content_digest(&payload);

        // First POST stores the bytes and the pointer.
        let app1 = app(pool.clone(), "tok", "crucible-turn-x");
        let resp = app1
            .oneshot(post_req(
                "crucible-turn-x",
                "scope-pack",
                "tok",
                payload.clone(),
            ))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        let r: IngestResponse = json_body(resp).await;
        assert!(r.stored, "first POST stores");
        assert_eq!(r.digest, expect_digest);
        assert_eq!(r.bytes, payload.len() as u64);

        // The bytes landed in the chunk store, and the pointer row exists.
        let owner = ArtifactOwner::PodEvidence {
            pod: "crucible-turn-x".to_string(),
        };
        let held = crate::runs::blob_store::get_artifact(&pool, &owner, "scope-pack")
            .await
            .expect("get")
            .expect("stored artifact");
        assert_eq!(held.data, payload);
        let db = Db::new(pool.clone());
        let ptr = crate::runs::work_pods::pod_artifact(db.pool(), "crucible-turn-x", "scope-pack")
            .await
            .expect("query")
            .expect("row");
        assert_eq!(ptr.digest, expect_digest);
        assert_eq!(ptr.bytes, payload.len() as i64);

        // Re-POST of the identical bytes is idempotent: 200, stored:false, one row still.
        let app2 = app(pool.clone(), "tok", "crucible-turn-x");
        let resp = app2
            .oneshot(post_req(
                "crucible-turn-x",
                "scope-pack",
                "tok",
                payload.clone(),
            ))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        let r: IngestResponse = json_body(resp).await;
        assert!(!r.stored, "re-POST of a held digest dedups");
        assert_eq!(r.digest, expect_digest);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pod_artifacts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(n, 1);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn oversize_is_413_with_the_limit(pool: PgPool) {
        // scope-pack cap is 16 MiB; forge a body one byte over.
        let over = ArtifactKind::ScopePack.max_bytes() as usize + 1;
        let big = vec![0u8; over];
        let app = app(pool.clone(), "tok", "crucible-turn-x");
        let resp = app
            .oneshot(post_req("crucible-turn-x", "scope-pack", "tok", big))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let e: IngestError = json_body(resp).await;
        assert_eq!(e.limit_bytes, Some(ArtifactKind::ScopePack.max_bytes()));
        // Nothing was published — the aborted transaction left no artifact.
        let owner = ArtifactOwner::PodEvidence {
            pod: "crucible-turn-x".to_string(),
        };
        assert!(
            crate::runs::blob_store::get_artifact(&pool, &owner, "scope-pack")
                .await
                .expect("get")
                .is_none(),
            "an oversize body must not leave an artifact"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn unknown_kind_is_404(pool: PgPool) {
        let app = app(pool, "tok", "crucible-turn-x");
        let resp = app
            .oneshot(post_req(
                "crucible-turn-x",
                "not-a-kind",
                "tok",
                b"x".to_vec(),
            ))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn missing_bearer_is_401(pool: PgPool) {
        let app = app(pool, "tok", "crucible-turn-x");
        let req = Request::builder()
            .method("POST")
            .uri("/api/pods/crucible-turn-x/artifacts/scope-pack")
            .body(Body::from(b"x".to_vec()))
            .expect("req");
        let resp = app.oneshot(req).await.expect("resp");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn untrusted_token_fails_closed_503(pool: PgPool) {
        // Validator trusts "tok"; a POST with a different bearer isn't cached and has no kube client
        // to fall back on → fail closed 503.
        let app = app(pool, "tok", "crucible-turn-x");
        let resp = app
            .oneshot(post_req(
                "crucible-turn-x",
                "scope-pack",
                "other-token",
                b"x".to_vec(),
            ))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
