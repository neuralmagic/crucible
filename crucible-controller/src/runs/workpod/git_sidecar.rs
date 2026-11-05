//! The `crucible-git` container of a run pod granted the `git` agent server: a native sidecar (an
//! init container that keeps running), so it is up before the loop starts and does not hold the
//! pod open after the loop exits. It shares the pod's network, and is the only container the run's GitHub App token is
//! mounted into: by directory, so a token rewritten in the Secret reaches it. The loop writes the
//! run's output bounds into a directory the two share, and `crucible-git` reads them from there.
//!
//! ```text
//!   pod ─┬─ loop          /run/crucible-git (rw)   no token
//!        └─ crucible-git  /run/crucible-git (ro)   /var/run/crucible-git-token/token
//! ```

use crucible_contract::agent_servers::{AgentServer, ENV_AGENT_SERVERS, LocalServer, Transport};
use crucible_contract::git;
use k8s_openapi::api::core::v1 as core;

const CONTAINER: &str = "crucible-git";
const SHARED_VOLUME: &str = "crucible-git-shared";
const TOKEN_VOLUME: &str = "crucible-git-token";

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
}

/// The granted `git` server in the delivery's plain env, if any.
fn granted(
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
    let run_id = pod.metadata.name.clone().unwrap_or_default();
    let Some(spec) = pod.spec.as_mut() else {
        return Err(SidecarError::NoLoop);
    };
    let loop_container = spec.containers.first_mut().ok_or(SidecarError::NoLoop)?;
    loop_container
        .volume_mounts
        .get_or_insert_with(Default::default)
        .push(core::VolumeMount {
            name: SHARED_VOLUME.to_string(),
            mount_path: git::SHARED_DIR.to_string(),
            ..Default::default()
        });
    let image = loop_container.image.clone();
    let security_context = loop_container.security_context.clone();

    let mut args: Vec<String> = [
        "serve",
        "--bind",
        &format!("0.0.0.0:{port}"),
        "--local-bind",
        git::LOCAL_BIND,
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
    if jwks.scheme() == "http" {
        args.push("--insecure-jwks-http".to_string());
    }
    let mut mounts = vec![core::VolumeMount {
        name: SHARED_VOLUME.to_string(),
        mount_path: git::SHARED_DIR.to_string(),
        read_only: Some(true),
        ..Default::default()
    }];
    let volumes = spec.volumes.get_or_insert_with(Default::default);
    volumes.push(core::Volume {
        name: SHARED_VOLUME.to_string(),
        empty_dir: Some(core::EmptyDirVolumeSource::default()),
        ..Default::default()
    });
    if let Some(key) = &delivery.git_token {
        args.extend([
            "--token-file".to_string(),
            format!("{}/{}", git::TOKEN_DIR, git::TOKEN_FILE_NAME),
        ]);
        volumes.push(core::Volume {
            name: TOKEN_VOLUME.to_string(),
            secret: Some(core::SecretVolumeSource {
                secret_name: Some(secret.to_string()),
                items: Some(vec![core::KeyToPath {
                    key: key.clone(),
                    path: git::TOKEN_FILE_NAME.to_string(),
                    mode: None,
                }]),
                default_mode: Some(0o400),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(core::VolumeMount {
            name: TOKEN_VOLUME.to_string(),
            mount_path: git::TOKEN_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
    }
    spec.init_containers
        .get_or_insert_with(Default::default)
        .push(core::Container {
            name: CONTAINER.to_string(),
            restart_policy: Some("Always".to_string()),
            image,
            command: Some(vec!["crucible-git".to_string()]),
            args: Some(args),
            volume_mounts: Some(mounts),
            security_context,
            resources: Some(core::ResourceRequirements {
                requests: Some(
                    [
                        (
                            "cpu".to_string(),
                            k8s_openapi::apimachinery::pkg::api::resource::Quantity(
                                "50m".to_string(),
                            ),
                        ),
                        (
                            "memory".to_string(),
                            k8s_openapi::apimachinery::pkg::api::resource::Quantity(
                                "64Mi".to_string(),
                            ),
                        ),
                    ]
                    .into(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        });
    Ok(())
}
