//! A cluster SPIRE's JWT authorities, read from its OIDC discovery provider through the cluster's
//! API server Service proxy with the controller's own client for that cluster.

use crate::agent_auth::cluster_spire::ClusterSpire;
use crate::runs::clusters::ClusterClients;
use crucible_mcp_auth::jwks::{FetchFuture, KeySource, KeySourceError};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use std::sync::Arc;
use std::time::Duration;

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
enum ProxyError {
    #[error("the API server answered {0}")]
    Status(http::StatusCode),
    #[error("no answer within {}s", FETCH_TIMEOUT.as_secs())]
    Timeout,
}

pub struct ProxyKeys {
    clusters: Arc<ClusterClients>,
    cluster: String,
    path: String,
}

impl ProxyKeys {
    pub fn new(clusters: Arc<ClusterClients>, spire: &ClusterSpire) -> Self {
        Self {
            clusters,
            cluster: spire.cluster.clone(),
            path: spire.discovery.keys_path(),
        }
    }

    async fn read(&self, limit: usize) -> Result<Vec<u8>, KeySourceError> {
        let fetch = |e: Box<dyn std::error::Error + Send + Sync>| KeySourceError::Fetch(e);
        let client = self
            .clusters
            .client(&self.cluster)
            .await
            .map_err(|e| fetch(e.into()))?;
        let request = http::Request::get(&self.path)
            .body(kube::client::Body::from(Vec::new()))
            .map_err(|e| fetch(e.into()))?;
        let response = tokio::time::timeout(FETCH_TIMEOUT, client.send(request))
            .await
            .map_err(|_| fetch(ProxyError::Timeout.into()))?
            .map_err(|e| fetch(e.into()))?;
        if !response.status().is_success() {
            return Err(fetch(ProxyError::Status(response.status()).into()));
        }
        let body = tokio::time::timeout(
            FETCH_TIMEOUT,
            Limited::new(response.into_body(), limit).collect(),
        )
        .await
        .map_err(|_| fetch(ProxyError::Timeout.into()))?
        .map_err(|e| match e.is::<LengthLimitError>() {
            true => KeySourceError::TooLarge { limit },
            false => fetch(e),
        })?;
        Ok(body.to_bytes().to_vec())
    }
}

impl KeySource for ProxyKeys {
    fn fetch(&self, limit: usize) -> FetchFuture<'_> {
        Box::pin(self.read(limit))
    }

    fn describe(&self) -> String {
        format!("{} on cluster {}", self.path, self.cluster)
    }
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::cluster_spire::ClusterSpire;
    use crate::agent_auth::spire::*;
    use axum::http::StatusCode;

    /// A cluster whose API server is a loopback listener answering `path` with `status` and
    /// `body`, and 404 elsewhere.
    async fn cluster(
        dir: &std::path::Path,
        path: &'static str,
        status: StatusCode,
        body: Vec<u8>,
    ) -> Arc<ClusterClients> {
        crate::install_crypto_provider();
        let app = axum::Router::new().route(
            path,
            axum::routing::get(move || {
                let body = body.clone();
                async move { (status, body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let spoke = dir.join("spoke");
        std::fs::create_dir_all(&spoke).unwrap();
        std::fs::write(
            spoke.join("kubeconfig"),
            format!(
                "apiVersion: v1\nkind: Config\ncurrent-context: c\nclusters:\n- name: c\n  \
                 cluster:\n    server: http://{addr}\ncontexts:\n- name: c\n  context:\n    \
                 cluster: c\n    user: u\nusers:\n- name: u\n  user: {{}}\n"
            ),
        )
        .unwrap();
        Arc::new(ClusterClients::new(Some(dir.to_path_buf())))
    }

    const KEYS: &str = "/api/v1/namespaces/spire/services/https:oidc-discovery:443/proxy/keys";

    fn spire() -> ClusterSpire {
        "spoke=https://oidc.spoke.example spiffe://spoke.example/openshell/gateway/ \
         spire/oidc-discovery:443"
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn the_keys_come_from_the_discovery_service_through_the_api_server_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let clusters = cluster(dir.path(), KEYS, StatusCode::OK, b"{\"keys\":[]}".to_vec()).await;
        let keys = ProxyKeys::new(clusters, &spire());
        assert_eq!(keys.fetch(1024).await.unwrap(), b"{\"keys\":[]}");
        assert_eq!(keys.describe(), format!("{KEYS} on cluster spoke"));
    }

    #[tokio::test]
    async fn a_refusal_an_oversized_set_and_an_unknown_cluster_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let clusters = cluster(dir.path(), KEYS, StatusCode::FORBIDDEN, Vec::new()).await;
        let refused = ProxyKeys::new(clusters, &spire()).fetch(1024).await;
        assert!(
            matches!(&refused, Err(KeySourceError::Fetch(e)) if e.to_string().contains("403")),
            "{refused:?}"
        );

        let dir = tempfile::tempdir().unwrap();
        let clusters = cluster(dir.path(), KEYS, StatusCode::OK, vec![b' '; 2048]).await;
        let big = ProxyKeys::new(clusters, &spire()).fetch(1024).await;
        assert!(
            matches!(big, Err(KeySourceError::TooLarge { limit: 1024 })),
            "{big:?}"
        );

        let dir = tempfile::tempdir().unwrap();
        let clusters = cluster(dir.path(), KEYS, StatusCode::OK, Vec::new()).await;
        let mut elsewhere = spire();
        elsewhere.cluster = "gone".into();
        let unknown = ProxyKeys::new(clusters, &elsewhere).fetch(1024).await;
        assert!(
            matches!(unknown, Err(KeySourceError::Fetch(_))),
            "{unknown:?}"
        );
    }
}
