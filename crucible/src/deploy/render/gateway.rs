//! A run's OpenShell gateway in a pod of its own, in `cluster.sandbox_namespace`, running as
//! [`GATEWAY_SERVICE_ACCOUNT`]: the only identity that may create sandboxes and pods there. The
//! loop pod reaches it over mTLS through its Service; supervisors dial the same Service, and reach
//! the run's broker, collector, and MCP servers through the loop Service.
//!
//! ```text
//!  loop ns                                   sandbox ns
//!  ┌──────────────┐  mTLS :17670   ┌────────────────────┐ creates ┌────────────┐
//!  │ loop pod     │──────────────▶ │ <run>-gw Service    │────────▶│ sandboxes  │
//!  │ (pack code)  │                │  gateway pod (SA)   │         │ supervisors│
//!  └──────▲───────┘                └─────────▲──────────┘         └─────┬──────┘
//!         │ broker/collector/MCP             └── IssueSandboxToken ─────┤
//!   <run> loop Service ◀────────────── host.openshell.internal ─────────┘
//! ```
//!
//! Everything here is derived from the rendered loop pod and the profile; the controller creates
//! these objects per run and supplies the per-run addresses and Secret names.

use crate::deploy::profile::DeployProfile;
use crate::openshell::gateway::{
    AWS_SANDBOX_ROLE_ENV, AWS_WEB_IDENTITY_TOKEN_PATH, GATEWAY_CLIENT_TLS_DIR_ENV, GATEWAY_URL_ENV,
    SANDBOX_RUNTIME_IMAGE_ENV, SERVE_CLIENT_TLS_SECRET_ENV, SERVE_GATEWAY_ID_ENV,
    SERVE_GRPC_ENDPOINT_ENV, SERVE_HOST_IP_ENV, SERVE_SERVER_TLS_DIR_ENV, SUPERVISOR_IMAGE_ENV,
};
use crate::openshell::grpc::GATEWAY_PORT;
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::api::networking::v1 as networking;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use std::collections::BTreeMap;
use std::net::IpAddr;

/// The service account every run's gateway pod runs as.
pub const GATEWAY_SERVICE_ACCOUNT: &str = "crucible-openshell-gateway";
/// Carried by every per-run gateway object; its value is the run pod's name.
pub const GATEWAY_FOR_LABEL: &str = "crucible.dev/gateway-for";
const ROLE_LABEL: &str = "crucible.dev/role";
const ROLE_VALUE: &str = "openshell-gateway";
/// The loop pod's label naming its run, which the loop Service and the gateway's NetworkPolicy
/// select on.
pub const RUN_LABEL: &str = "crucible/run";
/// The label the OpenShell driver puts on supervisor pods.
const SUPERVISOR_COMPONENT: (&str, &str) = ("openshell.ai/component", "supervisor");
/// The label the OpenShell driver stamps, from its own config, on every supervisor pod and
/// Sandbox it creates. Sandbox SPIFFE IDs are derived from it.
pub const GATEWAY_ID_LABEL: &str = "openshell.ai/gateway-id";

const SERVER_TLS_DIR: &str = "/etc/crucible/gateway-server-tls";
/// Where the loop pod finds its gateway client material (delivered through the run Secret).
pub const CLIENT_TLS_DIR: &str = "/etc/crucible/gateway-tls";
const GATEWAY_HOME: &str = "/var/lib/openshell-gateway";

/// Whether this profile runs each run's gateway in its own pod: only under the kubernetes driver
/// with a dedicated sandbox namespace.
pub fn isolated(profile: &DeployProfile) -> bool {
    profile.cluster.sandbox_driver == crate::openshell::gateway::ComputeDriver::Kubernetes
        && profile.cluster.sandbox_namespace.is_some()
}

/// The names of one run's gateway objects, all derived from the run pod's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayNames {
    pub run_pod: String,
    /// The gateway pod, its Service, and its NetworkPolicy.
    pub gateway: String,
    /// The loop Service in the loop namespace.
    pub loop_service: String,
    /// The gateway's server certificate, key, and client CA.
    pub server_secret: String,
    /// The client material the driver mounts into supervisors.
    pub client_secret: String,
    /// The gateway's OpenShell id: the [`GATEWAY_ID_LABEL`] on everything it creates, and the
    /// `gateway/<id>` segment of its sandboxes' SPIFFE IDs.
    pub gateway_id: String,
}

impl GatewayNames {
    pub fn for_run_pod(run_pod: &str) -> Self {
        Self {
            run_pod: run_pod.to_string(),
            gateway: label_safe(&format!("{run_pod}-gw")),
            loop_service: label_safe(run_pod),
            server_secret: format!("{run_pod}-gw-tls"),
            client_secret: format!("{run_pod}-gw-client"),
            gateway_id: label_safe(run_pod),
        }
    }
}

/// `name` when it is a DNS-1035 label, else a hashed stand-in that is one.
fn label_safe(name: &str) -> String {
    if name.len() <= 63 {
        return name.to_string();
    }
    let digest = crucible_contract::artifact::sha256_hex(name.as_bytes());
    format!("gw-{}", &digest[..20])
}

fn gateway_labels(names: &GatewayNames) -> BTreeMap<String, String> {
    BTreeMap::from([
        (GATEWAY_FOR_LABEL.to_string(), names.run_pod.clone()),
        (ROLE_LABEL.to_string(), ROLE_VALUE.to_string()),
    ])
}

/// Selects every run's gateway objects.
pub fn gateway_selector() -> String {
    format!("{ROLE_LABEL}={ROLE_VALUE}")
}

/// Selects the gateway objects of the run whose pod is `run_pod`.
pub fn run_gateway_selector(run_pod: &str) -> String {
    format!("{},{GATEWAY_FOR_LABEL}={run_pod}", gateway_selector())
}

/// Selects what the gateway with `names` created: its Sandboxes and supervisor pods.
pub fn gateway_created_selector(names: &GatewayNames) -> String {
    format!("{GATEWAY_ID_LABEL}={}", names.gateway_id)
}

fn sandbox_namespace(profile: &DeployProfile) -> String {
    profile.cluster.sandbox_namespace().to_string()
}

/// The gateway's Service in the sandbox namespace. Its ClusterIP is the endpoint the loop pod and
/// supervisors dial, and the server certificate's IP SAN.
pub fn gateway_service(profile: &DeployProfile, names: &GatewayNames) -> core::Service {
    core::Service {
        metadata: ObjectMeta {
            name: Some(names.gateway.clone()),
            namespace: Some(sandbox_namespace(profile)),
            labels: Some(gateway_labels(names)),
            ..Default::default()
        },
        spec: Some(core::ServiceSpec {
            selector: Some(gateway_labels(names)),
            ports: Some(vec![core::ServicePort {
                name: Some("grpc".to_string()),
                port: GATEWAY_PORT.into(),
                target_port: Some(IntOrString::Int(GATEWAY_PORT.into())),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The loop Service in the loop namespace, selecting the run's loop pod on `ports` (the ones its
/// NetworkPolicy admits sandbox traffic on). Its ClusterIP is `host.openshell.internal` inside the
/// run's sandboxes.
pub fn loop_service(
    profile: &DeployProfile,
    names: &GatewayNames,
    run: &str,
    ports: &[u16],
) -> core::Service {
    core::Service {
        metadata: ObjectMeta {
            name: Some(names.loop_service.clone()),
            namespace: Some(profile.cluster.loop_namespace.clone()),
            labels: Some(gateway_labels(names)),
            ..Default::default()
        },
        spec: Some(core::ServiceSpec {
            selector: Some(BTreeMap::from([(RUN_LABEL.to_string(), run.to_string())])),
            ports: Some(
                ports
                    .iter()
                    .map(|port| core::ServicePort {
                        name: Some(format!("p{port}")),
                        port: (*port).into(),
                        target_port: Some(IntOrString::Int((*port).into())),
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The per-run addresses the gateway pod is configured with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatewayAddresses {
    /// The gateway Service's ClusterIP.
    pub gateway: IpAddr,
    /// The loop Service's ClusterIP.
    pub loop_service: IpAddr,
}

impl GatewayAddresses {
    pub fn url(&self) -> String {
        gateway_url(self.gateway)
    }
}

/// The endpoint the loop pod and supervisors dial: the gateway Service's ClusterIP.
pub fn gateway_url(gateway: IpAddr) -> String {
    format!(
        "https://{}",
        std::net::SocketAddr::new(gateway, GATEWAY_PORT)
    )
}

/// The loop pod env the gateway reads its driver config from.
fn forwarded(name: &str) -> bool {
    name.starts_with("CRUCIBLE_SANDBOX_")
        || name == AWS_SANDBOX_ROLE_ENV
        || name == SUPERVISOR_IMAGE_ENV
        || name == SANDBOX_RUNTIME_IMAGE_ENV
        || name == "OTEL_EXPORTER_OTLP_ENDPOINT"
        || name == "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"
}

/// The gateway pod for the run whose rendered loop pod is `loop_pod`: the loop image, running
/// `crucible openshell-gateway` as [`GATEWAY_SERVICE_ACCOUNT`], with the loop pod's driver env, the
/// per-run addresses, and the server TLS Secret mounted. When the loop pod names a sandbox AWS
/// role, the pod also gets its own STS-audienced service account token for it, so that role's
/// trust policy names [`GATEWAY_SERVICE_ACCOUNT`] in the sandbox namespace.
pub fn gateway_pod(
    profile: &DeployProfile,
    names: &GatewayNames,
    loop_pod: &core::Pod,
    addresses: GatewayAddresses,
) -> core::Pod {
    let loop_spec = loop_pod.spec.as_ref();
    let loop_container = loop_spec.and_then(|s| s.containers.first());
    let plain = |name: &str, value: String| core::EnvVar {
        name: name.to_string(),
        value: Some(value),
        value_from: None,
    };
    let mut env: Vec<core::EnvVar> = loop_container
        .and_then(|c| c.env.as_ref())
        .into_iter()
        .flatten()
        .filter(|e| e.value_from.is_none() && forwarded(&e.name))
        .cloned()
        .collect();
    env.extend([
        plain("HOME", GATEWAY_HOME.to_string()),
        plain(SERVE_GRPC_ENDPOINT_ENV, addresses.url()),
        plain(SERVE_HOST_IP_ENV, addresses.loop_service.to_string()),
        plain(SERVE_CLIENT_TLS_SECRET_ENV, names.client_secret.clone()),
        plain(SERVE_SERVER_TLS_DIR_ENV, SERVER_TLS_DIR.to_string()),
        plain(SERVE_GATEWAY_ID_ENV, names.gateway_id.clone()),
    ]);
    let mut mounts = vec![
        core::VolumeMount {
            name: "server-tls".to_string(),
            mount_path: SERVER_TLS_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        core::VolumeMount {
            name: "home".to_string(),
            mount_path: GATEWAY_HOME.to_string(),
            ..Default::default()
        },
    ];
    let mut volumes = vec![
        core::Volume {
            name: "server-tls".to_string(),
            secret: Some(core::SecretVolumeSource {
                secret_name: Some(names.server_secret.clone()),
                ..Default::default()
            }),
            ..Default::default()
        },
        core::Volume {
            name: "home".to_string(),
            empty_dir: Some(core::EmptyDirVolumeSource::default()),
            ..Default::default()
        },
    ];
    if env.iter().any(|e| e.name == AWS_SANDBOX_ROLE_ENV) {
        let token = std::path::Path::new(AWS_WEB_IDENTITY_TOKEN_PATH);
        mounts.push(core::VolumeMount {
            name: "aws-token".to_string(),
            mount_path: token
                .parent()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default(),
            read_only: Some(true),
            ..Default::default()
        });
        volumes.push(core::Volume {
            name: "aws-token".to_string(),
            projected: Some(core::ProjectedVolumeSource {
                sources: Some(vec![core::VolumeProjection {
                    service_account_token: Some(core::ServiceAccountTokenProjection {
                        audience: Some("sts.amazonaws.com".to_string()),
                        expiration_seconds: Some(3600),
                        path: token
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    let container = core::Container {
        name: "gateway".to_string(),
        image: loop_container.and_then(|c| c.image.clone()),
        image_pull_policy: Some("IfNotPresent".to_string()),
        command: Some(vec![
            "crucible".to_string(),
            "openshell-gateway".to_string(),
        ]),
        env: Some(env),
        ports: Some(vec![core::ContainerPort {
            name: Some("grpc".to_string()),
            container_port: GATEWAY_PORT.into(),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }]),
        security_context: Some(core::SecurityContext {
            allow_privilege_escalation: Some(false),
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
        volume_mounts: Some(mounts),
        ..Default::default()
    };
    core::Pod {
        metadata: ObjectMeta {
            name: Some(names.gateway.clone()),
            namespace: Some(sandbox_namespace(profile)),
            labels: Some(gateway_labels(names)),
            ..Default::default()
        },
        spec: Some(core::PodSpec {
            service_account_name: Some(GATEWAY_SERVICE_ACCOUNT.to_string()),
            restart_policy: Some("Never".to_string()),
            image_pull_secrets: loop_spec.and_then(|s| s.image_pull_secrets.clone()),
            node_selector: loop_spec.and_then(|s| s.node_selector.clone()),
            affinity: loop_spec.and_then(|s| s.affinity.clone()),
            containers: vec![container],
            volumes: Some(volumes),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Admit only this run's loop pod and the supervisors this gateway created to its API port.
pub fn gateway_network_policy(
    profile: &DeployProfile,
    names: &GatewayNames,
    run: &str,
) -> networking::NetworkPolicy {
    let port = networking::NetworkPolicyPort {
        port: Some(IntOrString::Int(GATEWAY_PORT.into())),
        protocol: Some("TCP".to_string()),
        ..Default::default()
    };
    networking::NetworkPolicy {
        metadata: ObjectMeta {
            name: Some(names.gateway.clone()),
            namespace: Some(sandbox_namespace(profile)),
            labels: Some(gateway_labels(names)),
            ..Default::default()
        },
        spec: Some(networking::NetworkPolicySpec {
            pod_selector: Some(LabelSelector {
                match_labels: Some(gateway_labels(names)),
                ..Default::default()
            }),
            policy_types: Some(vec!["Ingress".to_string()]),
            ingress: Some(vec![networking::NetworkPolicyIngressRule {
                from: Some(vec![
                    networking::NetworkPolicyPeer {
                        namespace_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([(
                                "kubernetes.io/metadata.name".to_string(),
                                profile.cluster.loop_namespace.clone(),
                            )])),
                            ..Default::default()
                        }),
                        pod_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([(
                                RUN_LABEL.to_string(),
                                run.to_string(),
                            )])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    networking::NetworkPolicyPeer {
                        pod_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([
                                (
                                    SUPERVISOR_COMPONENT.0.to_string(),
                                    SUPERVISOR_COMPONENT.1.to_string(),
                                ),
                                (GATEWAY_ID_LABEL.to_string(), names.gateway_id.clone()),
                            ])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ]),
                ports: Some(vec![port]),
            }]),
            ..Default::default()
        }),
    }
}

/// PEM material for one end of a run's gateway mTLS: the run CA, a certificate it issued, and that
/// certificate's key.
#[derive(Clone, Copy)]
pub struct TlsMaterial<'a> {
    pub ca: &'a str,
    pub cert: &'a str,
    pub key: &'a str,
}

/// A gateway TLS Secret in the sandbox namespace under the `ca.crt`/`tls.crt`/`tls.key` keys the
/// gateway's server TLS dir and the driver's supervisor mount both read.
pub fn gateway_tls_secret(
    profile: &DeployProfile,
    names: &GatewayNames,
    name: &str,
    material: TlsMaterial<'_>,
) -> core::Secret {
    core::Secret {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(sandbox_namespace(profile)),
            labels: Some(gateway_labels(names)),
            ..Default::default()
        },
        type_: Some("Opaque".to_string()),
        string_data: Some(BTreeMap::from([
            ("ca.crt".to_string(), material.ca.to_string()),
            ("tls.crt".to_string(), material.cert.to_string()),
            ("tls.key".to_string(), material.key.to_string()),
        ])),
        ..Default::default()
    }
}

/// The loop pod env pointing the engine at the gateway behind `gateway`.
pub fn loop_pod_env(gateway: IpAddr) -> Vec<core::EnvVar> {
    vec![
        core::EnvVar {
            name: GATEWAY_URL_ENV.to_string(),
            value: Some(gateway_url(gateway)),
            value_from: None,
        },
        core::EnvVar {
            name: GATEWAY_CLIENT_TLS_DIR_ENV.to_string(),
            value: Some(CLIENT_TLS_DIR.to_string()),
            value_from: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use crate::deploy::render::gateway::*;

    fn profile(extra: &str) -> DeployProfile {
        toml::from_str(&format!(
            r#"
            [cluster]
            loop_namespace = "loops"
            rig_namespace = "rig"
            service_account = "crucible-spoke"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            sandbox_driver = "kubernetes"
            {extra}
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "pull"
            "#
        ))
        .expect("profile")
    }

    fn isolated_profile() -> DeployProfile {
        profile(r#"sandbox_namespace = "sandboxes""#)
    }

    fn addresses() -> GatewayAddresses {
        GatewayAddresses {
            gateway: IpAddr::from([172, 30, 1, 9]),
            loop_service: IpAddr::from([172, 30, 1, 10]),
        }
    }

    fn loop_pod() -> core::Pod {
        let env = |name: &str, value: &str| core::EnvVar {
            name: name.into(),
            value: Some(value.into()),
            value_from: None,
        };
        core::Pod {
            spec: Some(core::PodSpec {
                image_pull_secrets: Some(vec![core::LocalObjectReference {
                    name: "pull".into(),
                }]),
                containers: vec![core::Container {
                    name: "loop".into(),
                    image: Some("registry.example.com/crucible-loop@sha256:abc".into()),
                    env: Some(vec![
                        env("CRUCIBLE_SANDBOX_NAMESPACE", "sandboxes"),
                        env(
                            "CRUCIBLE_SANDBOX_DEFAULT_IMAGE",
                            "registry.example.com/sbx:1",
                        ),
                        env(
                            SUPERVISOR_IMAGE_ENV,
                            "registry.example.com/openshell-supervisor:1",
                        ),
                        env("BROKER_TOKEN_FILE", "/run/broker"),
                        core::EnvVar {
                            name: "CRUCIBLE_SANDBOX_SECRET".into(),
                            value: None,
                            value_from: Some(core::EnvVarSource::default()),
                        },
                    ]),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_a_kubernetes_profile_with_a_sandbox_namespace_isolates_its_gateways() {
        assert!(isolated(&isolated_profile()));
        assert!(!isolated(&profile("")));
        let podman: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "loops"
            rig_namespace = "rig"
            service_account = "crucible-spoke"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            sandbox_namespace = "sandboxes"
            [image]
            loop = "registry.example.com/crucible-loop:latest"
            pull_secret = "pull"
            "#,
        )
        .expect("profile");
        assert!(!isolated(&podman), "podman nests sandboxes in the loop pod");
    }

    #[test]
    fn names_derive_from_the_run_pod_and_stay_dns_labels() {
        let names = GatewayNames::for_run_pod("crucible-run-brave-otter");
        assert_eq!(names.gateway, "crucible-run-brave-otter-gw");
        assert_eq!(names.loop_service, "crucible-run-brave-otter");
        assert_eq!(names.server_secret, "crucible-run-brave-otter-gw-tls");
        assert_eq!(names.client_secret, "crucible-run-brave-otter-gw-client");
        assert_eq!(names.gateway_id, "crucible-run-brave-otter");
        assert_eq!(
            gateway_created_selector(&names),
            "openshell.ai/gateway-id=crucible-run-brave-otter"
        );
        let long = GatewayNames::for_run_pod(&format!("crucible-run-{}", "x".repeat(60)));
        assert!(
            long.gateway.len() <= 63 && long.gateway.starts_with("gw-"),
            "{}",
            long.gateway
        );
        assert!(long.loop_service.len() <= 63, "{}", long.loop_service);
        assert!(long.gateway_id.len() <= 63, "{}", long.gateway_id);
        assert_ne!(
            GatewayNames::for_run_pod(&format!("crucible-run-{}", "x".repeat(60))).gateway,
            GatewayNames::for_run_pod(&format!("crucible-run-{}", "y".repeat(60))).gateway
        );
    }

    #[test]
    fn the_services_front_the_gateway_and_the_loop_pod() {
        let p = isolated_profile();
        let names = GatewayNames::for_run_pod("crucible-run-a");
        let gw = gateway_service(&p, &names);
        assert_eq!(gw.metadata.namespace.as_deref(), Some("sandboxes"));
        let spec = gw.spec.expect("spec");
        assert_eq!(
            spec.selector.expect("selector")[GATEWAY_FOR_LABEL],
            "crucible-run-a"
        );
        assert_eq!(spec.ports.expect("ports")[0].port, i32::from(GATEWAY_PORT));

        let svc = loop_service(&p, &names, "run-a", &[8849, 17671]);
        assert_eq!(svc.metadata.namespace.as_deref(), Some("loops"));
        let spec = svc.spec.expect("spec");
        assert_eq!(spec.selector.expect("selector")[RUN_LABEL], "run-a");
        assert_eq!(
            spec.ports
                .expect("ports")
                .iter()
                .map(|p| p.port)
                .collect::<Vec<_>>(),
            [8849, 17671]
        );
    }

    #[test]
    fn the_gateway_pod_runs_as_its_own_account_with_only_the_driver_env() {
        let p = isolated_profile();
        let names = GatewayNames::for_run_pod("crucible-run-a");
        let pod = gateway_pod(&p, &names, &loop_pod(), addresses());
        assert_eq!(pod.metadata.namespace.as_deref(), Some("sandboxes"));
        let spec = pod.spec.expect("spec");
        assert_eq!(
            spec.service_account_name.as_deref(),
            Some(GATEWAY_SERVICE_ACCOUNT)
        );
        assert_eq!(spec.image_pull_secrets.expect("pull")[0].name, "pull");
        let c = &spec.containers[0];
        assert_eq!(
            c.image.as_deref(),
            Some("registry.example.com/crucible-loop@sha256:abc")
        );
        assert_eq!(
            c.command.clone().expect("command"),
            ["crucible", "openshell-gateway"]
        );
        let env: BTreeMap<String, Option<String>> = c
            .env
            .clone()
            .expect("env")
            .into_iter()
            .map(|e| (e.name, e.value))
            .collect();
        assert_eq!(
            env["CRUCIBLE_SANDBOX_NAMESPACE"].as_deref(),
            Some("sandboxes")
        );
        assert!(env.contains_key(SUPERVISOR_IMAGE_ENV));
        assert!(
            !env.contains_key("BROKER_TOKEN_FILE"),
            "loop-only env stays in the loop pod"
        );
        assert!(
            !env.contains_key("CRUCIBLE_SANDBOX_SECRET"),
            "Secret-backed env is never copied out of the loop namespace"
        );
        assert_eq!(
            env[SERVE_GRPC_ENDPOINT_ENV].as_deref(),
            Some("https://172.30.1.9:17670")
        );
        assert_eq!(env[SERVE_HOST_IP_ENV].as_deref(), Some("172.30.1.10"));
        assert_eq!(
            env[SERVE_CLIENT_TLS_SECRET_ENV].as_deref(),
            Some("crucible-run-a-gw-client")
        );
        assert_eq!(env[SERVE_GATEWAY_ID_ENV].as_deref(), Some("crucible-run-a"));
        let volumes = spec.volumes.expect("volumes");
        assert_eq!(
            volumes[0]
                .secret
                .as_ref()
                .and_then(|s| s.secret_name.as_deref()),
            Some("crucible-run-a-gw-tls")
        );
        let sc = c.security_context.clone().expect("security context");
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(sc.run_as_non_root, Some(true));
    }

    #[test]
    fn a_sandbox_aws_role_gives_the_gateway_pod_its_own_sts_token() {
        let p = isolated_profile();
        let names = GatewayNames::for_run_pod("crucible-run-a");
        let without = gateway_pod(&p, &names, &loop_pod(), addresses());
        let has_token = |pod: &core::Pod| {
            pod.spec
                .as_ref()
                .and_then(|s| s.volumes.as_ref())
                .is_some_and(|v| v.iter().any(|v| v.name == "aws-token"))
        };
        assert!(!has_token(&without), "no role, no token");

        let mut with_role = loop_pod();
        with_role.spec.as_mut().expect("spec").containers[0]
            .env
            .as_mut()
            .expect("env")
            .push(core::EnvVar {
                name: AWS_SANDBOX_ROLE_ENV.into(),
                value: Some("arn:aws:iam::1:role/sandbox-ro".into()),
                value_from: None,
            });
        let pod = gateway_pod(&p, &names, &with_role, addresses());
        assert!(has_token(&pod));
        let spec = pod.spec.expect("spec");
        let token = spec
            .volumes
            .iter()
            .flatten()
            .find(|v| v.name == "aws-token")
            .and_then(|v| v.projected.as_ref())
            .and_then(|p| p.sources.as_ref())
            .and_then(|s| s.first())
            .and_then(|s| s.service_account_token.clone())
            .expect("a projected token");
        assert_eq!(token.audience.as_deref(), Some("sts.amazonaws.com"));
        let container = &spec.containers[0];
        let mount = container
            .volume_mounts
            .iter()
            .flatten()
            .find(|m| m.name == "aws-token")
            .expect("mounted");
        assert_eq!(
            format!("{}/{}", mount.mount_path, token.path),
            AWS_WEB_IDENTITY_TOKEN_PATH
        );
        assert!(
            container
                .env
                .iter()
                .flatten()
                .any(|e| e.name == AWS_SANDBOX_ROLE_ENV),
            "the role is forwarded for the gateway to assume"
        );
    }

    #[test]
    fn only_the_runs_loop_pod_and_supervisors_reach_the_gateway() {
        let p = isolated_profile();
        let names = GatewayNames::for_run_pod("crucible-run-a");
        let policy = gateway_network_policy(&p, &names, "run-a");
        let json = serde_json::to_value(&policy).expect("json");
        assert_eq!(json["metadata"]["namespace"], "sandboxes");
        assert_eq!(
            json["spec"]["podSelector"]["matchLabels"][GATEWAY_FOR_LABEL],
            "crucible-run-a"
        );
        let rule = &json["spec"]["ingress"][0];
        assert_eq!(
            rule["ports"],
            serde_json::json!([{"port": 17670, "protocol": "TCP"}])
        );
        assert_eq!(
            rule["from"],
            serde_json::json!([
                {
                    "namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "loops"}},
                    "podSelector": {"matchLabels": {"crucible/run": "run-a"}}
                },
                {"podSelector": {"matchLabels": {
                    "openshell.ai/component": "supervisor",
                    "openshell.ai/gateway-id": "crucible-run-a"
                }}}
            ])
        );
    }

    #[test]
    fn the_loop_pod_is_pointed_at_its_gateway_and_client_material() {
        let env = loop_pod_env(addresses().gateway);
        assert_eq!(env[0].name, GATEWAY_URL_ENV);
        assert_eq!(env[0].value.as_deref(), Some("https://172.30.1.9:17670"));
        assert_eq!(env[1].name, GATEWAY_CLIENT_TLS_DIR_ENV);
        assert_eq!(env[1].value.as_deref(), Some(CLIENT_TLS_DIR));
    }

    #[test]
    fn tls_secrets_carry_the_keys_the_gateway_and_driver_read_and_the_run_labels() {
        let p = isolated_profile();
        let names = GatewayNames::for_run_pod("crucible-run-a");
        let secret = gateway_tls_secret(
            &p,
            &names,
            &names.client_secret,
            TlsMaterial {
                ca: "CA",
                cert: "CERT",
                key: "KEY",
            },
        );
        assert_eq!(
            secret.metadata.name.as_deref(),
            Some("crucible-run-a-gw-client")
        );
        assert_eq!(secret.metadata.namespace.as_deref(), Some("sandboxes"));
        let data = secret.string_data.expect("string data");
        assert_eq!(data.get("ca.crt").map(String::as_str), Some("CA"));
        assert_eq!(data.get("tls.crt").map(String::as_str), Some("CERT"));
        assert_eq!(data.get("tls.key").map(String::as_str), Some("KEY"));
        let labels = secret.metadata.labels.expect("labels");
        assert_eq!(
            labels.get(GATEWAY_FOR_LABEL).map(String::as_str),
            Some("crucible-run-a")
        );
    }

    #[test]
    fn the_selectors_match_every_gateway_object_and_one_runs() {
        let names = GatewayNames::for_run_pod("crucible-run-a");
        let labels = gateway_labels(&names);
        let matches = |selector: String| {
            selector.split(',').all(|term| {
                let (k, v) = term.split_once('=').expect("equality term");
                labels.get(k).map(String::as_str) == Some(v)
            })
        };
        assert!(matches(gateway_selector()));
        assert!(matches(run_gateway_selector("crucible-run-a")));
        assert!(!matches(run_gateway_selector("crucible-run-b")));
    }
}
