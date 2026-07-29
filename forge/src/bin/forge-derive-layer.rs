//! forge-derive-layer: append a file-overlay layer onto a base image and push it, no container runtime.
//!
//! The CLI over [`forge::oci::derive_layer`]: the overlay-candidate path (base image + the agent's
//! edited files). Prints the derived image's digest-pinned ref to stdout.
//!
//!   forge-derive-layer --base docker.io/vllm/vllm-openai:v0.23.0 \
//!     --target quay.io/acme/candidate:cand-abc \
//!     --file usr/local/lib/python3.12/dist-packages/app/metrics.py=./app/metrics.py

use anyhow::{Context, Result};
use clap::Parser;
use forge::oci::{OverlayFile, derive_layer, registry_of, resolve_auth};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    about = "Append a file-overlay layer onto a base image and push it (no container runtime)."
)]
struct Args {
    /// The base image to derive from (e.g. docker.io/vllm/vllm-openai:v0.23.0).
    #[arg(long)]
    base: String,
    /// The full target ref to push the derived image to, including a tag.
    #[arg(long)]
    target: String,
    /// docker config.json-format authfile for the target registry (the same one buildah uses).
    #[arg(long, env = "FORGE_AUTHFILE")]
    authfile: Option<PathBuf>,
    /// `IMGPATH=LOCALPATH`: bake LOCALPATH's bytes at IMGPATH (no leading slash) in the image. Repeatable.
    #[arg(long = "file", value_name = "IMGPATH=LOCALPATH", required = true)]
    files: Vec<String>,
}

fn main() -> Result<()> {
    // Events on stderr (RUST_LOG-filtered, default info) so stdout stays the machine-readable ref.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();

    let mut files = Vec::with_capacity(args.files.len());
    for spec in &args.files {
        let (img_path, local) = spec
            .split_once('=')
            .with_context(|| format!("--file must be IMGPATH=LOCALPATH, got {spec}"))?;
        let contents =
            std::fs::read(local).with_context(|| format!("reading overlay source {local}"))?;
        files.push(OverlayFile {
            path: img_path.to_string(),
            contents,
        });
    }

    // Per-registry creds, resolved the way docker/crane do (explicit authfile, else the operator's
    // default login, else anonymous). The base gets its own lookup so a private base (same-registry
    // mirror or otherwise) pulls with real creds instead of 401ing.
    let base_auth = resolve_auth(args.authfile.as_deref(), &registry_of(&args.base)?)?;
    let target_auth = resolve_auth(args.authfile.as_deref(), &registry_of(&args.target)?)?;

    let rt = tokio::runtime::Runtime::new().context("building the tokio runtime")?;
    let pushed = rt.block_on(derive_layer(
        &args.base,
        &args.target,
        &files,
        &base_auth,
        &target_auth,
    ))?;
    println!("{pushed}");
    Ok(())
}
