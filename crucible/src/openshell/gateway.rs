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

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Gateway bind/connect port.
pub const GATEWAY_PORT: u16 = 17670;
/// The registered gateway name.
pub const GATEWAY_NAME: &str = "ci";

/// The Secret name carrying the generated client mTLS material to sandbox pods (see
/// [`KubernetesDriverConfig::client_tls_secret_name`]). Published by `boot()` into the sandbox
/// namespace from the local certgen output.
pub const CLIENT_TLS_SECRET: &str = "crucible-openshell-client-tls";

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
    /// The value OpenShell expects in `compute_drivers` and as the `[openshell.drivers.<name>]`
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

/// The `[openshell.drivers.kubernetes]` config, emitted from a typed struct so the serde field
/// names track the driver's own `KubernetesComputeConfig`. Every field is skip-if-empty: an
/// omitted key means "use the driver's default", which is load-bearing (e.g. an empty
/// `host_gateway_ip` deliberately omits the pod's `hostAliases`). `deny_unknown_fields` on the
/// upstream struct rejects any name we mistype, so every field here must match exactly.
#[derive(Debug, Default, Serialize)]
pub struct KubernetesDriverConfig {
    /// The gateway URL sandbox pods dial back (`OPENSHELL_ENDPOINT`, tonic-parsed). The driver
    /// defaults it to the empty string and passes it verbatim, which the supervisor's policy
    /// fetch rejects as "invalid gRPC endpoint" and crash-loops on, so this must always be
    /// set. `https://` puts the sandbox client in mTLS mode, reading its material from the
    /// mounted client-TLS secret; the hostname must match both a server-cert SAN and the
    /// `hostAliases` entry the driver injects from `host_gateway_ip`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub grpc_endpoint: String,
    /// The Secret (in the sandbox namespace) carrying the generated client mTLS material. The
    /// driver mounts it into sandbox pods and points `OPENSHELL_TLS_CA/CERT/KEY` at its
    /// `ca.crt`/`tls.crt`/`tls.key` keys, the only channel a sandbox gets TLS material
    /// through, and an `https://` endpoint without it dies with "OPENSHELL_TLS_CA is required".
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
    pub supervisor_image: Option<String>,
    /// Always `init-container`: `image-volume` needs the `ImageVolume` feature gate our cluster
    /// (v1.35) does not have.
    pub supervisor_sideload_method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_gateway_ip: Option<String>,
    /// Ubuntu 24.04 nodes run AppArmor enforcing; RuntimeDefault can block the
    /// supervisor's netns setup. Set Unconfined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_armor_profile: Option<String>,
}

impl KubernetesDriverConfig {
    pub fn new(supervisor_image: Option<&str>) -> Self {
        Self {
            supervisor_image: supervisor_image.map(str::to_owned),
            supervisor_sideload_method: "init-container".to_owned(),
            ..Self::default()
        }
    }
}

/// Render `~/.config/openshell/gateway.toml` for `driver`. Under `Podman` an optional supervisor
/// image override appends a `[openshell.drivers.podman]` block (the in-pod sandbox image source);
/// under `Kubernetes` it flows into the typed `[openshell.drivers.kubernetes]` block.
pub fn gateway_toml(
    port: u16,
    driver: ComputeDriver,
    supervisor_image: Option<&str>,
) -> Result<String> {
    let mut s = format!(
        "[openshell]\nversion = 1\n\n[openshell.gateway]\nbind_address = \"0.0.0.0:{port}\"\ncompute_drivers = [\"{}\"]\n",
        driver.as_str()
    );
    match driver {
        ComputeDriver::Podman => {
            if let Some(img) = supervisor_image {
                s.push_str(&format!(
                    "\n[openshell.drivers.podman]\nsupervisor_image = \"{img}\"\n"
                ));
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
            let mut cfg = KubernetesDriverConfig::new(supervisor_image);
            // `host.openshell.internal` is the one name that lines up end to end: it is the
            // `--server-san` we pass to generate-certs, and the hostAlias the driver injects
            // into sandbox pods from `host_gateway_ip`, so the sandbox resolves it to this
            // pod and the TLS handshake's SAN check passes. The raw pod IP would resolve but
            // fail SAN verification.
            cfg.grpc_endpoint =
                format!("https://{}:{port}", ComputeDriver::Kubernetes.broker_host());
            cfg.client_tls_secret_name = CLIENT_TLS_SECRET.to_owned();
            // At runtime the render-projected env vars fill the driver config fields that
            // are unknowable at render time or vary per profile.
            if let Ok(ip) = std::env::var("CRUCIBLE_POD_IP")
                && !ip.is_empty()
            {
                cfg.host_gateway_ip = Some(ip);
            }
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
            if let Ok(profile) = std::env::var("CRUCIBLE_SANDBOX_APP_ARMOR_PROFILE")
                && !profile.is_empty()
            {
                cfg.app_armor_profile = Some(profile);
            }
            let body = toml::to_string(&cfg).context("serializing kubernetes driver config")?;
            s.push_str(&format!("\n[openshell.drivers.kubernetes]\n{body}"));
        }
    }
    Ok(s)
}

/// `openshell-gateway generate-certs --output-dir <tls> --server-san host.openshell.internal`.
pub fn generate_certs_args(tls_dir: &str) -> Vec<String> {
    vec![
        "generate-certs".into(),
        "--output-dir".into(),
        tls_dir.into(),
        "--server-san".into(),
        "host.openshell.internal".into(),
    ]
}

/// `openshell gateway add https://localhost:<port> --local --name ci`.
pub fn register_args(port: u16) -> Vec<String> {
    vec![
        "gateway".into(),
        "add".into(),
        format!("https://localhost:{port}"),
        "--local".into(),
        "--name".into(),
        GATEWAY_NAME.into(),
    ]
}

fn registration_matches(body: &[u8], port: u16) -> bool {
    let Ok(rows) = serde_json::from_slice::<Vec<serde_json::Value>>(body) else {
        return false;
    };
    let endpoint = format!("https://localhost:{port}");
    rows.iter().any(|row| {
        row.get("name").and_then(serde_json::Value::as_str) == Some(GATEWAY_NAME)
            && row.get("endpoint").and_then(serde_json::Value::as_str) == Some(endpoint.as_str())
            && row.get("type").and_then(serde_json::Value::as_str) == Some("local")
            && row.get("auth").and_then(serde_json::Value::as_str) == Some("mtls")
    })
}

fn registration_exists(port: u16) -> bool {
    Command::new("openshell")
        .args(["gateway", "list", "--output", "json"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| registration_matches(&output.stdout, port))
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

fn podman_socket() -> PathBuf {
    let override_socket = external_podman_socket();
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    podman_socket_from(override_socket.as_deref(), xdg.as_deref(), libc_getuid())
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
/// `supervisor_image` flows into the podman driver block (default emulator image used
/// when `None`).
#[tracing::instrument(name = "gateway_boot", skip_all, fields(driver = ?driver))]
pub async fn ensure_running(
    driver: ComputeDriver,
    supervisor_image: Option<&str>,
) -> Result<Option<String>> {
    if !is_running().await {
        // The loop pod sets OPENSHELL_SUPERVISOR_IMAGE (e.g. the aws-provider supervisor);
        // honor it when the caller didn't pass one explicitly.
        let env_img = std::env::var("OPENSHELL_SUPERVISOR_IMAGE")
            .ok()
            .filter(|s| !s.is_empty());
        boot(driver, supervisor_image.or(env_img.as_deref())).await?;
    }
    check_gateway_version().await
}

/// Gate the healthy gateway's reported version. Hard-fail only below the minimum; every
/// degraded probe outcome (no probe, no version, mismatched rev, unparseable string) is a
/// returned warning, so a gateway-side format change can't brick the loop.
async fn check_gateway_version() -> Result<Option<String>> {
    use crate::openshell::grpc::{self, VersionGate};
    let Some(probe) = grpc::HealthProbe::new() else {
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
        VersionGate::TooOld { reported } => bail!(
            "openshell gateway {reported} is older than the minimum {min_major}.{min_minor}.{min_patch} \
             this crucible requires — rebuild the loop image's gateway from the pinned fork rev \
             {} (Cargo.lock's openshell-core rev; see the openshell-gateway workflow)",
            grpc::EXPECTED_GATEWAY_REV
        ),
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

async fn boot(driver: ComputeDriver, supervisor_image: Option<&str>) -> Result<()> {
    // 1. rootless podman API socket (the podman compute driver). Skipped under kubernetes,
    // where the sandbox is a sibling pod and there is no local daemon to boot.
    if needs_podman_socket(driver) {
        let sock = podman_socket();
        if external_podman_socket().is_none() {
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
            bail!(
                "{} podman API socket did not appear at {}",
                if external_podman_socket().is_some() {
                    "configured"
                } else {
                    "managed"
                },
                sock.display()
            );
        }
    }

    // 2. gateway config + 3. TLS certs.
    write_config(driver, supervisor_image)?;
    let tls_dir = state_dir()?.join("tls");
    std::fs::create_dir_all(&tls_dir)
        .with_context(|| format!("creating tls dir {}", tls_dir.display()))?;
    let certs = Command::new("openshell-gateway")
        .args(generate_certs_args(&tls_dir.to_string_lossy()))
        .env("OPENSHELL_LOCAL_TLS_DIR", &tls_dir)
        .output()
        .context("exec openshell-gateway generate-certs")?;
    if !certs.status.success() {
        bail!(
            "generate-certs failed: {}",
            String::from_utf8_lossy(&certs.stderr).trim()
        );
    }

    // 3b. Under kubernetes, ship the freshly generated client TLS material to the sandbox
    //     namespace: the driver mounts this Secret into every sandbox pod, and it is the only
    //     way the sandbox supervisor gets the CA/cert/key it needs to dial the https
    //     `grpc_endpoint` back to this gateway.
    if driver == ComputeDriver::Kubernetes {
        publish_client_tls_secret(&tls_dir)?;
    }

    // 4. launch the gateway, scrubbing the in-cluster k8s detection vars only under podman
    //    (the kubernetes driver needs them, see `scrub_k8s_vars`). It is spawned detached, so
    //    its stdout/stderr never land in this process's own log; redirect them to a file instead
    //    of discarding them, so a failure below (register/health timeout) can quote the
    //    gateway's own diagnostics rather than a bare "did not become healthy".
    let log_path = state_dir()?.join("gateway.log");
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
    gw.spawn().context("spawn openshell-gateway")?;

    // 5. register, retrying until the gateway is listening; 6. wait healthy.
    let mut last = String::new();
    let registered = registration_exists(GATEWAY_PORT)
        || wait_for(Duration::from_secs(30), || {
            match Command::new("openshell")
                .args(register_args(GATEWAY_PORT))
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
    if !registered {
        bail!(
            "gateway register failed within 30s: {last}\n{}",
            tail_gateway_log(&log_path)
        );
    }
    // The certs now exist (register wrote them), so a single reusable probe covers the poll loop.
    // Its `healthy()` is async, so the health wait is an async loop (not the sync `wait_for` the
    // socket/register waits use).
    let healthy = match crate::openshell::grpc::HealthProbe::new() {
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
        bail!(
            "gateway did not become healthy within 60s\nlast `openshell status`: {last_status}\n{}",
            tail_gateway_log(&log_path)
        );
    }
    Ok(())
}

/// Whether the gateway is up and answering: a `Health` RPC over the local mTLS channel
/// succeeds with a non-unhealthy status. Before the certs exist (pre-boot) there is nothing to
/// probe, which reads as "not running".
pub async fn is_running() -> bool {
    match crate::openshell::grpc::HealthProbe::new() {
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

/// Server-side apply the [`CLIENT_TLS_SECRET`] Secret from the certgen output at `tls_dir`
/// (`ca.crt`, `client/tls.crt`, `client/tls.key` → the `ca.crt`/`tls.crt`/`tls.key` keys the
/// driver's mount points `OPENSHELL_TLS_CA/CERT/KEY` at). Idempotent across turns; re-applying
/// after a cert refresh converges the mounted material. The namespace mirrors the driver
/// config: `CRUCIBLE_SANDBOX_NAMESPACE`, falling back to the driver's own default.
fn publish_client_tls_secret(tls_dir: &std::path::Path) -> Result<()> {
    let read = |rel: &str| -> Result<String> {
        let p = tls_dir.join(rel);
        std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))
    };
    let ns = std::env::var("CRUCIBLE_SANDBOX_NAMESPACE")
        .ok()
        .filter(|s| !s.is_empty())
        // The kubernetes driver's DEFAULT_K8S_NAMESPACE, used when the config omits `namespace`.
        .unwrap_or_else(|| "openshell".to_string());
    let secret = k8s_openapi::api::core::v1::Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(CLIENT_TLS_SECRET.to_string()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        string_data: Some(std::collections::BTreeMap::from([
            ("ca.crt".to_string(), read("ca.crt")?),
            ("tls.crt".to_string(), read("client/tls.crt")?),
            ("tls.key".to_string(), read("client/tls.key")?),
        ])),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    };
    let yaml = serde_norway::to_string(&secret).context("serializing the client TLS secret")?;
    forge::kube::apply_yaml(&yaml)
        .with_context(|| format!("publishing Secret {CLIENT_TLS_SECRET} to namespace {ns}"))
}

/// `~/.local/state/openshell`, the gateway's state/cert home.
fn state_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME unset")?;
    Ok(PathBuf::from(home).join(".local/state/openshell"))
}

/// Write `~/.config/openshell/gateway.toml` if its content changed (avoids churning a config
/// a running gateway may have read).
fn write_config(driver: ComputeDriver, supervisor_image: Option<&str>) -> Result<()> {
    let home = std::env::var("HOME").context("HOME unset")?;
    let dir = PathBuf::from(home).join(".config/openshell");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("gateway.toml");
    let rendered = gateway_toml(GATEWAY_PORT, driver, supervisor_image)?;
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
    use super::*;

    // The podman rendering must be byte-for-byte what `gateway_toml` produced before the driver
    // became a knob. These are frozen snapshots.
    const PODMAN_NO_IMAGE: &str = "[openshell]\nversion = 1\n\n[openshell.gateway]\nbind_address = \"0.0.0.0:17670\"\ncompute_drivers = [\"podman\"]\n";
    const PODMAN_WITH_IMAGE: &str = "[openshell]\nversion = 1\n\n[openshell.gateway]\nbind_address = \"0.0.0.0:17670\"\ncompute_drivers = [\"podman\"]\n\n[openshell.drivers.podman]\nsupervisor_image = \"registry.example.com/epp-sandbox:x\"\n";

    #[test]
    fn podman_rendering_is_byte_identical_without_image() {
        let t = gateway_toml(17670, ComputeDriver::Podman, None).unwrap();
        assert_eq!(t, PODMAN_NO_IMAGE);
    }

    #[test]
    fn podman_rendering_is_byte_identical_with_image() {
        let t = gateway_toml(
            17670,
            ComputeDriver::Podman,
            Some("registry.example.com/epp-sandbox:x"),
        )
        .unwrap();
        assert_eq!(t, PODMAN_WITH_IMAGE);
    }

    #[test]
    fn kubernetes_rendering_parses_and_carries_expected_keys() {
        let t = gateway_toml(
            17670,
            ComputeDriver::Kubernetes,
            Some("registry.example.com/epp-sandbox:x"),
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&t).expect("kubernetes gateway.toml must parse");

        assert_eq!(
            parsed["openshell"]["gateway"]["compute_drivers"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(toml::Value::as_str),
            Some("kubernetes")
        );
        let k8s = &parsed["openshell"]["drivers"]["kubernetes"];
        assert_eq!(
            k8s["supervisor_image"].as_str(),
            Some("registry.example.com/epp-sandbox:x")
        );
        assert_eq!(
            k8s["supervisor_sideload_method"].as_str(),
            Some("init-container")
        );
        // Skip-if-empty fields must be absent, not emitted as empty strings, so the driver's
        // own defaults win (e.g. an empty host_gateway_ip omits hostAliases).
        assert!(k8s.get("host_gateway_ip").is_none());
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
        let k8s = gateway_toml(17670, ComputeDriver::Kubernetes, None).unwrap();
        let parsed: toml::Value = toml::from_str(&k8s).unwrap();
        assert_eq!(
            parsed["openshell"]["gateway"]["auth"]["allow_unauthenticated_users"].as_bool(),
            Some(true),
            "{k8s}"
        );

        let podman = gateway_toml(17670, ComputeDriver::Podman, None).unwrap();
        assert!(!podman.contains("allow_unauthenticated_users"), "{podman}");
    }

    /// The driver defaults `grpc_endpoint` to "" and passes it verbatim into the sandbox's
    /// `OPENSHELL_ENDPOINT`, which tonic rejects ("invalid gRPC endpoint") and the supervisor
    /// crash-loops on, so the k8s rendering must always pin it. The hostname must be the
    /// certgen `--server-san` AND the driver-injected hostAlias (both host.openshell.internal),
    /// and the secret name must be exactly what `boot()` publishes.
    #[test]
    fn kubernetes_rendering_pins_the_sandbox_dial_back_endpoint() {
        let t = gateway_toml(17670, ComputeDriver::Kubernetes, None).unwrap();
        let parsed: toml::Value = toml::from_str(&t).unwrap();
        let k8s = &parsed["openshell"]["drivers"]["kubernetes"];
        assert_eq!(
            k8s["grpc_endpoint"].as_str(),
            Some("https://host.openshell.internal:17670")
        );
        assert_eq!(
            k8s["client_tls_secret_name"].as_str(),
            Some(CLIENT_TLS_SECRET)
        );

        // Podman stays untouched (the frozen snapshots above also guard this).
        let podman = gateway_toml(17670, ComputeDriver::Podman, None).unwrap();
        assert!(!podman.contains("grpc_endpoint"), "{podman}");
    }

    #[test]
    fn kubernetes_rendering_omits_supervisor_image_when_absent() {
        let t = gateway_toml(17670, ComputeDriver::Kubernetes, None).unwrap();
        assert!(!t.contains("supervisor_image"), "{t}");
        assert!(
            t.contains("supervisor_sideload_method = \"init-container\""),
            "{t}"
        );
    }

    /// The field names `openshell-gateway`'s pinned fork (`wseaton/OpenShell@f25ab2e4`,
    /// `crates/openshell-driver-kubernetes/src/config.rs::KubernetesComputeConfig`) actually
    /// accepts. That struct is `#[serde(deny_unknown_fields)]`, so any name we emit that is not
    /// in this list kills the gateway process on startup with no output in the pod log (its
    /// stdout/stderr are captured to `gateway.log` in the state dir, not this process's own
    /// log, see `boot`'s `tail_gateway_log`). Confirmed live against the pinned binary;
    /// update this list (and re-verify live) whenever the fork pin bumps.
    const UPSTREAM_KUBERNETES_COMPUTE_CONFIG_FIELDS: &[&str] = &[
        "namespace",
        "service_account_name",
        "default_image",
        "image_pull_policy",
        "image_pull_secrets",
        "supervisor_image",
        "supervisor_image_pull_policy",
        "supervisor_sideload_method",
        "grpc_endpoint",
        "ssh_socket_path",
        "client_tls_secret_name",
        "host_gateway_ip",
        "enable_user_namespaces",
        "app_armor_profile",
        "workspace_default_storage_size",
        "default_runtime_class_name",
        "sa_token_ttl_secs",
        "provider_spiffe_workload_api_socket_path",
    ];

    #[test]
    fn kubernetes_rendering_only_emits_fields_upstream_accepts() {
        let t = gateway_toml(
            17670,
            ComputeDriver::Kubernetes,
            Some("registry.example.com/epp-sandbox:x"),
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&t).unwrap();
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
            register_args(17670),
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
    fn exact_existing_registration_is_idempotent_but_wrong_endpoint_is_not() {
        let exact =
            br#"[{"name":"ci","endpoint":"https://localhost:17670","type":"local","auth":"mtls"}]"#;
        assert!(registration_matches(exact, 17670));
        let wrong = br#"[{"name":"ci","endpoint":"https://remote.example:17670","type":"local","auth":"mtls"}]"#;
        assert!(!registration_matches(wrong, 17670));
        assert!(!registration_matches(b"not json", 17670));
    }

    #[test]
    fn certs_request_the_internal_san() {
        let v = generate_certs_args("/tls");
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
}
