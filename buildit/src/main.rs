#![allow(clippy::disallowed_macros)]

mod auth;
mod backend;
mod build;
mod context;
mod job;
mod logs;
mod mcp;
mod oci;
mod pod;
mod sandbox;
mod schedule;
mod step;
mod task;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::backend::Backend;

#[derive(Parser)]
#[command(
    name = "buildit",
    version,
    about = "Remote container builds on a Kubernetes cluster"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build and push an image via an unprivileged builder pod
    Build(Box<BuildArgs>),
    /// Wait for a detached build Job and print its digest-pinned ref
    Wait {
        /// Job name printed by `build --detach`
        job: String,
        #[arg(short, long, default_value = "default", env = "BUILDIT_NAMESPACE")]
        namespace: String,
        /// Kubeconfig context to use (defaults to the current context)
        #[arg(long)]
        kubecontext: Option<String>,
    },
    /// Serve build/status/run/logs/clean as stateless MCP tools over streamable http
    Mcp(Box<mcp::McpArgs>),
    /// Delete leftover buildit pods, jobs, and secrets (label app=buildit)
    Clean {
        #[arg(short, long, default_value = "default", env = "BUILDIT_NAMESPACE")]
        namespace: String,
        /// Kubeconfig context to use (defaults to the current context)
        #[arg(long)]
        kubecontext: Option<String>,
    },
}

#[derive(Args)]
pub struct BuildArgs {
    /// Fully-qualified image reference to build and push, e.g. quay.io/acme/foo:tag
    pub image: String,
    #[arg(short = 'f', long, default_value = "Dockerfile")]
    pub dockerfile: String,
    /// Build context directory
    #[arg(short, long, default_value = ".")]
    pub context: PathBuf,
    #[arg(short, long, default_value = "default", env = "BUILDIT_NAMESPACE")]
    pub namespace: String,
    /// Builder backend. All three run unprivileged; buildkit and buildah are
    /// rootless (uid 1000), kaniko is the legacy (unmaintained) fallback.
    #[arg(long, value_enum, default_value_t = Backend::Buildkit)]
    pub backend: Backend,
    /// pod: interactive build driven over the exec API, deleted when done.
    /// job: context pushed to the registry, build runs as a Kubernetes Job
    /// that survives client disconnect; follow up with `buildit wait <job>`.
    #[arg(long, value_enum, default_value_t = Mode::Pod)]
    pub mode: Mode,
    /// apply: create the resources and run the build. render: print the
    /// manifests to stdout as YAML (no cluster access, no context push,
    /// secret data redacted).
    #[arg(long, value_enum, default_value_t = Output::Apply)]
    pub output: Output,
    /// idle: prefer nodes with no GPU workloads (best effort). any: skip the
    /// scout and let the scheduler place the builder anywhere.
    #[arg(long, value_enum, default_value_t = Schedule::Idle)]
    pub schedule: Schedule,
    /// Resource requests for the builder container, repeatable: --request cpu=2 --request memory=4Gi
    #[arg(long = "request", value_name = "KEY=QTY", value_parser = parse_kv)]
    pub requests: Vec<(String, String)>,
    /// Resource limits for the builder container, repeatable: --limit cpu=8 --limit memory=16Gi
    #[arg(long = "limit", value_name = "KEY=QTY", value_parser = parse_kv)]
    pub limits: Vec<(String, String)>,
    /// Name of a PersistentVolumeClaim to mount as the builder's cache
    /// (created if missing, labeled app=buildit). Persists layer/store cache
    /// across builds. RWO caveat: concurrent builds contend for the volume.
    #[arg(long, value_name = "PVC_NAME", conflicts_with = "cache_hostpath")]
    pub cache_pvc: Option<String>,
    /// Size for the cache PVC when buildit has to create it
    #[arg(long, value_name = "QTY", default_value = "20Gi")]
    pub cache_size: String,
    /// Node-local directory to mount as the builder's cache (hostPath,
    /// DirectoryOrCreate). Fast on NVMe-backed nodes; pair with --node so
    /// consecutive builds land on the same cache.
    #[arg(long, value_name = "PATH", conflicts_with = "cache_pvc")]
    pub cache_hostpath: Option<String>,
    /// Pin the builder to a specific node (nodeSelector on hostname).
    /// Overrides --schedule.
    #[arg(long, value_name = "NODE")]
    pub node: Option<String>,
    /// Labels for the built image, repeatable: --label team=infra
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = parse_kv)]
    pub labels: Vec<(String, String)>,
    /// Labels for the pushed context image (job mode). Defaults to
    /// quay.expires-after=2w on quay registries so buildit-ctx-* tags
    /// self-prune; pass any --context-label to override the default.
    #[arg(long = "context-label", value_name = "KEY=VALUE", value_parser = parse_kv)]
    pub context_labels: Vec<(String, String)>,
    /// Multi-stage build target to stop at
    #[arg(long, value_name = "STAGE")]
    pub target: Option<String>,
    /// Build without pushing (pod mode only). Nothing is printed to stdout,
    /// and registry credentials are shipped only if ~/.docker/config.json has them.
    #[arg(long)]
    pub no_push: bool,
    /// Build args, repeatable: --build-arg KEY=VALUE
    #[arg(long = "build-arg", value_name = "KEY=VALUE")]
    pub build_args: Vec<String>,
    /// Kubeconfig context to use (defaults to the current context)
    #[arg(long)]
    pub kubecontext: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Mode {
    /// interactive builder pod, exec-driven
    Pod,
    /// detached Kubernetes Job, reattach with `buildit wait`
    Job,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Output {
    /// create the resources and run the build
    Apply,
    /// print manifests as YAML without touching the cluster
    Render,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Schedule {
    /// prefer nodes without active GPU workloads (best effort)
    Idle,
    /// no placement hints
    Any,
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .ok_or_else(|| format!("expected KEY=QTY, got {s:?}"))
}

impl BuildArgs {
    fn resources(&self) -> backend::Resources {
        backend::Resources {
            requests: self.requests.clone(),
            limits: self.limits.clone(),
        }
    }

    fn cache(&self) -> Option<backend::CacheVolume<'_>> {
        match (&self.cache_pvc, &self.cache_hostpath) {
            (Some(pvc), _) => Some(backend::CacheVolume::Pvc(pvc)),
            (_, Some(path)) => Some(backend::CacheVolume::HostPath(path)),
            _ => None,
        }
    }
}

async fn client_for(kubecontext: Option<&str>) -> Result<kube::Client> {
    let config = match kubecontext {
        Some(ctx) => kube::Config::from_kubeconfig(&kube::config::KubeConfigOptions {
            context: Some(ctx.to_string()),
            ..Default::default()
        })
        .await
        .with_context(|| format!("loading kubeconfig context {ctx}"))?,
        None => {
            if let Ok(kc) = kube::config::Kubeconfig::read()
                && let Some(current) = &kc.current_context
            {
                tracing::info!("using current kubecontext: {current}");
            }
            kube::Config::infer()
                .await
                .context("inferring kube config")?
        }
    };
    kube::Client::try_from(config).context("building kube client")
}

#[tokio::main]
async fn main() -> Result<()> {
    // kube wants ring, oci-client's deps drag in aws-lc-rs; with both in the
    // tree rustls refuses to guess, so pick ring explicitly
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("installing rustls crypto provider"))?;
    // stderr for logs, stdout stays clean for the digest-pinned ref
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Build(args) => match args.output {
            Output::Render => build::render(&args),
            Output::Apply => {
                let client = client_for(args.kubecontext.as_deref()).await?;
                build::run(client, &args).await
            }
        },
        Cmd::Wait {
            job,
            namespace,
            kubecontext,
        } => {
            let client = client_for(kubecontext.as_deref()).await?;
            job::wait(client, &namespace, &job).await
        }
        Cmd::Mcp(args) => {
            let client = client_for(args.kubecontext.as_deref()).await?;
            mcp::serve(client, *args).await
        }
        Cmd::Clean {
            namespace,
            kubecontext,
        } => {
            let client = client_for(kubecontext.as_deref()).await?;
            let deleted = pod::clean(client.clone(), &namespace).await?
                + job::clean(client, &namespace).await?;
            if deleted == 0 {
                tracing::info!("nothing to clean in {namespace}");
            }
            Ok(())
        }
    }
}
