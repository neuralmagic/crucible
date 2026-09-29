use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `[agent.resources]`: what the agent's sandbox is scheduled with. Only the `openshell` backend
/// runs a turn in a sandbox a scheduler places, so every other backend refuses a non-empty table.
/// The deployment's `[cluster.gpu_sandbox]` adds its own placement for GPU sandboxes on top.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxResources {
    /// GPUs the sandbox gets (`nvidia.com/gpu`); 0 is none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gpus: u32,
    /// CPU, requested and limited alike, as a Kubernetes quantity (`"8"`, `"500m"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Quantity>,
    /// Memory, requested and limited alike, as a Kubernetes quantity (`"32Gi"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Quantity>,
    /// Node labels the sandbox must land on, passed to the pod's `nodeSelector` as written
    /// (`"nvidia.com/gpu.product" = "NVIDIA-H100-80GB-HBM3"`). A key the deployment's GPU
    /// placement also sets keeps the deployment's value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl SandboxResources {
    pub fn is_empty(&self) -> bool {
        self.gpus == 0
            && self.cpu.is_none()
            && self.memory.is_none()
            && self.node_selector.is_empty()
    }
}

/// A Kubernetes resource quantity: a positive decimal with an optional SI or binary suffix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Quantity(String);

const SUFFIXES: [&str; 13] = [
    "m", "k", "M", "G", "T", "P", "E", "Ki", "Mi", "Gi", "Ti", "Pi", "Ei",
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "{0:?} is not a resource quantity: a positive number with an optional suffix \
     (m, k, M, G, T, P, E, Ki, Mi, Gi, Ti, Pi, Ei)"
)]
pub struct QuantityError(String);

impl Quantity {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Quantity {
    type Error = QuantityError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let digits_end = raw
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(raw.len());
        let (number, suffix) = raw.split_at(digits_end);
        let well_formed = !number.is_empty()
            && number.matches('.').count() <= 1
            && !number.starts_with('.')
            && !number.ends_with('.')
            && number.chars().any(|c| c.is_ascii_digit() && c != '0')
            && (suffix.is_empty() || SUFFIXES.contains(&suffix));
        if well_formed {
            Ok(Quantity(raw))
        } else {
            Err(QuantityError(raw))
        }
    }
}

impl From<Quantity> for String {
    fn from(q: Quantity) -> String {
        q.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quantity(raw: &str) -> Result<Quantity, QuantityError> {
        Quantity::try_from(raw.to_string())
    }

    #[test]
    fn quantities_take_a_positive_number_and_a_kubernetes_suffix() {
        for ok in ["8", "500m", "0.5", "32Gi", "1.5G", "100k", "2Ei"] {
            assert_eq!(quantity(ok).expect(ok).as_str(), ok);
        }
        for bad in [
            "", "0", "0.0", "Gi", ".5", "5.", "1.2.3", "32gb", "32 Gi", "-1", "1e3", "8cpu",
        ] {
            assert!(quantity(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn the_table_parses_typed_and_refuses_what_it_does_not_know() {
        let parsed: SandboxResources =
            toml::from_str("gpus = 2\ncpu = \"8\"\nmemory = \"32Gi\"").expect("parse");
        assert_eq!(parsed.gpus, 2);
        assert_eq!(parsed.cpu.as_ref().map(Quantity::as_str), Some("8"));
        assert_eq!(parsed.memory.as_ref().map(Quantity::as_str), Some("32Gi"));
        assert!(!parsed.is_empty());

        assert!(
            SandboxResources::default().is_empty(),
            "an absent table asks for nothing"
        );
        assert!(toml::from_str::<SandboxResources>("memory = \"32GB\"").is_err());
        assert!(toml::from_str::<SandboxResources>("gpus = -1").is_err());
        assert!(
            toml::from_str::<SandboxResources>("\"nvidia.com/gpu\" = 1").is_err(),
            "device-plugin names are the deployment's, not the pack's"
        );

        let placed: SandboxResources = toml::from_str(
            "[node_selector]\n\"nvidia.com/gpu.product\" = \"NVIDIA-H100-80GB-HBM3\"",
        )
        .expect("parse");
        assert_eq!(
            placed.node_selector["nvidia.com/gpu.product"],
            "NVIDIA-H100-80GB-HBM3"
        );
        assert!(!placed.is_empty(), "a selector alone is a request");
    }

    #[test]
    fn it_round_trips_through_json_without_empty_fields() {
        let only_gpus = SandboxResources {
            gpus: 1,
            ..SandboxResources::default()
        };
        assert_eq!(
            serde_json::to_value(&only_gpus).expect("json"),
            serde_json::json!({"gpus": 1})
        );
        assert_eq!(
            serde_json::to_value(SandboxResources::default()).expect("json"),
            serde_json::json!({})
        );
        let back: SandboxResources =
            serde_json::from_value(serde_json::json!({"gpus": 1, "memory": "4Gi"})).expect("back");
        assert_eq!(back.memory.as_ref().map(Quantity::as_str), Some("4Gi"));
    }
}
