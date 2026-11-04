//! Render the shared `buildit cache` from the profile's `[buildit.cache]` table: its
//! PersistentVolumeClaim, Deployment and Service in the loop namespace, and a NetworkPolicy that
//! admits only builder pods. A run on one of its clusters is told its URL, and whether it writes,
//! in its buildit site (`crate::deploy::servers`).

use crate::deploy::profile::{BuilditCacheCfg, BuilditCfg, DeployProfile};
use crate::deploy::render::{RenderOpts, pin_image};
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1 as apps;
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::api::networking::v1 as networking;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde::Serialize;
use std::collections::BTreeMap;

const NAME: &str = BuilditCacheCfg::NAME;
const PORT_NAME: &str = "http";
const APP_LABEL: &str = "app.kubernetes.io/name";
const DATA_DIR: &str = "/var/cache/buildit";
pub const TOKEN_DIR: &str = "/etc/buildit-cache";

#[derive(Debug, thiserror::Error)]
#[error("rendering the buildit cache needs a [buildit.cache] table in the deploy profile")]
pub struct MissingCacheTable;

/// The `[buildit]` table and its `cache`, when the profile has both.
pub fn configured(profile: &DeployProfile) -> Option<(&BuilditCfg, &BuilditCacheCfg)> {
    let buildit = profile.buildit.as_ref()?;
    Some((buildit, buildit.cache.as_ref()?))
}

/// The cache's objects as one multi-document YAML string.
pub fn render(profile: &DeployProfile, opts: &RenderOpts) -> Result<String> {
    let (buildit, cache) = configured(profile).ok_or(MissingCacheTable)?;
    let namespace = profile.cluster.loop_namespace.as_str();
    let image = cache
        .image
        .as_deref()
        .or(buildit.image.as_deref())
        .unwrap_or(&profile.image.loop_image);
    let image = pin_image(opts.digests.as_deref(), "buildit cache image", image)?;
    let docs = [
        yaml(&claim(cache, namespace)),
        yaml(&deployment(
            cache,
            namespace,
            &image,
            profile.image.pull_secret.as_str(),
        )),
        yaml(&service(namespace)),
        yaml(&netpol(namespace, buildit.namespace.as_str())),
    ];
    let docs: Vec<String> = docs
        .into_iter()
        .collect::<Result<_, _>>()
        .context("serializing the buildit cache")?;
    Ok(docs.join("---\n"))
}

fn yaml<T: Serialize>(object: &T) -> Result<String, serde_norway::Error> {
    serde_norway::to_string(object)
}

fn labels() -> BTreeMap<String, String> {
    BTreeMap::from([(APP_LABEL.to_string(), NAME.to_string())])
}

fn meta(namespace: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(NAME.to_string()),
        namespace: Some(namespace.to_string()),
        labels: Some(labels()),
        ..Default::default()
    }
}

fn claim(cache: &BuilditCacheCfg, namespace: &str) -> core::PersistentVolumeClaim {
    core::PersistentVolumeClaim {
        metadata: meta(namespace),
        spec: Some(core::PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            storage_class_name: cache.storage_class.clone(),
            resources: Some(core::VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    Quantity(format!("{}Gi", cache.size_gib)),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn deployment(
    cache: &BuilditCacheCfg,
    namespace: &str,
    image: &str,
    pull_secret: &str,
) -> apps::Deployment {
    let probe = |initial: i32| core::Probe {
        http_get: Some(core::HTTPGetAction {
            path: Some("/_health".to_string()),
            port: IntOrString::String(PORT_NAME.to_string()),
            ..Default::default()
        }),
        initial_delay_seconds: Some(initial),
        period_seconds: Some(10),
        ..Default::default()
    };
    let args = [
        "cache",
        "--bind",
        &format!("0.0.0.0:{}", BuilditCacheCfg::PORT),
        "--dir",
        DATA_DIR,
        "--token-file",
        crucible_contract::buildit::CACHE_TOKEN_FILE,
        "--max-gib",
        &cache.keep_gib().to_string(),
    ]
    .map(String::from)
    .to_vec();
    let mount = |name: &str, path: &str, read_only: bool| core::VolumeMount {
        name: name.to_string(),
        mount_path: path.to_string(),
        read_only: Some(read_only),
        ..Default::default()
    };
    let container = core::Container {
        name: NAME.to_string(),
        image: Some(image.to_string()),
        image_pull_policy: Some("IfNotPresent".to_string()),
        command: Some(vec!["buildit".to_string()]),
        args: Some(args),
        env: Some(vec![core::EnvVar {
            name: "RUST_LOG".to_string(),
            value: Some("info".to_string()),
            ..Default::default()
        }]),
        ports: Some(vec![core::ContainerPort {
            name: Some(PORT_NAME.to_string()),
            container_port: i32::from(BuilditCacheCfg::PORT),
            ..Default::default()
        }]),
        readiness_probe: Some(probe(2)),
        liveness_probe: Some(probe(10)),
        resources: Some(core::ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity("250m".to_string())),
                ("memory".to_string(), Quantity("256Mi".to_string())),
            ])),
            limits: Some(BTreeMap::from([(
                "memory".to_string(),
                Quantity("1Gi".to_string()),
            )])),
            ..Default::default()
        }),
        volume_mounts: Some(vec![
            mount("data", DATA_DIR, false),
            mount("token", TOKEN_DIR, true),
        ]),
        security_context: Some(core::SecurityContext {
            allow_privilege_escalation: Some(false),
            read_only_root_filesystem: Some(true),
            run_as_non_root: Some(true),
            capabilities: Some(core::Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                ..Default::default()
            }),
            seccomp_profile: Some(core::SeccompProfile {
                type_: "RuntimeDefault".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    apps::Deployment {
        metadata: meta(namespace),
        spec: Some(apps::DeploymentSpec {
            replicas: Some(1),
            strategy: Some(apps::DeploymentStrategy {
                type_: Some("Recreate".to_string()),
                ..Default::default()
            }),
            selector: LabelSelector {
                match_labels: Some(labels()),
                ..Default::default()
            },
            template: core::PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels()),
                    ..Default::default()
                }),
                spec: Some(core::PodSpec {
                    automount_service_account_token: Some(false),
                    image_pull_secrets: Some(vec![core::LocalObjectReference {
                        name: pull_secret.to_string(),
                    }]),
                    containers: vec![container],
                    volumes: Some(vec![
                        core::Volume {
                            name: "data".to_string(),
                            persistent_volume_claim: Some(
                                core::PersistentVolumeClaimVolumeSource {
                                    claim_name: NAME.to_string(),
                                    ..Default::default()
                                },
                            ),
                            ..Default::default()
                        },
                        core::Volume {
                            name: "token".to_string(),
                            secret: Some(core::SecretVolumeSource {
                                secret_name: Some(cache.token_secret.to_string()),
                                items: Some(vec![core::KeyToPath {
                                    key: BuilditCacheCfg::TOKEN_KEY.to_string(),
                                    path: BuilditCacheCfg::TOKEN_KEY.to_string(),
                                    ..Default::default()
                                }]),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn service(namespace: &str) -> core::Service {
    core::Service {
        metadata: meta(namespace),
        spec: Some(core::ServiceSpec {
            type_: Some("ClusterIP".to_string()),
            selector: Some(labels()),
            ports: Some(vec![core::ServicePort {
                name: Some(PORT_NAME.to_string()),
                port: i32::from(BuilditCacheCfg::PORT),
                target_port: Some(IntOrString::String(PORT_NAME.to_string())),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Ingress to the cache from builder pods in `builder_namespace` only.
fn netpol(namespace: &str, builder_namespace: &str) -> networking::NetworkPolicy {
    networking::NetworkPolicy {
        metadata: meta(namespace),
        spec: Some(networking::NetworkPolicySpec {
            pod_selector: Some(LabelSelector {
                match_labels: Some(labels()),
                ..Default::default()
            }),
            policy_types: Some(vec!["Ingress".to_string()]),
            ingress: Some(vec![networking::NetworkPolicyIngressRule {
                from: Some(vec![builder_peer(builder_namespace)]),
                ports: Some(vec![networking::NetworkPolicyPort {
                    port: Some(IntOrString::Int(BuilditCacheCfg::PORT.into())),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
            }]),
            ..Default::default()
        }),
    }
}

fn builder_peer(builder_namespace: &str) -> networking::NetworkPolicyPeer {
    networking::NetworkPolicyPeer {
        namespace_selector: Some(LabelSelector {
            match_labels: Some(BTreeMap::from([(
                "kubernetes.io/metadata.name".to_string(),
                builder_namespace.to_string(),
            )])),
            ..Default::default()
        }),
        pod_selector: Some(LabelSelector {
            match_labels: Some(BTreeMap::from([(
                "buildit.dev/managed-by".to_string(),
                "mcp".to_string(),
            )])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The builder pods' egress rule to the cache in `namespace`.
pub fn builder_egress(namespace: &str) -> networking::NetworkPolicyEgressRule {
    networking::NetworkPolicyEgressRule {
        ports: Some(vec![networking::NetworkPolicyPort {
            port: Some(IntOrString::Int(BuilditCacheCfg::PORT.into())),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }]),
        to: Some(vec![networking::NetworkPolicyPeer {
            namespace_selector: Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    "kubernetes.io/metadata.name".to_string(),
                    namespace.to_string(),
                )])),
                ..Default::default()
            }),
            pod_selector: Some(LabelSelector {
                match_labels: Some(labels()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
    }
}

#[cfg(test)]
mod tests {
    use crate::deploy::buildit_cache::{MissingCacheTable, TOKEN_DIR, render};
    use crate::deploy::profile::{BuilditCacheCfg, DeployProfile};
    use crate::deploy::render::RenderOpts;

    const PROFILE: &str = r#"
        [cluster]
        loop_namespace = "crucible-system"
        rig_namespace = "rig"
        service_account = "crucible-spoke"
        supervisor_image = "registry.example.com/openshell-supervisor:latest"
        [image]
        loop = "registry.example.com/crucible-loop:latest"
        pull_secret = "quay-pull"
        [buildit]
        namespace = "crucible-builds"
    "#;

    fn docs(extra: &str) -> Vec<serde_json::Value> {
        let profile: DeployProfile = toml::from_str(&format!("{PROFILE}{extra}")).unwrap();
        let yaml = render(&profile, &RenderOpts::default()).unwrap();
        yaml.split("---\n")
            .map(|d| serde_norway::from_str(d).unwrap())
            .collect()
    }

    fn kind<'a>(docs: &'a [serde_json::Value], kind: &str) -> &'a serde_json::Value {
        docs.iter().find(|d| d["kind"] == kind).unwrap()
    }

    #[test]
    fn the_cache_runs_buildit_on_a_claim_in_the_loop_namespace() {
        let docs = docs(
            r#"
            [buildit.cache]
            token_secret = "buildit-cache-token"
            size_gib = 500
            storage_class = "lvms-vg1"
        "#,
        );
        let kinds: Vec<&str> = docs.iter().map(|d| d["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "PersistentVolumeClaim",
                "Deployment",
                "Service",
                "NetworkPolicy"
            ]
        );
        for d in &docs {
            assert_eq!(d["metadata"]["name"], "buildit-cache");
            assert_eq!(d["metadata"]["namespace"], "crucible-system");
        }
        let claim = kind(&docs, "PersistentVolumeClaim");
        assert_eq!(claim["spec"]["resources"]["requests"]["storage"], "500Gi");
        assert_eq!(claim["spec"]["storageClassName"], "lvms-vg1");

        let deploy = kind(&docs, "Deployment");
        assert_eq!(deploy["spec"]["strategy"]["type"], "Recreate");
        let pod = &deploy["spec"]["template"]["spec"];
        assert_eq!(pod["automountServiceAccountToken"], false);
        let c = &pod["containers"][0];
        assert_eq!(c["image"], "registry.example.com/crucible-loop:latest");
        assert_eq!(c["command"], serde_json::json!(["buildit"]));
        assert_eq!(
            c["args"],
            serde_json::json!([
                "cache",
                "--bind",
                "0.0.0.0:8850",
                "--dir",
                "/var/cache/buildit",
                "--token-file",
                "/etc/buildit-cache/token",
                "--max-gib",
                "450"
            ])
        );
        assert_eq!(
            format!("{TOKEN_DIR}/{}", BuilditCacheCfg::TOKEN_KEY),
            crucible_contract::buildit::CACHE_TOKEN_FILE,
            "the loop pod mounts the token where the loop tells buildit it is"
        );
        assert_eq!(c["securityContext"]["readOnlyRootFilesystem"], true);
        assert_eq!(c["readinessProbe"]["httpGet"]["path"], "/_health");
        let volumes = &pod["volumes"];
        assert_eq!(
            volumes[0]["persistentVolumeClaim"]["claimName"],
            "buildit-cache"
        );
        assert_eq!(
            volumes[1]["secret"],
            serde_json::json!({
                "secretName": "buildit-cache-token",
                "items": [{ "key": "token", "path": "token" }]
            })
        );

        let svc = kind(&docs, "Service");
        assert_eq!(svc["spec"]["ports"][0]["port"], 8850);
        let netpol = kind(&docs, "NetworkPolicy");
        assert_eq!(
            netpol["spec"]["ingress"],
            serde_json::json!([{
                "from": [{
                    "namespaceSelector": { "matchLabels": {
                        "kubernetes.io/metadata.name": "crucible-builds"
                    } },
                    "podSelector": { "matchLabels": { "buildit.dev/managed-by": "mcp" } }
                }],
                "ports": [{ "port": 8850, "protocol": "TCP" }]
            }])
        );
    }

    #[test]
    fn the_cache_image_and_defaults() {
        let docs = docs(
            r#"
            [buildit.cache]
            token_secret = "t"
            image = "registry.example.com/buildit:1"
        "#,
        );
        let claim = kind(&docs, "PersistentVolumeClaim");
        assert_eq!(claim["spec"]["resources"]["requests"]["storage"], "200Gi");
        assert!(claim["spec"].get("storageClassName").is_none());
        let c = &kind(&docs, "Deployment")["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(c["image"], "registry.example.com/buildit:1");
        assert_eq!(c["args"][8], "180");
    }

    #[test]
    fn the_cache_runs_the_buildit_image_unless_it_names_its_own() {
        let image = |extra: &str| {
            kind(&docs(extra), "Deployment")["spec"]["template"]["spec"]["containers"][0]["image"]
                .clone()
        };
        let pinned = "image = \"registry.example.com/crucible-buildit:2\"\n";
        let cache = "[buildit.cache]\ntoken_secret = \"t\"\n";
        assert_eq!(image(cache), "registry.example.com/crucible-loop:latest");
        assert_eq!(
            image(&format!("{pinned}{cache}")),
            "registry.example.com/crucible-buildit:2"
        );
        assert_eq!(
            image(&format!(
                "{pinned}{cache}image = \"registry.example.com/buildit:1\"\n"
            )),
            "registry.example.com/buildit:1"
        );
    }

    #[test]
    fn rendering_without_the_table_says_what_is_missing() {
        let profile: DeployProfile = toml::from_str(PROFILE).unwrap();
        let e = render(&profile, &RenderOpts::default()).unwrap_err();
        assert!(e.downcast_ref::<MissingCacheTable>().is_some(), "{e:#}");
        let bad: Result<DeployProfile, _> =
            toml::from_str(&format!("{PROFILE}[buildit.cache]\nsize_gib = 1\n"));
        assert!(bad.is_err(), "token_secret is required");
    }
}
