//! The `crucible-git` container of a run pod granted the `git` agent server: a native sidecar (an
//! init container that keeps running), so it is up before the loop starts and does not hold the
//! pod open after the loop exits. It shares the pod's network, and is the only container the run's
//! GitHub App token is mounted into: by directory, so a token rewritten in the Secret reaches it.
//! The loop writes the run's output bounds into a directory the two share, and `crucible-git`
//! reads them from there.
//!
//! The pod also maps GitHub's hostnames to loopback, where `crucible-git` serves them over TLS
//! with a CA it mints at startup, so the loop's tasks reach GitHub with stock `gh` and `git`.
//!
//! ```text
//!   pod ─┬─ loop          /run/crucible-git (rw)  /run/crucible-git-ca (ro)  no token
//!        └─ crucible-git  /run/crucible-git (ro)  /run/crucible-git-ca (rw)  /var/run/crucible-git-token/token
//!   hostAliases: 127.0.0.1 github.com api.github.com raw.githubusercontent.com codeload.github.com
//! ```

use crucible_contract::agent_servers::{AgentServer, ENV_AGENT_SERVERS, LocalServer, Transport};
use crucible_contract::git;
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

const CONTAINER: &str = "crucible-git";
const SHARED_VOLUME: &str = "crucible-git-shared";
const TOKEN_VOLUME: &str = "crucible-git-token";
const CA_VOLUME: &str = "crucible-git-ca";

/// Lets `crucible-git` bind [`git::TLS_BIND`] without privilege. Pod-scoped and namespaced, so
/// Kubernetes counts it among the safe sysctls.
const UNPRIVILEGED_PORT_START: &str = "net.ipv4.ip_unprivileged_port_start";

#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("{ENV_AGENT_SERVERS} does not decode: {0}")]
    Servers(#[from] serde_json::Error),
    #[error("the git server's URL {0:?} has no host and port")]
    Url(String),
    #[error("the git server's run issuer {0:?} is not <issuer>=<jwks uri>")]
    Issuer(String),
    #[error("the run pod has no loop container to run crucible-git beside")]
    NoLoop,
    #[error(
        "the git server's JWKS URI {0} is plain http; whoever answers it could mint run tokens \
         crucible-git accepts, so it must be https"
    )]
    InsecureJwks(String),
}

/// The granted `git` server in the delivery's plain env, if any.
pub(crate) fn granted(
    delivery: &crate::secrets::deliver::Delivery,
) -> Result<Option<AgentServer>, SidecarError> {
    let Some((_, raw)) = delivery
        .plain_env
        .iter()
        .find(|(var, _)| var == ENV_AGENT_SERVERS)
    else {
        return Ok(None);
    };
    Ok(crucible_contract::agent_servers::decode(raw)?
        .into_iter()
        .find(|s| matches!(s.transport, Transport::Local(LocalServer::Git(_)))))
}

fn mount(volume: &str, path: &str, read_only: bool) -> core::VolumeMount {
    core::VolumeMount {
        name: volume.to_string(),
        mount_path: path.to_string(),
        read_only: Some(read_only),
        ..Default::default()
    }
}

/// Add the `crucible-git` container when the run is granted the `git` server. The token is mounted
/// only when the run binds one; without it every call is refused.
pub fn add(
    pod: &mut core::Pod,
    secret: &str,
    delivery: &crate::secrets::deliver::Delivery,
) -> Result<(), SidecarError> {
    let Some(server) = granted(delivery)? else {
        return Ok(());
    };
    let Transport::Local(LocalServer::Git(site)) = &server.transport else {
        return Ok(());
    };
    let url = url::Url::parse(&server.url).map_err(|_| SidecarError::Url(server.url.clone()))?;
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return Err(SidecarError::Url(server.url.clone()));
    };
    let jwks = site
        .run_issuer
        .split_once('=')
        .and_then(|(_, jwks)| url::Url::parse(jwks).ok())
        .ok_or_else(|| SidecarError::Issuer(site.run_issuer.clone()))?;
    if jwks.scheme() != "https" {
        return Err(SidecarError::InsecureJwks(jwks.to_string()));
    }
    let run_id = pod.metadata.name.clone().unwrap_or_default();
    let Some(spec) = pod.spec.as_mut() else {
        return Err(SidecarError::NoLoop);
    };

    let loop_container = spec.containers.first_mut().ok_or(SidecarError::NoLoop)?;
    let loop_mounts = loop_container
        .volume_mounts
        .get_or_insert_with(Default::default);
    loop_mounts.push(mount(SHARED_VOLUME, git::SHARED_DIR, false));
    loop_mounts.push(mount(CA_VOLUME, git::CA_DIR, true));
    let loop_env = loop_container.env.get_or_insert_with(Default::default);
    for (name, value) in git::task_env() {
        if !loop_env.iter().any(|e| e.name == name) {
            loop_env.push(core::EnvVar {
                name: name.to_string(),
                value: Some(value),
                value_from: None,
            });
        }
    }
    let image = loop_container.image.clone();
    let security_context = loop_container.security_context.clone();

    let mut args: Vec<String> = [
        "serve",
        "--bind",
        &format!("0.0.0.0:{port}"),
        "--tls-bind",
        git::TLS_BIND,
        "--ca-dir",
        git::CA_DIR,
        "--run-issuer",
        &site.run_issuer,
        "--resource",
        &server.url,
        "--allowed-host",
        host,
        "--run-id",
        &run_id,
        "--bounds-file",
        git::BOUNDS_FILE,
    ]
    .map(String::from)
    .to_vec();
    let mut mounts = vec![
        mount(SHARED_VOLUME, git::SHARED_DIR, true),
        mount(CA_VOLUME, git::CA_DIR, false),
    ];
    let volumes = spec.volumes.get_or_insert_with(Default::default);
    for name in [SHARED_VOLUME, CA_VOLUME] {
        volumes.push(core::Volume {
            name: name.to_string(),
            empty_dir: Some(core::EmptyDirVolumeSource::default()),
            ..Default::default()
        });
    }
    if let Some(token) = &delivery.git_token {
        args.extend([
            "--token-file".to_string(),
            format!("{}/{}", git::TOKEN_DIR, git::TOKEN_FILE_NAME),
        ]);
        volumes.push(core::Volume {
            name: TOKEN_VOLUME.to_string(),
            secret: Some(core::SecretVolumeSource {
                secret_name: Some(secret.to_string()),
                items: Some(vec![core::KeyToPath {
                    key: token.key.clone(),
                    path: git::TOKEN_FILE_NAME.to_string(),
                    mode: None,
                }]),
                default_mode: Some(0o400),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(mount(TOKEN_VOLUME, git::TOKEN_DIR, true));
    }

    spec.host_aliases
        .get_or_insert_with(Default::default)
        .push(core::HostAlias {
            ip: "127.0.0.1".to_string(),
            hostnames: Some(git::GITHUB_HOSTS.iter().map(|h| h.to_string()).collect()),
        });
    spec.security_context
        .get_or_insert_with(Default::default)
        .sysctls
        .get_or_insert_with(Default::default)
        .push(core::Sysctl {
            name: UNPRIVILEGED_PORT_START.to_string(),
            value: "443".to_string(),
        });
    spec.init_containers
        .get_or_insert_with(Default::default)
        .push(core::Container {
            name: CONTAINER.to_string(),
            restart_policy: Some("Always".to_string()),
            image,
            command: Some(vec!["crucible-git".to_string()]),
            args: Some(args),
            volume_mounts: Some(mounts),
            // The kubelet probes the pod IP, so the sandbox listener; the CA is written before
            // any listener binds, so the loop starts only once tasks can trust it.
            startup_probe: Some(core::Probe {
                tcp_socket: Some(core::TCPSocketAction {
                    host: None,
                    port: IntOrString::Int(i32::from(port)),
                }),
                period_seconds: Some(1),
                failure_threshold: Some(30),
                ..Default::default()
            }),
            security_context,
            resources: Some(core::ResourceRequirements {
                requests: Some(
                    [
                        ("cpu".to_string(), Quantity("50m".to_string())),
                        ("memory".to_string(), Quantity("64Mi".to_string())),
                    ]
                    .into(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        });
    Ok(())
}
