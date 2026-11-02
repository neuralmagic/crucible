//! Test fixtures: generated P-256 keys, a loopback JWKS endpoint standing in for SPIRE's OIDC
//! discovery provider, SVIDs shaped like a sandbox supervisor's, and a running run pod to bind
//! sandboxes to.

use crate::agent_auth::bindings::Binding;
use crate::agent_auth::cluster_spire::ClusterSpire;
use crate::agent_auth::issuer::{AgentIssuer, AgentIssuerConfig, Audience, IssuerUrl, SigningKey};
use crate::agent_auth::sandbox::SandboxId;
use base64::Engine;
use crucible_mcp_auth::jwks::Jwks;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use url::Url;

pub const ISSUER: &str = "http://crucible-controller.tms.svc:8080";
pub const SVID_ISSUER: &str = "https://oidc-discovery.apps.example";
pub const PREFIX: &str = "spiffe://example.crucible/openshell/gateway/";
/// The run pod most tests bind their sandboxes to; its gateway created those sandboxes.
pub const POD: &str = "crucible-run-brave-otter";
pub const KUEUE: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";
pub const BUILDIT: &str = "http://host.openshell.internal:8849/mcp";
pub const CLUSTER: &str = "dev";
pub const SANDBOX_NAMESPACE: &str = "sandboxes";

pub const PROFILE: &str = r#"
[cluster]
loop_namespace = "crucible-system"
rig_namespace = "rig"
service_account = "crucible-spoke"
supervisor_image = "registry.example.com/openshell-supervisor:latest"
sandbox_driver = "kubernetes"
sandbox_namespace = "sandboxes"
[image]
loop = "registry.example.com/crucible-loop:latest"
pull_secret = "quay-pull"
[kueue]
image = "registry.example.com/crucible-kueue:latest"
cluster = "dev"
namespace = "crucible-system"
workload_namespace = "crucible-kueue-e2e"
kube_authz = "openshift"
[buildit]
cluster = "dev"
namespace = "builds"
url = "http://host.openshell.internal:8849/mcp"
"#;

pub fn now() -> u64 {
    jsonwebtoken::get_current_timestamp()
}

/// A freshly generated P-256 key, as PKCS#8.
pub struct EcKey(Vec<u8>);

impl EcKey {
    pub fn generate() -> Self {
        use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
        let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap();
        Self(pair.to_pkcs8v1().unwrap().as_ref().to_vec())
    }

    pub fn pem(&self) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.0);
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

    pub fn encoding(&self) -> EncodingKey {
        EncodingKey::from_ec_der(&self.0)
    }

    pub fn jwk(&self, kid: &str) -> Jwk {
        let _ = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default();
        let mut jwk = Jwk::from_encoding_key(&self.encoding(), Algorithm::ES256).unwrap();
        jwk.common.key_id = Some(kid.to_string());
        jwk
    }
}

/// Serve `set` at a loopback `/keys`.
pub async fn serve_jwks(set: JwkSet) -> Url {
    let app = axum::Router::new().route(
        "/keys",
        axum::routing::get(move || {
            let set = set.clone();
            async move { axum::Json(set) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Url::parse(&format!("http://{addr}/keys")).unwrap()
}

/// SPIRE's signing key and its JWKS endpoint.
pub struct Spire {
    pub key: EcKey,
    pub jwks_uri: Url,
}

impl Spire {
    pub async fn start() -> Self {
        let key = EcKey::generate();
        let jwks_uri = serve_jwks(JwkSet {
            keys: vec![key.jwk("spire")],
        })
        .await;
        Self { key, jwks_uri }
    }

    /// A JWT-SVID's claims for `sandbox`, created by [`POD`]'s gateway, fetched with the
    /// controller as audience.
    pub fn claims(sandbox: &str) -> Value {
        Self::claims_from(POD, sandbox)
    }

    /// A JWT-SVID's claims for `sandbox`, created by the gateway of the run whose pod is
    /// `run_pod`.
    pub fn claims_from(run_pod: &str, sandbox: &str) -> Value {
        let gateway = crate::agent_auth::issuer::GatewayId::of_run_pod(run_pod);
        json!({
            "iss": SVID_ISSUER,
            "aud": [ISSUER],
            "sub": format!("{PREFIX}{gateway}/sandbox/{sandbox}"),
            "iat": now(),
            "exp": now() + 300,
        })
    }

    pub fn svid(&self, claims: &Value) -> String {
        sign(&self.key, "spire", claims)
    }
}

pub fn sign(key: &EcKey, kid: &str, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(kid.to_string());
    jsonwebtoken::encode(&header, claims, &key.encoding()).unwrap()
}

pub fn config(signing: &EcKey) -> AgentIssuerConfig {
    AgentIssuerConfig {
        issuer: IssuerUrl::parse(ISSUER).unwrap(),
        signing_key: SigningKey::from_pem(signing.pem().as_bytes()).unwrap(),
        previous_key: None,
        token_ttl: Default::default(),
        spire: vec![cluster_spire()],
        audiences: vec![
            audience(CLUSTER, KUEUE, "kueue.read kueue.submit"),
            audience(CLUSTER, BUILDIT, "buildit.read buildit.build"),
        ],
    }
}

/// An audience for runs on `cluster` at `resource` with `scopes`, needing no `kube_user`.
pub fn audience(cluster: &str, resource: &str, scopes: &str) -> Audience {
    Audience {
        resource: resource.to_string(),
        cluster: cluster.to_string(),
        scopes: scopes.split_whitespace().map(str::to_string).collect(),
        requires_kube_user: false,
        namespaces: Vec::new(),
    }
}

/// [`CLUSTER`]'s SPIRE, issuing as [`SVID_ISSUER`] under [`PREFIX`].
pub fn cluster_spire() -> ClusterSpire {
    format!("{CLUSTER}={SVID_ISSUER} {PREFIX} spire/oidc-discovery:443")
        .parse()
        .unwrap()
}

/// An issuer trusting [`CLUSTER`]'s SPIRE, whose keys `spire` serves.
pub fn issuer(spire: &Spire, signing: &EcKey) -> AgentIssuer {
    AgentIssuer::new(config(signing), |_| {
        Jwks::new(reqwest::Client::new(), spire.jwks_uri.clone())
    })
}

/// A running run pod on [`CLUSTER`] with its run row, attributed to `attributed_to`.
pub async fn running_run(
    pool: &sqlx::PgPool,
    pod: &str,
    run_id: &str,
    attributed_to: Option<&str>,
) {
    crate::runs::work_pods::insert_work_pod(
        pool,
        &crate::runs::workpod::NewWorkPod {
            pod_name: pod.to_string(),
            kind: "run".to_string(),
            issue_key: None,
            state: crate::runs::workpod::WorkPodState::Running,
            cost_tag: "run".to_string(),
            cluster: CLUSTER.to_string(),
        },
    )
    .await
    .unwrap();
    crate::runs::store::insert_run(
        pool,
        &crate::NewRun {
            run_id: run_id.to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "running".to_string(),
            pod: Some(pod.to_string()),
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE runs SET attributed_to = $2 WHERE run_id = $1")
        .bind(run_id)
        .bind(attributed_to)
        .execute(pool)
        .await
        .unwrap();
    crate::runs::store::stamp_dispatcher(pool, run_id)
        .await
        .unwrap();
}

pub async fn set_pod_state(
    pool: &sqlx::PgPool,
    pod: &str,
    state: crate::runs::workpod::WorkPodState,
) {
    crate::runs::work_pods::set_work_pod_state(pool, pod, state, None, None)
        .await
        .unwrap();
}

/// A binding for sandbox `id`, named `ci-<id>`, its workspace at `/sandbox/repo`.
pub fn binding(id: &str) -> Binding {
    Binding {
        id: SandboxId::parse(id).unwrap(),
        name: format!("ci-{id}"),
        workdir: Some("/sandbox/repo".into()),
    }
}

/// The cluster API's answer for sandbox ids, as the gateways' `Sandbox` resources in
/// [`SANDBOX_NAMESPACE`] would give it: `ci-<id>` for every `(run pod, id)` in `known`, whose run's
/// gateway created it, nothing otherwise. Records every adoption.
pub struct StaticDirectory {
    pub known: Vec<(String, String)>,
    pub adopted: std::sync::Mutex<Vec<(String, String)>>,
}

impl crate::agent_auth::directory::SandboxDirectory for StaticDirectory {
    fn sandbox_name(
        &self,
        cluster: &str,
        namespace: &str,
        id: &SandboxId,
        run_pod: &str,
    ) -> crate::daemon::queue::BoxFuture<
        Result<Option<String>, crate::agent_auth::directory::DirectoryError>,
    > {
        let answer = (cluster == CLUSTER
            && namespace == SANDBOX_NAMESPACE
            && self
                .known
                .iter()
                .any(|(pod, k)| pod == run_pod && k == id.as_str()))
        .then(|| format!("ci-{id}"));
        Box::pin(async move { Ok(answer) })
    }

    fn adopt(
        &self,
        _cluster: &str,
        _namespace: &str,
        id: &SandboxId,
        run_pod: &str,
    ) -> crate::daemon::queue::BoxFuture<Result<(), crate::agent_auth::directory::DirectoryError>>
    {
        if let Ok(mut adopted) = self.adopted.lock() {
            adopted.push((run_pod.to_string(), id.to_string()));
        }
        Box::pin(async move { Ok(()) })
    }
}

/// The issuer state over `pool` with `profile_toml` as its deploy profile and `known`
/// `(run pod, sandbox id)` pairs in the directory, which is returned for inspection.
pub fn agent_auth(
    pool: &sqlx::PgPool,
    spire: &Spire,
    profile_toml: &str,
    known: &[(&str, &str)],
) -> (
    std::sync::Arc<crate::agent_auth::AgentAuth>,
    std::sync::Arc<StaticDirectory>,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("profile.toml");
    std::fs::write(&path, profile_toml).unwrap();
    let profile = std::sync::Arc::new(crucible::deploy::DeployProfile::load(&path).unwrap());
    let directory = std::sync::Arc::new(StaticDirectory {
        known: known
            .iter()
            .map(|(pod, id)| (pod.to_string(), id.to_string()))
            .collect(),
        adopted: Default::default(),
    });
    let described = config(&EcKey::generate()).serving(&profile.agent_servers().unwrap());
    let state = crate::agent_auth::AgentAuth {
        issuer: AgentIssuer::new(described, |_| {
            Jwks::new(reqwest::Client::new(), spire.jwks_uri.clone())
        }),
        pool: pool.clone(),
        profile,
        directory: directory.clone(),
        people: None,
    };
    (std::sync::Arc::new(state), directory)
}
