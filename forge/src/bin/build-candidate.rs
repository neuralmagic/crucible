//! build-candidate: build a workspace into a candidate image, push it, record it as the latest
//! candidate, and (unless `--no-deploy`) roll the live deployment onto it. The engine-side capability
//! the broker tools call into; run it by hand on the loop pod to prove build → push → deploy → measure
//! before the broker is connected (phase 1). Domain-neutral: every target comes from a flag or
//! its `FORGE_*` env fallback.
//!
//!   build-candidate                          # build+push+deploy the cwd at its git sha
//!   build-candidate --context src --tag spike --no-deploy

use anyhow::Result;
use clap::Parser;
use forge::{
    BuildConfig, BuildOutcome, DEFAULT_LATEST_FILE, DeployConfig, build_and_push, context_tag,
    deploy, record_latest,
};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
#[error("build failed (see log above)")]
struct CompileFailed;

#[derive(Debug, thiserror::Error)]
#[error("{what} is required to deploy (or pass --no-deploy)")]
struct MissingDeployArg {
    what: String,
}

#[derive(Parser)]
#[command(about = "Build + push (+ deploy) a workspace candidate image on the loop pod.")]
struct Args {
    /// Build context: the source root (Dockerfile + go.mod/etc). Defaults to the cwd.
    #[arg(long, env = "FORGE_BUILD_CONTEXT", default_value = ".")]
    context: PathBuf,
    /// Image tag. Defaults to the context's git short SHA.
    #[arg(long)]
    tag: Option<String>,
    /// Destination repo without a tag, e.g. quay.io/acme/candidate.
    #[arg(long, env = "FORGE_REGISTRY")]
    registry: String,
    #[arg(long, env = "FORGE_DOCKERFILE", default_value = "Dockerfile")]
    dockerfile: String,
    /// Robot push credential (auths json) scoped to the destination repo.
    #[arg(long, env = "FORGE_AUTHFILE")]
    authfile: PathBuf,
    #[arg(long, env = "FORGE_PLATFORM", default_value = "linux/amd64")]
    platform: String,
    /// Isolated buildah storage root (so the build can't race a co-located podman).
    #[arg(long, env = "FORGE_STORAGE_ROOT", default_value = "/var/lib/forge")]
    storage_root: PathBuf,
    /// Build + push only; don't roll the deployment onto the candidate.
    #[arg(long)]
    no_deploy: bool,
    /// Content-addressed reuse: if `--tag` is already in the registry, skip the (multi-minute) rebuild
    /// and reuse the pushed image. Pair with a content tag (e.g. `cand-<sig-of-edits>`) so an apply
    /// hook only rebuilds the component the agent actually changed this turn.
    #[arg(long)]
    skip_if_exists: bool,
    #[arg(long, env = "FORGE_DEPLOY_NAMESPACE")]
    namespace: Option<String>,
    #[arg(long, env = "FORGE_DEPLOY_NAME")]
    deployment: Option<String>,
    #[arg(long, env = "FORGE_DEPLOY_CONTAINER")]
    container: Option<String>,
    #[arg(long, env = "FORGE_DEPLOY_TIMEOUT", default_value = "300s")]
    timeout: String,
    /// Where to record the latest built candidate (for deploy-candidate / ensure-deployed).
    #[arg(long, env = "FORGE_LATEST_FILE", default_value = DEFAULT_LATEST_FILE)]
    latest_file: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Pull the deploy target out before `args` is partially moved into BuildConfig below.
    let no_deploy = args.no_deploy;
    let skip_if_exists = args.skip_if_exists;
    let latest_file = args.latest_file.clone();
    let deploy_target = (
        args.namespace.clone(),
        args.deployment.clone(),
        args.container.clone(),
        args.timeout.clone(),
    );

    let build_cfg = BuildConfig {
        registry: args.registry,
        dockerfile: args.dockerfile,
        authfile: args.authfile,
        platform: args.platform,
        storage_root: Some(args.storage_root),
    };
    let tag = args.tag.unwrap_or_else(|| context_tag(&args.context));
    let image_ref = build_cfg.image_ref(&tag);

    // Roll the deployment onto `r` (unless --no-deploy). Closed over the deploy target so both the
    // reuse and the fresh-build path share one exit.
    let deploy_if = |r: &str| -> Result<()> {
        if no_deploy {
            return Ok(());
        }
        let (ns, dep, con, timeout) = &deploy_target;
        let deploy_cfg = DeployConfig {
            namespace: require(ns.clone(), "--namespace / FORGE_DEPLOY_NAMESPACE")?,
            deployment: require(dep.clone(), "--deployment / FORGE_DEPLOY_NAME")?,
            container: require(con.clone(), "--container / FORGE_DEPLOY_CONTAINER")?,
            rollout_timeout: timeout.clone(),
        };
        deploy(&deploy_cfg, r)
    };

    // Content-addressed reuse: if the tag is already in the registry, skip the rebuild entirely. A HEAD
    // for the manifest digest (no pull) is the cheap existence check; reuse its digest-pinned ref.
    if skip_if_exists
        && let Ok(pinned) = forge::oci::pin_digest(&image_ref, Some(&build_cfg.authfile))
    {
        eprintln!("==> {image_ref} already in the registry; reusing {pinned} (no rebuild)");
        record_latest(&latest_file, &pinned)?;
        deploy_if(&pinned)?;
        println!("{pinned}");
        return Ok(());
    }

    match build_and_push(&build_cfg, &args.context, &tag)? {
        BuildOutcome::CompileError { log } => {
            eprintln!("{log}");
            Err(CompileFailed.into())
        }
        BuildOutcome::Built { image_ref } => {
            // Pin to a digest so the deployed ref is immutable AND byte-identical across turns for
            // identical content: re-rolling an unchanged candidate then patches the same string, a
            // no-op rollout, no churn. Fall back to the tag ref if the registry HEAD can't resolve.
            let pinned = forge::oci::pin_digest(&image_ref, Some(&build_cfg.authfile))
                .unwrap_or_else(|e| {
                    eprintln!("==> warning: could not pin {image_ref} to a digest ({e:#}); using the tag ref");
                    image_ref.clone()
                });
            record_latest(&latest_file, &pinned)?;
            deploy_if(&pinned)?;
            // The pushed (and possibly deployed) ref on the last line, scriptable.
            println!("{pinned}");
            Ok(())
        }
    }
}

/// A deploy target is only required when actually deploying; surface a clear error rather than a
/// clap "required" that would also block `--no-deploy`.
fn require(v: Option<String>, what: &str) -> Result<String> {
    v.filter(|s| !s.is_empty()).ok_or_else(|| {
        MissingDeployArg {
            what: what.to_owned(),
        }
        .into()
    })
}
