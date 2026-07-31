//! `crucible-broker`: the generic provisioning-broker binary.
//!
//! Same MCP wire as a per-domain broker binary, but with NO domain trace-cache backend: it injects
//! the always-miss [`NullResolver`], so it serves only the cache-free tools (build/deploy/measure/
//! profile and the composite `deploy_composite`). For domains that just need the loop pod to build +
//! roll the agent's edits and never call `request_trace`. The engine spawns this binary from
//! `[agent.broker] bin = "crucible-broker"`; a domain with a real trace cache ships its own binary.
//!
//! All behaviour is env-configured (the gate `BROKER_BUILD` / `BROKER_COMPOSITE`, `FORGE_*` build/deploy
//! config, `BROKER_BIND`); the tool/handler code lives in `crucible_broker::server`.

use anyhow::Context;
use clap::Parser;
use crucible_broker::NullResolver;
use crucible_broker::server::McpServer;
use crucible_broker::types::TraceParams;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::path::PathBuf;

/// Where the broker listens. The sandbox reaches it via `host.containers.internal:8849` through the
/// openshell egress proxy; binding `0.0.0.0` puts it on the podman bridge rather than loopback only.
const DEFAULT_BIND: &str = "0.0.0.0:8849";
/// The MCP endpoint path. The sandbox's `.mcp.json` points at `http://host.containers.internal:8849/mcp`.
const MCP_PATH: &str = "/mcp";

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Submit a CPU-only sentinel job to a spoke through the full production submit/stream/parse
    /// path and print the typed result JSON. The standing hub-spoke acceptance probe.
    SpokeSmoke {
        /// The spoke's name (quoted in the sentinel result and in errors).
        cluster: String,
        /// Kubeconfig file path (a mounted spoke Secret in a pod, a file on a laptop). Unset with
        /// no --context = the ambient client.
        #[arg(long)]
        kubeconfig: Option<PathBuf>,
        /// Kubeconfig context to select (laptop parity; default kubeconfig resolution when
        /// --kubeconfig is unset).
        #[arg(long)]
        context: Option<String>,
        /// Namespace the smoke Job runs in on the spoke.
        #[arg(long)]
        namespace: String,
        /// Kueue LocalQueue on the spoke (must quota cpu/memory flavors).
        #[arg(long, default_value = "crucible-measure")]
        queue: String,
        /// Busybox-class image whose shell prints the sentinel result.
        #[arg(long, default_value = "busybox:1.37")]
        image: String,
        /// The Job's activeDeadlineSeconds (queue wait gets extra slack on top).
        #[arg(long, default_value_t = 300)]
        deadline_secs: i64,
        /// HTTP CONNECT proxy for the spoke API server (the proxy-reachable tier, e.g.
        /// http://10.x.x.x:3128). Wins over any proxy-url inside the kubeconfig.
        #[arg(long)]
        proxy_url: Option<String>,
        /// The spoke's reachability tier, quoted in errors (matches the MCP error contract).
        #[arg(long, default_value = "public")]
        tier: String,
    },
}

fn run_spoke_smoke(opts: crucible_broker::codegen::SpokeSmoke, tier: &str) -> anyhow::Result<()> {
    match crucible_broker::codegen::spoke_smoke(&opts) {
        Ok(result) => {
            println!("{result}");
            Ok(())
        }
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({
                    "status": "error",
                    "spoke": opts.cluster,
                    "tier": tier,
                    "error": e,
                })
            );
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Some(Cmd::SpokeSmoke {
        cluster,
        kubeconfig,
        context,
        namespace,
        queue,
        image,
        deadline_secs,
        proxy_url,
        tier,
    }) = Cli::parse().command
    {
        let _telemetry = crucible_broker::telemetry::init("crucible-broker");
        let target = if kubeconfig.is_none() && context.is_none() && proxy_url.is_none() {
            forge::kube::KubeTarget::Ambient
        } else {
            forge::kube::KubeTarget::Kubeconfig {
                path: kubeconfig,
                context,
                proxy_url,
            }
        };
        // run_gpu_job_with is a sync boundary; keep it off the async runtime's core thread.
        return tokio::task::spawn_blocking(move || {
            run_spoke_smoke(
                crucible_broker::codegen::SpokeSmoke {
                    cluster,
                    target,
                    namespace,
                    queue,
                    image,
                    deadline_secs,
                },
                &tier,
            )
        })
        .await
        .context("joining the spoke-smoke task")?;
    }
    // stderr fmt always; OTLP span export only when OTEL_EXPORTER_OTLP_ENDPOINT is set (the
    // engine's gRPC convention). The guard flushes spans on exit.
    let _telemetry = crucible_broker::telemetry::init("crucible-broker");

    // This broker serves no trace cache, so the frozen regime (the judge-impact reference for
    // `request_trace`) is unused; a default placeholder keeps the shared server type happy.
    let frozen = TraceParams {
        model: String::new(),
        concurrency: 0,
        prefixes: 0,
        max_tokens: 0,
        long_tokens: 0,
        long_frac: 0.0,
    };

    let bind = std::env::var("BROKER_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());
    let stateful = std::env::var("BROKER_STATEFUL").is_ok_and(|v| v == "1" || v == "true");
    // `StreamableHttpServerConfig` is `#[non_exhaustive]`, so build it from `Default` and assign
    // (a struct literal will not compile, `..Default::default()` included).
    let mut config = StreamableHttpServerConfig::default();
    config.stateful_mode = stateful;
    config.allowed_hosts = crucible_broker::auth::allowed_hosts(config.allowed_hosts);
    let degraded = std::env::var("BROKER_DEGRADED").is_ok_and(|v| v == "1" || v == "true");

    // One JIRA client shared across sessions. `None` (off / misconfigured) just means the
    // `jira_*` tools report `disabled`.
    let jira = crucible_broker::jira::from_env();

    // ONE deploy registry, shared across every per-request `McpServer` the http service builds: in
    // stateless mode the factory runs per request, so `deploy_composite` and the `deploy_status` that
    // polls it land on different server instances; they must share this state or the deploy vanishes.
    let deploys = std::sync::Arc::new(crucible_broker::deploy::DeployRegistry::new());
    // Same shared-across-per-request-instances rationale as `deploys`: the code-gen build/measure memo.
    let codegen = std::sync::Arc::new(crucible_broker::codegen::CodegenState::new());

    let service: StreamableHttpService<McpServer, LocalSessionManager> = StreamableHttpService::new(
        move || {
            Ok(McpServer::new(
                Box::new(NullResolver),
                frozen.clone(),
                degraded,
                jira.clone(),
                deploys.clone(),
                codegen.clone(),
            ))
        },
        Default::default(),
        config,
    );
    // Bearer guard (crucible mints BROKER_TOKEN and seeds it into the sandbox's `.mcp.json`):
    // without it, the 0.0.0.0 bind answers any pod that can route to this port.
    let token = std::sync::Arc::new(crucible_broker::auth::expected_token());
    if token.is_none() {
        eprintln!("warning: BROKER_TOKEN unset — the broker endpoint is unauthenticated");
    }
    let app = axum::Router::new().nest_service(MCP_PATH, service).layer(
        axum::middleware::from_fn_with_state(token, crucible_broker::auth::require_bearer),
    );

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding the crucible-broker to {bind}"))?;
    eprintln!("crucible-broker listening on http://{bind}{MCP_PATH}");
    // Graceful shutdown on SIGTERM/ctrl-c: main returns, the telemetry guard drops, and the final
    // span batch flushes before the pod dies.
    axum::serve(listener, app)
        .with_graceful_shutdown(crucible_broker::telemetry::shutdown_signal())
        .await
        .context("serving the crucible-broker")?;
    Ok(())
}
