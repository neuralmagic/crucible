//! `crucible-controller` — the outer loop's ledger and daemon core (discovery+scheduling and state model).
//!
//! This crate is the codebase's one async island: sqlx (and kube + axum) share a
//! tokio runtime here and nowhere else. The `crucible` binary's thin subcommands build a runtime
//! and `block_on` into these functions (the `publish.rs` pattern), so no other crate grows an
//! async dependency.
//!
//! The foundation is the Postgres pool setup ([`connect`]), the migration set behind one
//! shared [`MIGRATOR`], the DB layering (each slice's `store` module: raw SQL over `impl Executor` + the
//! [`Db`] domain facade), the append-only [`event_log`], and the clap/env [`ControllerCfg`].
//!
//! On top of that sits the in-memory work [`queue`] (coalescing set + FIFO, backoff,
//! park-after-N) and the [`daemon`] shell that `select!`s its sources (discovery timer, kube pod
//! watch) over it, plus the HTTP [`api`] surface, mounted together with the React SPA by
//! [`Surface::bind`].

// So `#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]` resolves inside this crate's own
// unit tests (a lib can't otherwise name itself). The migrator path is frozen by the work plan.
extern crate self as crucible_controller;

pub mod agent_auth;
pub mod api;
pub mod authz;
#[cfg(feature = "autoresearch")]
pub mod builds;
pub mod client;
pub(crate) mod clock;
pub mod config;
pub mod daemon;
pub(crate) mod dto;
#[cfg(feature = "embedded-db")]
pub mod embedded_db;
pub mod event_log;
pub mod identity;
pub mod images;
pub mod issues;
pub mod launches;
pub mod ledger;
pub mod mcp;
pub mod metrics;
pub mod model;
mod oidc_mirror;
pub mod playbooks;
pub mod runs;
pub mod secrets;
mod spa;
pub mod telemetry;
pub mod testing;
pub mod tls;
mod wire_enum;

pub use client::{
    Db, MAINTENANCE_ADVISORY_LOCK, connect, db_name, sibling_db_url, try_maintenance_lock,
};
pub use config::{ControllerCfg, GroundedExecutor, PlaybookExecutor, Profile, ScopeExecutor};
#[cfg(feature = "autoresearch")]
pub use daemon::autopilot_flag::AutopilotFlag;
pub use daemon::overrides::{OverrideStore, QueueOverrideSink};
pub use daemon::overrides_store::{ConfigStore, KubeConfigMapApi};
pub use daemon::queue::{IssueKey, Override, OverrideSink, QueueConfig, WorkQueue};
pub use daemon::rebuild::{Drift, DriftReport, rebuild, verify};
pub use event_log::Event;
pub use issues::model::{NewIssue, NewScope};
#[cfg(feature = "autoresearch")]
pub use issues::triage::{TriageSummary, triage_repo};
pub use metrics::Metrics;
pub use model::{ParkedBy, Status};
pub use runs::model::{NewCandidate, NewRun, Run};
pub use runs::workpod::{
    KubePodDispatcher, PodDispatcher, RunDocs, RunRenderOpts, WorkPodState, install_contracts,
    install_dispatcher, reconcile_on_startup, render_run_docs, reset_contracts, reset_dispatcher,
    run_pod_name, stamp_run_pod,
};

/// Choose the process-level rustls `CryptoProvider`, once. Both backends are in the dependency
/// tree, so rustls refuses to pick one on its own and panics inside the first TLS client build.
/// Idempotent: a provider already installed stays installed.
pub fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// Generate the OpenAPI spec for all `/api/*` routes as JSON. Can be called without a running server
/// for typegen workflows — the spec is compile-time generated from handler annotations.
pub fn openapi_spec() -> anyhow::Result<String> {
    api::openapi_spec()
}

use anyhow::Context;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

/// What guards the human surface and what runs its logins. Built once by [`Surface::bind`] and
/// handed to [`human_router`] so the guard and the `/auth/*` routes can never disagree about the
/// mode.
pub(crate) struct HumanAuth {
    pub(crate) guard: Arc<identity::auth::BearerGuard>,
    pub(crate) routes: identity::oidc::routes::AuthState,
    /// The issuer whose `/oauth/authorize` rides the browser session, when it serves people.
    pub(crate) issuer: Option<Arc<agent_auth::AgentAuth>>,
}

/// The human surface: API + SPA behind the auth guard, the run evidence routes and the `/auth/*`
/// login routes beside it, and the session layer over all three. Machine surfaces (`/metrics`, ingest, the discovery mirror) never
/// route through this. Shared with the API tests so the test stack cannot drift from what
/// [`Surface::bind`] mounts.
///
/// The layering is the whole point of native mode: sessions load OUTSIDE the guard, so a request
/// may authenticate by cookie, and the login routes sit outside the guard, so a caller with no
/// credential can get one.
pub(crate) fn human_router(
    state: api::state::ApiState,
    store: tower_sessions_sqlx_store::PostgresStore,
    secure_cookies: bool,
    auth: HumanAuth,
    ui: spa::Source,
) -> axum::Router {
    let link = axum::Extension(auth.guard.clone());
    let both = identity::auth::HumanOrKeyGuard {
        guard: auth.guard,
        pool: state.db.pool().clone(),
    };
    let evidence = runs::evidence_api::router(runs::evidence_api::EvidenceState {
        db: state.db.clone(),
        scratch_dir: state.scratch_dir.clone(),
        guard: both.clone(),
    });
    let router = api::router(state)
        .merge(spa::router(ui))
        .layer(axum::middleware::from_fn_with_state(
            both,
            identity::auth::require_auth_or_api_key,
        ))
        .merge(evidence)
        .merge(identity::oidc::routes::router(auth.routes));
    let router = match auth.issuer.filter(|i| i.people.is_some()) {
        Some(issuer) => router.merge(agent_auth::people::router(issuer)),
        None => router,
    };
    router
        .layer(link)
        .layer(identity::session::layer(store, secure_cookies))
}

/// The webhook delivery surface's bind address, `CONTROLLER_HOOKS_ADDR` (host:port). Unset serves
/// no delivery surface.
fn hooks_addr_from_env() -> anyhow::Result<Option<SocketAddr>> {
    match std::env::var("CONTROLLER_HOOKS_ADDR") {
        Ok(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse()
            .map(Some)
            .with_context(|| format!("CONTROLLER_HOOKS_ADDR {raw:?} is not host:port")),
        _ => Ok(None),
    }
}

/// The controller's listeners, bound, and the routers they serve.
pub struct Surface {
    http: tokio::net::TcpListener,
    router: axum::Router,
    hooks: Option<(tokio::net::TcpListener, axum::Router)>,
    tls: Option<tls::TlsListener>,
}

impl Surface {
    /// Mount the API + React SPA on one axum router and bind it at `addr`, with the webhook and
    /// TLS surfaces their environment configures. A listener that cannot bind is an error here,
    /// before anything is served. Without `CONTROLLER_API_TOKEN` set, the guard is off — so this
    /// forces the bind to loopback regardless of the requested `addr`'s host, matching the frozen
    /// "binds loopback by default" contract instead of trusting an unauthenticated port to
    /// whatever address the caller asked for.
    pub async fn bind(
        state: api::state::ApiState,
        addr: SocketAddr,
        secure_session_cookies: bool,
        agent_auth: Option<Arc<agent_auth::AgentAuth>>,
    ) -> anyhow::Result<Self> {
        let kube_user_auth = identity::kube_user::KubeUserAuth::from_env()
            .context("building the cluster-token bearer check")?;
        let oidc =
            identity::oidc::OidcProvider::from_env().context("reading the oidc registration")?;
        let credential_keys = identity::oidc::credentials::CredentialKeys::from_env()
            .context("reading the offline credential key")?;
        if oidc.is_some() && credential_keys.is_none() {
            tracing::warn!(
                "no credential key mounted: logins store no offline credential, so scheduled launches stay on the schedule-row snapshot"
            );
        }
        let webhook_keys = identity::oidc::credentials::CredentialKeys::webhook_from_env()
            .context("reading the webhook key")?;
        let pool_for_hooks = state.db.pool().clone();
        let state = state
            .with_oidc(oidc.clone(), credential_keys.clone())
            .with_webhook_keys(webhook_keys.clone());
        let auth_mode = identity::auth::AuthMode::from_env();
        let guard = Arc::new(
            identity::auth::BearerGuard::from_env(
                kube_user_auth,
                oidc.clone(),
                Some(state.db.pool().clone()),
            )
            .context("building the bearer guard")?,
        );
        if guard.kube.is_some() {
            tracing::info!(
                "cluster-token bearer auth on: unrecognized bearers resolve via the own-cluster users/~"
            );
        }
        tracing::info!(mode = ?auth_mode, "controller human-surface auth mode");
        let bind_addr = if guard.is_open() {
            tracing::warn!(
                "CONTROLLER_API_TOKEN unset — binding the controller http surface to loopback only"
            );
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
        } else {
            addr
        };

        // Cloned off the shared state before it moves into the router, for the machine surfaces
        // below.
        let db = state.db.clone();
        let cluster_stats_for_push = state.cluster_stats.clone();
        let clusters = state.clusters.clone();

        // The API + SPA sit behind the bearer guard; `/metrics` is merged in *after* the layer so
        // the shared kube-prometheus-stack can scrape it without a token (the ServiceMonitor sends
        // none), exactly as a health endpoint would be exempted. The MCP host's view of the API:
        // the same routes, with no human guard layered over them, because the key its callers hold
        // is checked at that surface's own edge. Built before the state moves into the human
        // router.
        let mcp_router = mcp::router(
            api::router(state.clone()),
            identity::auth::KeyGuard {
                pool: db.pool().clone(),
                refresh: guard.refresh.clone(),
            },
            state.public_url.clone().unwrap_or_default(),
        );

        let session_store = identity::session::store(db.pool())
            .await
            .context("building the session store")?;
        identity::session::spawn_expiry_sweep(session_store.clone());
        let guarded = human_router(
            state.clone(),
            session_store,
            secure_session_cookies,
            HumanAuth {
                guard,
                routes: identity::oidc::routes::AuthState {
                    mode: auth_mode,
                    oidc,
                    pool: db.pool().clone(),
                    credential_keys,
                    proxy_prefix: std::env::var("CONTROLLER_PROXY_PREFIX")
                        .unwrap_or_else(|_| "/oauth2".to_string()),
                },
                issuer: agent_auth.clone(),
            },
            spa::Source::from_env(),
        );

        // The in-cluster client the discovery mirror fetches its upstream through; if it can't be
        // built the mirror fails closed (503) until kube is reachable.
        let kube_client = clusters.client(runs::clusters::HUB_CLUSTER).await;

        // The OIDC discovery mirror (hub-spoke trust bootstrap): mounted OUTSIDE the bearer layer
        // like `/metrics` — public read-only by design. Disabled (routes 404) unless
        // CONTROLLER_EXTERNAL_URL names the mirror's own external base URL.
        let mirror = oidc_mirror::external_url_from_env().map(|base| {
        let upstream: Arc<dyn oidc_mirror::DiscoveryUpstream> = match kube_client.as_ref() {
            Ok(client) => Arc::new(oidc_mirror::KubeUpstream(client.clone())),
            Err(e) => {
                tracing::warn!(error = %e, "no kube client for the discovery mirror — it serves 503 until one is reachable");
                Arc::new(oidc_mirror::UnavailableUpstream)
            }
        };
        oidc_mirror::Mirror::new(upstream, base)
    });

        // The cluster-snapshot push surface: outside the bearer guard like `/metrics`,
        // authenticated by its own per-cluster static tokens (see `cluster_push`'s module doc for
        // why TokenReview can't work for unreachable spokes).
        let push_router = {
            let tokens = runs::cluster_push::PushTokens::from_env()
                .context("parsing CONTROLLER_PUSH_TOKENS")?;
            if tokens.is_empty() {
                tracing::info!("CONTROLLER_PUSH_TOKENS unset — cluster snapshot pushes answer 401");
            }
            runs::cluster_push::router(runs::cluster_push::PushState {
                stats: cluster_stats_for_push,
                tokens: Arc::new(tokens),
            })
        };

        let router = guarded
            .merge(api::metrics::router(state))
            .merge(push_router)
            .merge(mcp_router)
            .merge(oidc_mirror::router(mirror));
        // The sandbox bind, like the evidence routes, authenticates with the run's own pod token.
        let router = match agent_auth {
            Some(state) => router
                .merge(agent_auth::bindings::router(state.bind_state(db.clone())))
                .merge(agent_auth::router(state)),
            None => router,
        };

        let hooks = match hooks_addr_from_env()? {
            Some(hooks_addr) => {
                let hooks = launches::webhooks::receive::router(
                    launches::webhooks::receive::HooksState::new(
                        pool_for_hooks,
                        webhook_keys.clone(),
                    ),
                );
                let listener = tokio::net::TcpListener::bind(hooks_addr)
                    .await
                    .with_context(|| {
                        format!("binding the webhook delivery surface to {hooks_addr}")
                    })?;
                tracing::info!(%hooks_addr, "crucible-controller webhook delivery surface listening");
                Some((listener, hooks))
            }
            None => None,
        };

        let http = tokio::net::TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("binding the controller http surface to {bind_addr}"))?;
        tracing::info!(%bind_addr, "crucible-controller http surface listening");

        // The TLS surface serves the same router, for the redemption Route's reencrypt hop. It
        // follows the loopback rule above: an unauthenticated deployment exposes nothing off-host.
        let tls_front = tls::TlsFront::from_env()
            .context("reading the controller tls surface's configuration")?
            .map(|front| front.with_host(bind_addr.ip()));
        let tls = match tls_front {
            Some(front) => {
                let listener = tls::bind(&front).await?;
                tracing::info!(addr = %front.addr, "crucible-controller tls surface listening");
                Some(listener)
            }
            None => None,
        };
        Ok(Self {
            http,
            router,
            hooks,
            tls,
        })
    }

    /// Serve every bound listener. Each one serves for the life of the process, so this resolves
    /// only when one of them stops, to the error naming it.
    pub async fn serve(self) -> SurfaceStopped {
        let Self {
            http,
            router,
            hooks,
            tls,
        } = self;
        let hooks = async move {
            match hooks {
                Some((listener, hooks)) => stopped(
                    "webhook delivery surface",
                    axum::serve(listener, hooks).await,
                ),
                None => std::future::pending().await,
            }
        };
        let tls_router = router.clone();
        let tls = async move {
            match tls {
                Some(listener) => stopped("tls surface", axum::serve(listener, tls_router).await),
                None => std::future::pending().await,
            }
        };
        let http = async move {
            let peers = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
            stopped("http surface", axum::serve(http, peers).await)
        };
        tokio::select! {
            e = http => e,
            e = hooks => e,
            e = tls => e,
        }
    }
}

/// Why [`Surface::serve`] resolved: one of its listeners stopped serving.
#[derive(Debug, thiserror::Error)]
pub enum SurfaceStopped {
    #[error("the controller {surface} stopped serving")]
    Returned { surface: &'static str },
    #[error("serving the controller {surface}")]
    Failed {
        surface: &'static str,
        #[source]
        source: std::io::Error,
    },
}

fn stopped(surface: &'static str, served: std::io::Result<()>) -> SurfaceStopped {
    match served {
        Ok(()) => SurfaceStopped::Returned { surface },
        Err(source) => SurfaceStopped::Failed { surface, source },
    }
}

/// The one migration set, referenced by every `#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]`.
/// One shared static MIGRATOR avoids re-expanding the migration set's proc-macro at every call
/// site, which is slow and easy to get subtly wrong across duplicates. Never re-run
/// `sqlx::migrate!()` elsewhere.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// The applied schema version (the highest migration), for `crucible-controller db verify`.
pub async fn schema_version(pool: &sqlx::PgPool) -> anyhow::Result<i64> {
    client::schema_version(pool).await
}

/// THE one crate-wide lock for tests that mutate any process-global env var (`CRUCIBLE_BIN`,
/// `GITHUB_API_URL`, …). The environ is a single global, so per-module or per-variable locks
/// wouldn't serialize the tests that race through it. An async mutex: the guard is held across the
/// test's own `.await`s by design, which an async-aware mutex is meant for (a `std::sync::Mutex`
/// guard held that long is exactly what `clippy::await_holding_lock` warns about); the rare sync
/// test takes it via `blocking_lock()` (fine — sync tests run outside any tokio runtime).
#[cfg(test)]
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A unique ledger URL on the test server (`DATABASE_URL`), for the tests that open a second
/// "live" database beside their `#[sqlx::test]` pool (rebuild/verify, autopilot). The caller
/// owns cleanup: `sqlx::Postgres::drop_database` it (best-effort) before returning.
#[cfg(test)]
pub(crate) fn test_ledger_url() -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must point at the test server");
    client::sibling_db_url(
        &base,
        &format!("crucible_test_{}", uuid::Uuid::new_v4().simple()),
    )
    .expect("deriving a test ledger URL")
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use sqlx::PgPool;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct NoSink;
    impl crate::daemon::queue::OverrideSink for NoSink {
        fn submit(&self, _ov: crate::daemon::queue::Override) {}
    }

    async fn answer(router: &axum::Router, method: Method, uri: &str) -> (StatusCode, Vec<u8>) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, body.to_vec())
    }

    /// A controller started as the binary starts it, with a deploy profile but none of the agent
    /// configuration, serves none of the agent routes (each answers as an unrouted path does),
    /// runs none of their background sources, and tells the SPA every agent feature is off.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_controller_without_agent_config_serves_and_runs_none_of_it(pool: PgPool) {
        let _env = crate::ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::testing::cfg_from_args(["ctl"]);
        cfg.deploy_profile = Some(crate::testing::fixtures::write_deploy_profile(dir.path()));
        assert!(!cfg.issues_agent_tokens());
        let db = crate::Db::new(pool.clone());
        assert!(
            crate::daemon::agent_sources(&db, &cfg).await.is_empty(),
            "no gateway sweep without isolated gateways"
        );

        let clusters = Arc::new(crate::runs::clusters::ClusterClients::new(None));
        let agent_auth = crate::agent_auth::AgentAuth::from_env(
            pool.clone(),
            &cfg,
            clusters.clone(),
            crate::identity::people_directory::people,
        )
        .await
        .unwrap();
        assert!(agent_auth.is_none(), "no issuer without its configuration");
        let links = crate::identity::cluster_link::ClusterLinks::from_env(&cfg, clusters.clone())
            .await
            .unwrap();
        assert!(
            links.is_none(),
            "no cluster links without a [cluster_link] table"
        );
        let state = crate::api::state::ApiState::new(
            db.clone(),
            Arc::new(NoSink),
            Arc::new(crate::daemon::queue::WorkQueue::new()),
            clusters,
            None,
            Arc::new(tokio::sync::Notify::new()),
            crate::runs::contract::permissive(),
            crate::authz::policy::ActivePolicy::default_set().unwrap(),
            &cfg,
        )
        .with_cluster_links(links.map(Arc::new))
        .with_people_issuer(agent_auth.clone());
        let surface =
            crate::Surface::bind(state, "127.0.0.1:0".parse().unwrap(), false, agent_auth)
                .await
                .unwrap();
        let router = &surface.router;

        let routes = [
            (Method::POST, "/oauth/token"),
            (Method::GET, "/.well-known/jwks.json"),
            (Method::GET, "/.well-known/oauth-authorization-server"),
            (Method::GET, "/oauth/authorize"),
            (Method::POST, "/api/sandbox-bindings/runs/run-1"),
            (Method::GET, "/oauth/cluster-link/hub/callback"),
            (Method::GET, "/api/me/kube-user"),
            (Method::PUT, "/api/users/alice/kube-user"),
            (Method::GET, "/api/me/cluster-identities"),
            (Method::PUT, "/api/me/cluster-identities/hub"),
            (Method::DELETE, "/api/me/cluster-identities/hub"),
            (Method::POST, "/api/me/cluster-identities/hub/link"),
            (Method::GET, "/api/me/mcp-sign-ins"),
            (Method::DELETE, "/api/me/mcp-sign-ins"),
            (Method::GET, "/api/oauth/consent"),
            (Method::POST, "/api/oauth/consent"),
            (Method::GET, "/api/agent-tools"),
            (Method::GET, "/api/agent-clusters"),
            (Method::GET, "/api/prefs/notices"),
            (Method::PUT, "/api/prefs/notices"),
        ];
        for (method, path) in &routes {
            let unrouted = match path.starts_with("/api/") {
                true => "/api/no-such-route",
                false => "/no-such-route",
            };
            assert_eq!(
                answer(router, method.clone(), path).await,
                answer(router, method.clone(), unrouted).await,
                "{method} {path} is served"
            );
        }

        let (status, body) = answer(router, Method::GET, "/api/whoami").await;
        assert_eq!(status, StatusCode::OK);
        let whoami: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            whoami["features"],
            serde_json::json!({"agent_tools": false, "identities": false})
        );
        let (_, spec) = answer(router, Method::GET, "/api/openapi.json").await;
        let spec: serde_json::Value = serde_json::from_slice(&spec).unwrap();
        for (_, path) in &routes {
            let template = path
                .replace("/run-1", "/{run_id}")
                .replace("/alice/", "/{login}/")
                .replace("/hub", "/{cluster}");
            assert!(
                spec["paths"].get(&template).is_none(),
                "the served spec documents {template}"
            );
        }
    }
}
