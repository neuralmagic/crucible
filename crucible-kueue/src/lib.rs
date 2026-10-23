//! `crucible-kueue`: an MCP server through which sandboxed agents submit, inspect, and control
//! Kueue workloads. It holds the Kubernetes credentials; callers hold only an access token, a
//! run's from the crucible controller or a person's from the crucible realm.

pub mod access;
pub mod auth;
pub mod caller;
pub mod hardware;
pub mod kueue;
pub mod manifest;
pub mod quantity;
pub mod server;
pub mod status;
pub mod task;

use crucible_mcp_auth::access::AccessReview;
use crucible_mcp_auth::guard::PolicyGuard;
use crucible_mcp_auth::policy::ToolPolicy;
use crucible_mcp_auth::server::ResourceServer;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use std::sync::Arc;

pub const MCP_PATH: &str = "/mcp";

/// Selects aws-lc-rs as the process TLS provider. Several rustls backends are linked in, so rustls
/// refuses to pick one; call this before building the first kube client.
pub fn install_tls_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Everything a crucible-kueue app is built from. The binary fills it from flags; a host that
/// embeds the server fills it in code.
pub struct Options {
    /// Validates every access token: runs' from the controller, people's from the realm.
    pub oidc: Arc<ResourceServer>,
    /// Mutate and validate rules for tool arguments; [`ToolPolicy::default`] has none. Which
    /// calls exist, their scopes and their access checks are [`access::KueueRules`].
    pub policy: ToolPolicy,
    /// Reviews access checks (Kubernetes RBAC) for callers carrying a Kubernetes user. People's
    /// tokens are accepted only with a reviewer.
    pub reviewer: Option<Arc<dyn AccessReview>>,
    /// Host header values to accept besides loopback, e.g. the Service DNS name.
    pub allowed_hosts: Vec<String>,
    /// The Kueue API version the cluster serves; see [`kueue::KueueVersion::discover`].
    pub kueue: kueue::KueueVersion,
    /// The image pull secrets a workload's pods may name; none when empty.
    pub pull_secrets: Vec<String>,
}

/// The HTTP app: the MCP endpoint behind authentication and the guard; plus an
/// unauthenticated `/healthz` and the RFC 9728 protected-resource metadata.
pub fn router(client: kube::Client, options: Options) -> axum::Router {
    let Options {
        oidc,
        policy,
        reviewer,
        allowed_hosts,
        kueue,
        pull_secrets,
    } = options;
    let pull_secrets: Arc<[String]> = pull_secrets.into();
    let people = reviewer.is_some();
    let auth = auth::Auth {
        oidc: oidc.clone(),
        people,
    };
    let extra_hosts = &allowed_hosts;
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    for host in extra_hosts {
        if !config.allowed_hosts.contains(host) {
            config.allowed_hosts.push(host.clone());
        }
    }
    let service: StreamableHttpService<server::KueueMcp, NeverSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(server::KueueMcp::new(
                    client.clone(),
                    kueue,
                    pull_secrets.clone(),
                ))
            },
            Arc::new(NeverSessionManager::default()),
            config,
        );
    let guard = PolicyGuard {
        rules: Arc::new(access::KueueRules),
        policy,
        server: Some(oidc.clone()),
        reviewer,
    };
    let mcp = axum::Router::new()
        .fallback_service(service)
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(guard),
            crucible_mcp_auth::guard::enforce,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(auth),
            auth::authenticate,
        ));
    axum::Router::new()
        .nest_service(MCP_PATH, mcp)
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(crucible_mcp_auth::metadata::router(oidc))
}
