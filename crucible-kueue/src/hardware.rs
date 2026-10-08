//! Hardware behind a ResourceFlavor: its node selector joined with the live node inventory. A
//! flavor's labels are what Kueue enforces, so a GPU flavor with no labels is reported as able to
//! land on every GPU product in the cluster.

use crate::kueue::ResourceFlavor;
use k8s_openapi::api::core::v1::Node;
use kube_quantity::ParsedQuantity;
use serde::Serialize;
use std::collections::BTreeMap;

pub const GPU_RESOURCES: [&str; 2] = ["nvidia.com/gpu", "amd.com/gpu"];
const PRODUCT_LABELS: [&str; 2] = ["nvidia.com/gpu.product", "amd.com/gpu.product-name"];

#[derive(Debug, Clone, Serialize)]
pub struct ProductCount {
    pub product: String,
    pub nodes: usize,
    pub schedulable_nodes: usize,
    #[serde(serialize_with = "crate::quantity::serialize")]
    pub gpus: ParsedQuantity,
    #[serde(serialize_with = "crate::quantity::serialize")]
    pub schedulable_gpus: ParsedQuantity,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlavorHardware {
    pub flavor: String,
    pub provides: Vec<String>,
    pub node_labels: BTreeMap<String, String>,
    pub node_taints: Vec<serde_json::Value>,
    pub tolerations: Vec<serde_json::Value>,
    pub topology_name: Option<String>,
    pub matching_nodes: usize,
    pub gpu_products: Vec<ProductCount>,
    pub warnings: Vec<String>,
}

fn product(node: &Node) -> Option<String> {
    let labels = node.metadata.labels.as_ref()?;
    PRODUCT_LABELS.iter().find_map(|k| labels.get(*k).cloned())
}

fn gpus(node: &Node) -> ParsedQuantity {
    let alloc = node.status.as_ref().and_then(|s| s.allocatable.as_ref());
    GPU_RESOURCES
        .iter()
        .filter_map(|r| alloc.and_then(|a| a.get(*r)))
        .filter_map(|q| ParsedQuantity::try_from(q).ok())
        .fold(crate::quantity::zero(), |a, b| a + b)
}

fn schedulable(node: &Node) -> bool {
    !node
        .spec
        .as_ref()
        .and_then(|s| s.unschedulable)
        .unwrap_or(false)
}

fn matches(node: &Node, selector: &BTreeMap<String, String>) -> bool {
    let labels = node.metadata.labels.as_ref();
    selector
        .iter()
        .all(|(k, v)| labels.and_then(|l| l.get(k)) == Some(v))
}

pub fn describe(flavor: &ResourceFlavor, provides: &[String], nodes: &[Node]) -> FlavorHardware {
    let provides_gpu = provides.iter().any(|r| GPU_RESOURCES.contains(&r.as_str()));
    let zero = crate::quantity::zero();
    let matching: Vec<&Node> = nodes
        .iter()
        .filter(|n| matches(n, &flavor.spec.node_labels))
        .filter(|n| !provides_gpu || gpus(n) > zero)
        .collect();
    let mut products: BTreeMap<String, ProductCount> = BTreeMap::new();
    for node in matching.iter().filter(|n| gpus(n) > zero) {
        let name = product(node).unwrap_or_else(|| "unlabeled".into());
        let entry = products.entry(name.clone()).or_insert(ProductCount {
            product: name,
            nodes: 0,
            schedulable_nodes: 0,
            gpus: crate::quantity::zero(),
            schedulable_gpus: crate::quantity::zero(),
        });
        let count = gpus(node);
        entry.nodes += 1;
        entry.gpus = entry.gpus.clone() + count.clone();
        if schedulable(node) {
            entry.schedulable_nodes += 1;
            entry.schedulable_gpus = entry.schedulable_gpus.clone() + count;
        }
    }
    let gpu_products: Vec<ProductCount> = products.into_values().collect();
    let mut warnings = Vec::new();
    if provides_gpu && flavor.spec.node_labels.is_empty() {
        let names: Vec<&str> = gpu_products.iter().map(|p| p.product.as_str()).collect();
        warnings.push(format!(
            "flavor sets no nodeLabels, so GPU pods admitted under it can land on any GPU node ({}); add a nodeSelector or nodeAffinity to the pod template to pin hardware",
            names.join(", ")
        ));
    }
    if provides_gpu && matching.is_empty() {
        warnings.push("no GPU node matches this flavor's nodeLabels".into());
    }
    FlavorHardware {
        flavor: flavor.metadata.name.clone(),
        provides: provides.to_vec(),
        node_labels: flavor.spec.node_labels.clone(),
        node_taints: flavor.spec.node_taints.clone(),
        tolerations: flavor.spec.tolerations.clone(),
        topology_name: flavor.spec.topology_name.clone(),
        matching_nodes: matching.len(),
        gpu_products,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use crate::hardware::*;
    use serde_json::json;

    fn node(name: &str, product: &str, gpus: &str, cordoned: bool) -> Node {
        serde_json::from_value(json!({
            "metadata": {"name": name, "labels": {"nvidia.com/gpu.product": product}},
            "spec": {"unschedulable": cordoned},
            "status": {"allocatable": {"nvidia.com/gpu": gpus, "cpu": "64"}}
        }))
        .unwrap()
    }

    fn cpu_node(name: &str) -> Node {
        serde_json::from_value(json!({
            "metadata": {"name": name, "labels": {}},
            "status": {"allocatable": {"cpu": "32"}}
        }))
        .unwrap()
    }

    fn flavor(name: &str, labels: serde_json::Value) -> ResourceFlavor {
        serde_json::from_value(json!({"metadata": {"name": name}, "spec": {"nodeLabels": labels}}))
            .unwrap()
    }

    fn fleet() -> Vec<Node> {
        vec![
            node("a100-01", "NVIDIA-A100-SXM4-80GB", "8", false),
            node("a100-06", "NVIDIA-A100-SXM4-80GB", "7", false),
            node("b200-02", "NVIDIA-B200", "8", false),
            node("dgx", "NVIDIA-B200", "8", true),
            cpu_node("cpu-1"),
        ]
    }

    #[test]
    fn a_labeled_gpu_flavor_reports_only_its_product_and_counts_cordoned_nodes_apart() {
        let hw = describe(
            &flavor("b200", json!({"nvidia.com/gpu.product": "NVIDIA-B200"})),
            &["nvidia.com/gpu".into()],
            &fleet(),
        );
        assert_eq!(hw.matching_nodes, 2);
        assert_eq!(hw.gpu_products.len(), 1);
        let p = &hw.gpu_products[0];
        assert_eq!((p.nodes, p.schedulable_nodes), (2, 1));
        assert_eq!(
            (p.gpus.to_string(), p.schedulable_gpus.to_string()),
            ("16".into(), "8".into())
        );
        assert!(hw.warnings.is_empty());
    }

    #[test]
    fn an_unlabeled_gpu_flavor_warns_that_it_spans_every_product() {
        let hw = describe(
            &flavor("nvidia-gpu-flavor", json!({})),
            &["nvidia.com/gpu".into()],
            &fleet(),
        );
        assert_eq!(hw.gpu_products.len(), 2);
        assert_eq!(hw.matching_nodes, 4);
        assert!(
            hw.warnings[0].contains("NVIDIA-A100-SXM4-80GB, NVIDIA-B200"),
            "{:?}",
            hw.warnings
        );
        let a100 = &hw.gpu_products[0];
        assert_eq!(a100.gpus.to_string(), "15");
    }

    #[test]
    fn a_cpu_flavor_matches_every_node_and_reports_no_warning() {
        let hw = describe(
            &flavor("default-flavor", json!({})),
            &["cpu".into(), "memory".into()],
            &fleet(),
        );
        assert_eq!(hw.matching_nodes, 5);
        assert!(hw.warnings.is_empty());
    }

    #[test]
    fn a_gpu_flavor_whose_labels_match_nothing_says_so() {
        let hw = describe(
            &flavor("h100", json!({"nvidia.com/gpu.product": "NVIDIA-H100"})),
            &["nvidia.com/gpu".into()],
            &fleet(),
        );
        assert_eq!(hw.matching_nodes, 0);
        assert!(hw.warnings.iter().any(|w| w.contains("no GPU node")));
    }
}
