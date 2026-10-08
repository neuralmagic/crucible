//! Live tenant records: the ConfigMaps the controller writes into the tenants namespace, one per
//! run. The server only reads them; whoever can write ConfigMaps in that namespace can mint a
//! tenant, so it must be a namespace only the controller writes.

use crate::tenant::{RecordRejection, Registry};
use crucible_contract::kueue::{TENANT_LABEL, TENANT_LABEL_VALUE, TENANT_RECORD_KEY, TenantRecord};
use futures_util::StreamExt;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::Api;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("has no {key:?} data key")]
    Missing { key: &'static str },
    #[error("{key:?} is not a tenant record: {message}")]
    Invalid { key: &'static str, message: String },
}

/// The record a tenant ConfigMap carries.
pub fn decode(cm: &ConfigMap) -> Result<TenantRecord, DecodeError> {
    let text = cm
        .data
        .as_ref()
        .and_then(|d| d.get(TENANT_RECORD_KEY))
        .ok_or(DecodeError::Missing {
            key: TENANT_RECORD_KEY,
        })?;
    serde_json::from_str(text).map_err(|e| DecodeError::Invalid {
        key: TENANT_RECORD_KEY,
        message: e.to_string(),
    })
}

fn source(namespace: &str, cm: &ConfigMap) -> String {
    format!(
        "{namespace}/{}",
        cm.metadata.name.as_deref().unwrap_or("<unnamed>")
    )
}

/// Rebuild the registry's live records from every ConfigMap in `objects`.
pub async fn sync(
    objects: &[Arc<ConfigMap>],
    namespace: &str,
    registry: &Registry,
) -> Vec<RecordRejection> {
    let mut rejected = Vec::new();
    let mut decoded = Vec::with_capacity(objects.len());
    for cm in objects {
        let source = source(namespace, cm);
        match decode(cm) {
            Ok(record) => decoded.push((source, record)),
            Err(e) => rejected.push(RecordRejection {
                source,
                reason: e.to_string(),
            }),
        }
    }
    rejected.extend(
        registry
            .replace_live(decoded.iter().map(|(s, r)| (s.as_str(), r)))
            .await,
    );
    rejected
}

/// Keep the registry's live records in step with the tenants namespace until the process exits.
/// Until the first full list lands no live record authenticates.
pub async fn watch(client: kube::Client, namespace: String, registry: Arc<Registry>) {
    let api: Api<ConfigMap> = Api::namespaced(client, &namespace);
    let config = watcher::Config::default().labels(&format!("{TENANT_LABEL}={TENANT_LABEL_VALUE}"));
    let (reader, writer) = reflector::store();
    let mut stream = reflector::reflector(writer, watcher(api, config))
        .default_backoff()
        .boxed();
    let mut reported: BTreeSet<(String, String)> = BTreeSet::new();
    while let Some(event) = stream.next().await {
        match event {
            Err(error) => {
                tracing::warn!(%error, namespace, "tenant record watch failed; retrying");
            }
            Ok(watcher::Event::Init | watcher::Event::InitApply(_)) => {}
            Ok(watcher::Event::InitDone | watcher::Event::Apply(_) | watcher::Event::Delete(_)) => {
                reported = refresh(&reader, &namespace, &registry, reported).await;
            }
        }
    }
}

async fn refresh(
    reader: &Store<ConfigMap>,
    namespace: &str,
    registry: &Registry,
    reported: BTreeSet<(String, String)>,
) -> BTreeSet<(String, String)> {
    let rejected = sync(&reader.state(), namespace, registry).await;
    let now: BTreeSet<(String, String)> =
        rejected.into_iter().map(|r| (r.source, r.reason)).collect();
    for (source, reason) in now.difference(&reported) {
        tracing::warn!(source, reason, "tenant record refused");
    }
    let live = registry.live_len().await;
    tracing::info!(live, refused = now.len(), "tenant records synced");
    now
}

#[cfg(test)]
mod tests {
    use crate::records::*;
    use crate::tenant::Tenants;
    use crucible_contract::kueue::{TenantRole, token_digest};
    use std::collections::BTreeMap;

    fn cm(name: &str, data: Option<&str>) -> Arc<ConfigMap> {
        Arc::new(ConfigMap {
            metadata: kube::api::ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            data: data.map(|d| BTreeMap::from([(TENANT_RECORD_KEY.to_string(), d.to_string())])),
            ..Default::default()
        })
    }

    fn record_json(token: &str) -> String {
        serde_json::to_string(&TenantRecord {
            run_id: "run-1".into(),
            token_sha256: token_digest(token),
            namespace: "team-a".into(),
            role: TenantRole::Submitter,
            queues: vec!["gpu".into()],
            expires_at: None,
        })
        .unwrap()
    }

    #[test]
    fn a_configmap_without_the_record_key_or_with_bad_json_does_not_decode() {
        assert_eq!(
            decode(&cm("a", None)).unwrap_err(),
            DecodeError::Missing {
                key: TENANT_RECORD_KEY
            }
        );
        assert!(matches!(
            decode(&cm("a", Some("{"))).unwrap_err(),
            DecodeError::Invalid { .. }
        ));
    }

    #[tokio::test]
    async fn sync_admits_good_records_and_names_the_configmap_of_each_refusal() {
        let registry = Registry::new(Tenants::default());
        let objects = vec![
            cm("good", Some(&record_json("tok"))),
            cm("empty", None),
            cm("garbled", Some("not json")),
        ];
        let rejected = sync(&objects, "tenants", &registry).await;
        let sources: Vec<&str> = rejected.iter().map(|r| r.source.as_str()).collect();
        assert_eq!(sources, ["tenants/empty", "tenants/garbled"]);
        let caller = registry.authenticate(Some("Bearer tok"), 0).await.unwrap();
        assert_eq!(caller.namespace.as_str(), "team-a");
        let rejected = sync(&objects[1..], "tenants", &registry).await;
        assert_eq!(rejected.len(), 2);
        assert!(registry.authenticate(Some("Bearer tok"), 0).await.is_none());
    }
}
