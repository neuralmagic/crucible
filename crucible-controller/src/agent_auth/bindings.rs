//! Which run each sandbox belongs to. Only a run pod, authenticated by its own pod-bound token,
//! binds a sandbox, and only to its own run; the token endpoint trusts nothing else.

use crate::runs::ingest_auth::PodAuth;
use crate::runs::ingest_drop::IngestState;
use crate::runs::workpod::WorkPodState;
use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use crucible_contract::sandbox::SandboxBinding;
use sqlx::PgPool;

const MAX_SANDBOX_ID_LEN: usize = 128;

/// An OpenShell sandbox id: the gateway-assigned id, also the last segment of the sandbox
/// supervisor's SPIFFE ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxId(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("sandbox id {0:?} is not 1-{MAX_SANDBOX_ID_LEN} characters of [A-Za-z0-9._-]")]
pub struct SandboxIdError(pub String);

impl SandboxId {
    pub fn parse(raw: &str) -> Result<Self, SandboxIdError> {
        let valid = !raw.is_empty()
            && raw.len() <= MAX_SANDBOX_ID_LEN
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        match valid {
            true => Ok(Self(raw.to_string())),
            false => Err(SandboxIdError(raw.to_string())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SandboxId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a run pod binds: the sandbox's id, its name, and where its workspace sits inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub id: SandboxId,
    /// A Kubernetes label value, the `sandbox` claim.
    pub name: String,
    /// An absolute, normal path, the `workdir` claim.
    pub workdir: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BindingError {
    #[error(transparent)]
    Id(#[from] SandboxIdError),
    #[error(
        "sandbox name {0:?} is not a label value: at most 63 characters of [A-Za-z0-9._-], \
         alphanumeric at both ends"
    )]
    Name(String),
    #[error("sandbox workdir {0:?} is not an absolute, normal path")]
    Workdir(String),
}

impl TryFrom<&SandboxBinding> for Binding {
    type Error = BindingError;

    fn try_from(wire: &SandboxBinding) -> Result<Self, Self::Error> {
        let id = SandboxId::parse(&wire.sandbox_id)?;
        let name = &wire.sandbox_name;
        let bytes = name.as_bytes();
        let label = name.len() <= 63
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !label {
            return Err(BindingError::Name(name.clone()));
        }
        let workdir = match &wire.workdir {
            None => None,
            Some(raw) => {
                let rest = raw.trim_end_matches('/');
                let normal = rest.starts_with('/')
                    && rest.len() <= 4096
                    && rest[1..].split('/').all(|p| !matches!(p, "" | "." | ".."))
                    && !raw.chars().any(|c| c.is_whitespace() || c.is_control());
                if !normal {
                    return Err(BindingError::Workdir(raw.clone()));
                }
                Some(rest.to_string())
            }
        };
        Ok(Self {
            id,
            name: name.clone(),
            workdir,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("pod {0} has no work-pod row")]
    UnknownPod(String),
    #[error("pod {pod} is {}; only a running pod binds sandboxes", state.as_str())]
    NotRunning { pod: String, state: WorkPodState },
    #[error("pod {0} has no run to bind a sandbox to")]
    NoRun(String),
    #[error("sandbox {sandbox} is already bound to pod {holder}")]
    Taken { sandbox: SandboxId, holder: String },
    #[error("reading or writing the sandbox binding")]
    Db(#[source] anyhow::Error),
}

impl From<sqlx::Error> for BindError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e.into())
    }
}

/// Bind a sandbox to the run `pod` is executing. Rebinding from the same pod refreshes the name
/// and workdir; a sandbox already bound elsewhere is refused. Returns the run id.
pub async fn bind(pool: &PgPool, pod: &str, binding: &Binding) -> Result<String, BindError> {
    let sandbox = &binding.id;
    let row = crate::runs::work_pods::get_work_pod(pool, pod)
        .await
        .map_err(BindError::Db)?
        .ok_or_else(|| BindError::UnknownPod(pod.to_string()))?;
    if row.state != WorkPodState::Running {
        return Err(BindError::NotRunning {
            pod: pod.to_string(),
            state: row.state,
        });
    }
    let run_id: String =
        sqlx::query_scalar("SELECT run_id FROM runs WHERE pod = $1 ORDER BY seq DESC LIMIT 1")
            .bind(pod)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| BindError::NoRun(pod.to_string()))?;
    let bound: Option<String> = sqlx::query_scalar(
        "INSERT INTO run_sandboxes (sandbox_id, sandbox_name, workdir, run_id, pod_name, bound_at)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (sandbox_id) DO UPDATE SET
             sandbox_name = excluded.sandbox_name,
             workdir = excluded.workdir,
             bound_at = excluded.bound_at
           WHERE run_sandboxes.pod_name = excluded.pod_name
             AND run_sandboxes.run_id = excluded.run_id
         RETURNING run_id",
    )
    .bind(sandbox.as_str())
    .bind(&binding.name)
    .bind(binding.workdir.as_deref())
    .bind(&run_id)
    .bind(pod)
    .bind(crate::clock::now_rfc3339())
    .fetch_optional(pool)
    .await?;
    if bound.is_some() {
        return Ok(run_id);
    }
    let holder: String =
        sqlx::query_scalar("SELECT pod_name FROM run_sandboxes WHERE sandbox_id = $1")
            .bind(sandbox.as_str())
            .fetch_one(pool)
            .await?;
    Err(BindError::Taken {
        sandbox: sandbox.clone(),
        holder,
    })
}

/// A bound sandbox's run, as the token endpoint needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundRun {
    pub run_id: String,
    /// The run pod, which crucible-kueue labels the run's workloads with (a run id can exceed a
    /// label value's 63 characters).
    pub pod_name: String,
    pub cluster: String,
    /// The owner principal the run's spend is attributed to (`user:<login>` for a person).
    pub attributed_to: Option<String>,
    pub sandbox_name: String,
    pub workdir: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("sandbox {0} is bound to no run")]
    Unbound(SandboxId),
    #[error("run {run_id} is {}; its sandboxes get no tokens", state.as_str())]
    Ended { run_id: String, state: WorkPodState },
    #[error("reading the sandbox binding")]
    Db(#[from] sqlx::Error),
}

/// The live run `sandbox` is bound to. A run whose pod is anything but running is refused.
pub async fn resolve(pool: &PgPool, sandbox: &SandboxId) -> Result<BoundRun, ResolveError> {
    type Row = (
        String,
        String,
        WorkPodState,
        String,
        Option<String>,
        String,
        Option<String>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT s.run_id, s.pod_name, w.state, w.cluster, r.attributed_to, s.sandbox_name, s.workdir
         FROM run_sandboxes s
         JOIN work_pods w ON w.pod_name = s.pod_name
         JOIN runs r ON r.run_id = s.run_id
         WHERE s.sandbox_id = $1",
    )
    .bind(sandbox.as_str())
    .fetch_optional(pool)
    .await?;
    let (run_id, pod_name, state, cluster, attributed_to, sandbox_name, workdir) =
        row.ok_or_else(|| ResolveError::Unbound(sandbox.clone()))?;
    if state != WorkPodState::Running {
        return Err(ResolveError::Ended { run_id, state });
    }
    Ok(BoundRun {
        run_id,
        pod_name,
        cluster,
        attributed_to,
        sandbox_name,
        workdir,
    })
}

pub(crate) fn router(state: IngestState) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/pods/{pod}/sandboxes",
            axum::routing::post(bind_sandbox),
        )
        .with_state(state)
}

async fn bind_sandbox(
    auth: PodAuth,
    State(state): State<IngestState>,
    body: Result<Json<SandboxBinding>, JsonRejection>,
) -> Response {
    let refuse = |status: StatusCode, error: String| {
        (status, Json(serde_json::json!({ "error": error }))).into_response()
    };
    let binding = match body {
        Ok(Json(binding)) => binding,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let binding = match Binding::try_from(&binding) {
        Ok(b) => b,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let sandbox = &binding.id;
    match bind(state.db.pool(), &auth.pod, &binding).await {
        Ok(run_id) => {
            tracing::info!(pod = %auth.pod, run_id, sandbox = %sandbox, name = binding.name, "sandbox bound");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e @ (BindError::UnknownPod(_) | BindError::NoRun(_))) => {
            refuse(StatusCode::NOT_FOUND, e.to_string())
        }
        Err(e @ (BindError::NotRunning { .. } | BindError::Taken { .. })) => {
            tracing::warn!(pod = %auth.pod, sandbox = %sandbox, reason = %e, "sandbox bind refused");
            refuse(StatusCode::CONFLICT, e.to_string())
        }
        Err(e @ BindError::Db(_)) => {
            tracing::error!(pod = %auth.pod, sandbox = %sandbox, error = format!("{e:#}"), "sandbox bind failed");
            refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::bindings::*;
    use crate::agent_auth::testing::{CLUSTER, running_run, set_pod_state};
    use crate::runs::clusters::HUB_CLUSTER;
    use crate::runs::ingest_auth::test_harness::FakeClusters;
    use crate::runs::ingest_auth::{ExpectedServiceAccount, IngestValidator};
    use k8s_openapi::api::authentication::v1::{TokenReviewStatus, UserInfo};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn sandbox(id: &str) -> SandboxId {
        SandboxId::parse(id).unwrap()
    }

    fn binding(id: &str) -> Binding {
        Binding {
            id: sandbox(id),
            name: format!("ci-{id}"),
            workdir: Some("/sandbox/repo".into()),
        }
    }

    fn wire(id: &str, name: &str, workdir: Option<&str>) -> SandboxBinding {
        SandboxBinding {
            sandbox_id: id.into(),
            sandbox_name: name.into(),
            workdir: workdir.map(str::to_string),
        }
    }

    #[test]
    fn a_binding_carries_a_label_value_name_and_a_normal_absolute_workdir() {
        assert_eq!(
            Binding::try_from(&wire("sbx-1", "ci-0123abcd", Some("/sandbox/repo/"))).unwrap(),
            Binding {
                id: sandbox("sbx-1"),
                name: "ci-0123abcd".into(),
                workdir: Some("/sandbox/repo".into()),
            }
        );
        assert_eq!(
            Binding::try_from(&wire("sbx-1", "ci-0123abcd", None))
                .unwrap()
                .workdir,
            None
        );
        for name in ["", "-ci", "ci-", "ci/x", "ci x", &"a".repeat(64)] {
            assert_eq!(
                Binding::try_from(&wire("sbx-1", name, None)),
                Err(BindingError::Name(name.to_string())),
                "{name:?}"
            );
        }
        for workdir in [
            "sandbox/repo",
            "/sandbox/../etc",
            "/sandbox//repo",
            "/sandbox/./repo",
            "/sand box",
            "/",
        ] {
            assert_eq!(
                Binding::try_from(&wire("sbx-1", "ci-1", Some(workdir))),
                Err(BindingError::Workdir(workdir.to_string())),
                "{workdir:?}"
            );
        }
        assert!(matches!(
            Binding::try_from(&wire("a/b", "ci-1", None)),
            Err(BindingError::Id(_))
        ));
    }

    #[test]
    fn sandbox_ids_are_one_path_segment_of_safe_characters() {
        for ok in [
            "0199c1e2-7a4b-7cde-8f00-1234567890ab",
            "auth-spike",
            "a.b_c",
        ] {
            assert_eq!(SandboxId::parse(ok).unwrap().as_str(), ok);
        }
        for bad in ["", "a/b", "a b", "a%2Fb", "ü", &"a".repeat(129)] {
            assert!(SandboxId::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_bound_sandbox_resolves_to_its_running_run(pool: PgPool) {
        running_run(&pool, "crucible-run-a", "run-a", Some("user:weaton")).await;
        assert_eq!(
            bind(&pool, "crucible-run-a", &binding("sbx-1"))
                .await
                .unwrap(),
            "run-a"
        );
        assert_eq!(
            bind(&pool, "crucible-run-a", &binding("sbx-1"))
                .await
                .unwrap(),
            "run-a",
            "rebinding from the same pod is a no-op"
        );
        let renamed = Binding {
            name: "ci-renamed".into(),
            workdir: None,
            ..binding("sbx-1")
        };
        bind(&pool, "crucible-run-a", &renamed).await.unwrap();
        let rebound = resolve(&pool, &sandbox("sbx-1")).await.unwrap();
        assert_eq!(
            (rebound.sandbox_name.as_str(), rebound.workdir),
            ("ci-renamed", None)
        );
        bind(&pool, "crucible-run-a", &binding("sbx-1"))
            .await
            .unwrap();
        bind(&pool, "crucible-run-a", &binding("sbx-2"))
            .await
            .unwrap();
        for id in ["sbx-1", "sbx-2"] {
            assert_eq!(
                resolve(&pool, &sandbox(id)).await.unwrap(),
                BoundRun {
                    run_id: "run-a".into(),
                    pod_name: "crucible-run-a".into(),
                    cluster: CLUSTER.into(),
                    attributed_to: Some("user:weaton".into()),
                    sandbox_name: format!("ci-{id}"),
                    workdir: Some("/sandbox/repo".into()),
                }
            );
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_sandbox_bound_by_one_pod_cannot_be_claimed_by_another(pool: PgPool) {
        running_run(&pool, "crucible-run-a", "run-a", Some("user:weaton")).await;
        running_run(&pool, "crucible-run-b", "run-b", Some("user:mallory")).await;
        bind(&pool, "crucible-run-a", &binding("sbx-1"))
            .await
            .unwrap();
        match bind(&pool, "crucible-run-b", &binding("sbx-1")).await {
            Err(BindError::Taken { holder, .. }) => assert_eq!(holder, "crucible-run-a"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            resolve(&pool, &sandbox("sbx-1")).await.unwrap().run_id,
            "run-a"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn only_a_running_pod_with_a_run_binds(pool: PgPool) {
        assert!(matches!(
            bind(&pool, "crucible-run-ghost", &binding("sbx-1")).await,
            Err(BindError::UnknownPod(_))
        ));
        running_run(&pool, "crucible-run-a", "run-a", None).await;
        set_pod_state(&pool, "crucible-run-a", WorkPodState::Collected).await;
        assert!(matches!(
            bind(&pool, "crucible-run-a", &binding("sbx-1")).await,
            Err(BindError::NotRunning {
                state: WorkPodState::Collected,
                ..
            })
        ));
        crate::runs::work_pods::insert_work_pod(
            &pool,
            &crate::runs::workpod::NewWorkPod {
                pod_name: "crucible-run-norun".into(),
                kind: "run".into(),
                issue_key: None,
                state: WorkPodState::Running,
                cost_tag: "run".into(),
                cluster: CLUSTER.into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            bind(&pool, "crucible-run-norun", &binding("sbx-1")).await,
            Err(BindError::NoRun(_))
        ));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn unbound_sandboxes_and_ended_runs_resolve_to_nothing(pool: PgPool) {
        assert!(matches!(
            resolve(&pool, &sandbox("sbx-1")).await,
            Err(ResolveError::Unbound(_))
        ));
        running_run(&pool, "crucible-run-a", "run-a", Some("user:weaton")).await;
        bind(&pool, "crucible-run-a", &binding("sbx-1"))
            .await
            .unwrap();
        for ended in [
            WorkPodState::Succeeded,
            WorkPodState::Failed,
            WorkPodState::Collected,
            WorkPodState::Swept,
        ] {
            set_pod_state(&pool, "crucible-run-a", ended).await;
            match resolve(&pool, &sandbox("sbx-1")).await {
                Err(ResolveError::Ended { run_id, state }) => {
                    assert_eq!((run_id.as_str(), state), ("run-a", ended))
                }
                other => panic!("{ended:?}: {other:?}"),
            }
        }
    }

    fn review_for(pod: &str) -> TokenReviewStatus {
        TokenReviewStatus {
            authenticated: Some(true),
            audiences: Some(vec!["crucible-ingest".into()]),
            error: None,
            user: Some(UserInfo {
                username: Some("system:serviceaccount:crucible-system:crucible-spoke".into()),
                extra: Some(BTreeMap::from([(
                    "authentication.kubernetes.io/pod-name".to_string(),
                    vec![pod.to_string()],
                )])),
                groups: None,
                uid: None,
            }),
        }
    }

    /// The bind route with the real TokenReview validator over a cluster boundary that vouches
    /// for `token_pod`'s token.
    fn app(pool: PgPool, token_pod: &str) -> axum::Router {
        let clusters = Arc::new(FakeClusters {
            namespaces: HashMap::from([(CLUSTER.to_string(), "crucible-system".to_string())]),
            statuses: HashMap::from([
                (CLUSTER.to_string(), review_for(token_pod)),
                (HUB_CLUSTER.to_string(), review_for(token_pod)),
            ]),
            ..Default::default()
        });
        let validator = Arc::new(IngestValidator::new(
            clusters,
            pool.clone(),
            "crucible-ingest",
            ExpectedServiceAccount {
                namespace: "autoresearch".into(),
                name: None,
            },
            BTreeMap::from([(CLUSTER.to_string(), "crucible-spoke".to_string())]),
        ));
        router(IngestState {
            db: crate::Db::new(pool),
            validator,
        })
    }

    async fn post(app: axum::Router, pod: &str, body: &str) -> (StatusCode, String) {
        let req = axum::http::Request::post(crucible_contract::sandbox::binding_path(pod))
            .header("authorization", "Bearer pod-token")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_route_binds_for_the_pod_its_token_is_bound_to(pool: PgPool) {
        running_run(&pool, "crucible-run-a", "run-a", Some("user:weaton")).await;
        running_run(&pool, "crucible-run-b", "run-b", Some("user:weaton")).await;
        let body =
            serde_json::to_string(&wire("sbx-1", "ci-0123abcd", Some("/sandbox/repo"))).unwrap();

        let (status, text) =
            post(app(pool.clone(), "crucible-run-a"), "crucible-run-b", &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
        assert!(resolve(&pool, &sandbox("sbx-1")).await.is_err());

        let (status, text) =
            post(app(pool.clone(), "crucible-run-a"), "crucible-run-a", &body).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
        assert_eq!(
            resolve(&pool, &sandbox("sbx-1")).await.unwrap().run_id,
            "run-a"
        );

        let (status, text) =
            post(app(pool.clone(), "crucible-run-b"), "crucible-run-b", &body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{text}");
        assert!(
            text.contains("already bound to pod crucible-run-a"),
            "{text}"
        );

        for bad in [
            r#"{"sandbox_id": "a/b", "sandbox_name": "ci-1"}"#,
            r#"{"sandbox_id": "sbx-2"}"#,
            r#"{"sandbox_id": "sbx-2", "sandbox_name": "ci-1", "workdir": "relative"}"#,
            r#"{"sandbox_id": "sbx-2", "sandbox_name": "ci-1", "run_id": "run-b"}"#,
            "not json",
        ] {
            let (status, text) =
                post(app(pool.clone(), "crucible-run-a"), "crucible-run-a", bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {text}");
        }
    }
}
