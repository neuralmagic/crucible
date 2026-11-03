//! `build`, `status`, `run`, `push`, `logs` and `clean` against a running `buildit mcp`, over the
//! unix socket it serves its run on. Each command prints one JSON object on stdout and exits
//! nonzero when the call failed or what it reports did: a build that did not succeed, a run whose
//! command did not exit 0.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rmcp::RoleClient;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation, ProtocolVersion,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, UnixSocketHttpClient};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::{
    BuildParams, BuildReply, CleanParams, LogsParams, MCP_PATH, PushParams, RunParams, RunReply,
    StatusParams, StatusReply,
};
use crate::step::Status;

/// Names the socket of the server these commands call.
pub const ENV_SERVER: &str = "CRUCIBLE_BUILDIT_SOCKET";

/// One call to the server.
pub enum Call {
    /// Start a build, then poll its status until it ends unless `wait` is false.
    Build {
        params: BuildParams,
        wait: bool,
    },
    Status(StatusParams),
    Run(RunParams),
    Push(PushParams),
    Logs(LogsParams),
    Clean(CleanParams),
}

/// What a command prints, and whether it succeeded.
#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub reply: Value,
    pub ok: bool,
}

impl Outcome {
    pub fn ok(reply: Value) -> Self {
        Self { reply, ok: true }
    }

    fn failed(reply: Value) -> Self {
        Self { reply, ok: false }
    }

    /// A failure that never reached the server, or whose call broke on the way.
    pub fn error(e: &anyhow::Error) -> Self {
        Self::failed(serde_json::json!({ "error": format!("{e:#}") }))
    }

    /// Prints the reply as one line of JSON and returns the exit code: 0 when it succeeded.
    pub fn print(&self) -> ExitCode {
        println!("{}", self.reply);
        match self.ok {
            true => ExitCode::SUCCESS,
            false => ExitCode::FAILURE,
        }
    }
}

/// A tool's reply object, or what the server sent instead when the call failed.
enum Reply {
    Ok(Value),
    Failed(Value),
}

/// An MCP session with a `buildit mcp` over its unix socket.
pub struct Server {
    service: RunningService<RoleClient, ClientConfig>,
}

impl Server {
    pub async fn connect(socket: &Path) -> Result<Self> {
        let path = std::path::absolute(socket)
            .with_context(|| format!("resolving {}", socket.display()))?;
        let path = path
            .to_str()
            .ok_or_else(|| anyhow!("socket path {} is not utf-8", socket.display()))?;
        let uri = format!("http://localhost{MCP_PATH}");
        let transport = StreamableHttpClientTransport::with_client(
            UnixSocketHttpClient::new(path, &uri),
            StreamableHttpClientTransportConfig::with_uri(uri),
        );
        let service = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("buildit", env!("CARGO_PKG_VERSION")),
        )
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .with_context(|| format!("connecting to buildit at {path}"))?;
        Ok(Self { service })
    }

    async fn call(&self, tool: &str, params: &impl Serialize) -> Result<Reply> {
        let Value::Object(mut args) = serde_json::to_value(params).context("encoding the call")?
        else {
            bail!("{tool}'s arguments are not an object");
        };
        args.retain(|_, v| !v.is_null());
        let request = CallToolRequestParams::new(tool.to_string()).with_arguments(args);
        let result = self
            .service
            .call_tool(request)
            .await
            .with_context(|| format!("calling {tool}"))?;
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .ok_or_else(|| anyhow!("{tool} replied without text"))?;
        let object = serde_json::from_str::<Value>(&text)
            .ok()
            .filter(Value::is_object);
        match (result.is_error.unwrap_or(false), object) {
            (false, Some(reply)) => Ok(Reply::Ok(reply)),
            (false, None) => {
                bail!("{tool} replied with something other than a JSON object: {text}")
            }
            (true, Some(refusal)) => Ok(Reply::Failed(refusal)),
            (true, None) => Ok(Reply::Failed(serde_json::json!({ "error": text }))),
        }
    }

    async fn close(self) {
        if let Err(e) = self.service.cancel().await {
            tracing::debug!("closing the session: {e:#}");
        }
    }
}

/// Makes `call` against the server on `socket`. Every failure, a connection that never opened
/// included, comes back as an [`Outcome`] whose reply is an object with an `error`.
pub async fn run(socket: &Path, call: Call) -> Outcome {
    let server = match Server::connect(socket).await {
        Ok(server) => server,
        Err(e) => return Outcome::error(&e),
    };
    let outcome = match dispatch(&server, call).await {
        Ok(outcome) => outcome,
        Err(e) => Outcome::error(&e),
    };
    server.close().await;
    outcome
}

async fn dispatch(server: &Server, call: Call) -> Result<Outcome> {
    let reply = match call {
        Call::Build { params, wait } => return build(server, &params, wait).await,
        Call::Run(params) => {
            return Ok(match server.call("run", &params).await? {
                Reply::Ok(reply) => {
                    let ran = RunReply::deserialize(&reply).context("reading run's reply")?;
                    Outcome {
                        ok: ran.exit == Some(0) && !ran.timed_out,
                        reply,
                    }
                }
                Reply::Failed(reply) => Outcome::failed(reply),
            });
        }
        Call::Status(params) => server.call("status", &params).await?,
        Call::Push(params) => server.call("push", &params).await?,
        Call::Logs(params) => server.call("logs", &params).await?,
        Call::Clean(params) => server.call("clean", &params).await?,
    };
    Ok(match reply {
        Reply::Ok(reply) => Outcome::ok(reply),
        Reply::Failed(reply) => Outcome::failed(reply),
    })
}

async fn build(server: &Server, params: &BuildParams, wait: bool) -> Result<Outcome> {
    let started = match server.call("build", params).await? {
        Reply::Ok(reply) => reply,
        Reply::Failed(reply) => return Ok(Outcome::failed(reply)),
    };
    let built = BuildReply::deserialize(&started).context("reading build's reply")?;
    if !wait {
        return Ok(Outcome::ok(started));
    }
    let poll = Duration::from_millis(crate::task::POLL_INTERVAL_MS);
    let status = StatusParams {
        build_id: built.build_id,
    };
    loop {
        let reply = match server.call("status", &status).await? {
            Reply::Ok(reply) => reply,
            Reply::Failed(reply) => return Ok(Outcome::failed(reply)),
        };
        let polled = StatusReply::deserialize(&reply).context("reading status's reply")?;
        if polled.status != Status::Running {
            return Ok(Outcome {
                ok: polled.status == Status::Succeeded,
                reply,
            });
        }
        tokio::time::sleep(poll).await;
    }
}
