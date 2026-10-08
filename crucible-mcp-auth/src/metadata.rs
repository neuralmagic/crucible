//! RFC 9728 OAuth 2.0 Protected Resource Metadata.
//!
//! Served at the root well-known URI and at the path-inserted form
//! (`/.well-known/oauth-protected-resource/mcp` for resource `https://host/mcp`), which is the
//! one the 401 challenge names.

use crate::server::ResourceServer;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, StatusCode};
use serde::Serialize;
use std::sync::Arc;
use url::Url;

pub const WELL_KNOWN: &str = "/.well-known/oauth-protected-resource";

#[derive(Debug, Serialize, PartialEq)]
pub struct ProtectedResourceMetadata {
    pub resource: Url,
    pub authorization_servers: Vec<Url>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scopes_supported: Vec<String>,
    pub bearer_methods_supported: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_name: Option<String>,
}

impl ResourceServer {
    pub fn metadata(&self, resource: &Url) -> ProtectedResourceMetadata {
        let config = self.config();
        ProtectedResourceMetadata {
            resource: resource.clone(),
            authorization_servers: vec![config.issuer.clone()],
            scopes_supported: config.scopes_supported.clone(),
            bearer_methods_supported: vec!["header"],
            resource_name: config.resource_name.clone(),
        }
    }
}

/// Routes for both well-known forms. Merge into the app outside any auth layer.
pub fn router(server: Arc<ResourceServer>) -> axum::Router {
    axum::Router::new()
        .route(WELL_KNOWN, axum::routing::get(root))
        .route(
            &format!("{WELL_KNOWN}/{{*path}}"),
            axum::routing::get(inserted),
        )
        .with_state(server)
}

async fn root(State(server): State<Arc<ResourceServer>>, headers: HeaderMap) -> Response {
    match server.resource_for(&headers) {
        Some(resource) => axum::Json(server.metadata(resource)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The path-inserted form: the resource whose path is `/{path}`, preferring the one whose
/// authority matches `Host`.
async fn inserted(
    State(server): State<Arc<ResourceServer>>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    let want = format!("/{}", path.trim_end_matches('/'));
    let by_host = server
        .resource_for(&headers)
        .filter(|r| r.path().trim_end_matches('/') == want);
    let any = || {
        server
            .config()
            .resources
            .iter()
            .find(|r| r.path().trim_end_matches('/') == want)
    };
    match by_host.or_else(any) {
        Some(resource) => axum::Json(server.metadata(resource)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
