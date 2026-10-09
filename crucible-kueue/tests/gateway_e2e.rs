//! End-to-end for isolated gateways against a real cluster: the controller's own code starts a
//! run's gateway pod in the sandbox namespace, a stand-in loop pod in the loop namespace creates
//! a sandbox through it over mTLS with the OpenShell CLI, and the supervisor the gateway created
//! gets a gateway-keyed SPIFFE ID from SPIRE. A JWT-SVID with that identity (fetched by a pod
//! carrying the supervisor's SPIRE selectors, see [`supervisor_svid`]) is exchanged at the
//! controller's token endpoint (on loopback, over a real Postgres ledger, binding through the
//! cluster's Sandbox resources) and the token drives crucible-kueue on loopback. The run's gateway is torn down at the end.
//!
//! Prepare the cluster with `just gateway-e2e-setup` for the same profile, then run with the
//! values `crucible_mcp_auth::testing::cluster` documents for SPIRE, plus:
//!   CRUCIBLE_GATEWAY_E2E_KUBECONFIG=<kubeconfig whose current context's namespace is the loop
//!     namespace> CRUCIBLE_GATEWAY_E2E_CLUSTER=<the profile's [kueue] cluster>
//!   CRUCIBLE_GATEWAY_E2E_PROFILE=<isolated deploy profile> CRUCIBLE_GATEWAY_E2E_DATABASE_URL=<url>
//!   CRUCIBLE_GATEWAY_E2E_SPIRE_AGENT_IMAGE=<spire-agent image>
//!   CRUCIBLE_GATEWAY_E2E_KUBE_USER=<Kubernetes user the run's launcher acts as> \
//!   cargo test -p crucible-kueue --test gateway_e2e -- --ignored --nocapture --test-threads=1

use base64::Engine;
use crucible::deploy::DeployProfile;
use crucible_controller::PodDispatcher;
use crucible_controller::agent_auth::bindings::{self, Binding, SandboxId};
use crucible_controller::agent_auth::directory::{KubeDirectory, SandboxDirectory};
use crucible_controller::agent_auth::issuer::{
    AgentIssuer, AgentIssuerConfig, GatewayPrefix, IssuerUrl, SigningKey,
};
use crucible_controller::agent_auth::{AgentAuth, router as agent_router};
use crucible_controller::runs::clusters::ClusterClients;
use crucible_controller::runs::workpod::gateway::{RunGateway, teardown};
use crucible_mcp_auth::jwks::Jwks;
use crucible_mcp_auth::policy::ToolPolicy;
use crucible_mcp_auth::principal::Callers;
use crucible_mcp_auth::server::{ResourceServer, ResourceServerConfig, TrustedIssuer};
use crucible_mcp_auth::testing::cluster;
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::{Api, ApiResource, DynamicObject, ListParams, LogParams};
use kube::core::GroupVersionKind;
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

const CONTROLLER: &str = "http://crucible-controller.crucible-system.svc.cluster.local:8080";
const RESOURCE: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";
const RUN_POD: &str = "crucible-run-gateway-e2e";
const RUN_ID: &str = "gateway-e2e";
const OTHER_POD: &str = "crucible-run-gateway-e2e-other";
const SANDBOX_IMAGE: &str = "registry.access.redhat.com/ubi9/ubi-minimal:latest";
const SOCKET: &str = "/spiffe-workload-api/spire-agent.sock";
const GATEWAY_ID_LABEL: &str = "openshell.ai/gateway-id";

struct Env {
    kubeconfig: String,
    cluster: String,
    profile: std::path::PathBuf,
    database_url: String,
    spire_agent_image: String,
    kube_user: String,
}

fn env() -> Env {
    let var = |k: &str| {
        std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set for the gateway e2e"))
    };
    Env {
        kubeconfig: var("CRUCIBLE_GATEWAY_E2E_KUBECONFIG"),
        cluster: var("CRUCIBLE_GATEWAY_E2E_CLUSTER"),
        profile: var("CRUCIBLE_GATEWAY_E2E_PROFILE").into(),
        database_url: var("CRUCIBLE_GATEWAY_E2E_DATABASE_URL"),
        spire_agent_image: var("CRUCIBLE_GATEWAY_E2E_SPIRE_AGENT_IMAGE"),
        kube_user: var("CRUCIBLE_GATEWAY_E2E_KUBE_USER"),
    }
}

/// The controller's cluster registry with `env.cluster` mounted the way a spoke is, and the plain
/// client for that same cluster.
async fn clusters(env: &Env, dir: &std::path::Path) -> (Arc<ClusterClients>, kube::Client) {
    crucible_kueue::install_tls_provider();
    let spoke = dir.join(&env.cluster);
    std::fs::create_dir_all(&spoke).unwrap();
    std::fs::copy(&env.kubeconfig, spoke.join("kubeconfig")).unwrap();
    let clusters = Arc::new(ClusterClients::new(Some(dir.to_path_buf())));
    let kube = clusters.client(&env.cluster).await.unwrap();
    (clusters, kube)
}

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

/// A running run pod on `cluster`, launched by alice, with no sandbox bound yet.
async fn seed_run(pool: &sqlx::PgPool, cluster: &str, pod: &str, run_id: &str) {
    for sql in [
        "DELETE FROM run_sandboxes WHERE pod_name = $1",
        "DELETE FROM runs WHERE pod = $1",
        "DELETE FROM work_pods WHERE pod_name = $1",
    ] {
        sqlx::query(sql).bind(pod).execute(pool).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO work_pods (pod_name, kind, state, cost_tag, created_at, updated_at, cluster)
         VALUES ($1, 'run', 'running', 'run', now()::text, now()::text, $2)",
    )
    .bind(pod)
    .bind(cluster)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO runs (run_id, status, pod, attributed_to, dispatched_by)
         VALUES ($1, 'running', $2, 'user:alice', 'user:alice')",
    )
    .bind(run_id)
    .bind(pod)
    .execute(pool)
    .await
    .unwrap();
}

/// The stand-in for the run's loop pod: the loop image with the env the render projects for the
/// gateway, and the run's gateway client material. It registers the gateway with the OpenShell
/// CLI and creates one sandbox through it.
fn loop_pod(
    profile: &DeployProfile,
    namespace: &str,
    image: &str,
    gateway_url: &str,
    client_secret: &str,
) -> Pod {
    let cluster = &profile.cluster;
    let sandbox_ns = cluster.sandbox_namespace.as_ref().unwrap().as_str();
    let pull_secret = profile.image.pull_secret.as_str();
    let script = r#"set -eu
export HOME=/tmp/home XDG_CONFIG_HOME=/tmp/home/.config
mkdir -p /tmp/tls/client "$XDG_CONFIG_HOME"
cp /etc/crucible/gateway-tls/ca.crt /tmp/tls/ca.crt
cp /etc/crucible/gateway-tls/tls.crt /etc/crucible/gateway-tls/tls.key /tmp/tls/client/
until OPENSHELL_LOCAL_TLS_DIR=/tmp/tls openshell gateway add "$GATEWAY_URL" --local --name ci; do sleep 3; done
until openshell status; do sleep 3; done
until openshell sandbox create --name gateway-e2e --from "$SANDBOX_IMAGE" -- true; do
    openshell sandbox delete gateway-e2e || true
    sleep 10
done
echo SANDBOX_CREATE_DONE
sleep 1800
"#;
    let env = |name: &str, value: &str| json!({"name": name, "value": value});
    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": RUN_POD,
            "namespace": namespace,
            "labels": {"crucible/run": RUN_POD},
        },
        "spec": {
            "restartPolicy": "Never",
            "automountServiceAccountToken": false,
            "imagePullSecrets": [{"name": pull_secret}],
            "securityContext": {"seccompProfile": {"type": "RuntimeDefault"}},
            "containers": [{
                "name": "loop",
                "image": image,
                "command": ["/bin/sh", "-c", script],
                "env": [
                    env("GATEWAY_URL", gateway_url),
                    env("SANDBOX_IMAGE", SANDBOX_IMAGE),
                    env("CRUCIBLE_SANDBOX_NAMESPACE", sandbox_ns),
                    env(
                        "CRUCIBLE_SANDBOX_SERVICE_ACCOUNT",
                        cluster.sandbox_service_account.as_deref().unwrap(),
                    ),
                    env("CRUCIBLE_SANDBOX_DEFAULT_IMAGE", SANDBOX_IMAGE),
                    env("CRUCIBLE_SANDBOX_IMAGE_PULL_SECRETS", pull_secret),
                    env(
                        "CRUCIBLE_SANDBOX_SPIFFE_SOCKET",
                        cluster.sandbox_spiffe_socket.as_deref().unwrap(),
                    ),
                    env("OPENSHELL_SUPERVISOR_IMAGE", &cluster.supervisor_image),
                ],
                "securityContext": {
                    "allowPrivilegeEscalation": false,
                    "capabilities": {"drop": ["ALL"]},
                },
                "volumeMounts": [{
                    "name": "gateway-tls",
                    "mountPath": "/etc/crucible/gateway-tls",
                    "readOnly": true,
                }],
            }],
            "volumes": [{"name": "gateway-tls", "secret": {"secretName": client_secret}}],
        },
    }))
    .unwrap()
}

/// A pod in the loop namespace dressed as a supervisor of this run's gateway, asking SPIRE for an
/// SVID: the ClusterSPIFFEID selects only the sandbox namespace, so it must get none.
fn impostor_pod(namespace: &str, image: &str, sandbox_id: &str) -> Pod {
    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "gateway-e2e-impostor",
            "namespace": namespace,
            "labels": {"openshell.ai/boundary-role": "supervisor", GATEWAY_ID_LABEL: RUN_POD},
            "annotations": {"openshell.ai/sandbox-id": sandbox_id},
        },
        "spec": {
            "restartPolicy": "Never",
            "automountServiceAccountToken": false,
            "securityContext": {"seccompProfile": {"type": "RuntimeDefault"}},
            "containers": [{
                "name": "fetch",
                "image": image,
                "command": ["/spire-agent", "api", "fetch", "jwt", "-audience", CONTROLLER,
                            "-socketPath", SOCKET, "-output", "json", "-timeout", "20s"],
                "securityContext": {"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]}},
                "volumeMounts": [{"name": "spiffe-workload-api", "mountPath": "/spiffe-workload-api", "readOnly": true}],
            }],
            "volumes": [{"name": "spiffe-workload-api", "csi": {"driver": "csi.spiffe.io", "readOnly": true}}],
        },
    }))
    .unwrap()
}

async fn wait_for<T, F, Fut>(what: &str, timeout: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(found) = probe().await {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn sandbox_api(kube: &kube::Client, namespace: &str) -> Api<DynamicObject> {
    let gvk = GroupVersionKind::gvk("agents.x-k8s.io", "v1alpha1", "Sandbox");
    Api::namespaced_with(kube.clone(), namespace, &ApiResource::from_gvk(&gvk))
}

/// A JWT-SVID for `audience` with exactly the identity SPIRE gives `supervisor`: a pod beside it
/// carrying the selectors the ClusterSPIFFEID reads (its boundary-role and gateway-id labels and
/// its sandbox-id annotation), running as the sandbox account. SPIRE's Kubernetes attestor does
/// not attest ephemeral containers, and a supervisor fetches SVIDs itself only for a configured
/// provider token grant, so the SVID is fetched this way. The pod carries the gateway-id label, so
/// teardown removes it with the gateway's other pods.
async fn supervisor_svid(pods: &Api<Pod>, supervisor: &Pod, image: &str, audience: &str) -> String {
    let labels = supervisor.metadata.labels.clone().unwrap_or_default();
    let sandbox_id = supervisor
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("openshell.ai/sandbox-id"))
        .cloned()
        .unwrap();
    let spec = supervisor.spec.clone().unwrap();
    let running_as = spec
        .containers
        .iter()
        .find(|c| c.name == "supervisor")
        .and_then(|c| c.security_context.clone())
        .unwrap_or_default();
    let name = "gateway-e2e-svid";
    let _ = pods.delete(name, &Default::default()).await;
    wait_for(
        "the old SVID pod to go",
        Duration::from_secs(60),
        || async { pods.get_opt(name).await.ok()?.is_none().then_some(()) },
    )
    .await;
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "labels": {
                "openshell.ai/boundary-role": labels.get("openshell.ai/boundary-role"),
                GATEWAY_ID_LABEL: labels.get(GATEWAY_ID_LABEL),
            },
            "annotations": {"openshell.ai/sandbox-id": sandbox_id},
        },
        "spec": {
            "restartPolicy": "OnFailure",
            "serviceAccountName": spec.service_account_name,
            "automountServiceAccountToken": false,
            "nodeName": spec.node_name,
            "securityContext": {"seccompProfile": {"type": "RuntimeDefault"}},
            "containers": [{
                "name": "fetch",
                "image": image,
                "command": ["/spire-agent", "api", "fetch", "jwt", "-audience", audience,
                            "-socketPath", SOCKET, "-output", "json", "-timeout", "60s"],
                "securityContext": {"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]},
                                    "runAsNonRoot": true, "runAsUser": running_as.run_as_user,
                                    "runAsGroup": running_as.run_as_group},
                "volumeMounts": [{"name": "spiffe-workload-api", "mountPath": "/spiffe-workload-api", "readOnly": true}],
            }],
            "volumes": [{"name": "spiffe-workload-api", "csi": {"driver": "csi.spiffe.io", "readOnly": true}}],
        },
    }))
    .unwrap();
    pods.create(&Default::default(), &pod).await.unwrap();
    let phase = wait_for(
        "SPIRE to register the pod and the fetch to succeed",
        Duration::from_secs(300),
        || async {
            let phase = pods.get(name).await.ok()?.status?.phase?;
            (phase == "Succeeded").then_some(phase)
        },
    )
    .await;
    let logs = pods
        .logs(name, &LogParams::default())
        .await
        .unwrap_or_default();
    let fetched: Value = serde_json::from_str(logs.trim())
        .unwrap_or_else(|e| panic!("SVID fetch {phase}: {e}: {logs}"));
    fetched[0]["svids"][0]["svid"]
        .as_str()
        .unwrap_or_else(|| panic!("no SVID: {logs}"))
        .to_string()
}

fn claims(token: &str) -> Value {
    let payload = token.split('.').nth(1).unwrap();
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap(),
    )
    .unwrap()
}

async fn serve_controller(
    pool: &sqlx::PgPool,
    profile: &std::path::Path,
    gateway_prefix: &str,
    clusters: Arc<ClusterClients>,
) -> String {
    let cfg = AgentIssuerConfig {
        issuer: IssuerUrl::parse(CONTROLLER).unwrap(),
        signing_key: SigningKey::from_pem(signing_key_pem().as_bytes()).unwrap(),
        previous_key: None,
        token_ttl: Default::default(),
        svid_issuer: cluster::spire(),
        svid_jwks_uri: Url::parse(&format!("{}/keys", cluster::spire())).unwrap(),
        svid_ca: None,
        gateway_prefix: GatewayPrefix::parse(gateway_prefix).unwrap(),
        audiences: vec![
            format!("{RESOURCE}=kueue.read kueue.submit")
                .parse()
                .unwrap(),
        ],
    };
    let jwks = Jwks::new(
        cluster::routes_http(cluster::ingress_ip()),
        cfg.svid_jwks_uri.clone(),
    );
    let state = Arc::new(AgentAuth {
        issuer: AgentIssuer::new(cfg, jwks),
        pool: pool.clone(),
        profile: crucible_controller::agent_auth::placement::load_profile(Some(profile))
            .await
            .unwrap()
            .unwrap(),
        directory: Arc::new(KubeDirectory(clusters)),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = agent_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

async fn exchange(controller: &str, svid: &str) -> (u16, Value) {
    let response = cluster::token_grant(
        &reqwest::Client::new(),
        &format!("{controller}/oauth/token"),
        svid,
        &[("audience", RESOURCE), ("scope", "kueue.read")],
    )
    .await;
    let status = response.status().as_u16();
    (status, response.json().await.unwrap())
}

async fn serve_kueue(kube: kube::Client, controller: &str) -> String {
    let config = ResourceServerConfig {
        issuers: vec![TrustedIssuer {
            issuer: CONTROLLER.to_string(),
            jwks_uri: Url::parse(&format!("{controller}/.well-known/jwks.json")).unwrap(),
            callers: Callers::Runs,
        }],
        resources: vec![Url::parse(RESOURCE).unwrap()],
        required_scopes: vec!["kueue.read".into()],
        scopes_supported: vec!["kueue.read".into(), "kueue.submit".into()],
        resource_name: Some("crucible-kueue".into()),
    };
    let http = reqwest::Client::new();
    let server = ResourceServer::with_jwks(config, |t| Jwks::new(http.clone(), t.jwks_uri.clone()));
    let policy = ToolPolicy::from_toml(include_str!("../deploy/kueue-policy.toml")).unwrap();
    let kueue_version = crucible_kueue::kueue::KueueVersion::discover(&kube)
        .await
        .unwrap();
    let app = crucible_kueue::router(
        kube,
        crucible_kueue::Options {
            oidc: Arc::new(server),
            policy,
            reviewer: None,
            kueue: kueue_version,
            allowed_hosts: Vec::new(),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}{}", crucible_kueue::MCP_PATH)
}

async fn whoami(url: &str, token: &str) -> Value {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
    );
    let client: Client = ClientConfig::default().serve(transport).await.unwrap();
    let result = client
        .call_tool(CallToolRequestParams::new("whoami".to_string()))
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

async fn can_create_pods(kube: &kube::Client, user: &str, namespace: &str) -> bool {
    let review = SubjectAccessReview {
        spec: SubjectAccessReviewSpec {
            user: Some(user.to_string()),
            resource_attributes: Some(ResourceAttributes {
                namespace: Some(namespace.to_string()),
                verb: Some("create".into()),
                resource: Some("pods".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let api: Api<SubjectAccessReview> = Api::all(kube.clone());
    api.create(&Default::default(), &review)
        .await
        .unwrap()
        .status
        .is_some_and(|s| s.allowed)
}

#[tokio::test]
#[ignore = "needs a cluster prepared by `just gateway-e2e-setup`, its SPIRE, and a Postgres"]
async fn a_runs_own_gateway_creates_the_only_supervisor_that_gets_its_token() {
    let outcome = tokio::spawn(scenario()).await;
    cleanup().await;
    if let Err(failed) = outcome {
        std::panic::resume_unwind(failed.into_panic());
    }
}

/// Remove everything the scenario creates, whether or not it got that far.
async fn cleanup() {
    let env = env();
    let dir = tempfile::tempdir().unwrap();
    let (clusters, kube) = clusters(&env, dir.path()).await;
    let dispatcher = crucible_controller::KubePodDispatcher::new(clusters);
    let profile = crucible_controller::agent_auth::placement::load_profile(Some(&env.profile))
        .await
        .unwrap()
        .unwrap();
    let loop_ns = profile.cluster.loop_namespace.as_str();
    let sandbox_ns = profile.cluster.sandbox_namespace.as_ref().unwrap().as_str();
    let loop_pods: Api<Pod> = Api::namespaced(kube.clone(), loop_ns);
    let secrets: Api<Secret> = Api::namespaced(kube.clone(), loop_ns);
    for pod in [RUN_POD, "gateway-e2e-impostor"] {
        let _ = loop_pods.delete(pod, &Default::default()).await;
    }
    let _ = secrets
        .delete(&format!("{RUN_POD}-loop-tls"), &Default::default())
        .await;
    teardown(&dispatcher, &env.cluster, &profile, RUN_POD, loop_ns)
        .await
        .unwrap();
    gone(&kube, sandbox_ns, loop_ns).await;
}

async fn scenario() {
    let env = env();
    let dir = tempfile::tempdir().unwrap();
    let (clusters, kube) = clusters(&env, dir.path()).await;
    let dispatcher = crucible_controller::KubePodDispatcher::new(clusters.clone());
    let profile = crucible_controller::agent_auth::placement::load_profile(Some(&env.profile))
        .await
        .unwrap()
        .unwrap();
    let loop_ns = profile.cluster.loop_namespace.as_str();
    let sandbox_ns = profile.cluster.sandbox_namespace.as_ref().unwrap().as_str();
    let loop_sa = profile.cluster.service_account.as_str();
    let image = profile.image.loop_image.as_str();

    let pool = crucible_controller::connect(&env.database_url)
        .await
        .unwrap();
    seed_run(&pool, &env.cluster, RUN_POD, RUN_ID).await;
    crucible_controller::identity::oidc::users::record_login(
        &pool,
        "sub-alice",
        "alice",
        None,
        jiff::Timestamp::now(),
    )
    .await
    .unwrap();
    crucible_controller::identity::kube_identity::learn(
        &pool,
        "alice",
        &crucible_controller::identity::kube_identity::KubeUser::parse(&env.kube_user).unwrap(),
    )
    .await
    .unwrap();

    assert!(
        !can_create_pods(
            &kube,
            &format!("system:serviceaccount:{loop_ns}:{loop_sa}"),
            sandbox_ns
        )
        .await,
        "the loop service account must not create pods where supervisors run"
    );
    assert!(
        can_create_pods(
            &kube,
            &format!("system:serviceaccount:{sandbox_ns}:crucible-openshell-gateway"),
            sandbox_ns
        )
        .await,
        "the gateway account is the one that creates them"
    );

    let loop_pods: Api<Pod> = Api::namespaced(kube.clone(), loop_ns);
    let sandbox_pods: Api<Pod> = Api::namespaced(kube.clone(), sandbox_ns);
    let secrets: Api<Secret> = Api::namespaced(kube.clone(), loop_ns);
    let client_secret = format!("{RUN_POD}-loop-tls");
    let _ = loop_pods.delete(RUN_POD, &Default::default()).await;
    let _ = secrets.delete(&client_secret, &Default::default()).await;
    teardown(&dispatcher, &env.cluster, &profile, RUN_POD, loop_ns)
        .await
        .unwrap();
    gone(&kube, sandbox_ns, loop_ns).await;

    let gateway = RunGateway::reserve(&dispatcher, &env.cluster, &profile, RUN_POD, Some(3600))
        .await
        .unwrap();
    let tls = gateway.loop_client_tls();
    secrets
        .create(
            &Default::default(),
            &serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Secret",
                "metadata": {"name": client_secret, "namespace": loop_ns},
                "stringData": {"ca.crt": tls.ca, "tls.crt": tls.cert, "tls.key": tls.key},
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let stand_in = loop_pod(&profile, loop_ns, image, &gateway.url(), &client_secret);
    gateway
        .start(&dispatcher, &env.cluster, loop_ns, &stand_in, &[8849])
        .await
        .unwrap();
    dispatcher
        .create(&env.cluster, loop_ns, stand_in)
        .await
        .unwrap();

    let gateway_id_selector =
        format!("{GATEWAY_ID_LABEL}={RUN_POD},openshell.ai/boundary-role=supervisor");
    let supervisor = wait_for(
        "a supervisor of this run's gateway",
        Duration::from_secs(1200),
        || async {
            let pods = sandbox_pods
                .list(&ListParams::default().labels(&gateway_id_selector))
                .await
                .ok()?;
            pods.items
                .into_iter()
                .find(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        },
    )
    .await;
    let sandbox_id = supervisor
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("openshell.ai/sandbox-id"))
        .cloned()
        .unwrap();

    let svid = supervisor_svid(
        &sandbox_pods,
        &supervisor,
        &env.spire_agent_image,
        CONTROLLER,
    )
    .await;
    let spiffe_id = claims(&svid)["sub"].as_str().unwrap().to_string();
    let suffix = format!("/openshell/gateway/{RUN_POD}/sandbox/{sandbox_id}");
    assert!(spiffe_id.ends_with(&suffix), "{spiffe_id}");
    let gateway_prefix = spiffe_id.trim_end_matches(&format!("{RUN_POD}/sandbox/{sandbox_id}"));

    let controller = serve_controller(&pool, &env.profile, gateway_prefix, clusters.clone()).await;
    let directory = KubeDirectory(clusters.clone());
    let id = SandboxId::parse(&sandbox_id).unwrap();
    let name = directory
        .sandbox_name(&env.cluster, sandbox_ns, &id, RUN_POD)
        .await
        .unwrap()
        .expect("the run's gateway created this Sandbox");
    assert_eq!(
        directory
            .sandbox_name(&env.cluster, sandbox_ns, &id, OTHER_POD)
            .await
            .unwrap(),
        None,
        "another run's gateway did not"
    );
    bindings::bind(
        &pool,
        RUN_POD,
        &Binding {
            id: id.clone(),
            name: name.clone(),
            workdir: Some("/sandbox".into()),
        },
    )
    .await
    .unwrap();
    directory
        .adopt(&env.cluster, sandbox_ns, &id, RUN_POD)
        .await
        .unwrap();
    let gateway_pod = sandbox_pods.get(&format!("{RUN_POD}-gw")).await.unwrap();
    let sandboxes = sandbox_api(&kube, sandbox_ns)
        .list(&ListParams::default().labels(&format!("openshell.ai/sandbox-id={sandbox_id}")))
        .await
        .unwrap();
    assert!(
        sandboxes.items[0]
            .metadata
            .owner_references
            .iter()
            .flatten()
            .any(|o| Some(&o.uid) == gateway_pod.metadata.uid.as_ref()),
        "the bound Sandbox is owned by its gateway pod"
    );

    let (status, body) = exchange(&controller, &svid).await;
    assert_eq!(status, 200, "{body}");
    let token = body["access_token"].as_str().unwrap().to_string();
    let issued = claims(&token);
    assert_eq!(issued["sub"], "alice");
    assert_eq!(issued["act"]["sub"], spiffe_id);
    assert_eq!(issued["run_id"], RUN_POD);
    assert_eq!(issued["sandbox"], name);
    assert_eq!(issued["kube_user"], env.kube_user);

    let kueue = serve_kueue(kube.clone(), &controller).await;
    let me = whoami(&kueue, &token).await;
    assert_eq!(me["launched_by"], "alice", "{me}");
    assert_eq!(me["run_id"], RUN_POD, "{me}");

    seed_run(&pool, &env.cluster, OTHER_POD, "gateway-e2e-other").await;
    sqlx::query("UPDATE run_sandboxes SET pod_name = $2, run_id = $3 WHERE sandbox_id = $1")
        .bind(&sandbox_id)
        .bind(OTHER_POD)
        .bind("gateway-e2e-other")
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = exchange(&controller, &svid).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("invalid_grant")),
        "a binding to another run is refused: this SVID names this run's gateway"
    );

    let impostors: Api<Pod> = Api::namespaced(kube.clone(), loop_ns);
    let _ = impostors
        .delete("gateway-e2e-impostor", &Default::default())
        .await;
    wait_for(
        "the old impostor to go",
        Duration::from_secs(60),
        || async {
            impostors
                .get_opt("gateway-e2e-impostor")
                .await
                .ok()?
                .is_none()
                .then_some(())
        },
    )
    .await;
    impostors
        .create(
            &Default::default(),
            &impostor_pod(loop_ns, &env.spire_agent_image, &sandbox_id),
        )
        .await
        .unwrap();
    let phase = wait_for(
        "the impostor to finish",
        Duration::from_secs(120),
        || async {
            let phase = impostors
                .get("gateway-e2e-impostor")
                .await
                .ok()?
                .status?
                .phase?;
            matches!(phase.as_str(), "Succeeded" | "Failed").then_some(phase)
        },
    )
    .await;
    let logs = impostors
        .logs("gateway-e2e-impostor", &LogParams::default())
        .await
        .unwrap_or_default();
    assert_eq!(
        phase, "Failed",
        "a supervisor look-alike outside the sandbox namespace: {logs}"
    );
    assert!(!logs.contains("\"svid\""), "{logs}");
    impostors
        .delete("gateway-e2e-impostor", &Default::default())
        .await
        .unwrap();

    loop_pods
        .delete(RUN_POD, &Default::default())
        .await
        .unwrap();
    secrets
        .delete(&client_secret, &Default::default())
        .await
        .unwrap();
    teardown(&dispatcher, &env.cluster, &profile, RUN_POD, loop_ns)
        .await
        .unwrap();
    gone(&kube, sandbox_ns, loop_ns).await;
    for pod in [RUN_POD, OTHER_POD] {
        sqlx::query("UPDATE work_pods SET state = 'collected' WHERE pod_name = $1")
            .bind(pod)
            .execute(&pool)
            .await
            .unwrap();
    }
}

/// Wait until nothing of this run's gateway is left: its pod, what it created, and the loop pod.
async fn gone(kube: &kube::Client, sandbox_ns: &str, loop_ns: &str) {
    let sandbox_pods: Api<Pod> = Api::namespaced(kube.clone(), sandbox_ns);
    let loop_pods: Api<Pod> = Api::namespaced(kube.clone(), loop_ns);
    wait_for(
        "the gateway and everything it made to go",
        Duration::from_secs(300),
        || async {
            let supervisors = sandbox_pods
                .list(&ListParams::default().labels(&format!("{GATEWAY_ID_LABEL}={RUN_POD}")))
                .await
                .ok()?;
            let gateway = sandbox_pods.get_opt(&format!("{RUN_POD}-gw")).await.ok()?;
            let stand_in = loop_pods.get_opt(RUN_POD).await.ok()?;
            let left = sandbox_api(kube, sandbox_ns)
                .list(&ListParams::default().labels(&format!("{GATEWAY_ID_LABEL}={RUN_POD}")))
                .await
                .ok()?;
            (supervisors.items.is_empty()
                && gateway.is_none()
                && stand_in.is_none()
                && left.items.is_empty())
            .then_some(())
        },
    )
    .await;
}
