//! A run pod's claim on the OpenShell sandboxes it creates. The engine binds each new sandbox to
//! its run at the controller, which issues agent tokens only to bound sandboxes.

use serde::{Deserialize, Serialize};

/// Body of `POST /api/pods/{pod}/sandboxes`, authenticated by the pod's ingest token.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxBinding {
    /// The gateway-assigned sandbox id, the last segment of the sandbox supervisor's SPIFFE ID.
    pub sandbox_id: String,
    /// The sandbox's name, a Kubernetes label value; the `sandbox` claim buildit acts on.
    pub sandbox_name: String,
    /// The absolute path the workspace is uploaded to inside the sandbox; the `workdir` claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
}

/// The bind endpoint for `pod`, relative to the controller's ingest base URL.
pub fn binding_path(pod: &str) -> String {
    format!("/api/pods/{pod}/sandboxes")
}
