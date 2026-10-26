//! Where each platform's SPIRE installer puts what the controller reads: the OIDC discovery
//! Service its JWT keys come through, and the resources naming its trust domain and JWT issuer.
//! The controller discovers a cluster's SPIRE from these, and the render grants it the reads.

use forge::fleet::Platform;

/// The OIDC discovery Service SPIRE's JWT keys are read through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryLocation {
    pub namespace: &'static str,
    pub service: &'static str,
    pub port: u16,
    pub tls: bool,
}

impl DiscoveryLocation {
    /// The Service name the API server's proxy takes, `<scheme>:<service>:<port>`.
    pub fn proxy_name(&self) -> String {
        let scheme = match self.tls {
            true => "https",
            false => "http",
        };
        format!("{scheme}:{}:{}", self.service, self.port)
    }
}

/// Where the platform's SPIRE installer runs the OIDC discovery provider: the Zero Trust
/// Workload Identity Manager on OpenShift, the upstream Helm chart elsewhere.
pub fn discovery_location(platform: Platform) -> DiscoveryLocation {
    match platform {
        Platform::Openshift => DiscoveryLocation {
            namespace: "zero-trust-workload-identity-manager",
            service: "spire-spiffe-oidc-discovery-provider",
            port: 443,
            tls: true,
        },
        Platform::Kubernetes => DiscoveryLocation {
            namespace: "spire-server",
            service: "spire-spiffe-oidc-discovery-provider",
            port: 80,
            tls: false,
        },
    }
}

/// The Zero Trust Workload Identity Manager's API group and version, on OpenShift.
pub const OPERATOR_GROUP: &str = "operator.openshift.io";
pub const OPERATOR_VERSION: &str = "v1alpha1";
/// Its cluster-scoped resources naming the trust domain (`spec.trustDomain`) and the JWT issuer
/// (`spec.jwtIssuer`), each named [`OPERATOR_OBJECT`]: kind and plural.
pub const TRUST_DOMAIN_KIND: (&str, &str) = (
    "ZeroTrustWorkloadIdentityManager",
    "zerotrustworkloadidentitymanagers",
);
pub const JWT_ISSUER_KIND: (&str, &str) =
    ("SpireOIDCDiscoveryProvider", "spireoidcdiscoveryproviders");
pub const OPERATOR_OBJECT: &str = "cluster";
/// The upstream chart's ConfigMap whose `server.conf` names `server.trust_domain` and
/// `server.jwt_issuer`, in the discovery namespace.
pub const SERVER_CONFIGMAP: &str = "spire-server";
