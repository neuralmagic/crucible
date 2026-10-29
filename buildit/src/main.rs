#![allow(clippy::disallowed_macros)]

mod auth;
mod backend;
mod build;
mod client;
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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};

use crate::backend::Backend;
use crate::client::Call;
use crate::mcp::{BuildParams, CleanParams, LogsParams, PushParams, RunParams, StatusParams};

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
    /// Build and push IMAGE via an unprivileged builder pod. With --server, build --context on
    /// that server instead and print its status once the build ends
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
    /// Delete leftover buildit pods, jobs, and secrets (label app=buildit). With --server, delete
    /// BUILD_ID on that server, or every build of its caller
    Clean {
        /// The build to delete on the server
        #[arg(requires = "server", conflicts_with = "kubecontext")]
        build_id: Option<String>,
        #[arg(short, long, default_value = "default", env = "BUILDIT_NAMESPACE")]
        namespace: String,
        /// Kubeconfig context to use (defaults to the current context)
        #[arg(long, conflicts_with = "server")]
        kubecontext: Option<String>,
        #[command(flatten)]
        server: ServerOpt,
    },
    /// Report a build's status, from a server
    Status {
        build_id: String,
        #[command(flatten)]
        server: ServerArg,
    },
    /// Run a command in a fresh container of a succeeded build, on a server
    Run(Box<RunArgs>),
    /// Push a succeeded build to the server's repository
    Push {
        build_id: String,
        #[command(flatten)]
        server: ServerArg,
    },
    /// Read part of a build or run log, from a server
    Logs(Box<LogsArgs>),
    /// Print the digest DIR gets as a build context, the `context_digest` builds report
    ContextDigest { dir: PathBuf },
}

/// A running `buildit mcp`'s socket, which the command calls.
#[derive(Args)]
pub struct ServerArg {
    /// The socket of the `buildit mcp` to call
    #[arg(long, env = client::ENV_SERVER, value_name = "SOCKET")]
    pub server: PathBuf,
}

/// A running `buildit mcp`'s socket; set, the command calls it instead of the cluster.
#[derive(Args)]
pub struct ServerOpt {
    /// The socket of a `buildit mcp` to call instead of the cluster
    #[arg(long, env = client::ENV_SERVER, value_name = "SOCKET")]
    pub server: Option<PathBuf>,
}

#[derive(Args)]
pub struct RunArgs {
    /// A build whose status is succeeded
    pub build_id: String,
    /// Run timeout in seconds (default 600)
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,
    /// Container path to copy into .buildit/<build_id>/<path> in the server's workdir, repeatable
    #[arg(long = "fetch", value_name = "PATH")]
    pub fetch: Vec<String>,
    /// The command and its arguments, after `--`
    #[arg(last = true, required = true)]
    pub cmd: Vec<String>,
    #[command(flatten)]
    pub server: ServerArg,
}

#[derive(Args)]
pub struct LogsArgs {
    pub build_id: String,
    /// "build" (default) or "run:<n>"
    #[arg(long)]
    pub which: Option<String>,
    /// Regex (Rust syntax); matches print as `N:text`, context as `N-text`, gaps as `--`
    #[arg(long)]
    pub grep: Option<String>,
    /// Lines of context around each grep match (max 20)
    #[arg(short = 'C', long, requires = "grep")]
    pub context: Option<u64>,
    /// First line to print (1-based)
    #[arg(long)]
    pub from_line: Option<u64>,
    /// Last line to print
    #[arg(long)]
    pub to_line: Option<u64>,
    /// Without --grep or a line range: the last N lines (default 100)
    #[arg(long, value_name = "N")]
    pub tail: Option<u64>,
    /// Output cap in bytes (default 16384, max 65536)
    #[arg(long)]
    pub max_bytes: Option<u64>,
    #[command(flatten)]
    pub server: ServerArg,
}

#[derive(Args)]
pub struct BuildArgs {
    /// Fully-qualified image reference to build and push, e.g. quay.io/acme/foo:tag
    #[arg(required_unless_present = "server", conflicts_with = "server")]
    pub image: Option<String>,
    #[arg(short = 'f', long, default_value = "Dockerfile")]
    pub dockerfile: String,
    /// Build context directory
    #[arg(short, long, default_value = ".")]
    pub context: PathBuf,
    #[arg(short, long, default_value = "default", env = "BUILDIT_NAMESPACE")]
    pub namespace: String,
    /// Builder backend. All three run unprivileged; buildkit and buildah are
    /// rootless (uid 1000), kaniko is the legacy (unmaintained) fallback.
    #[arg(long, value_enum, default_value_t = Backend::Buildkit, conflicts_with = "server")]
    pub backend: Backend,
    /// pod: interactive build driven over the exec API, deleted when done.
    /// job: context pushed to the registry, build runs as a Kubernetes Job
    /// that survives client disconnect; follow up with `buildit wait <job>`.
    #[arg(long, value_enum, default_value_t = Mode::Pod, conflicts_with = "server")]
    pub mode: Mode,
    /// apply: create the resources and run the build. render: print the
    /// manifests to stdout as YAML (no cluster access, no context push,
    /// secret data redacted).
    #[arg(long, value_enum, default_value_t = Output::Apply, conflicts_with = "server")]
    pub output: Output,
    /// idle: prefer nodes with no GPU workloads (best effort). any: skip the
    /// scout and let the scheduler place the builder anywhere.
    #[arg(long, value_enum, default_value_t = Schedule::Idle, conflicts_with = "server")]
    pub schedule: Schedule,
    /// Resource requests for the builder container, repeatable: --request cpu=2 --request memory=4Gi
    #[arg(long = "request", value_name = "KEY=QTY", value_parser = parse_kv, conflicts_with = "server")]
    pub requests: Vec<(String, String)>,
    /// Resource limits for the builder container, repeatable: --limit cpu=8 --limit memory=16Gi
    #[arg(long = "limit", value_name = "KEY=QTY", value_parser = parse_kv, conflicts_with = "server")]
    pub limits: Vec<(String, String)>,
    /// Name of a PersistentVolumeClaim to mount as the builder's cache
    /// (created if missing, labeled app=buildit). Persists layer/store cache
    /// across builds. RWO caveat: concurrent builds contend for the volume.
    #[arg(long, value_name = "PVC_NAME", conflicts_with_all = ["cache_hostpath", "server"])]
    pub cache_pvc: Option<String>,
    /// Size for the cache PVC when buildit has to create it
    #[arg(
        long,
        value_name = "QTY",
        default_value = "20Gi",
        conflicts_with = "server"
    )]
    pub cache_size: String,
    /// Node-local directory to mount as the builder's cache (hostPath,
    /// DirectoryOrCreate). Fast on NVMe-backed nodes; pair with --node so
    /// consecutive builds land on the same cache.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["cache_pvc", "server"])]
    pub cache_hostpath: Option<String>,
    /// Pin the builder to a specific node (nodeSelector on hostname).
    /// Overrides --schedule.
    #[arg(long, value_name = "NODE", conflicts_with = "server")]
    pub node: Option<String>,
    /// Labels for the built image, repeatable: --label team=infra
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = parse_kv, conflicts_with = "server")]
    pub labels: Vec<(String, String)>,
    /// Labels for the pushed context image (job mode). Defaults to
    /// quay.expires-after=2w on quay registries so buildit-ctx-* tags
    /// self-prune; pass any --context-label to override the default.
    #[arg(long = "context-label", value_name = "KEY=VALUE", value_parser = parse_kv, conflicts_with = "server")]
    pub context_labels: Vec<(String, String)>,
    /// Multi-stage build target to stop at
    #[arg(long, value_name = "STAGE")]
    pub target: Option<String>,
    /// Build without pushing (pod mode only). Nothing is printed to stdout,
    /// and registry credentials are shipped only if ~/.docker/config.json has them.
    #[arg(long, conflicts_with = "server")]
    pub no_push: bool,
    /// Build args, repeatable: --build-arg KEY=VALUE
    #[arg(long = "build-arg", value_name = "KEY=VALUE")]
    pub build_args: Vec<String>,
    /// Kubeconfig context to use (defaults to the current context)
    #[arg(long, conflicts_with = "server")]
    pub kubecontext: Option<String>,
    /// Build timeout in seconds on the server (default 1800)
    #[arg(
        long,
        value_name = "SECS",
        requires = "server",
        conflicts_with = "image"
    )]
    pub timeout: Option<u64>,
    /// Print the started build at once instead of waiting for it to end
    #[arg(long, requires = "server", conflicts_with = "image")]
    pub no_wait: bool,
    #[command(flatten)]
    pub server: ServerOpt,
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
    /// The image a standalone build pushes, which clap requires whenever no server is named.
    fn image(&self) -> Result<&str> {
        self.image
            .as_deref()
            .ok_or_else(|| anyhow!("a build without --server needs IMAGE"))
    }

    /// The server's build call: the context as an absolute path, which the server resolves in
    /// its own workdir.
    fn params(&self) -> Result<BuildParams> {
        let context = std::fs::canonicalize(&self.context)
            .with_context(|| format!("resolving the context {}", self.context.display()))?;
        let context = context
            .to_str()
            .ok_or_else(|| anyhow!("context {} is not utf-8", context.display()))?
            .to_string();
        let build_args = self
            .build_args
            .iter()
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
                _ => Err(anyhow!("--build-arg {kv:?} is not KEY=VALUE")),
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(BuildParams {
            context,
            dockerfile: Some(self.dockerfile.clone()),
            target: self.target.clone(),
            build_args,
            timeout_s: self.timeout,
        })
    }

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

async fn remote(socket: &std::path::Path, call: Result<Call>) -> ExitCode {
    match call {
        Ok(call) => client::run(socket, call).await.print(),
        Err(e) => client::Outcome::error(&e).print(),
    }
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
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
        Cmd::Build(args) => match (&args.server.server, args.output) {
            (Some(socket), _) => {
                let call = args.params().map(|params| Call::Build {
                    params,
                    wait: !args.no_wait,
                });
                return Ok(remote(socket, call).await);
            }
            (None, Output::Render) => build::render(&args)?,
            (None, Output::Apply) => {
                let client = client_for(args.kubecontext.as_deref()).await?;
                build::run(client, &args).await?
            }
        },
        Cmd::Wait {
            job,
            namespace,
            kubecontext,
        } => {
            let client = client_for(kubecontext.as_deref()).await?;
            job::wait(client, &namespace, &job).await?
        }
        Cmd::Mcp(args) => {
            let client = client_for(args.kubecontext.as_deref()).await?;
            mcp::serve(client, *args).await?
        }
        Cmd::Clean {
            build_id,
            server: ServerOpt {
                server: Some(socket),
            },
            ..
        } => {
            return Ok(remote(&socket, Ok(Call::Clean(CleanParams { build_id }))).await);
        }
        Cmd::Clean {
            namespace,
            kubecontext,
            ..
        } => {
            let client = client_for(kubecontext.as_deref()).await?;
            let deleted = pod::clean(client.clone(), &namespace).await?
                + job::clean(client, &namespace).await?;
            if deleted == 0 {
                tracing::info!("nothing to clean in {namespace}");
            }
        }
        Cmd::Status { build_id, server } => {
            let call = Call::Status(StatusParams { build_id });
            return Ok(remote(&server.server, Ok(call)).await);
        }
        Cmd::Run(args) => {
            let RunArgs {
                build_id,
                timeout,
                fetch,
                cmd,
                server,
            } = *args;
            let call = Call::Run(RunParams {
                build_id,
                cmd,
                timeout_s: timeout,
                fetch_paths: fetch,
            });
            return Ok(remote(&server.server, Ok(call)).await);
        }
        Cmd::Push { build_id, server } => {
            let call = Call::Push(PushParams { build_id });
            return Ok(remote(&server.server, Ok(call)).await);
        }
        Cmd::Logs(args) => {
            let LogsArgs {
                build_id,
                which,
                grep,
                context,
                from_line,
                to_line,
                tail,
                max_bytes,
                server,
            } = *args;
            let call = Call::Logs(LogsParams {
                build_id,
                which,
                grep,
                context,
                from_line,
                to_line,
                tail_lines: tail,
                max_bytes,
            });
            return Ok(remote(&server.server, Ok(call)).await);
        }
        Cmd::ContextDigest { dir } => {
            let outcome = match context::digest_dir(&dir) {
                Ok(digest) => client::Outcome::ok(serde_json::json!({ "context_digest": digest })),
                Err(e) => client::Outcome::error(&e),
            };
            return Ok(outcome.print());
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use clap::Parser;

    use crate::{Cli, Cmd};

    fn parse(argv: &[&str]) -> Result<Cmd, clap::Error> {
        Cli::try_parse_from(std::iter::once("buildit").chain(argv.iter().copied())).map(|c| c.cmd)
    }

    fn refused(argv: &[&str]) -> String {
        match parse(argv) {
            Ok(_) => panic!("{argv:?} parsed"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_server_build_takes_no_image_and_no_cluster_flags() {
        let Ok(Cmd::Build(args)) = parse(&["build", "--server", "/s", "-c", "svc", "--no-wait"])
        else {
            panic!("a server build parses")
        };
        assert_eq!(args.image, None);
        assert!(args.no_wait);
        for flags in [
            &["quay.io/a/b:c"][..],
            &["--backend", "kaniko"],
            &["--mode", "job"],
            &["--output", "render"],
            &["--schedule", "any"],
            &["--request", "cpu=2"],
            &["--limit", "cpu=2"],
            &["--cache-pvc", "c"],
            &["--cache-size", "1Gi"],
            &["--cache-hostpath", "/c"],
            &["--node", "n"],
            &["--label", "a=b"],
            &["--context-label", "a=b"],
            &["--no-push"],
            &["--kubecontext", "k"],
        ] {
            let mut argv = vec!["build", "--server", "/s"];
            argv.extend(flags);
            assert!(refused(&argv).contains("cannot be used with"), "{flags:?}");
        }
    }

    #[test]
    fn a_cluster_build_needs_an_image_and_takes_no_server_flags() {
        let Ok(Cmd::Build(args)) = parse(&["build", "quay.io/a/b:c", "-n", "ns"]) else {
            panic!("a cluster build parses")
        };
        assert_eq!(args.image().unwrap(), "quay.io/a/b:c");
        assert_eq!(args.server.server, None);
        assert!(refused(&["build"]).contains("<IMAGE>"));
        for flags in [&["--timeout", "5"][..], &["--no-wait"]] {
            let mut argv = vec!["build", "quay.io/a/b:c"];
            argv.extend(flags);
            refused(&argv);
        }
    }

    #[test]
    fn the_server_commands_need_a_server_and_clean_takes_either() {
        for argv in [
            &["status", "b"][..],
            &["push", "b"],
            &["logs", "b"],
            &["run", "b", "--", "true"],
            &["clean", "b"],
        ] {
            assert!(refused(argv).contains("--server <SOCKET>"), "{argv:?}");
        }
        assert!(
            refused(&["clean", "b", "--kubecontext", "k", "--server", "/s"])
                .contains("cannot be used")
        );
        assert!(matches!(
            parse(&["clean", "-n", "ns"]),
            Ok(Cmd::Clean { .. })
        ));
        assert!(refused(&["run", "b", "--server", "/s"]).contains("<CMD>"));
        let Ok(Cmd::Run(run)) = parse(&[
            "run",
            "b",
            "--server",
            "/s",
            "--timeout",
            "9",
            "--fetch",
            "/out",
            "--",
            "sh",
            "-c",
            "exit 3",
        ]) else {
            panic!("run parses")
        };
        assert_eq!(run.cmd, ["sh", "-c", "exit 3"]);
        assert_eq!(run.fetch, ["/out"]);
        assert_eq!(run.timeout, Some(9));
        assert!(refused(&["logs", "b", "--server", "/s", "-C", "2"]).contains("--grep"));
    }

    #[test]
    fn a_server_build_sends_its_context_as_an_absolute_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("svc"))?;
        let ctx = dir.path().join("svc");
        let ctx = ctx.to_str().unwrap_or_default();
        let Ok(Cmd::Build(args)) = parse(&[
            "build",
            "--server",
            "/s",
            "-c",
            ctx,
            "-f",
            "docker/Dockerfile",
            "--target",
            "base",
            "--build-arg",
            "A=x=y",
            "--build-arg",
            "EMPTY=",
            "--timeout",
            "60",
        ]) else {
            panic!("a server build parses")
        };
        let params = args.params()?;
        assert_eq!(
            std::path::Path::new(&params.context),
            dir.path().join("svc").canonicalize()?
        );
        assert_eq!(params.dockerfile.as_deref(), Some("docker/Dockerfile"));
        assert_eq!(params.target.as_deref(), Some("base"));
        assert_eq!(
            params.build_args,
            BTreeMap::from([("A".into(), "x=y".into()), ("EMPTY".into(), String::new())])
        );
        assert_eq!(params.timeout_s, Some(60));

        for bad in [
            &["-c", ctx, "--build-arg", "NOVALUE"][..],
            &["-c", ctx, "--build-arg", "=v"],
            &["-c", "/no/such/dir"],
        ] {
            let mut argv = vec!["build", "--server", "/s"];
            argv.extend(bad);
            let Ok(Cmd::Build(args)) = parse(&argv) else {
                panic!("{bad:?} parses")
            };
            assert!(args.params().is_err(), "{bad:?}");
        }
        Ok(())
    }
}
