//! End-to-end against a cluster's real SPIRE: the spike pod fetches a JWT-SVID shaped like a
//! sandbox supervisor's, with the crucible controller's issuer as audience. The SVID is exchanged
//! at the controller's agent token endpoint exactly as OpenShell's provider token grant does
//! (client_credentials, jwt-bearer assertion, no client_id), for a sandbox a run pod bound in the
//! controller's ledger. The resulting token drives crucible-kueue served on loopback with the
//! shipped tool policy, trusting both the controller and the Keycloak realm.
//!
//! The controller's token endpoint runs on loopback over a real Postgres ledger, and verifies the
//! SVID against SPIRE's published JWKS through its discovery host.
//!
//! Run with the cluster values `crucible_mcp_auth::testing::cluster` documents, plus:
//!   CRUCIBLE_KUEUE_E2E_CONTEXT=<kube context> CRUCIBLE_KUEUE_E2E_NAMESPACE=<namespace> \
//!   CRUCIBLE_KUEUE_E2E_QUEUE=<local queue> CRUCIBLE_KUEUE_E2E_DATABASE_URL=<postgres url> \
//!   cargo test -p crucible-controller --test kueue_oidc_e2e -- --ignored --nocapture --test-threads=1
//!   CRUCIBLE_KUEUE_E2E_KUBE_USER=<the launcher's Kubernetes user>
//! The launcher's user needs `use` on that LocalQueue and Jobs in that namespace, and no
//! Deployments; the realm's spike client carries the same user for the RBAC test.

use base64::Engine;
use crucible_controller::agent_auth::bindings::{self, Binding};
use crucible_controller::agent_auth::issuer::{
    AgentIssuer, AgentIssuerConfig, IssuerUrl, SigningKey,
};
use crucible_controller::agent_auth::sandbox::SandboxId;
use crucible_controller::agent_auth::{AgentAuth, router as agent_router};
use crucible_mcp_auth::jwks::Jwks;
use crucible_mcp_auth::kube::{GroupSource, SubjectAccessReviewer};
use crucible_mcp_auth::policy::ToolPolicy;
use crucible_mcp_auth::principal::Callers;
use crucible_mcp_auth::server::{ResourceServer, ResourceServerConfig, TrustedIssuer};
use crucible_mcp_auth::testing::cluster;
use k8s_openapi::api::batch::v1::Job;
use kube::api::Api;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use std::sync::Arc;
use url::Url;

type Client = RunningService<RoleClient, ClientConfig>;

/// The issuer identifier sandboxes request SVIDs for: the controller's in-cluster Service URL.
const CONTROLLER: &str = "http://crucible-controller.crucible-system.svc.cluster.local:8080";
const RESOURCE: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";
const SANDBOX_NAME: &str = "ci-0123456789abcdef";
const WORKDIR: &str = "/sandbox/repo";
const RUN_ID: &str = "playbook_oidc-e2e_auth-spike";
const IMAGE: &str = "registry.access.redhat.com/ubi9/ubi-minimal:latest";

/// The spike's SPIFFE ID, `<prefix><gateway id>/sandbox/<sandbox id>`, split into its parts.
fn spiffe_parts() -> (String, String, String) {
    let id = cluster::spiffe_id();
    let cut = id.find("/gateway/").expect("a gateway-keyed SPIFFE ID") + "/gateway/".len();
    let (prefix, rest) = id.split_at(cut);
    let (gateway, sandbox) = rest
        .split_once("/sandbox/")
        .expect("<gateway id>/sandbox/<sandbox id>");
    (prefix.to_string(), gateway.to_string(), sandbox.to_string())
}

fn gateway_prefix() -> String {
    spiffe_parts().0
}

/// The run pod the spike stands in for: a run's gateway id is its pod's name.
fn run_pod() -> String {
    spiffe_parts().1
}

fn sandbox() -> String {
    spiffe_parts().2
}

struct Env {
    context: String,
    namespace: String,
    queue: String,
    database_url: String,
    kube_user: String,
}

fn env() -> Env {
    let var =
        |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set for the OIDC e2e"));
    Env {
        context: var("CRUCIBLE_KUEUE_E2E_CONTEXT"),
        namespace: var("CRUCIBLE_KUEUE_E2E_NAMESPACE"),
        queue: var("CRUCIBLE_KUEUE_E2E_QUEUE"),
        database_url: var("CRUCIBLE_KUEUE_E2E_DATABASE_URL"),
        kube_user: var("CRUCIBLE_KUEUE_E2E_KUBE_USER"),
    }
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

/// A fresh P-256 signing key, as the Secret the controller mounts would hold it.
fn signing_key_pem() -> String {
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
    let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap();
    let der = pair.to_pkcs8v1().unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        lines.join("\n")
    )
}

/// The run the spike's sandbox belongs to: a running run pod on the `dev` cluster, launched by
/// alice (whose Kubernetes user is `kube_user`), with the sandbox bound through the controller's
/// own bind path.
async fn seed_run(pool: &sqlx::PgPool, kube_user: &str) {
    crucible_controller::identity::oidc::users::record_login(
        pool,
        "sub-alice",
        "alice",
        None,
        jiff::Timestamp::now(),
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kube_identity_links (sub, cluster, kube_user, source, linked_at)
         VALUES ('sub-alice', $1, $2, 'token-review', '2026-01-01T00:00:00Z')
         ON CONFLICT (sub, cluster) DO UPDATE SET kube_user = EXCLUDED.kube_user",
    )
    .bind("dev")
    .bind(kube_user)
    .execute(pool)
    .await
    .unwrap();
    for sql in [
        "DELETE FROM run_sandboxes WHERE sandbox_id = $1 OR pod_name = $2",
        "DELETE FROM runs WHERE pod = $2",
        "DELETE FROM work_pods WHERE pod_name = $2",
    ] {
        sqlx::query(sql)
            .bind(sandbox())
            .bind(run_pod())
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO work_pods (pod_name, kind, state, cost_tag, created_at, updated_at, cluster)
         VALUES ($1, 'run', 'running', 'run', now()::text, now()::text, 'dev')",
    )
    .bind(run_pod())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO runs (run_id, status, pod, attributed_to, dispatched_by)
         VALUES ($1, 'running', $2, 'user:alice', 'user:alice')",
    )
    .bind(RUN_ID)
    .bind(run_pod())
    .execute(pool)
    .await
    .unwrap();
    let binding = Binding {
        id: SandboxId::parse(&sandbox()).unwrap(),
        name: SANDBOX_NAME.to_string(),
        workdir: Some(WORKDIR.to_string()),
    };
    let run = bindings::run_pod(pool, RUN_ID).await.unwrap();
    assert_eq!(run.pod, run_pod());
    bindings::bind(pool, &run, &binding).await.unwrap();
}

async fn collect_run(pool: &sqlx::PgPool) {
    sqlx::query("UPDATE work_pods SET state = 'collected' WHERE pod_name = $1")
        .bind(run_pod())
        .execute(pool)
        .await
        .unwrap();
}

/// The controller's agent token endpoint on loopback, over the ledger at `database_url`.
async fn serve_controller(env: &Env, profile_dir: &std::path::Path) -> (String, sqlx::PgPool) {
    let pool = crucible_controller::connect(&env.database_url)
        .await
        .unwrap();
    seed_run(&pool, &env.kube_user).await;
    let profile = profile_dir.join("profile.toml");
    std::fs::write(
        &profile,
        format!(
            r#"
[cluster]
loop_namespace = "crucible-system"
rig_namespace = "rig"
service_account = "crucible-spoke"
sandbox_driver = "kubernetes"
sandbox_namespace = "crucible-sandboxes"
supervisor_image = "registry.example.com/openshell-supervisor:latest"
[image]
loop = "registry.example.com/crucible-loop:latest"
pull_secret = "quay-pull"
[kueue]
image = "registry.example.com/crucible-kueue:latest"
cluster = "dev"
namespace = "crucible-system"
workload_namespace = "{}"
run_issuer = "{CONTROLLER}"
kube_authz = "openshift"
"#,
            env.namespace
        ),
    )
    .unwrap();
    let cfg = AgentIssuerConfig {
        issuer: IssuerUrl::parse(CONTROLLER).unwrap(),
        signing_key: SigningKey::from_pem(signing_key_pem().as_bytes()).unwrap(),
        previous_key: None,
        token_ttl: Default::default(),
        spire: vec![
            format!(
                "dev={} {} spire/oidc-discovery:443",
                cluster::spire(),
                gateway_prefix()
            )
            .parse()
            .unwrap(),
        ],
        audiences: vec![
            format!("{RESOURCE}=kueue.read kueue.submit")
                .parse()
                .unwrap(),
        ],
    };
    let jwks_uri = Url::parse(&format!("{}/keys", cluster::spire())).unwrap();
    let keys = |_: &_| {
        Jwks::new(
            cluster::routes_http(cluster::ingress_ip()),
            jwks_uri.clone(),
        )
    };
    let state = Arc::new(AgentAuth {
        people: None,
        issuer: AgentIssuer::new(cfg, keys),
        pool: pool.clone(),
        profile: Arc::new(crucible::deploy::DeployProfile::load(&profile).unwrap()),
        directory: Arc::new(crucible_controller::agent_auth::directory::KubeDirectory(
            Arc::new(crucible_controller::runs::clusters::ClusterClients::new(
                None,
            )),
        )),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = agent_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}"), pool)
}

/// The supervisor's token grant at the controller, for the MCP resource as audience.
async fn exchange(controller: &str, svid: &str) -> reqwest::Response {
    cluster::token_grant(
        &reqwest::Client::new(),
        &format!("{controller}/oauth/token"),
        svid,
        &[("audience", RESOURCE), ("scope", "kueue.read kueue.submit")],
    )
    .await
}

async fn access_token(controller: &str, svid: &str) -> String {
    let response = exchange(controller, svid).await;
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(
        status.is_success(),
        "token exchange failed: {status} {body}"
    );
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["scope"], "kueue.read kueue.submit");
    let token = body["access_token"].as_str().unwrap().to_string();
    let payload = token.split('.').nth(1).unwrap();
    let claims: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(claims["sandbox"], SANDBOX_NAME);
    assert_eq!(claims["workdir"], WORKDIR);
    token
}

async fn serve_kueue(kube: kube::Client, controller: &str) -> String {
    let jwks_uris = [
        (
            CONTROLLER,
            Url::parse(&format!("{controller}/.well-known/jwks.json")).unwrap(),
        ),
        (
            &cluster::realm(),
            Url::parse(&format!(
                "{}/protocol/openid-connect/certs",
                cluster::realm()
            ))
            .unwrap(),
        ),
    ];
    let config = ResourceServerConfig {
        issuers: jwks_uris
            .iter()
            .map(|(issuer, jwks_uri)| TrustedIssuer {
                issuer: issuer.to_string(),
                jwks_uri: jwks_uri.clone(),
                callers: Callers::Runs,
            })
            .collect(),
        resources: vec![Url::parse(RESOURCE).unwrap()],
        required_scopes: vec!["kueue.read".into()],
        scopes_supported: vec!["kueue.read".into(), "kueue.submit".into()],
        resource_name: Some("crucible-kueue".into()),
        cluster: None,
    };
    let http = cluster::routes_http(cluster::ingress_ip());
    let server = ResourceServer::with_jwks(config, |t| Jwks::new(http.clone(), t.jwks_uri.clone()));
    let policy = ToolPolicy::from_toml(include_str!(
        "../../crucible-kueue/deploy/kueue-policy.toml"
    ))
    .unwrap();
    let reviewer = Arc::new(SubjectAccessReviewer::new(
        kube.clone(),
        GroupSource::OpenShift,
    ));
    let kueue_version = crucible_kueue::kueue::KueueVersion::discover(&kube)
        .await
        .unwrap();
    let app = crucible_kueue::router(
        kube,
        crucible_kueue::Options {
            kinds: crucible_kueue::manifest::WorkloadKind::ALL.to_vec(),
            config: crucible_kueue::config::Config::default(),
            oidc: Arc::new(server),
            policy,
            reviewer: Some(reviewer),
            kueue: kueue_version,
            allowed_hosts: Vec::new(),
        },
    );
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

/// A tool's JSON reply, or the text of a tool error result (a refusal by the guard or the tool).
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
    if result.is_error.unwrap_or(false) {
        return Err(text);
    }
    Ok(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool}: {e}: {text}")))
}

fn job(name: &str) -> Value {
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": name},
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
#[ignore = "needs a cluster's SPIRE and a Postgres; see the module docs"]
async fn a_sandbox_svid_becomes_a_controller_token_that_policy_and_kueue_honor() {
    let env = env();
    let kube = kube_client(&env).await;
    let profile = tempfile::tempdir().unwrap();
    let (controller, pool) = serve_controller(&env, profile.path()).await;
    let svid = cluster::fresh_svid_for(&kube, CONTROLLER).await;
    let token = access_token(&controller, &svid).await;
    let url = serve_kueue(kube.clone(), &controller).await;

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
    let mid = token.len() - 20;
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
    assert_eq!(me["run_id"], run_pod());
    assert_eq!(me["namespace"], env.namespace);
    assert_eq!(me["launched_by"], "alice");

    let name = format!("oidc-e2e-{}", std::process::id());
    let refused = call(
        &client,
        "workload_plan",
        json!({"queue": "not-my-queue", "manifest": job(&name)}),
    )
    .await;
    let refusal = refused.expect_err("a queue the launcher may not use must be refused");
    assert!(
        refusal.contains("queue-use"),
        "the refusal names the check: {refusal}"
    );

    let mut forged = job(&name);
    forged["metadata"]["annotations"] = json!({"crucible.dev/actor": "forged-by-the-agent"});
    let refused = call(
        &client,
        "workload_plan",
        json!({"queue": env.queue, "manifest": forged}),
    )
    .await
    .expect_err("a forged actor annotation must be refused");
    let refused: Value = serde_json::from_str(&refused).unwrap();
    assert_eq!(refused["status"], "refused", "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|e| e.contains("reserved for the server")),
        "an agent cannot set the actor annotation: {refused}"
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
        Some("alice")
    );
    assert_eq!(
        labels.get("crucible.dev/run-id").map(String::as_str),
        Some(run_pod().as_str())
    );
    assert_eq!(
        annotations.get("crucible.dev/actor").map(String::as_str),
        Some(cluster::spiffe_id().as_str()),
        "the server stamps the token's act.sub as the actor"
    );

    collect_run(&pool).await;
    let after = exchange(&controller, &svid).await;
    assert_eq!(after.status(), 400);
    let body: Value = after.json().await.unwrap();
    assert_eq!(body["error"], "invalid_grant", "{body}");
}

#[tokio::test]
#[ignore = "needs a cluster's SPIRE and a Postgres; see the module docs"]
async fn a_tampered_svid_or_a_foreign_audience_gets_no_token() {
    let env = env();
    let kube = kube_client(&env).await;
    let profile = tempfile::tempdir().unwrap();
    let (controller, _pool) = serve_controller(&env, profile.path()).await;
    let svid = cluster::fresh_svid_for(&kube, CONTROLLER).await;
    let tampered = format!("{}AAAA", &svid[..svid.len() - 4]);
    let response = exchange(&controller, &tampered).await;
    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], "invalid_client", "{body}");

    let other = cluster::token_grant(
        &reqwest::Client::new(),
        &format!("{controller}/oauth/token"),
        &svid,
        &[("audience", "https://elsewhere.example/mcp")],
    )
    .await;
    assert_eq!(other.status(), 400);
    let body: Value = other.json().await.unwrap();
    assert_eq!(body["error"], "invalid_target", "{body}");
}

fn deployment(name: &str) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name},
        "spec": {
            "replicas": 1,
            "selector": {"matchLabels": {"app": name}},
            "template": {
                "metadata": {"labels": {"app": name}},
                "spec": {"containers": [{
                    "name": "main",
                    "image": IMAGE,
                    "command": ["sleep", "60"],
                    "resources": {"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"memory": "64Mi"}}
                }]}
            }
        }
    })
}

#[tokio::test]
#[ignore = "needs a cluster's SPIRE + Keycloak; see the module docs"]
async fn kubernetes_rbac_decides_what_the_launcher_may_create() {
    let env = env();
    let kube = kube_client(&env).await;
    let token = cluster::access_token(&cluster::routes_http(cluster::ingress_ip()), &kube).await;
    let url = serve_kueue(kube, "http://127.0.0.1:1").await;
    let client = connect(&url, &token).await;

    let name = format!("oidc-e2e-sar-{}", std::process::id());
    let refused = call(
        &client,
        "workload_plan",
        json!({"queue": env.queue, "manifest": deployment(&name)}),
    )
    .await;
    let refusal = refused.expect_err("crucible-e2e-sar has no RBAC to create Deployments");
    assert!(
        refusal.contains("workload-create"),
        "the refusal names the rule: {refusal}"
    );

    let elsewhere = call(&client, "queues_list", json!({"namespace": "kube-system"})).await;
    let refusal = elsewhere.expect_err("the launcher may not list queues in kube-system");
    assert!(refusal.contains("queues-list"), "{refusal}");

    let listed = call(&client, "queues_list", json!({})).await.unwrap();
    assert!(listed.to_string().contains(&env.queue), "{listed}");
}

#[test]
fn the_contract_names_the_scopes_crucible_kueue_checks() {
    assert_eq!(
        crucible_contract::kueue::SCOPE_READ,
        crucible_kueue::access::SCOPE_READ
    );
    assert_eq!(
        crucible_contract::kueue::SCOPE_SUBMIT,
        crucible_kueue::access::SCOPE_SUBMIT
    );
}
