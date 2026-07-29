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
use crucible_broker::NullResolver;
use crucible_broker::server::McpServer;
use crucible_broker::types::TraceParams;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

/// Where the broker listens. The sandbox reaches it via `host.containers.internal:8849` through the
/// openshell egress proxy; binding `0.0.0.0` puts it on the podman bridge rather than loopback only.
const DEFAULT_BIND: &str = "0.0.0.0:8849";
/// The MCP endpoint path. The sandbox's `.mcp.json` points at `http://host.containers.internal:8849/mcp`.
const MCP_PATH: &str = "/mcp";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    let config = StreamableHttpServerConfig {
        stateful_mode: stateful,
        ..Default::default()
    };
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
