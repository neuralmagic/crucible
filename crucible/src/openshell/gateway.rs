//! Boot and tear down the local OpenShell gateway + rootless podman socket the openshell
//! backend needs. The command sequence was validated live against a real gateway in a
//! cluster pod, so this encodes a known-good boot, not a guess.
//!
//! The fiddly bits, all confirmed in-pod:
//!   - an externally managed Podman API socket can be supplied through
//!     `OPENSHELL_PODMAN_SOCKET` (Podman Desktop on macOS exposes one); otherwise
//!     `XDG_RUNTIME_DIR` can be empty in a pod → fall back to `/run/user/<uid>`.
//!   - under the **podman** compute driver the gateway must launch with
//!     `KUBERNETES_SERVICE_HOST`/`PORT` **scrubbed**: it auto-detects the in-cluster
//!     client-go signal and then demands a kubernetes driver config, conflicting with the
//!     podman driver. The kubernetes driver is the opposite: it *needs* those vars, since it
//!     builds its client via `kube::Config::infer()`, so the scrub is podman-specific.
//!   - `bind_address` is `0.0.0.0` so the sandbox supervisor reaches the gateway over the
//!     container bridge (not 127.0.0.1); TLS+mTLS gates it.
//!
//! The daemons (podman service, gateway) are spawned detached and live for the run; crucible
//! is the pod entrypoint, so the container runtime reaps them when crucible exits.

use crate::openshell::grpc::{GATEWAY_NAME, GATEWAY_PORT, mtls_dir};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The ways gateway startup gives up. Each carries the diagnostics an operator needs (the log
/// tail, the last status line) as fields, so the message is assembled once here.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error(
        "openshell gateway {reported} is older than the minimum {minimum} this crucible requires \
         — rebuild the loop image's gateway from the pinned fork rev {} (Cargo.lock's \
         openshell-core rev; see the openshell-gateway workflow)",
        crate::openshell::grpc::EXPECTED_GATEWAY_REV
    )]
    TooOld { reported: String, minimum: String },
    #[error(
        "{POD_IP_ENV} is unset: the kubernetes driver's supervisor pod dials the gateway at \
         this pod's IP (render it from the downward API's status.podIP)"
    )]
    PodIpMissing,
    #[error("{POD_IP_ENV}={raw:?} is not an IP address")]
    PodIpUnparseable { raw: String },
    #[error(
        "the kubernetes driver needs a dial-back address: the gateway endpoint supervisors reach \
         and the host the run's own services listen on"
    )]
    NoDialBack,
    #[error("{var} is unset; a gateway pod needs it")]
    ServeEnvMissing { var: &'static str },
    #[error("{var}={raw:?} is not an IP address")]
    ServeEnvUnparseable { var: &'static str, raw: String },
    #[error("{GATEWAY_URL_ENV} is set but {GATEWAY_CLIENT_TLS_DIR_ENV} is not")]
    RemoteTlsMissing,
    #[error("{origin} podman API socket did not appear at {}", .socket.display())]
    PodmanSocketMissing {
        origin: &'static str,
        socket: PathBuf,
    },
    #[error("generate-certs failed: {stderr}")]
    GenerateCertsFailed { stderr: String },
    #[error("gateway remove {name} failed: {stderr}")]
    RemoveRegistrationFailed { name: &'static str, stderr: String },
    #[error("gateway register failed within 30s: {last}\n{log_tail}")]
    RegisterTimeout { last: String, log_tail: String },
    #[error(
        "gateway did not become healthy within 60s\nlast `openshell status`: {last_status}\n{log_tail}"
    )]
    HealthTimeout {
        last_status: String,
        log_tail: String,
    },
}

/// The in-process OTLP collector's bind port for sandboxed turns. Fixed, not OS-assigned: under
/// the kubernetes driver the loop pod's deny-ingress NetworkPolicy admits sandbox traffic per
/// port, and a random port cannot be named there. 17671 rides next to the gateway port, which the
/// same policy already covers.
pub const OTEL_COLLECTOR_PORT: u16 = 17671;
/// The Secret name carrying the generated client mTLS material to sandbox pods (see
/// [`KubernetesDriverConfig::client_tls_secret_name`]). Published by `boot()` into the sandbox
/// namespace from the local certgen output. Used verbatim only off-cluster; in a pod,
/// [`ClientTlsSecret`] makes it unique per boot.
pub const CLIENT_TLS_SECRET: &str = "crucible-openshell-client-tls";

/// The Secret one gateway boot publishes its client mTLS material under.
///
/// Every boot generates its own CA, so a shared name means concurrent gateways overwrite each
/// other's material and a sandbox presents a credential its own gateway's CA never signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientTlsSecret {
    /// In a pod: a name no other boot uses, created once and owned by the pod. Creating needs only
    /// the unscoped secrets `create` grant, never `get` or `patch` on a name RBAC can't predict.
    PodOwned {
        name: String,
        pod: String,
        uid: String,
    },
    /// Off-cluster: [`CLIENT_TLS_SECRET`], applied over whatever an earlier boot left.
    Shared,
}

/// How [`publish_client_tls_secret`] writes the Secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublishVerb {
    Create,
    Apply,
}

impl ClientTlsSecret {
    /// The Secret for a boot of this process, keyed by `boot_id`.
    pub fn for_boot(boot_id: &str) -> Self {
        match pod_identity() {
            Some((pod, uid)) => Self::for_pod(pod, uid, boot_id),
            None => Self::Shared,
        }
    }

    pub(crate) fn for_pod(pod: String, uid: String, boot_id: &str) -> Self {
        Self::PodOwned {
            name: format!("{CLIENT_TLS_SECRET}-{pod}-{boot_id}"),
            pod,
            uid,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::PodOwned { name, .. } => name,
            Self::Shared => CLIENT_TLS_SECRET,
        }
    }

    fn owner(&self) -> Option<(String, String)> {
        match self {
            Self::PodOwned { pod, uid, .. } => Some((pod.clone(), uid.clone())),
            Self::Shared => None,
        }
    }

    pub(crate) fn verb(&self) -> PublishVerb {
        match self {
            Self::PodOwned { .. } => PublishVerb::Create,
            Self::Shared => PublishVerb::Apply,
        }
    }
}

/// A short id unique to one gateway boot: the random tail of a UUIDv7.
fn boot_id() -> String {
    format!("{:012x}", uuid::Uuid::now_v7().as_u128() & 0xffff_ffff_ffff)
}

/// This pod's `(name, uid)` from the downward API, `None` off-cluster.
fn pod_identity() -> Option<(String, String)> {
    let name = std::env::var(crucible_contract::ENV_POD_NAME).ok()?;
    let uid = std::env::var("CRUCIBLE_POD_UID").ok()?;
    (!name.is_empty() && !uid.is_empty()).then_some((name, uid))
}

/// The env the loop pod renders the sandbox S3 read role into.
pub const AWS_SANDBOX_ROLE_ENV: &str = "CRUCIBLE_AWS_SANDBOX_ROLE_ARN";

/// Where the loop pod projects its `sts.amazonaws.com`-audience ServiceAccount token.
pub const AWS_WEB_IDENTITY_TOKEN_PATH: &str = "/var/run/secrets/aws/token";

/// The sandbox S3 read role, `None` when the deployment grants none.
pub fn aws_sandbox_role() -> Option<String> {
    std::env::var(AWS_SANDBOX_ROLE_ENV)
        .ok()
        .map(|arn| arn.trim().to_string())
        .filter(|arn| !arn.is_empty())
}

/// One edit to the environment the gateway child inherits from the loop process.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvChange {
    Set(&'static str, String),
    Remove(&'static str),
}

/// The gateway child's AWS identity. The loop process runs as the publish role (`AWS_ROLE_ARN`),
/// which must never become the gateway's: with a sandbox role the gateway runs as that role via
/// the projected token, and without one it gets no role at all. IMDS stays off either way, so the
/// SDK chain cannot fall through to the node's instance role.
fn gateway_aws_env(sandbox_role: Option<&str>) -> Vec<EnvChange> {
    let mut env = match sandbox_role {
        Some(arn) => vec![
            EnvChange::Set("AWS_ROLE_ARN", arn.to_string()),
            EnvChange::Set(
                "AWS_WEB_IDENTITY_TOKEN_FILE",
                AWS_WEB_IDENTITY_TOKEN_PATH.to_string(),
            ),
        ],
        None => vec![
            EnvChange::Remove("AWS_ROLE_ARN"),
            EnvChange::Remove("AWS_WEB_IDENTITY_TOKEN_FILE"),
        ],
    };
    env.push(EnvChange::Set(
        "AWS_EC2_METADATA_DISABLED",
        "true".to_string(),
    ));
    env
}

fn apply_gateway_aws_env(cmd: &mut Command, sandbox_role: Option<&str>) {
    for change in gateway_aws_env(sandbox_role) {
        match change {
            EnvChange::Set(name, value) => cmd.env(name, value),
            EnvChange::Remove(name) => cmd.env_remove(name),
        };
    }
}

/// In-cluster k8s detection vars to strip from the gateway's environment before launch.
/// (See the module docs, load-bearing under the podman driver in a pod, a no-op on a laptop;
/// the kubernetes driver needs them, so the scrub is gated on the driver.)
pub const K8S_DETECTION_VARS: &[&str] = &["KUBERNETES_SERVICE_HOST", "KUBERNETES_SERVICE_PORT"];

/// The OpenShell compute driver that runs the agent sandbox. `Podman` nests it as a container
/// inside the loop pod (laptop/EC2); `Kubernetes` schedules it as a sibling pod in-cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComputeDriver {
    #[default]
    Podman,
    Kubernetes,
}

impl ComputeDriver {
    /// The value OpenShell expects in `compute_driver` and as the `[openshell.drivers.<name>]`
    /// table key.
    fn as_str(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Kubernetes => "kubernetes",
        }
    }

    /// The hostname the sandbox uses to reach the loop pod (broker, OTEL collector). Under
    /// `Podman` the sandbox is nested inside the loop pod and reaches it on podman's bridge as
    /// `host.containers.internal`. Under `Kubernetes` the sandbox is a sibling pod and the
    /// driver injects `host.openshell.internal` as a `hostAlias` pointing at the loop pod's IP.
    /// Hostname only; the port stays where it already lives, in `bind` and the URL builder.
    pub fn broker_host(self) -> &'static str {
        match self {
            Self::Podman => "host.containers.internal",
            Self::Kubernetes => "host.openshell.internal",
        }
    }
}

/// Whether the in-cluster k8s detection vars must be scrubbed from the gateway's environment
/// for `driver`. Podman conflicts with the client-go auto-detection; the kubernetes driver
/// depends on it (`kube::Config::infer()`), so only podman scrubs.
fn scrub_k8s_vars(driver: ComputeDriver) -> bool {
    matches!(driver, ComputeDriver::Podman)
}

/// Whether `boot()` must stand up a local rootless podman API socket for `driver`. Only the
/// podman compute driver talks to podman; under kubernetes there is no local daemon to boot,
/// so waiting for a socket that will never appear would just hang and then bail.
fn needs_podman_socket(driver: ComputeDriver) -> bool {
    matches!(driver, ComputeDriver::Podman)
}

/// The env the loop pod renders the supervisor image into.
pub const SUPERVISOR_IMAGE_ENV: &str = "OPENSHELL_SUPERVISOR_IMAGE";
/// The env the loop pod renders the sandbox runtime image (the `openshell-sandbox` binary) into.
pub const SANDBOX_RUNTIME_IMAGE_ENV: &str = "OPENSHELL_SANDBOX_RUNTIME_IMAGE";
/// The env the loop pod's downward API carries its own IP in.
pub const POD_IP_ENV: &str = "CRUCIBLE_POD_IP";

/// The trusted images the compute driver pairs with every sandbox. `None` leaves the driver's
/// default, which names upstream's registry at the gateway's own version tag.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DriverImages {
    pub supervisor: Option<String>,
    pub sandbox_runtime: Option<String>,
}

impl DriverImages {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Self {
            supervisor: var(SUPERVISOR_IMAGE_ENV),
            sandbox_runtime: var(SANDBOX_RUNTIME_IMAGE_ENV),
        }
    }
}

/// This pod's IP from the downward API, `None` off-cluster.
fn pod_ip() -> Result<Option<IpAddr>> {
    match std::env::var(POD_IP_ENV) {
        Ok(raw) if !raw.is_empty() => raw
            .parse()
            .map(Some)
            .map_err(|_| GatewayError::PodIpUnparseable { raw }.into()),
        _ => Ok(None),
    }
}

/// The `[openshell.drivers.podman]` config. Only the image overrides; everything else is the
/// driver's default.
#[derive(Debug, Default, Serialize)]
struct PodmanDriverConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    supervisor_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_runtime_image: Option<String>,
}

/// The `[openshell.drivers.kubernetes]` config, emitted from a typed struct so the serde field
/// names track the driver's own `KubernetesComputeConfig`. Every optional field is
/// skip-if-empty: an omitted key means "use the driver's default". `deny_unknown_fields` on the
/// upstream struct rejects any name we mistype, so every field here must match exactly.
#[derive(Debug, Default, Serialize)]
pub struct KubernetesDriverConfig {
    /// Accept the per-sandbox `driver_config` crucible sends (node selector, tolerations,
    /// runtime class, container resources). Resource admission still applies to what it names.
    pub allow_driver_config: bool,
    /// The gateway URL the supervisor pod dials (`OPENSHELL_ENDPOINT`, tonic-parsed). The driver
    /// defaults it to the empty string and passes it verbatim, which the supervisor rejects as
    /// "invalid gRPC endpoint" and crash-loops on, so this must always be set. The supervisor is
    /// its own pod with cluster DNS only, so this is the loop pod's IP, which the gateway's
    /// server certificate carries as an IP SAN.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub grpc_endpoint: String,
    /// The Secret (in the sandbox namespace) carrying the generated client mTLS material. The
    /// driver mounts it into the supervisor pod and points `OPENSHELL_TLS_CA/CERT/KEY` at its
    /// `ca.crt`/`tls.crt`/`tls.key` keys, and an `https://` endpoint without it dies with
    /// "OPENSHELL_TLS_CA is required".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub client_tls_secret_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_account_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_image: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_runtime_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supervisor_image: Option<String>,
    /// The address `host.openshell.internal` resolves to for sandbox egress (broker, collector).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_gateway_ip: Option<IpAddr>,
    /// The `openshell.ai/gateway-id` label the driver stamps on every supervisor pod and Sandbox
    /// it creates, and scopes its own lookups by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_id: Option<String>,
    /// The SPIFFE Workload API socket the driver mounts into supervisor pods.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_spiffe_workload_api_socket_path: Option<String>,
}

impl KubernetesDriverConfig {
    pub fn new(images: &DriverImages) -> Self {
        Self {
            allow_driver_config: true,
            supervisor_image: images.supervisor.clone(),
            sandbox_runtime_image: images.sandbox_runtime.clone(),
            ..Self::default()
        }
    }
}

/// Where sandbox supervisors reach their gateway, and the address `host.openshell.internal`
/// resolves to inside a sandbox (the run's broker, collector, and MCP servers). In the loop pod
/// both are the pod's IP; for a gateway in its own pod they are that gateway's Service and the
/// run's loop Service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialBack {
    pub grpc_endpoint: String,
    pub host_gateway_ip: IpAddr,
}

impl DialBack {
    /// The in-pod gateway's dial-back: everything at this pod's IP.
    pub fn local(pod_ip: IpAddr, port: u16) -> Self {
        Self {
            grpc_endpoint: format!("https://{}", SocketAddr::new(pod_ip, port)),
            host_gateway_ip: pod_ip,
        }
    }
}

/// The loop pod's view of a gateway running in its own pod: the URL, and a directory holding the
/// client mTLS material the controller minted for this run (`ca.crt`, `tls.crt`, `tls.key`).
pub const GATEWAY_URL_ENV: &str = "CRUCIBLE_OPENSHELL_GATEWAY_URL";
pub const GATEWAY_CLIENT_TLS_DIR_ENV: &str = "CRUCIBLE_OPENSHELL_GATEWAY_TLS_DIR";

/// The gateway pod's inputs: the endpoint supervisors dial, the loop Service's address, the
/// Secret the driver mounts into supervisors, and the directory holding the server certificate
/// (`tls.crt`, `tls.key`) and the CA client certificates must chain to (`ca.crt`).
pub const SERVE_GRPC_ENDPOINT_ENV: &str = "CRUCIBLE_GATEWAY_GRPC_ENDPOINT";
pub const SERVE_HOST_IP_ENV: &str = "CRUCIBLE_GATEWAY_HOST_IP";
/// The run's gateway id, which supervisor SPIFFE IDs are derived from.
pub const SERVE_GATEWAY_ID_ENV: &str = "CRUCIBLE_GATEWAY_ID";
/// The SPIFFE Workload API socket path supervisors get (`[cluster] sandbox_spiffe_socket`).
pub const SANDBOX_SPIFFE_SOCKET_ENV: &str = "CRUCIBLE_SANDBOX_SPIFFE_SOCKET";
pub const SERVE_CLIENT_TLS_SECRET_ENV: &str = "CRUCIBLE_GATEWAY_CLIENT_TLS_SECRET";
pub const SERVE_SERVER_TLS_DIR_ENV: &str = "CRUCIBLE_GATEWAY_SERVER_TLS_DIR";

/// How this process reaches its gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayAccess {
    /// Spawned in this pod (podman, or kubernetes without a dedicated sandbox namespace), on the
    /// fixed port, with the client material its own `generate-certs` wrote.
    Local,
    /// A gateway pod of its own at `endpoint`, with the controller-minted client material
    /// (`ca.crt`, `tls.crt`, `tls.key`) in `tls_dir`.
    Remote { endpoint: String, tls_dir: PathBuf },
}

impl GatewayAccess {
    pub fn from_env() -> Result<Self> {
        let endpoint = std::env::var(GATEWAY_URL_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty());
        let Some(endpoint) = endpoint else {
            return Ok(Self::Local);
        };
        let tls_dir = std::env::var(GATEWAY_CLIENT_TLS_DIR_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or(GatewayError::RemoteTlsMissing)?;
        Ok(Self::Remote {
            endpoint,
            tls_dir: PathBuf::from(tls_dir),
        })
    }

    /// The URL and client material the gRPC client dials with.
    pub fn dial(&self) -> Result<crate::openshell::grpc::GatewayDial> {
        Ok(crate::openshell::grpc::GatewayDial {
            endpoint: self.endpoint(),
            tls_dir: self.tls_dir()?,
        })
    }

    /// The URL dialed over mTLS.
    pub fn endpoint(&self) -> String {
        match self {
            Self::Local => format!("https://localhost:{}", crate::openshell::grpc::GATEWAY_PORT),
            Self::Remote { endpoint, .. } => endpoint.clone(),
        }
    }

    /// The directory holding the CA and client certificate the channel presents.
    pub fn tls_dir(&self) -> Result<PathBuf> {
        match self {
            Self::Local => crate::openshell::grpc::mtls_dir(),
            Self::Remote { tls_dir, .. } => Ok(tls_dir.clone()),
        }
    }
}

/// Render `~/.config/openshell/gateway.toml` (schema version 2) for `driver`. The image
/// overrides go to the driver's table. An `otlp_endpoint` appends `[openshell.gateway.otlp]`:
/// the gateway's span export is switched on by that table's presence, not by the `OTEL_*` env
/// vars it leaves to the SDK. Under `Kubernetes` the supervisor dials back to `pod_ip`, so it is
/// required there.
pub fn gateway_toml(
    port: u16,
    driver: ComputeDriver,
    images: &DriverImages,
    otlp_endpoint: Option<&str>,
    dial_back: Option<&DialBack>,
    client_tls_secret: &str,
) -> Result<String> {
    let mut s = format!(
        "[openshell]\nversion = 2\n\n[openshell.gateway]\nbind_address = \"0.0.0.0:{port}\"\ncompute_driver = \"{}\"\n",
        driver.as_str()
    );
    if let Some(endpoint) = otlp_endpoint {
        s.push_str(&format!(
            "\n[openshell.gateway.otlp]\nendpoint = \"{endpoint}\"\n"
        ));
    }
    match driver {
        ComputeDriver::Podman => {
            let cfg = PodmanDriverConfig {
                supervisor_image: images.supervisor.clone(),
                sandbox_runtime_image: images.sandbox_runtime.clone(),
            };
            if cfg.supervisor_image.is_some() || cfg.sandbox_runtime_image.is_some() {
                let body = toml::to_string(&cfg).context("serializing podman driver config")?;
                s.push_str(&format!("\n[openshell.drivers.podman]\n{body}"));
            }
        }
        ComputeDriver::Kubernetes => {
            // The gateway auto-detects the JWT bundle `generate-certs` writes next to the TLS
            // bundle and turns on its authenticator chain, and it hard-rejects mTLS *user* auth
            // under the kubernetes compute driver (the implicit auth the podman path rides via
            // its singleplayer-driver auto-default). Without this block, crucible's own
            // bearer-less RPCs bounce UNAUTHENTICATED at the first authenticated method
            // (CreateProvider). Trust model is unchanged: the socket still requires the
            // generated client cert (require_client_auth), so possession of the local mTLS
            // client cert = authorized, exactly what mtls_auth grants under podman. Sandbox
            // supervisor calls keep using gateway-minted JWTs either way.
            s.push_str("\n[openshell.gateway.auth]\nallow_unauthenticated_users = true\n");
            let dial_back = dial_back.ok_or(GatewayError::NoDialBack)?;
            let mut cfg = KubernetesDriverConfig::new(images);
            cfg.grpc_endpoint = dial_back.grpc_endpoint.clone();
            cfg.client_tls_secret_name = client_tls_secret.to_string();
            cfg.host_gateway_ip = Some(dial_back.host_gateway_ip);
            // At runtime the render-projected env vars fill the driver config fields that
            // are unknowable at render time or vary per profile.
            if let Ok(ns) = std::env::var("CRUCIBLE_SANDBOX_NAMESPACE")
                && !ns.is_empty()
            {
                cfg.namespace = Some(ns);
            }
            if let Ok(sa) = std::env::var("CRUCIBLE_SANDBOX_SERVICE_ACCOUNT")
                && !sa.is_empty()
            {
                cfg.service_account_name = Some(sa);
            }
            if let Ok(img) = std::env::var("CRUCIBLE_SANDBOX_DEFAULT_IMAGE")
                && !img.is_empty()
            {
                cfg.default_image = Some(img);
            }
            if let Ok(id) = std::env::var(SERVE_GATEWAY_ID_ENV)
                && !id.is_empty()
            {
                cfg.gateway_id = Some(id);
            }
            if let Ok(socket) = std::env::var(SANDBOX_SPIFFE_SOCKET_ENV)
                && !socket.is_empty()
            {
                cfg.provider_spiffe_workload_api_socket_path = Some(socket);
            }
            if let Ok(secrets) = std::env::var("CRUCIBLE_SANDBOX_IMAGE_PULL_SECRETS") {
                let v: Vec<String> = secrets
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                if !v.is_empty() {
                    cfg.image_pull_secrets = v;
                }
            }
            let body = toml::to_string(&cfg).context("serializing kubernetes driver config")?;
            s.push_str(&format!("\n[openshell.drivers.kubernetes]\n{body}"));
        }
    }
    Ok(s)
}

/// Every field v0.1.2's `KubernetesComputeConfig` (`crates/openshell-driver-kubernetes/src/
/// config.rs`) accepts. That struct is `deny_unknown_fields`, so a name outside this list kills
/// the gateway on startup, and its log lands in `gateway.log`, not the pod log. Re-derive it
/// from the struct whenever the pin moves.
#[cfg(test)]
pub(crate) const UPSTREAM_KUBERNETES_COMPUTE_CONFIG_FIELDS: &[&str] = &[
    "allow_driver_config",
    "resource_admission",
    "workspace_mode",
    "gateway_id",
    "namespace",
    "operator_namespace_label",
    "operator_namespace_file",
    "service_account_name",
    "default_image",
    "image_pull_policy",
    "image_pull_secrets",
    "managed_ssh_ingress",
    "sandbox_runtime_image",
    "sandbox_runtime_image_pull_policy",
    "supervisor_image",
    "supervisor_image_pull_policy",
    "sandbox_runtime",
    "https_proxy",
    "no_proxy",
    "proxy_auth_secret_name",
    "proxy_auth_secret_key",
    "proxy_auth_allow_insecure",
    "proxy_connect_by_hostname",
    "proxy_ca_bundle",
    "grpc_endpoint",
    "ssh_socket_path",
    "client_tls_secret_name",
    "host_gateway_ip",
    "enable_user_namespaces",
    "workspace_default_storage_size",
    "workspace_storage_class",
    "default_runtime_class_name",
    "sa_token_ttl_secs",
    "provider_spiffe_workload_api_socket_path",
    "sandbox_uid",
    "sandbox_gid",
];

/// `openshell-gateway generate-certs --output-dir <tls> --server-san host.openshell.internal`,
/// plus `--server-san <pod_ip>` in a pod, the address the supervisor dials.
pub fn generate_certs_args(tls_dir: &str, pod_ip: Option<IpAddr>) -> Vec<String> {
    let mut args = vec![
        "generate-certs".into(),
        "--output-dir".into(),
        tls_dir.into(),
        "--server-san".into(),
        "host.openshell.internal".into(),
    ];
    if let Some(ip) = pod_ip {
        args.extend(["--server-san".into(), ip.to_string()]);
    }
    args
}

/// The in-pod gateway's endpoint.
fn local_endpoint(port: u16) -> String {
    format!("https://localhost:{port}")
}

/// `openshell gateway add <endpoint> --local --name ci`.
pub fn register_args(endpoint: &str) -> Vec<String> {
    vec![
        "gateway".into(),
        "add".into(),
        endpoint.into(),
        "--local".into(),
        "--name".into(),
        GATEWAY_NAME.into(),
    ]
}

fn registration_matches(body: &[u8], endpoint: &str) -> bool {
    let Ok(rows) = serde_json::from_slice::<Vec<serde_json::Value>>(body) else {
        return false;
    };
    rows.iter().any(|row| {
        row.get("name").and_then(serde_json::Value::as_str) == Some(GATEWAY_NAME)
            && row.get("endpoint").and_then(serde_json::Value::as_str) == Some(endpoint)
            && row.get("type").and_then(serde_json::Value::as_str) == Some("local")
            && row.get("auth").and_then(serde_json::Value::as_str) == Some("mtls")
    })
}

/// Whether the CLI's registered client material is the copy `gateway add` would take from
/// `tls_dir` now. False when either side is missing.
fn client_certs_match(mtls_dir: &Path, tls_dir: &Path) -> bool {
    [
        ("ca.crt", "ca.crt"),
        ("tls.crt", "client/tls.crt"),
        ("tls.key", "client/tls.key"),
    ]
    .iter()
    .all(|(registered, generated)| {
        match (
            std::fs::read(mtls_dir.join(registered)),
            std::fs::read(tls_dir.join(generated)),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    })
}

/// Whether something already accepts connections on the gateway port. A gateway from an
/// earlier boot in this process (or a previous process on the same host) holds it; a second
/// spawn would die on the bind and truncate the live one's log.
fn gateway_listening(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(1),
    )
    .is_ok()
}

fn registration_exists(endpoint: &str) -> bool {
    Command::new("openshell")
        .args(["gateway", "list", "--output", "json"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| registration_matches(&output.stdout, endpoint))
}

/// Register the CLI's gateway at `endpoint` with the client material in `tls_dir` (`ca.crt`,
/// `client/tls.crt`, `client/tls.key`), retrying while the gateway comes up. `gateway add` copies
/// that material and refuses an existing name, so a registration whose copy no longer matches is
/// removed and made again. Returns the CLI's last complaint when it never registers.
fn register_cli(endpoint: &str, tls_dir: &Path) -> Result<Result<(), String>> {
    let current = registration_exists(endpoint)
        && mtls_dir().is_ok_and(|dir| client_certs_match(&dir, tls_dir));
    if current {
        return Ok(Ok(()));
    }
    let listed = Command::new("openshell")
        .args(["gateway", "list", "--output", "json"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| registered_name(&output.stdout));
    if listed {
        let removed = Command::new("openshell")
            .args(["gateway", "remove", GATEWAY_NAME])
            .output()
            .context("exec openshell gateway remove")?;
        if !removed.status.success() {
            return Err(GatewayError::RemoveRegistrationFailed {
                name: GATEWAY_NAME,
                stderr: String::from_utf8_lossy(&removed.stderr).trim().to_owned(),
            }
            .into());
        }
    }
    let mut last = String::new();
    let registered = wait_for(Duration::from_secs(30), || {
        match Command::new("openshell")
            .args(register_args(endpoint))
            .env("OPENSHELL_LOCAL_TLS_DIR", tls_dir)
            .output()
        {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                last = String::from_utf8_lossy(&o.stderr).trim().to_string();
                false
            }
            Err(e) => {
                last = e.to_string();
                false
            }
        }
    });
    Ok(match registered {
        true => Ok(()),
        false => Err(last),
    })
}

fn registered_name(body: &[u8]) -> bool {
    serde_json::from_slice::<Vec<serde_json::Value>>(body).is_ok_and(|rows| {
        rows.iter()
            .any(|row| row.get("name").and_then(serde_json::Value::as_str) == Some(GATEWAY_NAME))
    })
}

/// Lay a gateway pod's client material (`ca.crt`, `tls.crt`, `tls.key`, as its Secret mounts it)
/// out the way `gateway add` reads it, under `staged`.
fn stage_client_tls(mounted: &Path, staged: &Path) -> Result<()> {
    let client = staged.join("client");
    std::fs::create_dir_all(&client).with_context(|| format!("creating {}", client.display()))?;
    for (from, to) in [
        ("ca.crt", staged.join("ca.crt")),
        ("tls.crt", client.join("tls.crt")),
        ("tls.key", client.join("tls.key")),
    ] {
        std::fs::copy(mounted.join(from), &to).with_context(|| {
            format!(
                "copying {} to {}",
                mounted.join(from).display(),
                to.display()
            )
        })?;
    }
    Ok(())
}

/// The Podman API socket path. An explicit `OPENSHELL_PODMAN_SOCKET` names a socket managed
/// outside Crucible (notably Podman Desktop's host-forwarded API socket); otherwise Crucible
/// owns a rootless service socket beneath `XDG_RUNTIME_DIR`, with the in-pod Linux fallback.
fn podman_socket_from(override_socket: Option<&str>, xdg: Option<&str>, uid: u32) -> PathBuf {
    if let Some(socket) = override_socket.filter(|socket| !socket.is_empty()) {
        return PathBuf::from(socket);
    }
    let runtime = xdg
        .filter(|runtime| !runtime.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("/run/user/{uid}"));
    PathBuf::from(runtime).join("podman/podman.sock")
}

fn external_podman_socket() -> Option<String> {
    std::env::var("OPENSHELL_PODMAN_SOCKET")
        .ok()
        .filter(|socket| !socket.is_empty())
}

/// `getuid()` without pulling a crate: crucible already depends on `nix`, but a tiny extern
/// keeps this module self-contained. Always succeeds.
fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid() is a pure syscall that cannot fail and takes no arguments.
    unsafe { getuid() }
}

/// Ensure a healthy gateway is up, booting one if not (idempotent, the first turn boots,
/// later turns no-op). The daemons it spawns are not handle-held: crucible is the pod
/// entrypoint, so when it exits the container exits and the runtime reaps every process,
/// the gateway/podman die with the pod, no leak. (On a non-pod Linux host the daemons
/// outlive the run; the openshell backend is pod-oriented, so that's an accepted caveat.)
///
/// Once healthy, the gateway's self-reported version is gated against
/// [`crate::openshell::grpc::MIN_GATEWAY_VERSION`]: too old is a hard error (an old gateway
/// answers newer RPCs with UNIMPLEMENTED mid-turn, so fail up front); a rev mismatch or an
/// unparseable version returns `Ok(Some(warning))` for the caller's sink, never a hard fail.
///
/// The driver images come from the render-projected env ([`DriverImages::from_env`]).
/// Fan-out instances run their turns on concurrent threads and each arrives here; a second
/// `generate-certs` under a live gateway rotates the CA its clients were issued from.
static BOOT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tracing::instrument(name = "gateway_boot", skip_all, fields(driver = ?driver))]
pub async fn ensure_running(
    driver: ComputeDriver,
    access: &GatewayAccess,
) -> Result<Option<String>> {
    let _boot = BOOT.lock().await;
    match access {
        GatewayAccess::Local => {
            if !is_running(access).await {
                boot(driver, &DriverImages::from_env()).await?;
            }
        }
        GatewayAccess::Remote { endpoint, .. } => {
            if !is_running(access).await {
                await_remote(access, endpoint).await?;
            }
            register_remote(access, endpoint)?;
        }
    }
    check_gateway_version(access).await
}

/// Wait for a gateway running in its own pod to answer. It may still be starting, and its
/// client material is already mounted, so only health is polled.
async fn await_remote(access: &GatewayAccess, endpoint: &str) -> Result<()> {
    let healthy = match access
        .dial()
        .ok()
        .and_then(|d| crate::openshell::grpc::HealthProbe::new(&d))
    {
        Some(probe) => {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                if probe.healthy().await {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        None => false,
    };
    if !healthy {
        return Err(GatewayError::HealthTimeout {
            last_status: format!("no healthy answer from {endpoint}"),
            log_tail: remote_log_hint(endpoint),
        }
        .into());
    }
    Ok(())
}

fn remote_log_hint(endpoint: &str) -> String {
    format!("(the gateway runs in its own pod at {endpoint}; see that pod's log)")
}

/// Register the CLI with a gateway running in its own pod, whose client material is mounted.
fn register_remote(access: &GatewayAccess, endpoint: &str) -> Result<()> {
    let staged = state_dir()?.join("remote-tls");
    stage_client_tls(&access.tls_dir()?, &staged)?;
    register_cli(endpoint, &staged)?.map_err(|last| GatewayError::RegisterTimeout {
        last,
        log_tail: remote_log_hint(endpoint),
    })?;
    Ok(())
}

/// Run as a gateway pod's entrypoint: write the gateway config from the controller's env,
/// generate the sandbox-JWT signing material locally, and exec the gateway with the
/// controller-minted server certificate. Returns only on failure.
pub fn serve() -> Result<()> {
    let need = |var: &'static str| {
        std::env::var(var)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or(GatewayError::ServeEnvMissing { var })
    };
    let host_raw = need(SERVE_HOST_IP_ENV)?;
    let host_gateway_ip: IpAddr =
        host_raw
            .parse()
            .map_err(|_| GatewayError::ServeEnvUnparseable {
                var: SERVE_HOST_IP_ENV,
                raw: host_raw.clone(),
            })?;
    let dial_back = DialBack {
        grpc_endpoint: need(SERVE_GRPC_ENDPOINT_ENV)?,
        host_gateway_ip,
    };
    let client_tls_secret = need(SERVE_CLIENT_TLS_SECRET_ENV)?;
    let server_tls = PathBuf::from(need(SERVE_SERVER_TLS_DIR_ENV)?);
    need(SERVE_GATEWAY_ID_ENV)?;
    write_config(
        ComputeDriver::Kubernetes,
        &DriverImages::from_env(),
        Some(&dial_back),
        &client_tls_secret,
    )?;
    let local_dir = state_dir()?.join("tls");
    std::fs::create_dir_all(&local_dir)
        .with_context(|| format!("creating tls dir {}", local_dir.display()))?;
    let certs = Command::new("openshell-gateway")
        .args(generate_certs_args(&local_dir.to_string_lossy(), None))
        .env("OPENSHELL_LOCAL_TLS_DIR", &local_dir)
        .output()
        .context("exec openshell-gateway generate-certs")?;
    if !certs.status.success() {
        return Err(GatewayError::GenerateCertsFailed {
            stderr: String::from_utf8_lossy(&certs.stderr).trim().to_owned(),
        }
        .into());
    }
    let mut gw = Command::new("openshell-gateway");
    apply_gateway_aws_env(&mut gw, aws_sandbox_role().as_deref());
    gw.args(serve_args())
        .env("OPENSHELL_LOCAL_TLS_DIR", &local_dir)
        .env("OPENSHELL_TLS_CERT", server_tls.join("tls.crt"))
        .env("OPENSHELL_TLS_KEY", server_tls.join("tls.key"))
        .env("OPENSHELL_TLS_CLIENT_CA", server_tls.join("ca.crt"));
    use std::os::unix::process::CommandExt;
    Err(anyhow::Error::new(gw.exec()).context("exec openshell-gateway"))
}

/// The gateway's own arguments in a gateway pod: in-memory state, like the in-pod gateway.
pub fn serve_args() -> Vec<String> {
    ["--db-url", "sqlite::memory:", "--log-level", "info"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Gate the healthy gateway's reported version. Hard-fail only below the minimum; every
/// degraded probe outcome (no probe, no version, mismatched rev, unparseable string) is a
/// returned warning, so a gateway-side format change can't brick the loop.
async fn check_gateway_version(access: &GatewayAccess) -> Result<Option<String>> {
    use crate::openshell::grpc::{self, VersionGate};
    let Some(probe) = access.dial().ok().and_then(|d| grpc::HealthProbe::new(&d)) else {
        return Ok(Some(
            "gateway version check skipped: mTLS certs unreadable".to_string(),
        ));
    };
    let Some(reported) = probe.report_version().await else {
        return Ok(Some(
            "gateway version check skipped: Health RPC reported no version".to_string(),
        ));
    };
    let (min_major, min_minor, min_patch) = grpc::MIN_GATEWAY_VERSION;
    match grpc::check_gateway_version(&reported) {
        VersionGate::Ok => Ok(None),
        VersionGate::TooOld { reported } => Err(GatewayError::TooOld {
            reported,
            minimum: format!("{min_major}.{min_minor}.{min_patch}"),
        }
        .into()),
        VersionGate::RevMismatch { reported_commit } => Ok(Some(format!(
            "openshell gateway commit g{reported_commit} differs from the rev crucible compiled \
             against ({}) — RPC surface looks compatible, provenance does not",
            grpc::EXPECTED_GATEWAY_REV
        ))),
        VersionGate::Unparseable { reported } => Ok(Some(format!(
            "openshell gateway reported an unrecognized version string '{reported}' — skipping \
             the minimum-version check"
        ))),
    }
}

async fn boot(driver: ComputeDriver, images: &DriverImages) -> Result<()> {
    let pod_ip = pod_ip()?;
    // 1. rootless podman API socket (the podman compute driver). Skipped under kubernetes,
    // where the sandbox is a sibling pod and there is no local daemon to boot.
    if needs_podman_socket(driver) {
        let external = external_podman_socket();
        let sock = podman_socket_from(
            external.as_deref(),
            std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
            libc_getuid(),
        );
        if external.is_none() {
            if let Some(parent) = sock.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating podman socket dir {}", parent.display()))?;
            }

            // `--time=0` => never self-exits. Spawned detached (not waited): it must outlive this
            // call and live for the run.
            Command::new("podman")
                .args(["system", "service", "--time=0"])
                .arg(format!("unix://{}", sock.display()))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("spawn `podman system service`")?;
        }
        if !wait_for(Duration::from_secs(15), || sock.exists()) {
            return Err(GatewayError::PodmanSocketMissing {
                origin: if external.is_some() {
                    "configured"
                } else {
                    "managed"
                },
                socket: sock.clone(),
            }
            .into());
        }
    }

    // 2. gateway config + 3. TLS certs.
    let tls_secret = ClientTlsSecret::for_boot(&boot_id());
    let dial_back = match driver {
        ComputeDriver::Kubernetes => Some(DialBack::local(
            pod_ip.ok_or(GatewayError::PodIpMissing)?,
            GATEWAY_PORT,
        )),
        ComputeDriver::Podman => None,
    };
    write_config(driver, images, dial_back.as_ref(), tls_secret.name())?;
    let tls_dir = state_dir()?.join("tls");
    std::fs::create_dir_all(&tls_dir)
        .with_context(|| format!("creating tls dir {}", tls_dir.display()))?;
    let certs = Command::new("openshell-gateway")
        .args(generate_certs_args(&tls_dir.to_string_lossy(), pod_ip))
        .env("OPENSHELL_LOCAL_TLS_DIR", &tls_dir)
        .output()
        .context("exec openshell-gateway generate-certs")?;
    if !certs.status.success() {
        return Err(GatewayError::GenerateCertsFailed {
            stderr: String::from_utf8_lossy(&certs.stderr).trim().to_owned(),
        }
        .into());
    }

    // 3b. Under kubernetes, ship the freshly generated client TLS material to the sandbox
    //     namespace: the driver mounts this Secret into every sandbox pod, and it is the only
    //     way the sandbox supervisor gets the CA/cert/key it needs to dial the https
    //     `grpc_endpoint` back to this gateway.
    if driver == ComputeDriver::Kubernetes {
        publish_client_tls_secret(&tls_dir, &tls_secret)?;
    }

    // 4. launch the gateway, scrubbing the in-cluster k8s detection vars only under podman
    //    (the kubernetes driver needs them, see `scrub_k8s_vars`). It is spawned detached, so
    //    its stdout/stderr never land in this process's own log; redirect them to a file instead
    //    of discarding them, so a failure below (register/health timeout) can quote the
    //    gateway's own diagnostics rather than a bare "did not become healthy".
    let log_path = state_dir()?.join("gateway.log");
    if !gateway_listening(GATEWAY_PORT) {
        let log_out = std::fs::File::create(&log_path)
            .with_context(|| format!("creating gateway log {}", log_path.display()))?;
        let log_err = log_out
            .try_clone()
            .with_context(|| format!("cloning gateway log handle {}", log_path.display()))?;
        let mut gw = Command::new("openshell-gateway");
        gw.args(["--db-url", "sqlite::memory:", "--log-level", "info"])
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err));
        if scrub_k8s_vars(driver) {
            for var in K8S_DETECTION_VARS {
                gw.env_remove(var);
            }
        }
        apply_gateway_aws_env(&mut gw, aws_sandbox_role().as_deref());
        gw.spawn().context("spawn openshell-gateway")?;
    }

    // 5. register, retrying until the gateway is listening; 6. wait healthy. `gateway add`
    //    copies the client material out of `tls_dir` and refuses an existing name, so a
    //    registration whose copy no longer matches is removed and made again.
    if let Err(last) = register_cli(&local_endpoint(GATEWAY_PORT), &tls_dir)? {
        return Err(GatewayError::RegisterTimeout {
            last,
            log_tail: tail_gateway_log(&log_path),
        }
        .into());
    }
    // The certs now exist (register wrote them), so a single reusable probe covers the poll loop.
    // Its `healthy()` is async, so the health wait is an async loop (not the sync `wait_for` the
    // socket/register waits use).
    let healthy = match GatewayAccess::Local
        .dial()
        .ok()
        .and_then(|d| crate::openshell::grpc::HealthProbe::new(&d))
    {
        Some(probe) => {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                if probe.healthy().await {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        None => false,
    };
    if !healthy {
        // Diagnostics only: quote the CLI's view of the gateway plus the gateway's own log
        // tail, so the timeout is never a bare one-liner.
        let (_, last_status) = status_check();
        return Err(GatewayError::HealthTimeout {
            last_status,
            log_tail: tail_gateway_log(&log_path),
        }
        .into());
    }
    Ok(())
}

/// Whether the gateway is up and answering: a `Health` RPC over the local mTLS channel
/// succeeds with a non-unhealthy status. Before the certs exist (pre-boot) there is nothing to
/// probe, which reads as "not running".
pub async fn is_running(access: &GatewayAccess) -> bool {
    match access
        .dial()
        .ok()
        .and_then(|d| crate::openshell::grpc::HealthProbe::new(&d))
    {
        Some(p) => p.healthy().await,
        None => false,
    }
}

/// Run `openshell status` once, returning whether it reports a healthy gateway alongside the
/// raw output (stdout+stderr). Diagnostics only: the health decision is the gRPC `Health`
/// probe ([`is_running`]); this exists so a boot timeout can quote what the CLI saw instead
/// of nothing.
fn status_check() -> (bool, String) {
    match Command::new("openshell").arg("status").output() {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let healthy = o.status.success() && !stdout.contains("No gateway configured");
            let text = if o.stderr.is_empty() {
                stdout.trim().to_string()
            } else {
                format!(
                    "{}\n{}",
                    stdout.trim(),
                    String::from_utf8_lossy(&o.stderr).trim()
                )
            };
            (healthy, text)
        }
        Err(e) => (false, e.to_string()),
    }
}

/// The last 4KiB of the gateway's log file, for embedding in a bail message. Reading failures
/// (e.g. the gateway never got far enough to write anything) become a note, not a second error.
fn tail_gateway_log(path: &std::path::Path) -> String {
    const TAIL_BYTES: u64 = 4096;
    match std::fs::metadata(path).and_then(|m| {
        let len = m.len();
        let start = len.saturating_sub(TAIL_BYTES);
        std::fs::read(path).map(|bytes| (start, bytes))
    }) {
        Ok((start, bytes)) => {
            let tail = String::from_utf8_lossy(&bytes[start as usize..]);
            format!("gateway log ({}):\n{}", path.display(), tail.trim())
        }
        Err(e) => format!("gateway log ({}) unreadable: {e}", path.display()),
    }
}

/// Publish this boot's client mTLS Secret from the certgen output at `tls_dir` (`ca.crt`,
/// `client/tls.crt`, `client/tls.key` → the `ca.crt`/`tls.crt`/`tls.key` keys the driver's mount
/// points `OPENSHELL_TLS_CA/CERT/KEY` at). The namespace mirrors the driver config:
/// `CRUCIBLE_SANDBOX_NAMESPACE`, falling back to the driver's own default.
///
/// In a pod the Secret is owned by that pod, so it is garbage-collected with the turn rather than
/// accumulating one per dispatch.
fn publish_client_tls_secret(tls_dir: &std::path::Path, target: &ClientTlsSecret) -> Result<()> {
    let read = |rel: &str| -> Result<String> {
        let p = tls_dir.join(rel);
        std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))
    };
    let name = target.name();
    let ns = std::env::var("CRUCIBLE_SANDBOX_NAMESPACE")
        .ok()
        .filter(|s| !s.is_empty())
        // The kubernetes driver's DEFAULT_K8S_NAMESPACE, used when the config omits `namespace`.
        .unwrap_or_else(|| "openshell".to_string());
    let secret = client_tls_secret(
        name,
        &ns,
        target.owner(),
        [
            ("ca.crt".to_string(), read("ca.crt")?),
            ("tls.crt".to_string(), read("client/tls.crt")?),
            ("tls.key".to_string(), read("client/tls.key")?),
        ],
    );
    match target.verb() {
        PublishVerb::Create => forge::kube::create_secret(&secret),
        PublishVerb::Apply => {
            let yaml =
                serde_norway::to_string(&secret).context("serializing the client TLS secret")?;
            forge::kube::apply_yaml(&yaml)
        }
    }
    .with_context(|| format!("publishing Secret {name} to namespace {ns}"))
}

/// The Secret object `publish_client_tls_secret` writes. `owner` present sets this gateway's pod
/// as the owner so collection cascades; absent leaves the Secret standing (off-cluster, where
/// nothing would collect it anyway).
fn client_tls_secret(
    name: &str,
    ns: &str,
    owner: Option<(String, String)>,
    material: [(String, String); 3],
) -> k8s_openapi::api::core::v1::Secret {
    k8s_openapi::api::core::v1::Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            owner_references: owner.map(|(pod, uid)| {
                vec![
                    k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                        api_version: "v1".to_string(),
                        kind: "Pod".to_string(),
                        name: pod,
                        uid,
                        controller: Some(false),
                        // Collection already cascades from the ownerReference; setting this would
                        // demand `update` on pods/finalizers, which the turn pod's SA lacks.
                        block_owner_deletion: None,
                    },
                ]
            }),
            ..Default::default()
        },
        string_data: Some(std::collections::BTreeMap::from(material)),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}

/// `~/.local/state/openshell`, the gateway's state/cert home.
fn state_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME unset")?;
    Ok(PathBuf::from(home).join(".local/state/openshell"))
}

/// Write `~/.config/openshell/gateway.toml` if its content changed (avoids churning a config
/// a running gateway may have read).
fn write_config(
    driver: ComputeDriver,
    images: &DriverImages,
    dial_back: Option<&DialBack>,
    tls_secret_name: &str,
) -> Result<()> {
    let home = std::env::var("HOME").context("HOME unset")?;
    let dir = PathBuf::from(home).join(".config/openshell");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("gateway.toml");
    let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT"))
        .ok()
        .filter(|s| !s.is_empty());
    let rendered = gateway_toml(
        GATEWAY_PORT,
        driver,
        images,
        otlp_endpoint.as_deref(),
        dial_back,
        tls_secret_name,
    )?;
    if std::fs::read_to_string(&path).ok().as_deref() == Some(rendered.as_str()) {
        return Ok(());
    }
    std::fs::write(&path, rendered).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Poll `cond` every 250ms until true or `timeout` elapses. Returns whether it became true.
fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use crate::openshell::gateway::*;

    // Frozen snapshots of the schema version 2 podman rendering.
    const PODMAN_NO_IMAGE: &str = "[openshell]\nversion = 2\n\n[openshell.gateway]\nbind_address = \"0.0.0.0:17670\"\ncompute_driver = \"podman\"\n";
    const PODMAN_WITH_IMAGE: &str = "[openshell]\nversion = 2\n\n[openshell.gateway]\nbind_address = \"0.0.0.0:17670\"\ncompute_driver = \"podman\"\n\n[openshell.drivers.podman]\nsupervisor_image = \"registry.example.com/epp-sandbox:x\"\n";

    fn images(supervisor: Option<&str>, sandbox_runtime: Option<&str>) -> DriverImages {
        DriverImages {
            supervisor: supervisor.map(str::to_owned),
            sandbox_runtime: sandbox_runtime.map(str::to_owned),
        }
    }

    fn pod_ip() -> Option<IpAddr> {
        Some(IpAddr::from([10, 1, 2, 3]))
    }

    fn dial_back() -> Option<DialBack> {
        pod_ip().map(|ip| DialBack::local(ip, 17670))
    }

    fn kubernetes_toml(images: &DriverImages) -> toml::Value {
        let t = gateway_toml(
            17670,
            ComputeDriver::Kubernetes,
            images,
            None,
            dial_back().as_ref(),
            CLIENT_TLS_SECRET,
        )
        .unwrap();
        toml::from_str(&t).expect("kubernetes gateway.toml must parse")
    }

    #[test]
    fn a_sandbox_role_becomes_the_gateway_identity() {
        assert_eq!(
            gateway_aws_env(Some("arn:aws:iam::1:role/sandbox-ro")),
            vec![
                EnvChange::Set("AWS_ROLE_ARN", "arn:aws:iam::1:role/sandbox-ro".into()),
                EnvChange::Set(
                    "AWS_WEB_IDENTITY_TOKEN_FILE",
                    "/var/run/secrets/aws/token".into()
                ),
                EnvChange::Set("AWS_EC2_METADATA_DISABLED", "true".into()),
            ]
        );
    }

    #[test]
    fn without_a_sandbox_role_the_gateway_gets_no_aws_identity() {
        assert_eq!(
            gateway_aws_env(None),
            vec![
                EnvChange::Remove("AWS_ROLE_ARN"),
                EnvChange::Remove("AWS_WEB_IDENTITY_TOKEN_FILE"),
                EnvChange::Set("AWS_EC2_METADATA_DISABLED", "true".into()),
            ]
        );
    }

    /// The child is a real process: what it prints is what a spawned gateway would inherit.
    fn child_aws_env(sandbox_role: Option<&str>) -> std::collections::BTreeMap<String, String> {
        let mut cmd = Command::new("env");
        cmd.env("AWS_ROLE_ARN", "arn:aws:iam::1:role/publish")
            .env("AWS_WEB_IDENTITY_TOKEN_FILE", "/var/run/secrets/aws/token");
        apply_gateway_aws_env(&mut cmd, sandbox_role);
        let out = cmd.output().expect("run env");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(k, _)| k.starts_with("AWS_"))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_gateway_child_never_inherits_the_publish_role() {
        let with_role = child_aws_env(Some("arn:aws:iam::1:role/sandbox-ro"));
        assert_eq!(
            with_role.get("AWS_ROLE_ARN").map(String::as_str),
            Some("arn:aws:iam::1:role/sandbox-ro")
        );
        assert_eq!(
            with_role
                .get("AWS_EC2_METADATA_DISABLED")
                .map(String::as_str),
            Some("true")
        );
        let without = child_aws_env(None);
        assert!(!without.contains_key("AWS_ROLE_ARN"), "{without:?}");
        assert!(
            !without.contains_key("AWS_WEB_IDENTITY_TOKEN_FILE"),
            "{without:?}"
        );
    }

    fn pod_secret(pod: &str, boot: &str) -> ClientTlsSecret {
        ClientTlsSecret::for_pod(pod.to_string(), format!("uid-{pod}"), boot)
    }

    #[test]
    fn each_pod_boot_publishes_its_client_material_under_its_own_name() {
        let a = pod_secret("crucible-turn-router-2316-abc", "000000000001");
        let b = pod_secret("crucible-turn-router-2399-def", "000000000001");
        let restarted = pod_secret("crucible-turn-router-2316-abc", "000000000002");
        assert_ne!(
            a.name(),
            b.name(),
            "concurrent turns must not share one secret"
        );
        assert_ne!(
            a.name(),
            restarted.name(),
            "a second boot in one pod must not collide with the first"
        );
        assert_eq!(
            a.name(),
            "crucible-openshell-client-tls-crucible-turn-router-2316-abc-000000000001"
        );
        assert_eq!(ClientTlsSecret::Shared.name(), CLIENT_TLS_SECRET);
    }

    #[test]
    fn a_pod_boot_creates_its_secret_and_only_off_cluster_applies() {
        let in_pod = pod_secret("crucible-turn-x-1", "000000000001");
        assert_eq!(in_pod.verb(), PublishVerb::Create);
        assert_eq!(
            in_pod.owner(),
            Some((
                "crucible-turn-x-1".to_string(),
                "uid-crucible-turn-x-1".to_string()
            ))
        );
        assert_eq!(ClientTlsSecret::Shared.verb(), PublishVerb::Apply);
        assert_eq!(ClientTlsSecret::Shared.owner(), None);
    }

    #[test]
    fn boot_ids_are_twelve_hex_digits_and_differ_between_boots() {
        let a = boot_id();
        let b = boot_id();
        for id in [&a, &b] {
            assert_eq!(id.len(), 12, "{id}");
            assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
        }
        assert_ne!(a, b);
    }

    #[test]
    fn kubernetes_rendering_names_the_secret_this_boot_publishes() {
        let secret = pod_secret("crucible-run-r1", "00000000abcd");
        let t = gateway_toml(
            17670,
            ComputeDriver::Kubernetes,
            &DriverImages::default(),
            None,
            dial_back().as_ref(),
            secret.name(),
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&t).unwrap();
        assert_eq!(
            parsed["openshell"]["drivers"]["kubernetes"]["client_tls_secret_name"].as_str(),
            Some(secret.name())
        );
    }

    #[test]
    fn a_published_secret_is_owned_by_the_pod_that_published_it() {
        let material = || {
            [
                ("ca.crt".to_string(), "ca".to_string()),
                ("tls.crt".to_string(), "crt".to_string()),
                ("tls.key".to_string(), "key".to_string()),
            ]
        };
        let owned = client_tls_secret(
            "s",
            "ns",
            Some(("turn-pod".to_string(), "uid-1".to_string())),
            material(),
        );
        let refs = owned.metadata.owner_references.expect("owner set in a pod");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "Pod");
        assert_eq!(refs[0].name, "turn-pod");
        assert_eq!(refs[0].uid, "uid-1");
        assert_eq!(refs[0].controller, Some(false));
        // Setting it would require `update` on pods/finalizers, which the turn pod's SA lacks.
        assert_eq!(refs[0].block_owner_deletion, None);

        let unowned = client_tls_secret("s", "ns", None, material());
        assert!(unowned.metadata.owner_references.is_none());
    }

    #[test]
    fn podman_rendering_is_byte_identical_without_image() {
        let t = gateway_toml(
            17670,
            ComputeDriver::Podman,
            &DriverImages::default(),
            None,
            None,
            CLIENT_TLS_SECRET,
        )
        .unwrap();
        assert_eq!(t, PODMAN_NO_IMAGE);
    }

    #[test]
    fn podman_rendering_is_byte_identical_with_image() {
        let t = gateway_toml(
            17670,
            ComputeDriver::Podman,
            &images(Some("registry.example.com/epp-sandbox:x"), None),
            None,
            None,
            CLIENT_TLS_SECRET,
        )
        .unwrap();
        assert_eq!(t, PODMAN_WITH_IMAGE);
    }

    #[test]
    fn podman_rendering_carries_the_sandbox_runtime_image() {
        let t = gateway_toml(
            17670,
            ComputeDriver::Podman,
            &images(None, Some("ghcr.io/neuralmagic/openshell-sandbox:sha-abc")),
            None,
            None,
            CLIENT_TLS_SECRET,
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&t).unwrap();
        let podman = &parsed["openshell"]["drivers"]["podman"];
        assert_eq!(
            podman["sandbox_runtime_image"].as_str(),
            Some("ghcr.io/neuralmagic/openshell-sandbox:sha-abc")
        );
        assert!(podman.get("supervisor_image").is_none());
    }

    #[test]
    fn otlp_endpoint_renders_gateway_otlp_table() {
        let t = gateway_toml(
            17670,
            ComputeDriver::Podman,
            &DriverImages::default(),
            Some("http://localhost:4317"),
            None,
            CLIENT_TLS_SECRET,
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&t).expect("gateway.toml must parse");
        assert_eq!(
            parsed["openshell"]["gateway"]["otlp"]["endpoint"].as_str(),
            Some("http://localhost:4317")
        );
    }

    #[test]
    fn kubernetes_rendering_parses_and_carries_expected_keys() {
        let parsed = kubernetes_toml(&images(
            Some("registry.example.com/supervisor:x"),
            Some("registry.example.com/sandbox:x"),
        ));
        assert_eq!(parsed["openshell"]["version"].as_integer(), Some(2));
        assert_eq!(
            parsed["openshell"]["gateway"]["compute_driver"].as_str(),
            Some("kubernetes")
        );
        assert!(
            parsed["openshell"]["gateway"]
                .get("compute_drivers")
                .is_none()
        );
        let k8s = &parsed["openshell"]["drivers"]["kubernetes"];
        assert_eq!(
            k8s["supervisor_image"].as_str(),
            Some("registry.example.com/supervisor:x")
        );
        assert_eq!(
            k8s["sandbox_runtime_image"].as_str(),
            Some("registry.example.com/sandbox:x")
        );
        assert_eq!(k8s["allow_driver_config"].as_bool(), Some(true));
        assert_eq!(k8s["host_gateway_ip"].as_str(), Some("10.1.2.3"));
        assert!(k8s.get("supervisor_sideload_method").is_none());
        assert!(k8s.get("app_armor_profile").is_none());
        // Skip-if-empty fields must be absent, not emitted as empty strings, so the driver's
        // own defaults win.
        assert!(k8s.get("namespace").is_none());
        assert!(k8s.get("image_pull_secrets").is_none());
    }

    /// Under kubernetes the gateway's authenticator chain is always on (certgen's JWT bundle)
    /// and mTLS user auth is rejected, so the rendered config must opt in to unauthenticated
    /// local users or every authenticated RPC from crucible itself dies UNAUTHENTICATED at
    /// CreateProvider. Podman must stay untouched: it authenticates via the mTLS
    /// singleplayer-driver auto-default, and the escape hatch would only widen it.
    #[test]
    fn kubernetes_rendering_allows_unauthenticated_local_users_podman_does_not() {
        let parsed = kubernetes_toml(&DriverImages::default());
        assert_eq!(
            parsed["openshell"]["gateway"]["auth"]["allow_unauthenticated_users"].as_bool(),
            Some(true),
            "{parsed}"
        );

        let podman = gateway_toml(
            17670,
            ComputeDriver::Podman,
            &DriverImages::default(),
            None,
            None,
            CLIENT_TLS_SECRET,
        )
        .unwrap();
        assert!(!podman.contains("allow_unauthenticated_users"), "{podman}");
    }

    /// The driver defaults `grpc_endpoint` to "" and passes it verbatim into the supervisor's
    /// `OPENSHELL_ENDPOINT`, which tonic rejects ("invalid gRPC endpoint") and the supervisor
    /// crash-loops on, so the k8s rendering must always pin it. The supervisor pod resolves
    /// nothing outside cluster DNS, so the endpoint is the pod IP, the same address certgen is
    /// told to put in the server certificate; the secret name must be exactly what `boot()`
    /// publishes.
    #[test]
    fn kubernetes_rendering_pins_the_supervisor_dial_back_to_the_pod_ip() {
        let parsed = kubernetes_toml(&DriverImages::default());
        let k8s = &parsed["openshell"]["drivers"]["kubernetes"];
        assert_eq!(
            k8s["grpc_endpoint"].as_str(),
            Some("https://10.1.2.3:17670")
        );
        assert_eq!(
            k8s["client_tls_secret_name"].as_str(),
            Some(CLIENT_TLS_SECRET)
        );
        assert!(
            generate_certs_args("/tls", pod_ip())
                .windows(2)
                .any(|w| w == ["--server-san", "10.1.2.3"]),
            "the endpoint's IP must be a server-cert SAN"
        );

        let podman = gateway_toml(
            17670,
            ComputeDriver::Podman,
            &DriverImages::default(),
            None,
            None,
            CLIENT_TLS_SECRET,
        )
        .unwrap();
        assert!(!podman.contains("grpc_endpoint"), "{podman}");
    }

    #[test]
    fn an_ipv6_pod_ip_is_bracketed_in_the_endpoint() {
        let ip: IpAddr = "fd00::1".parse().unwrap();
        let t = gateway_toml(
            17670,
            ComputeDriver::Kubernetes,
            &DriverImages::default(),
            None,
            Some(&DialBack::local(ip, 17670)),
            CLIENT_TLS_SECRET,
        )
        .unwrap();
        assert!(
            t.contains("grpc_endpoint = \"https://[fd00::1]:17670\""),
            "{t}"
        );
    }

    #[test]
    fn kubernetes_rendering_refuses_without_a_pod_ip() {
        let err = gateway_toml(
            17670,
            ComputeDriver::Kubernetes,
            &DriverImages::default(),
            None,
            None,
            CLIENT_TLS_SECRET,
        )
        .unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<GatewayError>(),
                Some(GatewayError::NoDialBack)
            ),
            "{err:#}"
        );
    }

    #[test]
    fn kubernetes_rendering_omits_images_when_absent() {
        let parsed = kubernetes_toml(&DriverImages::default());
        let k8s = &parsed["openshell"]["drivers"]["kubernetes"];
        assert!(k8s.get("supervisor_image").is_none());
        assert!(k8s.get("sandbox_runtime_image").is_none());
    }

    #[test]
    fn kubernetes_rendering_only_emits_fields_upstream_accepts() {
        let parsed = kubernetes_toml(&images(Some("s"), Some("r")));
        let k8s = parsed["openshell"]["drivers"]["kubernetes"]
            .as_table()
            .expect("[openshell.drivers.kubernetes] must be a table");
        for key in k8s.keys() {
            assert!(
                UPSTREAM_KUBERNETES_COMPUTE_CONFIG_FIELDS.contains(&key.as_str()),
                "emitting {key:?}, which `deny_unknown_fields` upstream does not know — \
                 the gateway will die on startup with no visible error"
            );
        }
    }

    /// The flag is the only thing that constructs `Kubernetes` outside tests. Without it the
    /// variant is dead in the bin target and CI's `-D warnings` clippy rejects the build.
    #[test]
    fn compute_driver_parses_the_closed_vocabulary() {
        use clap::ValueEnum;
        assert_eq!(
            ComputeDriver::from_str("podman", true).unwrap(),
            ComputeDriver::Podman
        );
        assert_eq!(
            ComputeDriver::from_str("kubernetes", true).unwrap(),
            ComputeDriver::Kubernetes
        );
        assert!(ComputeDriver::from_str("docker", true).is_err());
    }

    #[test]
    fn compute_driver_defaults_to_podman() {
        assert_eq!(ComputeDriver::default(), ComputeDriver::Podman);
    }

    #[test]
    fn scrub_applies_under_podman_not_kubernetes() {
        assert!(scrub_k8s_vars(ComputeDriver::Podman));
        assert!(!scrub_k8s_vars(ComputeDriver::Kubernetes));
    }

    #[test]
    fn podman_socket_boots_only_under_podman() {
        // Kubernetes ⇒ boot() skips the `podman system service` spawn and its socket wait.
        assert!(needs_podman_socket(ComputeDriver::Podman));
        assert!(!needs_podman_socket(ComputeDriver::Kubernetes));
    }

    #[test]
    fn explicit_podman_socket_wins_over_linux_runtime_defaults() {
        assert_eq!(
            podman_socket_from(
                Some("/tmp/podman-desktop-api.sock"),
                Some("/run/user/501"),
                501
            ),
            PathBuf::from("/tmp/podman-desktop-api.sock")
        );
        assert_eq!(
            podman_socket_from(None, Some("/runtime"), 501),
            PathBuf::from("/runtime/podman/podman.sock")
        );
        assert_eq!(
            podman_socket_from(None, None, 501),
            PathBuf::from("/run/user/501/podman/podman.sock")
        );
    }

    #[test]
    fn register_targets_local_named_gateway() {
        assert_eq!(
            register_args(&local_endpoint(17670)),
            [
                "gateway",
                "add",
                "https://localhost:17670",
                "--local",
                "--name",
                "ci"
            ]
        );
    }

    #[test]
    fn a_gateway_pods_mounted_client_material_is_staged_where_gateway_add_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let mounted = dir.path().join("mounted");
        std::fs::create_dir_all(&mounted).unwrap();
        for (name, body) in [("ca.crt", "ca"), ("tls.crt", "cert"), ("tls.key", "key")] {
            std::fs::write(mounted.join(name), body).unwrap();
        }
        let staged = dir.path().join("staged");
        stage_client_tls(&mounted, &staged).unwrap();
        assert_eq!(
            std::fs::read_to_string(staged.join("ca.crt")).unwrap(),
            "ca"
        );
        assert_eq!(
            std::fs::read_to_string(staged.join("client/tls.crt")).unwrap(),
            "cert"
        );
        assert_eq!(
            std::fs::read_to_string(staged.join("client/tls.key")).unwrap(),
            "key"
        );
        assert!(
            client_certs_match(&mounted, &staged),
            "a registration copied from the staged material is current"
        );
        std::fs::remove_file(mounted.join("tls.key")).unwrap();
        assert!(stage_client_tls(&mounted, &staged).is_err());
    }

    #[test]
    fn exact_existing_registration_is_idempotent_but_wrong_endpoint_is_not() {
        let exact =
            br#"[{"name":"ci","endpoint":"https://localhost:17670","type":"local","auth":"mtls"}]"#;
        assert!(registration_matches(exact, &local_endpoint(17670)));
        let wrong = br#"[{"name":"ci","endpoint":"https://remote.example:17670","type":"local","auth":"mtls"}]"#;
        assert!(!registration_matches(wrong, &local_endpoint(17670)));
        assert!(registration_matches(wrong, "https://remote.example:17670"));
        assert!(!registration_matches(b"not json", &local_endpoint(17670)));
        assert!(registered_name(wrong));
        assert!(!registered_name(br#"[{"name":"other"}]"#));
    }

    #[test]
    fn certs_request_the_internal_san() {
        let v = generate_certs_args("/tls", None);
        assert_eq!(
            v,
            [
                "generate-certs",
                "--output-dir",
                "/tls",
                "--server-san",
                "host.openshell.internal"
            ]
        );
    }

    #[test]
    fn certs_in_a_pod_also_carry_its_ip() {
        let v = generate_certs_args("/tls", pod_ip());
        assert_eq!(
            v,
            [
                "generate-certs",
                "--output-dir",
                "/tls",
                "--server-san",
                "host.openshell.internal",
                "--server-san",
                "10.1.2.3"
            ]
        );
    }

    #[test]
    fn scrub_list_is_the_in_cluster_signal() {
        assert!(K8S_DETECTION_VARS.contains(&"KUBERNETES_SERVICE_HOST"));
        assert!(K8S_DETECTION_VARS.contains(&"KUBERNETES_SERVICE_PORT"));
    }

    #[test]
    fn broker_host_returns_podman_bridge_for_podman() {
        assert_eq!(
            ComputeDriver::Podman.broker_host(),
            "host.containers.internal"
        );
    }

    #[test]
    fn broker_host_returns_openshell_alias_for_kubernetes() {
        assert_eq!(
            ComputeDriver::Kubernetes.broker_host(),
            "host.openshell.internal"
        );
    }

    #[test]
    fn a_bound_port_reads_as_a_listening_gateway_and_a_closed_one_does_not() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(gateway_listening(port));
        drop(listener);
        assert!(!gateway_listening(port));
    }

    #[test]
    fn a_registration_is_current_only_when_its_copy_matches_the_generated_client_material() {
        let root = tempfile::tempdir().unwrap();
        let mtls = root.path().join("mtls");
        let tls = root.path().join("tls");
        std::fs::create_dir_all(&mtls).unwrap();
        std::fs::create_dir_all(tls.join("client")).unwrap();
        assert!(!client_certs_match(&mtls, &tls), "nothing on either side");
        for (registered, generated, bytes) in [
            ("ca.crt", "ca.crt", "ca-1"),
            ("tls.crt", "client/tls.crt", "client-1"),
            ("tls.key", "client/tls.key", "key-1"),
        ] {
            std::fs::write(mtls.join(registered), bytes).unwrap();
            std::fs::write(tls.join(generated), bytes).unwrap();
        }
        assert!(client_certs_match(&mtls, &tls));
        std::fs::write(tls.join("ca.crt"), "ca-2").unwrap();
        assert!(
            !client_certs_match(&mtls, &tls),
            "a regenerated CA invalidates the registered copy"
        );
        std::fs::remove_file(mtls.join("tls.key")).unwrap();
        std::fs::write(tls.join("ca.crt"), "ca-1").unwrap();
        assert!(
            !client_certs_match(&mtls, &tls),
            "a missing registered file"
        );
    }

    fn clear_remote_env() {
        for var in [
            GATEWAY_URL_ENV,
            GATEWAY_CLIENT_TLS_DIR_ENV,
            SERVE_GRPC_ENDPOINT_ENV,
            SERVE_HOST_IP_ENV,
            SERVE_CLIENT_TLS_SECRET_ENV,
            SERVE_SERVER_TLS_DIR_ENV,
            SERVE_GATEWAY_ID_ENV,
        ] {
            unsafe {
                std::env::remove_var(var);
            }
        }
    }

    #[test]
    fn a_gateway_url_with_client_material_makes_the_gateway_remote() {
        let _guard = crate::test_support::env_lock();
        clear_remote_env();
        assert_eq!(GatewayAccess::from_env().unwrap(), GatewayAccess::Local);
        assert_eq!(GatewayAccess::Local.endpoint(), "https://localhost:17670");
        unsafe {
            std::env::set_var(GATEWAY_URL_ENV, "https://172.30.0.9:17670");
        }
        let err = GatewayAccess::from_env().unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<GatewayError>(),
                Some(GatewayError::RemoteTlsMissing)
            ),
            "{err:#}"
        );
        unsafe {
            std::env::set_var(GATEWAY_CLIENT_TLS_DIR_ENV, "/etc/crucible/gateway-tls");
        }
        let access = GatewayAccess::from_env().unwrap();
        assert_eq!(
            access,
            GatewayAccess::Remote {
                endpoint: "https://172.30.0.9:17670".into(),
                tls_dir: PathBuf::from("/etc/crucible/gateway-tls"),
            }
        );
        assert_eq!(access.endpoint(), "https://172.30.0.9:17670");
        assert_eq!(
            access.tls_dir().unwrap(),
            PathBuf::from("/etc/crucible/gateway-tls")
        );
        clear_remote_env();
    }

    #[test]
    fn a_gateway_pod_refuses_to_start_without_its_controller_env() {
        let _guard = crate::test_support::env_lock();
        clear_remote_env();
        let err = serve().unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<GatewayError>(),
                Some(GatewayError::ServeEnvMissing { var }) if *var == SERVE_HOST_IP_ENV
            ),
            "{err:#}"
        );
        unsafe {
            std::env::set_var(SERVE_HOST_IP_ENV, "not-an-ip");
        }
        let err = serve().unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<GatewayError>(),
                Some(GatewayError::ServeEnvUnparseable { .. })
            ),
            "{err:#}"
        );
        unsafe {
            std::env::set_var(SERVE_HOST_IP_ENV, "172.30.0.10");
        }
        let err = serve().unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<GatewayError>(),
                Some(GatewayError::ServeEnvMissing { var }) if *var == SERVE_GRPC_ENDPOINT_ENV
            ),
            "{err:#}"
        );
        unsafe {
            std::env::set_var(SERVE_GRPC_ENDPOINT_ENV, "https://172.30.0.9:17670");
            std::env::set_var(SERVE_CLIENT_TLS_SECRET_ENV, "crucible-run-a-gw-client");
            std::env::set_var(SERVE_SERVER_TLS_DIR_ENV, "/etc/crucible/gateway-server-tls");
        }
        let err = serve().unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<GatewayError>(),
                Some(GatewayError::ServeEnvMissing { var }) if *var == SERVE_GATEWAY_ID_ENV
            ),
            "a gateway without its run's id would label supervisors with the shared default: \
             {err:#}"
        );
        clear_remote_env();
    }

    #[test]
    fn a_remote_dial_back_names_the_gateway_service_and_the_loop_service() {
        let _guard = crate::test_support::env_lock();
        clear_remote_env();
        unsafe {
            std::env::set_var(SERVE_GATEWAY_ID_ENV, "crucible-run-a");
        }
        let dial_back = DialBack {
            grpc_endpoint: "https://172.30.0.9:17670".into(),
            host_gateway_ip: IpAddr::from([172, 30, 0, 10]),
        };
        let t = gateway_toml(
            17670,
            ComputeDriver::Kubernetes,
            &DriverImages::default(),
            None,
            Some(&dial_back),
            "crucible-run-a-gw-client",
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&t).unwrap();
        let k8s = &parsed["openshell"]["drivers"]["kubernetes"];
        assert_eq!(
            k8s["grpc_endpoint"].as_str(),
            Some("https://172.30.0.9:17670")
        );
        assert_eq!(k8s["host_gateway_ip"].as_str(), Some("172.30.0.10"));
        assert_eq!(
            k8s["client_tls_secret_name"].as_str(),
            Some("crucible-run-a-gw-client")
        );
        assert_eq!(k8s["gateway_id"].as_str(), Some("crucible-run-a"));
        assert!(
            k8s.get("provider_spiffe_workload_api_socket_path")
                .is_none(),
            "no socket configured, none mounted"
        );
        unsafe {
            std::env::set_var(
                SANDBOX_SPIFFE_SOCKET_ENV,
                "/spiffe-workload-api/spire-agent.sock",
            );
        }
        let with_socket = gateway_toml(
            17670,
            ComputeDriver::Kubernetes,
            &DriverImages::default(),
            None,
            Some(&dial_back),
            "crucible-run-a-gw-client",
        )
        .unwrap();
        unsafe {
            std::env::remove_var(SANDBOX_SPIFFE_SOCKET_ENV);
        }
        let parsed: toml::Value = toml::from_str(&with_socket).unwrap();
        assert_eq!(
            parsed["openshell"]["drivers"]["kubernetes"]["provider_spiffe_workload_api_socket_path"]
                .as_str(),
            Some("/spiffe-workload-api/spire-agent.sock")
        );
        clear_remote_env();
        let local = gateway_toml(
            17670,
            ComputeDriver::Kubernetes,
            &DriverImages::default(),
            None,
            Some(&dial_back),
            "crucible-run-a-gw-client",
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&local).unwrap();
        assert!(
            parsed["openshell"]["drivers"]["kubernetes"]
                .get("gateway_id")
                .is_none(),
            "an in-pod gateway keeps the driver's default id"
        );
    }
}
