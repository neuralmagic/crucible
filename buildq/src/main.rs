#![allow(clippy::disallowed_macros)]

//! buildq: container builds as Kueue-queued one-shot Jobs. A build pushes its context as an
//! image, runs as a Job Kueue admits against its queue's quota, pushes the built image, and
//! ends; Kubernetes deletes the Job after `--ttl`. Status comes from watching the Jobs.

mod context;
mod job;
mod mcp;
mod queue;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use rmcp::ServiceExt;

use crate::context::Registry;
use crate::job::{Profile, Storage};
use crate::queue::{BuildRequest, Queue};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(flatten)]
    opts: Opts,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct Opts {
    /// Kubeconfig context (default the current one)
    #[arg(long, global = true, env = "BUILDQ_KUBECONTEXT")]
    kubecontext: Option<String>,
    /// Namespace builds run in
    #[arg(
        long,
        global = true,
        env = "BUILDQ_NAMESPACE",
        default_value = "buildq"
    )]
    namespace: String,
    /// Kueue LocalQueue builds wait in; without one they start at once
    #[arg(long, global = true, env = "BUILDQ_QUEUE")]
    queue: Option<String>,
    /// Repository contexts and images are pushed to, e.g. quay.io/acme/builds
    #[arg(
        long,
        global = true,
        env = "BUILDQ_REPO",
        default_value = "quay.io/wseaton/buildq"
    )]
    repo: String,
    #[arg(long, global = true, value_enum, default_value = "overlay")]
    storage: Storage,
    /// Node architecture builds run on
    #[arg(long, global = true, default_value = "amd64")]
    arch: String,
    /// The builder's resource requests, e.g. cpu=32, repeatable
    #[arg(long = "request", global = true, value_parser = parse_kv)]
    requests: Vec<(String, String)>,
    /// Seconds a build may run
    #[arg(long, global = true, default_value_t = 3600)]
    deadline: i64,
    /// Seconds a finished build's Job is kept before Kubernetes deletes it
    #[arg(long, global = true, default_value_t = 900)]
    ttl: i32,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the build tools over MCP on stdio; contexts are directories of `--workdir`
    Serve {
        #[arg(long, default_value = ".")]
        workdir: PathBuf,
    },
    /// Queue a build of a context directory and print its status
    Build {
        context: PathBuf,
        #[arg(short = 'f', long, default_value = "Dockerfile")]
        dockerfile: String,
        #[arg(long = "build-arg", value_parser = parse_kv)]
        build_args: Vec<(String, String)>,
        /// Wait up to this many seconds for the build to end
        #[arg(long)]
        wait: Option<u64>,
    },
    /// Print a build's status
    Status {
        build_id: String,
        #[arg(long)]
        wait: Option<u64>,
    },
    /// Print the end of a build's log
    Logs {
        build_id: String,
        #[arg(long, default_value_t = 100)]
        tail: i64,
    },
    /// Cancel a build
    Cancel { build_id: String },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, got {s:?}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("installing the rustls crypto provider"))?;
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,rmcp=warn")),
        )
        .init();
    let cli = Cli::parse();
    let queue = queue(&cli.opts).await?;
    let print = |v: &queue::BuildStatus| -> Result<()> {
        println!("{}", serde_json::to_string(v)?);
        Ok(())
    };
    match cli.cmd {
        Cmd::Serve { workdir } => {
            let service = mcp::Buildq::new(queue, workdir)
                .serve(rmcp::transport::stdio())
                .await
                .context("starting the MCP server")?;
            service.waiting().await?;
        }
        Cmd::Build {
            context,
            dockerfile,
            build_args,
            wait,
        } => {
            let req = BuildRequest {
                context,
                dockerfile,
                build_args: build_args.into_iter().collect(),
            };
            let id = queue.submit(&req).await?;
            eprintln!("queued {id}");
            print(
                &queue
                    .status(
                        &id,
                        Some(wait.map_or(Duration::from_secs(5), Duration::from_secs)),
                    )
                    .await?,
            )?;
        }
        Cmd::Status { build_id, wait } => {
            print(
                &queue
                    .status(&build_id, wait.map(Duration::from_secs))
                    .await?,
            )?;
        }
        Cmd::Logs { build_id, tail } => print!("{}", queue.logs(&build_id, tail).await?),
        Cmd::Cancel { build_id } => queue.cancel(&build_id).await?,
    }
    Ok(())
}

async fn queue(opts: &Opts) -> Result<std::sync::Arc<Queue>> {
    let config = kube::Config::from_kubeconfig(&kube::config::KubeConfigOptions {
        context: opts.kubecontext.clone(),
        ..Default::default()
    })
    .await
    .context("loading kubeconfig")?;
    let client = kube::Client::try_from(config).context("building the kube client")?;
    let profile = Profile {
        namespace: opts.namespace.clone(),
        queue: opts.queue.clone(),
        storage: opts.storage,
        arch: Some(opts.arch.clone()),
        requests: opts.requests.iter().cloned().collect::<BTreeMap<_, _>>(),
        deadline_secs: opts.deadline,
        ttl_secs: opts.ttl,
    };
    Queue::start(client, profile, Registry::load(&opts.repo)?).await
}
