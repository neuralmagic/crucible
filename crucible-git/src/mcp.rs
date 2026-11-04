//! The MCP endpoint: one tool, `open_pr`.

use crate::SCOPE_WRITE;
use crate::github::{GitHub, NewPr};
use axum::Router;
use crucible_mcp_auth::access::{AccessCheck, AccessRules, Call};
use crucible_mcp_auth::guard::PolicyGuard;
use crucible_mcp_auth::mount::{Mount, mcp_router, stateless_config};
use crucible_mcp_auth::policy::{Refusal, ToolPolicy};
use crucible_mcp_auth::principal::Principal;
use crucible_mcp_auth::server::ResourceServer;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::sync::Arc;

const MCP_PATH: &str = "/mcp";

/// The sandbox hostnames the compute drivers hand the agent.
const SANDBOX_HOSTS: [&str; 2] = ["host.containers.internal", "host.openshell.internal"];

struct GitRules;

impl AccessRules for GitRules {
    fn scopes(&self, call: &Call<'_>) -> Result<&'static [&'static str], Refusal> {
        match call {
            Call::Tool {
                name: "open_pr", ..
            } => Ok(&[SCOPE_WRITE]),
            _ => Err(call.unserved()),
        }
    }

    fn checks(&self, _: &Call<'_>, _: &Principal) -> Result<Vec<AccessCheck>, Refusal> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenPrParams {
    /// The repository to open it on, `owner/name`.
    pub repo: String,
    /// The branch it merges into.
    pub base: String,
    /// The branch with the changes, `branch` on `repo` or `owner:branch` on a fork.
    pub head: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Open it as a draft. Default true.
    #[serde(default = "yes")]
    pub draft: bool,
    /// Labels to add, created on the repository when missing.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Users to ask for review, and teams as `org/team`.
    #[serde(default)]
    pub reviewers: Vec<String>,
}

fn yes() -> bool {
    true
}

#[derive(Clone)]
pub struct GitMcp {
    gh: Arc<GitHub>,
}

#[tool_router]
impl GitMcp {
    #[tool(
        description = "Open a pull request (params: repo, base, head, title, body, draft, labels, \
        reviewers) as the run's GitHub App, a draft unless draft is false, or return the one \
        already open from head into base; either way add labels and ask reviewers (users, or \
        teams as org/team). Push head first through this server's /git/<owner>/<repo>.git. \
        Returns url, number, draft, existing, labels and reviewers."
    )]
    async fn open_pr(&self, Parameters(p): Parameters<OpenPrParams>) -> Result<String, String> {
        let owner = p.repo.split_once('/').map(|(o, _)| o).unwrap_or_default();
        if owner.is_empty() {
            return Err(format!("repo {:?} is not owner/name", p.repo));
        }
        let head = match p.head.contains(':') {
            true => p.head.clone(),
            false => format!("{owner}:{}", p.head),
        };
        let opened = self
            .gh
            .open_pr(&NewPr {
                repo: &p.repo,
                base: &p.base,
                head: &head,
                title: &p.title,
                body: &p.body,
                draft: p.draft,
                labels: &p.labels,
                reviewers: &p.reviewers,
            })
            .await
            .map_err(|e| format!("{e:#}"))?;
        serde_json::to_string(&opened).map_err(|e| e.to_string())
    }
}

#[tool_handler]
impl ServerHandler for GitMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("crucible-git", env!("CARGO_PKG_VERSION")),
        )
    }
}

/// The endpoint at [`MCP_PATH`], behind `authenticate`, accepting `hosts` beside loopback and the
/// sandbox hostnames.
pub fn router<L>(
    gh: Arc<GitHub>,
    hosts: &[String],
    authenticate: L,
    server: Option<Arc<ResourceServer>>,
) -> Router
where
    L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
    L::Service: tower::Service<axum::extract::Request> + Clone + Send + Sync + 'static,
    <L::Service as tower::Service<axum::extract::Request>>::Response:
        axum::response::IntoResponse + 'static,
    <L::Service as tower::Service<axum::extract::Request>>::Error:
        Into<std::convert::Infallible> + 'static,
    <L::Service as tower::Service<axum::extract::Request>>::Future: Send + 'static,
{
    let hosts: Vec<String> = SANDBOX_HOSTS
        .iter()
        .map(|h| h.to_string())
        .chain(hosts.iter().cloned())
        .collect();
    mcp_router(
        move || Ok(GitMcp { gh: gh.clone() }),
        authenticate,
        Mount {
            path: MCP_PATH,
            config: stateless_config(&hosts)
                .with_sse_keep_alive(None)
                .with_sse_retry(None),
            guard: PolicyGuard {
                rules: Arc::new(GitRules),
                policy: ToolPolicy::default(),
                server: server.clone(),
                reviewer: None,
            },
            metadata: server,
        },
    )
}
