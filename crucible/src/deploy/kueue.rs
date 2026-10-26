//! Render the crucible-kueue MCP server from the profile's `[kueue]` table: its ServiceAccount,
//! Deployment, and Service in `namespace`, and the RBAC that confines it. Namespaced workload verbs
//! are bound only in `workload_namespace`; cluster-wide it reads queues and nodes and reviews its
//! callers' access.

use crate::deploy::profile::{DeployProfile, KubeAuthz, KubeName, KueueCfg};
use crate::deploy::render::{RenderOpts, pin_image};
use anyhow::{Context, Result};
use forge::fleet::Platform;
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
     workload_namespaces)"
)]
pub struct MissingKueueTable;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KueueClusterError {
    #[error("[kueue] serves {clusters:?}; name the one to render with --kueue-cluster")]
    Several { clusters: Vec<String> },
    #[error("[kueue] does not serve {cluster:?}; it serves {clusters:?}")]
    NotServed {
        cluster: String,
        clusters: Vec<String>,
    },
}

/// The `[kueue] clusters` entry a render is for: `requested`, or the only one.
fn render_cluster<'a>(
    cfg: &'a KueueCfg,
    requested: Option<&str>,
) -> Result<&'a str, KueueClusterError> {
    match (requested, cfg.clusters.as_slice()) {
        (Some(c), clusters) => clusters
            .iter()
            .find(|served| *served == c)
            .map(String::as_str)
            .ok_or_else(|| KueueClusterError::NotServed {
                cluster: c.to_string(),
                clusters: clusters.to_vec(),
            }),
        (None, [only]) => Ok(only),
        (None, clusters) => Err(KueueClusterError::Several {
            clusters: clusters.to_vec(),
        }),
    }
}

/// The server's objects for `cluster` (see [`render_cluster`]) as one multi-document YAML string.
/// What a crucible-kueue render needs beyond the profile.
#[derive(Debug, Clone, Copy)]
pub struct KueueTarget<'a> {
    /// The `[kueue] clusters` entry to render for; required when the table names several.
    pub cluster: Option<&'a str>,
    /// The controller's agent issuer (`CONTROLLER_AGENT_ISSUER`), whose tokens the server takes.
    pub issuer: &'a str,
    /// The cluster's platform, when the render cannot ask it: over the profile's `platform`, which
    /// is over `kubernetes`.
    pub platform: Option<Platform>,
}

pub fn render(profile: &DeployProfile, opts: &RenderOpts, target: &KueueTarget) -> Result<String> {
    let cfg = profile.kueue.as_ref().ok_or(MissingKueueTable)?;
    let cluster = render_cluster(cfg, target.cluster)?;
    let platform = target
        .platform
        .or_else(|| profile.platform(cluster))
        .unwrap_or_default();
    let authz = cfg.kube_authz(platform);
    let image = pin_image(opts.digests.as_deref(), "crucible-kueue image", &cfg.image)?;
    let pull_secret = profile.image.pull_secret.as_str();
    let mut docs = Vec::new();
    let mut push = |what: &str, doc: Result<String, serde_norway::Error>| -> Result<()> {
        docs.push(doc.with_context(|| format!("serializing the crucible-kueue {what}"))?);
        Ok(())
    };
    for namespace in &cfg.workload_namespaces {
        push(
            "workload Namespace",
            yaml(&workload_namespace(namespace, platform)),
        )?;
    }
    push("ServiceAccount", yaml(&service_account(cfg)))?;
    push(
        "Deployment",
        yaml(&deployment(
            cfg,
            cluster,
            target.issuer,
            authz,
            &image,
            pull_secret,
        )?),
    )?;
    push("Service", yaml(&service(cfg)))?;
    push("cluster ClusterRole", yaml(&cluster_role(authz)))?;
    push(
        "cluster ClusterRoleBinding",
        yaml(&cluster_role_binding(cfg)),
    )?;
    push("workload ClusterRole", yaml(&workload_cluster_role()))?;
    for namespace in &cfg.workload_namespaces {
        push(
            "workload RoleBinding",
            yaml(&workload_binding(cfg, namespace)),
        )?;
    }
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

/// Where agents' workloads run. On OpenShift, under Pod Security `restricted` (its SCCs fill in
/// the fields `restricted` asks for) and the Red Hat build of Kueue's managed label. On plain
/// Kubernetes, under `baseline`: nothing fills those fields in, and an agent's pod template rarely
/// sets them; upstream Kueue manages any namespace its `managedJobsNamespaceSelector` admits.
fn workload_namespace(namespace: &KubeName, platform: Platform) -> core::Namespace {
    let mut metadata = meta(namespace.as_str(), None);
    let labels = metadata.labels.get_or_insert_with(BTreeMap::new);
    let enforce = match platform {
        Platform::Openshift => {
            labels.insert(KUEUE_MANAGED_LABEL.to_string(), "true".to_string());
            "restricted"
        }
        Platform::Kubernetes => "baseline",
    };
    labels.insert(
        "pod-security.kubernetes.io/enforce".to_string(),
        enforce.to_string(),
    );
    core::Namespace {
        metadata,
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

fn args(cfg: &KueueCfg, cluster: &str, issuer: &str, authz: KubeAuthz) -> Result<Vec<String>> {
    let jwks = cfg
        .run_jwks_uri(issuer)
        .with_context(|| format!("deriving the key set URI of {issuer}"))?;
    let mut args = vec![
        "--bind".to_string(),
        format!("0.0.0.0:{}", KueueCfg::PORT),
        "--allowed-host".to_string(),
        cfg.service_host(),
        "--run-issuer".to_string(),
        format!("{issuer}={jwks}"),
        "--resource".to_string(),
        cfg.url(),
        "--kube-authz".to_string(),
        authz.as_str().to_string(),
        "--cluster".to_string(),
        cluster.to_string(),
    ];
    for namespace in &cfg.workload_namespaces {
        args.extend(["--namespace".to_string(), namespace.to_string()]);
    }
    if cfg.people {
        args.extend(["--person-issuer".to_string(), format!("{issuer}={jwks}")]);
        let people = cfg.people_url(cluster);
        if people != cfg.url() {
            let host = url::Url::parse(&people)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .with_context(|| format!("[kueue] people_urls entry {people} names no host"))?;
            args.extend([
                "--resource".to_string(),
                people,
                "--allowed-host".to_string(),
                host,
            ]);
        }
    }
    for secret in &cfg.pull_secrets {
        args.extend(["--pull-secret".to_string(), secret.clone()]);
    }
    if jwks.scheme() == "http" {
        args.push("--insecure-jwks-http".to_string());
    }
    Ok(args)
}

fn deployment(
    cfg: &KueueCfg,
    cluster: &str,
    issuer: &str,
    authz: KubeAuthz,
    image: &str,
    pull_secret: &str,
) -> Result<apps::Deployment> {
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
        args: Some(args(cfg, cluster, issuer, authz)?),
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
            seccomp_profile: Some(core::SeccompProfile {
                type_: "RuntimeDefault".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    Ok(apps::Deployment {
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
    })
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
/// The label the Red Hat build of Kueue manages a namespace's workloads under.
const KUEUE_MANAGED_LABEL: &str = "kueue.openshift.io/managed";
const VISIBILITY_GROUP: &str = "visibility.kueue.x-k8s.io";
const WORKLOAD_VERBS: &[&str] = &["create", "get", "list", "patch", "delete"];

/// Cluster-scoped reads (queue capacity and hardware are cluster facts) and the reviews that
/// decide each caller's access as its `kube_user`.
fn cluster_role(authz: KubeAuthz) -> rbac::ClusterRole {
    let mut rules = vec![
        rule(
            &[KUEUE_GROUP],
            &["clusterqueues", "resourceflavors"],
            &["get", "list"],
        ),
        rule(&[""], &["nodes"], &["get", "list"]),
        rule(
            &["authorization.k8s.io"],
            &["subjectaccessreviews"],
            &["create"],
        ),
    ];
    if authz == KubeAuthz::Openshift {
        rules.push(rule(&["user.openshift.io"], &["groups"], &["list"]));
    }
    rbac::ClusterRole {
        metadata: meta(format!("{NAME}-cluster"), None),
        rules: Some(rules),
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
            rule(&["jobset.x-k8s.io"], &["jobsets"], WORKLOAD_VERBS),
            rule(&["kubeflow.org"], &["pytorchjobs"], WORKLOAD_VERBS),
            rule(&[""], &["pods", "pods/log", "events"], &["get", "list"]),
        ]),
        ..Default::default()
    }
}

fn workload_binding(cfg: &KueueCfg, namespace: &KubeName) -> rbac::RoleBinding {
    rbac::RoleBinding {
        metadata: meta(format!("{NAME}-workloads"), Some(namespace.as_str())),
        role_ref: role_ref("ClusterRole", &format!("{NAME}-workloads")),
        subjects: Some(vec![server_subject(cfg)]),
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
        cluster = "ocp"
        namespace = "crucible-system"
        workload_namespace = "crucible-kueue-e2e"
        kube_authz = "openshift"
    "#;

    fn profile(text: &str) -> DeployProfile {
        toml::from_str(text).unwrap()
    }

    const ISSUER: &str = "http://crucible-controller.crucible-system.svc.cluster.local:8080";

    fn target(cluster: Option<&str>) -> KueueTarget<'_> {
        KueueTarget {
            cluster,
            issuer: ISSUER,
            platform: None,
        }
    }

    fn docs(text: &str) -> Vec<serde_json::Value> {
        let yaml = render(&profile(text), &RenderOpts::default(), &target(None)).unwrap();
        yaml.split("---\n")
            .map(|d| serde_norway::from_str(d).unwrap())
            .collect()
    }

    fn find<'a>(docs: &'a [serde_json::Value], kind: &str, name: &str) -> &'a serde_json::Value {
        docs.iter()
            .find(|d| d["kind"] == kind && d["metadata"]["name"] == name)
            .unwrap_or_else(|| panic!("no {kind} {name}"))
    }

    fn args(docs: &[serde_json::Value]) -> Vec<String> {
        find(docs, "Deployment", "crucible-kueue")["spec"]["template"]["spec"]["containers"][0]
            ["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_server_trusts_the_controllers_run_tokens_for_its_own_service_url() {
        let docs = docs(PROFILE);
        let deploy = find(&docs, "Deployment", "crucible-kueue");
        assert_eq!(deploy["metadata"]["namespace"], "crucible-system");
        assert_eq!(
            args(&docs),
            [
                "--bind",
                "0.0.0.0:8850",
                "--allowed-host",
                "crucible-kueue.crucible-system.svc.cluster.local",
                "--run-issuer",
                "http://crucible-controller.crucible-system.svc.cluster.local:8080=\
                 http://crucible-controller.crucible-system.svc.cluster.local:8080/.well-known/jwks.json",
                "--resource",
                "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp",
                "--kube-authz",
                "openshift",
                "--cluster",
                "ocp",
                "--namespace",
                "crucible-kueue-e2e",
                "--insecure-jwks-http",
            ]
        );
        let pulling = crate::deploy::kueue::tests::docs(&PROFILE.replace(
            "[kueue]",
            "[kueue]\npull_secrets = [\"quay-pull\", \"ghcr\"]",
        ));
        let rendered = args(&pulling);
        assert!(
            rendered
                .windows(4)
                .any(|w| w == ["--pull-secret", "quay-pull", "--pull-secret", "ghcr"]),
            "{rendered:?}"
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
        assert_eq!(
            docs.iter()
                .filter(|d| d["kind"] == "Namespace")
                .map(|d| d["metadata"]["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["crucible-kueue-e2e"]
        );
    }

    #[test]
    fn people_are_accepted_from_the_same_issuer_only_when_the_profile_says_so() {
        let person_issuer = |text: &str| {
            let args = args(&docs(text));
            args.windows(2)
                .find(|w| w[0] == "--person-issuer")
                .map(|w| w[1].clone())
        };
        assert_eq!(person_issuer(PROFILE), None);
        assert_eq!(
            person_issuer(&PROFILE.replace("[kueue]", "[kueue]\npeople = true")).as_deref(),
            Some(
                "http://crucible-controller.crucible-system.svc.cluster.local:8080=\
                 http://crucible-controller.crucible-system.svc.cluster.local:8080/.well-known/jwks.json"
            )
        );
    }

    /// A people URL other than the Service URL is served too: people's tokens name it.
    #[test]
    fn a_people_url_is_another_resource_and_host() {
        let flag = |args: &[String], name: &str| -> Vec<String> {
            args.windows(2)
                .filter(|w| w[0] == name)
                .map(|w| w[1].clone())
                .collect()
        };
        let plain = args(&docs(&PROFILE.replace("[kueue]", "[kueue]\npeople = true")));
        assert_eq!(flag(&plain, "--resource").len(), 1, "the Service URL alone");
        let routed = args(&docs(&PROFILE.replace(
            "[kueue]",
            "[kueue]\npeople = true\npeople_urls = { ocp = \"https://kueue.ocp.example/mcp\" }",
        )));
        assert_eq!(
            flag(&routed, "--resource"),
            [
                "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp",
                "https://kueue.ocp.example/mcp"
            ]
        );
        assert!(flag(&routed, "--allowed-host").contains(&"kueue.ocp.example".to_string()));
    }

    #[test]
    fn an_https_key_set_named_in_the_profile_needs_no_insecure_flag() {
        let text = PROFILE.replace(
            "kube_authz",
            "run_jwks_uri = \"https://controller.example/.well-known/jwks.json\"\n        kube_authz",
        );
        let args = args(&docs(&text));
        assert!(
            args.contains(
                &"http://crucible-controller.crucible-system.svc.cluster.local:8080=\
              https://controller.example/.well-known/jwks.json"
                    .to_string()
            )
        );
        assert!(
            !args.contains(&"--insecure-jwks-http".to_string()),
            "{args:?}"
        );
    }

    #[test]
    fn the_server_reviews_access_and_reads_openshift_groups_only_when_told_to() {
        let rules = |text: &str| {
            let docs = docs(text);
            find(&docs, "ClusterRole", "crucible-kueue-cluster")["rules"].clone()
        };
        let review = serde_json::json!({
            "apiGroups": ["authorization.k8s.io"],
            "resources": ["subjectaccessreviews"],
            "verbs": ["create"],
        });
        let groups = serde_json::json!({
            "apiGroups": ["user.openshift.io"],
            "resources": ["groups"],
            "verbs": ["list"],
        });
        let openshift = rules(PROFILE);
        let openshift = openshift.as_array().unwrap();
        assert!(openshift.contains(&review) && openshift.contains(&groups));
        let token = rules(&PROFILE.replace("\"openshift\"", "\"token\""));
        let token = token.as_array().unwrap();
        assert!(token.contains(&review) && !token.contains(&groups));
        assert!(
            args(&docs(&PROFILE.replace("\"openshift\"", "\"token\"")))
                .contains(&"token".to_string())
        );
    }

    /// A hub on OpenShift dispatching to an OpenShift spoke and a plain Kubernetes spoke renders
    /// each spoke's server for that spoke's platform, never the hub's.
    /// The server's pod passes Pod Security `restricted` without an SCC to fill fields in.
    #[test]
    fn the_server_pod_meets_restricted_pod_security_on_its_own() {
        let docs = docs(PROFILE);
        let c = &find(&docs, "Deployment", "crucible-kueue")["spec"]["template"]["spec"]["containers"]
            [0]["securityContext"];
        assert_eq!(c["allowPrivilegeEscalation"], false);
        assert_eq!(c["runAsNonRoot"], true);
        assert_eq!(c["capabilities"]["drop"], serde_json::json!(["ALL"]));
        assert_eq!(c["seccompProfile"]["type"], "RuntimeDefault");
    }

    #[test]
    fn one_profile_renders_each_cluster_for_its_own_platform() {
        let fleet = profile(
            &PROFILE
                .replace("cluster = \"ocp\"", "clusters = [\"ocp\", \"k8s\"]")
                .replace("kube_authz = \"openshift\"", "")
                .replace(
                    "[kueue]",
                    "[clusters.ocp]\nkubeconfig_secret = \"p\"\nplatform = \"openshift\"\n\
                     [clusters.k8s]\nkubeconfig_secret = \"w\"\nplatform = \"kubernetes\"\n[kueue]",
                )
                .replace("[cluster]\n", "[cluster]\nplatform = \"openshift\"\n"),
        );
        let rendered = |cluster: &str| -> Vec<serde_json::Value> {
            render(&fleet, &RenderOpts::default(), &target(Some(cluster)))
                .unwrap()
                .split("---\n")
                .map(|d| serde_norway::from_str(d).unwrap())
                .collect()
        };
        let ocp = rendered("ocp");
        let k8s = rendered("k8s");
        let labels = |docs: &[serde_json::Value]| {
            find(docs, "Namespace", "crucible-kueue-e2e")["metadata"]["labels"].clone()
        };
        assert_eq!(
            labels(&ocp)["pod-security.kubernetes.io/enforce"],
            "restricted"
        );
        assert_eq!(labels(&ocp)["kueue.openshift.io/managed"], "true");
        assert_eq!(
            labels(&k8s)["pod-security.kubernetes.io/enforce"],
            "baseline"
        );
        assert!(labels(&k8s).get("kueue.openshift.io/managed").is_none());
        let authz = |docs: &[serde_json::Value]| {
            let a = args(docs);
            a.windows(2)
                .find(|w| w[0] == "--kube-authz")
                .map(|w| w[1].clone())
        };
        assert_eq!(authz(&ocp).as_deref(), Some("openshift"));
        assert_eq!(
            authz(&k8s).as_deref(),
            Some("token"),
            "the hub is OpenShift; k8s is not"
        );
        for docs in [&ocp, &k8s] {
            assert!(
                !docs
                    .iter()
                    .any(|d| d.to_string().contains("securitycontextconstraints")),
                "SCC grants are the operator's, on either platform"
            );
        }
    }

    #[test]
    fn the_workload_namespace_and_the_groups_source_follow_the_clusters_platform() {
        let openshift = PROFILE.replace(
            "[kueue]",
            "[clusters.ocp]\nkubeconfig_secret = \"k\"\nplatform = \"openshift\"\n[kueue]",
        );
        let without_authz = |text: &str| text.replace("kube_authz = \"openshift\"", "");
        let on_openshift = docs(&without_authz(&openshift));
        let ns = find(&on_openshift, "Namespace", "crucible-kueue-e2e");
        assert_eq!(
            ns["metadata"]["labels"]["pod-security.kubernetes.io/enforce"], "restricted",
            "OpenShift's SCCs fill in what restricted asks for"
        );
        assert_eq!(
            ns["metadata"]["labels"]["kueue.openshift.io/managed"], "true",
            "OpenShift's Kueue admits workloads only in namespaces carrying it"
        );
        let a = args(&on_openshift);
        assert!(
            a.windows(2).any(|w| w == ["--kube-authz", "openshift"]),
            "{a:?}"
        );

        let on_kubernetes = docs(&without_authz(PROFILE));
        let ns = find(&on_kubernetes, "Namespace", "crucible-kueue-e2e");
        assert_eq!(
            ns["metadata"]["labels"]["pod-security.kubernetes.io/enforce"], "baseline",
            "nothing fills in restricted's fields on plain Kubernetes"
        );
        assert!(
            ns["metadata"]["labels"]
                .get("kueue.openshift.io/managed")
                .is_none()
        );
        let a = args(&on_kubernetes);
        assert!(
            a.windows(2).any(|w| w == ["--kube-authz", "token"]),
            "{a:?}"
        );
        let rules =
            find(&on_kubernetes, "ClusterRole", "crucible-kueue-cluster")["rules"].to_string();
        assert!(!rules.contains("user.openshift.io"), "{rules}");
        let explicit = docs(&openshift);
        assert!(
            args(&explicit)
                .windows(2)
                .any(|w| w == ["--kube-authz", "openshift"]),
            "an explicit kube_authz wins"
        );
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
        for rule in cluster["rules"].as_array().unwrap() {
            let verbs = rule["verbs"].as_array().unwrap();
            assert!(
                verbs.iter().all(|v| *v == "get" || *v == "list")
                    || rule["resources"] == serde_json::json!(["subjectaccessreviews"]),
                "cluster-wide, the server only reads and reviews: {rule}"
            );
        }
    }

    #[test]
    fn the_server_is_rendered_for_one_cluster_and_accepts_only_its_tokens() {
        let cluster_flag = |args: &[String]| {
            args.windows(2)
                .find(|w| w[0] == "--cluster")
                .map(|w| w[1].clone())
        };
        assert_eq!(
            cluster_flag(&args(&docs(PROFILE))).as_deref(),
            Some("ocp"),
            "the only cluster"
        );
        let two = profile(&PROFILE.replace("cluster = \"ocp\"", "clusters = [\"ocp\", \"k8s\"]"));
        let rendered = render(&two, &RenderOpts::default(), &target(Some("k8s"))).unwrap();
        let docs: Vec<serde_json::Value> = rendered
            .split("---\n")
            .map(|d| serde_norway::from_str(d).unwrap())
            .collect();
        assert_eq!(cluster_flag(&args(&docs)).as_deref(), Some("k8s"));
        let several = render(&two, &RenderOpts::default(), &target(None)).unwrap_err();
        assert_eq!(
            several.downcast_ref::<KueueClusterError>(),
            Some(&KueueClusterError::Several {
                clusters: vec!["ocp".into(), "k8s".into()]
            })
        );
        let elsewhere = render(&two, &RenderOpts::default(), &target(Some("east"))).unwrap_err();
        assert!(matches!(
            elsewhere.downcast_ref::<KueueClusterError>(),
            Some(KueueClusterError::NotServed { .. })
        ));
    }

    #[test]
    fn each_workload_namespace_gets_its_namespace_binding_and_flag() {
        let many = PROFILE.replace(
            r#"workload_namespace = "crucible-kueue-e2e""#,
            r#"workload_namespaces = ["team-a", "team-b"]"#,
        );
        let docs = docs(&many);
        for ns in ["team-a", "team-b"] {
            let namespace = find(&docs, "Namespace", ns);
            assert_eq!(
                namespace["metadata"]["labels"]["pod-security.kubernetes.io/enforce"],
                "baseline"
            );
            assert!(docs.iter().any(|d| d["kind"] == "RoleBinding"
                && d["metadata"]["name"] == "crucible-kueue-workloads"
                && d["metadata"]["namespace"] == ns));
        }
        let args = args(&docs);
        let served: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "--namespace")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(served, ["team-a", "team-b"]);
        let empty = PROFILE.replace(
            r#"workload_namespace = "crucible-kueue-e2e""#,
            "workload_namespaces = []",
        );
        assert!(toml::from_str::<DeployProfile>(&empty).is_err());
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
            ("jobset.x-k8s.io", "jobsets"),
            ("kubeflow.org", "pytorchjobs"),
        ]
    }

    #[test]
    fn a_profile_without_kueue_refuses_to_render() {
        let without = PROFILE.split("[kueue]").next().unwrap();
        let err = render(&profile(without), &RenderOpts::default(), &target(None)).unwrap_err();
        assert!(err.downcast_ref::<MissingKueueTable>().is_some());
    }

    #[test]
    fn malformed_kueue_names_fail_the_profile_load() {
        let bad_ns = PROFILE.replace("crucible-kueue-e2e", "Not_A_Namespace");
        let err = toml::from_str::<DeployProfile>(&bad_ns)
            .err()
            .expect("refused");
        assert!(err.to_string().contains("Not_A_Namespace"), "{err}");
        let bad_authz = PROFILE.replace("\"openshift\"", "\"ldap\"");
        let err = toml::from_str::<DeployProfile>(&bad_authz)
            .err()
            .expect("refused");
        assert!(err.to_string().contains("ldap"), "{err}");
    }

    #[test]
    fn the_server_defaults_to_the_hub() {
        let cfg = profile(PROFILE.replace("cluster = \"ocp\"\n", "").as_str())
            .kueue
            .unwrap();
        assert_eq!(cfg.clusters, ["hub"]);
        assert_eq!(
            cfg.url(),
            "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp"
        );
    }

    #[test]
    fn the_server_may_run_on_several_clusters() {
        let one = profile(PROFILE).kueue.unwrap();
        assert!(one.serves("ocp") && !one.serves("k8s") && !one.serves("hub"));
        let many = profile(
            PROFILE
                .replace("cluster = \"ocp\"", "clusters = [\"ocp\", \"k8s\"]")
                .as_str(),
        )
        .kueue
        .unwrap();
        assert!(many.serves("ocp") && many.serves("k8s") && !many.serves("hub"));
        let none = toml::from_str::<crate::deploy::profile::DeployProfile>(
            &PROFILE.replace("cluster = \"ocp\"", "clusters = []"),
        );
        assert!(none.is_err(), "a [kueue] table on no cluster is refused");
    }

    #[test]
    fn the_service_host_follows_the_cluster_domain() {
        let text = PROFILE.replace(
            "kube_authz",
            "cluster_domain = \"corp.example\"\n        kube_authz",
        );
        let cfg = profile(&text).kueue.unwrap();
        assert_eq!(
            cfg.service_host(),
            "crucible-kueue.crucible-system.svc.corp.example"
        );
    }
}
