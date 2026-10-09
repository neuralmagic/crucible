#![allow(clippy::disallowed_macros)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use clap::Args;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::{DeleteParams, ListParams};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResponse, CallToolResult, CancelTaskParams, ContentBlock, CreateTaskResult,
    DetailedTask, GetTaskParams, GetTaskResult, Implementation, ServerCapabilities, ServerConfig,
    Task, TaskPayload, TaskStatus,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, PodOpts, Resources};
use crate::logs::{LOG_DIR, Log, LogName, MAX_READ, Page, Query};
use crate::pod::BuilderPod;
use crate::sandbox::{RESULTS_DIR, RelPath, SandboxDir, SandboxName, Workspace, is_label_value};
use crate::step::{self, Fetch, Published, Snapshot, Status};
use crate::task::{self, Progress, TaskRef};
use crucible_mcp_auth::access::{AccessCheck, AccessRules, Call};
use crucible_mcp_auth::guard::PolicyGuard;
use crucible_mcp_auth::policy::Refusal;
use crucible_mcp_auth::policy::ToolPolicy;
use crucible_mcp_auth::principal::{Callers, Principal};
use crucible_mcp_auth::server::{
    AuthError, JwksTls, ResourceServer, ResourceServerConfig, TrustedIssuer,
};

pub const MCP_PATH: &str = "/mcp";

const SANDBOX_HOSTS: [&str; 2] = ["host.containers.internal", "host.openshell.internal"];

const LABEL_MANAGED_BY: &str = "buildit.dev/managed-by";
const MANAGED_BY: &str = "mcp";
const LABEL_SANDBOX: &str = "buildit.dev/sandbox";
const LABEL_BUILD_ID: &str = "buildit.dev/build-id";
/// The person a realm token acted for, on every builder pod it starts.
const LABEL_LAUNCHED_BY: &str = "crucible.dev/launched-by";

/// Scope that lets a token read build status and logs.
pub const SCOPE_READ: &str = "buildit.read";
/// Scope that lets a token build, run, and clean.
pub const SCOPE_BUILD: &str = "buildit.build";

const POD_READY: Duration = Duration::from_secs(180);
const DEFAULT_BUILD_TIMEOUT_S: u64 = 1800;
const DEFAULT_RUN_TIMEOUT_S: u64 = 600;
const FROM_TIMEOUT: Duration = Duration::from_secs(300);
const EXEC_SLACK: Duration = Duration::from_secs(60);
const SETTLE_RETRY: Duration = Duration::from_secs(2);
const SUMMARY_LINES: usize = 40;
const SUMMARY_BYTES: usize = 8 * 1024;
const SUMMARY_WINDOW: u64 = 256 * 1024;
const DEADLINE_MARGIN: Duration = Duration::from_secs(10);
const DEADLINE_EXCEEDED: &str = "DeadlineExceeded";
const DEFAULT_LOG_BYTES: u64 = 16 * 1024;
const MAX_LOG_BYTES: u64 = 64 * 1024;

const ALLOC_SCRIPT: &str = r#"set -C; n=1; until true > "$1/run-$n.log"; do n=$((n + 1)); [ "$n" -le 9999 ] || exit 1; done 2>/dev/null; echo "$n""#;

#[derive(Args)]
pub struct McpArgs {
    /// Listen address for the streamable-http endpoint
    #[arg(long, env = "MCP_BIND", default_value = "0.0.0.0:8849")]
    pub bind: String,
    /// File of `<token> <sandbox> [<workdir>]` lines, re-read on every request
    #[arg(long, env = "MCP_TOKENS_FILE", required_unless_present = "run_issuers")]
    pub tokens_file: Option<PathBuf>,
    /// Accept runs' access tokens from an OAuth issuer (the crucible controller), as
    /// `<issuer>=<jwks uri>`, alongside the tokens file. Repeatable
    #[arg(
        long = "run-issuer",
        env = "MCP_RUN_ISSUERS",
        value_delimiter = ',',
        value_parser = TrustedIssuer::runs,
        requires = "resources"
    )]
    pub run_issuers: Vec<TrustedIssuer>,
    /// This server's canonical MCP URLs; an access token's aud must name one
    #[arg(
        long = "resource",
        env = "MCP_RESOURCES",
        value_delimiter = ',',
        requires = "run_issuers"
    )]
    pub resources: Vec<url::Url>,
    /// PEM bundle of CAs to trust for issuers' JWKS hosts, beside the platform roots
    #[arg(long, env = "MCP_JWKS_CA")]
    pub jwks_ca: Option<PathBuf>,
    /// Allow plain-http JWKS URIs on non-loopback hosts. Only for a cluster network you trust
    #[arg(long, env = "MCP_INSECURE_JWKS_HTTP")]
    pub insecure_jwks_http: bool,
    /// TOML tool policy: per-method and per-tool scopes, CEL validate and mutate rules. Required
    /// with --run-issuer; without one, the tokens file decides alone
    #[arg(long, env = "MCP_TOOL_POLICY")]
    pub tool_policy: Option<PathBuf>,
    /// Sandbox workdir for token lines that name none
    #[arg(long, env = "BROKER_SANDBOX_WORKDIR")]
    pub sandbox_workdir: Option<String>,
    /// Host header values accepted besides loopback and the sandbox gateway names
    #[arg(
        long = "allowed-host",
        env = "BROKER_ALLOWED_HOSTS",
        value_delimiter = ','
    )]
    pub allowed_hosts: Vec<String>,
    /// Run off-cluster against this kubeconfig context
    #[arg(long)]
    pub kubecontext: Option<String>,
    /// Namespace for builder pods (default: the client's)
    #[arg(short, long)]
    pub namespace: Option<String>,
    /// Builder pod lifetime in seconds (activeDeadlineSeconds)
    #[arg(long, default_value_t = 7200)]
    pub pod_deadline: i64,
    /// Live builder pods per sandbox; `build` is refused past this
    #[arg(long, default_value_t = 4)]
    pub max_builds: usize,
    /// Schedule builder pods only on nodes of this architecture (kubernetes.io/arch)
    #[arg(long)]
    pub node_arch: Option<String>,
    /// Resource requests for builder pods, repeatable: --request cpu=2
    #[arg(long = "request", value_name = "KEY=QTY", value_parser = crate::parse_kv)]
    pub requests: Vec<(String, String)>,
    /// Resource limits for builder pods, repeatable: --limit memory=8Gi
    #[arg(long = "limit", value_name = "KEY=QTY", value_parser = crate::parse_kv)]
    pub limits: Vec<(String, String)>,
}

pub async fn serve(client: kube::Client, args: McpArgs) -> Result<()> {
    let namespace = args
        .namespace
        .unwrap_or_else(|| client.default_namespace().to_string());
    let oidc = match args.run_issuers.is_empty() {
        true => None,
        false => {
            let config = ResourceServerConfig {
                issuers: args.run_issuers,
                resources: args.resources,
                required_scopes: vec![SCOPE_READ.to_string()],
                scopes_supported: vec![SCOPE_READ.to_string(), SCOPE_BUILD.to_string()],
                resource_name: Some("buildit".to_string()),
            };
            let tls = JwksTls {
                ca_bundle: args.jwks_ca,
                insecure_http: args.insecure_jwks_http,
            };
            Some(Arc::new(
                ResourceServer::new(config, &tls).context("building the resource server")?,
            ))
        }
    };
    let policy = match &args.tool_policy {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading tool policy {}", path.display()))?;
            ToolPolicy::from_toml(&text)
                .with_context(|| format!("loading tool policy {}", path.display()))?
        }
        None => ToolPolicy::default(),
    };
    let broker = Arc::new(Broker {
        client,
        namespace,
        tokens_file: args.tokens_file,
        oidc,
        default_workdir: args
            .sandbox_workdir
            .as_deref()
            .map(SandboxDir::parse)
            .transpose()?,
        local_workdir: None,
        resources: Resources {
            requests: args.requests,
            limits: args.limits,
        },
        deadline_secs: args.pod_deadline,
        max_builds: args.max_builds,
        node_arch: args.node_arch,
    });
    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    tracing::info!(
        "buildit mcp listening on http://{}{MCP_PATH} (namespace {})",
        args.bind,
        broker.namespace
    );
    axum::serve(listener, router(broker, &args.allowed_hosts, policy))
        .await
        .context("serving buildit mcp")
}

/// The MCP endpoint behind authentication and the guard: [`BuilditRules`] decide which calls
/// exist and their scopes, and `policy` adds validate and mutate rules.
pub fn router(broker: Arc<Broker>, extra_hosts: &[String], policy: ToolPolicy) -> axum::Router {
    let mut hosts = StreamableHttpServerConfig::default().allowed_hosts;
    hosts.extend(SANDBOX_HOSTS.map(String::from));
    hosts.extend(extra_hosts.iter().map(|h| h.trim().to_string()));
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_sse_retry(None)
        .with_allowed_hosts(hosts);
    let served = broker.clone();
    let service: StreamableHttpService<BuilditMcp, NeverSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(BuilditMcp {
                    broker: served.clone(),
                })
            },
            Arc::new(NeverSessionManager::default()),
            config,
        );
    let oidc = broker.oidc.clone();
    let guard = PolicyGuard {
        policy,
        rules: Arc::new(BuilditRules),
        server: oidc.clone(),
        reviewer: None,
    };
    let mcp = axum::Router::new()
        .fallback_service(service)
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(guard),
            crucible_mcp_auth::guard::enforce,
        ))
        .layer(axum::middleware::from_fn_with_state(broker, authenticate));
    let app = axum::Router::new().nest_service(MCP_PATH, mcp);
    match oidc {
        Some(server) => app.merge(crucible_mcp_auth::metadata::router(server)),
        None => app,
    }
}

async fn authenticate(State(broker): State<Arc<Broker>>, mut req: Request, next: Next) -> Response {
    match broker.authenticate(req.headers()).await {
        Ok((caller, principal)) => {
            req.extensions_mut().insert(caller);
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        Err(refused) => broker.respond(req.headers(), refused),
    }
}

fn is_jwt(token: &str) -> bool {
    token.split('.').count() == 3
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|t| !t.is_empty())
}

#[derive(Debug, PartialEq, Eq)]
struct TokenLine {
    token: String,
    sandbox: SandboxName,
    workdir: Option<SandboxDir>,
}

fn parse_tokens(text: &str) -> Result<Vec<TokenLine>> {
    let mut lines = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let fields: Vec<&str> = raw.split_whitespace().collect();
        let (token, sandbox, workdir) = match fields[..] {
            [] => continue,
            [token, sandbox] => (token, sandbox, None),
            [token, sandbox, workdir] => (token, sandbox, Some(workdir)),
            _ => bail!(
                "token file line {} is not `<token> <sandbox> [<workdir>]`",
                i + 1
            ),
        };
        lines.push(TokenLine {
            token: token.to_string(),
            sandbox: SandboxName::parse(sandbox)?,
            workdir: workdir.map(SandboxDir::parse).transpose()?,
        });
    }
    Ok(lines)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildId(String);

impl BuildId {
    fn fresh() -> Self {
        Self(crate::pod::unique_name())
    }

    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let ok = raw.len() <= 63
            && raw.starts_with(|c: char| c.is_ascii_alphanumeric())
            && raw.ends_with(|c: char| c.is_ascii_alphanumeric())
            && raw
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok {
            bail!("invalid build_id {raw:?}");
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn tag(&self) -> String {
        format!("localhost/buildit/{}:latest", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub sandbox: SandboxName,
    pub workspace: Workspace,
    /// The person a realm token acts for; tokens-file callers carry none.
    pub launched_by: Option<Launcher>,
}

/// A realm token's `sub`, kept as a label value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher(String);

impl Launcher {
    pub fn parse(raw: &str) -> Option<Self> {
        is_label_value(raw).then(|| Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a request was not authenticated.
#[derive(Debug)]
pub enum Refused {
    /// A realm token failed validation, or no token matched while OIDC is on.
    Token(AuthError),
    /// A valid realm token that names no usable caller; why is logged, not returned.
    Claims,
    /// The tokens-file path's own status.
    Status(StatusCode),
}

#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error("token names no sandbox: claim sandbox is missing or not a string")]
    NoSandbox,
    #[error("token claim sandbox {0:?} is not a sandbox name")]
    Sandbox(String),
    #[error("token claim workdir {0:?} is not an absolute, normal path")]
    Workdir(String),
    #[error("token subject {0:?} is not a label value")]
    Subject(String),
    #[error("sandbox {0} has no workdir and BROKER_SANDBOX_WORKDIR is unset")]
    NoWorkdir(String),
    #[error("buildit serves runs only, not people's tokens")]
    Person,
}

impl Caller {
    /// The principal a tokens-file caller stands for, claims shaped like a realm token's so one
    /// tool policy covers both.
    pub fn token_file_principal(&self) -> Principal {
        let subject = format!("sandbox:{}", self.sandbox.as_str());
        let scope = format!("{SCOPE_READ} {SCOPE_BUILD}");
        let claims = serde_json::json!({
            "sub": subject,
            "sandbox": self.sandbox.as_str(),
            "scope": scope,
        });
        Principal {
            subject,
            actor_chain: Vec::new(),
            client_id: None,
            namespace: None,
            scopes: [SCOPE_READ, SCOPE_BUILD].map(String::from).into(),
            claims: match claims {
                serde_json::Value::Object(m) => m,
                _ => serde_json::Map::new(),
            },
            callers: Callers::Runs,
        }
    }
}

pub struct Broker {
    client: kube::Client,
    namespace: String,
    tokens_file: Option<PathBuf>,
    oidc: Option<Arc<ResourceServer>>,
    default_workdir: Option<SandboxDir>,
    local_workdir: Option<PathBuf>,
    resources: Resources,
    deadline_secs: i64,
    max_builds: usize,
    node_arch: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BuildParams {
    /// Build context: a subdirectory of the workspace, e.g. "svc/api"
    pub context: String,
    /// Dockerfile path relative to the context (default "Dockerfile")
    pub dockerfile: Option<String>,
    /// Multi-stage target to stop at
    #[serde(alias = "build_target")]
    pub target: Option<String>,
    /// Build args, NAME -> value
    #[serde(default)]
    pub build_args: BTreeMap<String, String>,
    /// Build timeout in seconds (default 1800)
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StatusParams {
    /// build_id from build
    pub build_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunParams {
    /// build_id of a build whose status is succeeded
    pub build_id: String,
    /// argv to run in a fresh container of the built image; use ["sh", "-c", "..."] for a shell
    pub cmd: Vec<String>,
    /// Run timeout in seconds (default 600)
    pub timeout_s: Option<u64>,
    /// Container paths to copy back into .buildit/<build_id>/<path> after the run
    #[serde(default)]
    pub fetch_paths: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LogsParams {
    /// build_id from build
    pub build_id: String,
    /// "build" (default) or "run:<n>"
    pub which: Option<String>,
    /// Regex (Rust syntax); returns matches as `N:text`, context as `N-text`, gaps as `--`
    pub grep: Option<String>,
    /// Lines of context around each grep match (max 20)
    pub context: Option<u64>,
    /// First line to return (1-based)
    pub from_line: Option<u64>,
    /// Last line to return
    pub to_line: Option<u64>,
    /// Without grep or a line range: the last N lines (default 100)
    pub tail_lines: Option<u64>,
    /// Output cap in bytes (default 16384, max 65536)
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CleanParams {
    /// Build to delete; omit to delete every build of this sandbox
    pub build_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PodView {
    Live,
    Ended(step::State),
}

impl PodView {
    // the build's pod, unless it is gone or being deleted
    fn of(pods: &[Pod]) -> Option<Self> {
        let pod = pods
            .iter()
            .find(|p| p.metadata.deletion_timestamp.is_none())?;
        if alive(pod) {
            return Some(Self::Live);
        }
        let status = pod.status.as_ref();
        let record = status
            .and_then(|s| s.container_statuses.as_deref())
            .unwrap_or_default()
            .iter()
            .filter_map(|c| c.state.as_ref()?.terminated.as_ref()?.message.as_deref())
            .find_map(step::State::record);
        Some(Self::Ended(match record {
            Some(state) => state,
            None if status.and_then(|s| s.reason.as_deref()) == Some(DEADLINE_EXCEEDED) => {
                step::State::Expired
            }
            None => step::State::Lost,
        }))
    }
}

enum Found {
    Live(BuilderPod),
    Ended {
        pod: BuilderPod,
        state: step::State,
        created: String,
    },
}

#[derive(Debug, Serialize)]
pub struct BuildReply {
    pub build_id: String,
    pub status: Status,
}

#[derive(Debug, Serialize)]
pub struct StatusReply {
    pub build_id: String,
    pub status: Status,
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub log_tail: String,
}

#[derive(Debug, Serialize)]
pub struct RunReply {
    pub log: String,
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub log_tail: String,
    pub fetched: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CleanReply {
    pub deleted: Vec<String>,
}

fn argv<const N: usize>(parts: [&str; N]) -> Vec<String> {
    parts.map(String::from).to_vec()
}

fn timeout(requested: Option<u64>, default: u64) -> Duration {
    Duration::from_secs(requested.unwrap_or(default).clamp(1, 7200))
}

fn build_limit(requested: Duration, deadline_secs: i64, used: Duration) -> Result<Duration> {
    let deadline = Duration::from_secs(u64::try_from(deadline_secs).unwrap_or(0));
    let left = deadline
        .saturating_sub(used)
        .saturating_sub(DEADLINE_MARGIN);
    if left < Duration::from_secs(1) {
        bail!(
            "the {deadline_secs}s pod deadline leaves no time to build after {}s of setup",
            used.as_secs()
        );
    }
    Ok(requested.min(left))
}

fn alive(pod: &Pod) -> bool {
    pod.metadata.deletion_timestamp.is_none()
        && !matches!(
            pod.status.as_ref().and_then(|s| s.phase.as_deref()),
            Some("Succeeded" | "Failed")
        )
}

fn bud_argv(
    id: &BuildId,
    dockerfile: &RelPath,
    target: Option<&str>,
    build_args: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let ws = Backend::Buildah.workspace();
    let mut out = argv(["buildah", "bud"]);
    if let Some(t) = target {
        out.extend(["--target".to_string(), t.to_string()]);
    }
    for (k, v) in build_args {
        if k.is_empty() || k.contains('=') {
            bail!("invalid build arg name {k:?}");
        }
        out.extend(["--build-arg".to_string(), format!("{k}={v}")]);
    }
    out.extend([
        "-f".to_string(),
        format!("{ws}/{}", dockerfile.as_str()),
        "-t".to_string(),
        id.tag(),
        ws.to_string(),
    ]);
    Ok(out)
}

fn summary(log: &Log) -> String {
    let tail = Query::Tail {
        lines: SUMMARY_LINES,
    };
    log.query(&tail, SUMMARY_BYTES).lines
}

async fn log_tail(pod: &BuilderPod, log: LogName) -> Result<String> {
    Ok(summary(&Log::fetch(pod, log, SUMMARY_WINDOW).await?))
}

async fn run_reply(
    pod: &BuilderPod,
    id: &BuildId,
    log: LogName,
    state: step::State,
    fetched: &[String],
) -> Result<RunReply> {
    let (exit, timed_out) = match state {
        step::State::Running => (None, true),
        state => (state.exit(), state.timed_out()),
    };
    Ok(RunReply {
        log: log.to_string(),
        exit,
        timed_out,
        log_tail: log_tail(pod, log).await?,
        fetched: fetched
            .iter()
            .map(|f| format!("{RESULTS_DIR}/{}/{f}", id.0))
            .collect(),
    })
}

fn usize_of(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

impl LogsParams {
    fn query(&self) -> Result<Query> {
        match (&self.grep, self.from_line, self.to_line, self.tail_lines) {
            (Some(p), None, None, None) => Query::grep(p, usize_of(self.context.unwrap_or(0))),
            _ if self.context.is_some() => bail!("context needs grep"),
            (None, None, None, n) => Ok(Query::Tail {
                lines: usize_of(n.unwrap_or(100)),
            }),
            (None, from, to, None) => Ok(Query::Range {
                from: usize_of(from.unwrap_or(1)),
                to: to.map(usize_of),
            }),
            _ => bail!("grep, from_line/to_line and tail_lines are mutually exclusive"),
        }
    }
}

// regular files and directories only; anything else, or outside dest, is dropped
fn unpack(tar: &[u8], dest: &Path) -> Result<Vec<String>> {
    use tar::EntryType;
    let mut files = Vec::new();
    for entry in tar::Archive::new(tar)
        .entries()
        .context("reading fetched tar")?
    {
        let mut entry = entry.context("reading fetched tar entry")?;
        let Ok(rel) = RelPath::parse(&entry.path()?.to_string_lossy()) else {
            continue;
        };
        match entry.header().entry_type() {
            EntryType::Regular | EntryType::Continuous => {
                if entry.unpack_in(dest)? {
                    files.push(rel.as_str().to_string());
                }
            }
            EntryType::Directory => std::fs::create_dir_all(dest.join(rel.as_str()))?,
            _ => continue,
        }
    }
    Ok(files)
}

impl Broker {
    /// A realm access token when OIDC is on and the bearer is a JWT, otherwise a tokens-file
    /// token. A JWT never falls back to the tokens file.
    async fn authenticate(&self, headers: &HeaderMap) -> Result<(Caller, Principal), Refused> {
        if let (Some(oidc), Some(token)) = (&self.oidc, bearer(headers))
            && is_jwt(token)
        {
            let principal = oidc.authenticate(headers).await.map_err(|error| {
                tracing::info!(auth = "oidc", %error, "token refused");
                Refused::Token(error)
            })?;
            let caller = self.realm_caller(&principal).map_err(|error| {
                tracing::info!(auth = "oidc", sub = %principal.subject, %error, "token refused");
                Refused::Claims
            })?;
            tracing::info!(auth = "oidc", sub = %principal.subject, presenter = principal.presenter(), sandbox = caller.sandbox.as_str(), "caller authenticated");
            return Ok((caller, principal));
        }
        match self.caller(headers).await {
            Ok(caller) => {
                tracing::debug!(
                    auth = "token-file",
                    sandbox = caller.sandbox.as_str(),
                    "caller authenticated"
                );
                let principal = caller.token_file_principal();
                Ok((caller, principal))
            }
            Err(StatusCode::UNAUTHORIZED) if self.oidc.is_some() => {
                Err(Refused::Token(match bearer(headers) {
                    None => AuthError::Missing,
                    Some(_) => AuthError::Unrecognized,
                }))
            }
            Err(status) => Err(Refused::Status(status)),
        }
    }

    fn respond(&self, headers: &HeaderMap, refused: Refused) -> Response {
        match (refused, &self.oidc) {
            (Refused::Token(error), Some(oidc)) => oidc.challenge(headers, &error),
            (Refused::Token(_), None) => StatusCode::UNAUTHORIZED.into_response(),
            (Refused::Claims, _) => {
                let body = serde_json::json!({
                    "error": "insufficient_scope",
                    "error_description": "the access token does not name a sandbox this server serves",
                });
                (StatusCode::FORBIDDEN, axum::Json(body)).into_response()
            }
            (Refused::Status(status), _) => status.into_response(),
        }
    }

    /// The caller a realm token names: its `sandbox` claim, optional `workdir`, and `sub`.
    fn realm_caller(&self, principal: &Principal) -> Result<Caller, ClaimError> {
        if principal.callers != Callers::Runs {
            return Err(ClaimError::Person);
        }
        let raw = principal
            .claims
            .get("sandbox")
            .and_then(serde_json::Value::as_str)
            .ok_or(ClaimError::NoSandbox)?;
        let sandbox = SandboxName::parse(raw).map_err(|_| ClaimError::Sandbox(raw.to_string()))?;
        let workdir = match principal
            .claims
            .get("workdir")
            .and_then(serde_json::Value::as_str)
        {
            Some(w) => Some(SandboxDir::parse(w).map_err(|_| ClaimError::Workdir(w.to_string()))?),
            None => None,
        };
        let workspace = self
            .workspace(workdir)
            .ok_or_else(|| ClaimError::NoWorkdir(sandbox.as_str().to_string()))?;
        let launched_by = Launcher::parse(&principal.subject)
            .ok_or_else(|| ClaimError::Subject(principal.subject.clone()))?;
        Ok(Caller {
            sandbox,
            workspace,
            launched_by: Some(launched_by),
        })
    }

    fn workspace(&self, workdir: Option<SandboxDir>) -> Option<Workspace> {
        match (
            &self.local_workdir,
            workdir.or(self.default_workdir.clone()),
        ) {
            (Some(dir), _) => Some(Workspace::Local { dir: dir.clone() }),
            (None, Some(workdir)) => Some(Workspace::Openshell { workdir }),
            (None, None) => None,
        }
    }

    async fn caller(&self, headers: &HeaderMap) -> Result<Caller, StatusCode> {
        let token = bearer(headers).ok_or(StatusCode::UNAUTHORIZED)?;
        let tokens_file = self.tokens_file.as_ref().ok_or(StatusCode::UNAUTHORIZED)?;
        let lines = tokio::fs::read_to_string(tokens_file)
            .await
            .context("reading the token file")
            .and_then(|text| parse_tokens(&text))
            .map_err(|e| {
                tracing::error!("{}: {e:#}", tokens_file.display());
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        let line = lines
            .into_iter()
            .fold(None, |found, l| {
                if constant_time_eq(l.token.as_bytes(), token.as_bytes()) {
                    Some(l)
                } else {
                    found
                }
            })
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let Some(workspace) = self.workspace(line.workdir) else {
            tracing::error!(
                "sandbox {} has no workdir and BROKER_SANDBOX_WORKDIR is unset",
                line.sandbox.as_str()
            );
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        };
        Ok(Caller {
            sandbox: line.sandbox,
            workspace,
            launched_by: None,
        })
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    async fn list(&self, sandbox: &SandboxName, id: Option<&BuildId>) -> Result<Vec<Pod>> {
        let mut sel = format!(
            "{LABEL_MANAGED_BY}={MANAGED_BY},{LABEL_SANDBOX}={}",
            sandbox.as_str()
        );
        if let Some(id) = id {
            sel.push_str(&format!(",{LABEL_BUILD_ID}={}", id.0));
        }
        Ok(self
            .pods()
            .list(&ListParams::default().labels(&sel))
            .await
            .with_context(|| format!("listing builder pods ({sel})"))?
            .items)
    }

    // None when this sandbox has no such build
    async fn found(&self, sandbox: &SandboxName, id: &BuildId) -> Result<Option<Found>> {
        let pods = self.list(sandbox, Some(id)).await?;
        let Some(view) = PodView::of(&pods) else {
            return Ok(None);
        };
        let pod = BuilderPod::existing(self.client.clone(), &self.namespace, &id.0);
        Ok(Some(match view {
            PodView::Live => Found::Live(pod),
            PodView::Ended(state) => Found::Ended {
                pod,
                state,
                created: pods
                    .iter()
                    .find_map(|p| p.metadata.creation_timestamp.as_ref())
                    .map(|t| t.0.to_string())
                    .unwrap_or_default(),
            },
        }))
    }

    async fn lookup(&self, sandbox: &SandboxName, id: &BuildId) -> Result<Found> {
        self.found(sandbox, id)
            .await?
            .ok_or_else(|| anyhow!("no live build {}; build again", id.0))
    }

    async fn find(&self, sandbox: &SandboxName, id: &BuildId) -> Result<BuilderPod> {
        match self.lookup(sandbox, id).await? {
            Found::Live(pod) => Ok(pod),
            Found::Ended { .. } => bail!("build {}'s pod has ended; build again", id.0),
        }
    }

    fn builder_pod(&self, caller: &Caller, id: &BuildId) -> Result<Pod> {
        let opts = PodOpts {
            idle_nodes: &[],
            resources: &self.resources,
            cache: None,
            node: None,
        };
        let mut pod = Backend::Buildah.pod_spec(&id.0, &self.namespace, &opts)?;
        let labels = pod.metadata.labels.get_or_insert_default();
        labels.extend(
            [
                (LABEL_MANAGED_BY, MANAGED_BY),
                (LABEL_SANDBOX, caller.sandbox.as_str()),
                (LABEL_BUILD_ID, &id.0),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        if let Some(launcher) = &caller.launched_by {
            labels.insert(LABEL_LAUNCHED_BY.to_string(), launcher.as_str().to_string());
        }
        let spec = pod.spec.get_or_insert_default();
        spec.active_deadline_seconds = Some(self.deadline_secs);
        spec.automount_service_account_token = Some(false);
        spec.enable_service_links = Some(false);
        if let Some(arch) = &self.node_arch {
            spec.node_selector
                .get_or_insert_default()
                .insert("kubernetes.io/arch".to_string(), arch.clone());
        }
        for c in &mut spec.containers {
            c.command = Some(argv(["sleep", &self.deadline_secs.to_string()]));
        }
        Ok(pod)
    }

    async fn start_build(
        &self,
        caller: &Caller,
        p: BuildParams,
    ) -> Result<(BuildId, BuilderPod, Snapshot)> {
        let context = RelPath::parse(&p.context)?;
        let dockerfile = RelPath::parse(p.dockerfile.as_deref().unwrap_or("Dockerfile"))?;
        let id = BuildId::fresh();
        let bud = bud_argv(&id, &dockerfile, p.target.as_deref(), &p.build_args)?;
        let live: Vec<Pod> = self
            .list(&caller.sandbox, None)
            .await?
            .into_iter()
            .filter(alive)
            .collect();
        if live.len() >= self.max_builds {
            let ids: Vec<&str> = live
                .iter()
                .filter_map(|p| p.metadata.name.as_deref())
                .collect();
            bail!(
                "this sandbox has {} live builds (max {}): {}; clean one first",
                live.len(),
                self.max_builds,
                ids.join(", ")
            );
        }
        let stage = tempfile::tempdir().context("creating staging dir")?;
        let ctx = stage.path().join("ctx");
        tokio::task::block_in_place(|| {
            caller
                .workspace
                .fetch_context(&caller.sandbox, &context, &ctx)
        })?;
        let tarball = crate::context::tarball(&ctx)?;
        tracing::info!(
            "build {}: sandbox {} context {} ({} KiB)",
            id.0,
            caller.sandbox.as_str(),
            context.as_str(),
            tarball.len() / 1024
        );
        let created = Instant::now();
        let pod = BuilderPod::create_from(
            self.client.clone(),
            &self.namespace,
            &self.builder_pod(caller, &id)?,
        )
        .await?;
        let built = async {
            pod.wait_ready(POD_READY).await?;
            pod.exec_capture(&Backend::Buildah.setup_command()).await?;
            pod.exec_with_stdin(&Backend::Buildah.untar_command(), &tarball)
                .await?;
            let requested = timeout(p.timeout_s, DEFAULT_BUILD_TIMEOUT_S);
            let limit = build_limit(requested, self.deadline_secs, created.elapsed())?;
            let args = step::build_args(limit, &bud);
            let mirror = Some(step::CONTAINER_STDOUT);
            step::launch(&pod, LogName::Build, mirror, &[], args).await?;
            step::snapshot(&pod, LogName::Build)
                .await?
                .ok_or_else(|| anyhow!("the build step did not start"))
        }
        .await;
        match built {
            Ok(snap) => Ok((id, pod, snap)),
            Err(e) => {
                if let Err(e) = pod.delete().await {
                    tracing::warn!("deleting builder pod {}: {e:#}", id.0);
                }
                Err(e)
            }
        }
    }

    pub async fn build(&self, caller: &Caller, p: BuildParams) -> Result<BuildReply> {
        let (id, _, snap) = self.start_build(caller, p).await?;
        Ok(BuildReply {
            build_id: id.0,
            status: snap.state.status(),
        })
    }

    pub async fn status(&self, sandbox: &SandboxName, p: StatusParams) -> Result<StatusReply> {
        let id = BuildId::parse(&p.build_id)?;
        let found = self.lookup(sandbox, &id).await?;
        let (_, reply) = self
            .build_status(&id, &found)
            .await?
            .ok_or_else(|| anyhow!("build {} has no build step", id.0))?;
        Ok(reply)
    }

    // the build's state and its status reply, live or from an ended pod; None before it starts
    async fn build_status(
        &self,
        id: &BuildId,
        found: &Found,
    ) -> Result<Option<(Snapshot, StatusReply)>> {
        let (snap, log_tail) = match found {
            Found::Live(pod) => {
                let Some(snap) = step::snapshot(pod, LogName::Build).await? else {
                    return Ok(None);
                };
                (snap, log_tail(pod, LogName::Build).await?)
            }
            Found::Ended {
                pod,
                state,
                created,
            } => {
                let tail = match Log::from_container(pod, SUMMARY_WINDOW).await {
                    Ok(log) => summary(&log),
                    Err(e) => {
                        tracing::warn!("reading the ended build {}'s log: {e:#}", id.0);
                        String::new()
                    }
                };
                let snap = Snapshot {
                    created: created.clone(),
                    updated: created.clone(),
                    state: *state,
                    fetch: Fetch::Nothing,
                    published: None,
                };
                (snap, tail)
            }
        };
        let reply = StatusReply {
            build_id: id.0.clone(),
            status: snap.state.status(),
            exit: snap.state.exit(),
            timed_out: snap.state.timed_out(),
            log_tail,
        };
        Ok(Some((snap, reply)))
    }

    async fn start_run(
        &self,
        caller: &Caller,
        p: RunParams,
    ) -> Result<(BuildId, BuilderPod, LogName, Snapshot)> {
        if p.cmd.is_empty() {
            bail!("cmd must not be empty");
        }
        let id = BuildId::parse(&p.build_id)?;
        let paths = p
            .fetch_paths
            .iter()
            .map(|f| RelPath::parse(f.trim_start_matches('/')))
            .collect::<Result<Vec<_>>>()?;
        let pod = self.find(&caller.sandbox, &id).await?;
        let built = step::snapshot(&pod, LogName::Build)
            .await?
            .ok_or_else(|| anyhow!("build {} has no build step", id.0))?;
        match built.state {
            step::State::Exited { code: 0, .. } => {}
            step::State::Running => bail!("build {} is still building; poll status", id.0),
            state => bail!(
                "build {} {} (exit {}); read its logs, then clean",
                id.0,
                if state == step::State::Cancelled {
                    "was cancelled"
                } else {
                    "failed"
                },
                state.exit().map_or("none".to_string(), |c| c.to_string())
            ),
        }
        let alloc = pod
            .exec_capture(&argv(["sh", "-c", ALLOC_SCRIPT, "sh", LOG_DIR]))
            .await?;
        let n: u32 = alloc
            .trim()
            .parse()
            .with_context(|| format!("bad run number {alloc:?}"))?;
        let log = LogName::Run(n);
        let ctr = format!("{}-run{n}", id.0);
        let limit = timeout(p.timeout_s, DEFAULT_RUN_TIMEOUT_S);
        let args = step::run_args(FROM_TIMEOUT, limit, &ctr, &id.tag(), &p.cmd);
        step::launch(&pod, log, None, &paths, args).await?;
        let snap = step::snapshot(&pod, log)
            .await?
            .ok_or_else(|| anyhow!("the {log} step did not start"))?;
        Ok((id, pod, log, snap))
    }

    pub async fn run(&self, caller: &Caller, p: RunParams) -> Result<RunReply> {
        let limit = FROM_TIMEOUT + timeout(p.timeout_s, DEFAULT_RUN_TIMEOUT_S) + EXEC_SLACK;
        let (id, pod, log, _) = self.start_run(caller, p).await?;
        let mut snap = step::wait(&pod, log, limit).await?;
        let published = loop {
            match task::progress(&snap) {
                Progress::Done { published, .. } => break published,
                Progress::Working | Progress::Cancelled => break Published::Fetched(Vec::new()),
                Progress::Publishing => {
                    if let Some(p) = self.settle(caller, &pod, &id, log, snap.fetch).await? {
                        break p;
                    }
                }
            }
            tokio::time::sleep(SETTLE_RETRY).await;
            snap = step::snapshot(&pod, log)
                .await?
                .ok_or_else(|| anyhow!("the {log} step is gone"))?;
        };
        match published {
            Published::Fetched(files) => run_reply(&pod, &id, log, snap.state, &files).await,
            Published::Error(e) => Err(anyhow!(e)),
        }
    }

    // publishes a finished run's fetch_paths once; None while another caller holds the claim
    async fn settle(
        &self,
        caller: &Caller,
        pod: &BuilderPod,
        id: &BuildId,
        log: LogName,
        fetch: Fetch,
    ) -> Result<Option<Published>> {
        if !step::claim(pod, log).await? {
            return Ok(None);
        }
        let published = match fetch {
            Fetch::Nothing => Published::Fetched(Vec::new()),
            Fetch::Ready => match self.publish(caller, pod, id, log).await {
                Ok(files) => Published::Fetched(files),
                Err(e) => Published::Error(format!("{e:#}")),
            },
            Fetch::Failed => Published::Error(format!(
                "fetching paths failed: {}",
                step::fetch_error(pod, log).await?
            )),
        };
        step::record(pod, log, &published).await?;
        Ok(Some(published))
    }

    async fn publish(
        &self,
        caller: &Caller,
        pod: &BuilderPod,
        id: &BuildId,
        log: LogName,
    ) -> Result<Vec<String>> {
        let tar = step::fetched_tar(pod, log).await?;
        let stage = tempfile::tempdir().context("creating staging dir")?;
        let results = stage.path().join(&id.0);
        std::fs::create_dir(&results).context("creating results dir")?;
        let fetched = unpack(&tar, &results)?;
        tokio::task::block_in_place(|| caller.workspace.publish(&caller.sandbox, &results))?;
        Ok(fetched)
    }

    // None when the task is unknown to this caller; only a caller who may build settles a run
    async fn observe(
        &self,
        caller: &Caller,
        t: &TaskRef,
        may_build: bool,
    ) -> Result<Option<(Snapshot, View)>> {
        let Some(found) = self.found(&caller.sandbox, &t.build).await? else {
            return Ok(None);
        };
        if t.log == LogName::Build {
            let Some((snap, reply)) = self.build_status(&t.build, &found).await? else {
                return Ok(None);
            };
            let view = match task::progress(&snap) {
                Progress::Working | Progress::Publishing => View::Working("building"),
                Progress::Cancelled => View::Cancelled,
                Progress::Done { state, .. } => View::Completed {
                    is_error: state.status() != Status::Succeeded,
                    text: serde_json::to_string(&reply).context("serializing reply")?,
                },
            };
            return Ok(Some((snap, view)));
        }
        let Found::Live(pod) = found else {
            return Ok(None);
        };
        let Some(mut snap) = step::snapshot(&pod, t.log).await? else {
            return Ok(None);
        };
        if may_build
            && task::progress(&snap) == Progress::Publishing
            && self
                .settle(caller, &pod, &t.build, t.log, snap.fetch)
                .await?
                .is_some()
        {
            snap = step::snapshot(&pod, t.log)
                .await?
                .ok_or_else(|| anyhow!("the {} step is gone", t.log))?;
        }
        let view = match task::progress(&snap) {
            Progress::Working => View::Working("running"),
            Progress::Publishing => View::Working("publishing fetch_paths"),
            Progress::Cancelled => View::Cancelled,
            Progress::Done {
                published: Published::Error(e),
                ..
            } => View::Completed {
                is_error: true,
                text: e,
            },
            Progress::Done {
                state,
                published: Published::Fetched(files),
            } => {
                let reply = run_reply(&pod, &t.build, t.log, state, &files).await?;
                View::Completed {
                    is_error: state.status() != Status::Succeeded,
                    text: serde_json::to_string(&reply).context("serializing reply")?,
                }
            }
        };
        Ok(Some((snap, view)))
    }

    // None when the task is unknown to this caller; a task that already ended is left as is
    async fn cancel(&self, caller: &Caller, t: &TaskRef) -> Result<Option<()>> {
        let Some(found) = self.found(&caller.sandbox, &t.build).await? else {
            return Ok(None);
        };
        let pod = match found {
            Found::Live(pod) => pod,
            Found::Ended { .. } if t.log == LogName::Build => return Ok(Some(())),
            Found::Ended { .. } => return Ok(None),
        };
        let Some(snap) = step::snapshot(&pod, t.log).await? else {
            return Ok(None);
        };
        if snap.state == step::State::Running && step::cancel(&pod, t.log).await? {
            tracing::info!(
                "task {} cancelled by sandbox {}",
                t.encode(),
                caller.sandbox.as_str()
            );
        }
        Ok(Some(()))
    }

    pub async fn logs(&self, sandbox: &SandboxName, p: LogsParams) -> Result<Page> {
        let id = BuildId::parse(&p.build_id)?;
        let name = LogName::parse(p.which.as_deref().unwrap_or("build"))?;
        let query = p.query()?;
        let max = p
            .max_bytes
            .unwrap_or(DEFAULT_LOG_BYTES)
            .clamp(1024, MAX_LOG_BYTES);
        let log = match self.lookup(sandbox, &id).await? {
            Found::Live(pod) => Log::fetch(&pod, name, MAX_READ).await?,
            Found::Ended { pod, .. } if name == LogName::Build => {
                Log::from_container(&pod, MAX_READ).await?
            }
            Found::Ended { .. } => {
                bail!("build {}'s pod has ended; only its build log is kept", id.0)
            }
        };
        Ok(log.query(&query, usize_of(max)))
    }

    pub async fn clean(&self, sandbox: &SandboxName, id: Option<&str>) -> Result<CleanReply> {
        let id = id.map(BuildId::parse).transpose()?;
        let mut deleted = Vec::new();
        for pod in self.list(sandbox, id.as_ref()).await? {
            if pod.metadata.deletion_timestamp.is_some() {
                continue;
            }
            let name = pod.metadata.name.unwrap_or_default();
            self.pods()
                .delete(&name, &DeleteParams::default())
                .await
                .with_context(|| format!("deleting builder pod {name}"))?;
            deleted.push(name);
        }
        deleted.sort();
        Ok(CleanReply { deleted })
    }
}

enum View {
    Working(&'static str),
    Cancelled,
    Completed { is_error: bool, text: String },
}

#[derive(Clone)]
pub struct BuilditMcp {
    broker: Arc<Broker>,
}

fn caller(ctx: &RequestContext<RoleServer>) -> Result<Caller> {
    ctx.extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<Caller>())
        .cloned()
        .ok_or_else(|| anyhow!("request reached a tool without an authenticated caller"))
}

/// Every tool and method buildit serves and the scope each needs; the guard refuses anything
/// else. Builds run in buildit's own namespace under the sandbox's identity, so there are no
/// Kubernetes access checks.
pub struct BuilditRules;

impl AccessRules for BuilditRules {
    fn scopes(&self, call: &Call<'_>) -> Result<&'static [&'static str], Refusal> {
        match call {
            Call::Tool { name, .. } => match *name {
                "status" | "logs" => Ok(&[SCOPE_READ]),
                "build" | "run" | "clean" => Ok(&[SCOPE_BUILD]),
                _ => Err(call.unserved()),
            },
            Call::Method { name, .. } => match *name {
                "tasks/get" => Ok(&[SCOPE_READ]),
                "tasks/cancel" => Ok(&[SCOPE_BUILD]),
                _ => Err(call.unserved()),
            },
        }
    }

    fn checks(&self, _: &Call<'_>, _: &Principal) -> Result<Vec<AccessCheck>, Refusal> {
        Ok(Vec::new())
    }
}

fn may_build(ctx: &RequestContext<RoleServer>) -> bool {
    ctx.extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<Principal>())
        .is_some_and(|p| p.scopes.iter().any(|s| s == SCOPE_BUILD))
}

fn wants_task(ctx: &RequestContext<RoleServer>) -> bool {
    ctx.client_capabilities()
        .is_some_and(|c| c.supports_tasks())
}

fn reply<T: Serialize>(result: Result<T>) -> Result<String, String> {
    result
        .and_then(|r| serde_json::to_string(&r).context("serializing reply"))
        .map_err(|e| format!("{e:#}"))
}

fn complete<T: Serialize>(result: Result<T>) -> Result<CallToolResponse, String> {
    reply(result).map(|text| CallToolResult::success(vec![ContentBlock::text(text)]).into())
}

fn task_meta(t: &TaskRef, snap: &Snapshot, status: TaskStatus) -> Task {
    Task::new(
        t.encode(),
        status,
        snap.created.clone(),
        snap.updated.clone(),
    )
    .with_poll_interval_ms(task::POLL_INTERVAL_MS)
}

fn created(t: &TaskRef, snap: &Snapshot) -> CallToolResponse {
    CallToolResponse::Task(CreateTaskResult::new(
        task_meta(t, snap, TaskStatus::Working).with_status_message("running"),
    ))
}

fn detailed(t: &TaskRef, snap: &Snapshot, view: View) -> Result<DetailedTask, McpError> {
    Ok(match view {
        View::Working(message) => DetailedTask::new(
            task_meta(t, snap, TaskStatus::Working).with_status_message(message),
            TaskPayload::Working,
        ),
        View::Cancelled => DetailedTask::new(
            task_meta(t, snap, TaskStatus::Cancelled).with_status_message("cancelled"),
            TaskPayload::Cancelled,
        ),
        View::Completed { is_error, text } => {
            let (result, message) = if is_error {
                (
                    CallToolResult::error(vec![ContentBlock::text(text)]),
                    "failed",
                )
            } else {
                (
                    CallToolResult::success(vec![ContentBlock::text(text)]),
                    "succeeded",
                )
            };
            let serde_json::Value::Object(result) = serde_json::to_value(result)
                .map_err(|e| McpError::internal_error(format!("serializing result: {e}"), None))?
            else {
                return Err(McpError::internal_error("result is not an object", None));
            };
            DetailedTask::new(
                task_meta(t, snap, TaskStatus::Completed).with_status_message(message),
                TaskPayload::Completed { result },
            )
        }
    })
}

fn task_ref(id: &str) -> Result<TaskRef, McpError> {
    TaskRef::parse(id).ok_or_else(|| unknown_task(id))
}

fn unknown_task(id: &str) -> McpError {
    McpError::invalid_params(format!("unknown task: {id}"), None)
}

fn internal(e: anyhow::Error) -> McpError {
    McpError::internal_error(format!("{e:#}"), None)
}

#[tool_router]
impl BuilditMcp {
    #[tool(
        description = "Start building a Dockerfile (params: context, dockerfile, target, build_args, \
        timeout_s); pushes nothing. Returns build_id with status running at once. Poll `status` \
        until succeeded or failed, then `run`/`logs` as needed, and always `clean` the build_id. \
        Clients with the tasks extension get a task to poll instead."
    )]
    async fn build(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<BuildParams>,
    ) -> Result<CallToolResponse, String> {
        if !wants_task(&ctx) {
            return complete(async { self.broker.build(&caller(&ctx)?, p).await }.await);
        }
        let started = async { self.broker.start_build(&caller(&ctx)?, p).await }.await;
        let (id, _, snap) = started.map_err(|e| format!("{e:#}"))?;
        let t = TaskRef {
            build: id,
            log: LogName::Build,
        };
        Ok(created(&t, &snap))
    }

    #[tool(
        description = "Report a build's status (params: build_id): running, succeeded, failed or \
        cancelled, with exit, timed_out and the build log's last lines. Poll it after `build`; \
        `clean` when done."
    )]
    async fn status(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<StatusParams>,
    ) -> Result<String, String> {
        reply(async { self.broker.status(&caller(&ctx)?.sandbox, p).await }.await)
    }

    #[tool(
        description = "Run a command (params: build_id, cmd, timeout_s, fetch_paths) in a fresh \
        container of a build whose status is succeeded. Returns exit, the \
        log name (run:<n>) and its last lines; fetch_paths are copied out of the container into \
        .buildit/<build_id>/<path>. Clients with the tasks extension get a task to poll."
    )]
    async fn run(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<RunParams>,
    ) -> Result<CallToolResponse, String> {
        if !wants_task(&ctx) {
            return complete(async { self.broker.run(&caller(&ctx)?, p).await }.await);
        }
        let started = async { self.broker.start_run(&caller(&ctx)?, p).await }.await;
        let (id, _, log, snap) = started.map_err(|e| format!("{e:#}"))?;
        Ok(created(&TaskRef { build: id, log }, &snap))
    }

    #[tool(
        description = "Read part of a build or run log (params: build_id, which, grep, context, \
        from_line, to_line, tail_lines, max_bytes), as numbered lines: grep with optional context, a from_line/to_line range, or the last tail_lines (default 100). Reports \
        total_lines and matched; output is capped at max_bytes and long lines are clipped. \
        When output overflows max_bytes, tail and grep keep the newest lines (the end of the \
        log) and range keeps the first; truncated is set. \
        Only the newest 4 MiB is read; when truncated_head is set, line 1 is the first whole \
        line of that window."
    )]
    async fn logs(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<LogsParams>,
    ) -> Result<String, String> {
        reply(async { self.broker.logs(&caller(&ctx)?.sandbox, p).await }.await)
    }

    #[tool(
        description = "Delete a build's builder pod (params: build_id), or all of this sandbox's \
        builds when build_id is omitted. Call it once done with a build, succeeded or not."
    )]
    async fn clean(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<CleanParams>,
    ) -> Result<String, String> {
        reply(
            async {
                self.broker
                    .clean(&caller(&ctx)?.sandbox, p.build_id.as_deref())
                    .await
            }
            .await,
        )
    }
}

#[tool_handler]
impl ServerHandler for BuilditMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tasks()
                .build(),
        )
        .with_server_info(Implementation::new("buildit", env!("CARGO_PKG_VERSION")))
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        let caller = caller(&ctx).map_err(internal)?;
        let t = task_ref(&request.task_id)?;
        let (snap, view) = self
            .broker
            .observe(&caller, &t, may_build(&ctx))
            .await
            .map_err(internal)?
            .ok_or_else(|| unknown_task(&request.task_id))?;
        Ok(GetTaskResult::new(detailed(&t, &snap, view)?))
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let caller = caller(&ctx).map_err(internal)?;
        let t = task_ref(&request.task_id)?;
        self.broker
            .cancel(&caller, &t)
            .await
            .map_err(internal)?
            .ok_or_else(|| unknown_task(&request.task_id))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::backend::Resources;
    use std::time::Duration;

    use k8s_openapi::api::core::v1::Pod;

    use crate::mcp::{
        Broker, BuildId, BuildParams, BuilditMcp, BuilditRules, Caller, Launcher, LogsParams,
        MCP_PATH, PodView, ResourceServer, ResourceServerConfig, Response, SCOPE_BUILD, SCOPE_READ,
        StatusCode, TokenLine, ToolPolicy, TrustedIssuer, bud_argv, build_limit, parse_tokens,
        router, unpack,
    };
    use crate::sandbox::{RelPath, SandboxDir, SandboxName, Workspace};
    use crate::step::{State, Status};
    use crucible_mcp_auth::access::{AccessRules, Call};
    use crucible_mcp_auth::principal::Callers;
    use crucible_mcp_auth::server::JwksTls;

    fn broker(client: kube::Client, tokens: &Path, local: Option<&Path>) -> Arc<Broker> {
        broker_with_deadline(client, tokens, local, 900)
    }

    fn broker_with_deadline(
        client: kube::Client,
        tokens: &Path,
        local: Option<&Path>,
        deadline_secs: i64,
    ) -> Arc<Broker> {
        Arc::new(Broker {
            namespace: client.default_namespace().to_string(),
            client,
            tokens_file: Some(tokens.to_path_buf()),
            oidc: None,
            default_workdir: Some(SandboxDir::parse("/sandbox/default").unwrap()),
            local_workdir: local.map(Path::to_path_buf),
            resources: Resources::default(),
            deadline_secs,
            max_builds: 4,
            node_arch: None,
        })
    }

    fn sandbox_caller(sandbox: &str) -> Caller {
        Caller {
            sandbox: SandboxName::parse(sandbox).unwrap(),
            workspace: Workspace::Openshell {
                workdir: SandboxDir::parse("/sandbox/default").unwrap(),
            },
            launched_by: None,
        }
    }

    fn offline_client() -> kube::Client {
        let _ = rustls::crypto::ring::default_provider().install_default();
        kube::Client::try_from(kube::Config::new("http://127.0.0.1:9".parse().unwrap())).unwrap()
    }

    fn tokens(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("tokens");
        std::fs::write(&path, body).unwrap();
        path
    }

    async fn serve(broker: Arc<Broker>) -> (String, tokio::task::JoinHandle<()>) {
        serve_with(broker, ToolPolicy::default()).await
    }

    async fn serve_with(
        broker: Arc<Broker>,
        policy: ToolPolicy,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let app = router(broker, &["builds.svc".to_string()], policy);
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, task)
    }

    #[test]
    fn token_lines_take_an_optional_workdir() {
        let lines = parse_tokens("tok-a sb-a /sandbox/task-a\n\n  tok-b  sb-b \n").unwrap();
        assert_eq!(
            lines,
            [
                TokenLine {
                    token: "tok-a".to_string(),
                    sandbox: SandboxName::parse("sb-a").unwrap(),
                    workdir: Some(SandboxDir::parse("/sandbox/task-a").unwrap()),
                },
                TokenLine {
                    token: "tok-b".to_string(),
                    sandbox: SandboxName::parse("sb-b").unwrap(),
                    workdir: None,
                },
            ]
        );
        for bad in [
            "tok-a\n",
            "tok-a sb-a /w x\n",
            "tok-a --flag\n",
            "tok-a sb-a rel\n",
        ] {
            assert!(parse_tokens(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn callers_come_from_the_bearer_token_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = tokens(dir.path(), "tok-a sb-a /sandbox/a\ntok-b sb-b\n");
        let broker = broker(offline_client(), &file, None);
        let caller = |sandbox: &str, workdir: &str| Caller {
            sandbox: SandboxName::parse(sandbox).unwrap(),
            workspace: Workspace::Openshell {
                workdir: SandboxDir::parse(workdir).unwrap(),
            },
            launched_by: None,
        };
        let a = [
            ("authorization", "Bearer tok-a"),
            ("x-crucible-sandbox", "sb-b"),
        ];
        assert_eq!(
            broker.caller(&headers(&a)).await,
            Ok(caller("sb-a", "/sandbox/a"))
        );
        assert_eq!(
            broker
                .caller(&headers(&[("authorization", "Bearer tok-b")]))
                .await,
            Ok(caller("sb-b", "/sandbox/default"))
        );
        for bad in [
            &[("authorization", "Bearer tok-")][..],
            &[("authorization", "Bearer sb-a")],
            &[("authorization", "tok-a")],
            &[("x-crucible-sandbox", "sb-a")],
        ] {
            assert_eq!(
                broker.caller(&headers(bad)).await,
                Err(axum::http::StatusCode::UNAUTHORIZED)
            );
        }
        std::fs::write(&file, "tok-c sb-c /sandbox/c\n").unwrap();
        assert_eq!(
            broker
                .caller(&headers(&[("authorization", "Bearer tok-a")]))
                .await,
            Err(axum::http::StatusCode::UNAUTHORIZED)
        );
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> axum::http::HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| (axum::http::HeaderName::from_static(k), v.parse().unwrap()))
            .collect()
    }

    async fn post(addr: &str, host: &str, auth: &str, body: &str) -> (u16, String) {
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "POST {MCP_PATH} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\n{auth}Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        sock.read_to_string(&mut out).await.unwrap();
        let status = out.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, out)
    }

    const RESOURCE: &str = "http://host.openshell.internal:8849/mcp";
    const POLICY: &str = include_str!("../deploy/buildit-policy.toml");

    fn realm_claims() -> serde_json::Value {
        let mut claims = crucible_mcp_auth::testing::run_claims();
        claims["aud"] = serde_json::json!([RESOURCE]);
        claims["scope"] = serde_json::json!("buildit.read buildit.build");
        claims["sandbox"] = serde_json::json!("ci-0123456789abcdef");
        claims["workdir"] = serde_json::json!("/sandbox/repo");
        claims
    }

    async fn realm_broker(file: &Path) -> (crucible_mcp_auth::testing::JwksServer, Arc<Broker>) {
        let jwks =
            crucible_mcp_auth::testing::JwksServer::start(vec![crucible_mcp_auth::testing::jwk(
                "a",
            )])
            .await;
        let config = ResourceServerConfig {
            issuers: vec![TrustedIssuer {
                issuer: crucible_mcp_auth::testing::ISSUER.to_string(),
                jwks_uri: jwks.url.clone(),
                callers: Callers::Runs,
            }],
            resources: vec![url::Url::parse(RESOURCE).unwrap()],
            required_scopes: vec![SCOPE_READ.to_string()],
            scopes_supported: vec![SCOPE_READ.to_string(), SCOPE_BUILD.to_string()],
            resource_name: Some("buildit".to_string()),
        };
        let broker = Broker {
            oidc: Some(Arc::new(
                ResourceServer::new(config, &JwksTls::default()).unwrap(),
            )),
            ..Arc::into_inner(broker(offline_client(), file, None)).unwrap()
        };
        (jwks, Arc::new(broker))
    }

    fn bearer_header(token: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    async fn status_of(r: Response) -> (StatusCode, String) {
        let status = r.status();
        let challenge = r
            .headers()
            .get(axum::http::header::WWW_AUTHENTICATE)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (status, challenge)
    }

    #[tokio::test]
    async fn a_realm_token_names_its_sandbox_workdir_and_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let (_jwks, broker) = realm_broker(&tokens(dir.path(), "")).await;
        let token = crucible_mcp_auth::testing::sign("a", &realm_claims());
        let (caller, principal) = broker.authenticate(&bearer_header(&token)).await.unwrap();
        assert_eq!(caller.sandbox.as_str(), "ci-0123456789abcdef");
        assert_eq!(
            caller.workspace,
            Workspace::Openshell {
                workdir: SandboxDir::parse("/sandbox/repo").unwrap()
            }
        );
        assert_eq!(
            caller.launched_by.as_ref().map(Launcher::as_str),
            Some("alice")
        );
        assert_eq!(
            principal.presenter(),
            "spiffe://example.crucible/openshell/sandbox/auth-spike"
        );
        let pod = broker
            .builder_pod(&caller, &BuildId::parse("buildit-1").unwrap())
            .unwrap();
        assert_eq!(
            pod.metadata.labels.unwrap()["crucible.dev/launched-by"],
            "alice"
        );
    }

    #[tokio::test]
    async fn a_realm_token_without_workdir_uses_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let (_jwks, broker) = realm_broker(&tokens(dir.path(), "")).await;
        let mut claims = realm_claims();
        claims.as_object_mut().unwrap().remove("workdir");
        let token = crucible_mcp_auth::testing::sign("a", &claims);
        let (caller, _) = broker.authenticate(&bearer_header(&token)).await.unwrap();
        assert_eq!(
            caller.workspace,
            Workspace::Openshell {
                workdir: SandboxDir::parse("/sandbox/default").unwrap()
            }
        );
    }

    #[tokio::test]
    async fn realm_tokens_without_a_usable_sandbox_or_subject_are_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        let (_jwks, broker) = realm_broker(&tokens(dir.path(), "")).await;
        let cases = [
            ("sandbox", None),
            ("sandbox", Some(serde_json::json!("not a name"))),
            ("workdir", Some(serde_json::json!("relative/path"))),
            ("sub", Some(serde_json::json!("alice@example.com"))),
        ];
        for (key, value) in cases {
            let mut claims = realm_claims();
            let map = claims.as_object_mut().unwrap();
            match &value {
                Some(v) => map.insert(key.into(), v.clone()),
                None => map.remove(key),
            };
            let token = crucible_mcp_auth::testing::sign("a", &claims);
            let refused = broker
                .authenticate(&bearer_header(&token))
                .await
                .unwrap_err();
            assert_eq!(
                status_of(broker.respond(&bearer_header(&token), refused))
                    .await
                    .0,
                StatusCode::FORBIDDEN,
                "{key} = {value:?}"
            );
        }
    }

    #[tokio::test]
    async fn bad_or_missing_tokens_get_the_mcp_challenge_when_oidc_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let (_jwks, broker) = realm_broker(&tokens(dir.path(), "")).await;
        let missing = broker
            .authenticate(&axum::http::HeaderMap::new())
            .await
            .unwrap_err();
        let missing = broker.respond(&axum::http::HeaderMap::new(), missing);
        let (status, challenge) = status_of(missing).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            challenge.contains(r#"resource_metadata="http://host.openshell.internal:8849/.well-known/oauth-protected-resource/mcp""#),
            "{challenge}"
        );
        let mut expired = realm_claims();
        expired["exp"] = serde_json::json!(crucible_mcp_auth::testing::now() - 300);
        for token in [
            crucible_mcp_auth::testing::sign("a", &expired),
            "unknown-opaque".to_string(),
        ] {
            let refused = broker
                .authenticate(&bearer_header(&token))
                .await
                .unwrap_err();
            let (status, challenge) =
                status_of(broker.respond(&bearer_header(&token), refused)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert!(
                challenge.contains(r#"error="invalid_token""#),
                "{challenge}"
            );
        }
    }

    #[tokio::test]
    async fn tokens_file_callers_keep_working_and_a_jwt_never_falls_back_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let jwt_shaped = "a.b.c";
        let file = tokens(
            dir.path(),
            &format!("tok-a sb-a /sandbox/a\n{jwt_shaped} sb-b\n"),
        );
        let (_jwks, broker) = realm_broker(&file).await;
        let (caller, principal) = broker.authenticate(&bearer_header("tok-a")).await.unwrap();
        assert_eq!(caller.sandbox.as_str(), "sb-a");
        assert_eq!(caller.launched_by, None);
        assert_eq!(principal.subject, "sandbox:sb-a");
        assert_eq!(principal.claims["sandbox"], "sb-a");
        assert!(principal.scopes.contains(SCOPE_BUILD));
        let refused = broker
            .authenticate(&bearer_header(jwt_shaped))
            .await
            .unwrap_err();
        let refused = broker.respond(&bearer_header(jwt_shaped), refused);
        assert_eq!(status_of(refused).await.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn every_tool_has_a_scope_and_the_shipped_policy_loads() {
        ToolPolicy::from_toml(POLICY).unwrap();
        let args = serde_json::Map::new();
        for tool in BuilditMcp::tool_router().list_all() {
            let call = Call::Tool {
                name: &tool.name,
                args: &args,
            };
            assert_eq!(
                BuilditRules.scopes(&call).map(<[_]>::len),
                Ok(1),
                "{}",
                tool.name
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_policy_refuses_an_over_long_build_before_it_reaches_buildit() {
        let dir = tempfile::tempdir().unwrap();
        let (_jwks, broker) = realm_broker(&tokens(dir.path(), "")).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let app = router(broker, &[], ToolPolicy::from_toml(POLICY).unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let token = crucible_mcp_auth::testing::sign("a", &realm_claims());
        let call = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"build","arguments":{"context":".","timeout_s":7200}}}"#;
        let (status, reply) = post(
            &addr,
            "host.openshell.internal",
            &format!("Authorization: Bearer {token}\r\n"),
            call,
        )
        .await;
        assert_eq!(status, 200, "{reply}");
        assert!(reply.contains("build-timeout-cap"), "{reply}");
        assert!(reply.contains(r#""code":-32001"#), "{reply}");
        let (status, metadata) = post(&addr, "host.openshell.internal", "", "").await;
        assert_eq!(status, 401, "{metadata}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_is_stateless_json_behind_the_token_and_host_checks() {
        let dir = tempfile::tempdir().unwrap();
        let file = tokens(dir.path(), "tok-a sb-a /sandbox/a\n");
        let (addr, _task) = serve(broker(offline_client(), &file, None)).await;
        let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
        let auth = "Authorization: Bearer tok-a\r\n";
        let (status, reply) = post(&addr, "127.0.0.1", auth, list).await;
        let reply = reply.to_ascii_lowercase();
        assert_eq!(status, 200, "{reply}");
        assert!(reply.contains("content-type: application/json"), "{reply}");
        assert!(!reply.contains("mcp-session-id"), "{reply}");
        for tool in [
            "\"build\"",
            "\"status\"",
            "\"run\"",
            "\"logs\"",
            "\"clean\"",
        ] {
            assert!(reply.contains(tool), "{tool}: {reply}");
        }
        for host in ["host.openshell.internal:8849", "builds.svc"] {
            assert_eq!(post(&addr, host, auth, list).await.0, 200, "{host}");
        }
        assert_eq!(post(&addr, "evil.example", auth, list).await.0, 403);
        assert_eq!(post(&addr, "127.0.0.1", "", list).await.0, 401);
        let wrong = "Authorization: Bearer nope\r\n";
        assert_eq!(post(&addr, "127.0.0.1", wrong, list).await.0, 401);
    }

    fn rpc_body(reply: &str) -> serde_json::Value {
        let (_, body) = reply.split_once("\r\n\r\n").unwrap();
        serde_json::from_str(body).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discover_advertises_tasks_and_malformed_task_ids_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let file = tokens(dir.path(), "tok-a sb-a /sandbox/a\n");
        let (addr, _task) = serve(broker(offline_client(), &file, None)).await;
        let headers = |method: &str, name: Option<&str>| {
            let name = name
                .map(|n| format!("Mcp-Name: {n}\r\n"))
                .unwrap_or_default();
            format!(
                "Authorization: Bearer tok-a\r\nMCP-Protocol-Version: 2026-07-28\r\n\
                 Mcp-Method: {method}\r\n{name}"
            )
        };
        let meta = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {
                "extensions": { "io.modelcontextprotocol/tasks": {} },
            },
        });
        let discover = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": { "_meta": meta },
        });
        let auth = headers("server/discover", None);
        let (status, reply) = post(&addr, "127.0.0.1", &auth, &discover.to_string()).await;
        assert_eq!(status, 200, "{reply}");
        let reply = rpc_body(&reply);
        let result = &reply["result"];
        assert!(
            result["capabilities"]["extensions"]["io.modelcontextprotocol/tasks"].is_object(),
            "{reply}"
        );
        assert!(result["capabilities"]["tools"].is_object(), "{reply}");
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "buildit",
            "{reply}"
        );

        for method in ["tasks/get", "tasks/cancel"] {
            for id in ["nope", "build/B", "run/buildit-1/01", "run/buildit-1"] {
                let body = serde_json::json!({
                    "jsonrpc": "2.0", "id": 2, "method": method,
                    "params": { "taskId": id, "_meta": meta },
                });
                let auth = headers(method, Some(id));
                let (_, reply) = post(&addr, "127.0.0.1", &auth, &body.to_string()).await;
                let reply = rpc_body(&reply);
                assert_eq!(reply["error"]["code"], -32602, "{method} {id}: {reply}");
                assert_eq!(
                    reply["error"]["message"],
                    format!("unknown task: {id}"),
                    "{method} {id}: {reply}"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_only_token_cannot_cancel_a_task_whatever_the_policy() {
        let dir = tempfile::tempdir().unwrap();
        let mut claims = realm_claims();
        claims["scope"] = serde_json::json!("buildit.read");
        let token = crucible_mcp_auth::testing::sign("a", &claims);
        let auth = |method: &str| {
            format!(
                "Authorization: Bearer {token}\r\nMCP-Protocol-Version: 2026-07-28\r\n\
                 Mcp-Method: {method}\r\nMcp-Name: build/buildit-1\r\n"
            )
        };
        let body = |method: &str| {
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": method,
                "params": {
                    "taskId": "build/buildit-1",
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {
                            "extensions": { "io.modelcontextprotocol/tasks": {} },
                        },
                    },
                },
            })
            .to_string()
        };
        for policy in [
            ToolPolicy::from_toml(POLICY).unwrap(),
            ToolPolicy::default(),
        ] {
            let (_jwks, broker) = realm_broker(&tokens(dir.path(), "")).await;
            let (addr, _task) = serve_with(broker, policy).await;
            let (status, reply) = post(
                &addr,
                "127.0.0.1",
                &auth("tasks/cancel"),
                &body("tasks/cancel"),
            )
            .await;
            assert_eq!(status, 403, "{reply}");
            assert!(reply.contains("insufficient_scope"), "{reply}");
            let (_, reply) = post(&addr, "127.0.0.1", &auth("tasks/get"), &body("tasks/get")).await;
            let reply = rpc_body(&reply);
            assert_ne!(reply["error"]["code"], -32600, "a reader may poll: {reply}");
        }
    }

    #[tokio::test]
    async fn a_token_from_a_people_issuer_is_refused_whatever_its_claims() {
        let dir = tempfile::tempdir().unwrap();
        let jwks =
            crucible_mcp_auth::testing::JwksServer::start(vec![crucible_mcp_auth::testing::jwk(
                "a",
            )])
            .await;
        let config = ResourceServerConfig {
            issuers: vec![TrustedIssuer {
                issuer: crucible_mcp_auth::testing::ISSUER.to_string(),
                jwks_uri: jwks.url.clone(),
                callers: Callers::People,
            }],
            resources: vec![url::Url::parse(RESOURCE).unwrap()],
            required_scopes: vec![SCOPE_READ.to_string()],
            scopes_supported: vec![SCOPE_READ.to_string(), SCOPE_BUILD.to_string()],
            resource_name: Some("buildit".to_string()),
        };
        let broker = Broker {
            oidc: Some(Arc::new(
                ResourceServer::new(config, &JwksTls::default()).unwrap(),
            )),
            ..Arc::into_inner(broker(offline_client(), &tokens(dir.path(), ""), None)).unwrap()
        };
        let token = crucible_mcp_auth::testing::sign("a", &realm_claims());
        let refused = broker
            .authenticate(&bearer_header(&token))
            .await
            .unwrap_err();
        let (status, _) = status_of(broker.respond(&bearer_header(&token), refused)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn log_queries_take_one_mode() {
        let params = |v: serde_json::Value| -> LogsParams {
            let mut v = v;
            v["build_id"] = "b".into();
            serde_json::from_value(v).unwrap()
        };
        let ok = [
            serde_json::json!({}),
            serde_json::json!({ "tail_lines": 5 }),
            serde_json::json!({ "from_line": 2 }),
            serde_json::json!({ "grep": "x", "context": 2 }),
        ];
        for v in ok {
            assert!(params(v.clone()).query().is_ok(), "{v}");
        }
        let bad = [
            serde_json::json!({ "grep": "x", "tail_lines": 5 }),
            serde_json::json!({ "grep": "x", "to_line": 5 }),
            serde_json::json!({ "from_line": 1, "tail_lines": 5 }),
            serde_json::json!({ "context": 1 }),
        ];
        for v in bad {
            assert!(params(v.clone()).query().is_err(), "{v}");
        }
    }

    #[test]
    fn build_target_is_an_alias_of_target() {
        let parse = |v: serde_json::Value| serde_json::from_value::<BuildParams>(v);
        for key in ["target", "build_target"] {
            let p = parse(serde_json::json!({ "context": "svc", key: "base" })).unwrap();
            assert_eq!(p.target.as_deref(), Some("base"), "{key}");
        }
        let p = parse(serde_json::json!({ "context": "svc" })).unwrap();
        assert_eq!(p.target, None);
        let both = serde_json::json!({ "context": "svc", "target": "a", "build_target": "b" });
        assert!(parse(both).is_err());
        let schema = serde_json::to_value(rmcp::schemars::schema_for!(BuildParams)).unwrap();
        assert!(schema["properties"]["target"].is_object(), "{schema}");
        assert_ne!(schema["additionalProperties"], false, "{schema}");
    }

    fn pod(status: serde_json::Value, deleting: bool) -> Pod {
        let mut meta = serde_json::json!({ "name": "buildit-1" });
        if deleting {
            meta["deletionTimestamp"] = "2026-10-02T00:00:00Z".into();
        }
        serde_json::from_value(serde_json::json!({ "metadata": meta, "status": status })).unwrap()
    }

    fn terminated(message: Option<&str>) -> serde_json::Value {
        let mut state = serde_json::json!({ "exitCode": 137 });
        if let Some(m) = message {
            state["message"] = m.into();
        }
        serde_json::json!([{
            "name": "builder", "image": "buildah", "imageID": "", "ready": false,
            "restartCount": 0, "state": { "terminated": state }
        }])
    }

    #[test]
    fn ended_pods_still_report_their_build() {
        let view = |status: serde_json::Value| PodView::of(&[pod(status, false)]);
        let expired = serde_json::json!({ "phase": "Failed", "reason": "DeadlineExceeded" });
        assert_eq!(PodView::of(&[]), None);
        assert_eq!(PodView::of(&[pod(expired.clone(), true)]), None);
        assert_eq!(
            view(serde_json::json!({ "phase": "Running" })),
            Some(PodView::Live)
        );
        assert_eq!(
            view(serde_json::json!({ "phase": "Pending" })),
            Some(PodView::Live)
        );
        assert_eq!(view(serde_json::json!({})), Some(PodView::Live));

        let cases = [
            (expired.clone(), State::Expired),
            (
                serde_json::json!({
                    "phase": "Failed", "reason": "DeadlineExceeded",
                    "containerStatuses": terminated(None),
                }),
                State::Expired,
            ),
            (
                serde_json::json!({
                    "phase": "Failed", "reason": "DeadlineExceeded",
                    "containerStatuses": terminated(Some("garbage")),
                }),
                State::Expired,
            ),
            (
                serde_json::json!({
                    "phase": "Failed", "reason": "DeadlineExceeded",
                    "containerStatuses": terminated(Some("124 timeout\n")),
                }),
                State::Exited {
                    code: 124,
                    timed_out: true,
                },
            ),
            (
                serde_json::json!({
                    "phase": "Failed", "reason": "DeadlineExceeded",
                    "containerStatuses": terminated(Some("0\n")),
                }),
                State::Exited {
                    code: 0,
                    timed_out: false,
                },
            ),
            (serde_json::json!({ "phase": "Succeeded" }), State::Lost),
            (
                serde_json::json!({ "phase": "Failed", "reason": "Evicted" }),
                State::Lost,
            ),
            (
                serde_json::json!({
                    "phase": "Succeeded", "containerStatuses": terminated(Some("2")),
                }),
                State::Exited {
                    code: 2,
                    timed_out: false,
                },
            ),
        ];
        for (status, want) in cases {
            assert_eq!(view(status.clone()), Some(PodView::Ended(want)), "{status}");
        }

        let Some(PodView::Ended(state)) = view(expired) else {
            panic!("expired pod is not ended");
        };
        assert_eq!(
            (state.status(), state.exit(), state.timed_out()),
            (Status::Failed, None, true)
        );
        let Some(PodView::Ended(state)) = view(serde_json::json!({ "phase": "Succeeded" })) else {
            panic!("finished pod is not ended");
        };
        assert_eq!(
            (state.status(), state.exit(), state.timed_out()),
            (Status::Failed, None, false)
        );
    }

    #[test]
    fn build_timeouts_fire_before_the_pod_deadline() {
        let s = Duration::from_secs;
        assert_eq!(build_limit(s(1800), 7200, s(0)).unwrap(), s(1800));
        assert_eq!(build_limit(s(1800), 900, s(0)).unwrap(), s(890));
        assert_eq!(build_limit(s(1800), 900, s(100)).unwrap(), s(790));
        assert_eq!(build_limit(s(60), 900, s(100)).unwrap(), s(60));
        assert_eq!(build_limit(s(1800), 900, s(889)).unwrap(), s(1));
        let left = build_limit(s(1800), 900, Duration::from_millis(100_500)).unwrap();
        assert_eq!(left.as_secs(), 789);
        for (deadline, used) in [(900, 890), (900, 2000), (10, 0), (0, 0), (-5, 0)] {
            assert!(
                build_limit(s(1800), deadline, s(used)).is_err(),
                "{deadline} {used}"
            );
        }
    }

    #[test]
    fn run_logs_allocate_in_sequence_under_a_posix_shell() {
        let dir = tempfile::tempdir().unwrap();
        let alloc = || {
            let out = std::process::Command::new("bash")
                .args(["--posix", "-c", crate::mcp::ALLOC_SCRIPT, "sh"])
                .arg(dir.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap()
        };

        assert_eq!(alloc(), "1\n");
        assert_eq!(alloc(), "2\n");
        std::fs::remove_file(dir.path().join("run-1.log")).unwrap();
        assert_eq!(alloc(), "1\n");
        assert_eq!(alloc(), "3\n");
    }

    #[test]
    fn build_ids_cannot_smuggle_selectors() {
        assert!(BuildId::parse(&BuildId::fresh().0).is_ok());
        for bad in [
            "",
            "A",
            "-x",
            "x-",
            "a,buildit.dev/sandbox=b",
            "a=b",
            "a b",
            "a.b",
        ] {
            assert!(BuildId::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn builder_pods_carry_no_credentials_and_end_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let broker = broker(offline_client(), &tokens(dir.path(), ""), None);
        let id = BuildId::parse("buildit-1").unwrap();
        let pod = broker.builder_pod(&sandbox_caller("sb-a"), &id).unwrap();
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(labels["buildit.dev/sandbox"], "sb-a");
        assert_eq!(labels["buildit.dev/build-id"], "buildit-1");
        assert!(!labels.contains_key("crucible.dev/launched-by"));
        let spec = pod.spec.unwrap();
        assert_eq!(spec.automount_service_account_token, Some(false));
        assert_eq!(spec.enable_service_links, Some(false));
        assert_eq!(spec.active_deadline_seconds, Some(900));
        let c = &spec.containers[0];
        assert_eq!(c.command.as_deref().unwrap(), ["sleep", "900"]);
        assert!(c.env_from.is_none());
        assert!(c.env.iter().flatten().all(|e| e.value_from.is_none()));

        let args = BTreeMap::from([("A".to_string(), "x=y z".to_string())]);
        let df = RelPath::parse("docker/Dockerfile").unwrap();
        let bud = bud_argv(&id, &df, Some("base"), &args).unwrap().join(" ");
        assert_eq!(
            bud,
            "buildah bud --target base --build-arg A=x=y z -f /home/build/workspace/docker/Dockerfile \
             -t localhost/buildit/buildit-1:latest /home/build/workspace"
        );
        let bad = BTreeMap::from([("A=B".to_string(), "v".to_string())]);
        assert!(bud_argv(&id, &df, None, &bad).is_err());
    }

    #[tokio::test]
    async fn builder_pods_pin_the_node_arch_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = sandbox_caller("sb-a");
        let id = BuildId::parse("buildit-1").unwrap();
        let unpinned = broker(offline_client(), &tokens(dir.path(), ""), None)
            .builder_pod(&sandbox, &id)
            .unwrap();
        let arch = |pod: Pod| {
            pod.spec
                .unwrap()
                .node_selector
                .and_then(|s| s.get("kubernetes.io/arch").cloned())
        };
        assert_eq!(arch(unpinned), None);

        let pinned = Broker {
            node_arch: Some("amd64".to_string()),
            ..Arc::into_inner(broker(offline_client(), &tokens(dir.path(), ""), None)).unwrap()
        };
        assert_eq!(
            arch(pinned.builder_pod(&sandbox, &id).unwrap()).as_deref(),
            Some("amd64")
        );
    }

    #[test]
    fn unpack_keeps_files_and_dirs_inside_dest_only() {
        let mut b = tar::Builder::new(Vec::new());
        let mut file = |path: &str, body: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(&mut h, path, body).unwrap();
        };
        file("out/app", b"bin");
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "out/passwd", "/etc/passwd").unwrap();
        let mut raw = b.into_inner().unwrap();
        let mut evil = tar::Header::new_old();
        evil.as_old_mut().name[..8].copy_from_slice(b"../evil\0");
        evil.set_size(1);
        evil.set_entry_type(tar::EntryType::Regular);
        evil.set_cksum();
        raw.truncate(raw.len() - 1024);
        raw.extend_from_slice(evil.as_bytes());
        raw.extend_from_slice(&[b'x'; 512]);
        raw.extend_from_slice(&[0; 1024]);

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("b1");
        std::fs::create_dir(&dest).unwrap();
        assert_eq!(unpack(&raw, &dest).unwrap(), ["out/app"]);
        assert_eq!(std::fs::read(dest.join("out/app")).unwrap(), b"bin");
        assert!(std::fs::symlink_metadata(dest.join("out/passwd")).is_err());
        assert!(!dir.path().join("evil").exists());
    }

    // a tools/call POST with no initialize and no session
    async fn call(
        addr: &str,
        token: &str,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args },
        });
        let auth = format!("Authorization: Bearer {token}\r\n");
        let (status, reply) = post(addr, "127.0.0.1", &auth, &body.to_string()).await;
        assert_eq!(status, 200, "{reply}");
        let (_, body) = reply.split_once("\r\n\r\n").unwrap();
        let reply: serde_json::Value = serde_json::from_str(body).unwrap();
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        match reply["result"]["isError"].as_bool() {
            Some(true) => Err(text),
            _ => Ok(serde_json::from_str(&text).unwrap()),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: BUILDIT_E2E_KUBECONTEXT=kind-x cargo test -- --ignored"]
    async fn e2e_build_poll_restart_run_fetch_logs_and_clean() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("BUILDIT_E2E_KUBECONTEXT").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(work.join("svc")).unwrap();
        std::fs::write(
            work.join("svc/Dockerfile"),
            "FROM docker.io/library/busybox:latest AS base\n\
             RUN echo hello-from-build && sleep 20 && mkdir -p /out && echo artifact > /out/a.txt\n\
             FROM base AS broken\n\
             RUN exit 1\n",
        )
        .unwrap();
        let file = tokens(&work, "tok-a sb-a\ntok-b sb-b\n");
        let cluster = || async {
            let client = crate::client_for(Some(&context)).await.unwrap();
            broker(client, &file, Some(&work))
        };

        let (addr, first) = serve(cluster().await).await;
        let args = serde_json::json!({ "context": "svc", "build_target": "base" });
        let built = call(&addr, "tok-a", "build", args).await.unwrap();
        assert_eq!(built["status"], "running", "{built}");
        let id = built["build_id"].as_str().unwrap().to_string();
        let args = serde_json::json!({ "context": "svc", "target": "broken" });
        let broken = call(&addr, "tok-a", "build", args).await.unwrap();
        assert_eq!(broken["status"], "running", "{broken}");
        let broken = broken["build_id"].as_str().unwrap().to_string();

        let only_id = serde_json::json!({ "build_id": id });
        let status = call(&addr, "tok-a", "status", only_id.clone())
            .await
            .unwrap();
        assert_eq!(status["status"], "running", "{status}");
        assert_eq!(status["exit"], serde_json::Value::Null, "{status}");
        let run = serde_json::json!({ "build_id": id, "cmd": ["true"] });
        let e = call(&addr, "tok-a", "run", run).await.unwrap_err();
        assert!(e.contains("still building"), "{e}");
        first.abort();

        // a new server process with no memory of the builds
        let (addr, _second) = serve(cluster().await).await;
        let a = |tool, args| call(&addr, "tok-a", tool, args);
        let b = |tool, args| call(&addr, "tok-b", tool, args);
        let finished = |build_id: String| {
            let addr = addr.clone();
            async move {
                let args = serde_json::json!({ "build_id": build_id });
                for _ in 0..150 {
                    let status = call(&addr, "tok-a", "status", args.clone()).await.unwrap();
                    if status["status"] != "running" {
                        return status;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                panic!("build {build_id} still running after 300s");
            }
        };
        let done = finished(id.clone()).await;
        assert_eq!(done["status"], "succeeded", "{done}");
        assert_eq!(done["exit"], 0, "{done}");
        assert_eq!(done["timed_out"], false, "{done}");
        let tail = done["log_tail"].as_str().unwrap();
        assert!(tail.contains("hello-from-build"), "{done}");
        let failed = finished(broken.clone()).await;
        assert_eq!(failed["status"], "failed", "{failed}");
        assert_ne!(failed["exit"], 0, "{failed}");
        assert!(failed["exit"].is_i64(), "{failed}");
        let run = serde_json::json!({ "build_id": broken, "cmd": ["true"] });
        let e = a("run", run).await.unwrap_err();
        assert!(e.contains("failed (exit"), "{e}");
        let args = serde_json::json!({
            "build_id": id,
            "cmd": ["sh", "-c", "echo out-1; exit 5"],
            "fetch_paths": ["/out/a.txt", "/nope"],
        });
        let ran = a("run", args).await.unwrap();
        assert_eq!(ran["exit"], 5, "{ran}");
        assert_eq!(ran["log"], "run:1");
        assert!(ran["log_tail"].as_str().unwrap().contains("out-1"), "{ran}");
        let fetched = format!(".buildit/{id}/out/a.txt");
        assert_eq!(ran["fetched"], serde_json::json!([fetched]));
        let body = std::fs::read_to_string(work.join(&fetched)).unwrap();
        assert_eq!(body, "artifact\n");

        let args = serde_json::json!({ "build_id": id, "grep": "^hello-from-build$" });
        let grep = a("logs", args).await.unwrap();
        assert!(
            grep["lines"]
                .as_str()
                .unwrap()
                .ends_with(":hello-from-build\n")
        );
        assert_eq!(grep["lines"].as_str().unwrap().lines().count(), 1);
        assert_eq!(grep["matched"], 1, "{grep}");
        assert_eq!(grep["truncated_head"], false, "{grep}");
        let args = serde_json::json!({ "build_id": id, "which": "run:1", "to_line": 1 });
        let line = a("logs", args).await.unwrap();
        assert!(
            line["lines"]
                .as_str()
                .unwrap()
                .starts_with("1:$ buildah from"),
            "{line}"
        );
        let args = serde_json::json!({ "build_id": id, "which": "run:1", "grep": "^out-1$", "context": 1 });
        let ctx = a("logs", args).await.unwrap();
        assert_eq!(ctx["matched"], 1, "{ctx}");
        let ctx = ctx["lines"].as_str().unwrap();
        assert!(
            ctx.contains(":out-1\n") && ctx.contains("-$ buildah run"),
            "{ctx}"
        );
        let args = serde_json::json!({ "build_id": id, "which": "run:2" });
        assert!(a("logs", args.clone()).await.is_err());
        let again = serde_json::json!({ "build_id": id, "cmd": ["sh", "-c", "echo out-2"] });
        let ran = a("run", again).await.unwrap();
        assert_eq!(ran["exit"], 0, "{ran}");
        assert_eq!(ran["log"], "run:2");
        let second = a("logs", args).await.unwrap();
        assert!(
            second["lines"].as_str().unwrap().contains(":out-2\n"),
            "{second}"
        );

        let run = serde_json::json!({ "build_id": id, "cmd": ["true"] });
        for (tool, args) in [
            ("run", run),
            ("logs", only_id.clone()),
            ("status", only_id.clone()),
        ] {
            let e = b(tool, args).await.unwrap_err();
            assert!(e.contains("no live build"), "{tool}: {e}");
        }
        let cleaned = b("clean", only_id.clone()).await.unwrap();
        assert_eq!(cleaned["deleted"], serde_json::json!([]));
        let cleaned = a("clean", serde_json::json!({ "build_id": broken }))
            .await
            .unwrap();
        assert_eq!(cleaned["deleted"], serde_json::json!([broken]));
        let cleaned = a("clean", serde_json::json!({})).await.unwrap();
        assert_eq!(cleaned["deleted"], serde_json::json!([id]));
        for tool in ["logs", "status"] {
            let e = a(tool, only_id.clone()).await.unwrap_err();
            assert!(e.contains("no live build"), "{tool}: {e}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: BUILDIT_E2E_KUBECONTEXT=kind-x cargo test -- --ignored"]
    async fn e2e_pod_deadline_reports_failed_timed_out() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("BUILDIT_E2E_KUBECONTEXT").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(work.join("svc")).unwrap();
        std::fs::write(
            work.join("svc/Dockerfile"),
            "FROM docker.io/library/busybox:latest\nRUN echo deadline-build && sleep 600\n",
        )
        .unwrap();
        let file = tokens(&work, "tok-a sb-a\n");
        let client = crate::client_for(Some(&context)).await.unwrap();
        let deadline = 120;
        let broker = broker_with_deadline(client.clone(), &file, Some(&work), deadline);
        let pods: kube::Api<Pod> = kube::Api::namespaced(client, &broker.namespace);
        let (addr, _task) = serve(broker).await;
        let a = |tool, args| call(&addr, "tok-a", tool, args);

        let built = a("build", serde_json::json!({ "context": "svc" }))
            .await
            .unwrap();
        let id = built["build_id"].as_str().unwrap().to_string();
        let only_id = serde_json::json!({ "build_id": id });

        let mut status = serde_json::Value::Null;
        for _ in 0..deadline {
            status = a("status", only_id.clone()).await.unwrap();
            if status["status"] != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(status["status"], "failed", "{status}");
        assert_eq!(status["timed_out"], true, "{status}");
        assert_eq!(status["exit"], 124, "{status}");
        let phase = pods.get(&id).await.unwrap().status.unwrap().phase;
        assert_eq!(
            phase.as_deref(),
            Some("Running"),
            "the build timeout fired first"
        );

        let mut expired = false;
        for _ in 0..120 {
            let pod = pods.get(&id).await.unwrap();
            if pod.status.and_then(|s| s.reason).as_deref() == Some("DeadlineExceeded") {
                expired = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert!(expired, "pod {id} never hit its deadline");

        let ended = a("status", only_id.clone()).await.unwrap();
        assert_eq!(ended["status"], "failed", "{ended}");
        assert_eq!(ended["timed_out"], true, "{ended}");
        assert_eq!(ended["exit"], 124, "{ended}");
        let tail = ended["log_tail"].as_str().unwrap();
        assert!(tail.contains(":$ buildah bud"), "{ended}");
        assert!(tail.contains("deadline-build"), "{ended}");

        let grep = serde_json::json!({ "build_id": id, "grep": "^deadline-build$" });
        let grep = a("logs", grep).await.unwrap();
        assert_eq!(grep["matched"], 1, "{grep}");
        let run = serde_json::json!({ "build_id": id, "which": "run:1" });
        let e = a("logs", run).await.unwrap_err();
        assert!(e.contains("only its build log"), "{e}");
        let run = serde_json::json!({ "build_id": id, "cmd": ["true"] });
        let e = a("run", run).await.unwrap_err();
        assert!(e.contains("has ended"), "{e}");

        let cleaned = a("clean", only_id.clone()).await.unwrap();
        assert_eq!(cleaned["deleted"], serde_json::json!([id]));
        let e = a("status", only_id).await.unwrap_err();
        assert!(e.contains("no live build"), "{e}");
    }

    const SLEEPERS: &str = r#"n=0; for p in /proc/[0-9]*; do
  case "$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null)" in "sleep 600 "*) n=$((n + 1)) ;; esac
done; echo "$n""#;

    type TaskClient = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

    async fn connect_with_tasks(addr: &str, token: &str) -> TaskClient {
        use rmcp::model::{ClientCapabilities, ClientConfig, Implementation, ProtocolVersion};
        use rmcp::service::{ClientLifecycleMode, ClientServiceExt};
        use rmcp::transport::StreamableHttpClientTransport;
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
        let url = format!("http://{addr}{MCP_PATH}");
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
        );
        ClientConfig::new(
            ClientCapabilities::builder().enable_tasks().build(),
            Implementation::from_build_env(),
        )
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap()
    }

    async fn start_task(client: &TaskClient, tool: &str, args: serde_json::Value) -> String {
        use rmcp::model::{CallToolRequestParams, CallToolResponse, TaskStatus};
        let serde_json::Value::Object(args) = args else {
            panic!("args must be an object")
        };
        let params = CallToolRequestParams::new(tool.to_string()).with_arguments(args);
        match client.call_tool_once(params).await.unwrap() {
            CallToolResponse::Task(created) => {
                assert_eq!(created.task.status, TaskStatus::Working);
                assert_eq!(
                    created.task.poll_interval_ms,
                    Some(crate::task::POLL_INTERVAL_MS)
                );
                created.task.task_id
            }
            other => panic!("expected a task from {tool}, got {other:?}"),
        }
    }

    async fn get_task(
        client: &TaskClient,
        id: &str,
    ) -> Result<rmcp::model::DetailedTask, rmcp::ServiceError> {
        let params = rmcp::model::GetTaskParams::new(id.to_string());
        client.peer().get_task(params).await.map(|r| r.task)
    }

    async fn cancel_task(client: &TaskClient, id: &str) -> Result<(), rmcp::ServiceError> {
        let params = rmcp::model::CancelTaskParams::new(id.to_string());
        client.peer().cancel_task(params).await.map(|_| ())
    }

    async fn poll(
        client: &TaskClient,
        id: &str,
        limit: std::time::Duration,
    ) -> rmcp::model::DetailedTask {
        let start = std::time::Instant::now();
        loop {
            let task = get_task(client, id).await.unwrap();
            if task.status().is_terminal() {
                return task;
            }
            assert!(
                start.elapsed() < limit,
                "task {id} never finished: {task:?}"
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    // (isError, the result's text)
    fn completed(task: &rmcp::model::DetailedTask) -> (bool, String) {
        let rmcp::model::TaskPayload::Completed { result } = &task.payload else {
            panic!("expected completed, got {task:?}")
        };
        let result: rmcp::model::CallToolResult =
            serde_json::from_value(serde_json::Value::Object(result.clone())).unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        (result.is_error.unwrap_or(false), text)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: BUILDIT_E2E_KUBECONTEXT=kind-x cargo test -- --ignored"]
    async fn e2e_tasks_build_run_cancel_across_restarts_and_isolate_sandboxes() {
        use std::time::Duration;

        use rmcp::model::TaskStatus;

        use crate::pod::BuilderPod;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("BUILDIT_E2E_KUBECONTEXT").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(work.join("svc")).unwrap();
        std::fs::write(
            work.join("svc/Dockerfile"),
            "FROM docker.io/library/busybox:latest AS base\n\
             RUN echo hello-from-task && mkdir -p /out && echo artifact > /out/a.txt\n\
             FROM base AS slow\n\
             RUN echo slow-build && sleep 600\n",
        )
        .unwrap();
        let file = tokens(&work, "tok-a sb-a\ntok-b sb-b\n");
        let client = crate::client_for(Some(&context)).await.unwrap();
        let cluster = || broker(client.clone(), &file, Some(&work));

        let (addr, first) = serve(cluster()).await;
        let a = connect_with_tasks(&addr, "tok-a").await;
        let args = serde_json::json!({ "context": "svc", "target": "base" });
        let build = start_task(&a, "build", args).await;
        let id = build.strip_prefix("build/").unwrap().to_string();
        assert!(BuildId::parse(&id).is_ok(), "{build}");
        let built = poll(&a, &build, Duration::from_secs(600)).await;
        let (is_error, text) = completed(&built);
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(!is_error, "{body}");
        assert_eq!(body["build_id"], id.as_str());
        assert_eq!(body["status"], "succeeded", "{body}");
        assert_eq!(body["exit"], 0, "{body}");
        assert!(
            body["log_tail"]
                .as_str()
                .unwrap()
                .contains("hello-from-task")
        );
        assert_eq!(get_task(&a, &build).await.unwrap(), built);

        let args = serde_json::json!({
            "build_id": id,
            "cmd": ["sh", "-c", "echo slow-start; sleep 600"],
        });
        let slow = start_task(&a, "run", args).await;
        assert_eq!(slow, format!("run/{id}/1"));
        first.abort();

        // a new server process with no memory of the tasks
        let (addr, _second) = serve(cluster()).await;
        let a = connect_with_tasks(&addr, "tok-a").await;
        let b = connect_with_tasks(&addr, "tok-b").await;
        assert_eq!(
            get_task(&a, &slow).await.unwrap().status(),
            TaskStatus::Working
        );
        for task in [&build, &slow] {
            assert!(get_task(&b, task).await.is_err(), "{task}");
            assert!(cancel_task(&b, task).await.is_err(), "{task}");
        }
        assert_eq!(
            get_task(&a, &slow).await.unwrap().status(),
            TaskStatus::Working
        );
        for bad in [
            format!("run/{id}/99"),
            format!("run/{id}/01"),
            "build/buildit-0000000000000000".to_string(),
            "nope".to_string(),
        ] {
            assert!(get_task(&a, &bad).await.is_err(), "{bad}");
            assert!(cancel_task(&a, &bad).await.is_err(), "{bad}");
        }

        cancel_task(&a, &slow).await.unwrap();
        let cancelled = poll(&a, &slow, Duration::from_secs(5)).await;
        assert_eq!(cancelled.status(), TaskStatus::Cancelled);
        cancel_task(&a, &slow).await.unwrap();
        cancel_task(&a, &build).await.unwrap();
        assert_eq!(
            get_task(&a, &slow).await.unwrap().status(),
            TaskStatus::Cancelled
        );
        assert_eq!(get_task(&a, &build).await.unwrap(), built);
        let pod = BuilderPod::existing(client.clone(), client.default_namespace(), &id);
        let containers = ["buildah", "containers", "--quiet"].map(String::from);
        let start = std::time::Instant::now();
        while !pod
            .exec_capture(&containers)
            .await
            .unwrap()
            .trim()
            .is_empty()
        {
            assert!(start.elapsed() < Duration::from_secs(60), "container left");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let ps = ["sh", "-c", SLEEPERS].map(String::from);
        assert_eq!(pod.exec_capture(&ps).await.unwrap().trim(), "0");

        let args = serde_json::json!({
            "build_id": id,
            "cmd": ["sh", "-c", "echo out-2; exit 5"],
            "fetch_paths": ["/out/a.txt", "/nope"],
        });
        let fetch = start_task(&a, "run", args).await;
        assert_eq!(fetch, format!("run/{id}/2"));
        let start = std::time::Instant::now();
        let finals = loop {
            let (x, y, z) = tokio::join!(
                get_task(&a, &fetch),
                get_task(&a, &fetch),
                get_task(&a, &fetch)
            );
            let all = [x.unwrap(), y.unwrap(), z.unwrap()];
            if all.iter().all(|t| t.status().is_terminal()) {
                break all;
            }
            assert!(start.elapsed() < Duration::from_secs(300), "{all:?}");
            tokio::time::sleep(Duration::from_secs(1)).await;
        };
        assert_eq!(finals[0], finals[1]);
        assert_eq!(finals[1], finals[2]);
        let (is_error, text) = completed(&finals[0]);
        let ran: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(is_error, "{ran}");
        assert_eq!(ran["exit"], 5, "{ran}");
        assert_eq!(ran["timed_out"], false, "{ran}");
        assert_eq!(ran["log"], "run:2");
        assert!(ran["log_tail"].as_str().unwrap().contains("out-2"), "{ran}");
        let fetched = format!(".buildit/{id}/out/a.txt");
        assert_eq!(ran["fetched"], serde_json::json!([fetched]));
        let body = std::fs::read_to_string(work.join(&fetched)).unwrap();
        assert_eq!(body, "artifact\n");
        assert_eq!(get_task(&a, &fetch).await.unwrap(), finals[0]);

        let args = serde_json::json!({ "build_id": id, "cmd": ["sleep", "60"], "timeout_s": 3 });
        let late = start_task(&a, "run", args).await;
        let timed = poll(&a, &late, Duration::from_secs(120)).await;
        let (is_error, text) = completed(&timed);
        let ran: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(is_error, "{ran}");
        assert_eq!(ran["timed_out"], true, "{ran}");
        assert_eq!(ran["exit"], 124, "{ran}");

        let args = serde_json::json!({ "build_id": id, "which": "run:1" });
        let log = call(&addr, "tok-a", "logs", args).await.unwrap();
        assert!(
            log["lines"].as_str().unwrap().contains(":slow-start\n"),
            "{log}"
        );

        let args = serde_json::json!({ "context": "svc", "target": "slow" });
        let slow_build = start_task(&a, "build", args).await;
        let slow_id = slow_build.strip_prefix("build/").unwrap().to_string();
        let only_slow = serde_json::json!({ "build_id": slow_id });
        let start = std::time::Instant::now();
        loop {
            let args = serde_json::json!({ "build_id": slow_id, "grep": "^slow-build$" });
            let seen = call(&addr, "tok-a", "logs", args).await.unwrap();
            if seen["matched"] == 1 {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(300), "{seen}");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        cancel_task(&a, &slow_build).await.unwrap();
        let cancelled = poll(&a, &slow_build, Duration::from_secs(10)).await;
        assert_eq!(cancelled.status(), TaskStatus::Cancelled);
        let status = call(&addr, "tok-a", "status", only_slow.clone())
            .await
            .unwrap();
        assert_eq!(status["status"], "cancelled", "{status}");
        assert_eq!(status["exit"], serde_json::Value::Null, "{status}");
        let run = serde_json::json!({ "build_id": slow_id, "cmd": ["true"] });
        let e = call(&addr, "tok-a", "run", run).await.unwrap_err();
        assert!(e.contains("was cancelled"), "{e}");
        let slow_pod = BuilderPod::existing(client.clone(), client.default_namespace(), &slow_id);
        let ps = ["sh", "-c", SLEEPERS].map(String::from);
        let start = std::time::Instant::now();
        while slow_pod.exec_capture(&ps).await.unwrap().trim() != "0" {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "build still running"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        let cleaned = call(&addr, "tok-a", "clean", serde_json::json!({}))
            .await
            .unwrap();
        let mut both = vec![id.clone(), slow_id.clone()];
        both.sort();
        assert_eq!(cleaned["deleted"], serde_json::json!(both));
        assert!(get_task(&a, &build).await.is_err());
        assert!(cancel_task(&a, &fetch).await.is_err());
    }
}
