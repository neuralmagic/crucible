//! A run's evidence over HTTP (RFC-0004:C-RUN-EVIDENCE): `PUT /api/runs/{run}/evidence/{kind}`
//! stores what a run pod uploads as the run's own artifact, and `GET` hands the gzipped bytes back
//! to the run's rerun or to a person.
//!
//! The routes carry their own guard, [`require_run_token_or_human`]: a `crr_` bearer is checked as
//! a run token and answers with its [`Grant`], and anything else goes through the human guard
//! unchanged. Only a run token uploads, and only to its own run.

use crate::client::Db;
use crate::identity::auth::{HumanOrKeyGuard, require_auth_or_api_key};
use crate::runs::blob_store::{ArtifactOwner, PutArtifactError};
use crate::runs::run_token::Grant;
use crate::runs::task_evidence::safe_segment;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use axum::{Extension, Json, Router};
use crucible_contract::{ArtifactKind, EvidenceRefused, EvidenceStored};
use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct EvidenceState {
    pub(crate) db: Db,
    /// The root a local run's directory sits under, for a parent the controller stored no copy of.
    pub(crate) scratch_dir: PathBuf,
    pub(crate) guard: HumanOrKeyGuard,
}

pub(crate) fn router(state: EvidenceState) -> Router {
    Router::new()
        .route("/api/runs/{run_id}/evidence/{kind}", put(upload).get(fetch))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_run_token_or_human,
        ))
        .with_state(state)
}

/// Admit a run token with its grant, or hand the request to the human guard.
async fn require_run_token_or_human(
    State(state): State<EvidenceState>,
    mut req: Request,
    next: Next,
) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|t| crate::runs::run_token::looks_like_token(t))
        .map(str::to_string);
    let Some(presented) = presented else {
        return require_auth_or_api_key(State(state.guard), req, next).await;
    };
    match crate::runs::run_token::verify(state.db.pool(), &presented).await {
        Ok(grant) => {
            req.extensions_mut().insert(grant);
            next.run(req).await
        }
        Err(refusal) => refuse(StatusCode::UNAUTHORIZED, refusal.to_string()),
    }
}

/// The kinds a run pod uploads.
fn uploadable(kind: ArtifactKind) -> bool {
    matches!(
        kind,
        ArtifactKind::RunSession
            | ArtifactKind::RunFiles
            | ArtifactKind::RunCommits
            | ArtifactKind::OtelLog
    )
}

/// The kinds a rerun reads back.
fn fetchable(kind: ArtifactKind) -> bool {
    matches!(
        kind,
        ArtifactKind::RunSession | ArtifactKind::RunFiles | ArtifactKind::RunCommits
    )
}

/// `PUT /api/runs/{run_id}/evidence/{kind}`: store the gzipped body as the run's `kind`, replacing
/// any earlier upload. Run token only.
async fn upload(
    grant: Option<Extension<Grant>>,
    State(state): State<EvidenceState>,
    Path((run_id, kind)): Path<(String, String)>,
    body: Body,
) -> Response {
    let Some(kind) = kind.parse::<ArtifactKind>().ok().filter(|k| uploadable(*k)) else {
        return refuse(
            StatusCode::NOT_FOUND,
            format!("{kind} is not run evidence a pod uploads"),
        );
    };
    let Some(Extension(grant)) = grant else {
        return refuse(
            StatusCode::FORBIDDEN,
            "only the run's own token uploads its evidence".to_string(),
        );
    };
    if !grant.may_write(&run_id) {
        return refuse(
            StatusCode::FORBIDDEN,
            format!("this run token writes the evidence of {}", grant.run_id),
        );
    }
    let owner = ArtifactOwner::Run {
        run_id: run_id.clone(),
    };
    match crate::runs::blob_store::put_artifact(
        state.db.pool(),
        &owner,
        kind.as_str(),
        kind.max_bytes(),
        body.into_data_stream(),
    )
    .await
    {
        Ok(stored) => Json(EvidenceStored {
            kind,
            digest: stored.digest,
            bytes: stored.bytes,
        })
        .into_response(),
        Err(PutArtifactError::TooLarge { limit }) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(EvidenceRefused {
                error: format!("{kind} exceeds its cap of {limit} bytes"),
                limit_bytes: Some(limit),
            }),
        )
            .into_response(),
        Err(PutArtifactError::Source(e)) => {
            tracing::warn!(%run_id, %kind, error = %e, "run evidence upload: reading the body failed");
            refuse(StatusCode::BAD_REQUEST, format!("reading the body: {e}"))
        }
        Err(PutArtifactError::Store(e)) => {
            tracing::error!(%run_id, %kind, error = format!("{e:#}"), "run evidence upload: storing failed");
            refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storing the evidence failed".to_string(),
            )
        }
    }
}

/// `GET /api/runs/{run_id}/evidence/{kind}`: the run's `kind` as gzipped bytes, from the store, or
/// for a local run from its directory on this controller's disk.
async fn fetch(
    grant: Option<Extension<Grant>>,
    State(state): State<EvidenceState>,
    Path((run_id, kind)): Path<(String, String)>,
) -> Response {
    let Some(kind) = kind.parse::<ArtifactKind>().ok().filter(|k| fetchable(*k)) else {
        return refuse(
            StatusCode::NOT_FOUND,
            format!("{kind} is not run evidence a rerun reads"),
        );
    };
    if let Some(Extension(grant)) = &grant
        && !grant.may_read(&run_id)
    {
        return refuse(
            StatusCode::FORBIDDEN,
            "this run token does not read that run's evidence".to_string(),
        );
    }
    if !safe_segment(&run_id) {
        return refuse(
            StatusCode::BAD_REQUEST,
            "a run id is one path segment".to_string(),
        );
    }
    match evidence_bytes(&state, &run_id, kind).await {
        Ok(Some(gz)) => ([(header::CONTENT_TYPE, "application/gzip")], gz).into_response(),
        Ok(None) => refuse(StatusCode::NOT_FOUND, format!("run {run_id} has no {kind}")),
        Err(e) => {
            tracing::error!(%run_id, %kind, error = format!("{e:#}"), "run evidence fetch failed");
            refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "reading the evidence failed".to_string(),
            )
        }
    }
}

/// The run's `kind`, gzipped: the stored artifact when there is one, else what a local run left
/// in its state directory.
async fn evidence_bytes(
    state: &EvidenceState,
    run_id: &str,
    kind: ArtifactKind,
) -> anyhow::Result<Option<Vec<u8>>> {
    let owner = ArtifactOwner::Run {
        run_id: run_id.to_string(),
    };
    if let Some(payload) =
        crate::runs::blob_store::get_artifact(state.db.pool(), &owner, kind.as_str()).await?
    {
        if payload.data.starts_with(&[0x1f, 0x8b]) {
            return Ok(Some(payload.data));
        }
        return Ok(Some(crucible_contract::artifact::gzip(&payload.data)?));
    }
    let local = crate::runs::task_evidence::state_dir(&state.scratch_dir, run_id);
    tokio::task::spawn_blocking(move || -> std::io::Result<Option<Vec<u8>>> {
        let path = match kind {
            ArtifactKind::RunSession => local.join("session.jsonl"),
            ArtifactKind::RunFiles => local.join("files"),
            _ => local.join(crucible::plan::rerun::GIT_MEMORY_DIR),
        };
        if kind == ArtifactKind::RunSession {
            return match std::fs::read(&path) {
                Ok(raw) => crucible_contract::artifact::gzip(&raw).map(Some),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            };
        }
        if !path.is_dir() {
            return Ok(None);
        }
        crucible_contract::artifact::tar_gz_dir(&path).map(Some)
    })
    .await?
    .map_err(anyhow::Error::from)
}

fn refuse(status: StatusCode, error: String) -> Response {
    (
        status,
        Json(EvidenceRefused {
            error,
            limit_bytes: None,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use crate::client::Db;
    use crate::identity::auth::{AuthMode, BearerGuard, HumanOrKeyGuard, SharedToken};
    use crate::runs::blob_store::ArtifactOwner;
    use crate::runs::evidence_api::{EvidenceState, router};
    use crate::runs::run_token::{mint, revoke_run};
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use crucible_contract::{ArtifactKind, EvidenceRefused, EvidenceStored};
    use sqlx::PgPool;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    const HOUR: Duration = Duration::from_secs(3600);
    const MACHINE: &str = "machine-token";

    /// The evidence routes over a real store, with a human guard that admits only the static
    /// machine token.
    fn app(pool: &PgPool, scratch: &Path) -> axum::Router {
        let guard = BearerGuard::new(SharedToken::new(MACHINE), None, None, None, AuthMode::Proxy)
            .expect("guard");
        router(EvidenceState {
            db: Db::new(pool.clone()),
            scratch_dir: scratch.to_path_buf(),
            guard: HumanOrKeyGuard {
                guard: Arc::new(guard),
                pool: pool.clone(),
            },
        })
    }

    fn req(method: &str, uri: &str, bearer: Option<&str>, body: Vec<u8>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(t) = bearer {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::from(body)).expect("request")
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        uri: &str,
        bearer: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let resp = app
            .clone()
            .oneshot(req(method, uri, bearer, body))
            .await
            .expect("response");
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        (status, bytes.to_vec())
    }

    fn gz(raw: &[u8]) -> Vec<u8> {
        crucible_contract::artifact::gzip(raw).expect("gzip")
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_run_token_uploads_its_own_evidence_and_reads_it_back(pool: PgPool) {
        let scratch = tempfile::tempdir().expect("scratch");
        let app = app(&pool, scratch.path());
        let token = mint(&pool, "run-b", None, HOUR).await.expect("mint");

        for (kind, body) in [
            ("run-files", gz(b"files-tar")),
            ("run-commits", gz(b"commits-tar")),
            ("otel-log", gz(b"{}\n")),
            ("run-session", gz(b"{\"v\":1}\n")),
        ] {
            let (status, resp) = call(
                &app,
                "PUT",
                &format!("/api/runs/run-b/evidence/{kind}"),
                Some(&token),
                body.clone(),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{kind}: {}",
                String::from_utf8_lossy(&resp)
            );
            let stored: EvidenceStored = serde_json::from_slice(&resp).expect("json");
            assert_eq!(stored.digest, crucible_contract::content_digest(&body));
            let held = crate::runs::blob_store::get_artifact(
                &pool,
                &ArtifactOwner::Run {
                    run_id: "run-b".to_string(),
                },
                kind,
            )
            .await
            .expect("get")
            .expect("stored as the run's own artifact");
            assert_eq!(held.data, body);
        }
        assert_eq!(
            crate::runs::blob_store::get_run_session(&pool, "run-b")
                .await
                .expect("session")
                .as_deref(),
            Some("{\"v\":1}\n"),
            "the uploaded session is the one completion reads"
        );

        let (status, got) = call(
            &app,
            "GET",
            "/api/runs/run-b/evidence/run-commits",
            Some(&token),
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got, gz(b"commits-tar"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_run_token_writes_only_its_run_and_reads_only_its_parent_besides(pool: PgPool) {
        let scratch = tempfile::tempdir().expect("scratch");
        let app = app(&pool, scratch.path());
        for run in ["run-a", "run-c"] {
            crate::runs::blob_store::put_run_bundle(
                &pool,
                run,
                ArtifactKind::RunFiles,
                gz(run.as_bytes()),
            )
            .await
            .expect("seed");
        }
        let token = mint(&pool, "run-b", Some("run-a"), HOUR)
            .await
            .expect("mint");

        let (status, got) = call(
            &app,
            "GET",
            "/api/runs/run-a/evidence/run-files",
            Some(&token),
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the parent is readable");
        assert_eq!(got, gz(b"run-a"));

        let (status, _) = call(
            &app,
            "GET",
            "/api/runs/run-c/evidence/run-files",
            Some(&token),
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "an unrelated run is not");

        for run in ["run-a", "run-c"] {
            let (status, _) = call(
                &app,
                "PUT",
                &format!("/api/runs/{run}/evidence/run-files"),
                Some(&token),
                gz(b"overwrite"),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{run} is not the token's run"
            );
        }
        assert_eq!(
            crate::runs::blob_store::get_run_bundle(&pool, "run-a", ArtifactKind::RunFiles)
                .await
                .expect("get"),
            Some(gz(b"run-a")),
            "the parent's evidence is untouched"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_human_reads_evidence_and_never_uploads_it(pool: PgPool) {
        let scratch = tempfile::tempdir().expect("scratch");
        let app = app(&pool, scratch.path());
        crate::runs::blob_store::put_run_session(&pool, "run-a", b"{\"v\":1}\n")
            .await
            .expect("seed");

        let (status, got) = call(
            &app,
            "GET",
            "/api/runs/run-a/evidence/run-session",
            Some(MACHINE),
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            crucible_contract::artifact::gunzip(&got).expect("gzip"),
            b"{\"v\":1}\n"
        );

        let (status, _) = call(
            &app,
            "GET",
            "/api/runs/run-a/evidence/run-session",
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "no credential, no evidence"
        );

        let (status, _) = call(
            &app,
            "PUT",
            "/api/runs/run-a/evidence/run-session",
            Some(MACHINE),
            gz(b"forged"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_revoked_or_unknown_token_is_refused(pool: PgPool) {
        let scratch = tempfile::tempdir().expect("scratch");
        let app = app(&pool, scratch.path());
        let token = mint(&pool, "run-b", None, HOUR).await.expect("mint");
        revoke_run(&pool, "run-b").await.expect("revoke");
        for bearer in [token.as_str(), "crr_nosuch_secret"] {
            let (status, body) = call(
                &app,
                "PUT",
                "/api/runs/run-b/evidence/run-session",
                Some(bearer),
                gz(b"x"),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            let refused: EvidenceRefused = serde_json::from_slice(&body).expect("json");
            assert!(refused.error.contains("run token"), "{}", refused.error);
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn only_run_evidence_kinds_are_routed(pool: PgPool) {
        let scratch = tempfile::tempdir().expect("scratch");
        let app = app(&pool, scratch.path());
        let token = mint(&pool, "run-b", None, HOUR).await.expect("mint");
        for kind in ["scope-pack", "scope-transcript", "nope"] {
            let (status, _) = call(
                &app,
                "PUT",
                &format!("/api/runs/run-b/evidence/{kind}"),
                Some(&token),
                gz(b"x"),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "PUT {kind}");
        }
        for kind in ["otel-log", "scope-pack"] {
            let (status, _) = call(
                &app,
                "GET",
                &format!("/api/runs/run-b/evidence/{kind}"),
                Some(&token),
                Vec::new(),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "GET {kind}");
        }
        let (status, _) = call(
            &app,
            "GET",
            "/api/runs/run-b/evidence/run-session",
            Some(&token),
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "nothing uploaded yet");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_oversize_upload_is_413_and_stores_nothing(pool: PgPool) {
        let scratch = tempfile::tempdir().expect("scratch");
        let app = app(&pool, scratch.path());
        let token = mint(&pool, "run-b", None, HOUR).await.expect("mint");
        let over = vec![0u8; ArtifactKind::OtelLog.max_bytes() as usize + 1];
        let (status, body) = call(
            &app,
            "PUT",
            "/api/runs/run-b/evidence/otel-log",
            Some(&token),
            over,
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let refused: EvidenceRefused = serde_json::from_slice(&body).expect("json");
        assert_eq!(refused.limit_bytes, Some(ArtifactKind::OtelLog.max_bytes()));
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(n, 0);
    }

    /// A local run's evidence is never uploaded: it is read off the run's directory.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_local_run_is_served_from_its_directory(pool: PgPool) {
        let scratch = tempfile::tempdir().expect("scratch");
        let state = crate::runs::task_evidence::state_dir(scratch.path(), "playbook:x#1-17");
        std::fs::create_dir_all(state.join("files/triage")).expect("mkdir");
        std::fs::create_dir_all(state.join("git-memory")).expect("mkdir");
        std::fs::write(state.join("session.jsonl"), b"{\"v\":1}\n").expect("session");
        std::fs::write(state.join("files/triage/out.json"), b"{}").expect("file");
        std::fs::write(state.join("git-memory/tasks.json"), b"[]").expect("memory");
        let app = app(&pool, scratch.path());
        let get = |kind: &'static str| {
            let app = app.clone();
            async move {
                call(
                    &app,
                    "GET",
                    &format!("/api/runs/playbook:x%231-17/evidence/{kind}"),
                    Some(MACHINE),
                    Vec::new(),
                )
                .await
            }
        };

        let (status, session) = get("run-session").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            crucible_contract::artifact::gunzip(&session).expect("gzip"),
            b"{\"v\":1}\n"
        );
        for (kind, file, want) in [
            ("run-files", "triage/out.json", &b"{}"[..]),
            ("run-commits", "tasks.json", &b"[]"[..]),
        ] {
            let (status, tar) = get(kind).await;
            assert_eq!(status, StatusCode::OK, "{kind}");
            let out = tempfile::tempdir().expect("out");
            crucible_contract::artifact::untar_gz(&tar, out.path()).expect("untar");
            assert_eq!(std::fs::read(out.path().join(file)).expect(file), want);
        }

        std::fs::remove_dir_all(state.join("files")).expect("rm");
        let (status, _) = get("run-files").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "a run that captured nothing");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_traversing_run_id_is_refused(pool: PgPool) {
        let scratch = tempfile::tempdir().expect("scratch");
        let app = app(&pool, scratch.path());
        let (status, _) = call(
            &app,
            "GET",
            "/api/runs/..%2F..%2Fetc/evidence/run-session",
            Some(MACHINE),
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// A run token is refused everywhere but its evidence routes, however valid.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_human_surface_refuses_a_run_token(pool: PgPool) {
        let token = mint(&pool, "run-b", None, HOUR).await.expect("mint");
        let human = axum::Router::new()
            .route("/api/runs/{run_id}", axum::routing::get(|| async { "run" }))
            .layer(axum::middleware::from_fn_with_state(
                HumanOrKeyGuard {
                    guard: Arc::new(BearerGuard::default()),
                    pool: pool.clone(),
                },
                crate::identity::auth::require_auth_or_api_key,
            ));
        let (status, body) = call(&human, "GET", "/api/runs/run-b", Some(&token), Vec::new()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            String::from_utf8_lossy(&body).contains("only its run's evidence routes"),
            "{}",
            String::from_utf8_lossy(&body)
        );
        let (status, _) = call(&human, "GET", "/api/runs/run-b", None, Vec::new()).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the open guard admits everyone else"
        );
    }
}
