//! End-to-end against a cluster's real auth stack: the `buildit mcp` binary serves with realm
//! tokens and the shipped policy, and a token minted from the spike pod's JWT-SVID (the realm's
//! spike client carries a `sandbox` claim) drives it.
//!
//! Run with the cluster values `crucible_mcp_auth::testing::cluster` documents, plus:
//!   BUILDIT_E2E_CONTEXT=<kube context> BUILDIT_E2E_NAMESPACE=<namespace> \
//!   cargo test -p buildit --test oidc_e2e -- --ignored --nocapture
//! The spawned server fetches the realm's keys itself, so the realm host must resolve locally.

use crucible_mcp_auth::testing::cluster;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

type Client = RunningService<RoleClient, ClientConfig>;

const PORT: u16 = 8849;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn var(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set for the buildit OIDC e2e"))
}

async fn kube_client(context: &str) -> kube::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let options = kube::config::KubeConfigOptions {
        context: Some(context.to_string()),
        ..Default::default()
    };
    let config = kube::Config::from_kubeconfig(&options).await.unwrap();
    kube::Client::try_from(config).unwrap()
}

async fn spawn_server(context: &str, namespace: &str) -> Server {
    let child = Command::new(env!("CARGO_BIN_EXE_buildit"))
        .args([
            "mcp",
            "--bind",
            &format!("127.0.0.1:{PORT}"),
            "--kubecontext",
            context,
            "--namespace",
            namespace,
            "--sandbox-workdir",
            "/sandbox",
            "--run-issuer",
            &format!("{0}={0}/protocol/openid-connect/certs", cluster::realm()),
            "--resource",
            &format!("http://localhost:{PORT}/mcp"),
            "--tool-policy",
            concat!(env!("CARGO_MANIFEST_DIR"), "/deploy/buildit-policy.toml"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let server = Server(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    while tokio::net::TcpStream::connect(("127.0.0.1", PORT))
        .await
        .is_err()
    {
        assert!(Instant::now() < deadline, "buildit mcp never listened");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    server
}

async fn call(client: &Client, tool: &str, args: Value) -> Result<String, String> {
    let Value::Object(map) = args else {
        panic!("args must be an object")
    };
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(map))
        .await
        .map_err(|e| e.to_string())?;
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    if result.is_error.unwrap_or(false) {
        return Err(text);
    }
    Ok(text)
}

#[tokio::test]
#[ignore = "needs a cluster's SPIRE + Keycloak; see the module docs"]
async fn a_sandbox_token_drives_buildit_under_the_shipped_policy() {
    let context = var("BUILDIT_E2E_CONTEXT");
    let namespace = var("BUILDIT_E2E_NAMESPACE");
    let kube = kube_client(&context).await;
    let token = cluster::access_token(&cluster::routes_http(cluster::ingress_ip()), &kube).await;
    let _server = spawn_server(&context, &namespace).await;
    let url = format!("http://localhost:{PORT}/mcp");

    let raw = reqwest::Client::new();
    let anonymous = raw.post(&url).json(&json!({})).send().await.unwrap();
    assert_eq!(anonymous.status(), 401);
    let challenge = anonymous.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        challenge.contains(&format!(
            r#"resource_metadata="http://localhost:{PORT}/.well-known/oauth-protected-resource/mcp""#
        )),
        "{challenge}"
    );
    let metadata: Value = raw
        .get(format!(
            "http://localhost:{PORT}/.well-known/oauth-protected-resource/mcp"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["authorization_servers"], json!([cluster::realm()]));

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.as_str()).auth_header(token),
    );
    let client = ClientConfig::default().serve(transport).await.unwrap();

    let status = call(
        &client,
        "status",
        json!({"build_id": "buildit-0000000000000000"}),
    )
    .await
    .expect("an authenticated status call reaches buildit");
    assert!(
        status.contains("buildit-0000000000000000"),
        "buildit answered for the token's sandbox: {status}"
    );

    let refused = call(&client, "build", json!({"context": ".", "timeout_s": 7200})).await;
    let refusal = refused.expect_err("a build over the policy's timeout cap is refused");
    assert!(refusal.contains("build-timeout-cap"), "{refusal}");
}
