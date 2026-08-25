//! `crucible deploy`: crucible renders its own deployment instead of hand-writing the loop-pod /
//! RBAC YAML. The renderer projects the *manifest* (the run: components, broker, judge, world,
//! `[deploy]` targets) + a thin per-cluster *deploy profile* (the environment: namespaces, secret
//! names, resources, the loop image, generic hook/gate env), resolving image tags to digests.
//!
//! `render` emits the YAML for review/gitops (the default, the renderer stays declarative); `apply`
//! is a thin convenience that pipes it through `kubectl apply -f -` (same emit-vs-apply split as
//! engine-side build-vs-deploy). Both a composite manifest and a plain single-domain manifest render,
//! a single domain is a degenerate composite of one component, so it needs its own `[deploy]` block
//! naming that one target (see [`render::RenderInput::from_manifest`]).

mod controller;
pub mod profile;
mod render;

pub use render::{
    DigestResolver, MANAGED_BY_LABEL, PackDelivery, PlaybookLaunch, ProposeTier, RegistryDigests,
    RenderOpts, TurnKind, TurnOpts,
};

use crate::manifest::{self, CompositeManifest, Manifest};
use anyhow::{Context, Result};
use profile::DeployProfile;
use render::RenderInput;
use std::path::Path;

/// Render the deployment YAML for a manifest (composite or single-domain) + a deploy profile.
pub fn render_yaml(manifest_path: &Path, profile_path: &Path, opts: &RenderOpts) -> Result<String> {
    let manifest_dir = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let manifest_file = manifest_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("manifest path has a file name")?;
    let profile = DeployProfile::load_with_fleet(profile_path, opts.clusters_file.as_deref())?;

    if manifest::is_composite(manifest_path) {
        let composite = CompositeManifest::load(manifest_path)?;
        let input = RenderInput::from_composite(&composite, manifest_dir)?;
        render::render(input, manifest_dir, manifest_file, &profile, opts)
    } else {
        let manifest = Manifest::load(manifest_path)?;
        if manifest.is_task() {
            eprintln!(
                "[crucible deploy] WARNING: task mode: no [judge] — this pod keeps and \
                 publishes every completed turn unscored"
            );
        }
        manifest::ensure_injects_resolve(&manifest, manifest_dir)?;
        let name = manifest_dir
            .file_name()
            .and_then(|n| n.to_str())
            .context("manifest dir has a name")?;
        let input = match opts.playbook {
            Some(_) => RenderInput::from_playbook_manifest(&manifest, name),
            None => RenderInput::from_manifest(&manifest, name)?,
        };
        render::render(input, manifest_dir, manifest_file, &profile, opts)
    }
}

/// Emit the rendered YAML to stdout (review / gitops / `kubectl apply -f -` by hand).
pub fn render_cmd(manifest_path: &Path, profile_path: &Path, opts: &RenderOpts) -> Result<()> {
    let yaml = render_yaml(manifest_path, profile_path, opts)?;
    print!("{yaml}");
    Ok(())
}

/// Render then server-side apply via the typed kube client (no `kubectl`). The renderer stays
/// declarative; applying is the thin wrapper.
pub fn apply_cmd(manifest_path: &Path, profile_path: &Path, opts: &RenderOpts) -> Result<()> {
    let yaml = render_yaml(manifest_path, profile_path, opts)?;
    forge::kube::apply_yaml(&yaml).context("applying the rendered deployment")
}

/// Render the outer-loop controller's Deployment/PVC/Service/RBAC from the profile's `[controller]`
/// table alone, there is no per-run manifest, the controller watches many repos.
///
/// Deprecated: the `crucible-controller` Helm chart is the canonical packaging path now. Kept this
/// release for existing profile-native gitops; slated for removal.
pub fn render_controller_yaml(profile_path: &Path, opts: &RenderOpts) -> Result<String> {
    let profile = DeployProfile::load(profile_path)?;
    controller::render(&profile, opts)
}

/// Emit the rendered controller YAML to stdout.
pub fn render_controller_cmd(profile_path: &Path, opts: &RenderOpts) -> Result<()> {
    let yaml = render_controller_yaml(profile_path, opts)?;
    print!("{yaml}");
    Ok(())
}

/// Render one grounded-rank turn pod from a deploy profile + the per-turn issue/repo/sandbox, and
/// emit it to stdout. The controller shells this (the `PodDispatcher` subprocess render pattern),
/// then stamps the work-pod labels + its ownerReference before creating the pod.
pub fn render_turn_cmd(profile_path: &Path, opts: &render::TurnOpts) -> Result<()> {
    let profile = DeployProfile::load(profile_path)?;
    let yaml = render::render_turn(&profile, opts)?;
    print!("{yaml}");
    Ok(())
}

/// Render the controller then server-side apply it (same emit-vs-apply split as the run's `deploy apply`).
pub fn apply_controller_cmd(profile_path: &Path, opts: &RenderOpts) -> Result<()> {
    let yaml = render_controller_yaml(profile_path, opts)?;
    forge::kube::apply_yaml(&yaml).context("applying the rendered controller deployment")
}
