//! Render the outer-loop controller's runtime (the outer loop): a one-replica `Deployment` running
//! `crucible-controller autopilot`, a `PersistentVolumeClaim` for its state volume (`outer.sqlite` +
//! `controller-events.jsonl`), a cluster-internal `Service` fronting its HTTP API/debug UI, and the
//! `Role`/`RoleBinding` letting it watch and launch the loop pods it renders. Another render
//! template (the loop pod minus the agent, plus a durable volume in place of the ephemeral one) so it
//! reuses the same digest-pinning path and the `role_binding` helper the loop-pod RBAC uses.
//!
//! The `[controller]` deploy-profile table is the *only* input (there is no per-run manifest, the
//! controller watches many repos, not one domain): discovery cadence, the watched repos, and the four
//! admission caps project into the Deployment as `CONTROLLER_*` env, the contract the clap
//! `ControllerCfg` (env-fallback) reads.

use crate::deploy::profile::{ControllerCfg, DeployProfile};
use crate::deploy::render::{MANAGED_BY_LABEL, RenderOpts, node_avoid_affinity, role_binding};
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1 as apps;
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::api::rbac::v1 as rbac;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use std::collections::BTreeMap;

/// The object name every rendered controller resource shares (there is exactly one controller per
/// cluster, unlike the per-run loop pod).
const NAME: &str = "crucible-controller";
/// The state volume's mount path, the fixed contract `outer.sqlite`/`controller-events.jsonl` live under.
const STATE_DIR: &str = "/var/lib/crucible/state";
/// The named port the API/debug UI and the liveness probe both bind ("binds loopback
/// by default; the rendered Deployment exposes it cluster-internally only").
const API_PORT_NAME: &str = "api";
const API_PORT: i32 = 8080;
/// The label key/value distinguishing the controller from a run's loop pod (both carry
/// [`MANAGED_BY_LABEL`]).
const ROLE_LABEL_KEY: &str = "crucible.dev/role";
const ROLE_LABEL_VALUE: &str = "controller";

/// Render the controller's `Deployment` + `PersistentVolumeClaim` + `Service` + `Role`/`RoleBinding` as
/// one multi-document YAML string.
pub fn render(profile: &DeployProfile, opts: &RenderOpts) -> Result<String> {
    let cfg = profile.controller.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "rendering the controller needs a [controller] table in the deploy profile (image, \
             state_volume_size, discovery_cadence_secs, watched_repos, caps)"
        )
    })?;

    let image = if opts.pin_digests {
        forge::oci::pin_digest(&cfg.image, None)
            .with_context(|| format!("pinning controller image {}", cfg.image))?
    } else {
        cfg.image.clone()
    };

    let mut docs = vec![
        serde_norway::to_string(&pvc(profile, cfg)).context("serializing the state PVC")?,
        serde_norway::to_string(&deployment(profile, cfg, &image))
            .context("serializing the controller Deployment")?,
        serde_norway::to_string(&service(profile)).context("serializing the controller Service")?,
    ];
    let role = pod_role(profile);
    docs.push(serde_norway::to_string(&role).context("serializing the controller Role")?);
    docs.push(
        serde_norway::to_string(&role_binding(
            format!("{NAME}-pods"),
            profile.cluster.loop_namespace.clone(),
            "Role",
            role.metadata.name.as_deref().unwrap_or_default(),
            profile.cluster.service_account.clone(),
            profile.cluster.loop_namespace.clone(),
        ))
        .context("serializing the controller RoleBinding")?,
    );
    Ok(docs.join("---\n"))
}

/// The pod-template labels every controller pod carries: [`MANAGED_BY_LABEL`]'s pair (so `crucible ps`
/// finds it too) plus the role that tells it apart from a run's loop pod.
fn labels() -> BTreeMap<String, String> {
    let (k, v) = MANAGED_BY_LABEL
        .split_once('=')
        .expect("MANAGED_BY_LABEL is a key=value label selector");
    BTreeMap::from([
        (k.to_string(), v.to_string()),
        (ROLE_LABEL_KEY.to_string(), ROLE_LABEL_VALUE.to_string()),
    ])
}

fn pvc(profile: &DeployProfile, cfg: &ControllerCfg) -> core::PersistentVolumeClaim {
    core::PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(format!("{NAME}-state")),
            namespace: Some(profile.cluster.loop_namespace.clone()),
            labels: Some(labels()),
            ..Default::default()
        },
        spec: Some(core::PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(core::VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    Quantity(cfg.state_volume_size.clone()),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn deployment(profile: &DeployProfile, cfg: &ControllerCfg, image: &str) -> apps::Deployment {
    let container = core::Container {
        name: "controller".to_string(),
        image: Some(image.to_string()),
        image_pull_policy: Some("IfNotPresent".to_string()),
        command: Some(vec![
            "crucible-controller".to_string(),
            "autopilot".to_string(),
        ]),
        env: Some(env(cfg, &profile.cluster.loop_namespace)),
        ports: Some(vec![core::ContainerPort {
            name: Some(API_PORT_NAME.to_string()),
            container_port: API_PORT,
            ..Default::default()
        }]),
        volume_mounts: Some(vec![core::VolumeMount {
            name: "state".to_string(),
            mount_path: STATE_DIR.to_string(),
            ..Default::default()
        }]),
        liveness_probe: Some(core::Probe {
            http_get: Some(core::HTTPGetAction {
                path: Some("/healthz".to_string()),
                port: k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String(
                    API_PORT_NAME.to_string(),
                ),
                ..Default::default()
            }),
            initial_delay_seconds: Some(5),
            period_seconds: Some(10),
            ..Default::default()
        }),
        resources: Some(resources(profile)),
        ..Default::default()
    };

    apps::Deployment {
        metadata: ObjectMeta {
            name: Some(NAME.to_string()),
            namespace: Some(profile.cluster.loop_namespace.clone()),
            labels: Some(labels()),
            ..Default::default()
        },
        spec: Some(apps::DeploymentSpec {
            replicas: Some(1),
            // The state volume (outer.sqlite + the event log) must never have two writers: a rolling
            // update would briefly run old and new pods side by side, and SQLite's single-writer
            // assumption (and the ReadWriteOnce PVC) both break under that. Recreate tears the old pod
            // down before the new one starts, so there is exactly one writer at all times; the kubelet
            // restart policy is the only supervisor this needs.
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
                    annotations: Some(BTreeMap::from([(
                        "sidecar.istio.io/inject".to_string(),
                        "false".to_string(),
                    )])),
                    ..Default::default()
                }),
                spec: Some(core::PodSpec {
                    image_pull_secrets: Some(vec![core::LocalObjectReference {
                        name: profile.image.pull_secret.clone(),
                    }]),
                    service_account_name: Some(profile.cluster.service_account.clone()),
                    affinity: node_avoid_affinity(profile),
                    containers: vec![container],
                    volumes: Some(vec![core::Volume {
                        name: "state".to_string(),
                        persistent_volume_claim: Some(core::PersistentVolumeClaimVolumeSource {
                            claim_name: format!("{NAME}-state"),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The `CONTROLLER_*` env contract: the discovery cadence, the watched-repo list, and the four
/// admission caps, plus the fixed state-dir mount path and the namespace the
/// daemon's pod watch scans, everything the `ControllerCfg` clap-with-env-fallback
/// reads instead of a flag.
fn env(cfg: &ControllerCfg, pod_namespace: &str) -> Vec<core::EnvVar> {
    let plain = |name: &str, value: String| core::EnvVar {
        name: name.to_string(),
        value: Some(value),
        value_from: None,
    };
    vec![
        plain("CONTROLLER_STATE_DIR", STATE_DIR.to_string()),
        plain("CONTROLLER_POD_NAMESPACE", pod_namespace.to_string()),
        plain(
            "CONTROLLER_DISCOVERY_CADENCE_SECS",
            cfg.discovery_cadence_secs.to_string(),
        ),
        plain("CONTROLLER_WATCHED_REPOS", cfg.watched_repos.join(",")),
        plain(
            "CONTROLLER_PER_RECONCILE_COST_USD",
            cfg.caps.per_reconcile_cost_usd.to_string(),
        ),
        plain(
            "CONTROLLER_MAX_CONCURRENT_PODS",
            cfg.caps.max_concurrent_pods.to_string(),
        ),
        plain(
            "CONTROLLER_MAX_SCOPES_PER_DAY",
            cfg.caps.max_scopes_per_day.to_string(),
        ),
        plain(
            "CONTROLLER_DAILY_COST_CEILING_USD",
            cfg.caps.daily_cost_ceiling_usd.to_string(),
        ),
    ]
}

fn resources(profile: &DeployProfile) -> core::ResourceRequirements {
    let r = &profile.resources;
    core::ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(r.cpu_request.clone())),
            ("memory".to_string(), Quantity(r.mem_request.clone())),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(r.cpu_limit.clone())),
            ("memory".to_string(), Quantity(r.mem_limit.clone())),
        ])),
        ..Default::default()
    }
}

/// Cluster-internal `Service` fronting the controller's HTTP API/debug UI (the listener isn't
/// implemented yet; the port is correct to render now, "binds loopback by default; the
/// rendered Deployment exposes it cluster-internally only").
fn service(profile: &DeployProfile) -> core::Service {
    core::Service {
        metadata: ObjectMeta {
            name: Some(NAME.to_string()),
            namespace: Some(profile.cluster.loop_namespace.clone()),
            labels: Some(labels()),
            ..Default::default()
        },
        spec: Some(core::ServiceSpec {
            type_: Some("ClusterIP".to_string()),
            selector: Some(labels()),
            ports: Some(vec![core::ServicePort {
                name: Some(API_PORT_NAME.to_string()),
                port: API_PORT,
                target_port: Some(
                    k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String(
                        API_PORT_NAME.to_string(),
                    ),
                ),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The namespaced `Role` granting pod get/list/watch/create in the loop namespace, the controller
/// admits and watches the loop pods it renders and launches there (the daemon's kube pod watch).
fn pod_role(profile: &DeployProfile) -> rbac::Role {
    rbac::Role {
        metadata: ObjectMeta {
            name: Some(format!("{NAME}-pods")),
            namespace: Some(profile.cluster.loop_namespace.clone()),
            ..Default::default()
        },
        rules: Some(vec![rbac::PolicyRule {
            api_groups: Some(vec![String::new()]),
            resources: Some(vec!["pods".to_string()]),
            verbs: vec![
                "get".to_string(),
                "list".to_string(),
                "watch".to_string(),
                "create".to_string(),
            ],
            ..Default::default()
        }]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::profile::DeployProfile;

    fn profile_with_controller() -> DeployProfile {
        toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
            [controller]
            image = "registry.example.com/crucible-controller:latest"
            state_volume_size = "5Gi"
            discovery_cadence_secs = 300
            watched_repos = ["neuralmagic/crucible", "vllm-project/vllm"]
            [controller.caps]
            per_reconcile_cost_usd = 2.5
            max_concurrent_pods = 3
            max_scopes_per_day = 20
            daily_cost_ceiling_usd = 50.0
        "#,
        )
        .expect("profile parses")
    }

    /// The controller's `[controller]` profile table must render a Deployment (Recreate, one replica,
    /// the state PVC mounted, all `CONTROLLER_*` env present) + PVC + Service + Role + RoleBinding,
    /// the golden-render discipline `render.rs` uses for the loop pod, applied to the controller.
    #[test]
    fn controller_renders_from_profile() {
        let profile = profile_with_controller();
        let yaml = render(
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render");

        let docs: Vec<&str> = yaml.split("---\n").collect();
        assert_eq!(
            docs.len(),
            5,
            "pvc + deployment + service + role + rolebinding"
        );
        assert!(docs[0].contains("kind: PersistentVolumeClaim"));
        assert!(docs[0].contains("storage: 5Gi"));
        assert!(docs[1].contains("kind: Deployment"));
        assert!(docs[2].contains("kind: Service"));
        assert!(docs[3].contains("kind: Role"));
        assert!(docs[4].contains("kind: RoleBinding"));

        // One replica, Recreate strategy (single-writer state volume).
        assert!(docs[1].contains("replicas: 1"));
        assert!(docs[1].contains("type: Recreate"));

        // The command + state mount.
        assert!(docs[1].contains("- crucible-controller"));
        assert!(docs[1].contains("- autopilot"));
        assert!(docs[1].contains("/var/lib/crucible/state"));

        // The full CONTROLLER_* env contract.
        for name in [
            "CONTROLLER_STATE_DIR",
            "CONTROLLER_DISCOVERY_CADENCE_SECS",
            "CONTROLLER_WATCHED_REPOS",
            "CONTROLLER_PER_RECONCILE_COST_USD",
            "CONTROLLER_MAX_CONCURRENT_PODS",
            "CONTROLLER_MAX_SCOPES_PER_DAY",
            "CONTROLLER_DAILY_COST_CEILING_USD",
        ] {
            assert!(yaml.contains(&format!("name: {name}")), "missing {name}");
        }
        assert!(yaml.contains("value: '300'"), "discovery cadence value");
        assert!(
            yaml.contains("neuralmagic/crucible,vllm-project/vllm"),
            "comma-joined watched repos"
        );

        // The named API port + the /healthz liveness probe on it.
        assert!(docs[1].contains("name: api"));
        assert!(docs[1].contains("path: /healthz"));
        assert!(docs[2].contains("port: 8080"));

        // Managed-by + role labels, so `crucible ps` and operators can both select it.
        assert!(docs[1].contains("app.kubernetes.io/managed-by: crucible"));
        assert!(docs[1].contains("crucible.dev/role: controller"));

        // The pod Role is scoped to the loop namespace, not cluster-wide, and only pods+the four verbs.
        assert!(docs[3].contains("namespace: autoresearch"));
        assert!(docs[3].contains("- pods"));
        assert!(docs[3].contains("- create"));

        // No avoid-list in the profile, no affinity stanza.
        assert!(!yaml.contains("affinity"), "no affinity key: {yaml}");
    }

    /// A profile listing avoid_nodes puts the required NotIn nodeAffinity on the controller's pod
    /// template too, the controller itself must not land on an operator-flagged node.
    #[test]
    fn avoid_nodes_render_the_notin_affinity_on_the_controller() {
        let mut profile = profile_with_controller();
        profile.cluster.avoid_nodes = vec!["g12e022".to_string()];
        let yaml = render(
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        )
        .expect("render");

        let docs: Vec<&str> = yaml.split("---\n").collect();
        assert!(docs[1].contains("kind: Deployment"));
        assert!(docs[1].contains("nodeAffinity"));
        assert!(docs[1].contains("requiredDuringSchedulingIgnoredDuringExecution"));
        assert!(docs[1].contains("key: kubernetes.io/hostname"));
        assert!(docs[1].contains("operator: NotIn"));
        assert!(docs[1].contains("- g12e022"));
    }

    /// A profile with no `[controller]` table can't render the controller (there is nothing to
    /// project), so it fails loudly with a fix-it instead of emitting a hollow Deployment.
    #[test]
    fn controller_without_profile_table_fails_with_clear_error() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "quay-pull"
        "#,
        )
        .expect("profile parses");

        match render(
            &profile,
            &RenderOpts {
                iterations: 1,
                max_cost: 0.0,
                pin_digests: false,
                pr_repo: None,
                pack: None,
                clusters_file: None,
            },
        ) {
            Ok(_) => panic!("expected an error: no [controller] table"),
            Err(err) => assert!(
                err.to_string().contains("[controller]"),
                "error names the missing table: {err}"
            ),
        }
    }
}
