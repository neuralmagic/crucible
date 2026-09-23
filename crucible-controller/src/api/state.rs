//! The served app's shared state and the response types every handler returns. Slice handler
//! modules depend on this and on [`crate::api::dto`]; only [`crate::api`] itself assembles the router.

use crate::client::Db;
use crate::daemon::queue::{Enqueue, OverrideSink};
#[cfg(feature = "autoresearch")]
use crate::issues::repo_ref::RepoWhitelist;
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

/// JSON request bodies and responses. Decoding goes through `serde_json::Value`, which is the
/// only reader that carries the engine's arbitrary-precision numbers through tagged enums and
/// flattened structs; a body that parses but does not fit the type is a 422 like `axum::Json`.
pub(crate) struct Json<T>(pub(crate) T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> FromRequest<S> for Json<T> {
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let axum::Json(value) = axum::Json::<serde_json::Value>::from_request(req, state)
            .await
            .map_err(IntoResponse::into_response)?;
        serde_json::from_value(value).map(Json).map_err(|e| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(ErrorBody::new(format!("invalid request body: {e}"))),
            )
                .into_response()
        })
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// The daemon's unattended-run caps (circuit breakers) — exposed for the overview dashboard.
#[derive(Debug, Clone)]
pub struct Caps {
    pub max_concurrent_pods: u32,
    pub max_scopes_per_day: u32,
    pub daily_cost_ceiling: f64,
}

/// Shared handler state: the read-only DB facade plus the override sink writes go through.
#[derive(Clone)]
pub struct ApiState {
    pub(crate) db: Db,
    pub(crate) schedules: crate::launches::schedules::ScheduleStore,
    pub(crate) sink: Arc<dyn OverrideSink>,
    pub(crate) queue: Arc<dyn Enqueue>,
    pub(crate) caps: Option<Caps>,
    pub(crate) roles: crate::identity::auth::Roles,
    /// The daemon's autopilot flag; `None` where no daemon loaded one.
    #[cfg(feature = "autoresearch")]
    pub(crate) autopilot: Option<crate::daemon::autopilot_flag::AutopilotFlag>,
    /// Whether the autoresearch routes are mounted: the feature is built and
    /// `CONTROLLER_AUTORESEARCH` is on.
    #[cfg(feature = "autoresearch")]
    pub(crate) autoresearch: bool,
    /// The namespace loop pods run in — the read-only live relay (`GET /api/runs/:id/live`) resolves
    /// a running run's pod IP here, the same namespace the pod dispatch uses.
    pub(crate) pod_namespace: String,
    /// The shared per-cluster kube clients: the turn live relay follows a pod on whichever cluster
    /// its ledger row names, and `stop_pod` deletes it there.
    pub(crate) clusters: Arc<crate::runs::clusters::ClusterClients>,
    /// The per-cluster GPU/Kueue snapshot cache behind `GET /api/clusters`.
    pub(crate) cluster_stats: Arc<crate::runs::cluster_stats::ClusterStats>,
    /// The TCP port a loop pod's control bridge listens on, dialed by the live relay.
    pub(crate) control_port: u16,
    /// The runtime config-override store (Lane O2): `GET /api/config` reads its resolved knobs +
    /// source tags, `PUT /api/config/overrides` writes the ConfigMap. `None` in tests / when no
    /// override store is threaded (both config endpoints then answer 503).
    pub(crate) config: Option<crate::daemon::overrides_store::ConfigStore>,
    /// The `POST /api/repos` org whitelist + env-seeded-repo exemption (Lane O3), built once off
    /// the parsed `ControllerCfg` at startup (`cfg.repo_whitelist()`). Deploy-pinned — never part
    /// of the Lane O2 runtime-override set.
    #[cfg(feature = "autoresearch")]
    pub(crate) repo_whitelist: Arc<RepoWhitelist>,
    /// `ControllerCfg::scratch_root()` — the flow-cache root (`GET /api/runs/{id}/flow`).
    pub(crate) scratch_dir: std::path::PathBuf,
    /// The daemon's manual reconcile trigger: `POST /api/reconcile` fires `notify_one` and the
    /// select loop runs a discovery poll + full non-terminal re-enqueue. The daemon holds a clone;
    /// permit semantics coalesce clicks that land while a pass runs.
    pub(crate) reconcile_now: Arc<tokio::sync::Notify>,
    /// The Jira Cloud creds the `POST /api/jira` adopt path fetches with (controller-side only).
    /// `None` when unconfigured — the endpoint then answers a clear 503 instead of a broken fetch.
    pub(crate) jira: Option<crate::launches::jira::JiraConfig>,
    /// The experiment-emission context (`ControllerCfg::emission_ctx`) behind
    /// `POST /api/emissions/run`. `None` ⇒ the endpoint answers 503.
    pub(crate) emission: Option<crate::launches::emission::EmissionCtx>,
    /// The deploy's named broker codegen contracts (`ControllerCfg::broker_contracts`): the set
    /// `GET /api/config/broker-contracts` lists and `POST /api/scenarios` validates a named
    /// `codegen_contract` against. Deploy-pinned like `repo_whitelist`, never a Lane O2 override.
    pub(crate) broker_contracts: crate::config::BrokerContracts,
    /// The admin bounds a playbook launcher's ceilings are checked against
    /// (`CONTROLLER_PLAYBOOK_MAX_COST_USD` / `CONTROLLER_PLAYBOOK_MAX_TIME`). Deploy-pinned like
    /// `broker_contracts`; a pack may not declare either.
    pub(crate) playbook_caps: crate::config::PlaybookCaps,
    /// What substrate this deployment can dispatch a pack's agent onto, derived from the deploy
    /// profile and the playbook executor. Deploy-pinned; the preview gate warns on it and the
    /// launch endpoints refuse on it.
    pub(crate) dispatch: crate::playbooks::dispatch::DispatchCapability,
    /// The GitHub App the graduation PR pushes as, when the deploy configured one; `None` falls
    /// back to the PAT chain the scope-pack PR already uses.
    pub(crate) pack_pr_app: Option<crate::secrets::github_app::GithubAppTokenSource>,
    /// This controller's own externally reachable base URL (`CONTROLLER_PUBLIC_URL`), which the
    /// co-draft skill file and setup commands are rendered against. `None` falls back to the
    /// forwarded host of the request that asked.
    pub(crate) public_url: Option<String>,
    /// The hub's one Vault client, and the whole reason the secrets registry can hold bytes.
    /// `None` — local mode, a deployment with no Vault reach, tests — answers the registry's
    /// write routes with 503 instead of storing metadata that points at nothing.
    pub(crate) vault: Option<Arc<crate::secrets::vault::VaultClient>>,
    /// The environment variable names the deploy profile already fills from pre-created cluster
    /// Secrets. The preview gate warns when a pack declares a secret one of these also supplies.
    pub(crate) profile_secret_env: Arc<Vec<String>>,
    /// Which identity model the human surface runs, reported by `GET /api/whoami` so the SPA knows
    /// whose sign-in URLs to use.
    pub(crate) auth_mode: crate::identity::auth::AuthMode,
    /// The chart-mounted key the offline credential is sealed under. `None` — no key mounted —
    /// answers the revoke route 503, because nothing was ever stored.
    pub(crate) credential_keys: Option<Arc<crate::identity::oidc::credentials::CredentialKeys>>,
    /// The issuer, so a revoke can tell it the token is dead too. `None` leaves a revoke local.
    pub(crate) oidc: Option<Arc<crate::identity::oidc::OidcProvider>>,
    /// The dispatch-target contract records `GET /api/config` reports; the same registry the
    /// launch gate reads.
    pub(crate) contracts: Arc<crate::runs::contract::ContractRegistry>,
    /// `POST /api/images/refresh` fires this; the catalog watcher sweeps on it.
    pub(crate) images_refresh: Arc<tokio::sync::Notify>,
    /// The cluster a launch that names no target dispatches onto (`CONTROLLER_DISPATCH_CLUSTER`).
    pub(crate) default_cluster: String,
    /// Who may dispatch to which shared cluster. Empty = every connected cluster is open to every
    /// authenticated caller, which is what a deployment configuring no policy keeps.
    pub(crate) cluster_policy: Arc<crate::runs::dispatch_target::ClusterPolicy>,
    /// The policy set in force (RFC-0003 C-POLICY), swapped whole by `activate`.
    pub(crate) policy: crate::authz::policy::ActivePolicy,
}

impl ApiState {
    /// The catalog watcher's refresh signal, fired by `POST /api/images/refresh`.
    pub fn images_refresh(&self) -> Arc<tokio::sync::Notify> {
        self.images_refresh.clone()
    }

    /// The trackers a watch may sweep on this controller, built from the same Jira credentials
    /// the adopt path uses.
    pub(crate) fn trackers(&self) -> crate::launches::tracker::Trackers {
        crate::launches::jira::trackers(self.jira.clone())
    }

    /// Assemble the shared handler state: the runtime handles the daemon owns, plus the
    /// deploy-pinned fields (caps, roles, whitelist, jira, …) read off the parsed [`ControllerCfg`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Db,
        sink: Arc<dyn OverrideSink>,
        queue: Arc<dyn Enqueue>,
        clusters: Arc<crate::runs::clusters::ClusterClients>,
        config: Option<crate::daemon::overrides_store::ConfigStore>,
        reconcile_now: Arc<tokio::sync::Notify>,
        contracts: Arc<crate::runs::contract::ContractRegistry>,
        policy: crate::authz::policy::ActivePolicy,
        cfg: &crate::config::ControllerCfg,
    ) -> Self {
        let schedules = crate::launches::schedules::ScheduleStore::new(db.clone());
        Self {
            db,
            schedules,
            sink,
            queue,
            caps: Some(Caps {
                max_concurrent_pods: cfg.profile.max_concurrent_pods,
                max_scopes_per_day: cfg.profile.max_scopes_per_day,
                daily_cost_ceiling: cfg.profile.daily_cost_ceiling,
            }),
            roles: crate::identity::auth::Roles::new(
                cfg.admins.clone(),
                cfg.operators.clone(),
                cfg.operator_groups.clone(),
            ),
            #[cfg(feature = "autoresearch")]
            autopilot: cfg.autopilot.clone(),
            #[cfg(feature = "autoresearch")]
            autoresearch: cfg.autoresearch_enabled(),
            pod_namespace: cfg.pod_namespace.clone(),
            cluster_stats: Arc::new(crate::runs::cluster_stats::ClusterStats::new(
                clusters.clone(),
            )),
            clusters,
            control_port: cfg.control_port,
            config,
            #[cfg(feature = "autoresearch")]
            repo_whitelist: Arc::new(cfg.repo_whitelist()),
            scratch_dir: cfg.scratch_root().to_path_buf(),
            reconcile_now,
            jira: cfg.jira_config(),
            emission: crate::launches::emission::EmissionCtx::from_cfg(cfg),
            broker_contracts: cfg.broker_contracts.clone(),
            playbook_caps: crate::config::PlaybookCaps {
                max_cost: cfg.playbook_max_cost_cap,
                max_time: cfg.playbook_max_time_cap.clone(),
            },
            dispatch: crate::playbooks::dispatch::DispatchCapability::from_cfg(cfg),
            default_cluster: cfg.dispatch_cluster.clone(),
            // A malformed policy already failed the boot validation; an unparsable one here would
            // be a second, silent source of truth, so it reads as "nothing restricted".
            cluster_policy: Arc::new(
                crate::runs::dispatch_target::cluster_policy(cfg).unwrap_or_default(),
            ),
            pack_pr_app: cfg.github_app.clone(),
            public_url: cfg.public_url.clone(),
            vault: None,
            profile_secret_env: Arc::new(
                cfg.deploy_profile
                    .as_deref()
                    .map(crate::secrets::manifest::profile_secret_env)
                    .unwrap_or_default(),
            ),
            auth_mode: crate::identity::auth::AuthMode::from_env(),
            credential_keys: None,
            oidc: None,
            contracts,
            images_refresh: Arc::new(tokio::sync::Notify::new()),
            policy,
        }
    }

    /// Attach the issuer and the mounted credential key the offline-credential routes need. Both
    /// are built by the binary (either can fail to read its environment), so assembling the state
    /// itself stays infallible.
    pub fn with_oidc(
        mut self,
        oidc: Option<Arc<crate::identity::oidc::OidcProvider>>,
        credential_keys: Option<Arc<crate::identity::oidc::credentials::CredentialKeys>>,
    ) -> Self {
        self.oidc = oidc;
        self.credential_keys = credential_keys;
        self
    }

    /// Attach the hub's Vault client. Built by the binary (the client reads its own environment
    /// and can fail), so assembling the state itself stays infallible.
    pub fn with_vault(mut self, vault: Option<Arc<crate::secrets::vault::VaultClient>>) -> Self {
        self.vault = vault;
        self
    }

    /// Append an audit event to the controller event log. A failed append is logged and swallowed,
    /// never surfaced as a 500: the action already happened, the audit trail is best-effort.
    /// `context` names the action for the warn line (e.g. "add_repo").
    pub(crate) async fn audit(&self, event: crate::event_log::Event<'_>, context: &str) {
        if let Err(e) = self.db.events().append(&event).await {
            tracing::warn!(
                error = format!("{e:#}"),
                "{context}: audit event append failed"
            );
        }
    }

    /// Append an audit event whose durability is part of the API contract.
    pub(crate) async fn audit_required(
        &self,
        event: crate::event_log::Event<'_>,
    ) -> anyhow::Result<()> {
        self.db.events().append(&event).await
    }
}

#[cfg(test)]
impl ApiState {
    /// Baseline router state for tests: guards off, stores absent, loopback defaults. Override the
    /// few fields a test cares about with `..ApiState::test(db, sink)` struct-update syntax.
    pub(crate) fn test(db: Db, sink: Arc<dyn OverrideSink>) -> Self {
        let schedules = crate::launches::schedules::ScheduleStore::new(db.clone());
        #[cfg(feature = "autoresearch")]
        let autopilot =
            crate::daemon::autopilot_flag::AutopilotFlag::seeded(db.pool().clone(), true);
        let clusters = Arc::new(crate::runs::clusters::ClusterClients::new(None));
        Self {
            db,
            schedules,
            sink,
            queue: Arc::new(crate::daemon::queue::WorkQueue::new()),
            caps: None,
            roles: crate::identity::auth::Roles::new(vec![], vec![], vec![]),
            #[cfg(feature = "autoresearch")]
            autopilot: Some(autopilot),
            #[cfg(feature = "autoresearch")]
            autoresearch: true,
            pod_namespace: "autoresearch".to_string(),
            cluster_stats: Arc::new(crate::runs::cluster_stats::ClusterStats::new(
                clusters.clone(),
            )),
            clusters,
            control_port: 7777,
            config: None,
            #[cfg(feature = "autoresearch")]
            repo_whitelist: Arc::new(RepoWhitelist::default()),
            scratch_dir: std::path::PathBuf::new(),
            reconcile_now: Arc::new(tokio::sync::Notify::new()),
            jira: None,
            emission: None,
            broker_contracts: Default::default(),
            playbook_caps: Default::default(),
            dispatch: crate::playbooks::dispatch::DispatchCapability::new(
                crate::config::PlaybookExecutor::Pod,
                true,
            ),
            pack_pr_app: None,
            public_url: None,
            vault: None,
            profile_secret_env: Arc::new(Vec::new()),
            auth_mode: crate::identity::auth::AuthMode::Proxy,
            credential_keys: None,
            oidc: None,
            contracts: crate::runs::contract::permissive(),
            images_refresh: Arc::new(tokio::sync::Notify::new()),
            default_cluster: crate::runs::clusters::HUB_CLUSTER.to_string(),
            cluster_policy: Arc::new(Default::default()),
            policy: crate::authz::policy::ActivePolicy::default_set()
                .expect("the shipped default policy set loads"),
        }
    }
}

impl axum::extract::FromRef<ApiState> for crate::identity::auth::Roles {
    fn from_ref(state: &ApiState) -> Self {
        state.roles.clone()
    }
}

/// The error boundary: any `?`-propagated `anyhow::Error` becomes a 500 with its
/// message. Expected outcomes (not found, bad query) are returned as explicit responses instead —
/// this newtype exists only for "something we didn't plan for went wrong."
pub struct AppError(pub(crate) anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", self.0)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        AppError(err.into())
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ErrorBody {
    pub(crate) error: String,
}

impl ErrorBody {
    pub(crate) fn new(error: impl Into<String>) -> Self {
        ErrorBody {
            error: error.into(),
        }
    }
}
