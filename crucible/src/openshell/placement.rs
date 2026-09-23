//! Where a GPU sandbox is scheduled. The deploy profile's `[cluster.gpu_sandbox]` holds it, the
//! loop pod carries it as [`GPU_PLACEMENT_ENV`], and a sandbox that asks for GPUs is created with
//! it as its Kubernetes driver's pod config.

use k8s_openapi::api::core::v1::Toleration;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The loop-pod variable carrying [`GpuPlacement`] as JSON.
pub const GPU_PLACEMENT_ENV: &str = "CRUCIBLE_SANDBOX_GPU_POD";

/// Scheduling for sandboxes that request GPUs. Field names are the OpenShell Kubernetes driver's
/// pod config keys, so the value is handed over as is.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GpuPlacement {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<Toleration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_class_name: Option<String>,
}

impl GpuPlacement {
    pub fn is_empty(&self) -> bool {
        self.node_selector.is_empty()
            && self.tolerations.is_empty()
            && self.runtime_class_name.is_none()
    }
}
