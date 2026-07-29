//! forge-layer-capture: run a command against a base image's own root filesystem and push the files it
//! wrote as an OCI layer appended to that base. No container runtime, no buildah, no full rebuild: the
//! base is unpacked once into a digest-keyed rootfs cache, the command runs in an overlay+chroot so its
//! writes land in an upperdir, and that diff becomes one pushed layer. Prints the derived image's
//! digest-pinned ref as the last stdout line.
//!
//!   forge-layer-capture --base docker.io/vllm/vllm-openai:v0.23.0 \
//!     --cmd 'pip install -e /workspace/src' \
//!     --mount ./src:/workspace/src \
//!     --env PIP_NO_BUILD_ISOLATION=1 \
//!     --push quay.io/acme/candidate

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use forge::capture::{
    BindMount, CaptureConfig, ExecutorChoice, RootfsCache, capture_and_push, select_executor,
};
use forge::oci::{registry_of, resolve_auth};
use std::path::PathBuf;

#[derive(Clone, Copy, ValueEnum)]
enum ExecutorArg {
    /// Probe for a working privileged mount, else fall back to the user-namespace executor.
    Auto,
    /// Unprivileged: self-mapped user namespace with an in-namespace overlay mount.
    Userns,
    /// Direct privileged mount + chroot (needs real mount capability).
    Privileged,
}

impl From<ExecutorArg> for ExecutorChoice {
    fn from(a: ExecutorArg) -> Self {
        match a {
            ExecutorArg::Auto => ExecutorChoice::Auto,
            ExecutorArg::Userns => ExecutorChoice::Userns,
            ExecutorArg::Privileged => ExecutorChoice::Privileged,
        }
    }
}

#[derive(Parser)]
#[command(
    about = "Run a command against a base image's rootfs and push the resulting diff as an OCI layer."
)]
struct Args {
    /// The base image to capture against (e.g. docker.io/vllm/vllm-openai:v0.23.0).
    #[arg(long)]
    base: String,
    /// The command to run inside the image's rootfs (via `/bin/sh -c`).
    #[arg(long)]
    cmd: String,
    /// `HOST:CONTAINER`: bind-mount HOST into the execution root at CONTAINER. Repeatable.
    #[arg(long = "mount", value_name = "HOST:CONTAINER")]
    mounts: Vec<String>,
    /// `KEY=VALUE` env override, layered on top of the image's own environment. Repeatable.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// Destination repo (no tag); the pushed image is tagged by the diff's content and returned pinned.
    #[arg(long)]
    push: String,
    /// docker config.json-format authfile for the registries (the same one buildah uses).
    #[arg(long, env = "FORGE_AUTHFILE")]
    authfile: Option<PathBuf>,
    /// Root of the unpacked-rootfs cache and per-capture overlay scratch. Defaults to the forge volume.
    #[arg(long, default_value = "/var/lib/forge/capture")]
    cache: PathBuf,
    /// How many unpacked base rootfs to keep before least-recently-used eviction.
    #[arg(long, default_value_t = 3)]
    cache_keep: usize,
    /// Execution mechanism.
    #[arg(long, value_enum, default_value_t = ExecutorArg::Auto)]
    executor: ExecutorArg,
}

fn parse_mount(spec: &str) -> Result<BindMount> {
    let (host, container) = spec
        .split_once(':')
        .with_context(|| format!("--mount must be HOST:CONTAINER, got {spec}"))?;
    if !container.starts_with('/') {
        anyhow::bail!("--mount CONTAINER path must be absolute, got {container}");
    }
    Ok(BindMount {
        host: PathBuf::from(host),
        container: container.to_string(),
    })
}

fn parse_env(spec: &str) -> Result<(String, String)> {
    let (k, v) = spec
        .split_once('=')
        .with_context(|| format!("--env must be KEY=VALUE, got {spec}"))?;
    Ok((k.to_string(), v.to_string()))
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

    let mounts = args
        .mounts
        .iter()
        .map(|s| parse_mount(s))
        .collect::<Result<Vec<_>>>()?;
    let env = args
        .env
        .iter()
        .map(|s| parse_env(s))
        .collect::<Result<Vec<_>>>()?;

    // Per-registry creds, resolved the way docker/crane do (explicit authfile, else the operator's
    // default login, else anonymous). The base gets its own lookup so a private base still pulls.
    let base_auth = resolve_auth(args.authfile.as_deref(), &registry_of(&args.base)?)?;
    let target_auth = resolve_auth(args.authfile.as_deref(), &registry_of(&args.push)?)?;

    // The one runtime for all registry IO; everything below borrows its handle. Multi-thread so
    // Handle::block_on drives IO from sync code.
    let rt = tokio::runtime::Runtime::new().context("building the tokio runtime")?;

    let cfg = CaptureConfig {
        base_ref: args.base,
        command: args.cmd,
        mounts,
        env,
        target_repo: args.push,
    };
    let cache = RootfsCache::new(args.cache.join("rootfs-cache"), rt.handle().clone())
        .with_keep(args.cache_keep);
    let exec = select_executor(args.executor.into());

    let pushed = capture_and_push(
        &cfg,
        &cache,
        &args.cache,
        exec.as_ref(),
        &base_auth,
        &target_auth,
    )?;
    println!("{pushed}");
    Ok(())
}
