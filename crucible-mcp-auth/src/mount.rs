//! The axum app an MCP server is served as: rmcp's streamable-http service behind the host's
//! authentication and the [`PolicyGuard`], nested at one path, with the RFC 9728 document beside
//! it when the host names a resource server.

use crate::guard::{self, PolicyGuard};
use crate::metadata;
use crate::server::ResourceServer;
use axum::Router;
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::Route;
use rmcp::ServerHandler;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use std::convert::Infallible;
use std::sync::Arc;
use tower::{Layer, Service};

/// The transport config a stateless server mounts with: no sessions, JSON replies, and
/// `extra_hosts` accepted beside loopback.
pub fn stateless_config(extra_hosts: &[String]) -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    for host in extra_hosts
        .iter()
        .map(|h| h.trim())
        .filter(|h| !h.is_empty())
    {
        if !config.allowed_hosts.iter().any(|h| h == host) {
            config.allowed_hosts.push(host.to_string());
        }
    }
    config
}

pub struct Mount {
    /// The path the endpoint is nested at.
    pub path: &'static str,
    pub config: StreamableHttpServerConfig,
    /// Decides every request after authentication.
    pub guard: PolicyGuard,
    /// When set, the protected-resource metadata is served beside the endpoint.
    pub metadata: Option<Arc<ResourceServer>>,
}

/// The app for `handler`'s server: `authenticate` runs first and must leave a
/// [`crate::principal::Principal`] in the request extensions for the guard.
pub fn mcp_router<H, L>(
    handler: impl Fn() -> Result<H, std::io::Error> + Send + Sync + 'static,
    authenticate: L,
    mount: Mount,
) -> Router
where
    H: ServerHandler + Send + 'static,
    L: Layer<Route> + Clone + Send + Sync + 'static,
    L::Service: Service<Request> + Clone + Send + Sync + 'static,
    <L::Service as Service<Request>>::Response: IntoResponse + 'static,
    <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
    <L::Service as Service<Request>>::Future: Send + 'static,
{
    let service: StreamableHttpService<H, NeverSessionManager> = StreamableHttpService::new(
        handler,
        Arc::new(NeverSessionManager::default()),
        mount.config,
    );
    let endpoint = Router::new()
        .fallback_service(service)
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(mount.guard),
            guard::enforce,
        ))
        .layer(authenticate);
    let app = Router::new().nest_service(mount.path, endpoint);
    match mount.metadata {
        Some(server) => app.merge(metadata::router(server)),
        None => app,
    }
}

#[cfg(test)]
mod tests {
    use crate::mount::stateless_config;

    #[test]
    fn a_stateless_config_keeps_loopback_and_adds_each_extra_host_once() {
        let config = stateless_config(&[
            " kueue.crucible-system.svc ".into(),
            "kueue.crucible-system.svc".into(),
            "".into(),
            "localhost".into(),
        ]);
        assert!(!config.legacy_session_mode);
        assert!(config.json_response);
        let defaults =
            rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
                .allowed_hosts;
        assert!(
            config.allowed_hosts.starts_with(&defaults),
            "{:?}",
            config.allowed_hosts
        );
        assert_eq!(
            config.allowed_hosts[defaults.len()..],
            ["kueue.crucible-system.svc".to_string()]
        );
    }
}
