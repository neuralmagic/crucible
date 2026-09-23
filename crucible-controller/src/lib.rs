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
//! [`serve`].

// So `#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]` resolves inside this crate's own
// unit tests (a lib can't otherwise name itself). The migrator path is frozen by the work plan.
extern crate self as crucible_controller;

pub mod api;
pub mod authz;
#[cfg(feature = "autoresearch")]
pub mod builds;
pub mod client;
pub(crate) mod clock;
pub mod config;
pub mod daemon;
pub(crate) mod dto;
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
    KubePodDispatcher, PodDispatcher, RunRenderOpts, WorkPodState, install_contracts,
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

/// What guards the human surface and what runs its logins. Built once by [`serve`] and handed to
/// [`human_router`] so the guard and the `/auth/*` routes can never disagree about the mode.
pub(crate) struct HumanAuth {
    pub(crate) guard: Arc<identity::auth::BearerGuard>,
    pub(crate) routes: identity::oidc::routes::AuthState,
}

/// The human surface: API + SPA behind the auth guard, the `/auth/*` login routes beside it, and
/// the session layer over both. Machine surfaces (`/metrics`, ingest, the discovery mirror) never
/// route through this. Shared with the API tests so the test stack cannot drift from what `serve`
/// mounts.
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
    let both = identity::auth::HumanOrKeyGuard {
        guard: auth.guard,
        pool: state.db.pool().clone(),
    };
    api::router(state)
        .merge(spa::router(ui))
        .layer(axum::middleware::from_fn_with_state(
            both,
            identity::auth::require_auth_or_api_key,
        ))
        .merge(identity::oidc::routes::router(auth.routes))
        .layer(identity::session::layer(store, secure_cookies))
}

/// Mount the API + React SPA on one axum router and serve it at `addr`. The daemon owns calling
/// this; it does not wire itself into the daemon. Without `CONTROLLER_API_TOKEN` set, the guard is
/// off — so this forces the bind to loopback regardless of the requested `addr`'s host, matching
/// the frozen "binds loopback by default" contract instead of trusting an unauthenticated port to
/// whatever address the caller asked for.
pub async fn serve(
    state: api::state::ApiState,
    addr: SocketAddr,
    turn_accounts: config::TurnAccounts,
    secure_session_cookies: bool,
) -> anyhow::Result<()> {
    let kube_user_auth = identity::kube_user::KubeUserAuth::from_env()
        .context("building the cluster-token bearer check")?;
    let oidc = identity::oidc::OidcProvider::from_env().context("reading the oidc registration")?;
    let credential_keys = identity::oidc::credentials::CredentialKeys::from_env()
        .context("reading the offline credential key")?;
    if oidc.is_some() && credential_keys.is_none() {
        tracing::warn!(
            "no credential key mounted: logins store no offline credential, so scheduled launches stay on the schedule-row snapshot"
        );
    }
    let state = state.with_oidc(oidc.clone(), credential_keys.clone());
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

    // Cloned off the shared state before it moves into the router, for the separate ingest surface below.
    let db = state.db.clone();
    let cluster_stats_for_push = state.cluster_stats.clone();
    let clusters = state.clusters.clone();
    let pod_namespace_for_ingest = state.pod_namespace.clone();

    // The API + SPA sit behind the bearer guard; `/metrics` is merged in *after* the layer so the
    // shared kube-prometheus-stack can scrape it without a token (the ServiceMonitor sends none),
    // exactly as a health endpoint would be exempted.
    // The MCP host's view of the API: the same routes, with no human guard layered over them,
    // because the key its callers hold is checked at that surface's own edge. Built before the
    // state moves into the human router.
    let mcp_router = mcp::router(
        api::router(state.clone()),
        db.pool().clone(),
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
        },
        spa::Source::from_env(),
    );

    // One in-cluster client shared by the two non-bearer machine surfaces below (the ingest
    // drop-box's TokenReview and the discovery mirror's upstream fetch); if it can't be built
    // both fail closed (503) until kube is reachable.
    let kube_client = clusters.client(runs::clusters::HUB_CLUSTER).await;

    // The Tier 2 ingest drop-box (turn result contract): merged in *after* the human bearer-guard layer,
    // exactly like `/metrics`, because it authenticates with its OWN pod-bound TokenReview credential,
    // not the oauth2-proxy/admin token. The validator is cluster-keyed: the uploading pod's ledger row
    // selects which cluster's API server reviews its token. A cluster whose client cannot be built
    // fails that POST closed (503); the rest of the surface stays up.
    if let Err(e) = kube_client.as_ref() {
        tracing::warn!(error = %e, "no hub kube client at startup — hub ingest TokenReviews fail closed until one is reachable");
    }
    let validator = Arc::new(runs::ingest_auth::IngestValidator::new(
        Arc::new(runs::ingest_auth::ClusterKube::new(
            clusters.clone(),
            pod_namespace_for_ingest.clone(),
        )),
        db.pool().clone(),
        crucible_contract::INGEST_TOKEN_AUDIENCE,
        runs::ingest_auth::ExpectedServiceAccount {
            namespace: pod_namespace_for_ingest,
            name: turn_accounts.hub,
        },
        turn_accounts.spokes,
    ));
    let ingest_router = runs::ingest_drop::router(runs::ingest_drop::IngestState { db, validator });

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

    // The cluster-snapshot push surface: outside the bearer guard like the ingest drop-box,
    // authenticated by its own per-cluster static tokens (see `cluster_push`'s module doc for
    // why TokenReview can't work for unreachable spokes).
    let push_router = {
        let tokens =
            runs::cluster_push::PushTokens::from_env().context("parsing CONTROLLER_PUSH_TOKENS")?;
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
        .merge(ingest_router)
        .merge(push_router)
        .merge(mcp_router)
        .merge(oidc_mirror::router(mirror));

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("binding the controller http surface to {bind_addr}"))?;
    tracing::info!(%bind_addr, "crucible-controller http surface listening");
    let http = {
        let router = router.clone();
        async move {
            axum::serve(listener, router)
                .await
                .context("serving the controller http surface")
        }
    };

    // The TLS surface serves the same router, for the redemption Route's reencrypt hop. It follows
    // the loopback rule above: an unauthenticated deployment exposes nothing off-host.
    let tls_front = tls::TlsFront::from_env()
        .context("reading the controller tls surface's configuration")?
        .map(|front| front.with_host(bind_addr.ip()));
    let Some(front) = tls_front else {
        http.await?;
        return Ok(());
    };
    let tls_listener = tls::bind(&front).await?;
    tracing::info!(addr = %front.addr, "crucible-controller tls surface listening");
    let tls = async move {
        axum::serve(tls_listener, router)
            .await
            .context("serving the controller tls surface")
    };
    tokio::try_join!(http, tls)?;
    Ok(())
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
