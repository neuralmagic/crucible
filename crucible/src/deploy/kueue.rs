//! Render the crucible-kueue MCP server from the profile's `[kueue]` table: its ServiceAccount,
//! Deployment, and Service in `namespace`, the tenants namespace, and the RBAC that confines it.
//! Namespaced workload verbs are bound only in `workload_namespace`; the tenants namespace grants
//! the server read and only `record_writers` write.

use crate::deploy::profile::{DeployProfile, KueueCfg};
use crate::deploy::render::{RenderOpts, pin_image};
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1 as apps;
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::api::rbac::v1 as rbac;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde::Serialize;
use std::collections::BTreeMap;

const NAME: &str = KueueCfg::SERVICE_NAME;
const PORT_NAME: &str = "mcp";
const APP_LABEL: &str = "app.kubernetes.io/name";

#[derive(Debug, thiserror::Error)]
#[error(
    "rendering crucible-kueue needs a [kueue] table in the deploy profile (image, namespace, \
     tenant_namespace, workload_namespace, record_writers)"
)]
pub struct MissingKueueTable;

/// The server's objects as one multi-document YAML string.
pub fn render(profile: &DeployProfile, opts: &RenderOpts) -> Result<String> {
    let cfg = profile.kueue.as_ref().ok_or(MissingKueueTable)?;
    let image = pin_image(opts.digests.as_deref(), "crucible-kueue image", &cfg.image)?;
    let pull_secret = profile.image.pull_secret.as_str();
    let mut docs = Vec::new();
    let mut push = |what: &str, doc: Result<String, serde_norway::Error>| -> Result<()> {
        docs.push(doc.with_context(|| format!("serializing the crucible-kueue {what}"))?);
        Ok(())
    };
    push("tenants Namespace", yaml(&tenant_namespace(cfg)))?;
    push("ServiceAccount", yaml(&service_account(cfg)))?;
    push("Deployment", yaml(&deployment(cfg, &image, pull_secret)))?;
    push("Service", yaml(&service(cfg)))?;
    push("cluster ClusterRole", yaml(&cluster_role()))?;
    push(
        "cluster ClusterRoleBinding",
        yaml(&cluster_role_binding(cfg)),
    )?;
    push("workload ClusterRole", yaml(&workload_cluster_role()))?;
    push("workload RoleBinding", yaml(&workload_binding(cfg)))?;
    push("tenant reader Role", yaml(&record_reader_role(cfg)))?;
    push(
        "tenant reader RoleBinding",
        yaml(&record_reader_binding(cfg)),
    )?;
    push("tenant writer Role", yaml(&record_writer_role(cfg)))?;
    push(
        "tenant writer RoleBinding",
        yaml(&record_writer_binding(cfg)),
    )?;
    Ok(docs.join("---\n"))
}

fn yaml<T: Serialize>(object: &T) -> Result<String, serde_norway::Error> {
    serde_norway::to_string(object)
}

fn labels() -> BTreeMap<String, String> {
    BTreeMap::from([(APP_LABEL.to_string(), NAME.to_string())])
}

fn meta(name: impl Into<String>, namespace: Option<&str>) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.into()),
        namespace: namespace.map(str::to_string),
        labels: Some(labels()),
        ..Default::default()
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

fn rule(groups: &[&str], resources: &[&str], verbs: &[&str]) -> rbac::PolicyRule {
    rbac::PolicyRule {
        api_groups: Some(strings(groups)),
        resources: Some(strings(resources)),
        verbs: strings(verbs),
        ..Default::default()
    }
}

fn server_subject(cfg: &KueueCfg) -> rbac::Subject {
    rbac::Subject {
        kind: "ServiceAccount".to_string(),
        name: NAME.to_string(),
        namespace: Some(cfg.namespace.to_string()),
        ..Default::default()
    }
}

fn role_ref(kind: &str, name: &str) -> rbac::RoleRef {
    rbac::RoleRef {
        api_group: "rbac.authorization.k8s.io".to_string(),
        kind: kind.to_string(),
        name: name.to_string(),
    }
}

fn tenant_namespace(cfg: &KueueCfg) -> core::Namespace {
    core::Namespace {
        metadata: meta(cfg.tenant_namespace.as_str(), None),
        ..Default::default()
    }
}

fn service_account(cfg: &KueueCfg) -> core::ServiceAccount {
    core::ServiceAccount {
        metadata: meta(NAME, Some(cfg.namespace.as_str())),
        automount_service_account_token: Some(true),
        ..Default::default()
    }
}

fn deployment(cfg: &KueueCfg, image: &str, pull_secret: &str) -> apps::Deployment {
    let probe = |initial: i32| core::Probe {
        http_get: Some(core::HTTPGetAction {
            path: Some("/healthz".to_string()),
            port: IntOrString::String(PORT_NAME.to_string()),
            ..Default::default()
        }),
        initial_delay_seconds: Some(initial),
        period_seconds: Some(10),
        ..Default::default()
    };
    let env = |name: &str, value: String| core::EnvVar {
        name: name.to_string(),
        value: Some(value),
        ..Default::default()
    };
    let container = core::Container {
        name: NAME.to_string(),
        image: Some(image.to_string()),
        image_pull_policy: Some("IfNotPresent".to_string()),
        args: Some(vec![
            "--bind".to_string(),
            format!("0.0.0.0:{}", KueueCfg::PORT),
            "--tenant-namespace".to_string(),
            cfg.tenant_namespace.to_string(),
            "--allowed-host".to_string(),
            cfg.service_host(),
        ]),
        env: Some(vec![env("RUST_LOG", "info".to_string())]),
        ports: Some(vec![core::ContainerPort {
            name: Some(PORT_NAME.to_string()),
            container_port: i32::from(KueueCfg::PORT),
            ..Default::default()
        }]),
        readiness_probe: Some(probe(2)),
        liveness_probe: Some(probe(10)),
        resources: Some(core::ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity("50m".to_string())),
                ("memory".to_string(), Quantity("64Mi".to_string())),
            ])),
            limits: Some(BTreeMap::from([(
                "memory".to_string(),
                Quantity("256Mi".to_string()),
            )])),
            ..Default::default()
        }),
        security_context: Some(core::SecurityContext {
            allow_privilege_escalation: Some(false),
            read_only_root_filesystem: Some(true),
            run_as_non_root: Some(true),
            capabilities: Some(core::Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    apps::Deployment {
        metadata: meta(NAME, Some(cfg.namespace.as_str())),
        spec: Some(apps::DeploymentSpec {
            replicas: Some(1),
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
                    service_account_name: Some(NAME.to_string()),
                    image_pull_secrets: Some(vec![core::LocalObjectReference {
                        name: pull_secret.to_string(),
                    }]),
                    containers: vec![container],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn service(cfg: &KueueCfg) -> core::Service {
    core::Service {
        metadata: meta(NAME, Some(cfg.namespace.as_str())),
        spec: Some(core::ServiceSpec {
            type_: Some("ClusterIP".to_string()),
            selector: Some(labels()),
            ports: Some(vec![core::ServicePort {
                name: Some(PORT_NAME.to_string()),
                port: i32::from(KueueCfg::PORT),
                target_port: Some(IntOrString::String(PORT_NAME.to_string())),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

const KUEUE_GROUP: &str = "kueue.x-k8s.io";
const VISIBILITY_GROUP: &str = "visibility.kueue.x-k8s.io";
const WORKLOAD_VERBS: &[&str] = &["create", "get", "list", "patch", "delete"];

/// Cluster-scoped reads: queue capacity and hardware are cluster facts.
fn cluster_role() -> rbac::ClusterRole {
    rbac::ClusterRole {
        metadata: meta(format!("{NAME}-cluster"), None),
        rules: Some(vec![
            rule(
                &[KUEUE_GROUP],
                &["clusterqueues", "resourceflavors"],
                &["get", "list"],
            ),
            rule(&[""], &["nodes"], &["get", "list"]),
        ]),
        ..Default::default()
    }
}

fn cluster_role_binding(cfg: &KueueCfg) -> rbac::ClusterRoleBinding {
    rbac::ClusterRoleBinding {
        metadata: meta(format!("{NAME}-cluster"), None),
        role_ref: role_ref("ClusterRole", &format!("{NAME}-cluster")),
        subjects: Some(vec![server_subject(cfg)]),
    }
}

/// Namespaced workload verbs, granted only where a RoleBinding names them.
fn workload_cluster_role() -> rbac::ClusterRole {
    rbac::ClusterRole {
        metadata: meta(format!("{NAME}-workloads"), None),
        rules: Some(vec![
            rule(&[KUEUE_GROUP], &["localqueues"], &["get", "list"]),
            rule(&[KUEUE_GROUP], &["workloads"], &["get", "list", "patch"]),
            rule(
                &[VISIBILITY_GROUP],
                &["localqueues/pendingworkloads"],
                &["get", "list"],
            ),
            rule(&["batch"], &["jobs"], WORKLOAD_VERBS),
            rule(&["ray.io"], &["rayjobs"], WORKLOAD_VERBS),
            rule(
                &["leaderworkerset.x-k8s.io"],
                &["leaderworkersets"],
                WORKLOAD_VERBS,
            ),
            rule(&["apps"], &["deployments", "statefulsets"], WORKLOAD_VERBS),
            rule(&[""], &["pods", "pods/log", "events"], &["get", "list"]),
        ]),
        ..Default::default()
    }
}

fn workload_binding(cfg: &KueueCfg) -> rbac::RoleBinding {
    rbac::RoleBinding {
        metadata: meta(
            format!("{NAME}-workloads"),
            Some(cfg.workload_namespace.as_str()),
        ),
        role_ref: role_ref("ClusterRole", &format!("{NAME}-workloads")),
        subjects: Some(vec![server_subject(cfg)]),
    }
}

fn record_reader_role(cfg: &KueueCfg) -> rbac::Role {
    rbac::Role {
        metadata: meta(
            format!("{NAME}-records-read"),
            Some(cfg.tenant_namespace.as_str()),
        ),
        rules: Some(vec![rule(
            &[""],
            &["configmaps"],
            &["get", "list", "watch"],
        )]),
    }
}

fn record_reader_binding(cfg: &KueueCfg) -> rbac::RoleBinding {
    rbac::RoleBinding {
        metadata: meta(
            format!("{NAME}-records-read"),
            Some(cfg.tenant_namespace.as_str()),
        ),
        role_ref: role_ref("Role", &format!("{NAME}-records-read")),
        subjects: Some(vec![server_subject(cfg)]),
    }
}

fn record_writer_role(cfg: &KueueCfg) -> rbac::Role {
    rbac::Role {
        metadata: meta(
            format!("{NAME}-records-write"),
            Some(cfg.tenant_namespace.as_str()),
        ),
        rules: Some(vec![rule(
            &[""],
            &["configmaps"],
            &["create", "get", "list", "delete"],
        )]),
    }
}

fn record_writer_binding(cfg: &KueueCfg) -> rbac::RoleBinding {
    rbac::RoleBinding {
        metadata: meta(
            format!("{NAME}-records-write"),
            Some(cfg.tenant_namespace.as_str()),
        ),
        role_ref: role_ref("Role", &format!("{NAME}-records-write")),
        subjects: Some(
            cfg.record_writers
                .iter()
                .map(|sa| rbac::Subject {
                    kind: "ServiceAccount".to_string(),
                    name: sa.name.to_string(),
                    namespace: Some(sa.namespace.to_string()),
                    ..Default::default()
                })
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::deploy::kueue::*;
    use crate::deploy::profile::DeployProfile;

    const PROFILE: &str = r#"
        [cluster]
        loop_namespace = "crucible-system"
        rig_namespace = "rig"
        service_account = "crucible-spoke"
        supervisor_image = "registry.example.com/openshell-supervisor:latest"
        [image]
        loop = "registry.example.com/crucible-loop:latest"
        pull_secret = "quay-pull"
        [kueue]
        image = "registry.example.com/crucible-kueue:latest"
        cluster = "pirate"
        namespace = "crucible-system"
        tenant_namespace = "crucible-kueue-tenants"
        workload_namespace = "crucible-kueue-e2e"
        queues = ["crucible-e2e"]
        record_writers = ["crucible-system/crucible-hub-dispatch"]
    "#;

    fn profile(text: &str) -> DeployProfile {
        toml::from_str(text).unwrap()
    }

    fn docs(text: &str) -> Vec<serde_json::Value> {
        let yaml = render(&profile(text), &RenderOpts::default()).unwrap();
        yaml.split("---\n")
            .map(|d| serde_norway::from_str(d).unwrap())
            .collect()
    }

    fn find<'a>(docs: &'a [serde_json::Value], kind: &str, name: &str) -> &'a serde_json::Value {
        docs.iter()
            .find(|d| d["kind"] == kind && d["metadata"]["name"] == name)
            .unwrap_or_else(|| panic!("no {kind} {name}"))
    }

    #[test]
    fn the_server_accepts_its_service_dns_name_and_watches_the_tenants_namespace() {
        let docs = docs(PROFILE);
        let deploy = find(&docs, "Deployment", "crucible-kueue");
        assert_eq!(deploy["metadata"]["namespace"], "crucible-system");
        let args: Vec<&str> = deploy["spec"]["template"]["spec"]["containers"][0]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "--bind",
                "0.0.0.0:8850",
                "--tenant-namespace",
                "crucible-kueue-tenants",
                "--allowed-host",
                "crucible-kueue.crucible-system.svc.cluster.local",
            ]
        );
        assert_eq!(
            deploy["spec"]["template"]["spec"]["serviceAccountName"],
            "crucible-kueue"
        );
        let svc = find(&docs, "Service", "crucible-kueue");
        assert_eq!(svc["spec"]["ports"][0]["port"], 8850);
        assert_eq!(
            svc["spec"]["selector"],
            deploy["spec"]["template"]["metadata"]["labels"]
        );
        find(&docs, "ServiceAccount", "crucible-kueue");
        find(&docs, "Namespace", "crucible-kueue-tenants");
    }

    #[test]
    fn workload_verbs_are_bound_only_in_the_workload_namespace() {
        let docs = docs(PROFILE);
        let binding = find(&docs, "RoleBinding", "crucible-kueue-workloads");
        assert_eq!(binding["metadata"]["namespace"], "crucible-kueue-e2e");
        assert_eq!(binding["roleRef"]["kind"], "ClusterRole");
        assert!(
            !docs.iter().any(|d| d["kind"] == "ClusterRoleBinding"
                && d["roleRef"]["name"] == "crucible-kueue-workloads"),
            "workload verbs must never be bound cluster-wide"
        );
        let cluster = find(&docs, "ClusterRole", "crucible-kueue-cluster");
        let verbs: Vec<&serde_json::Value> = cluster["rules"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r["verbs"].as_array().unwrap())
            .collect();
        assert!(verbs.iter().all(|v| *v == "get" || *v == "list"));
    }

    #[test]
    fn every_submittable_kind_is_granted_under_its_own_group_only() {
        let docs = docs(PROFILE);
        let role = find(&docs, "ClusterRole", "crucible-kueue-workloads");
        let granted: Vec<(String, String)> = role["rules"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["verbs"].as_array().unwrap().iter().any(|v| v == "create"))
            .flat_map(|r| {
                let groups = r["apiGroups"].as_array().unwrap().clone();
                let resources = r["resources"].as_array().unwrap().clone();
                groups.into_iter().flat_map(move |g| {
                    resources.clone().into_iter().map(move |res| {
                        (
                            g.as_str().unwrap().to_string(),
                            res.as_str().unwrap().to_string(),
                        )
                    })
                })
            })
            .collect();
        let mut expected: Vec<(String, String)> = crucible_kueue_kinds()
            .into_iter()
            .map(|(g, r)| (g.to_string(), r.to_string()))
            .collect();
        let mut granted_sorted = granted.clone();
        granted_sorted.sort();
        expected.sort();
        assert_eq!(granted_sorted, expected);
    }

    fn crucible_kueue_kinds() -> Vec<(&'static str, &'static str)> {
        vec![
            ("batch", "jobs"),
            ("ray.io", "rayjobs"),
            ("leaderworkerset.x-k8s.io", "leaderworkersets"),
            ("apps", "deployments"),
            ("apps", "statefulsets"),
        ]
    }

    #[test]
    fn only_record_writers_may_write_tenant_records_and_the_server_only_reads_them() {
        let docs = docs(PROFILE);
        let writers = find(&docs, "RoleBinding", "crucible-kueue-records-write");
        assert_eq!(writers["metadata"]["namespace"], "crucible-kueue-tenants");
        assert_eq!(writers["subjects"][0]["name"], "crucible-hub-dispatch");
        assert_eq!(writers["subjects"][0]["namespace"], "crucible-system");
        let reader = find(&docs, "Role", "crucible-kueue-records-read");
        assert_eq!(
            reader["rules"][0]["verbs"],
            serde_json::json!(["get", "list", "watch"])
        );
        let readers = find(&docs, "RoleBinding", "crucible-kueue-records-read");
        assert_eq!(readers["subjects"][0]["name"], "crucible-kueue");
    }

    #[test]
    fn a_profile_without_kueue_refuses_to_render() {
        let without = PROFILE.split("[kueue]").next().unwrap();
        let err = render(&profile(without), &RenderOpts::default()).unwrap_err();
        assert!(err.downcast_ref::<MissingKueueTable>().is_some());
    }

    #[test]
    fn malformed_kueue_names_fail_the_profile_load() {
        let bad_ns = PROFILE.replace("crucible-kueue-e2e", "Not_A_Namespace");
        let err = toml::from_str::<DeployProfile>(&bad_ns)
            .err()
            .expect("refused");
        assert!(err.to_string().contains("Not_A_Namespace"), "{err}");
        let bad_sa = PROFILE.replace("crucible-system/crucible-hub-dispatch", "crucible-hub");
        let err = toml::from_str::<DeployProfile>(&bad_sa)
            .err()
            .expect("refused");
        assert!(err.to_string().contains("namespace/name"), "{err}");
    }

    #[test]
    fn defaults_scope_a_run_as_submitter_on_the_hub_for_a_day() {
        let cfg = profile(PROFILE.replace("cluster = \"pirate\"\n", "").as_str())
            .kueue
            .unwrap();
        assert_eq!(cfg.cluster, "hub");
        assert_eq!(cfg.role, crucible_contract::kueue::TenantRole::Submitter);
        assert_eq!(cfg.token_ttl_secs, 86_400);
        assert_eq!(
            cfg.url(),
            "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp"
        );
    }

    #[test]
    fn the_service_host_follows_the_cluster_domain() {
        let text = PROFILE.replace(
            "record_writers",
            "cluster_domain = \"corp.example\"\n        record_writers",
        );
        let cfg = profile(&text).kueue.unwrap();
        assert_eq!(
            cfg.service_host(),
            "crucible-kueue.crucible-system.svc.corp.example"
        );
    }
}
