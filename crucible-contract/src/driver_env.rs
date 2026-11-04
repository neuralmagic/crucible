//! Env contracts between the pods the engine renders and the processes that read them: the
//! in-pod OpenShell gateway's compute-driver settings, and the broker's run facts. Each struct
//! writes its variables and reads them back, so the renderer and the reader cannot drift.

/// The OpenShell supervisor image the compute driver launches every sandbox with.
pub const SUPERVISOR_IMAGE: &str = "OPENSHELL_SUPERVISOR_IMAGE";
/// The image carrying the trusted `openshell-sandbox` binary.
pub const SANDBOX_RUNTIME_IMAGE: &str = "OPENSHELL_SANDBOX_RUNTIME_IMAGE";
/// The namespace the kubernetes driver creates sandboxes in.
pub const SANDBOX_NAMESPACE: &str = "CRUCIBLE_SANDBOX_NAMESPACE";
/// The service account sandbox pods run as.
pub const SANDBOX_SERVICE_ACCOUNT: &str = "CRUCIBLE_SANDBOX_SERVICE_ACCOUNT";
/// The sandbox image a turn runs when it names none.
pub const SANDBOX_DEFAULT_IMAGE: &str = "CRUCIBLE_SANDBOX_DEFAULT_IMAGE";
/// Comma-separated `kubernetes.io/dockerconfigjson` secrets the kubelet pulls sandbox images with.
pub const SANDBOX_IMAGE_PULL_SECRETS: &str = "CRUCIBLE_SANDBOX_IMAGE_PULL_SECRETS";
/// The `[cluster].host_aliases` map as JSON.
pub const SANDBOX_HOST_ALIASES: &str = "CRUCIBLE_SANDBOX_HOST_ALIASES";
/// The `[cluster.gpu_sandbox]` placement as JSON.
pub const SANDBOX_GPU_POD: &str = "CRUCIBLE_SANDBOX_GPU_POD";
/// The SPIFFE Workload API socket the driver mounts into supervisor pods.
pub const SANDBOX_SPIFFE_SOCKET: &str = "CRUCIBLE_SANDBOX_SPIFFE_SOCKET";

/// What the in-pod gateway's compute driver is told about its sandboxes. The renderer projects
/// it from the deploy profile; the gateway reads it back at boot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxDriverEnv {
    pub supervisor_image: Option<String>,
    pub sandbox_runtime_image: Option<String>,
    pub namespace: Option<String>,
    pub service_account: Option<String>,
    pub default_image: Option<String>,
    pub image_pull_secrets: Vec<String>,
    pub host_aliases_json: Option<String>,
    pub gpu_placement_json: Option<String>,
    pub spiffe_socket: Option<String>,
}

impl SandboxDriverEnv {
    /// The two driver images, projected into every loop and turn pod.
    pub fn images_env(&self) -> Vec<(&'static str, String)> {
        [
            (SUPERVISOR_IMAGE, &self.supervisor_image),
            (SANDBOX_RUNTIME_IMAGE, &self.sandbox_runtime_image),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.clone().map(|v| (k, v)))
        .collect()
    }

    /// The kubernetes driver's sandbox settings, projected only under that driver.
    pub fn sandbox_env(&self) -> Vec<(&'static str, String)> {
        let mut out: Vec<(&'static str, String)> = [
            (SANDBOX_NAMESPACE, &self.namespace),
            (SANDBOX_SERVICE_ACCOUNT, &self.service_account),
            (SANDBOX_DEFAULT_IMAGE, &self.default_image),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.clone().map(|v| (k, v)))
        .collect();
        if !self.image_pull_secrets.is_empty() {
            out.push((
                SANDBOX_IMAGE_PULL_SECRETS,
                self.image_pull_secrets.join(","),
            ));
        }
        out.extend(
            [
                (SANDBOX_HOST_ALIASES, &self.host_aliases_json),
                (SANDBOX_GPU_POD, &self.gpu_placement_json),
                (SANDBOX_SPIFFE_SOCKET, &self.spiffe_socket),
            ]
            .into_iter()
            .filter_map(|(k, v)| v.clone().map(|v| (k, v))),
        );
        out
    }

    /// Read every variable back; unset and empty are both absent.
    pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| lookup(key).filter(|v| !v.is_empty());
        SandboxDriverEnv {
            supervisor_image: get(SUPERVISOR_IMAGE),
            sandbox_runtime_image: get(SANDBOX_RUNTIME_IMAGE),
            namespace: get(SANDBOX_NAMESPACE),
            service_account: get(SANDBOX_SERVICE_ACCOUNT),
            default_image: get(SANDBOX_DEFAULT_IMAGE),
            image_pull_secrets: get(SANDBOX_IMAGE_PULL_SECRETS)
                .map(|raw| {
                    raw.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            host_aliases_json: get(SANDBOX_HOST_ALIASES),
            gpu_placement_json: get(SANDBOX_GPU_POD),
            spiffe_socket: get(SANDBOX_SPIFFE_SOCKET),
        }
    }
}

/// Whether the broker builds and rolls the composite the agent edits.
pub const BROKER_COMPOSITE: &str = "BROKER_COMPOSITE";
/// The in-sandbox path of the agent's workspace.
pub const BROKER_SANDBOX_WORKDIR: &str = "BROKER_SANDBOX_WORKDIR";
/// Where the synced composite tree is staged for the apply hook.
pub const BROKER_COMPOSITE_CTX: &str = "BROKER_COMPOSITE_CTX";
/// The judge's `measure_cmd`, run engine-side on the agent's behalf.
pub const BROKER_MEASURE_CMD: &str = "BROKER_MEASURE_CMD";
/// The composite's apply hook.
pub const BROKER_COMPOSITE_APPLY_CMD: &str = "BROKER_COMPOSITE_APPLY_CMD";
/// The mounted kubeconfig of the spoke the codegen jobs run on.
pub const BROKER_CODEGEN_KUBECONFIG: &str = "BROKER_CODEGEN_KUBECONFIG";
/// The spoke's reachability tier, named in unreachable-spoke errors.
pub const BROKER_CODEGEN_CLUSTER_TIER: &str = "BROKER_CODEGEN_CLUSTER_TIER";
/// The HTTP CONNECT proxy the spoke's API server is reached through.
pub const BROKER_CODEGEN_PROXY_URL: &str = "BROKER_CODEGEN_PROXY_URL";

/// The run facts the loop pod projects for its broker: what the agent's workspace is, whether
/// the broker composes and measures, and how it reaches a delegated spoke.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrokerEnv {
    pub composite: bool,
    pub sandbox_workdir: Option<String>,
    pub composite_ctx: Option<String>,
    pub measure_cmd: Option<String>,
    pub composite_apply_cmd: Option<String>,
    pub codegen_kubeconfig: Option<String>,
    pub codegen_cluster_tier: Option<String>,
    pub codegen_proxy_url: Option<String>,
}

impl BrokerEnv {
    /// The run facts, in the order the loop pod projects them.
    pub fn run_env(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if self.composite {
            out.push((BROKER_COMPOSITE, "1".to_string()));
        }
        out.extend(
            [
                (BROKER_SANDBOX_WORKDIR, &self.sandbox_workdir),
                (BROKER_COMPOSITE_CTX, &self.composite_ctx),
                (BROKER_MEASURE_CMD, &self.measure_cmd),
                (BROKER_COMPOSITE_APPLY_CMD, &self.composite_apply_cmd),
            ]
            .into_iter()
            .filter_map(|(k, v)| v.clone().map(|v| (k, v))),
        );
        out
    }

    /// The delegated spoke's reach, projected after the codegen substrate.
    pub fn spoke_env(&self) -> Vec<(&'static str, String)> {
        [
            (BROKER_CODEGEN_KUBECONFIG, &self.codegen_kubeconfig),
            (BROKER_CODEGEN_CLUSTER_TIER, &self.codegen_cluster_tier),
            (BROKER_CODEGEN_PROXY_URL, &self.codegen_proxy_url),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.clone().map(|v| (k, v)))
        .collect()
    }

    /// Read every variable back; unset and blank are both absent.
    pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| lookup(key).filter(|v| !v.trim().is_empty());
        BrokerEnv {
            composite: matches!(
                lookup(BROKER_COMPOSITE).as_deref(),
                Some("1") | Some("true")
            ),
            sandbox_workdir: get(BROKER_SANDBOX_WORKDIR),
            composite_ctx: get(BROKER_COMPOSITE_CTX),
            measure_cmd: get(BROKER_MEASURE_CMD),
            composite_apply_cmd: get(BROKER_COMPOSITE_APPLY_CMD),
            codegen_kubeconfig: get(BROKER_CODEGEN_KUBECONFIG),
            codegen_cluster_tier: get(BROKER_CODEGEN_CLUSTER_TIER),
            codegen_proxy_url: get(BROKER_CODEGEN_PROXY_URL),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::driver_env::*;

    fn lookup(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn the_driver_env_round_trips_and_leaves_unset_fields_out() {
        let env = SandboxDriverEnv {
            supervisor_image: Some("sup:1".into()),
            sandbox_runtime_image: Some("rt:1".into()),
            namespace: Some("sandboxes".into()),
            service_account: None,
            default_image: Some("agent:1".into()),
            image_pull_secrets: vec!["a".into(), "b".into()],
            host_aliases_json: None,
            gpu_placement_json: Some("{}".into()),
            spiffe_socket: None,
        };
        let mut projected = env.images_env();
        projected.extend(env.sandbox_env());
        let names: Vec<&str> = projected.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            names,
            [
                SUPERVISOR_IMAGE,
                SANDBOX_RUNTIME_IMAGE,
                SANDBOX_NAMESPACE,
                SANDBOX_DEFAULT_IMAGE,
                SANDBOX_IMAGE_PULL_SECRETS,
                SANDBOX_GPU_POD,
            ]
        );
        assert_eq!(SandboxDriverEnv::from_env(lookup(projected)), env);
    }

    #[test]
    fn blank_values_read_as_absent() {
        let env = SandboxDriverEnv::from_env(lookup(vec![
            (SANDBOX_NAMESPACE, String::new()),
            (SANDBOX_IMAGE_PULL_SECRETS, " , ".into()),
        ]));
        assert_eq!(env, SandboxDriverEnv::default());
    }

    #[test]
    fn the_broker_env_round_trips_in_projection_order() {
        let env = BrokerEnv {
            composite: true,
            sandbox_workdir: Some("/sandbox/ws".into()),
            composite_ctx: Some("/var/lib/forge/composite-ctx".into()),
            measure_cmd: None,
            composite_apply_cmd: Some("make apply".into()),
            codegen_kubeconfig: Some("/etc/crucible/spoke/kubeconfig".into()),
            codegen_cluster_tier: Some("proxy".into()),
            codegen_proxy_url: None,
        };
        let mut projected = env.run_env();
        projected.extend(env.spoke_env());
        let names: Vec<&str> = projected.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            names,
            [
                BROKER_COMPOSITE,
                BROKER_SANDBOX_WORKDIR,
                BROKER_COMPOSITE_CTX,
                BROKER_COMPOSITE_APPLY_CMD,
                BROKER_CODEGEN_KUBECONFIG,
                BROKER_CODEGEN_CLUSTER_TIER,
            ]
        );
        assert_eq!(BrokerEnv::from_env(lookup(projected)), env);
        assert!(!BrokerEnv::from_env(lookup(vec![(BROKER_COMPOSITE, "0".into())])).composite);
    }
}
