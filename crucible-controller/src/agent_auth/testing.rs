//! Test fixtures: generated P-256 keys, a loopback JWKS endpoint standing in for SPIRE's OIDC
//! discovery provider, SVIDs shaped like a sandbox supervisor's, and a running run pod to bind
//! sandboxes to.

use crate::agent_auth::issuer::{
    AgentIssuer, AgentIssuerConfig, IssuerUrl, SandboxIdPrefix, SigningKey,
};
use base64::Engine;
use crucible_mcp_auth::jwks::Jwks;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use url::Url;

pub const ISSUER: &str = "http://crucible-controller.tms.svc:8080";
pub const SVID_ISSUER: &str = "https://oidc-discovery.apps.example";
pub const PREFIX: &str = "spiffe://pirate.crucible/openshell/sandbox/";
pub const KUEUE: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";
pub const CLUSTER: &str = "pirate";

pub const PROFILE: &str = r#"
[cluster]
loop_namespace = "crucible-system"
rig_namespace = "rig"
service_account = "crucible-spoke"
supervisor_image = "registry.example.com/openshell-supervisor:latest"
[image]
loop = "registry.example.com/crucible-loop:latest"
pull_secret = "quay-pull"
[kueue]
image = "registry.example.com/crucible-kueue:latest"
cluster = "pirate"
namespace = "crucible-system"
tenant_namespace = "crucible-kueue-tenants"
workload_namespace = "crucible-kueue-e2e"
queues = ["crucible-e2e"]
record_writers = ["crucible-system/crucible-hub-dispatch"]
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

    /// A JWT-SVID's claims for `sandbox`, fetched with the controller as audience.
    pub fn claims(sandbox: &str) -> Value {
        json!({
            "iss": SVID_ISSUER,
            "aud": [ISSUER],
            "sub": format!("{PREFIX}{sandbox}"),
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

pub fn config(spire: &Spire, signing: &EcKey) -> AgentIssuerConfig {
    AgentIssuerConfig {
        issuer: IssuerUrl::parse(ISSUER).unwrap(),
        signing_key: SigningKey::from_pem(signing.pem().as_bytes()).unwrap(),
        svid_issuer: SVID_ISSUER.to_string(),
        svid_jwks_uri: spire.jwks_uri.clone(),
        svid_ca: None,
        sandbox_prefix: SandboxIdPrefix::parse(PREFIX).unwrap(),
        audiences: vec![KUEUE.to_string()],
    }
}

pub fn issuer(spire: &Spire, signing: &EcKey) -> AgentIssuer {
    AgentIssuer::new(
        config(spire, signing),
        Jwks::new(reqwest::Client::new(), spire.jwks_uri.clone()),
    )
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
