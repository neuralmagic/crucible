use serde::Deserialize;
use std::collections::BTreeMap;

/// Build/deploy targets for a component's candidate image. The deploy renderer projects these into
/// the loop pod's broker-child env, so the pod stops being a hand-typed shadow of the build target.
/// Two layers, by who consumes them:
/// - `buildah` / `deploy_name` are forge's *generic* build/deploy contract (`FORGE_*`); any domain
///   that builds a Dockerfile candidate uses it, so it's typed.
/// - `env` is a generic escape hatch for env the *domain's* apply hook reads under its own names.
///   The engine projects it verbatim and
///   names none of it, so onboarding a domain with a different hook is config, not an engine change.
///
/// On a composite, `[deploy.<component>]` overrides the component's own `[deploy]`, so the issue
/// overlay owns its candidate repos without forking the base domain manifest.
#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct DeployCfg {
    /// The k8s Deployment this component's candidate rolls (projected as `FORGE_DEPLOY_NAME`).
    #[serde(default)]
    pub deploy_name: Option<String>,
    /// Build-from-Dockerfile target (forge `build-candidate`, buildah).
    #[serde(default)]
    pub buildah: Option<BuildahTarget>,
    /// Extra broker-child env the domain's hook reads under its own names (verbatim, engine-agnostic).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A buildah build target, projected as `FORGE_REGISTRY` / `FORGE_DOCKERFILE` / `FORGE_PLATFORM`, the
/// generic forge `build-candidate` contract.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct BuildahTarget {
    pub registry: String,
    pub dockerfile: String,
    #[serde(default = "default_platform")]
    pub platform: String,
}

fn default_platform() -> String {
    "linux/amd64".to_string()
}

impl DeployCfg {
    /// The broker-child env this deploy target projects. `FORGE_*` is forge's generic contract;
    /// `env` is the domain hook's own names, passed through. The renderer emits these so the loop pod
    /// isn't a second, hand-synced copy of the target.
    pub fn broker_env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(name) = &self.deploy_name {
            env.push(("FORGE_DEPLOY_NAME".to_string(), name.clone()));
        }
        if let Some(b) = &self.buildah {
            env.push(("FORGE_REGISTRY".to_string(), b.registry.clone()));
            env.push(("FORGE_DOCKERFILE".to_string(), b.dockerfile.clone()));
            env.push(("FORGE_PLATFORM".to_string(), b.platform.clone()));
        }
        for (k, v) in &self.env {
            env.push((k.clone(), v.clone()));
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::CompositeManifest;

    #[test]
    fn deploy_cfg_projects_forge_and_passes_hook_env_through() {
        // The buildah target + deploy_name project forge's generic contract; the `env` map is the
        // domain hook's own names, passed through verbatim (the engine names none of it).
        let d = DeployCfg {
            deploy_name: Some("my-deploy".into()),
            buildah: Some(BuildahTarget {
                registry: "quay.io/acme/cand".into(),
                dockerfile: "Dockerfile.x".into(),
                platform: "linux/amd64".into(),
            }),
            env: BTreeMap::from([("VLLM_BASE_REF".into(), "quay.io/acme/base:v1".into())]),
        };
        let env = d.broker_env();
        let get = |k: &str| {
            env.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("FORGE_DEPLOY_NAME"), Some("my-deploy"));
        assert_eq!(get("FORGE_REGISTRY"), Some("quay.io/acme/cand"));
        assert_eq!(get("FORGE_DOCKERFILE"), Some("Dockerfile.x"));
        assert_eq!(get("FORGE_PLATFORM"), Some("linux/amd64"));
        assert_eq!(
            get("VLLM_BASE_REF"),
            Some("quay.io/acme/base:v1"),
            "hook env passed through"
        );
    }

    #[test]
    fn composite_deploy_overlay_overrides_component_default() {
        // `[deploy.<component>]` on the composite wins over the component's own `[deploy]`; an unlisted
        // component falls back to its base manifest's. Mirrors an issue overlay owning the
        // issue-specific candidate repos without forking the base domain manifests.
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/domains/gamma");
        let m = CompositeManifest::load(&dir.join("crucible.delta.toml")).unwrap();
        let comps = m.resolve_components(&dir).unwrap();
        let alpha = comps.iter().find(|c| c.name == "alpha").unwrap();
        let beta = comps.iter().find(|c| c.name == "beta").unwrap();
        let alpha_env = m.deploy_for(alpha).expect("alpha deploy").broker_env();
        let beta_env = m.deploy_for(beta).expect("beta deploy").broker_env();
        // alpha's base manifest names registry.example.com/alpha-default; the overlay must win.
        assert!(
            alpha_env
                .iter()
                .any(|(k, v)| k == "FORGE_REGISTRY" && v == "registry.example.com/alpha-candidate"),
            "alpha build target from overlay, not the component default"
        );
        assert!(
            beta_env
                .iter()
                .any(|(k, v)| k == "BETA_CANDIDATE_REPO"
                    && v == "registry.example.com/beta-candidate"),
            "beta build target from overlay (the base ref is derived in the apply hook, not pinned here)"
        );
    }
}
