//! Which run each sandbox belongs to. Only a run pod, authenticated by its own pod-bound token,
//! binds a sandbox, and only to its own run; the token endpoint trusts nothing else.

use crate::agent_auth::AgentAuth;
use crate::agent_auth::directory::DirectoryError;
use crate::runs::ingest_auth::{IngestValidator, PodAuth};
use crate::runs::ingest_drop::IngestState;
use crate::runs::workpod::WorkPodState;
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use crucible_contract::sandbox::SandboxBinding;
use sqlx::PgPool;
use std::sync::Arc;

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

/// A run pod's request to bind a sandbox: its id, and where its workspace sits inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindRequest {
    pub id: SandboxId,
    /// An absolute, normal path, the `workdir` claim.
    pub workdir: Option<String>,
}

/// A bound sandbox: the request plus the name its `Sandbox` resource carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub id: SandboxId,
    /// A Kubernetes label value, the `sandbox` claim.
    pub name: String,
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

impl TryFrom<&SandboxBinding> for BindRequest {
    type Error = BindingError;

    fn try_from(wire: &SandboxBinding) -> Result<Self, Self::Error> {
        let id = SandboxId::parse(&wire.sandbox_id)?;
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
        Ok(Self { id, workdir })
    }
}

impl Binding {
    /// `request` under the `name` its Sandbox resource carries, which must be a label value.
    pub fn new(request: BindRequest, name: String) -> Result<Self, BindingError> {
        let bytes = name.as_bytes();
        let label = name.len() <= 63
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !label {
            return Err(BindingError::Name(name));
        }
        Ok(Self {
            id: request.id,
            name,
            workdir: request.workdir,
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
    #[error("sandbox name {name} is already bound to pod {holder}")]
    NameTaken { name: String, holder: String },
    #[error("reading or writing the sandbox binding")]
    Db(#[source] anyhow::Error),
}

impl From<sqlx::Error> for BindError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e.into())
    }
}

/// Bind a sandbox to the run `pod` is executing. Rebinding from the same pod refreshes the name
/// and workdir, and a name the same pod bound to an earlier sandbox moves to this one, as does a
/// name a finished run's pod held. A sandbox bound by another pod, or a name another running pod
/// on the same cluster holds, is refused. Returns the run id.
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
    let mut tx = pool.begin().await?;
    let named: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT rs.sandbox_id, rs.pod_name, COALESCE(wp.state = 'running', false)
           FROM run_sandboxes rs
           LEFT JOIN work_pods wp ON wp.pod_name = rs.pod_name
          WHERE rs.sandbox_name = $1 AND rs.sandbox_id <> $2
            AND (wp.cluster IS NULL OR wp.cluster = $3)
          FOR UPDATE OF rs",
    )
    .bind(&binding.name)
    .bind(sandbox.as_str())
    .bind(&row.cluster)
    .fetch_all(&mut *tx)
    .await?;
    for (other, holder, live) in named {
        if live && holder != pod {
            return Err(BindError::NameTaken {
                name: binding.name.clone(),
                holder,
            });
        }
        sqlx::query("DELETE FROM run_sandboxes WHERE sandbox_id = $1")
            .bind(other)
            .execute(&mut *tx)
            .await?;
    }
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
    .fetch_optional(&mut *tx)
    .await?;
    if bound.is_some() {
        tx.commit().await?;
        return Ok(run_id);
    }
    let holder: String =
        sqlx::query_scalar("SELECT pod_name FROM run_sandboxes WHERE sandbox_id = $1")
            .bind(sandbox.as_str())
            .fetch_one(&mut *tx)
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
    /// The principal the run was dispatched for (`user:<login>` for a person).
    pub dispatched_by: Option<String>,
    /// `None` for a sandbox bound before names were recorded.
    pub sandbox_name: Option<String>,
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
        Option<String>,
        Option<String>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT s.run_id, s.pod_name, w.state, w.cluster, r.dispatched_by, s.sandbox_name, s.workdir
         FROM run_sandboxes s
         JOIN work_pods w ON w.pod_name = s.pod_name
         JOIN runs r ON r.run_id = s.run_id
         WHERE s.sandbox_id = $1",
    )
    .bind(sandbox.as_str())
    .fetch_optional(pool)
    .await?;
    let (run_id, pod_name, state, cluster, dispatched_by, sandbox_name, workdir) =
        row.ok_or_else(|| ResolveError::Unbound(sandbox.clone()))?;
    if state != WorkPodState::Running {
        return Err(ResolveError::Ended { run_id, state });
    }
    Ok(BoundRun {
        run_id,
        pod_name,
        cluster,
        dispatched_by,
        sandbox_name,
        workdir,
    })
}

/// What the bind route needs: the pod-token validator, and the issuer's view of where sandboxes
/// live.
#[derive(Clone)]
pub(crate) struct BindState {
    pub(crate) ingest: IngestState,
    pub(crate) agent: Arc<AgentAuth>,
}

impl FromRef<BindState> for Arc<IngestValidator> {
    fn from_ref(state: &BindState) -> Self {
        state.ingest.validator.clone()
    }
}

pub(crate) fn router(state: BindState) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/pods/{pod}/sandboxes",
            axum::routing::post(bind_sandbox),
        )
        .with_state(state)
}

fn refuse(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": error.into() }))).into_response()
}

async fn bind_sandbox(
    auth: PodAuth,
    State(state): State<BindState>,
    body: Result<Json<SandboxBinding>, JsonRejection>,
) -> Response {
    let wire = match body {
        Ok(Json(wire)) => wire,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let request = match BindRequest::try_from(&wire) {
        Ok(r) => r,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let pool = state.ingest.db.pool();
    let row = match crate::runs::work_pods::get_work_pod(pool, &auth.pod).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return refuse(
                StatusCode::NOT_FOUND,
                BindError::UnknownPod(auth.pod).to_string(),
            );
        }
        Err(e) => {
            tracing::error!(pod = %auth.pod, error = format!("{e:#}"), "sandbox bind failed");
            return refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "reading the work pod failed",
            );
        }
    };
    let profile =
        match crate::agent_auth::placement::load_profile(state.agent.deploy_profile.as_deref())
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(
                    error = format!("{e:#}"),
                    "sandbox bind could not read the deploy profile"
                );
                return refuse(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "reading the deploy profile failed",
                );
            }
        };
    let namespace = match crate::agent_auth::placement::sandbox_namespace(profile.as_ref()) {
        Ok(ns) => ns.to_string(),
        Err(e) => return refuse(StatusCode::CONFLICT, e.to_string()),
    };
    let name = match state
        .agent
        .directory
        .sandbox_name(&row.cluster, &namespace, &request.id, &auth.pod)
        .await
    {
        Ok(Some(name)) => name,
        Ok(None) => {
            return refuse(
                StatusCode::NOT_FOUND,
                format!(
                    "no Sandbox with id {} in {namespace} created by this run's gateway",
                    request.id
                ),
            );
        }
        Err(e @ DirectoryError::Unreachable { .. }) => {
            tracing::warn!(pod = %auth.pod, reason = %e, "sandbox bind could not reach the cluster");
            return refuse(StatusCode::SERVICE_UNAVAILABLE, e.to_string());
        }
        Err(e) => return refuse(StatusCode::CONFLICT, e.to_string()),
    };
    let binding = match Binding::new(request, name) {
        Ok(b) => b,
        Err(e) => return refuse(StatusCode::CONFLICT, e.to_string()),
    };
    let sandbox = &binding.id;
    match bind(pool, &auth.pod, &binding).await {
        Ok(run_id) => {
            tracing::info!(pod = %auth.pod, run_id, sandbox = %sandbox, name = binding.name, "sandbox bound");
            if let Err(e) = state
                .agent
                .directory
                .adopt(&row.cluster, &namespace, sandbox, &auth.pod)
                .await
            {
                tracing::warn!(pod = %auth.pod, sandbox = %sandbox, reason = %e, "the bound sandbox could not be owned by its gateway pod; teardown deletes it by label");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e @ (BindError::UnknownPod(_) | BindError::NoRun(_))) => {
            refuse(StatusCode::NOT_FOUND, e.to_string())
        }
        Err(
            e @ (BindError::NotRunning { .. }
            | BindError::Taken { .. }
            | BindError::NameTaken { .. }),
        ) => {
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
    use crate::agent_auth::testing::{
        self, CLUSTER, PROFILE, Spire, binding, running_run, set_pod_state,
    };
    use crate::runs::clusters::HUB_CLUSTER;
    use crate::runs::ingest_auth::ExpectedServiceAccount;
    use crate::runs::ingest_auth::test_harness::FakeClusters;
    use k8s_openapi::api::authentication::v1::{TokenReviewStatus, UserInfo};
    use std::collections::{BTreeMap, HashMap};
    use tower::ServiceExt;

    fn sandbox(id: &str) -> SandboxId {
        SandboxId::parse(id).unwrap()
    }

    fn wire(id: &str, workdir: Option<&str>) -> SandboxBinding {
        SandboxBinding {
            sandbox_id: id.into(),
            workdir: workdir.map(str::to_string),
        }
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

    #[test]
    fn a_request_carries_an_id_and_a_normal_absolute_workdir_and_a_binding_a_label_value_name() {
        let request = BindRequest::try_from(&wire("sbx-1", Some("/sandbox/repo/"))).unwrap();
        assert_eq!(
            request,
            BindRequest {
                id: sandbox("sbx-1"),
                workdir: Some("/sandbox/repo".into()),
            }
        );
        assert_eq!(
            BindRequest::try_from(&wire("sbx-1", None)).unwrap().workdir,
            None
        );
        for workdir in [
            "sandbox/repo",
            "/sandbox/../etc",
            "/sandbox//repo",
            "/sandbox/./repo",
            "/sand box",
            "/",
        ] {
            assert_eq!(
                BindRequest::try_from(&wire("sbx-1", Some(workdir))),
                Err(BindingError::Workdir(workdir.to_string())),
                "{workdir:?}"
            );
        }
        assert!(matches!(
            BindRequest::try_from(&wire("a/b", None)),
            Err(BindingError::Id(_))
        ));
        assert_eq!(
            Binding::new(request.clone(), "ci-0123abcd".into())
                .unwrap()
                .name,
            "ci-0123abcd"
        );
        for name in ["", "-ci", "ci-", "ci/x", "ci x", &"a".repeat(64)] {
            assert_eq!(
                Binding::new(request.clone(), name.to_string()),
                Err(BindingError::Name(name.to_string())),
                "{name:?}"
            );
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_bound_sandbox_resolves_to_its_running_run(pool: PgPool) {
        running_run(&pool, "crucible-run-a", "run-a", Some("user:alice")).await;
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
        bind(&pool, "crucible-run-a", &binding("sbx-2"))
            .await
            .unwrap();
        for id in ["sbx-1", "sbx-2"] {
            let run = resolve(&pool, &sandbox(id)).await.unwrap();
            assert_eq!(
                (
                    run.run_id.as_str(),
                    run.pod_name.as_str(),
                    run.cluster.as_str()
                ),
                ("run-a", "crucible-run-a", CLUSTER)
            );
            assert_eq!(run.sandbox_name, Some(format!("ci-{id}")));
            assert_eq!(run.workdir.as_deref(), Some("/sandbox/repo"));
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_name_moves_to_the_same_pods_next_sandbox_and_is_refused_to_another_pod(
        pool: PgPool,
    ) {
        running_run(&pool, "crucible-run-a", "run-a", Some("user:alice")).await;
        running_run(&pool, "crucible-run-b", "run-b", Some("user:bob")).await;
        let named = |id: &str, name: &str| Binding {
            name: name.to_string(),
            ..binding(id)
        };
        bind(&pool, "crucible-run-a", &named("turn-1", "ci-ws"))
            .await
            .unwrap();
        bind(&pool, "crucible-run-a", &named("turn-2", "ci-ws"))
            .await
            .unwrap();
        assert!(
            matches!(
                resolve(&pool, &sandbox("turn-1")).await,
                Err(ResolveError::Unbound(_))
            ),
            "the earlier turn's sandbox gave up its name"
        );
        assert_eq!(
            resolve(&pool, &sandbox("turn-2"))
                .await
                .unwrap()
                .sandbox_name
                .as_deref(),
            Some("ci-ws")
        );
        match bind(&pool, "crucible-run-b", &named("other", "ci-ws")).await {
            Err(BindError::NameTaken { name, holder }) => {
                assert_eq!(
                    (name.as_str(), holder.as_str()),
                    ("ci-ws", "crucible-run-a")
                )
            }
            other => panic!("{other:?}"),
        }

        set_pod_state(&pool, "crucible-run-a", WorkPodState::Collected).await;
        bind(&pool, "crucible-run-b", &named("other", "ci-ws"))
            .await
            .expect("a finished run's name is free again");
        assert_eq!(
            resolve(&pool, &sandbox("other")).await.unwrap().run_id,
            "run-b"
        );

        crate::runs::work_pods::insert_work_pod(
            &pool,
            &crate::runs::workpod::NewWorkPod {
                pod_name: "crucible-run-c".into(),
                kind: "run".into(),
                issue_key: None,
                state: WorkPodState::Running,
                cost_tag: "run".into(),
                cluster: "elsewhere".into(),
            },
        )
        .await
        .unwrap();
        crate::runs::store::insert_run(
            &pool,
            &crate::NewRun {
                run_id: "run-c".into(),
                scope: None,
                issue: None,
                identity_digest: None,
                status: "running".into(),
                pod: Some("crucible-run-c".into()),
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await
        .unwrap();
        bind(&pool, "crucible-run-c", &named("there", "ci-ws"))
            .await
            .expect("the same name on another cluster is another sandbox");
        assert_eq!(
            resolve(&pool, &sandbox("other")).await.unwrap().run_id,
            "run-b",
            "and run-b keeps its own"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_sandbox_bound_by_one_pod_cannot_be_claimed_by_another(pool: PgPool) {
        running_run(&pool, "crucible-run-a", "run-a", Some("user:alice")).await;
        running_run(&pool, "crucible-run-b", "run-b", Some("user:bob")).await;
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
        running_run(&pool, "crucible-run-a", "run-a", Some("user:alice")).await;
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

    /// The bind route with the real TokenReview validator over a cluster boundary that vouches for
    /// `token_pod`'s token, and a directory that knows `known` `(run pod, sandbox id)` pairs.
    async fn app(
        pool: PgPool,
        token_pod: &str,
        profile: &str,
        known: &[(&str, &str)],
    ) -> (
        axum::Router,
        tempfile::TempDir,
        Arc<crate::agent_auth::testing::StaticDirectory>,
    ) {
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
        let spire = Spire::start().await;
        let (agent, dir, directory) = testing::agent_auth(&pool, &spire, profile, known);
        let state = BindState {
            ingest: IngestState {
                db: crate::Db::new(pool),
                validator,
            },
            agent,
        };
        (router(state), dir, directory)
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
    async fn the_route_binds_under_the_name_the_cluster_gives_for_the_pods_own_token(pool: PgPool) {
        running_run(&pool, "crucible-run-a", "run-a", Some("user:alice")).await;
        running_run(&pool, "crucible-run-b", "run-b", Some("user:alice")).await;
        let body = serde_json::to_string(&wire("sbx-1", Some("/sandbox/repo"))).unwrap();

        let (a_token, _d1, a_directory) = app(
            pool.clone(),
            "crucible-run-a",
            PROFILE,
            &[("crucible-run-a", "sbx-1")],
        )
        .await;
        let (status, text) = post(a_token.clone(), "crucible-run-b", &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");

        let (status, text) = post(a_token.clone(), "crucible-run-a", &body).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
        let run = resolve(&pool, &sandbox("sbx-1")).await.unwrap();
        assert_eq!(
            (run.run_id.as_str(), run.sandbox_name.as_deref()),
            ("run-a", Some("ci-sbx-1")),
            "the name came from the cluster, not the request"
        );
        assert_eq!(
            *a_directory.adopted.lock().unwrap(),
            [("crucible-run-a".to_string(), "sbx-1".to_string())],
            "the bound Sandbox is owned by run-a's gateway pod"
        );

        let (b_token, _d2, _) = app(
            pool.clone(),
            "crucible-run-b",
            PROFILE,
            &[("crucible-run-a", "sbx-1")],
        )
        .await;
        let (status, text) = post(b_token.clone(), "crucible-run-b", &body).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "run-b's gateway did not create sbx-1: {text}"
        );

        let (both, _d3, _) = app(
            pool.clone(),
            "crucible-run-b",
            PROFILE,
            &[("crucible-run-a", "sbx-1"), ("crucible-run-b", "sbx-1")],
        )
        .await;
        let (status, text) = post(both, "crucible-run-b", &body).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "already bound to run-a: {text}"
        );

        let unknown = serde_json::to_string(&wire("sbx-nowhere", None)).unwrap();
        let (status, text) = post(a_token.clone(), "crucible-run-a", &unknown).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "no Sandbox resource has that id: {text}"
        );

        for bad in [
            r#"{"sandbox_id": "a/b"}"#,
            r#"{"sandbox_id": "sbx-2", "sandbox_name": "ci-victim"}"#,
            r#"{"sandbox_id": "sbx-2", "workdir": "relative"}"#,
            "not json",
        ] {
            let (status, text) = post(a_token.clone(), "crucible-run-a", bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {text}");
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_route_binds_nothing_without_a_declared_sandbox_namespace(pool: PgPool) {
        running_run(&pool, "crucible-run-a", "run-a", Some("user:alice")).await;
        let unplaced = PROFILE.replace("sandbox_namespace = \"sandboxes\"\n", "");
        let (app, _d, _) = app(
            pool.clone(),
            "crucible-run-a",
            &unplaced,
            &[("crucible-run-a", "sbx-1")],
        )
        .await;
        let body = serde_json::to_string(&wire("sbx-1", None)).unwrap();
        let (status, text) = post(app, "crucible-run-a", &body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{text}");
        assert!(resolve(&pool, &sandbox("sbx-1")).await.is_err());
    }
}
