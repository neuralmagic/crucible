//! The per-run Secret a dispatched pod mounts, per ADR-0036.
//!
//! The hub reads the bound values from Vault at dispatch and writes them into one Secret in the
//! target namespace, owner-referenced to the pod so the cluster collects it with the turn. The pod
//! spec names the Secret and nothing else about it: kubelet does the projection, which is why
//! there is no in-pod client and no endpoint to reach.

use crate::secrets::credentials::Credentials;
use crate::secrets::grant::GrantMint;
use crate::secrets::launch::{ProviderSecret, Refusal};
use crate::secrets::store::SecretRow;
use crate::secrets::{ProjectionKind, Visibility};
use anyhow::Context as _;
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;

/// Where a file projection's Secret volume is mounted from. One volume, one `subPath` per file, so
/// a binding's absolute path is honored exactly as written rather than rehomed under a fixed dir.
const VOLUME: &str = "crucible-run-secrets";
/// Owner-only, and the volume kubelet backs it with is memory, not the workspace disk.
const MODE: i32 = 0o400;

/// Why a run's Secret could not be assembled.
#[derive(Debug, thiserror::Error)]
pub enum DeliverError {
    #[error("the binding for {name} projects to an empty path")]
    EmptyProjection { name: String },
    #[error("the binding for {name} projects to {path}, which is not an absolute path")]
    RelativePath { name: String, path: String },
    #[error("{first} and {second} both project to {path}")]
    Collision {
        first: String,
        second: String,
        path: String,
    },
    #[error(
        "{second} sets {var}, which {first} already holds; one of the two would be overwritten"
    )]
    EnvCollision {
        first: String,
        second: String,
        var: String,
    },
    #[error("two deliveries both claim Secret key {key}")]
    KeyCollision { key: String },
}

/// One run's secret delivery: the object to create, and how the pod refers to it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Delivery {
    /// The Secret's key/value pairs, keyed by declared name.
    pub data: BTreeMap<String, String>,
    /// Env var name to Secret key, for every env projection.
    pub env: Vec<(String, String)>,
    /// Absolute container path to Secret key, for every file projection.
    pub files: Vec<(String, String)>,
    /// Plain environment, no Secret behind it: a provider's endpoint and the wire API it speaks.
    pub plain_env: Vec<(String, String)>,
    /// The env projections the registry marked agent-visible: the agent runs the tool itself, or
    /// the token's scope makes it holding one acceptable. The loop relays exactly these into the
    /// sandbox, so naming what may cross rather than what may not leaves a forgotten list
    /// withholding every secret instead of leaking each one.
    pub agent_visible: Vec<String>,
}

impl Delivery {
    /// Whether a Secret object has to exist for this delivery. Plain env needs none.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Fold `other` in. A variable, a Secret key, or a path that both deliveries claim is refused
    /// rather than resolved by order: neither value is the one to silently keep.
    pub fn merge(&mut self, other: Delivery) -> Result<(), DeliverError> {
        let claimant = |vars: &[(String, String)], var: &str| -> Option<String> {
            vars.iter()
                .find(|(v, _)| v == var)
                .map(|(_, key)| key.clone())
        };
        for (var, key) in &other.env {
            if let Some(first) = claimant(&self.env, var).or_else(|| claimant(&self.plain_env, var))
            {
                return Err(DeliverError::EnvCollision {
                    first,
                    second: key.clone(),
                    var: var.clone(),
                });
            }
        }
        for (var, value) in &other.plain_env {
            if let Some(first) = claimant(&self.env, var) {
                return Err(DeliverError::EnvCollision {
                    first,
                    second: value.clone(),
                    var: var.clone(),
                });
            }
        }
        for key in other.data.keys() {
            if self.data.contains_key(key) {
                return Err(DeliverError::KeyCollision { key: key.clone() });
            }
        }
        for (path, key) in &other.files {
            if let Some((_, first)) = self.files.iter().find(|(p, _)| p == path) {
                return Err(DeliverError::Collision {
                    first: first.clone(),
                    second: key.clone(),
                    path: path.clone(),
                });
            }
        }
        self.data.extend(other.data);
        self.env.extend(other.env);
        self.files.extend(other.files);
        self.plain_env.extend(other.plain_env);
        self.agent_visible.extend(other.agent_visible);
        Ok(())
    }
}

/// The Secret's object name for a pod. Deterministic and derived from the pod's own name, the way
/// the pack ConfigMap's is, so no delete site has to look it up.
pub fn secret_name(pod_name: &str) -> String {
    format!("{pod_name}-secrets")
}

/// Assemble the delivery from the resolved bindings and the values already read from Vault.
/// `values` is parallel to `mints`: reading Vault is the caller's, so this stays pure and testable.
pub fn assemble(
    mints: &[GrantMint],
    rows: &[SecretRow],
    values: &[String],
) -> Result<Delivery, DeliverError> {
    let mut delivery = Delivery::default();
    let mut seen_paths: BTreeMap<String, String> = BTreeMap::new();
    for ((mint, row), value) in mints.iter().zip(rows).zip(values) {
        let name = mint.item.declared_name.to_string();
        let projection = mint.item.projection.trim();
        if projection.is_empty() {
            return Err(DeliverError::EmptyProjection { name });
        }
        delivery.data.insert(name.clone(), value.clone());
        match mint.item.projection_kind {
            ProjectionKind::Env => {
                delivery.env.push((projection.to_string(), name.clone()));
                if row.visibility == Visibility::AgentVisible {
                    delivery.agent_visible.push(projection.to_string());
                }
            }
            ProjectionKind::File => {
                if !projection.starts_with('/') {
                    return Err(DeliverError::RelativePath {
                        name,
                        path: projection.to_string(),
                    });
                }
                if let Some(first) = seen_paths.get(projection) {
                    return Err(DeliverError::Collision {
                        first: first.clone(),
                        second: name,
                        path: projection.to_string(),
                    });
                }
                seen_paths.insert(projection.to_string(), name.clone());
                delivery.files.push((projection.to_string(), name.clone()));
            }
        }
    }
    Ok(delivery)
}

/// The Secret key a provider credential lands in: the secret's registry name, then the variable,
/// so two providers' keys and a scope binding of the same name can never share one.
fn credential_key(secret: &str, var: &str) -> String {
    format!("{secret}.{var}")
}

/// Everything a resolved provider adds to a run: the endpoint it is reached at as plain
/// environment, and its credentials, read from Vault and expanded one Secret key and one variable
/// per entry. A provider that names no key contributes its endpoint alone; a deployment that cannot
/// read values at all is an error rather than a keyless launch, because rendering the flags and
/// withholding the credential would fail in the pod, minutes later and further from the cause.
pub async fn provider_delivery(
    pool: &sqlx::PgPool,
    reader: Option<&std::sync::Arc<dyn crate::secrets::provider::SecretProvider>>,
    provider: &crate::playbooks::providers::ModelProvider,
) -> anyhow::Result<Result<Delivery, Refusal>> {
    let mut delivery = Delivery {
        plain_env: provider.config_env(),
        ..Delivery::default()
    };
    let secret = match crate::secrets::launch::resolve_provider_secret(pool, provider).await? {
        Ok(Some(secret)) => secret,
        Ok(None) => return Ok(Ok(delivery)),
        Err(refusal) => return Ok(Err(refusal)),
    };
    let reader = reader.with_context(|| {
        format!(
            "provider {} spends secret {}, and this deployment can read no values",
            provider.id, secret.row.name
        )
    })?;
    let value = reader.value_of(&secret.row).await?;
    match expand_credentials(&secret, &value) {
        Ok(expanded) => {
            delivery.data.extend(expanded.data);
            delivery.env.extend(expanded.env);
        }
        Err(reason) => {
            return Ok(Err(Refusal::ProviderCredentials {
                provider: provider.id.clone(),
                name: secret.row.name.clone(),
                reason,
            }));
        }
    }
    Ok(Ok(delivery))
}

/// A provider secret's bytes as Secret keys and the variables that read them. Pure, so the shape
/// is testable without Vault.
pub fn expand_credentials(secret: &ProviderSecret, value: &str) -> Result<Delivery, String> {
    let creds = Credentials::parse(value).map_err(|e| e.to_string())?;
    let vars = creds.for_env(secret.key_env).map_err(|e| e.to_string())?;
    let mut delivery = Delivery::default();
    let name = secret.row.name.to_string();
    for (var, bytes) in vars {
        let key = credential_key(&name, &var);
        delivery.env.push((var, key.clone()));
        delivery.data.insert(key, bytes);
    }
    Ok(delivery)
}

/// The Secret object itself, owner-referenced to the pod that mounts it. `owner` comes from the
/// create response, so the reference carries the UID the cluster garbage-collects on.
pub fn secret_object(
    name: &str,
    namespace: &str,
    delivery: &Delivery,
    pod: &core::Pod,
) -> core::Secret {
    core::Secret {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            owner_references: pod_owner_reference(pod).map(|r| vec![r]),
            ..Default::default()
        },
        string_data: Some(delivery.data.clone()),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}

fn pod_owner_reference(
    pod: &core::Pod,
) -> Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference> {
    let name = pod.metadata.name.clone()?;
    let uid = pod.metadata.uid.clone().filter(|u| !u.is_empty())?;
    Some(
        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "v1".to_string(),
            kind: "Pod".to_string(),
            name,
            uid,
            controller: Some(true),
            block_owner_deletion: Some(true),
        },
    )
}

/// The env var naming the projections the loop may relay into the agent's sandbox. Must match
/// core's `openshell::run::AGENT_VISIBLE_ENV`.
pub const AGENT_VISIBLE_ENV: &str = "CRUCIBLE_AGENT_VISIBLE_ENV";

/// Point the pod at the Secret: a `secretKeyRef` per env projection, and one volume with a
/// `subPath` mount per file projection. Every main container gets them; init containers clone and
/// stage and have no business holding a credential.
pub fn stamp(pod: &mut core::Pod, secret: &str, delivery: &Delivery) {
    let Some(spec) = pod.spec.as_mut() else {
        return;
    };
    if !delivery.files.is_empty() {
        spec.volumes
            .get_or_insert_with(Default::default)
            .push(core::Volume {
                name: VOLUME.to_string(),
                secret: Some(core::SecretVolumeSource {
                    secret_name: Some(secret.to_string()),
                    default_mode: Some(MODE),
                    ..Default::default()
                }),
                ..Default::default()
            });
    }
    for container in spec.containers.iter_mut() {
        // What the loop may relay into the agent's sandbox. A plain value, not a secretKeyRef:
        // these are projection names, not secrets. Absent when nothing is agent-visible, which is
        // what makes the relay withhold everything by default.
        if !delivery.agent_visible.is_empty() {
            container
                .env
                .get_or_insert_with(Default::default)
                .push(core::EnvVar {
                    name: AGENT_VISIBLE_ENV.to_string(),
                    value: Some(delivery.agent_visible.join(",")),
                    value_from: None,
                });
        }
        for (var, value) in &delivery.plain_env {
            container
                .env
                .get_or_insert_with(Default::default)
                .push(core::EnvVar {
                    name: var.clone(),
                    value: Some(value.clone()),
                    value_from: None,
                });
        }
        for (var, key) in &delivery.env {
            container
                .env
                .get_or_insert_with(Default::default)
                .push(core::EnvVar {
                    name: var.clone(),
                    value: None,
                    value_from: Some(core::EnvVarSource {
                        secret_key_ref: Some(core::SecretKeySelector {
                            name: secret.to_string(),
                            key: key.clone(),
                            optional: Some(false),
                        }),
                        ..Default::default()
                    }),
                });
        }
        for (path, key) in &delivery.files {
            container
                .volume_mounts
                .get_or_insert_with(Default::default)
                .push(core::VolumeMount {
                    name: VOLUME.to_string(),
                    mount_path: path.clone(),
                    sub_path: Some(key.clone()),
                    read_only: Some(true),
                    ..Default::default()
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_delivery(entries: &[(&str, &str)]) -> Delivery {
        let mut d = Delivery::default();
        for (var, key) in entries {
            d.env.push((var.to_string(), key.to_string()));
            d.data.insert(key.to_string(), format!("value-of-{key}"));
        }
        d
    }

    /// Two deliveries fold into one; a variable, a Secret key, or a path both claim is refused
    /// rather than resolved by order, and plain env counts as a claim on the variable.
    #[test]
    fn merging_deliveries_refuses_every_kind_of_collision() {
        let mut base = env_delivery(&[("AUTORESEARCH_PR_TOKEN", "pr_token")]);
        base.merge(env_delivery(&[(
            "OPENAI_API_KEY",
            "openai_key.OPENAI_API_KEY",
        )]))
        .expect("distinct variables and keys fold");
        assert_eq!(base.env.len(), 2);
        assert_eq!(base.data.len(), 2);

        let same_var = base
            .clone()
            .merge(env_delivery(&[("OPENAI_API_KEY", "other.OPENAI_API_KEY")]))
            .expect_err("the variable is taken");
        assert!(
            matches!(same_var, DeliverError::EnvCollision { .. }),
            "{same_var}"
        );
        assert!(same_var.to_string().contains("already holds"), "{same_var}");

        let same_key = base
            .clone()
            .merge(env_delivery(&[("SOMETHING_ELSE", "pr_token")]))
            .expect_err("the Secret key is taken");
        assert!(
            matches!(same_key, DeliverError::KeyCollision { .. }),
            "{same_key}"
        );

        let plain_over_secret = base
            .clone()
            .merge(Delivery {
                plain_env: vec![("OPENAI_API_KEY".to_string(), "x".to_string())],
                ..Delivery::default()
            })
            .expect_err("plain env cannot shadow a secret's variable");
        assert!(matches!(
            plain_over_secret,
            DeliverError::EnvCollision { .. }
        ));

        let mut with_url = base.clone();
        with_url
            .merge(Delivery {
                plain_env: vec![("OPENAI_BASE_URL".to_string(), "http://vllm/v1".to_string())],
                ..Delivery::default()
            })
            .expect("a fresh plain variable folds");
        assert_eq!(with_url.plain_env.len(), 1);
    }

    /// Plain env is stamped as a literal value on every main container, beside the secretKeyRefs,
    /// and needs no Secret object.
    #[test]
    fn plain_env_is_stamped_as_a_value_and_needs_no_secret() {
        let delivery = Delivery {
            plain_env: vec![("OPENAI_BASE_URL".to_string(), "http://vllm/v1".to_string())],
            ..Delivery::default()
        };
        assert!(delivery.is_empty(), "no Secret to create");
        let mut pod = core::Pod {
            spec: Some(core::PodSpec {
                containers: vec![core::Container {
                    name: "run".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        stamp(&mut pod, "run-secrets", &delivery);
        let env = pod.spec.expect("spec").containers[0]
            .env
            .clone()
            .expect("env");
        let url = env
            .iter()
            .find(|v| v.name == "OPENAI_BASE_URL")
            .expect("the base URL");
        assert_eq!(url.value.as_deref(), Some("http://vllm/v1"));
        assert!(url.value_from.is_none());
    }
    use crate::secrets::SecretName;

    fn mint(name: &str, kind: ProjectionKind, projection: &str) -> GrantMint {
        GrantMint {
            item: crate::secrets::grant::GrantItem {
                secret_id: format!("id-{name}"),
                declared_name: SecretName::parse(name).unwrap(),
                projection_kind: kind,
                projection: projection.to_string(),
            },
            secret_name: SecretName::parse(name).unwrap(),
            owner: crate::authz::model::Principal::parse("user:wynn").unwrap(),
        }
    }

    fn row(visibility: Visibility) -> SecretRow {
        SecretRow {
            id: "id".to_string(),
            name: SecretName::parse("pr_token").unwrap(),
            owner: crate::authz::model::Principal::parse("user:wynn").unwrap(),
            kind: crate::secrets::SecretKind::Opaque,
            visibility,
            consumer: crate::secrets::ConsumerClass::Run,
            mode: crate::secrets::SecretMode::Managed,
            vault_path: "crucible/data/user/wynn/pr_token".to_string(),
            current_version: Some(1),
            created_by: None,
            created_at: "2026-08-29T00:00:00Z".to_string(),
            updated_at: "2026-08-29T00:00:00Z".to_string(),
        }
    }

    fn pod(uid: Option<&str>) -> core::Pod {
        core::Pod {
            metadata: ObjectMeta {
                name: Some("crucible-run-1".to_string()),
                uid: uid.map(str::to_string),
                ..Default::default()
            },
            spec: Some(core::PodSpec {
                containers: vec![core::Container {
                    name: "loop".to_string(),
                    ..Default::default()
                }],
                init_containers: Some(vec![core::Container {
                    name: "clone".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn env_and_file_projections_become_keys_of_one_secret() {
        let delivery = assemble(
            &[
                mint("pr_token", ProjectionKind::Env, "GH_TOKEN"),
                mint("registry", ProjectionKind::File, "/etc/quay/push.json"),
            ],
            &[row(Visibility::AgentVisible), row(Visibility::BrokerOnly)],
            &["t".to_string(), "{}".to_string()],
        )
        .unwrap();
        assert_eq!(delivery.data.get("pr_token").map(String::as_str), Some("t"));
        assert_eq!(
            delivery.env,
            vec![("GH_TOKEN".to_string(), "pr_token".to_string())]
        );
        assert_eq!(
            delivery.files,
            vec![("/etc/quay/push.json".to_string(), "registry".to_string())]
        );
    }

    /// The pod carries the allowlist as a plain value, and only for what is agent-visible. A
    /// delivery with nothing agent-visible sets no allowlist at all, which is what makes the
    /// loop-side relay withhold every secret rather than each one needing to be named.
    #[test]
    fn the_pod_carries_only_the_agent_visible_projections_as_the_allowlist() {
        let delivery = assemble(
            &[
                mint("push", ProjectionKind::Env, "PUSH_TOKEN"),
                mint("pr", ProjectionKind::Env, "GH_TOKEN"),
            ],
            &[row(Visibility::BrokerOnly), row(Visibility::AgentVisible)],
            &["p".to_string(), "t".to_string()],
        )
        .unwrap();
        let mut pod = pod(Some("uid-1"));
        stamp(&mut pod, "run-secrets", &delivery);
        let env = pod.spec.expect("spec").containers[0]
            .env
            .clone()
            .expect("env");

        let allow = env
            .iter()
            .find(|e| e.name == AGENT_VISIBLE_ENV)
            .expect("the allowlist");
        assert_eq!(allow.value.as_deref(), Some("GH_TOKEN"));
        assert!(allow.value_from.is_none(), "a name is not a secret");

        // Both still arrive as secretKeyRefs; the allowlist gates only the sandbox relay.
        for var in ["PUSH_TOKEN", "GH_TOKEN"] {
            assert!(
                env.iter().any(|e| e.name == var && e.value_from.is_some()),
                "{var} must reach the pod"
            );
        }
    }

    /// Nothing agent-visible means no allowlist, so the relay has nothing to act on.
    #[test]
    fn a_delivery_with_nothing_agent_visible_sets_no_allowlist() {
        let delivery = assemble(
            &[mint("push", ProjectionKind::Env, "PUSH_TOKEN")],
            &[row(Visibility::BrokerOnly)],
            &["p".to_string()],
        )
        .unwrap();
        let mut pod = pod(Some("uid-1"));
        stamp(&mut pod, "run-secrets", &delivery);
        let env = pod.spec.expect("spec").containers[0]
            .env
            .clone()
            .expect("env");
        assert!(!env.iter().any(|e| e.name == AGENT_VISIBLE_ENV));
    }

    /// Only an agent-visible projection is named, and a broker-only one is named nowhere: the loop
    /// relays what this list holds, so silence about a secret is what keeps it from the agent.
    #[test]
    fn only_an_agent_visible_env_projection_is_named_for_the_sandbox() {
        let delivery = assemble(
            &[
                mint("push", ProjectionKind::Env, "PUSH_TOKEN"),
                mint("pr", ProjectionKind::Env, "GH_TOKEN"),
            ],
            &[row(Visibility::BrokerOnly), row(Visibility::AgentVisible)],
            &["p".to_string(), "t".to_string()],
        )
        .unwrap();
        assert_eq!(delivery.agent_visible, vec!["GH_TOKEN".to_string()]);
        assert_eq!(
            delivery.env,
            vec![
                ("PUSH_TOKEN".to_string(), "push".to_string()),
                ("GH_TOKEN".to_string(), "pr".to_string()),
            ],
            "both still reach the pod; only the sandbox relay is gated"
        );
    }

    #[test]
    fn two_bindings_may_not_write_the_same_file() {
        let err = assemble(
            &[
                mint("a", ProjectionKind::File, "/etc/token"),
                mint("b", ProjectionKind::File, "/etc/token"),
            ],
            &[row(Visibility::AgentVisible), row(Visibility::AgentVisible)],
            &["1".to_string(), "2".to_string()],
        )
        .expect_err("one path, one writer");
        assert!(matches!(err, DeliverError::Collision { .. }), "{err:?}");
    }

    #[test]
    fn a_relative_file_projection_is_refused() {
        let err = assemble(
            &[mint("a", ProjectionKind::File, "etc/token")],
            &[row(Visibility::AgentVisible)],
            &["1".to_string()],
        )
        .expect_err("a relative mount path is not a location");
        assert!(matches!(err, DeliverError::RelativePath { .. }), "{err:?}");
    }

    #[test]
    fn the_secret_is_owned_by_the_pod_that_mounts_it() {
        let delivery = assemble(
            &[mint("pr_token", ProjectionKind::Env, "GH_TOKEN")],
            &[row(Visibility::AgentVisible)],
            &["t".to_string()],
        )
        .unwrap();
        let object = secret_object(
            "crucible-run-1-secrets",
            "autoresearch",
            &delivery,
            &pod(Some("uid-1")),
        );
        let owner = &object.metadata.owner_references.expect("owner-referenced")[0];
        assert_eq!(owner.kind, "Pod");
        assert_eq!(owner.uid, "uid-1");
        assert_eq!(owner.block_owner_deletion, Some(true));
    }

    /// Without a UID there is nothing to garbage-collect on, and an unowned Secret full of
    /// credentials outlives every run. The caller must treat this as a failed dispatch.
    #[test]
    fn a_pod_with_no_uid_yields_no_owner_reference() {
        let object = secret_object("s", "autoresearch", &Delivery::default(), &pod(None));
        assert!(object.metadata.owner_references.is_none());
    }

    #[test]
    fn stamping_reaches_main_containers_and_leaves_init_containers_alone() {
        let delivery = assemble(
            &[
                mint("pr_token", ProjectionKind::Env, "GH_TOKEN"),
                mint("registry", ProjectionKind::File, "/etc/quay/push.json"),
            ],
            &[row(Visibility::AgentVisible), row(Visibility::AgentVisible)],
            &["t".to_string(), "{}".to_string()],
        )
        .unwrap();
        let mut p = pod(Some("uid-1"));
        stamp(&mut p, "crucible-run-1-secrets", &delivery);
        let spec = p.spec.unwrap();

        let env = spec.containers[0].env.as_ref().expect("env stamped");
        let var = env
            .iter()
            .find(|e| e.name == "GH_TOKEN")
            .expect("the env var");
        assert!(var.value.is_none(), "the value never rides the spec");
        let selector = var
            .value_from
            .as_ref()
            .and_then(|f| f.secret_key_ref.as_ref())
            .expect("a secretKeyRef");
        assert_eq!(selector.name, "crucible-run-1-secrets");
        assert_eq!(selector.key, "pr_token");

        let mount = spec.containers[0]
            .volume_mounts
            .as_ref()
            .and_then(|m| m.first())
            .expect("the file mount");
        assert_eq!(mount.mount_path, "/etc/quay/push.json");
        assert_eq!(mount.sub_path.as_deref(), Some("registry"));

        let init = &spec.init_containers.expect("init containers")[0];
        assert!(init.env.is_none(), "an init container holds no credential");
        assert!(init.volume_mounts.is_none());
    }

    #[test]
    fn a_run_with_no_bindings_stamps_nothing() {
        let mut p = pod(Some("uid-1"));
        let delivery = assemble(&[], &[], &[]).unwrap();
        assert!(delivery.is_empty());
        stamp(&mut p, "unused", &delivery);
        let spec = p.spec.unwrap();
        assert!(spec.volumes.is_none());
        assert!(spec.containers[0].env.is_none());
    }
}
