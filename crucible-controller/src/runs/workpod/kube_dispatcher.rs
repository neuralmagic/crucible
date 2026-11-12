use crate::runs::workpod::dispatcher::{PodDispatcher, TerminalState, TurnPhase};
use anyhow::{Context, Result};
use k8s_openapi::NamespaceResourceScope;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, PodStatus, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use std::collections::BTreeSet;
use std::time::Duration;

/// The production cluster boundary: create/watch/logs/delete a work pod over the kube API,
/// cluster-aware — every call goes through the shared per-cluster client registry
/// ([`crate::runs::clusters::ClusterClients`]). Reached only in-cluster; a test installs a fake, so
/// this is compile-tested + covered by an `#[ignore]`d live test.
pub struct KubePodDispatcher {
    clusters: std::sync::Arc<crate::runs::clusters::ClusterClients>,
}

impl KubePodDispatcher {
    pub fn new(clusters: std::sync::Arc<crate::runs::clusters::ClusterClients>) -> Self {
        Self { clusters }
    }

    async fn api(&self, cluster: &str, namespace: &str) -> Result<kube::Api<Pod>> {
        let client = self.clusters.client(cluster).await?;
        Ok(kube::Api::namespaced(client, namespace))
    }
}

#[async_trait::async_trait]
impl PodDispatcher for KubePodDispatcher {
    async fn create(&self, cluster: &str, namespace: &str, pod: Pod) -> Result<Pod> {
        let api = self.api(cluster, namespace).await?;
        api.create(&kube::api::PostParams::default(), &pod)
            .await
            .context("creating the turn pod")
    }

    async fn create_configmap(&self, cluster: &str, namespace: &str, cm: ConfigMap) -> Result<()> {
        let client = self.clusters.client(cluster).await?;
        let api: kube::Api<ConfigMap> = kube::Api::namespaced(client, namespace);
        api.create(&kube::api::PostParams::default(), &cm)
            .await
            .with_context(|| {
                let name = cm.metadata.name.as_deref().unwrap_or_default();
                format!("creating ConfigMap {namespace}/{name}")
            })?;
        Ok(())
    }

    async fn create_secret(
        &self,
        cluster: &str,
        namespace: &str,
        secret: k8s_openapi::api::core::v1::Secret,
    ) -> Result<()> {
        let client = self.clusters.client(cluster).await?;
        let api: kube::Api<k8s_openapi::api::core::v1::Secret> =
            kube::Api::namespaced(client, namespace);
        api.create(&kube::api::PostParams::default(), &secret)
            .await
            .context("creating the run Secret")?;
        Ok(())
    }

    async fn set_secret_value(
        &self,
        cluster: &str,
        namespace: &str,
        secret: &str,
        key: &str,
        value: &str,
    ) -> Result<()> {
        let client = self.clusters.client(cluster).await?;
        let api: kube::Api<Secret> = kube::Api::namespaced(client, namespace);
        let patch = serde_json::json!({ "stringData": { key: value } });
        api.patch(
            secret,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(&patch),
        )
        .await
        .with_context(|| format!("patching Secret {namespace}/{secret}"))?;
        Ok(())
    }

    async fn create_service(
        &self,
        cluster: &str,
        namespace: &str,
        service: Service,
    ) -> Result<Service> {
        let client = self.clusters.client(cluster).await?;
        let api: kube::Api<Service> = kube::Api::namespaced(client, namespace);
        api.create(&kube::api::PostParams::default(), &service)
            .await
            .with_context(|| {
                let name = service.metadata.name.as_deref().unwrap_or_default();
                format!("creating Service {namespace}/{name}")
            })
    }

    async fn create_network_policy(
        &self,
        cluster: &str,
        namespace: &str,
        policy: NetworkPolicy,
    ) -> Result<()> {
        let client = self.clusters.client(cluster).await?;
        let api: kube::Api<NetworkPolicy> = kube::Api::namespaced(client, namespace);
        api.create(&kube::api::PostParams::default(), &policy)
            .await
            .with_context(|| {
                let name = policy.metadata.name.as_deref().unwrap_or_default();
                format!("creating NetworkPolicy {namespace}/{name}")
            })?;
        Ok(())
    }

    async fn delete_selected(&self, cluster: &str, namespace: &str, selector: &str) -> Result<()> {
        let client = self.clusters.client(cluster).await?;
        delete_selected_of::<NetworkPolicy>(&client, namespace, selector).await?;
        delete_selected_of::<Pod>(&client, namespace, selector).await?;
        delete_selected_of::<Service>(&client, namespace, selector).await?;
        delete_selected_of::<Secret>(&client, namespace, selector).await
    }

    async fn delete_services(&self, cluster: &str, namespace: &str, selector: &str) -> Result<()> {
        let client = self.clusters.client(cluster).await?;
        delete_selected_of::<Service>(&client, namespace, selector).await
    }

    async fn delete_sandboxes(&self, cluster: &str, namespace: &str, selector: &str) -> Result<()> {
        let client = self.clusters.client(cluster).await?;
        for version in crate::agent_auth::directory::SANDBOX_VERSIONS {
            let api = crate::agent_auth::directory::sandbox_api(&client, namespace, version);
            let found = match api
                .list_metadata(&kube::api::ListParams::default().labels(selector))
                .await
            {
                Ok(found) => found,
                Err(kube::Error::Api(s)) if s.code == 404 => continue,
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("listing Sandboxes in {namespace} matching {selector}")
                    });
                }
            };
            for name in found.items.into_iter().filter_map(|o| o.metadata.name) {
                match api
                    .delete(&name, &kube::api::DeleteParams::background())
                    .await
                {
                    Ok(_) => {}
                    Err(kube::Error::Api(s)) if s.code == 404 => {}
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("deleting Sandbox {namespace}/{name}"));
                    }
                }
            }
            return Ok(());
        }
        Ok(())
    }

    async fn selected_label_values(
        &self,
        cluster: &str,
        namespace: &str,
        selector: &str,
        label: &str,
    ) -> Result<BTreeSet<String>> {
        let client = self.clusters.client(cluster).await?;
        let mut values = BTreeSet::new();
        values.extend(label_values_of::<Pod>(&client, namespace, selector, label).await?);
        values.extend(label_values_of::<Service>(&client, namespace, selector, label).await?);
        values.extend(label_values_of::<Secret>(&client, namespace, selector, label).await?);
        values.extend(label_values_of::<NetworkPolicy>(&client, namespace, selector, label).await?);
        Ok(values)
    }

    async fn await_terminal(
        &self,
        cluster: &str,
        namespace: &str,
        name: &str,
        timeout: Duration,
    ) -> Result<TerminalState> {
        let api = self.api(cluster, namespace).await?;
        let deadline = std::time::Instant::now() + timeout;
        let mut backoff = crate::runs::workpod::retry::POLL_BACKOFF_START;
        loop {
            match api.get(name).await {
                Ok(pod) => {
                    backoff = crate::runs::workpod::retry::POLL_BACKOFF_START;
                    let status = pod.status.as_ref();
                    match status.and_then(|s| s.phase.as_deref()) {
                        Some("Succeeded") => {
                            return Ok(TerminalState {
                                phase: TurnPhase::Succeeded,
                                message: terminated_message(status),
                            });
                        }
                        Some("Failed") => {
                            return Ok(TerminalState {
                                phase: TurnPhase::Failed,
                                message: terminated_message(status),
                            });
                        }
                        _ => {}
                    }
                    if std::time::Instant::now() >= deadline {
                        return Ok(TerminalState {
                            phase: TurnPhase::TimedOut,
                            message: status.and_then(waiting_message),
                        });
                    }
                }
                // Transient failures re-poll with backoff until the caller's deadline;
                // 401/403 and 404 are definitive and return an error immediately.
                Err(e) => match crate::runs::workpod::retry::classify(&e) {
                    crate::runs::workpod::retry::KubeFailure::Transient => {
                        if std::time::Instant::now() + backoff >= deadline {
                            return Err(anyhow::Error::from(e).context(format!(
                                "cluster `{cluster}` unreachable polling the turn pod"
                            )));
                        }
                        tracing::warn!(cluster, pod = name, error = %e, "turn-pod poll failed (transient), backing off");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(crate::runs::workpod::retry::POLL_BACKOFF_MAX);
                        continue;
                    }
                    _ => return Err(anyhow::Error::from(e).context("polling the turn pod")),
                },
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn logs(&self, cluster: &str, namespace: &str, name: &str) -> Result<String> {
        let api = self.api(cluster, namespace).await?;
        api.logs(name, &kube::api::LogParams::default())
            .await
            .context("reading the turn pod's logs")
    }

    async fn delete(&self, cluster: &str, namespace: &str, name: &str) -> Result<()> {
        let api = self.api(cluster, namespace).await?;
        api.delete(name, &kube::api::DeleteParams::default())
            .await
            .context("deleting the turn pod")?;
        Ok(())
    }

    async fn pod_namespace(&self, cluster: &str, hub_namespace: &str) -> Result<String> {
        self.clusters.pod_namespace(cluster, hub_namespace).await
    }
}

/// Delete every `K` in `namespace` matching `selector`, one by one: not every kind serves
/// `deletecollection`.
async fn delete_selected_of<K>(client: &kube::Client, namespace: &str, selector: &str) -> Result<()>
where
    K: kube::Resource<Scope = NamespaceResourceScope>
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug,
    K::DynamicType: Default,
{
    let api: kube::Api<K> = kube::Api::namespaced(client.clone(), namespace);
    let kind = K::kind(&K::DynamicType::default()).to_string();
    let found = api
        .list_metadata(&kube::api::ListParams::default().labels(selector))
        .await
        .with_context(|| format!("listing {kind} in {namespace} matching {selector}"))?;
    for name in found.items.into_iter().filter_map(|o| o.metadata.name) {
        match api
            .delete(&name, &kube::api::DeleteParams::background())
            .await
        {
            Ok(_) => {}
            Err(kube::Error::Api(s)) if s.code == 404 => {}
            Err(e) => {
                return Err(e).with_context(|| format!("deleting {kind} {namespace}/{name}"));
            }
        }
    }
    Ok(())
}

async fn label_values_of<K>(
    client: &kube::Client,
    namespace: &str,
    selector: &str,
    label: &str,
) -> Result<Vec<String>>
where
    K: kube::Resource<Scope = NamespaceResourceScope>
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug,
    K::DynamicType: Default,
{
    let api: kube::Api<K> = kube::Api::namespaced(client.clone(), namespace);
    let found = api
        .list_metadata(&kube::api::ListParams::default().labels(selector))
        .await
        .with_context(|| {
            let kind = K::kind(&K::DynamicType::default()).to_string();
            format!("listing {kind} in {namespace} matching {selector}")
        })?;
    Ok(found
        .items
        .into_iter()
        .filter_map(|o| o.metadata.labels?.remove(label))
        .collect())
}

fn waiting_message(status: &k8s_openapi::api::core::v1::PodStatus) -> Option<String> {
    status
        .container_statuses
        .as_ref()?
        .iter()
        .find_map(|container| {
            let waiting = container.state.as_ref()?.waiting.as_ref()?;
            let reason = waiting.reason.as_deref().unwrap_or("waiting");
            Some(
                match waiting
                    .message
                    .as_deref()
                    .filter(|message| !message.is_empty())
                {
                    Some(message) => format!("{reason}: {message}"),
                    None => reason.to_string(),
                },
            )
        })
}

/// The kubelet-captured termination message from pod status. Turn pods are single-container, so this
/// reads the first container status carrying a terminated state; an empty message (an OOM/eviction
/// that never wrote the file) collapses to `None`.
pub(crate) fn terminated_message(status: Option<&PodStatus>) -> Option<String> {
    status?
        .container_statuses
        .as_ref()?
        .iter()
        .find_map(|cs| cs.state.as_ref()?.terminated.as_ref()?.message.clone())
        .filter(|m| !m.trim().is_empty())
}
