//! Per-kind facts the server learns from the cluster: which served kinds are installed, and how
//! to find an object's pods whoever created it.

use crate::manifest::WorkloadKind;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::core::Selector;
use serde_json::Value;

/// The served kinds whose API the cluster serves. A kind whose group is missing is left out; a
/// discovery failure other than not-found is returned.
pub async fn installed(client: &kube::Client) -> Result<Vec<WorkloadKind>, kube::Error> {
    let mut out = Vec::new();
    for kind in WorkloadKind::ALL {
        let resources = match client.list_api_group_resources(kind.api_version()).await {
            Ok(list) => list,
            Err(kube::Error::Api(s)) if s.code == 404 => continue,
            Err(e) => return Err(e),
        };
        if resources.resources.iter().any(|r| r.name == kind.plural()) {
            out.push(kind);
        }
    }
    Ok(out)
}

fn equals(key: &str, value: &str) -> Selector {
    [(key.to_string(), value.to_string())].into_iter().collect()
}

/// The object's own `spec.selector`, unless it is empty and would select every pod.
fn spec_selector(object: &Value) -> Option<Selector> {
    let ls: LabelSelector =
        serde_json::from_value(object.pointer("/spec/selector")?.clone()).ok()?;
    let selector = Selector::try_from(ls).ok()?;
    (!selector.selects_all()).then_some(selector)
}

/// Selectors whose union is the object's pods: its own selector where it has one, else the
/// labels its controller puts on every pod it creates. `object` is the full object.
pub fn pod_selectors(kind: WorkloadKind, object: &Value) -> Vec<Selector> {
    let name = object
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        WorkloadKind::Job => vec![
            spec_selector(object).unwrap_or_else(|| equals("batch.kubernetes.io/job-name", name)),
        ],
        WorkloadKind::Deployment | WorkloadKind::StatefulSet => {
            spec_selector(object).into_iter().collect()
        }
        WorkloadKind::LeaderWorkerSet => vec![equals("leaderworkerset.sigs.k8s.io/name", name)],
        WorkloadKind::JobSet => vec![equals("jobset.sigs.k8s.io/jobset-name", name)],
        WorkloadKind::PyTorchJob => vec![equals("training.kubeflow.org/job-name", name)],
        WorkloadKind::RayJob => {
            let mut out = vec![equals("batch.kubernetes.io/job-name", name)];
            if let Some(cluster) = object
                .pointer("/status/rayClusterName")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
            {
                out.push(equals("ray.io/cluster", cluster));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::kinds::*;
    use kube::core::SelectorExt;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn shown(kind: WorkloadKind, object: Value) -> Vec<String> {
        pod_selectors(kind, &object)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_job_and_a_deployment_use_their_own_selector() {
        let job = json!({"metadata": {"name": "j"}, "spec": {"selector": {"matchLabels": {"batch.kubernetes.io/controller-uid": "u1"}}}});
        assert_eq!(
            shown(WorkloadKind::Job, job),
            ["batch.kubernetes.io/controller-uid=u1"]
        );
        let deploy = json!({"metadata": {"name": "d"}, "spec": {"selector": {
            "matchLabels": {"app": "web"},
            "matchExpressions": [{"key": "tier", "operator": "In", "values": ["a", "b"]}]
        }}});
        let selectors = pod_selectors(WorkloadKind::Deployment, &deploy);
        assert_eq!(selectors.len(), 1);
        let pod = |labels: &[(&str, &str)]| {
            labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        assert!(selectors[0].matches(&pod(&[("app", "web"), ("tier", "a")])));
        assert!(!selectors[0].matches(&pod(&[("app", "web"), ("tier", "c")])));
    }

    #[test]
    fn an_empty_selector_never_selects_the_whole_namespace() {
        let open = json!({"metadata": {"name": "d"}, "spec": {"selector": {}}});
        assert!(pod_selectors(WorkloadKind::Deployment, &open).is_empty());
        let job = json!({"metadata": {"name": "j"}, "spec": {}});
        assert_eq!(
            shown(WorkloadKind::Job, job),
            ["batch.kubernetes.io/job-name=j"]
        );
    }

    #[test]
    fn controller_labels_find_the_pods_of_kinds_without_a_selector() {
        let named = |n: &str| json!({"metadata": {"name": n}});
        assert_eq!(
            shown(WorkloadKind::LeaderWorkerSet, named("serve")),
            ["leaderworkerset.sigs.k8s.io/name=serve"]
        );
        assert_eq!(
            shown(WorkloadKind::JobSet, named("train")),
            ["jobset.sigs.k8s.io/jobset-name=train"]
        );
        assert_eq!(
            shown(WorkloadKind::PyTorchJob, named("ddp")),
            ["training.kubeflow.org/job-name=ddp"]
        );
        assert_eq!(
            shown(WorkloadKind::RayJob, named("r")),
            ["batch.kubernetes.io/job-name=r"]
        );
        assert_eq!(
            shown(
                WorkloadKind::RayJob,
                json!({"metadata": {"name": "r"}, "status": {"rayClusterName": "r-raycluster-x"}})
            ),
            [
                "batch.kubernetes.io/job-name=r",
                "ray.io/cluster=r-raycluster-x"
            ]
        );
    }
}
