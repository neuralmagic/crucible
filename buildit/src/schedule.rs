use std::collections::BTreeMap;

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams};

const GPU_RESOURCE: &str = "nvidia.com/gpu";
// keep the nodeAffinity values list sane on big clusters
const MAX_HINTS: usize = 50;

// best effort: any failure (RBAC, flaky API) means no hint, never a blocked build
pub async fn idle_nodes(client: kube::Client) -> Vec<String> {
    match try_idle_nodes(client).await {
        Ok(nodes) => nodes,
        Err(err) => {
            tracing::warn!("could not scout idle nodes ({err:#}); scheduling without hints");
            Vec::new()
        }
    }
}

async fn try_idle_nodes(client: kube::Client) -> Result<Vec<String>> {
    let nodes: Api<Node> = Api::all(client.clone());
    let pods: Api<Pod> = Api::all(client);
    let node_list = nodes
        .list(&ListParams::default())
        .await
        .context("listing nodes")?;
    let pod_list = pods
        .list(&ListParams::default())
        .await
        .context("listing pods cluster-wide")?;

    let mut gpus_in_use: BTreeMap<String, u64> = BTreeMap::new();
    for pod in &pod_list.items {
        let Some(spec) = &pod.spec else { continue };
        let Some(node) = &spec.node_name else {
            continue;
        };
        let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
        if !matches!(phase, Some("Running" | "Pending")) {
            continue;
        }
        let gpus: u64 = spec
            .containers
            .iter()
            .filter_map(|c| {
                let res = c.resources.as_ref()?;
                let quantity = res
                    .requests
                    .as_ref()
                    .and_then(|r| r.get(GPU_RESOURCE))
                    .or_else(|| res.limits.as_ref().and_then(|l| l.get(GPU_RESOURCE)))?;
                quantity.0.parse::<u64>().ok()
            })
            .sum();
        if gpus > 0 {
            *gpus_in_use.entry(node.clone()).or_default() += gpus;
        }
    }

    let mut idle = Vec::new();
    for node in node_list.items {
        let Some(name) = node.metadata.name else {
            continue;
        };
        let unschedulable = node
            .spec
            .as_ref()
            .and_then(|s| s.unschedulable)
            .unwrap_or(false);
        let ready = node
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|conds| {
                conds
                    .iter()
                    .any(|c| c.type_ == "Ready" && c.status == "True")
            });
        if ready && !unschedulable && gpus_in_use.get(&name).copied().unwrap_or(0) == 0 {
            idle.push(name);
        }
    }
    idle.truncate(MAX_HINTS);
    Ok(idle)
}
