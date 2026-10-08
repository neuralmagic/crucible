//! `crucible-kueue`: an MCP server through which sandboxed agents submit, inspect, and control
//! Kueue workloads. It holds the Kubernetes credentials; agents hold only a per-run bearer token
//! or an access token from the crucible realm.

pub mod auth;
pub mod hardware;
pub mod kueue;
pub mod manifest;
pub mod quantity;
pub mod records;
pub mod server;
pub mod status;
pub mod task;
pub mod tenant;

use crucible_mcp_auth::guard::{AccessReview, PolicyGuard};
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
    /// Tenant tokens: the static file plus controller-written records.
    pub registry: Arc<tenant::Registry>,
    /// Accept realm access tokens validated by this resource server.
    pub oidc: Option<Arc<ResourceServer>>,
    /// Decides every MCP request. A server that takes only tenant tokens may pass
    /// [`ToolPolicy::allow_all`]: their records already pin them.
    pub policy: ToolPolicy,
    /// Reviews the policy's authorize rules (Kubernetes RBAC). People's tokens are accepted only
    /// with a reviewer and a policy that has authorize rules.
    pub reviewer: Option<Arc<dyn AccessReview>>,
    /// Host header values to accept besides loopback, e.g. the Service DNS name.
    pub allowed_hosts: Vec<String>,
    /// The Kueue API version the cluster serves; see [`kueue::KueueVersion::discover`].
    pub kueue: kueue::KueueVersion,
}

/// The HTTP app: the MCP endpoint behind authentication and the tool policy; plus an
/// unauthenticated `/healthz` and, with OIDC on, the RFC 9728 protected-resource metadata.
pub fn router(client: kube::Client, options: Options) -> axum::Router {
    let Options {
        registry,
        oidc,
        policy,
        reviewer,
        allowed_hosts,
        kueue,
    } = options;
    let people = reviewer.is_some() && policy.authorizes();
    let auth = auth::Auth {
        registry,
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
    let task_reviewer = reviewer.clone();
    let service: StreamableHttpService<server::KueueMcp, NeverSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(server::KueueMcp::new(
                    client.clone(),
                    task_reviewer.clone(),
                    kueue,
                ))
            },
            Arc::new(NeverSessionManager::default()),
            config,
        );
    let guard = PolicyGuard {
        policy,
        methods: server::METHODS,
        server: oidc.clone(),
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
    let app = axum::Router::new()
        .nest_service(MCP_PATH, mcp)
        .route("/healthz", axum::routing::get(|| async { "ok" }));
    match oidc {
        Some(server) => app.merge(crucible_mcp_auth::metadata::router(server)),
        None => app,
    }
}
