//! Cluster e2e helpers against pirate's auth stack (deploy/): recreate the spike pod for a fresh
//! JWT-SVID shaped like a sandbox supervisor's, and exchange it for an access token exactly as
//! OpenShell's provider token grant does, at the realm or at any token endpoint.

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, LogParams, PostParams};
use serde_json::Value;
use std::time::{Duration, Instant};
use url::Url;

pub const REALM: &str = "https://keycloak.apps.openshift.nm.rdu4.corp.redhat.com/realms/crucible";
/// SPIRE's OIDC discovery issuer: the `iss` of every JWT-SVID; its JWKS is at `/keys`.
pub const SPIRE: &str = "https://oidc-discovery.apps.openshift.nm.rdu4.corp.redhat.com";
pub const SPIFFE_ID: &str = "spiffe://pirate.crucible/openshell/sandbox/auth-spike";
const SPIKE_POD: &str = "auth-spike";
const SPIKE_NAMESPACE: &str = "rhbk";
const SPIKE_YAML: &str = include_str!("../../deploy/spike.yaml");

/// An HTTP client for pirate's Routes (the realm, SPIRE's discovery provider). `ingress_ip` pins
/// their hosts when they have no DNS.
pub fn routes_http(ingress_ip: Option<std::net::IpAddr>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
    if let Some(ip) = ingress_ip {
        for route in [REALM, SPIRE] {
            let host = Url::parse(route).unwrap().host_str().unwrap().to_string();
            builder = builder.resolve(&host, std::net::SocketAddr::new(ip, 443));
        }
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
    fresh_svid_for(kube, REALM).await
}

/// [`fresh_svid`] with `audience` in place of the manifest's, as a supervisor configured with
/// `token_grant.jwt_svid_audience` requests it.
pub async fn fresh_svid_for(kube: &kube::Client, audience: &str) -> String {
    let pods: Api<Pod> = Api::namespaced(kube.clone(), SPIKE_NAMESPACE);
    let _ = pods.delete(SPIKE_POD, &DeleteParams::default()).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while pods.get_opt(SPIKE_POD).await.unwrap().is_some() {
        assert!(Instant::now() < deadline, "old spike pod never went away");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let mut pod: Pod = serde_norway::from_str(SPIKE_YAML).unwrap();
    let args = pod
        .spec
        .as_mut()
        .and_then(|s| s.containers.first_mut())
        .and_then(|c| c.command.as_mut())
        .unwrap();
    let at = args.iter().position(|a| a == "-audience").unwrap() + 1;
    args[at] = audience.to_string();
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

/// The supervisor's token grant at the realm: client_credentials with the SVID as a jwt-bearer
/// assertion and no client_id.
pub async fn exchange(http: &reqwest::Client, svid: &str) -> reqwest::Response {
    token_grant(
        http,
        &format!("{REALM}/protocol/openid-connect/token"),
        svid,
        &[],
    )
    .await
}

/// The supervisor's token grant at `token_endpoint`, with OpenShell's optional `audience` and
/// `scope` fields in `extra`.
pub async fn token_grant(
    http: &reqwest::Client,
    token_endpoint: &str,
    svid: &str,
    extra: &[(&str, &str)],
) -> reqwest::Response {
    let mut form = vec![
        ("grant_type", "client_credentials"),
        (
            "client_assertion_type",
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
        ),
        ("client_assertion", svid),
    ];
    form.extend_from_slice(extra);
    http.post(token_endpoint).form(&form).send().await.unwrap()
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
