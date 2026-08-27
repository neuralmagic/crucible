//! Render a domain's run deployment (Rendered deployments): the loop `Pod`, the cross-namespace `RoleBinding`,
//! and a deny-all-ingress `NetworkPolicy` on the loop pod, projected from the manifest (the source
//! of truth) + the per-cluster [`DeployProfile`], as real
//! `k8s-openapi` objects (so the rendered spec stays honest to the Kubernetes API). A composite
//! manifest and a plain single-domain manifest both project into a [`RenderInput`], a single domain
//! is a degenerate composite of one component, so everything below reads one shape.
//!
//! The split this enforces: the *manifest* owns the run (components, `[agent.broker]`, `[judge]`,
//! `[world]`, `[deploy]` targets) and the *profile* owns the environment (namespaces, secret names,
//! resources, the loop image, generic hook/gate env). Nothing domain-specific is hardcoded here, the
//! engine projects only the env it itself consumes (`BROKER_*`, forge's `FORGE_*`, mount-path consts)
//! and passes everything else through generic maps. Image refs are resolved to `@sha256:…` via the
//! in-process registry client, so the stale-cached-layer footgun (forget to re-pin) is gone.
//!
//! Split into [`kube`] (the loop `Pod`/RBAC/`NetworkPolicy` renderer) and [`turn`] (the one-shot
//! WorkPod turn renderer), the two share only the profile-derived helpers (`resources`,
//! `secret_env_vars`, `node_avoid_affinity`) and a handful of mount-path consts.

mod kube;
mod turn;

use anyhow::{Context, Result};

/// Resolves an image tag to its `@sha256:…` digest. A render never reaches a registry on its own:
/// a caller that wants pinned images passes a resolver in, and `None` emits every tag verbatim.
pub trait DigestResolver: Send + Sync {
    fn pin(&self, image: &str) -> Result<String>;
}

/// forge's in-process registry client: HEADs the manifest, with credentials from
/// `REGISTRY_AUTH_FILE` or the docker config. What `crucible deploy` uses unless `--no-pin`.
pub struct RegistryDigests;

impl DigestResolver for RegistryDigests {
    fn pin(&self, image: &str) -> Result<String> {
        forge::oci::pin_digest(image, None)
    }
}

/// `image`, pinned through `digests` when one is given.
pub(in crate::deploy) fn pin_image(
    digests: Option<&dyn DigestResolver>,
    what: &str,
    image: &str,
) -> Result<String> {
    match digests {
        Some(d) => d
            .pin(image)
            .with_context(|| format!("pinning {what} {image}")),
        None => Ok(image.to_string()),
    }
}

pub use kube::{MANAGED_BY_LABEL, PackDelivery, PlaybookLaunch, RenderInput, RenderOpts, render};
pub(in crate::deploy) use kube::{node_avoid_affinity, role_binding};
pub use turn::{PackPath, PackPathError, ProposeTier, TurnKind, TurnOpts, render_turn};
