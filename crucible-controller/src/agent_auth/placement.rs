//! Where agent tokens may be minted from. A sandbox's SPIFFE ID is only as trustworthy as the
//! namespace its supervisor pod runs in: anyone who can create a pod there can claim any sandbox's
//! id. So the controller mints nothing until the deploy profile names a dedicated sandbox
//! namespace that only per-run gateway pods create pods in, and refuses to hold the signing key in
//! a namespace pack code shares.

use crucible::deploy::DeployProfile;
use crucible::deploy::profile::KubeName;

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
    #[error(
        "cluster {cluster}'s SPIRE (CONTROLLER_AGENT_SPIRE) runs in {namespace:?}, which is also \
         {field}; pack code that can exec into pods there could mint any SVID"
    )]
    SpireExposed {
        cluster: String,
        namespace: String,
        field: &'static str,
    },
    #[error(
        "cluster {cluster}'s loop pods run in {namespace:?}, which is also {what}; pack code there \
         could reach what agent tokens rest on, so runs with isolated gateways are not dispatched"
    )]
    LoopExposed {
        cluster: String,
        namespace: String,
        what: String,
    },
}

/// The namespaces pack code can create or exec into pods in: loop pods, rigs, sandboxes, buildit's
/// builder pods and crucible-kueue's workloads.
fn pack_reachable(profile: &DeployProfile) -> Vec<(&'static str, &str)> {
    let mut reachable = vec![
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
    if let Some(buildit) = &profile.buildit {
        reachable.push(("buildit.namespace", buildit.namespace.as_str()));
    }
    if let Some(kueue) = &profile.kueue {
        for workloads in &kueue.workload_namespaces {
            reachable.push(("kueue.workload_namespaces", workloads.as_str()));
        }
    }
    reachable
}

/// Refuse a SPIRE whose namespace (its OIDC discovery Service's, in `CONTROLLER_AGENT_SPIRE`) is
/// one pack code reaches: a pod there can exec into the SPIRE server and mint any sandbox's SVID.
pub fn check_spire_exposure(
    spire: &[crate::agent_auth::issuer::ClusterSpire],
    profile: &DeployProfile,
) -> Result<(), PlacementError> {
    let reachable = pack_reachable(profile);
    for entry in spire {
        let namespace = entry.discovery.namespace.as_str();
        if let Some((field, _)) = reachable.iter().find(|(_, ns)| *ns == namespace) {
            return Err(PlacementError::SpireExposed {
                cluster: entry.cluster.clone(),
                namespace: namespace.to_string(),
                field,
            });
        }
    }
    Ok(())
}

/// Refuse a run whose loop pod would land, on `cluster`, in a namespace agent tokens rest on: the
/// sandbox namespace (pack code could create a pod claiming any sandbox's SPIFFE ID), that
/// cluster's SPIRE (it could exec in and mint any SVID), or on the hub the controller's own (it
/// holds the signing key). The profile's `loop_namespace` is checked at load,
/// but loop pods run where the controller's pod namespace or a spoke kubeconfig puts them.
pub fn check_loop_namespace(
    cluster: &str,
    loop_namespace: &str,
    profile: &DeployProfile,
    spire: &[crate::agent_auth::issuer::ClusterSpire],
    controller_namespace: Option<&str>,
) -> Result<(), PlacementError> {
    let mut guarded: Vec<(String, &str)> = vec![(
        "cluster.sandbox_namespace".to_string(),
        profile.cluster.sandbox_namespace(),
    )];
    for entry in spire.iter().filter(|s| s.cluster == cluster) {
        guarded.push((
            "the namespace of its SPIRE (CONTROLLER_AGENT_SPIRE)".to_string(),
            entry.discovery.namespace.as_str(),
        ));
    }
    if cluster == crate::runs::clusters::HUB_CLUSTER
        && let Some(own) = controller_namespace.filter(|ns| !ns.is_empty())
    {
        guarded.push(("the controller's own namespace".to_string(), own));
    }
    match guarded.into_iter().find(|(_, ns)| *ns == loop_namespace) {
        Some((what, _)) => Err(PlacementError::LoopExposed {
            cluster: cluster.to_string(),
            namespace: loop_namespace.to_string(),
            what,
        }),
        None => Ok(()),
    }
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

/// Refuse a controller whose namespace is one loop pods, rigs, sandboxes, builder pods or
/// crucible-kueue run in.
/// The people-only issuer needs this and not [`check_key_placement`]'s sandbox isolation, since
/// it vouches for no sandbox.
pub fn check_key_exposure(
    controller_namespace: Option<&str>,
    profile: &DeployProfile,
) -> Result<(), PlacementError> {
    let namespace = controller_namespace
        .filter(|ns| !ns.is_empty())
        .ok_or(PlacementError::UnknownControllerNamespace)?;
    let mut shared = pack_reachable(profile);
    if let Some(kueue) = &profile.kueue {
        shared.push(("kueue.namespace", kueue.namespace.as_str()));
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
            ("crucible-kueue-e2e", "kueue.workload_namespaces"),
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
        let building = profile(PROFILE);
        assert_eq!(
            check_key_placement(Some("builds"), &building),
            Err(PlacementError::KeyExposed {
                namespace: "builds".into(),
                field: "buildit.namespace",
            }),
            "the loop account can exec into any pod in buildit's namespace"
        );
        let unplaced = profile(&PROFILE.replace("sandbox_namespace = \"sandboxes\"\n", ""));
        assert_eq!(
            check_key_placement(Some("crucible-controller"), &unplaced),
            Err(PlacementError::NoSandboxNamespace)
        );
    }

    #[test]
    fn spire_must_run_where_no_pack_reachable_namespace_is() {
        let building = profile(PROFILE);
        let spire = |namespace: &str| -> crate::agent_auth::issuer::ClusterSpire {
            format!("dev=https://oidc.example spiffe://td/openshell/gateway/ {namespace}/oidc:443")
                .parse()
                .unwrap()
        };
        assert_eq!(
            check_spire_exposure(&[spire("spire-server")], &building),
            Ok(())
        );
        for (namespace, field) in [
            ("builds", "buildit.namespace"),
            ("crucible-system", "cluster.loop_namespace"),
            ("rig", "cluster.rig_namespace"),
            ("sandboxes", "cluster.sandbox_namespace"),
            ("crucible-kueue-e2e", "kueue.workload_namespaces"),
        ] {
            assert_eq!(
                check_spire_exposure(&[spire(namespace)], &building),
                Err(PlacementError::SpireExposed {
                    cluster: "dev".into(),
                    namespace: namespace.into(),
                    field,
                }),
                "{namespace}"
            );
        }
    }

    #[test]
    fn loop_pods_must_not_land_where_agent_tokens_rest() {
        let placed = profile(PROFILE);
        let spire: Vec<crate::agent_auth::issuer::ClusterSpire> = vec![
            "east=https://oidc.example spiffe://td/openshell/gateway/ spire-east/oidc:443"
                .parse()
                .unwrap(),
        ];
        let check = |cluster: &str, namespace: &str| {
            check_loop_namespace(
                cluster,
                namespace,
                &placed,
                &spire,
                Some("crucible-controller"),
            )
        };
        assert_eq!(check("hub", "crucible-system"), Ok(()));
        assert_eq!(check("east", "loops"), Ok(()));
        for (cluster, namespace, what) in [
            ("east", "sandboxes", "cluster.sandbox_namespace"),
            ("hub", "sandboxes", "cluster.sandbox_namespace"),
            (
                "east",
                "spire-east",
                "the namespace of its SPIRE (CONTROLLER_AGENT_SPIRE)",
            ),
            (
                "hub",
                "crucible-controller",
                "the controller's own namespace",
            ),
        ] {
            assert_eq!(
                check(cluster, namespace),
                Err(PlacementError::LoopExposed {
                    cluster: cluster.into(),
                    namespace: namespace.into(),
                    what: what.into(),
                }),
                "{cluster}/{namespace}"
            );
        }
        assert_eq!(
            check("hub", "spire-east"),
            Ok(()),
            "another cluster's SPIRE namespace is a different namespace"
        );
        assert_eq!(
            check("east", "crucible-controller"),
            Ok(()),
            "the signing key lives on the hub"
        );
    }
}
