//! Test fixtures: two real RSA keys and a JWKS endpoint on loopback whose key set tests swap.

use crate::jwks::Jwks;
use crate::server::{ResourceServer, ResourceServerConfig};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;
use url::Url;

pub const ISSUER: &str = "https://keycloak.example/realms/crucible";
pub const RESOURCE_SVC: &str = "http://kueue.crucible-system.svc.cluster.local:8850/mcp";
pub const RESOURCE_ROUTE: &str = "https://kueue.apps.example/mcp";

pub fn jwk(name: &str) -> Value {
    let all: Value = serde_json::from_str(include_str!("../../testdata/jwks.json")).unwrap();
    all[name].clone()
}

pub fn signing_key(name: &str) -> EncodingKey {
    let pem = match name {
        "a" => include_str!("../../testdata/rsa-a.pem"),
        "b" => include_str!("../../testdata/rsa-b.pem"),
        other => panic!("no key {other}"),
    };
    EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap()
}

pub fn now() -> i64 {
    jsonwebtoken::get_current_timestamp() as i64
}

/// Claims shaped like the realm's per-run client issues.
pub fn run_claims() -> Value {
    json!({
        "iss": ISSUER,
        "aud": [RESOURCE_SVC, RESOURCE_ROUTE],
        "sub": "weaton",
        "azp": "run-auth-spike",
        "exp": now() + 300,
        "iat": now(),
        "scope": "kueue.submit kueue.read",
        "act": {"sub": "spiffe://pirate.crucible/openshell/sandbox/auth-spike"},
        "run_id": "auth-spike",
        "namespace": "crucible-kueue-e2e",
        "queues": ["crucible-e2e"],
        "role": "submitter",
    })
}

pub fn sign(key: &str, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(format!("key-{key}"));
    jsonwebtoken::encode(&header, claims, &signing_key(key)).unwrap()
}

/// A loopback JWKS endpoint. `keys` is what it serves; `fetches` counts requests; `down` makes it
/// answer 503.
pub struct JwksServer {
    pub url: Url,
    pub keys: Arc<RwLock<Vec<Value>>>,
    pub fetches: Arc<AtomicUsize>,
    pub down: Arc<std::sync::atomic::AtomicBool>,
}

impl JwksServer {
    pub async fn start(keys: Vec<Value>) -> Self {
        let keys = Arc::new(RwLock::new(keys));
        let fetches = Arc::new(AtomicUsize::new(0));
        let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (k, f, d) = (keys.clone(), fetches.clone(), down.clone());
        let app = axum::Router::new().route(
            "/certs",
            axum::routing::get(move || {
                let (k, f, d) = (k.clone(), f.clone(), d.clone());
                async move {
                    f.fetch_add(1, Ordering::SeqCst);
                    if d.load(Ordering::SeqCst) {
                        return Err(http::StatusCode::SERVICE_UNAVAILABLE);
                    }
                    Ok(axum::Json(json!({"keys": *k.read().await})))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: Url::parse(&format!("http://{addr}/certs")).unwrap(),
            keys,
            fetches,
            down,
        }
    }

    pub fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

pub fn config(jwks_uri: Url) -> ResourceServerConfig {
    ResourceServerConfig {
        issuer: Url::parse(ISSUER).unwrap(),
        jwks_uri,
        resources: vec![
            Url::parse(RESOURCE_SVC).unwrap(),
            Url::parse(RESOURCE_ROUTE).unwrap(),
        ],
        required_scopes: vec!["kueue.read".into()],
        scopes_supported: vec!["kueue.read".into(), "kueue.submit".into()],
        resource_name: Some("crucible-kueue".into()),
    }
}

pub fn resource_server(
    jwks: &JwksServer,
    max_age: Duration,
    min_refetch: Duration,
) -> ResourceServer {
    let cfg = config(jwks.url.clone());
    let fetcher =
        Jwks::new(reqwest::Client::new(), cfg.jwks_uri.clone()).with_timing(max_age, min_refetch);
    ResourceServer::with_jwks(cfg, fetcher)
}

/// pirate cluster e2e: the spike pod and the realm token exchange.
#[cfg(feature = "e2e")]
pub mod cluster;
