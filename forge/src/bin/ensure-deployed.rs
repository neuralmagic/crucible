//! ensure-deployed, the `[world].apply_cmd` backstop: before the judge measures, make sure the
//! live deployment is running the latest built candidate.
//!
//! Idempotent. The agent normally calls the broker's `deploy_candidate` itself, so the deployment is
//! already current and this is a no-op. But `apply` runs every iteration as the engine-side guarantee
//! that "the measured candidate is the built one": if a candidate was built this run and the
//! deployment isn't on it, roll it now. If nothing was built (no recorded candidate), leave the
//! deployment untouched; the judge measures whatever is live.

use anyhow::Result;
use clap::Parser;
use forge::{DEFAULT_LATEST_FILE, DeployConfig, current_image, deploy, read_latest};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Ensure the deployment runs the latest built candidate (apply_cmd backstop).")]
struct Args {
    /// Where the latest built candidate was recorded by build-candidate.
    #[arg(long, env = "FORGE_LATEST_FILE", default_value = DEFAULT_LATEST_FILE)]
    latest_file: PathBuf,
    #[arg(long, env = "FORGE_DEPLOY_NAMESPACE")]
    namespace: String,
    #[arg(long, env = "FORGE_DEPLOY_NAME")]
    deployment: String,
    #[arg(long, env = "FORGE_DEPLOY_CONTAINER")]
    container: String,
    #[arg(long, env = "FORGE_DEPLOY_TIMEOUT", default_value = "300s")]
    timeout: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let Some(candidate) = read_latest(&args.latest_file)? else {
        eprintln!("ensure-deployed: no candidate built this run; leaving the deployment as-is");
        return Ok(());
    };

    let cfg = DeployConfig {
        namespace: args.namespace,
        deployment: args.deployment,
        container: args.container,
        rollout_timeout: args.timeout,
    };
    match current_image(&cfg)? {
        Some(live) if live == candidate => {
            eprintln!("ensure-deployed: deployment already on {candidate}");
            Ok(())
        }
        _ => {
            eprintln!("ensure-deployed: rolling deployment onto {candidate}");
            deploy(&cfg, &candidate)
        }
    }
}
