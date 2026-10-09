//! The tasks extension's methods over HTTP, against the router a deployment serves, with a kube
//! client whose API server never answers: a request the server refuses fails with its refusal,
//! one it lets through fails only when it reaches Kubernetes.

use crucible_contract::kueue::token_digest;
use crucible_kueue::tenant::{Registry, Tenants};
use crucible_mcp_auth::policy::ToolPolicy;
use crucible_mcp_auth::principal::Callers;
use crucible_mcp_auth::server::{ResourceServer, TrustedIssuer};
use crucible_mcp_auth::testing::{self, JwksServer};
use rmcp::model::{
    CancelTaskParams, ClientCapabilities, ClientConfig, GetTaskParams, Implementation,
    ProtocolVersion,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

type Client = RunningService<RoleClient, ClientConfig>;

const PEOPLE: &str = "https://people.example/realms/crucible";
const VIEWER_TOKEN: &str = "task-auth-viewer";
const OWN_TASK: &str = "team-e2e/Job/train/0b6f2c1e";
const FOREIGN_TASK: &str = "team-b/Job/train/0b6f2c1e";

fn unreachable_kube() -> kube::Client {
    crucible_kueue::install_tls_provider();
    let config = kube::Config::new("http://127.0.0.1:1".parse().unwrap());
    kube::Client::try_from(config).unwrap()
}

fn registry() -> Arc<Registry> {
    let tenants = Tenants::from_toml(&format!(
        "[[tenant]]\nrun_id = \"viewer-run\"\ntoken_sha256 = \"{}\"\nnamespace = \"team-e2e\"\nrole = \"viewer\"\n",
        token_digest(VIEWER_TOKEN)
    ))
    .unwrap();
    Arc::new(Registry::new(tenants))
}

async fn serve(jwks: &JwksServer, policy: ToolPolicy) -> String {
    let mut config = testing::config(jwks.url.clone());
    config.issuers.push(TrustedIssuer {
        issuer: PEOPLE.into(),
        jwks_uri: jwks.url.clone(),
        callers: Callers::People,
    });
    let server = ResourceServer::with_jwks(config, |t| {
        crucible_mcp_auth::jwks::Jwks::new(reqwest::Client::new(), t.jwks_uri.clone())
            .with_timing(Duration::from_secs(300), Duration::from_secs(1))
    });
    let app = crucible_kueue::router(
        unreachable_kube(),
        crucible_kueue::Options {
            registry: registry(),
            oidc: Some(Arc::new(server)),
            policy,
            reviewer: None,
            kueue: crucible_kueue::kueue::KueueVersion::default(),
            allowed_hosts: Vec::new(),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}{}", crucible_kueue::MCP_PATH)
}

fn shipped_policy() -> ToolPolicy {
    ToolPolicy::from_toml(include_str!("../deploy/kueue-policy.toml")).unwrap()
}

async fn connect(url: &str, token: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
    );
    ClientConfig::new(
        ClientCapabilities::builder().enable_tasks().build(),
        Implementation::from_build_env(),
    )
    .serve_with_lifecycle(
        transport,
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    )
    .await
    .unwrap()
}

fn run_token(edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> String {
    let mut claims = testing::run_claims();
    if let Value::Object(map) = &mut claims {
        edit(map);
    }
    testing::sign("a", &claims)
}

async fn get(client: &Client, id: &str) -> String {
    client
        .get_task(GetTaskParams::new(id))
        .await
        .expect_err("no task can be read without the API server")
        .to_string()
}

async fn cancel(client: &Client, id: &str) -> String {
    client
        .cancel_task(CancelTaskParams::new(id))
        .await
        .expect_err("no task can be cancelled without the API server")
        .to_string()
}

fn reached_kubernetes(error: &str) -> bool {
    error.contains("-32603")
}

#[tokio::test]
async fn a_run_reaches_its_own_tasks_and_no_other_namespaces() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let url = serve(&jwks, shipped_policy()).await;
    let client = connect(&url, &run_token(|_| {})).await;

    let own = get(&client, OWN_TASK).await;
    assert!(reached_kubernetes(&own), "{own}");
    let own = cancel(&client, OWN_TASK).await;
    assert!(reached_kubernetes(&own), "{own}");

    let foreign = get(&client, FOREIGN_TASK).await;
    assert!(foreign.contains("bound to namespace"), "{foreign}");
    let foreign = cancel(&client, FOREIGN_TASK).await;
    assert!(foreign.contains("bound to namespace"), "{foreign}");
}

#[tokio::test]
async fn a_read_only_token_cannot_cancel_under_the_policy_or_without_one() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let read_only = run_token(|c| {
        c.insert("scope".into(), json!("kueue.read"));
    });

    let url = serve(&jwks, shipped_policy()).await;
    let client = connect(&url, &read_only).await;
    let refused = cancel(&client, OWN_TASK).await;
    assert!(
        refused.contains("Insufficient scope"),
        "the policy wants kueue.submit: {refused}"
    );
    let read = get(&client, OWN_TASK).await;
    assert!(reached_kubernetes(&read), "{read}");

    let url = serve(&jwks, ToolPolicy::allow_all()).await;
    let client = connect(&url, &read_only).await;
    let refused = cancel(&client, OWN_TASK).await;
    assert!(refused.contains("kueue.submit"), "{refused}");

    let viewer = connect(&url, VIEWER_TOKEN).await;
    let refused = cancel(&viewer, OWN_TASK).await;
    assert!(refused.contains("submitter"), "{refused}");
}

#[tokio::test]
async fn a_kube_user_token_needs_a_reviewer_for_tasks() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let url = serve(&jwks, shipped_policy()).await;
    let token = run_token(|c| {
        c.insert("kube_user".into(), json!("alice"));
    });
    let client = connect(&url, &token).await;
    let refused = get(&client, OWN_TASK).await;
    assert!(refused.contains("access reviewer"), "{refused}");
    let refused = cancel(&client, OWN_TASK).await;
    assert!(refused.contains("access reviewer"), "{refused}");
}

#[tokio::test]
async fn a_person_is_refused_by_a_server_without_a_reviewer_whatever_their_claims() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let url = serve(&jwks, shipped_policy()).await;
    for claims in [
        json!({"kube_user": "alice"}),
        json!({"kube_user": "alice", "run_id": "auth-spike", "namespace": "team-e2e", "role": "submitter"}),
    ] {
        let token = run_token(|c| {
            c.insert("iss".into(), json!(PEOPLE));
            for key in ["run_id", "namespace", "role", "queues", "act"] {
                c.remove(key);
            }
            if let Value::Object(extra) = &claims {
                c.extend(extra.clone());
            }
        });
        let response = reqwest::Client::new()
            .post(&url)
            .bearer_auth(&token)
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": {"taskId": OWN_TASK}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{claims}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"], "insufficient_scope", "{body}");
    }
}
