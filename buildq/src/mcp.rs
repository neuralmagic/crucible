//! The queue's MCP tools, over stdio.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::queue::{BuildRequest, Queue};

/// The longest a `status` call waits.
const MAX_WAIT_S: u64 = 900;

#[derive(Clone)]
pub struct Buildq {
    queue: Arc<Queue>,
    workdir: PathBuf,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BuildParams {
    /// The build context, a directory relative to the server's workdir
    pub context: String,
    /// Dockerfile path inside the context (default "Dockerfile")
    pub dockerfile: Option<String>,
    /// Build args, NAME -> value
    #[serde(default)]
    pub build_args: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StatusParams {
    pub build_id: String,
    /// Seconds to wait for the build to end before answering (max 900; default 0, answer now)
    pub wait_s: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LogsParams {
    pub build_id: String,
    /// Lines from the end of the build's log (default 100)
    pub tail_lines: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BuildIdParams {
    pub build_id: String,
}

#[tool_router]
impl Buildq {
    pub fn new(queue: Arc<Queue>, workdir: PathBuf) -> Self {
        Self { queue, workdir }
    }

    #[tool(
        description = "Queue a Dockerfile build (params: context, dockerfile, build_args). Returns \
        build_id at once with the build queued. Call `status` with wait_s to wait for it to end; \
        a succeeded build reports its pushed image by digest."
    )]
    async fn build(&self, Parameters(p): Parameters<BuildParams>) -> Result<String, String> {
        let req = BuildRequest {
            context: self.workdir.join(&p.context),
            dockerfile: p.dockerfile.unwrap_or_else(|| "Dockerfile".to_string()),
            build_args: p.build_args,
        };
        let id = self
            .queue
            .submit(&req)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let status = self
            .queue
            .status(&id, Some(Duration::from_secs(5)))
            .await
            .map_err(|e| format!("{e:#}"))?;
        serde_json::to_string(&status).map_err(|e| e.to_string())
    }

    #[tool(
        description = "A build's state (params: build_id, wait_s): queued, running, succeeded with \
        its image, or failed with the reason. With wait_s it answers when the build ends or \
        wait_s passes, so one call replaces polling."
    )]
    async fn status(&self, Parameters(p): Parameters<StatusParams>) -> Result<String, String> {
        let wait = p
            .wait_s
            .filter(|s| *s > 0)
            .map(|s| Duration::from_secs(s.min(MAX_WAIT_S)));
        let status = self
            .queue
            .status(&p.build_id, wait)
            .await
            .map_err(|e| format!("{e:#}"))?;
        serde_json::to_string(&status).map_err(|e| e.to_string())
    }

    #[tool(description = "The end of a build's log (params: build_id, tail_lines).")]
    async fn logs(&self, Parameters(p): Parameters<LogsParams>) -> Result<String, String> {
        self.queue
            .logs(&p.build_id, p.tail_lines.unwrap_or(100))
            .await
            .map_err(|e| format!("{e:#}"))
    }

    #[tool(description = "Cancel a build (params: build_id); its Job and pod are deleted.")]
    async fn cancel(&self, Parameters(p): Parameters<BuildIdParams>) -> Result<String, String> {
        self.queue
            .cancel(&p.build_id)
            .await
            .map(|()| format!("cancelled {}", p.build_id))
            .map_err(|e| format!("{e:#}"))
    }
}

#[tool_handler]
impl ServerHandler for Buildq {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("buildq", env!("CARGO_PKG_VERSION")))
    }
}
