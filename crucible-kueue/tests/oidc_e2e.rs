//! End-to-end against pirate's real auth stack (deploy/auth): a SPIRE JWT-SVID shaped like a
//! sandbox supervisor's is exchanged at Keycloak for an access token exactly as OpenShell's
//! provider token grant does (client_credentials, jwt-bearer assertion, no client_id), and that
//! token drives crucible-kueue served on loopback with the shipped tool policy.
//!
//! Run with:
//!   CRUCIBLE_KUEUE_E2E_CONTEXT=<kube context> CRUCIBLE_KUEUE_E2E_NAMESPACE=crucible-kueue-e2e \
//!   CRUCIBLE_KUEUE_E2E_QUEUE=crucible-e2e CRUCIBLE_KUEUE_E2E_INGRESS_IP=10.14.217.8 \
//!   cargo test -p crucible-kueue --test oidc_e2e -- --ignored --nocapture
//! INGRESS_IP pins the realm's Route host when it has no DNS. The realm's run-auth-spike client
//! must carry the namespace and queue given here.

use crucible_contract::kueue::token_digest;
use crucible_kueue::auth::Auth;
use crucible_kueue::tenant::{Registry, Tenants};
use crucible_mcp_auth::jwks::Jwks;
use crucible_mcp_auth::policy::ToolPolicy;
use crucible_mcp_auth::server::{ResourceServer, ResourceServerConfig};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, LogParams, PostParams};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

type Client = RunningService<RoleClient, ClientConfig>;

const REALM: &str = "https://keycloak.apps.openshift.nm.rdu4.corp.redhat.com/realms/crucible";
const RESOURCE: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";
const SPIKE_POD: &str = "auth-spike";
const SPIKE_NAMESPACE: &str = "rhbk";
const SPIFFE_ID: &str = "spiffe://pirate.crucible/openshell/sandbox/auth-spike";
const TENANT_TOKEN: &str = "oidc-e2e-tenant-token";
const IMAGE: &str = "registry.access.redhat.com/ubi9/ubi-minimal:latest";

struct Env {
    context: String,
    namespace: String,
    queue: String,
    ingress_ip: Option<std::net::IpAddr>,
}

fn env() -> Env {
    let var =
        |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set for the OIDC e2e"));
    Env {
        context: var("CRUCIBLE_KUEUE_E2E_CONTEXT"),
        namespace: var("CRUCIBLE_KUEUE_E2E_NAMESPACE"),
        queue: var("CRUCIBLE_KUEUE_E2E_QUEUE"),
        ingress_ip: std::env::var("CRUCIBLE_KUEUE_E2E_INGRESS_IP")
            .ok()
            .map(|ip| ip.parse().unwrap()),
    }
}

fn realm_http(env: &Env) -> reqwest::Client {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
    if let Some(ip) = env.ingress_ip {
        let host = Url::parse(REALM).unwrap().host_str().unwrap().to_string();
        builder = builder.resolve(&host, std::net::SocketAddr::new(ip, 443));
    }
    builder.build().unwrap()
}

async fn kube_client(env: &Env) -> kube::Client {
    crucible_kueue::install_tls_provider();
    let options = kube::config::KubeConfigOptions {
        context: Some(env.context.clone()),
        ..Default::default()
    };
    let config = kube::Config::from_kubeconfig(&options).await.unwrap();
    kube::Client::try_from(config).unwrap()
}

/// Recreate the spike pod (deploy/auth/spike.yaml) and return the JWT-SVID it fetched.
async fn fresh_svid(kube: &kube::Client) -> String {
    let pods: Api<Pod> = Api::namespaced(kube.clone(), SPIKE_NAMESPACE);
    let _ = pods.delete(SPIKE_POD, &DeleteParams::default()).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while pods.get_opt(SPIKE_POD).await.unwrap().is_some() {
        assert!(Instant::now() < deadline, "old spike pod never went away");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let pod: Pod = serde_norway::from_str(include_str!("../deploy/auth/spike.yaml")).unwrap();
    pods.create(&PostParams::default(), &pod).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let phase = pods
            .get(SPIKE_POD)
            .await
            .unwrap()
            .status
            .and_then(|s| s.phase)
            .unwrap_or_default();
        if phase == "Succeeded" {
            break;
        }
        assert!(Instant::now() < deadline, "spike pod stuck in {phase:?}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let logs = pods.logs(SPIKE_POD, &LogParams::default()).await.unwrap();
    let fetched: Value =
        serde_json::from_str(logs.trim()).unwrap_or_else(|e| panic!("{e}: {logs}"));
    assert_eq!(fetched[0]["svids"][0]["spiffe_id"], SPIFFE_ID);
    fetched[0]["svids"][0]["svid"].as_str().unwrap().to_string()
}

/// The supervisor's token grant: client_credentials with the SVID as a jwt-bearer assertion.
async fn exchange(http: &reqwest::Client, svid: &str) -> reqwest::Response {
    http.post(format!("{REALM}/protocol/openid-connect/token"))
        .form(&[
            ("grant_type", "client_credentials"),
            (
                "client_assertion_type",
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
            ),
            ("client_assertion", svid),
        ])
        .send()
        .await
        .unwrap()
}

async fn access_token(env: &Env, kube: &kube::Client) -> String {
    let svid = fresh_svid(kube).await;
    let response = exchange(&realm_http(env), &svid).await;
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(
        status.is_success(),
        "token exchange failed: {status} {body}"
    );
    body["access_token"].as_str().unwrap().to_string()
}

async fn serve(env: &Env, kube: kube::Client) -> String {
    let tenants = Tenants::from_toml(&format!(
        "[[tenant]]\nrun_id = \"oidc-e2e-tenant\"\ntoken_sha256 = \"{}\"\nnamespace = \"{}\"\nrole = \"viewer\"\n",
        token_digest(TENANT_TOKEN),
        env.namespace
    ))
    .unwrap();
    let config = ResourceServerConfig {
        issuer: Url::parse(REALM).unwrap(),
        jwks_uri: Url::parse(&format!("{REALM}/protocol/openid-connect/certs")).unwrap(),
        resources: vec![Url::parse(RESOURCE).unwrap()],
        required_scopes: vec!["kueue.read".into()],
        scopes_supported: vec!["kueue.read".into(), "kueue.submit".into()],
        resource_name: Some("crucible-kueue".into()),
    };
    let jwks = Jwks::new(realm_http(env), config.jwks_uri.clone());
    let auth = Auth {
        registry: Arc::new(Registry::new(tenants)),
        oidc: Some(Arc::new(ResourceServer::with_jwks(config, jwks))),
    };
    let policy = ToolPolicy::from_toml(include_str!("../deploy/auth/kueue-policy.toml")).unwrap();
    let app = crucible_kueue::router(kube, auth, Some(policy), &[]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}{}", crucible_kueue::MCP_PATH)
}

async fn connect(url: &str, token: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
    );
    ClientConfig::default().serve(transport).await.unwrap()
}

/// A tool's JSON reply, or the JSON-RPC error message when the call was refused.
async fn call(client: &Client, tool: &str, args: Value) -> Result<Value, String> {
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
        .unwrap_or_else(|| panic!("{tool}: no text content"));
    Ok(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool}: {e}: {text}")))
}

fn job(name: &str) -> Value {
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": name, "annotations": {"crucible.dev/actor": "forged-by-the-agent"}},
        "spec": {
            "backoffLimit": 0,
            "ttlSecondsAfterFinished": 300,
            "template": {"spec": {
                "restartPolicy": "Never",
                "containers": [{
                    "name": "main",
                    "image": IMAGE,
                    "command": ["/bin/sh", "-c", "echo ok"],
                    "resources": {"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"memory": "64Mi"}}
                }]
            }}
        }
    })
}

#[tokio::test]
#[ignore = "needs pirate's SPIRE + Keycloak; see the module docs"]
async fn a_sandbox_svid_becomes_a_delegated_token_that_policy_and_kueue_honor() {
    let env = env();
    let kube = kube_client(&env).await;
    let token = access_token(&env, &kube).await;
    let url = serve(&env, kube.clone()).await;

    let raw = reqwest::Client::new();
    let unauthenticated = raw.post(&url).json(&json!({})).send().await.unwrap();
    assert_eq!(unauthenticated.status(), 401);
    let challenge = unauthenticated.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        challenge.contains(r#"resource_metadata="http://crucible-kueue.crucible-system.svc.cluster.local:8850/.well-known/oauth-protected-resource/mcp""#),
        "{challenge}"
    );
    let mid = token.len() - 40;
    let flipped = if &token[mid..mid + 1] == "A" {
        "B"
    } else {
        "A"
    };
    let forged = format!("{}{flipped}{}", &token[..mid], &token[mid + 1..]);
    let bad = raw
        .post(&url)
        .bearer_auth(&forged)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);
    assert!(
        bad.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains(r#"error="invalid_token""#)
    );

    let client = connect(&url, &token).await;
    let me = call(&client, "whoami", json!({})).await.unwrap();
    assert_eq!(me["run_id"], "auth-spike");
    assert_eq!(me["namespace"], env.namespace);
    assert_eq!(me["launched_by"], "weaton");
    assert_eq!(me["queues"], json!([env.queue]));

    let name = format!("oidc-e2e-{}", std::process::id());
    let refused = call(
        &client,
        "workload_plan",
        json!({"queue": "not-my-queue", "manifest": job(&name)}),
    )
    .await;
    let refusal = refused.expect_err("a queue outside the token's claims must be refused");
    assert!(
        refusal.contains("queue-allowed"),
        "the refusal names the rule: {refusal}"
    );

    let plan = call(
        &client,
        "workload_plan",
        json!({"queue": env.queue, "manifest": job(&name)}),
    )
    .await
    .unwrap();
    let plan_hash = plan["plan_hash"]
        .as_str()
        .unwrap_or_else(|| panic!("no plan_hash: {plan}"))
        .to_string();
    let submitted = call(
        &client,
        "workload_submit",
        json!({"queue": env.queue, "manifest": job(&name), "plan_hash": plan_hash}),
    )
    .await
    .unwrap();
    assert_eq!(submitted["submitted"], true, "{submitted}");

    let jobs: Api<Job> = Api::namespaced(kube.clone(), &env.namespace);
    let created = jobs.get(&name).await.unwrap();
    let labels = created.metadata.labels.clone().unwrap_or_default();
    let annotations = created.metadata.annotations.clone().unwrap_or_default();
    let _ = call(
        &client,
        "workload_delete",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(
        labels.get("crucible.dev/launched-by").map(String::as_str),
        Some("weaton")
    );
    assert_eq!(
        labels.get("crucible.dev/run-id").map(String::as_str),
        Some("auth-spike")
    );
    assert_eq!(
        annotations.get("crucible.dev/actor").map(String::as_str),
        Some(SPIFFE_ID),
        "the policy replaced the agent's forged actor with the token's act.sub"
    );
}

#[tokio::test]
#[ignore = "needs pirate's SPIRE + Keycloak; see the module docs"]
async fn tenant_tokens_keep_working_beside_realm_tokens_under_the_same_policy() {
    let env = env();
    let kube = kube_client(&env).await;
    let url = serve(&env, kube).await;
    let client = connect(&url, TENANT_TOKEN).await;
    let me = call(&client, "whoami", json!({})).await.unwrap();
    assert_eq!(me["run_id"], "oidc-e2e-tenant");
    assert_eq!(me["launched_by"], Value::Null);
    let refused = call(
        &client,
        "workload_plan",
        json!({"queue": env.queue, "manifest": job("never")}),
    )
    .await;
    let refusal = refused.expect_err("a viewer's tenant token lacks kueue.submit");
    assert!(
        refusal.contains("Insufficient scope"),
        "the 403 step-up challenge: {refusal}"
    );
}

#[tokio::test]
#[ignore = "needs pirate's SPIRE + Keycloak; see the module docs"]
async fn a_tampered_svid_gets_no_token() {
    let env = env();
    let kube = kube_client(&env).await;
    let svid = fresh_svid(&kube).await;
    let tampered = format!("{}AAAA", &svid[..svid.len() - 4]);
    let response = exchange(&realm_http(&env), &tampered).await;
    assert!(response.status().is_client_error(), "{}", response.status());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], "invalid_client");
}
