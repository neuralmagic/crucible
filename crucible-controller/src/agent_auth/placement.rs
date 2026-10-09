//! Where agent tokens may be minted from. A sandbox's SPIFFE ID is only as trustworthy as the
//! namespace its supervisor pod runs in: anyone who can create a pod there can claim any sandbox's
//! id. So the controller mints nothing until the deploy profile names a dedicated sandbox
//! namespace that only per-run gateway pods create pods in, and refuses to hold the signing key in
//! a namespace pack code shares.

use crucible::deploy::DeployProfile;
use crucible::deploy::profile::KubeName;
use std::path::{Path, PathBuf};

/// Where Kubernetes projects the namespace of the pod's service account.
const SERVICE_ACCOUNT_NAMESPACE: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlacementError {
    #[error(
        "the deploy profile declares no cluster.sandbox_namespace; agent tokens need sandboxes in a \
         namespace no loop pod or pack can create pods in"
    )]
    NoSandboxNamespace,
    #[error(
        "the deploy profile does not run each run's gateway in its own pod (sandbox_driver = \
         \"kubernetes\" with a sandbox_namespace); agent tokens need supervisor pods only a run's \
         own gateway could have created"
    )]
    SharedGateway,
    #[error(
        "agent tokens need a deploy profile (CONTROLLER_DEPLOY_PROFILE) to check sandbox placement"
    )]
    NoProfile,
    #[error(
        "the controller's namespace is unknown (set POD_NAMESPACE from the downward API); the agent \
         signing key's Secret cannot be shown to sit outside pack code's reach"
    )]
    UnknownControllerNamespace,
    #[error(
        "the controller runs in {namespace:?}, which is also {field}; its agent signing key Secret \
         would be readable there"
    )]
    KeyExposed {
        namespace: String,
        field: &'static str,
    },
}

/// The sandbox namespace agent tokens may vouch for, under a profile that isolates each run's
/// gateway. Profile load has already refused one that coincides with the loop, rig, or
/// crucible-kueue namespaces.
pub fn sandbox_namespace(profile: Option<&DeployProfile>) -> Result<&KubeName, PlacementError> {
    let profile = profile.ok_or(PlacementError::NoProfile)?;
    let namespace = profile
        .cluster
        .sandbox_namespace
        .as_ref()
        .ok_or(PlacementError::NoSandboxNamespace)?;
    match crucible::deploy::render::gateway::isolated(profile) {
        true => Ok(namespace),
        false => Err(PlacementError::SharedGateway),
    }
}

/// Refuse a controller whose namespace (where its signing key Secret is mounted from) is one pack
/// code or sandboxes can read.
pub fn check_key_placement(
    controller_namespace: Option<&str>,
    profile: &DeployProfile,
) -> Result<(), PlacementError> {
    sandbox_namespace(Some(profile))?;
    check_key_exposure(controller_namespace, profile)
}

/// Refuse a controller whose namespace is one loop pods, rigs, sandboxes or crucible-kueue run in.
/// The people-only issuer needs this and not [`check_key_placement`]'s sandbox isolation, since
/// it vouches for no sandbox.
pub fn check_key_exposure(
    controller_namespace: Option<&str>,
    profile: &DeployProfile,
) -> Result<(), PlacementError> {
    let namespace = controller_namespace
        .filter(|ns| !ns.is_empty())
        .ok_or(PlacementError::UnknownControllerNamespace)?;
    let mut shared = vec![
        (
            "cluster.loop_namespace",
            profile.cluster.loop_namespace.as_str(),
        ),
        (
            "cluster.rig_namespace",
            profile.cluster.rig_namespace.as_str(),
        ),
        (
            "cluster.sandbox_namespace",
            profile.cluster.sandbox_namespace(),
        ),
    ];
    if let Some(kueue) = &profile.kueue {
        shared.push(("kueue.namespace", kueue.namespace.as_str()));
        shared.push((
            "kueue.workload_namespace",
            kueue.workload_namespace.as_str(),
        ));
    }
    match shared.into_iter().find(|(_, ns)| *ns == namespace) {
        Some((field, _)) => Err(PlacementError::KeyExposed {
            namespace: namespace.to_string(),
            field,
        }),
        None => Ok(()),
    }
}

/// The controller's own namespace: `POD_NAMESPACE`, else the service account's projected one.
pub fn controller_namespace() -> Option<String> {
    std::env::var("POD_NAMESPACE")
        .ok()
        .filter(|ns| !ns.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string(SERVICE_ACCOUNT_NAMESPACE)
                .ok()
                .map(|ns| ns.trim().to_string())
                .filter(|ns| !ns.is_empty())
        })
}

/// The deploy profile at `path`, read off the async runtime. `None` without a path.
pub async fn load_profile(path: Option<&Path>) -> anyhow::Result<Option<DeployProfile>> {
    use anyhow::Context;
    let Some(path) = path.map(PathBuf::from) else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || DeployProfile::load(&path).map(Some))
        .await
        .context("joining the deploy profile read")?
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::placement::*;
    use crate::agent_auth::testing::PROFILE;

    fn profile(toml_text: &str) -> DeployProfile {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn tokens_need_a_profile_that_names_a_sandbox_namespace() {
        assert_eq!(sandbox_namespace(None), Err(PlacementError::NoProfile));
        let unplaced = profile(&PROFILE.replace("sandbox_namespace = \"sandboxes\"\n", ""));
        assert_eq!(
            sandbox_namespace(Some(&unplaced)),
            Err(PlacementError::NoSandboxNamespace)
        );
        assert_eq!(
            sandbox_namespace(Some(&profile(PROFILE))).map(|ns| ns.as_str()),
            Ok("sandboxes")
        );
        let in_pod = profile(&PROFILE.replace("sandbox_driver = \"kubernetes\"\n", ""));
        assert_eq!(
            sandbox_namespace(Some(&in_pod)),
            Err(PlacementError::SharedGateway),
            "a gateway in the loop pod can be made to create any gateway's supervisors"
        );
    }

    #[test]
    fn the_signing_key_must_live_where_no_loop_sandbox_or_kueue_namespace_reaches() {
        let placed = profile(PROFILE);
        check_key_placement(Some("crucible-controller"), &placed).expect("a namespace of its own");
        assert_eq!(
            check_key_placement(None, &placed),
            Err(PlacementError::UnknownControllerNamespace)
        );
        assert_eq!(
            check_key_placement(Some(""), &placed),
            Err(PlacementError::UnknownControllerNamespace)
        );
        for (namespace, field) in [
            ("crucible-system", "cluster.loop_namespace"),
            ("rig", "cluster.rig_namespace"),
            ("sandboxes", "cluster.sandbox_namespace"),
            ("crucible-kueue-e2e", "kueue.workload_namespace"),
        ] {
            assert_eq!(
                check_key_placement(Some(namespace), &placed),
                Err(PlacementError::KeyExposed {
                    namespace: namespace.to_string(),
                    field,
                }),
                "{namespace}"
            );
        }
        let unplaced = profile(&PROFILE.replace("sandbox_namespace = \"sandboxes\"\n", ""));
        assert_eq!(
            check_key_placement(Some("crucible-controller"), &unplaced),
            Err(PlacementError::NoSandboxNamespace)
        );
    }
}
