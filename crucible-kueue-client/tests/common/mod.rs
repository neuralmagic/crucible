//! A crucible-kueue served on loopback with a kube client whose API server never answers, and a
//! signer for task tokens it accepts.

use crucible_mcp_auth::access::{AccessCheck, AccessReview, ReviewFuture};
use crucible_mcp_auth::policy::ToolPolicy;
use crucible_mcp_auth::principal::Principal;
use crucible_mcp_auth::server::ResourceServer;
use crucible_mcp_auth::testing::{self, JwksServer};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Allows every check in team-e2e, standing in for the run's RBAC.
struct TeamE2e;

impl AccessReview for TeamE2e {
    fn review<'a>(&'a self, _: &'a Principal, check: &'a AccessCheck) -> ReviewFuture<'a> {
        let allowed = check.namespace == "team-e2e";
        Box::pin(async move { Ok(allowed) })
    }
}

/// Serve crucible-kueue on the current runtime and return its MCP URL.
pub async fn serve() -> String {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let server = ResourceServer::with_jwks(testing::config(jwks.url.clone()), |t| {
        crucible_mcp_auth::jwks::Jwks::new(reqwest::Client::new(), t.jwks_uri.clone())
            .with_timing(Duration::from_secs(300), Duration::from_secs(1))
    });
    crucible_kueue::install_tls_provider();
    let kube =
        kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().unwrap())).unwrap();
    let app = crucible_kueue::router(
        kube,
        crucible_kueue::Options {
            oidc: Arc::new(server),
            policy: ToolPolicy::from_toml(include_str!(
                "../../../crucible-kueue/deploy/kueue-policy.toml"
            ))
            .unwrap(),
            reviewer: Some(Arc::new(TeamE2e) as Arc<dyn AccessReview>),
            kueue: crucible_kueue::kueue::KueueVersion::default(),
            allowed_hosts: Vec::new(),
            kinds: crucible_kueue::manifest::WorkloadKind::ALL.to_vec(),
            config: crucible_kueue::config::Config {
                namespaces: vec!["team-e2e".into(), "team-b".into()],
                ..crucible_kueue::config::Config::default()
            },
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _jwks = jwks;
        axum::serve(listener, app).await
    });
    format!("http://{addr}{}", crucible_kueue::MCP_PATH)
}

/// A task token for the run in team-e2e, as the controller mints one.
pub fn task_token() -> String {
    let mut claims = testing::run_claims();
    claims["kube_user"] = json!("crucible:run:crucible-run-brave-otter");
    claims["act"] = json!({"sub": "crucible:run:crucible-run-brave-otter:task:probe"});
    testing::sign("a", &claims)
}
