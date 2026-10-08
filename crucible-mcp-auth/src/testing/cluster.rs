//! Cluster e2e helpers against pirate's auth stack (deploy/): recreate the spike pod for a fresh
//! JWT-SVID shaped like a sandbox supervisor's, and exchange it for an access token exactly as
//! OpenShell's provider token grant does.

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, LogParams, PostParams};
use serde_json::Value;
use std::time::{Duration, Instant};
use url::Url;

pub const REALM: &str = "https://keycloak.apps.openshift.nm.rdu4.corp.redhat.com/realms/crucible";
pub const SPIFFE_ID: &str = "spiffe://pirate.crucible/openshell/sandbox/auth-spike";
const SPIKE_POD: &str = "auth-spike";
const SPIKE_NAMESPACE: &str = "rhbk";
const SPIKE_YAML: &str = include_str!("../../deploy/spike.yaml");

/// An HTTP client for the realm. `ingress_ip` pins the realm's Route host when it has no DNS.
pub fn realm_http(ingress_ip: Option<std::net::IpAddr>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
    if let Some(ip) = ingress_ip {
        let host = Url::parse(REALM).unwrap().host_str().unwrap().to_string();
        builder = builder.resolve(&host, std::net::SocketAddr::new(ip, 443));
    }
    builder.build().unwrap()
}

/// `CRUCIBLE_E2E_INGRESS_IP`, when set.
pub fn ingress_ip() -> Option<std::net::IpAddr> {
    std::env::var("CRUCIBLE_E2E_INGRESS_IP")
        .ok()
        .map(|ip| ip.parse().unwrap())
}

/// Recreate the spike pod and return the JWT-SVID it fetched. The SVID's audience is the realm
/// issuer, which OpenShell's token grant derives from the token endpoint.
pub async fn fresh_svid(kube: &kube::Client) -> String {
    let pods: Api<Pod> = Api::namespaced(kube.clone(), SPIKE_NAMESPACE);
    let _ = pods.delete(SPIKE_POD, &DeleteParams::default()).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while pods.get_opt(SPIKE_POD).await.unwrap().is_some() {
        assert!(Instant::now() < deadline, "old spike pod never went away");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let pod: Pod = serde_norway::from_str(SPIKE_YAML).unwrap();
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

/// The supervisor's token grant: client_credentials with the SVID as a jwt-bearer assertion and
/// no client_id.
pub async fn exchange(http: &reqwest::Client, svid: &str) -> reqwest::Response {
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

/// A fresh SVID exchanged for an access token from the realm's run-auth-spike client.
pub async fn access_token(http: &reqwest::Client, kube: &kube::Client) -> String {
    let svid = fresh_svid(kube).await;
    let response = exchange(http, &svid).await;
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(
        status.is_success(),
        "token exchange failed: {status} {body}"
    );
    body["access_token"].as_str().unwrap().to_string()
}
