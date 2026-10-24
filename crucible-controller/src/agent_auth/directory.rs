//! A sandbox's name, read from the OpenShell `Sandbox` resource the run's own gateway created for
//! it, by id. The run pod that binds a sandbox reports only the id; the name buildit acts on, and
//! the proof that its gateway made it, come from the cluster, never from the binder.

use crate::agent_auth::sandbox::SandboxId;
use crate::daemon::queue::BoxFuture;
use crate::runs::clusters::ClusterClients;
use kube::api::{Api, ApiResource, DynamicObject, ListParams};
use kube::core::GroupVersionKind;
use std::sync::Arc;

const SANDBOX_GROUP: &str = "agents.x-k8s.io";
/// The `Sandbox` versions tried, newest first; a cluster serves one of them.
pub(crate) const SANDBOX_VERSIONS: &[&str] = &["v1beta1", "v1alpha1"];
const SANDBOX_KIND: &str = "Sandbox";

/// The `Sandbox` API at `version` in `namespace`.
pub(crate) fn sandbox_api(
    client: &kube::Client,
    namespace: &str,
    version: &str,
) -> Api<DynamicObject> {
    let gvk = GroupVersionKind::gvk(SANDBOX_GROUP, version, SANDBOX_KIND);
    Api::namespaced_with(client.clone(), namespace, &ApiResource::from_gvk(&gvk))
}
const SANDBOX_ID_LABEL: &str = "openshell.ai/sandbox-id";
const SANDBOX_NAME_LABEL: &str = "openshell.ai/sandbox-name";

/// Selects the `Sandbox` with `id` that the gateway of the run whose pod is `run_pod` created.
pub(crate) fn lookup_selector(id: &SandboxId, run_pod: &str) -> String {
    let names = crucible::deploy::render::gateway::GatewayNames::for_run_pod(run_pod);
    format!(
        "{SANDBOX_ID_LABEL}={id},{}",
        crucible::deploy::render::gateway::gateway_created_selector(&names)
    )
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DirectoryError {
    #[error("cluster {cluster} could not be asked about sandbox {id}: {detail}")]
    Unreachable {
        cluster: String,
        id: String,
        detail: String,
    },
    #[error("{count} Sandbox resources in {namespace} carry sandbox id {id}")]
    Ambiguous {
        namespace: String,
        id: String,
        count: usize,
    },
    #[error("the Sandbox resource for {id} in {namespace} names no sandbox")]
    Unnamed { namespace: String, id: String },
}

/// Looks sandboxes up where their gateway created them.
pub trait SandboxDirectory: Send + Sync {
    /// The name of the sandbox with `id` in `namespace` on `cluster` that the gateway of the run
    /// whose pod is `run_pod` created, `None` when there is none.
    fn sandbox_name(
        &self,
        cluster: &str,
        namespace: &str,
        id: &SandboxId,
        run_pod: &str,
    ) -> BoxFuture<Result<Option<String>, DirectoryError>>;

    /// Owner-reference that sandbox's `Sandbox` resource to its gateway's pod, so deleting the
    /// gateway collects it.
    fn adopt(
        &self,
        cluster: &str,
        namespace: &str,
        id: &SandboxId,
        run_pod: &str,
    ) -> BoxFuture<Result<(), DirectoryError>>;
}

/// The directory over the controller's per-cluster API clients.
pub struct KubeDirectory(pub Arc<ClusterClients>);

impl SandboxDirectory for KubeDirectory {
    fn sandbox_name(
        &self,
        cluster: &str,
        namespace: &str,
        id: &SandboxId,
        run_pod: &str,
    ) -> BoxFuture<Result<Option<String>, DirectoryError>> {
        let clusters = self.0.clone();
        let (cluster, namespace, id) = (cluster.to_string(), namespace.to_string(), id.clone());
        let selector = lookup_selector(&id, run_pod);
        Box::pin(async move {
            let unreachable = |detail: String| DirectoryError::Unreachable {
                cluster: cluster.clone(),
                id: id.to_string(),
                detail,
            };
            let client = clusters
                .client(&cluster)
                .await
                .map_err(|e| unreachable(format!("{e:#}")))?;
            let params = ListParams::default().labels(&selector);
            let mut last = String::new();
            for version in SANDBOX_VERSIONS {
                let api = sandbox_api(&client, &namespace, version);
                match api.list(&params).await {
                    Ok(list) => return name_of(&namespace, &id, &list.items),
                    Err(kube::Error::Api(status)) if status.code == 404 => {
                        last = format!("{SANDBOX_GROUP}/{version} is not served");
                    }
                    Err(e) => return Err(unreachable(e.to_string())),
                }
            }
            Err(unreachable(last))
        })
    }

    fn adopt(
        &self,
        cluster: &str,
        namespace: &str,
        id: &SandboxId,
        run_pod: &str,
    ) -> BoxFuture<Result<(), DirectoryError>> {
        let clusters = self.0.clone();
        let (cluster, namespace, id) = (cluster.to_string(), namespace.to_string(), id.clone());
        let selector = lookup_selector(&id, run_pod);
        let gateway_pod =
            crucible::deploy::render::gateway::GatewayNames::for_run_pod(run_pod).gateway;
        Box::pin(async move {
            let unreachable = |detail: String| DirectoryError::Unreachable {
                cluster: cluster.clone(),
                id: id.to_string(),
                detail,
            };
            let client = clusters
                .client(&cluster)
                .await
                .map_err(|e| unreachable(format!("{e:#}")))?;
            let pods: Api<k8s_openapi::api::core::v1::Pod> =
                Api::namespaced(client.clone(), &namespace);
            let pod = pods
                .get(&gateway_pod)
                .await
                .map_err(|e| unreachable(format!("reading gateway pod {gateway_pod}: {e}")))?;
            let uid = pod
                .metadata
                .uid
                .ok_or_else(|| unreachable(format!("gateway pod {gateway_pod} has no uid")))?;
            let patch = kube::api::Patch::Merge(serde_json::json!({
                "metadata": {"ownerReferences": [{
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "name": gateway_pod,
                    "uid": uid,
                    "blockOwnerDeletion": false,
                }]}
            }));
            for version in SANDBOX_VERSIONS {
                let api = sandbox_api(&client, &namespace, version);
                let found = match api
                    .list_metadata(&ListParams::default().labels(&selector))
                    .await
                {
                    Ok(found) => found,
                    Err(kube::Error::Api(status)) if status.code == 404 => continue,
                    Err(e) => return Err(unreachable(e.to_string())),
                };
                for name in found.items.into_iter().filter_map(|o| o.metadata.name) {
                    api.patch(&name, &kube::api::PatchParams::default(), &patch)
                        .await
                        .map_err(|e| unreachable(format!("adopting Sandbox {name}: {e}")))?;
                }
                return Ok(());
            }
            Err(unreachable(format!("{SANDBOX_GROUP} is not served")))
        })
    }
}

/// The name the one Sandbox resource in `items` carries, as a label or an annotation.
pub(crate) fn name_of(
    namespace: &str,
    id: &SandboxId,
    items: &[DynamicObject],
) -> Result<Option<String>, DirectoryError> {
    let object = match items {
        [] => return Ok(None),
        [one] => one,
        many => {
            return Err(DirectoryError::Ambiguous {
                namespace: namespace.to_string(),
                id: id.to_string(),
                count: many.len(),
            });
        }
    };
    let meta = &object.metadata;
    let from = |map: &Option<std::collections::BTreeMap<String, String>>| {
        map.as_ref()
            .and_then(|m| m.get(SANDBOX_NAME_LABEL))
            .cloned()
    };
    from(&meta.labels)
        .or_else(|| from(&meta.annotations))
        .filter(|name| !name.is_empty())
        .map(Some)
        .ok_or_else(|| DirectoryError::Unnamed {
            namespace: namespace.to_string(),
            id: id.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::directory::*;
    use kube::api::ObjectMeta;
    use std::collections::BTreeMap;

    fn sandbox(labels: &[(&str, &str)], annotations: &[(&str, &str)]) -> DynamicObject {
        let map = |kv: &[(&str, &str)]| {
            (!kv.is_empty()).then(|| {
                kv.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<BTreeMap<_, _>>()
            })
        };
        DynamicObject {
            types: None,
            metadata: ObjectMeta {
                labels: map(labels),
                annotations: map(annotations),
                ..Default::default()
            },
            data: serde_json::Value::Null,
        }
    }

    #[test]
    fn a_lookup_finds_only_what_the_runs_own_gateway_created() {
        assert_eq!(
            lookup_selector(&SandboxId::parse("sbx-1").unwrap(), "crucible-run-a"),
            "openshell.ai/sandbox-id=sbx-1,openshell.ai/gateway-id=crucible-run-a"
        );
    }

    #[test]
    fn the_one_sandbox_resource_names_the_sandbox() {
        let id = SandboxId::parse("sbx-1").unwrap();
        assert_eq!(name_of("sandboxes", &id, &[]), Ok(None));
        assert_eq!(
            name_of(
                "sandboxes",
                &id,
                &[sandbox(&[(SANDBOX_NAME_LABEL, "ci-0123abcd")], &[])]
            ),
            Ok(Some("ci-0123abcd".into()))
        );
        assert_eq!(
            name_of(
                "sandboxes",
                &id,
                &[sandbox(&[], &[(SANDBOX_NAME_LABEL, "ci-from-annotation")])]
            ),
            Ok(Some("ci-from-annotation".into()))
        );
        assert!(matches!(
            name_of("sandboxes", &id, &[sandbox(&[], &[])]),
            Err(DirectoryError::Unnamed { .. })
        ));
        assert!(matches!(
            name_of(
                "sandboxes",
                &id,
                &[
                    sandbox(&[(SANDBOX_NAME_LABEL, "a")], &[]),
                    sandbox(&[(SANDBOX_NAME_LABEL, "b")], &[])
                ]
            ),
            Err(DirectoryError::Ambiguous { count: 2, .. })
        ));
    }
}
