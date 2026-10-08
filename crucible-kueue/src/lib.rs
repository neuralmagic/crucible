//! `crucible-kueue`: an MCP server through which sandboxed agents submit, inspect, and control
//! Kueue workloads. It holds the Kubernetes credentials; agents hold only a per-run bearer token.

pub mod hardware;
pub mod kueue;
pub mod manifest;
pub mod quantity;
pub mod records;
pub mod server;
pub mod status;
pub mod task;
pub mod tenant;

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

/// The HTTP app: the MCP endpoint behind the bearer-token guard, plus an unauthenticated `/healthz`.
pub fn router(
    client: kube::Client,
    registry: Arc<tenant::Registry>,
    extra_hosts: &[String],
) -> axum::Router {
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
            move || Ok(server::KueueMcp::new(client.clone())),
            Arc::new(NeverSessionManager::default()),
            config,
        );
    axum::Router::new()
        .nest_service(
            MCP_PATH,
            axum::Router::new().fallback_service(service).layer(
                axum::middleware::from_fn_with_state(registry, tenant::require_caller),
            ),
        )
        .route("/healthz", axum::routing::get(|| async { "ok" }))
}
