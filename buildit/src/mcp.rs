#![allow(clippy::disallowed_macros)]

use std::collections::BTreeMap;
use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use clap::Args;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::Api;
use kube::api::{DeleteParams, ListParams, LogParams, ObjectMeta, Patch, PatchParams, PostParams};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResponse, CallToolResult, CancelTaskParams, ContentBlock, CreateTaskResult,
    DetailedTask, GetTaskParams, GetTaskResult, Implementation, ServerCapabilities, ServerConfig,
    Task, TaskPayload, TaskStatus,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::backend::{Resources, StorageDriver};
use crate::context::ContextDigest;
use crate::jobs;
use crate::logs::{Log, LogName, MAX_READ, Page, Query};
use crate::sandbox::{RESULTS_DIR, RelPath, SandboxDir, SandboxName, Workspace, is_label_value};
use crate::step::{self, Fetch, Published, Snapshot, Status};
use crate::task::{self, Progress, TaskRef};
use crucible_mcp_auth::access::{AccessCheck, AccessRules, Call};
use crucible_mcp_auth::guard::PolicyGuard;
use crucible_mcp_auth::mount::{Mount, mcp_router, stateless_config};
use crucible_mcp_auth::policy::Refusal;
use crucible_mcp_auth::policy::ToolPolicy;
use crucible_mcp_auth::principal::{Callers, Principal};
use crucible_mcp_auth::server::{
    AuthError, JwksTls, ResourceServer, ResourceServerConfig, TrustedIssuer, bearer_token,
};

pub const MCP_PATH: &str = "/mcp";

/// OpenShift's internal registry, where builds push unless told otherwise.
const OPENSHIFT_REGISTRY: &str = "image-registry.openshift-image-registry.svc:5000";

/// How build pods authenticate to the registry their images go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RegistryAuth {
    /// The cluster's own registry: the pod's service account token, the cluster's service CA.
    Cluster,
    /// Anonymous, over plain HTTP.
    Insecure,
}

const SANDBOX_HOSTS: [&str; 2] = ["host.containers.internal", "host.openshell.internal"];

const LABEL_MANAGED_BY: &str = "buildit.dev/managed-by";
const MANAGED_BY: &str = "mcp";
const LABEL_SANDBOX: &str = "buildit.dev/sandbox";
const LABEL_BUILD_ID: &str = "buildit.dev/build-id";
/// The person a realm token acted for, on every builder pod it starts.
const LABEL_LAUNCHED_BY: &str = "crucible.dev/launched-by";
/// The [`ContextDigest`] of what buildit uploaded for a builder pod's first build.
const ANNOTATION_CONTEXT_DIGEST: &str = "buildit.dev/context-digest";

/// Scope that lets a token read build status and logs.
pub const SCOPE_READ: &str = "buildit.read";
/// Scope that lets a token build, run, and clean.
pub const SCOPE_BUILD: &str = "buildit.build";

const DEFAULT_BUILD_TIMEOUT_S: u64 = 1800;
const DEFAULT_RUN_TIMEOUT_S: u64 = 600;
const FROM_TIMEOUT: Duration = Duration::from_secs(300);
const EXEC_SLACK: Duration = Duration::from_secs(60);
const SETTLE_RETRY: Duration = Duration::from_secs(2);
const SUMMARY_LINES: usize = 40;
const SUMMARY_BYTES: usize = 8 * 1024;
const SUMMARY_WINDOW: u64 = 256 * 1024;
const LIST_TRIES: u32 = 4;
/// The label the Job controller puts on a Job's pods.
const JOB_NAME_LABEL: &str = "batch.kubernetes.io/job-name";
/// Where a build pod reads its inputs (`jobs::build_job`).
const INPUTS_DIR: &str = "/buildit-inputs";
/// Time a build has beyond its own timeout to restore and save its cluster caches.
const RESTORE_SAVE_SECS: i64 = 1800;
const PUSH_TIMEOUT: Duration = Duration::from_secs(1800);
/// Where declared caches live in a builder pod, on its storage volume.
const CACHE_ROOT: &str = "/home/build/.local/share/containers/buildit-caches";
const MAX_CACHES: usize = 8;
/// Mounted read-write into every RUN step of a build, for it to leave `facts.json` in.
pub(crate) const FACTS_MOUNT: &str = "/buildit";
const CCACHE_CONF: &str = "ccache.conf";
const CCACHE_STATS: &str = "ccache-stats.log";
/// Where in the facts mount a build's steps are stamped (`step::build_args`).
const STEPS_LOG: &str = "steps.log";
const CCACHE_USER: &str = "buildit";
const MAX_FACTS: u64 = 64 * 1024;
const DEFAULT_LOG_BYTES: u64 = 16 * 1024;
const MAX_LOG_BYTES: u64 = 64 * 1024;

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
    /// Namespace builds run in (default: the client's)
    #[arg(short, long)]
    pub namespace: Option<String>,
    /// The longest any build, run or push Job runs, in seconds (activeDeadlineSeconds)
    #[arg(long, env = "MCP_POD_DEADLINE", default_value_t = 14400)]
    pub pod_deadline: i64,
    /// Seconds Kubernetes keeps an ended Job before deleting it
    #[arg(long, env = "MCP_JOB_TTL", default_value_t = 3600)]
    pub job_ttl: i32,
    /// Repository builds push their images to (default: the OpenShift internal registry,
    /// image-registry.openshift-image-registry.svc:5000/<namespace>/buildit)
    #[arg(long, env = "MCP_IMAGE_REPO")]
    pub image_repo: Option<String>,
    /// How build pods reach the image repository's registry
    #[arg(long, env = "MCP_REGISTRY_AUTH", value_enum, default_value_t = RegistryAuth::Cluster)]
    pub registry_auth: RegistryAuth,
    /// Whose user ids build and run pods run under
    #[arg(long, env = "MCP_POD_USERS", value_enum, default_value_t = jobs::PodUsers::Namespaced)]
    pub pod_users: jobs::PodUsers,
    /// Directory this server keeps ended builds' and runs' logs in
    #[arg(long, env = "MCP_LOG_DIR", default_value = "/tmp/buildit-mcp-logs")]
    pub log_dir: PathBuf,
    /// containers-storage driver of build pods
    #[arg(long, env = "MCP_STORAGE_DRIVER", value_enum, default_value_t = StorageDriver::Vfs)]
    pub storage_driver: StorageDriver,
    /// Extra `buildah bud` argument for every Dockerfile build, after buildit's own (repeatable)
    #[arg(long = "bud-arg", allow_hyphen_values = true)]
    pub bud_args: Vec<String>,
    /// Builds per sandbox that may be running at once; `build` is refused past this
    #[arg(long, default_value_t = 4)]
    pub max_builds: usize,
    /// Run builds only on nodes of this architecture (kubernetes.io/arch)
    #[arg(long)]
    pub node_arch: Option<String>,
    /// Kueue LocalQueue in the builder namespace that build Jobs are submitted to; a build is
    /// queued until Kueue admits it
    #[arg(long, env = "MCP_QUEUE")]
    pub queue: Option<String>,
    /// Resource requests for build and run pods, repeatable: --request cpu=2
    #[arg(long = "request", value_name = "KEY=QTY", value_parser = crate::parse_kv)]
    pub requests: Vec<(String, String)>,
    /// Resource limits for build and run pods, repeatable: --limit memory=8Gi
    #[arg(long = "limit", value_name = "KEY=QTY", value_parser = crate::parse_kv)]
    pub limits: Vec<(String, String)>,
    /// The run this server serves, compared with a run token's `run_id`; a token for any other run
    /// is forbidden
    #[arg(long, env = "MCP_RUN_ID", requires = "run_issuers")]
    pub run_id: Option<String>,
    /// The one repository `push` publishes builds to, e.g. quay.io/org/smoke; unset, `push` is
    /// refused. Tags are buildit's: <sandbox>-<build_id>
    #[arg(long, env = "MCP_PUSH_REPO", requires = "push_authfile")]
    pub push_repo: Option<String>,
    /// Registry authfile `push` uses, read on each push and piped to the builder pod for it alone
    #[arg(long, env = "MCP_PUSH_AUTHFILE", requires = "push_repo")]
    pub push_authfile: Option<PathBuf>,
    /// A `buildit cache` every build reads, named to it in /buildit/cache.json
    #[arg(long, env = "MCP_CACHE_URL")]
    pub cache_url: Option<url::Url>,
    /// The cache's write token, read again for each use. Builds need it: this server uploads
    /// their contexts with it, and their pods archive their results with it
    #[arg(long, env = "MCP_CACHE_TOKEN_FILE", requires = "cache_url")]
    pub cache_token_file: Option<PathBuf>,
    /// The builds that get the write token
    #[arg(long, env = "MCP_CACHE_WRITERS", value_enum, default_value_t = CacheWriters::Run)]
    pub cache_writers: CacheWriters,
    /// Also serve the tools on this unix socket, without tokens, to one caller: the run itself.
    /// Its builds are apart from every sandbox's, under the same policy and limits
    #[arg(long, env = "MCP_SOCKET", requires = "socket_workdir")]
    pub socket: Option<PathBuf>,
    /// The directory the socket's builds read their contexts from and `run` publishes into
    #[arg(long, env = "MCP_SOCKET_WORKDIR", requires = "socket")]
    pub socket_workdir: Option<PathBuf>,
}

/// The sandbox name the run itself builds under on the socket: `loop-` and a hash of the run,
/// which no openshell sandbox (`ci-<hex>`) takes and no other run shares.
pub fn run_sandbox(run_id: Option<&str>) -> Result<SandboxName> {
    use sha2::Digest;
    match run_id {
        None => SandboxName::parse("loop"),
        Some(run) => {
            let hash: String = sha2::Sha256::digest(run.as_bytes())[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            SandboxName::parse(&format!("loop-{hash}"))
        }
    }
}

fn load_policy(text: Option<&str>) -> Result<ToolPolicy> {
    match text {
        Some(text) => ToolPolicy::from_toml(text).context("loading the tool policy"),
        None => Ok(ToolPolicy::default()),
    }
}

/// Binds the run's socket, replacing a stale one, readable and writable by this user alone.
fn bind_socket(path: &Path) -> Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(path)
            .with_context(|| format!("removing the stale socket {}", path.display()))?,
        Ok(_) => bail!("{} exists and is not a socket", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))?;
    Ok(listener)
}

pub async fn serve(client: kube::Client, args: McpArgs) -> Result<()> {
    let namespace = args
        .namespace
        .unwrap_or_else(|| client.default_namespace().to_string());
    if let Some(q) = &args.queue
        && !is_label_value(q)
    {
        bail!("queue {q:?} is not a label value");
    }
    let oidc = match args.run_issuers.is_empty() {
        true => None,
        false => {
            let config = ResourceServerConfig {
                issuers: args.run_issuers,
                resources: args.resources,
                required_scopes: vec![SCOPE_READ.to_string()],
                scopes_supported: vec![SCOPE_READ.to_string(), SCOPE_BUILD.to_string()],
                resource_name: Some("buildit".to_string()),
                cluster: None,
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
    let policy_text = match &args.tool_policy {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("reading tool policy {}", path.display()))?,
        ),
        None => None,
    };
    let policy = load_policy(policy_text.as_deref())?;
    let socket = match (args.socket, args.socket_workdir) {
        (Some(path), Some(dir)) => Some((
            path,
            Caller {
                sandbox: run_sandbox(args.run_id.as_deref())?,
                workspace: Workspace::Local { dir },
                launched_by: None,
            },
        )),
        _ => None,
    };
    let image_repo = args
        .image_repo
        .unwrap_or_else(|| format!("{OPENSHIFT_REGISTRY}/{namespace}/buildit"));
    let host = image_repo.split('/').next().unwrap_or_default().to_string();
    let registry = match args.registry_auth {
        RegistryAuth::Cluster => jobs::Registry::Cluster { host },
        RegistryAuth::Insecure => jobs::Registry::Insecure { host },
    };
    let shared_cache = args.cache_url.map(|url| SharedCache {
        url,
        token_file: args.cache_token_file,
        writers: args.cache_writers,
    });
    let site = jobs::Site {
        namespace,
        queue: args.queue,
        node_arch: args.node_arch,
        resources: Resources {
            requests: args.requests,
            limits: args.limits,
        },
        storage: args.storage_driver,
        bud_args: args.bud_args,
        users: args.pod_users,
        deadline_secs: args.pod_deadline,
        ttl_secs: args.job_ttl,
        cache: shared_cache
            .as_ref()
            .map(SharedCache::base)
            .unwrap_or_default(),
        image_repo,
        registry,
    };
    let broker = Arc::new(Broker {
        client,
        tokens_file: args.tokens_file,
        oidc,
        default_workdir: args
            .sandbox_workdir
            .as_deref()
            .map(SandboxDir::parse)
            .transpose()?,
        local_workdir: None,
        site,
        max_builds: args.max_builds,
        log_dir: args.log_dir,
        http: reqwest::Client::new(),
        publishing: tokio::sync::Mutex::new(()),
        run_id: args.run_id,
        push: match (args.push_repo, args.push_authfile) {
            (Some(repo), Some(authfile)) => Some(PushTarget::new(repo, authfile)?),
            _ => None,
        },
        shared_cache,
        run_caller: socket.as_ref().map(|(_, caller)| caller.sandbox.clone()),
    });
    tokio::spawn(broker.clone().keep_logs());
    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    tracing::info!(
        "buildit mcp listening on http://{}{MCP_PATH} (namespace {})",
        args.bind,
        broker.site.namespace
    );
    let http = axum::serve(
        listener,
        router(broker.clone(), &args.allowed_hosts, policy),
    )
    .into_future();
    let Some((path, caller)) = socket else {
        return http.await.context("serving buildit mcp");
    };
    let unix = bind_socket(&path)?;
    tracing::info!(
        "buildit mcp serving run caller {} on {}",
        caller.sandbox.as_str(),
        path.display()
    );
    let policy = load_policy(policy_text.as_deref())?;
    let local = axum::serve(unix, socket_router(broker, caller, policy)).into_future();
    tokio::try_join!(http, local)
        .map(|_| ())
        .context("serving buildit mcp")
}

/// The MCP endpoint behind authentication and the guard: [`BuilditRules`] decide which calls
/// exist and their scopes, and `policy` adds validate and mutate rules.
pub fn router(broker: Arc<Broker>, extra_hosts: &[String], policy: ToolPolicy) -> axum::Router {
    let hosts: Vec<String> = SANDBOX_HOSTS
        .iter()
        .map(|h| h.to_string())
        .chain(extra_hosts.iter().cloned())
        .collect();
    let oidc = broker.oidc.clone();
    mcp_router(
        tools(&broker),
        axum::middleware::from_fn_with_state(broker, authenticate),
        mount(&hosts, policy, oidc),
    )
}

/// The same tools and guard on the run's own socket, every request made as `caller`.
pub fn socket_router(broker: Arc<Broker>, caller: Caller, policy: ToolPolicy) -> axum::Router {
    mcp_router(
        tools(&broker),
        axum::middleware::from_fn_with_state(Arc::new(caller), as_caller),
        mount(&[], policy, None),
    )
}

async fn as_caller(State(caller): State<Arc<Caller>>, mut req: Request, next: Next) -> Response {
    req.extensions_mut().insert(caller.principal());
    req.extensions_mut().insert(Caller::clone(&caller));
    next.run(req).await
}

fn tools(broker: &Arc<Broker>) -> impl Fn() -> std::io::Result<BuilditMcp> + Send + Sync + 'static {
    let broker = broker.clone();
    move || {
        Ok(BuilditMcp {
            broker: broker.clone(),
        })
    }
}

/// The endpoint at [`MCP_PATH`], accepting `hosts` beside loopback, its calls decided by
/// [`BuilditRules`] and `policy`.
fn mount(hosts: &[String], policy: ToolPolicy, server: Option<Arc<ResourceServer>>) -> Mount {
    Mount {
        path: MCP_PATH,
        config: stateless_config(hosts)
            .with_sse_keep_alive(None)
            .with_sse_retry(None),
        guard: PolicyGuard {
            policy,
            rules: Arc::new(BuilditRules),
            server: server.clone(),
            reviewer: None,
        },
        metadata: server,
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

/// The `--volume` mounts of a build's declared caches, after checking them.
fn cache_volumes(caches: &[CacheSpec]) -> Result<Vec<(String, String)>> {
    if caches.len() > MAX_CACHES {
        bail!("a build may declare at most {MAX_CACHES} caches");
    }
    let mut names = std::collections::BTreeSet::new();
    let mut paths = std::collections::BTreeSet::new();
    caches
        .iter()
        .map(|c| {
            let name_ok = (1..=63).contains(&c.name.len())
                && c.name.starts_with(|ch: char| ch.is_ascii_alphanumeric())
                && c.name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !name_ok {
                bail!(
                    "cache name {:?} must be 1-63 lowercase letters, digits or -",
                    c.name
                );
            }
            let path = Path::new(&c.path);
            let clean = c.path.len() <= 255
                && !c.path.contains([':', ',', '\0'])
                && c.path.strip_prefix('/').is_some_and(|rest| {
                    rest.split('/')
                        .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
                });
            if !clean {
                bail!(
                    "cache path {:?} must be a plain absolute path below /",
                    c.path
                );
            }
            if path.starts_with(FACTS_MOUNT) {
                bail!(
                    "cache path {:?} is under {FACTS_MOUNT}, which buildit mounts",
                    c.path
                );
            }
            if !names.insert(c.name.as_str()) || !paths.insert(c.path.as_str()) {
                bail!("cache {:?} at {:?} repeats a name or path", c.name, c.path);
            }
            if !c.key.is_empty() && c.scope != CacheScope::Cluster {
                bail!(
                    "cache {:?} has a key, which only a cluster cache uses",
                    c.name
                );
            }
            for key in &c.key {
                RelPath::parse(key).with_context(|| format!("cache {:?} key", c.name))?;
            }
            let mode = if c.read_only { "ro" } else { "rw" };
            Ok((
                format!("{CACHE_ROOT}/{}", c.name),
                format!("{CACHE_ROOT}/{}:{}:{mode}", c.name, c.path),
            ))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub sandbox: SandboxName,
    pub workspace: Workspace,
    /// The person a realm token acts for; tokens-file callers carry none.
    pub launched_by: Option<Launcher>,
}

/// A realm token's `sub` as a label value: itself when it is one, else its label-safe characters
/// with a hash of the whole appended, so an email subject (`alice@example.com`) still labels its
/// builds and two subjects never share a label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher(String);

impl Launcher {
    pub fn of(raw: &str) -> Self {
        if is_label_value(raw) {
            return Self(raw.to_string());
        }
        use sha2::Digest;
        let hash: String = sha2::Sha256::digest(raw.as_bytes())[..5]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let kept: String = raw
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
                _ => '-',
            })
            .take(63 - hash.len() - 1)
            .collect();
        let kept = kept.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        match kept.is_empty() {
            true => Self(hash),
            false => Self(format!("{kept}-{hash}")),
        }
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
    #[error("sandbox {0} has no workdir and BROKER_SANDBOX_WORKDIR is unset")]
    NoWorkdir(String),
    #[error("buildit serves runs only, not people's tokens")]
    Person,
    #[error("the token is for another run than the one this server serves")]
    OtherRun,
    #[error("sandbox {0} is the run's own caller, served on its socket alone")]
    RunCaller(String),
}

impl Caller {
    /// The principal a caller without a realm token stands for (a tokens-file line, or the run on
    /// its socket), claims shaped like a realm token's so one tool policy covers all of them.
    pub fn principal(&self) -> Principal {
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
    tokens_file: Option<PathBuf>,
    oidc: Option<Arc<ResourceServer>>,
    default_workdir: Option<SandboxDir>,
    local_workdir: Option<PathBuf>,
    site: jobs::Site,
    max_builds: usize,
    log_dir: PathBuf,
    http: reqwest::Client,
    /// Held while a run's fetch_paths are published, so they are published once.
    publishing: tokio::sync::Mutex<()>,
    run_id: Option<String>,
    push: Option<PushTarget>,
    shared_cache: Option<SharedCache>,
    /// The sandbox name the run builds under on its socket, which no token may name.
    run_caller: Option<SandboxName>,
}

/// What a build does with a cluster cache's snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotUse {
    /// Restore it into the cache when the cache is empty.
    Restore,
    /// Restore it, and once the build succeeds save the cache under its key if the key has no
    /// snapshot yet.
    Save,
}

impl SnapshotUse {
    fn as_str(self) -> &'static str {
        match self {
            Self::Restore => "restore",
            Self::Save => "save",
        }
    }
}

/// A build's cluster caches: the snapshot plan, and the header its saves send.
#[derive(Debug, Default, PartialEq, Eq)]
struct Snapshots {
    plan: String,
    auth: Option<String>,
}

/// The builds that may write ccache objects and cache snapshots to the shared cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CacheWriters {
    /// No build: every build reads only.
    None,
    /// The builds the run starts on its socket.
    Run,
    /// The run's builds and every sandbox's.
    All,
}

/// Who started a build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Starter {
    /// The run, on its socket.
    Run,
    Sandbox,
}

/// The `buildit cache` builds are told about, and the token that lets some of them write to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedCache {
    url: url::Url,
    token_file: Option<PathBuf>,
    writers: CacheWriters,
}

impl SharedCache {
    /// /buildit/cache.json: the cache's URL, and the write token if the build may write.
    fn document(&self, starter: Starter) -> Vec<u8> {
        let mut doc = serde_json::json!({ "url": self.url.as_str() });
        if let Some(t) = self.token_for(starter) {
            doc["token"] = serde_json::Value::String(t);
        }
        doc.to_string().into_bytes()
    }

    /// The snapshots of the cluster caches among `caches`, each keyed from the context `tar`. A
    /// build saves those it mounts read-write when `starter` may write to the shared cache.
    fn snapshots(
        &self,
        caches: &[CacheSpec],
        tar: &[u8],
        starter: Starter,
        root: &str,
    ) -> Result<Snapshots> {
        let token = self.token_for(starter);
        let mut snapshots = Snapshots::default();
        for c in caches.iter().filter(|c| c.scope == CacheScope::Cluster) {
            let paths = c
                .key
                .iter()
                .map(|k| RelPath::parse(k))
                .collect::<Result<Vec<_>>>()?;
            let key = crate::context::cache_key(&c.name, tar, &paths)?;
            let used = match (&token, c.read_only) {
                (Some(_), false) => SnapshotUse::Save,
                _ => SnapshotUse::Restore,
            };
            snapshots.plan.push_str(&format!(
                "{name} {key} {root}/{name} {}\n",
                used.as_str(),
                name = c.name
            ));
            if used == SnapshotUse::Save {
                snapshots.auth = token
                    .as_ref()
                    .map(|t| format!("Authorization: Bearer {t}\n"));
            }
        }
        Ok(snapshots)
    }

    /// The cache's URL without a trailing `/`, for the snapshot scripts to append paths to.
    fn base(&self) -> String {
        self.url.as_str().trim_end_matches('/').to_string()
    }

    /// The write token for a build `starter` started, if it may write.
    fn token_for(&self, starter: Starter) -> Option<String> {
        match (self.writers, starter) {
            (CacheWriters::All, _) | (CacheWriters::Run, Starter::Run) => self.token(),
            (CacheWriters::Run, Starter::Sandbox) | (CacheWriters::None, _) => None,
        }
    }

    fn token(&self) -> Option<String> {
        self.token_file
            .as_ref()
            .and_then(|path| match std::fs::read_to_string(path) {
                Ok(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(
                        "reading cache token {}: {e}; builds read only",
                        path.display()
                    );
                    None
                }
            })
    }

    /// ccache's remote storage URL for the cache: the write token as the basic-auth password, or
    /// read-only for a build that may not write.
    fn ccache_storage(&self, starter: Starter) -> String {
        let mut url = self.url.clone();
        url.set_path(&format!("{}/ccache", self.url.path().trim_end_matches('/')));
        match self.token_for(starter) {
            Some(token) => {
                let _ = url.set_username(CCACHE_USER);
                let _ = url.set_password(Some(&token));
                url.to_string()
            }
            None => format!("{url}|read-only"),
        }
    }
}

/// The /etc/ccache.conf of a build that asks for ccache.
fn ccache_conf(cache: Option<&SharedCache>, starter: Starter) -> String {
    let mut conf = format!("stats_log = {FACTS_MOUNT}/{CCACHE_STATS}\n");
    if let Some(cache) = cache {
        conf.push_str(&format!(
            "secondary_storage = {}\n",
            cache.ccache_storage(starter)
        ));
    }
    conf
}

/// Where `push` publishes: one repository the operator chose, and the authfile for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    repo: String,
    authfile: PathBuf,
}

impl PushTarget {
    pub fn new(repo: String, authfile: PathBuf) -> Result<Self> {
        let named = repo.contains('/')
            && !repo.contains("://")
            && !repo.contains('@')
            && !repo.rsplit('/').next().unwrap_or_default().contains(':')
            && repo
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-/:".contains(&b));
        if !named {
            bail!("push repository {repo:?} is not <registry>/<path> without a tag");
        }
        Ok(Self { repo, authfile })
    }

    /// The tag a build is published under: the sandbox's and the build's names.
    pub fn reference(&self, sandbox: &SandboxName, id: &BuildId) -> String {
        format!("{}:{}-{}", self.repo, sandbox.as_str(), id.0)
    }
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PushParams {
    /// build_id of a build whose status is succeeded
    pub build_id: String,
}

#[derive(Debug, Serialize)]
pub struct PushReply {
    pub build_id: String,
    /// The pushed image by digest, for a workload to run
    pub image: String,
    /// The tag it was pushed under
    pub tag: String,
    /// The digest of the context it was built from
    pub context_digest: Option<ContextDigest>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BuildParams {
    /// Build context: a subdirectory of the workspace, e.g. "svc/api", or on a local workspace
    /// an absolute path inside it
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
    /// Directories mounted into every RUN step that outlive the build: the sandbox's later
    /// builds in the same builder pod see what this one left
    #[serde(default)]
    pub caches: Vec<CacheSpec>,
    /// Instead of a Dockerfile, an image to run `script` in; the build is that image plus `path`
    /// as one new layer, so the image is neither unpacked nor pushed again. Its build_args reach
    /// the script as environment
    pub base: Option<String>,
    /// With `base`: a shell script, run with `sh -e` in `path` as the image's root
    pub script: Option<String>,
    /// With `base`: the absolute directory the context is placed at and the new layer holds
    pub path: Option<String>,
    /// Configure ccache in every RUN step through a generated /etc/ccache.conf: the shared cache
    /// as remote storage when the server has one, and a per-object log at
    /// /buildit/ccache-stats.log. Needs ccache 4.4 or later in the image
    #[serde(default)]
    pub ccache: bool,
}

/// A cache a build declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CacheSpec {
    /// Lowercase letters, digits and `-`; builds that name the same cache share it
    pub name: String,
    /// Absolute path RUN steps see it at
    pub path: String,
    /// Mount it read-only
    #[serde(default)]
    pub read_only: bool,
    /// Where it lives between builds (default "pod")
    #[serde(default)]
    pub scope: CacheScope,
    /// For a "cluster" cache: files or directories of the context whose contents decide what
    /// the cache holds, such as the files that pin its sources. Their hash keys its snapshot
    #[serde(default)]
    pub key: Vec<String>,
}

/// Where a cache lives between builds.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum CacheScope {
    /// In the sandbox's builder pod, for its later builds there
    #[default]
    Pod,
    /// Also in the shared cache. A new builder pod starts from the snapshot under its key, or
    /// else the newest one, and a build that may write to the shared cache saves it under its key
    Cluster,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StatusParams {
    /// build_id from build
    pub build_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CleanParams {
    /// Build to delete; omit to delete every build of this sandbox
    pub build_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BuildReply {
    pub build_id: String,
    pub status: Status,
    /// The digest of the context buildit uploaded for this build
    pub context_digest: ContextDigest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusReply {
    pub build_id: String,
    pub status: Status,
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub log_tail: String,
    pub context_digest: Option<ContextDigest>,
    /// The JSON object the build wrote to `/buildit/facts.json`, once it has ended
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Serialize, Deserialize)]
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

fn bud_argv(
    id: &BuildId,
    dockerfile: &RelPath,
    target: Option<&str>,
    build_args: &BTreeMap<String, String>,
    volumes: &[(String, String)],
    ccache: bool,
    extra: &[String],
) -> Result<Vec<String>> {
    let mut out = argv(["buildah", "bud"]);
    for (_, volume) in volumes {
        out.extend(["--volume".to_string(), volume.clone()]);
    }
    out.extend([
        "--volume".to_string(),
        format!("{}:{FACTS_MOUNT}:rw", jobs::FACTS),
    ]);
    if ccache {
        out.extend([
            "--volume".to_string(),
            format!("{}/{CCACHE_CONF}:/etc/ccache.conf:ro", jobs::FACTS),
        ]);
    }
    if let Some(t) = target {
        out.extend(["--target".to_string(), t.to_string()]);
    }
    for (k, v) in build_args {
        if k.is_empty() || k.contains('=') {
            bail!("invalid build arg name {k:?}");
        }
        out.extend(["--build-arg".to_string(), format!("{k}={v}")]);
    }
    out.extend(extra.iter().cloned());
    out.extend([
        "-f".to_string(),
        format!("{}/{}", jobs::WORKSPACE, dockerfile.as_str()),
        "-t".to_string(),
        id.tag(),
        jobs::WORKSPACE.to_string(),
    ]);
    Ok(out)
}

fn summary(log: &Log) -> String {
    let tail = Query::Tail {
        lines: SUMMARY_LINES,
    };
    log.query(&tail, SUMMARY_BYTES).lines
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
        if let (Some(oidc), Some(token)) = (&self.oidc, bearer_token(headers))
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
                let principal = caller.principal();
                Ok((caller, principal))
            }
            Err(StatusCode::UNAUTHORIZED) if self.oidc.is_some() => {
                Err(Refused::Token(match bearer_token(headers) {
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
        if let Some(run) = &self.run_id
            && principal
                .claims
                .get("run_id")
                .and_then(serde_json::Value::as_str)
                != Some(run)
        {
            return Err(ClaimError::OtherRun);
        }
        let raw = principal
            .claims
            .get("sandbox")
            .and_then(serde_json::Value::as_str)
            .ok_or(ClaimError::NoSandbox)?;
        let sandbox = SandboxName::parse(raw).map_err(|_| ClaimError::Sandbox(raw.to_string()))?;
        if self.run_caller.as_ref() == Some(&sandbox) {
            return Err(ClaimError::RunCaller(raw.to_string()));
        }
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
        let launched_by = Launcher::of(&principal.subject);
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
        let token = bearer_token(headers).ok_or(StatusCode::UNAUTHORIZED)?;
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
        if self.run_caller.as_ref() == Some(&line.sandbox) {
            tracing::error!(
                "{}: sandbox {} is the run's own caller, served on its socket alone",
                tokens_file.display(),
                line.sandbox.as_str()
            );
            return Err(StatusCode::FORBIDDEN);
        }
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

    fn jobs_api(&self) -> Api<Job> {
        Api::namespaced(self.client.clone(), &self.site.namespace)
    }

    fn pods_api(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.site.namespace)
    }

    fn cache(&self) -> Result<&SharedCache> {
        self.shared_cache.as_ref().ok_or_else(|| {
            anyhow!("this server has no shared cache (--cache-url); builds need one")
        })
    }

    /// The labels a Job of `caller`'s build `id` and its pod carry.
    fn labels(&self, caller: &Caller, id: &BuildId) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::from(
            [
                (LABEL_MANAGED_BY, MANAGED_BY),
                (LABEL_SANDBOX, caller.sandbox.as_str()),
                (LABEL_BUILD_ID, id.as_str()),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        if let Some(launcher) = &caller.launched_by {
            labels.insert(LABEL_LAUNCHED_BY.to_string(), launcher.as_str().to_string());
        }
        labels
    }

    /// The Job of `id`'s step `log`, listed by the caller's sandbox so no other's is seen.
    async fn step_job(
        &self,
        sandbox: &SandboxName,
        id: &BuildId,
        log: LogName,
    ) -> Result<Option<Job>> {
        let sel = format!(
            "{LABEL_MANAGED_BY}={MANAGED_BY},{LABEL_SANDBOX}={},{LABEL_BUILD_ID}={}",
            sandbox.as_str(),
            id.as_str()
        );
        let list = self
            .jobs_api()
            .list(&ListParams::default().labels(&sel))
            .await
            .with_context(|| format!("listing builds ({sel})"))?;
        let name = step_job_name(id, log);
        Ok(list
            .items
            .into_iter()
            .find(|j| j.metadata.name.as_deref() == Some(name.as_str())))
    }

    async fn pod_of(&self, job: &str) -> Result<Option<Pod>> {
        let params = ListParams::default().labels(&format!("{JOB_NAME_LABEL}={job}"));
        let pods = self
            .pods_api()
            .list(&params)
            .await
            .with_context(|| format!("listing job {job}'s pods"))?;
        Ok(pods
            .items
            .into_iter()
            .max_by_key(|p| p.metadata.creation_timestamp.clone()))
    }

    async fn blob(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let url = format!("{}/{key}", self.cache()?.base());
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("reading {url}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp
            .error_for_status()
            .with_context(|| format!("reading {url}"))?;
        Ok(Some(resp.bytes().await?.to_vec()))
    }

    async fn put_blob(&self, key: &str, body: Vec<u8>) -> Result<()> {
        let cache = self.cache()?;
        let token = cache.token().ok_or_else(|| {
            anyhow!("the shared cache's write token is unreadable; builds need it")
        })?;
        let url = format!("{}/{key}", cache.base());
        self.http
            .put(&url)
            .bearer_auth(token)
            .body(body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("writing {url}"))?;
        Ok(())
    }

    async fn has_blob(&self, key: &str) -> Result<bool> {
        let url = format!("{}/{key}", self.cache()?.base());
        let resp = self
            .http
            .head(&url)
            .send()
            .await
            .with_context(|| format!("checking {url}"))?;
        Ok(resp.status().is_success())
    }

    /// Where this server keeps a step's log once its pod has ended.
    fn log_file(&self, sandbox: &SandboxName, id: &BuildId, log: LogName) -> PathBuf {
        self.log_dir
            .join(sandbox.as_str())
            .join(id.as_str())
            .join(log.file())
    }

    /// Keeps an ended Job's log on this server's disk, unless it already holds as much.
    async fn keep_log(&self, job: &Job) -> Result<()> {
        let meta = &job.metadata;
        let (Some(sandbox), Some(build)) = (
            label(meta, LABEL_SANDBOX).and_then(|s| SandboxName::parse(s).ok()),
            label(meta, LABEL_BUILD_ID).and_then(|b| BuildId::parse(b).ok()),
        ) else {
            return Ok(());
        };
        let log = match label(meta, jobs::LABEL_KIND) {
            Some("build") => LogName::Build,
            Some("run") => match label(meta, jobs::LABEL_RUN).and_then(|n| n.parse().ok()) {
                Some(n) => LogName::Run(n),
                None => return Ok(()),
            },
            _ => return Ok(()),
        };
        let name = meta.name.clone().unwrap_or_default();
        let pod = self.pod_of(&name).await?;
        self.record_end(&sandbox, &build, log, job, pod.as_ref())
            .await?;
        let Some(pod) = pod else {
            return Ok(());
        };
        let text = self.pod_log(&pod, main_container(job, log)).await?;
        self.keep(&self.log_file(&sandbox, &build, log), &text)
            .await
    }

    /// Archives an ended step's exit record when its pod left none, as one killed before it
    /// wrote it does, so the step's end outlives its Job.
    async fn record_end(
        &self,
        sandbox: &SandboxName,
        id: &BuildId,
        log: LogName,
        job: &Job,
        pod: Option<&Pod>,
    ) -> Result<()> {
        let key = format!(
            "{}/{}",
            jobs::archive_prefix(sandbox.as_str(), id.as_str()),
            exit_file(log)
        );
        if self.has_blob(&key).await? {
            return Ok(());
        }
        let jobs::Observed::Ended { state, .. } = jobs::observe(job, pod, main_container(job, log))
        else {
            return Ok(());
        };
        match state.to_record() {
            Some(record) => {
                self.put_blob(&key, format!("{record}\n").into_bytes())
                    .await
            }
            None => Ok(()),
        }
    }

    /// Writes `text` to the log `file` when it holds more than the file does.
    async fn keep(&self, file: &Path, text: &str) -> Result<()> {
        let kept = tokio::fs::metadata(file).await.map_or(0, |m| m.len());
        if (text.len() as u64) <= kept {
            return Ok(());
        }
        save(file, text.as_bytes()).await
    }

    async fn pod_log(&self, pod: &Pod, container: &str) -> Result<String> {
        let name = pod.metadata.name.as_deref().unwrap_or_default();
        self.pods_api()
            .logs(
                name,
                &LogParams {
                    container: Some(container.to_string()),
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("reading pod {name}'s log"))
    }

    /// A step's log: from this server's disk once kept, else from its pod.
    async fn read_log(
        &self,
        sandbox: &SandboxName,
        id: &BuildId,
        log: LogName,
        window: u64,
    ) -> Result<Log> {
        let file = self.log_file(sandbox, id, log);
        if let Ok(bytes) = tokio::fs::read(&file).await {
            return Ok(Log::last(bytes, window));
        }
        let Some(job) = self.step_job(sandbox, id, log).await? else {
            bail!("build {} has no {log} log", id.0);
        };
        let name = job.metadata.name.clone().unwrap_or_default();
        let Some(pod) = self.pod_of(&name).await? else {
            return Ok(Log::last(Vec::new(), window));
        };
        if job_ended(&job)
            && let Err(e) = self.keep_log(&job).await
        {
            tracing::warn!("keeping {name}'s log: {e:#}");
        }
        match self.pod_log(&pod, main_container(&job, log)).await {
            Ok(text) => Ok(Log::last(text.into_bytes(), window)),
            Err(e) => {
                tracing::debug!("{e:#}");
                Ok(Log::last(Vec::new(), window))
            }
        }
    }

    /// Keeps the logs of Jobs as they end, so `logs` outlives the Jobs.
    pub async fn keep_logs(self: Arc<Self>) {
        use kube::runtime::{WatchStreamExt, watcher};
        let cfg = watcher::Config::default().labels(&format!("{LABEL_MANAGED_BY}={MANAGED_BY}"));
        let events = watcher(self.jobs_api(), cfg)
            .default_backoff()
            .applied_objects();
        futures_util::pin_mut!(events);
        while let Some(event) = futures_util::StreamExt::next(&mut events).await {
            match event {
                Ok(job) if job_ended(&job) => {
                    if let Err(e) = self.keep_log(&job).await {
                        tracing::warn!(
                            "keeping job {}'s log: {e:#}",
                            job.metadata.name.as_deref().unwrap_or_default()
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("watching build jobs: {e}"),
            }
        }
    }

    /// Where `id`'s step `log` is: from its Job while Kubernetes keeps it, else from the exit
    /// record the step archived. None when this sandbox has no such step.
    async fn seen(
        &self,
        sandbox: &SandboxName,
        id: &BuildId,
        log: LogName,
    ) -> Result<Option<Seen>> {
        let prefix = jobs::archive_prefix(sandbox.as_str(), id.as_str());
        let Some(job) = self.step_job(sandbox, id, log).await? else {
            let Some(raw) = self.blob(&format!("{prefix}/{}", exit_file(log))).await? else {
                return Ok(None);
            };
            let state = step::State::record(String::from_utf8_lossy(&raw).trim())
                .unwrap_or(step::State::Lost);
            let digest = match log {
                LogName::Build => self
                    .blob(&format!("{prefix}/image"))
                    .await?
                    .map(|d| String::from_utf8_lossy(&d).trim().to_string()),
                LogName::Run(_) => None,
            };
            return Ok(Some(Seen::ended(state, digest, None)));
        };
        let name = job.metadata.name.clone().unwrap_or_default();
        let pod = self.pod_of(&name).await?;
        let created = job
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.to_string())
            .unwrap_or_default();
        let context_digest = annotation(&job.metadata, ANNOTATION_CONTEXT_DIGEST)
            .and_then(|d| ContextDigest::parse(d).ok());
        let mut seen = match jobs::observe(&job, pod.as_ref(), main_container(&job, log)) {
            jobs::Observed::Queued => Seen::working(&created, Working::Queued),
            jobs::Observed::Running => Seen::working(&created, Working::Running),
            jobs::Observed::Ended { state, digest } => {
                let ended = finished_at(pod.as_ref(), main_container(&job, log))
                    .or_else(|| job_end(&job))
                    .unwrap_or_else(|| created.clone());
                Seen::ended(state, digest, Some((created.clone(), ended)))
            }
        };
        seen.context_digest = context_digest;
        let archived = matches!(log, LogName::Run(_)) || is_layer(&job);
        if archived
            && matches!(
                seen.snap.state,
                step::State::Exited { .. } | step::State::Expired
            )
        {
            match self.blob(&format!("{prefix}/{}", exit_file(log))).await? {
                None => {
                    seen.snap.state = step::State::Running;
                    seen.working = Some(Working::Archiving);
                }
                Some(raw) if is_layer(&job) => {
                    if let Some(state) = step::State::record(String::from_utf8_lossy(&raw).trim()) {
                        seen.snap.state = state;
                    }
                    seen.digest = self
                        .blob(&format!("{prefix}/image"))
                        .await?
                        .map(|d| String::from_utf8_lossy(&d).trim().to_string());
                }
                Some(_) => {}
            }
        }
        Ok(Some(seen))
    }

    /// The run's snapshot with its fetch and published state filled in.
    async fn run_snapshot(
        &self,
        sandbox: &SandboxName,
        id: &BuildId,
        n: u32,
        seen: &mut Seen,
    ) -> Result<()> {
        if seen.snap.state == step::State::Running || seen.snap.state == step::State::Cancelled {
            return Ok(());
        }
        let prefix = jobs::archive_prefix(sandbox.as_str(), id.as_str());
        seen.snap.fetch = if self.has_blob(&format!("{prefix}/run-{n}.tar")).await? {
            Fetch::Ready
        } else if self
            .blob(&format!("{prefix}/run-{n}.fetch-err"))
            .await?
            .is_some_and(|e| !e.is_empty())
        {
            Fetch::Failed
        } else {
            Fetch::Nothing
        };
        let record = self
            .log_file(sandbox, id, LogName::Run(n))
            .with_extension("published");
        seen.snap.published = match tokio::fs::read(&record).await {
            Ok(raw) => Some(serde_json::from_slice(&raw).context("reading a published record")?),
            Err(_) => None,
        };
        Ok(())
    }

    async fn start_build(
        &self,
        caller: &Caller,
        p: BuildParams,
    ) -> Result<(BuildId, Snapshot, ContextDigest)> {
        let cache = self.cache()?;
        let context = caller.workspace.context(&p.context)?;
        let recipe = Recipe::of(&p)?;
        let dockerfile = RelPath::parse(p.dockerfile.as_deref().unwrap_or("Dockerfile"))?;
        let limit = timeout(p.timeout_s, DEFAULT_BUILD_TIMEOUT_S);
        let volumes = cache_volumes(&p.caches)?;
        let live = self.live_builds(&caller.sandbox).await?;
        if live.len() >= self.max_builds {
            bail!(
                "this sandbox has {} builds running (max {}): {}; wait for one or clean it",
                live.len(),
                self.max_builds,
                live.join(", ")
            );
        }
        let stage = tempfile::tempdir().context("creating staging dir")?;
        let ctx = stage.path().join("ctx");
        tokio::task::block_in_place(|| {
            caller
                .workspace
                .fetch_context(&caller.sandbox, &context, &ctx)
        })?;
        let tar = crate::context::staged_tar_bytes(&ctx)?;
        let digest = crate::context::digest(&tar)?;
        let key = jobs::context_key(digest.as_str());
        if !self.has_blob(&key).await? {
            self.put_blob(&key, crate::context::gzip(&tar)?).await?;
        }
        let starter = if self.run_caller.as_ref() == Some(&caller.sandbox) {
            Starter::Run
        } else {
            Starter::Sandbox
        };
        let root = match recipe {
            Recipe::Dockerfile => CACHE_ROOT,
            Recipe::Layer { .. } => jobs::CACHES,
        };
        let snapshots = cache.snapshots(&p.caches, &tar, starter, root)?;
        let id = BuildId::fresh();
        let bud = bud_argv(
            &id,
            &dockerfile,
            p.target.as_deref(),
            &p.build_args,
            &volumes,
            p.ccache,
            &self.site.bud_args,
        )?;
        let files = (!snapshots.plan.is_empty()).then(|| step::SnapshotFiles {
            plan: format!("{INPUTS_DIR}/snapshots"),
            url: cache.base(),
            auth: format!("{INPUTS_DIR}/snapshots.auth"),
        });
        let steps = format!("{}/{STEPS_LOG}", jobs::FACTS);
        let runner_args = step::build_args(limit, &steps, files.as_ref(), &bud);
        let mut inputs = BTreeMap::from([
            ("cache.json".to_string(), cache.document(starter)),
            ("archive.auth".to_string(), archive_auth(cache)?),
        ]);
        if !snapshots.plan.is_empty() {
            inputs.insert("snapshots".to_string(), snapshots.plan.clone().into_bytes());
        }
        if let Some(auth) = &snapshots.auth {
            inputs.insert("snapshots.auth".to_string(), auth.clone().into_bytes());
        }
        if p.ccache {
            inputs.insert(
                "ccache.conf".to_string(),
                ccache_conf(Some(cache), starter).into_bytes(),
            );
        }
        let labels = self.labels(caller, &id);
        let ident = jobs::Ident {
            name: id.as_str(),
            build: id.as_str(),
            sandbox: caller.sandbox.as_str(),
            labels: &labels,
            annotations: BTreeMap::from([(
                ANNOTATION_CONTEXT_DIGEST.to_string(),
                digest.as_str().to_string(),
            )]),
        };
        let spec = match recipe {
            Recipe::Dockerfile => {
                let cache_dirs: Vec<String> = volumes.iter().map(|(dir, _)| dir.clone()).collect();
                jobs::build_job(
                    &self.site,
                    &ident,
                    &jobs::BuildJob {
                        context_digest: digest.as_str(),
                        cache_dirs: &cache_dirs,
                        runner_args: &runner_args,
                        local_tag: &id.tag(),
                        limit_secs: limit_secs(limit) + RESTORE_SAVE_SECS,
                    },
                )
            }
            Recipe::Layer { base, script, path } => {
                let base = pinned(base).await?;
                let caches: Vec<(String, String)> = p
                    .caches
                    .iter()
                    .map(|c| (c.name.clone(), c.path.clone()))
                    .collect();
                jobs::layer_job(
                    &self.site,
                    &ident,
                    &jobs::LayerJob {
                        context_digest: digest.as_str(),
                        base: &base,
                        script,
                        path,
                        env: &p.build_args,
                        caches: &caches,
                        ccache: p.ccache,
                        limit_secs: limit_secs(limit),
                    },
                )
            }
        };
        self.create(&spec, &labels, inputs).await?;
        tracing::info!(
            "build {}: sandbox {} context {} {digest} ({} KiB)",
            id.0,
            caller.sandbox.as_str(),
            context.as_str(),
            tar.len() / 1024,
        );
        let now = now_string();
        Ok((id, Snapshot::running(&now), digest))
    }

    /// Creates `job` and then its inputs Secret, owned by it; deletes the Job when the Secret
    /// cannot be made.
    async fn create(
        &self,
        job: &Job,
        labels: &BTreeMap<String, String>,
        inputs: BTreeMap<String, Vec<u8>>,
    ) -> Result<()> {
        let name = job.metadata.name.clone().unwrap_or_default();
        let created = self
            .jobs_api()
            .create(&PostParams::default(), job)
            .await
            .with_context(|| format!("creating job {name}"))?;
        let uid = created
            .metadata
            .uid
            .ok_or_else(|| anyhow!("job {name} came back without a uid"))?;
        let secret = jobs::inputs_secret(&self.site.namespace, &name, &uid, labels, inputs);
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.site.namespace);
        if let Err(e) = secrets.create(&PostParams::default(), &secret).await {
            let _ = self
                .jobs_api()
                .delete(&name, &DeleteParams::background())
                .await;
            return Err(e).with_context(|| format!("creating job {name}'s inputs"));
        }
        Ok(())
    }

    /// The sandbox's builds that have not ended.
    async fn live_builds(&self, sandbox: &SandboxName) -> Result<Vec<String>> {
        let sel = format!(
            "{LABEL_MANAGED_BY}={MANAGED_BY},{LABEL_SANDBOX}={},{}=build",
            sandbox.as_str(),
            jobs::LABEL_KIND
        );
        let list = self
            .jobs_api()
            .list(&ListParams::default().labels(&sel))
            .await
            .with_context(|| format!("listing builds ({sel})"))?;
        Ok(list
            .items
            .iter()
            .filter(|j| !job_ended(j))
            .filter_map(|j| j.metadata.name.clone())
            .collect())
    }

    pub async fn build(&self, caller: &Caller, p: BuildParams) -> Result<BuildReply> {
        let (id, snap, context_digest) = self.start_build(caller, p).await?;
        Ok(BuildReply {
            build_id: id.0,
            status: snap.state.status(),
            context_digest,
        })
    }

    /// The image a succeeded build pushed, by digest.
    async fn built_image(&self, sandbox: &SandboxName, id: &BuildId) -> Result<String> {
        let seen = self
            .seen(sandbox, id, LogName::Build)
            .await?
            .ok_or_else(|| anyhow!("no build {}; build again", id.0))?;
        match seen.snap.state {
            step::State::Exited { code: 0, .. } => {}
            step::State::Running => bail!("build {} is still building; poll status", id.0),
            state => bail!(
                "build {} {} (exit {}); read its logs",
                id.0,
                if state == step::State::Cancelled {
                    "was cancelled"
                } else {
                    "failed"
                },
                state.exit().map_or("none".to_string(), |c| c.to_string())
            ),
        }
        let digest = seen
            .digest
            .ok_or_else(|| anyhow!("build {} succeeded but recorded no image", id.0))?;
        Ok(format!("{}@{digest}", self.site.image_repo))
    }

    pub async fn push(&self, caller: &Caller, p: PushParams) -> Result<PushReply> {
        let Some(target) = &self.push else {
            bail!("this server publishes no builds: it was started without --push-repo");
        };
        let id = BuildId::parse(&p.build_id)?;
        let source = self.built_image(&caller.sandbox, &id).await?;
        let context_digest = self
            .seen(&caller.sandbox, &id, LogName::Build)
            .await?
            .and_then(|s| s.context_digest);
        let tag = target.reference(&caller.sandbox, &id);
        let authfile = tokio::fs::read(&target.authfile)
            .await
            .with_context(|| format!("reading push authfile {}", target.authfile.display()))?;
        let name = format!("{}-push-{}", id.0, &BuildId::fresh().0[8..16]);
        let labels = self.labels(caller, &id);
        let ident = jobs::Ident {
            name: &name,
            build: id.as_str(),
            sandbox: caller.sandbox.as_str(),
            labels: &labels,
            annotations: BTreeMap::new(),
        };
        let spec = jobs::push_job(
            &self.site,
            &ident,
            &jobs::PushJob {
                source: &source,
                dest: &tag,
            },
        );
        self.create(
            &spec,
            &labels,
            BTreeMap::from([("push.json".to_string(), authfile)]),
        )
        .await?;
        let ended = self.wait_ended(&name, PUSH_TIMEOUT).await;
        let pod = self.pod_of(&name).await.ok().flatten();
        let tail = match &pod {
            Some(pod) => match self.pod_log(pod, jobs::BUILDER).await {
                Ok(text) => summary(&Log::last(text.into_bytes(), SUMMARY_WINDOW)),
                Err(e) => format!("{e:#}"),
            },
            None => String::new(),
        };
        let _ = self
            .jobs_api()
            .delete(&name, &DeleteParams::background())
            .await;
        let job = ended.with_context(|| format!("pushing build {} to {tag}: {tail}", id.0))?;
        let digest = match jobs::observe(&job, pod.as_ref(), jobs::BUILDER) {
            jobs::Observed::Ended {
                state: step::State::Exited { code: 0, .. },
                ..
            } => terminated_message(pod.as_ref(), jobs::BUILDER),
            _ => None,
        };
        let digest = digest
            .map(|d| d.trim().to_string())
            .filter(|d| d.starts_with("sha256:"))
            .ok_or_else(|| anyhow!("pushing build {} to {tag} failed:\n{tail}", id.0))?;
        tracing::info!(sandbox = caller.sandbox.as_str(), build = id.0, %tag, digest, "pushed");
        Ok(PushReply {
            build_id: id.0,
            image: format!("{}@{digest}", target.repo),
            tag,
            context_digest,
        })
    }

    /// The Job once it has ended, or an error once `limit` passes.
    async fn wait_ended(&self, name: &str, limit: Duration) -> Result<Job> {
        use kube::runtime::wait::await_condition;
        let cond = |job: Option<&Job>| job.is_some_and(job_ended);
        let waited =
            tokio::time::timeout(limit, await_condition(self.jobs_api(), name, cond)).await;
        match waited {
            Ok(Ok(Some(job))) => Ok(job),
            Ok(Ok(None)) => bail!("job {name} is gone"),
            Ok(Err(e)) => Err(e).with_context(|| format!("watching job {name}")),
            Err(_) => bail!("job {name} did not end within {}s", limit.as_secs()),
        }
    }

    pub async fn status(&self, sandbox: &SandboxName, p: StatusParams) -> Result<StatusReply> {
        let id = BuildId::parse(&p.build_id)?;
        let seen = self
            .seen(sandbox, &id, LogName::Build)
            .await?
            .ok_or_else(|| anyhow!("no build {}; build again", id.0))?;
        self.build_status(sandbox, &id, &seen).await
    }

    async fn build_status(
        &self,
        sandbox: &SandboxName,
        id: &BuildId,
        seen: &Seen,
    ) -> Result<StatusReply> {
        let log_tail = match seen.working {
            Some(Working::Queued) => format!(
                "queued in {}: waiting for Kueue to admit the build",
                self.site.queue.as_deref().unwrap_or("the build namespace")
            ),
            _ => summary(
                &self
                    .read_log(sandbox, id, LogName::Build, SUMMARY_WINDOW)
                    .await?,
            ),
        };
        let facts = match seen.snap.state {
            step::State::Running => None,
            _ => self.facts(sandbox, id).await,
        };
        Ok(StatusReply {
            build_id: id.0.clone(),
            status: seen.snap.state.status(),
            exit: seen.snap.state.exit(),
            timed_out: seen.snap.state.timed_out(),
            log_tail,
            context_digest: seen.context_digest.clone(),
            facts,
        })
    }

    /// The object the build wrote to `facts.json`, from the facts it archived.
    async fn facts(
        &self,
        sandbox: &SandboxName,
        id: &BuildId,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        let prefix = jobs::archive_prefix(sandbox.as_str(), id.as_str());
        let tar = self.blob(&format!("{prefix}/facts.tar")).await.ok()??;
        let mut archive = tar::Archive::new(tar.as_slice());
        for entry in archive.entries().ok()? {
            let mut entry = entry.ok()?;
            let path = entry.path().ok()?.to_string_lossy().into_owned();
            if path.trim_start_matches("./") != "facts.json" {
                continue;
            }
            let mut raw = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(&mut entry, MAX_FACTS), &mut raw)
                .ok()?;
            return match serde_json::from_slice(&raw) {
                Ok(serde_json::Value::Object(facts)) => Some(facts),
                _ => {
                    tracing::warn!("build {} left a facts.json that is not a JSON object", id.0);
                    None
                }
            };
        }
        None
    }

    async fn start_run(
        &self,
        caller: &Caller,
        p: RunParams,
    ) -> Result<(BuildId, LogName, Snapshot)> {
        if p.cmd.is_empty() {
            bail!("cmd must not be empty");
        }
        let id = BuildId::parse(&p.build_id)?;
        let paths = p
            .fetch_paths
            .iter()
            .map(|f| RelPath::parse(f.trim_start_matches('/')).map(|r| r.as_str().to_string()))
            .collect::<Result<Vec<_>>>()?;
        let image = self.built_image(&caller.sandbox, &id).await?;
        let cache = self.cache()?;
        let limit = timeout(p.timeout_s, DEFAULT_RUN_TIMEOUT_S);
        let labels = self.labels(caller, &id);
        for _ in 0..LIST_TRIES {
            let n = self.next_run(&caller.sandbox, &id).await?;
            let name = step_job_name(&id, LogName::Run(n));
            let ident = jobs::Ident {
                name: &name,
                build: id.as_str(),
                sandbox: caller.sandbox.as_str(),
                labels: &labels,
                annotations: BTreeMap::new(),
            };
            let spec = jobs::run_job(
                &self.site,
                &ident,
                &jobs::RunJob {
                    n,
                    image: &image,
                    cmd: &p.cmd,
                    fetch_paths: &paths,
                    limit_secs: limit_secs(limit),
                },
            );
            let inputs = BTreeMap::from([("archive.auth".to_string(), archive_auth(cache)?)]);
            match self.create(&spec, &labels, inputs).await {
                Ok(()) => return Ok((id, LogName::Run(n), Snapshot::running(&now_string()))),
                Err(e) if already_exists(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        bail!("could not number a run of build {}; try again", id.0)
    }

    async fn next_run(&self, sandbox: &SandboxName, id: &BuildId) -> Result<u32> {
        let sel = format!(
            "{LABEL_SANDBOX}={},{LABEL_BUILD_ID}={},{}=run",
            sandbox.as_str(),
            id.as_str(),
            jobs::LABEL_KIND
        );
        let list = self
            .jobs_api()
            .list(&ListParams::default().labels(&sel))
            .await
            .with_context(|| format!("listing runs ({sel})"))?;
        let mut n = list
            .items
            .iter()
            .filter_map(|j| label(&j.metadata, jobs::LABEL_RUN)?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let prefix = jobs::archive_prefix(sandbox.as_str(), id.as_str());
        while self.log_file(sandbox, id, LogName::Run(n)).exists()
            || self.has_blob(&format!("{prefix}/run-{n}.exit")).await?
        {
            n += 1;
        }
        Ok(n)
    }

    pub async fn run(&self, caller: &Caller, p: RunParams) -> Result<RunReply> {
        let limit = FROM_TIMEOUT + timeout(p.timeout_s, DEFAULT_RUN_TIMEOUT_S) + EXEC_SLACK;
        let (id, log, _) = self.start_run(caller, p).await?;
        let deadline = Instant::now() + limit;
        loop {
            if let Some((snap, View::Completed { is_error, text })) = self
                .observe(
                    caller,
                    &TaskRef {
                        build: id.clone(),
                        log,
                    },
                    true,
                )
                .await?
            {
                let _ = snap;
                if is_error && serde_json::from_str::<RunReply>(&text).is_err() {
                    bail!(text);
                }
                return serde_json::from_str(&text).context("reading the run reply");
            }
            if Instant::now() >= deadline {
                bail!(
                    "run {log} of build {} did not end within {}s",
                    id.0,
                    limit.as_secs()
                );
            }
            tokio::time::sleep(SETTLE_RETRY).await;
        }
    }

    /// Publishes a finished run's fetch_paths into the caller's workspace, once.
    async fn settle(
        &self,
        caller: &Caller,
        id: &BuildId,
        n: u32,
        fetch: Fetch,
    ) -> Result<Published> {
        let _claim = self.publishing.lock().await;
        let record = self
            .log_file(&caller.sandbox, id, LogName::Run(n))
            .with_extension("published");
        if let Ok(raw) = tokio::fs::read(&record).await {
            return serde_json::from_slice(&raw).context("reading a published record");
        }
        let prefix = jobs::archive_prefix(caller.sandbox.as_str(), id.as_str());
        let published = match fetch {
            Fetch::Nothing => Published::Fetched(Vec::new()),
            Fetch::Ready => match self
                .publish(caller, id, &format!("{prefix}/run-{n}.tar"))
                .await
            {
                Ok(files) => Published::Fetched(files),
                Err(e) => Published::Error(format!("{e:#}")),
            },
            Fetch::Failed => {
                let err = self
                    .blob(&format!("{prefix}/run-{n}.fetch-err"))
                    .await?
                    .unwrap_or_default();
                Published::Error(format!(
                    "fetching paths failed: {}",
                    String::from_utf8_lossy(&err).trim()
                ))
            }
        };
        if let Some(dir) = record.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&record, serde_json::to_vec(&published)?).await?;
        Ok(published)
    }

    async fn publish(&self, caller: &Caller, id: &BuildId, key: &str) -> Result<Vec<String>> {
        let tar = self
            .blob(key)
            .await?
            .ok_or_else(|| anyhow!("the run's fetched paths are gone from the shared cache"))?;
        if tar.len() as u64 > step::FETCH_LIMIT {
            bail!(
                "fetch_paths add up to more than {} bytes",
                step::FETCH_LIMIT
            );
        }
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
        let Some(mut seen) = self.seen(&caller.sandbox, &t.build, t.log).await? else {
            return Ok(None);
        };
        if t.log == LogName::Build {
            let reply = self.build_status(&caller.sandbox, &t.build, &seen).await?;
            let view = match task::progress(&seen.snap) {
                Progress::Working | Progress::Publishing => {
                    View::Working(seen.working.unwrap_or(Working::Running).message("building"))
                }
                Progress::Cancelled => View::Cancelled,
                Progress::Done { state, .. } => View::Completed {
                    is_error: state.status() != Status::Succeeded,
                    text: serde_json::to_string(&reply).context("serializing reply")?,
                },
            };
            return Ok(Some((seen.snap, view)));
        }
        let LogName::Run(n) = t.log else {
            return Ok(None);
        };
        self.run_snapshot(&caller.sandbox, &t.build, n, &mut seen)
            .await?;
        if may_build && task::progress(&seen.snap) == Progress::Publishing {
            let published = self.settle(caller, &t.build, n, seen.snap.fetch).await?;
            seen.snap.published = Some(published);
        }
        let view = match task::progress(&seen.snap) {
            Progress::Working => {
                View::Working(seen.working.unwrap_or(Working::Running).message("running"))
            }
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
                let log = self
                    .read_log(&caller.sandbox, &t.build, t.log, SUMMARY_WINDOW)
                    .await?;
                let reply = run_reply(&t.build, t.log, state, &summary(&log), &files);
                View::Completed {
                    is_error: state.status() != Status::Succeeded,
                    text: serde_json::to_string(&reply).context("serializing reply")?,
                }
            }
        };
        Ok(Some((seen.snap, view)))
    }

    // None when the task is unknown to this caller; a task that already ended is left as is
    async fn cancel(&self, caller: &Caller, t: &TaskRef) -> Result<Option<()>> {
        let Some(seen) = self.seen(&caller.sandbox, &t.build, t.log).await? else {
            return Ok(None);
        };
        if seen.snap.state != step::State::Running || seen.working == Some(Working::Archiving) {
            return Ok(Some(()));
        }
        let Some(job) = self.step_job(&caller.sandbox, &t.build, t.log).await? else {
            return Ok(Some(()));
        };
        let name = job.metadata.name.clone().unwrap_or_default();
        if job.spec.as_ref().and_then(|s| s.suspend) == Some(true) {
            let key = format!(
                "{}/{}",
                jobs::archive_prefix(caller.sandbox.as_str(), t.build.as_str()),
                exit_file(t.log)
            );
            self.put_blob(&key, b"cancelled\n".to_vec()).await?;
            self.jobs_api()
                .delete(&name, &DeleteParams::background())
                .await
                .with_context(|| format!("cancelling queued job {name}"))?;
        } else {
            if let Some(pod) = self.pod_of(&name).await? {
                match self.pod_log(&pod, main_container(&job, t.log)).await {
                    Ok(text) => {
                        let file = self.log_file(&caller.sandbox, &t.build, t.log);
                        self.keep(&file, &text).await?;
                    }
                    Err(e) => tracing::warn!("keeping cancelled job {name}'s log: {e:#}"),
                }
            }
            let patch = serde_json::json!({
                "metadata": { "annotations": { (jobs::ANNOTATION_CANCELLED): "true" } },
                "spec": { "activeDeadlineSeconds": 1 },
            });
            self.jobs_api()
                .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await
                .with_context(|| format!("cancelling job {name}"))?;
        }
        tracing::info!(
            "task {} cancelled by sandbox {}",
            t.encode(),
            caller.sandbox.as_str()
        );
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
        let log = self.read_log(sandbox, &id, name, MAX_READ).await?;
        Ok(log.query(&query, usize_of(max)))
    }

    // one build's Jobs, or every Job of the sandbox; logs this server kept stay readable
    pub async fn clean(&self, sandbox: &SandboxName, id: Option<&str>) -> Result<CleanReply> {
        let id = id.map(BuildId::parse).transpose()?;
        let mut sel = format!(
            "{LABEL_MANAGED_BY}={MANAGED_BY},{LABEL_SANDBOX}={}",
            sandbox.as_str()
        );
        if let Some(id) = &id {
            sel.push_str(&format!(",{LABEL_BUILD_ID}={}", id.as_str()));
        }
        let list = self
            .jobs_api()
            .list(&ListParams::default().labels(&sel))
            .await
            .with_context(|| format!("listing builds ({sel})"))?;
        let mut deleted = Vec::new();
        for job in list.items {
            if job.metadata.deletion_timestamp.is_some() {
                continue;
            }
            if job_ended(&job)
                && let Err(e) = self.keep_log(&job).await
            {
                tracing::warn!("keeping a cleaned job's log: {e:#}");
            }
            let name = job.metadata.name.unwrap_or_default();
            self.jobs_api()
                .delete(&name, &DeleteParams::background())
                .await
                .with_context(|| format!("deleting job {name}"))?;
            deleted.push(name);
        }
        deleted.sort();
        Ok(CleanReply { deleted })
    }
}

/// What a step is doing while it has not ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Working {
    /// Its Job waits for Kueue to admit it.
    Queued,
    Running,
    /// Its pod ended and its archive is still going up.
    Archiving,
}

impl Working {
    fn message(self, running: &'static str) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => running,
            Self::Archiving => "archiving",
        }
    }
}

/// A step as last seen: its snapshot for tasks, and what a build left.
#[derive(Debug, Clone)]
struct Seen {
    snap: Snapshot,
    working: Option<Working>,
    digest: Option<String>,
    context_digest: Option<ContextDigest>,
}

impl Seen {
    fn working(created: &str, working: Working) -> Self {
        Self {
            snap: Snapshot {
                created: created.to_string(),
                updated: now_string(),
                state: step::State::Running,
                fetch: Fetch::Nothing,
                published: None,
            },
            working: Some(working),
            digest: None,
            context_digest: None,
        }
    }

    /// An ended step; `created` and `ended` are its Job's, when Kubernetes still keeps it.
    fn ended(state: step::State, digest: Option<String>, times: Option<(String, String)>) -> Self {
        let (created, updated) = times.unwrap_or_default();
        Self {
            snap: Snapshot {
                created,
                updated,
                state,
                fetch: Fetch::Nothing,
                published: None,
            },
            working: None,
            digest,
            context_digest: None,
        }
    }
}

/// The Job a build's step runs as: the build itself, or its run `n`.
fn step_job_name(id: &BuildId, log: LogName) -> String {
    match log {
        LogName::Build => id.0.clone(),
        LogName::Run(n) => format!("{}-run-{n}", id.0),
    }
}

fn exit_file(log: LogName) -> String {
    match log {
        LogName::Build => "build.exit".to_string(),
        LogName::Run(n) => format!("run-{n}.exit"),
    }
}

/// The container of `job` that runs the step: the builder, a layer build's script, or a run.
fn main_container(job: &Job, log: LogName) -> &'static str {
    match log {
        LogName::Build if is_layer(job) => jobs::LAYER_CONTAINER,
        LogName::Build => jobs::BUILDER,
        LogName::Run(_) => jobs::RUN_CONTAINER,
    }
}

fn is_layer(job: &Job) -> bool {
    label(&job.metadata, jobs::LABEL_RECIPE) == Some("layer")
}

/// How a build makes its image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recipe<'a> {
    Dockerfile,
    /// `script` in `base`, then `path` appended as a layer.
    Layer {
        base: &'a str,
        script: &'a str,
        path: &'a str,
    },
}

impl<'a> Recipe<'a> {
    fn of(p: &'a BuildParams) -> Result<Self> {
        let Some(base) = p.base.as_deref() else {
            if p.script.is_some() || p.path.is_some() {
                bail!("script and path go with base");
            }
            return Ok(Self::Dockerfile);
        };
        if p.dockerfile.is_some() || p.target.is_some() {
            bail!("a build with base runs a script; it takes no dockerfile or target");
        }
        let script = p
            .script
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("a build with base needs a script"))?;
        let path = p
            .path
            .as_deref()
            .ok_or_else(|| anyhow!("a build with base needs a path"))?;
        let plain = path.len() <= 255
            && !path.contains(['\n', '\0'])
            && path.strip_prefix('/').is_some_and(|rest| {
                !rest.is_empty()
                    && rest
                        .trim_end_matches('/')
                        .split('/')
                        .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
            });
        if !plain || path.starts_with(FACTS_MOUNT) || path.starts_with("/buildit-") {
            bail!(
                "path {path:?} must be a plain absolute directory below / that buildit does not use"
            );
        }
        Ok(Self::Layer { base, script, path })
    }
}

/// `image` by digest, resolving a tag in its registry.
async fn pinned(image: &str) -> Result<String> {
    if image.contains("@sha256:") {
        return Ok(image.to_string());
    }
    let digest =
        crate::oci::resolve_digest(image, &oci_client::secrets::RegistryAuth::Anonymous).await?;
    Ok(format!("{}@{digest}", crate::oci::repo_of(image)))
}

fn label<'a>(meta: &'a ObjectMeta, key: &str) -> Option<&'a str> {
    meta.labels.as_ref()?.get(key).map(String::as_str)
}

fn annotation<'a>(meta: &'a ObjectMeta, key: &str) -> Option<&'a str> {
    meta.annotations.as_ref()?.get(key).map(String::as_str)
}

fn job_ended(job: &Job) -> bool {
    job.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|cs| {
            cs.iter()
                .any(|c| (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True")
        })
}

/// When a pod's `container` finished.
fn finished_at(pod: Option<&Pod>, container: &str) -> Option<String> {
    pod?.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .find(|c| c.name == container)?
        .state
        .as_ref()?
        .terminated
        .as_ref()?
        .finished_at
        .as_ref()
        .map(|t| t.0.to_string())
}

/// When a Job ended: its completion, or else the condition that ended it.
fn job_end(job: &Job) -> Option<String> {
    let status = job.status.as_ref()?;
    status
        .completion_time
        .as_ref()
        .map(|t| t.0.to_string())
        .or_else(|| {
            status
                .conditions
                .as_ref()?
                .iter()
                .find(|c| (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True")?
                .last_transition_time
                .as_ref()
                .map(|t| t.0.to_string())
        })
}

fn terminated_message(pod: Option<&Pod>, container: &str) -> Option<String> {
    pod?.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .find(|c| c.name == container)?
        .state
        .as_ref()?
        .terminated
        .as_ref()?
        .message
        .clone()
}

fn already_exists(e: &anyhow::Error) -> bool {
    e.chain().any(
        |c| matches!(c.downcast_ref::<kube::Error>(), Some(kube::Error::Api(s)) if s.code == 409),
    )
}

/// The header the archive sidecar writes the shared cache with.
fn archive_auth(cache: &SharedCache) -> Result<Vec<u8>> {
    let token = cache
        .token()
        .ok_or_else(|| anyhow!("the shared cache's write token is unreadable; builds need it"))?;
    Ok(format!("Authorization: Bearer {token}\n").into_bytes())
}

/// Writes `body` to `file` whole, through a temporary file beside it.
async fn save(file: &Path, body: &[u8]) -> Result<()> {
    if let Some(dir) = file.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let part = file.with_extension("part");
    tokio::fs::write(&part, body)
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    tokio::fs::rename(&part, file)
        .await
        .with_context(|| format!("writing {}", file.display()))
}

fn limit_secs(limit: Duration) -> i64 {
    i64::try_from(limit.as_secs()).unwrap_or(i64::MAX)
}

fn now_string() -> String {
    k8s_openapi::jiff::Timestamp::now().to_string()
}

fn run_reply(
    id: &BuildId,
    log: LogName,
    state: step::State,
    tail: &str,
    fetched: &[String],
) -> RunReply {
    let (exit, timed_out) = match state {
        step::State::Running => (None, true),
        state => (state.exit(), state.timed_out()),
    };
    RunReply {
        log: log.to_string(),
        exit,
        timed_out,
        log_tail: tail.to_string(),
        fetched: fetched
            .iter()
            .map(|f| format!("{RESULTS_DIR}/{}/{f}", id.0))
            .collect(),
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
                "build" | "run" | "push" | "clean" => Ok(&[SCOPE_BUILD]),
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
        timeout_s); `push` publishes it once it succeeds. Returns build_id with status running at once; \
        the build waits in the queue until there is room. Poll `status` until succeeded or failed, \
        then `run`/`logs` as needed, and `clean` the build_id when done. \
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
        let (id, snap, _) = started.map_err(|e| format!("{e:#}"))?;
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
        let (id, log, snap) = started.map_err(|e| format!("{e:#}"))?;
        Ok(created(&TaskRef { build: id, log }, &snap))
    }

    #[tool(
        description = "Push a build whose status is succeeded (params: build_id) to the one \
        repository this server publishes to, tagged <sandbox>-<build_id>. Returns image, the \
        repository@sha256 digest a workload should run, and tag. Refused when the server \
        publishes nothing."
    )]
    async fn push(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<PushParams>,
    ) -> Result<String, String> {
        reply(async { self.broker.push(&caller(&ctx)?, p).await }.await)
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
        description = "Drop a build (params: build_id) and its runs, or every build of this sandbox \
        when build_id is omitted. Their logs stay readable. Call it once done with a build, \
        succeeded or not."
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

    use crate::backend::{Resources, StorageDriver};
    use std::time::Duration;

    use crate::jobs;
    use crate::mcp::{
        Broker, BuildId, BuildParams, BuilditMcp, BuilditRules, CACHE_ROOT, CacheScope, CacheSpec,
        CacheWriters, Caller, Launcher, LogsParams, MCP_PATH, PushParams, PushTarget,
        ResourceServer, ResourceServerConfig, Response, SCOPE_BUILD, SCOPE_READ, SharedCache,
        Snapshots, Starter, StatusCode, TokenLine, ToolPolicy, TrustedIssuer, bind_socket,
        bud_argv, cache_volumes, ccache_conf, parse_tokens, router, run_sandbox, socket_router,
        unpack,
    };
    use crate::sandbox::{RelPath, SandboxDir, SandboxName, Workspace};
    use crucible_mcp_auth::access::{AccessRules, Call};
    use crucible_mcp_auth::principal::Callers;
    use crucible_mcp_auth::server::JwksTls;

    fn broker(client: kube::Client, tokens: &Path, local: Option<&Path>) -> Arc<Broker> {
        Arc::new(broker_value(client, tokens, local))
    }

    /// A broker for tests. A cluster test names its image repository and shared cache with
    /// BUILDIT_E2E_IMAGE_REPO (a registry pods and the kubelet reach over plain HTTP) and
    /// BUILDIT_E2E_CACHE_URL with BUILDIT_E2E_CACHE_TOKEN_FILE.
    fn broker_value(client: kube::Client, tokens: &Path, local: Option<&Path>) -> Broker {
        let namespace = client.default_namespace().to_string();
        let image_repo = std::env::var("BUILDIT_E2E_IMAGE_REPO")
            .unwrap_or_else(|_| "registry.invalid/buildit".to_string());
        let host = image_repo.split('/').next().unwrap_or_default().to_string();
        let shared_cache = std::env::var("BUILDIT_E2E_CACHE_URL")
            .ok()
            .map(|url| SharedCache {
                url: url.parse().unwrap(),
                token_file: std::env::var("BUILDIT_E2E_CACHE_TOKEN_FILE")
                    .ok()
                    .map(PathBuf::from),
                writers: CacheWriters::All,
            });
        let log_dir = tempfile::tempdir().unwrap().keep();
        Broker {
            client,
            tokens_file: Some(tokens.to_path_buf()),
            oidc: None,
            default_workdir: Some(SandboxDir::parse("/sandbox/default").unwrap()),
            local_workdir: local.map(Path::to_path_buf),
            site: jobs::Site {
                namespace,
                queue: std::env::var("BUILDIT_E2E_QUEUE").ok(),
                node_arch: std::env::var("BUILDIT_E2E_NODE_ARCH").ok(),
                resources: Resources::default(),
                storage: std::env::var("BUILDIT_E2E_STORAGE_DRIVER")
                    .map(|d| <StorageDriver as clap::ValueEnum>::from_str(&d, false).unwrap())
                    .unwrap_or_default(),
                bud_args: Vec::new(),
                users: match std::env::var("BUILDIT_E2E_POD_USERS").as_deref() {
                    Ok("host") => jobs::PodUsers::Host,
                    _ => jobs::PodUsers::Namespaced,
                },
                deadline_secs: 900,
                ttl_secs: 600,
                cache: shared_cache
                    .as_ref()
                    .map(SharedCache::base)
                    .unwrap_or_default(),
                image_repo,
                registry: match std::env::var("BUILDIT_E2E_REGISTRY_AUTH").as_deref() {
                    Ok("cluster") => jobs::Registry::Cluster { host },
                    _ => jobs::Registry::Insecure { host },
                },
            },
            max_builds: 4,
            log_dir,
            http: e2e_http(shared_cache.as_ref()),
            publishing: tokio::sync::Mutex::new(()),
            run_id: None,
            push: None,
            shared_cache,
            run_caller: None,
        }
    }

    /// A Dockerfile with Docker Hub's library images pulled through BUILDIT_E2E_LIBRARY, such as
    /// mirror.gcr.io/library, for a cluster that Docker Hub's anonymous rate limit reaches.
    fn library(dockerfile: &str) -> String {
        match std::env::var("BUILDIT_E2E_LIBRARY") {
            Ok(mirror) => dockerfile.replace("docker.io/library", &mirror),
            Err(_) => dockerfile.to_string(),
        }
    }

    /// The test's HTTP client. BUILDIT_E2E_CACHE_FORWARD, the local end of a port-forward to the
    /// cluster's cache, stands in for the cache's in-cluster name, which build pods use.
    fn e2e_http(cache: Option<&SharedCache>) -> reqwest::Client {
        let forward = std::env::var("BUILDIT_E2E_CACHE_FORWARD").ok();
        match (cache.and_then(|c| c.url.host_str()), forward) {
            (Some(host), Some(addr)) => reqwest::Client::builder()
                .resolve(host, addr.parse().unwrap())
                .build()
                .unwrap(),
            _ => reqwest::Client::new(),
        }
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
            cluster: None,
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
        let labels = broker.labels(&caller, &BuildId::parse("buildit-1").unwrap());
        assert_eq!(labels["crucible.dev/launched-by"], "alice");
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
    async fn realm_tokens_without_a_usable_sandbox_are_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        let (_jwks, broker) = realm_broker(&tokens(dir.path(), "")).await;
        let cases = [
            ("sandbox", None),
            ("sandbox", Some(serde_json::json!("not a name"))),
            ("workdir", Some(serde_json::json!("relative/path"))),
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
        let mut email = realm_claims();
        email["sub"] = serde_json::json!("alice@example.com");
        let caller = broker
            .authenticate(&bearer_header(&crucible_mcp_auth::testing::sign(
                "a", &email,
            )))
            .await
            .unwrap();
        let launcher = caller.0.launched_by.unwrap();
        assert!(
            launcher.as_str().starts_with("alice-example.com-"),
            "a subject that is no label value is kept as a label-safe one: {}",
            launcher.as_str()
        );
    }

    #[test]
    fn the_run_builds_under_one_name_per_run_that_no_sandbox_takes() {
        let a = run_sandbox(Some("crucible-run-brave-otter")).unwrap();
        assert_eq!(
            a,
            run_sandbox(Some("crucible-run-brave-otter")).unwrap(),
            "stable across restarts of the run's server"
        );
        assert_ne!(a, run_sandbox(Some("crucible-run-calm-heron")).unwrap());
        assert!(
            a.as_str().starts_with("loop-") && a.as_str().len() == 21,
            "{a:?}"
        );
        assert!(crate::sandbox::is_label_value(a.as_str()));
        assert_eq!(run_sandbox(None).unwrap().as_str(), "loop");
    }

    #[tokio::test]
    async fn no_token_may_name_the_runs_own_caller() {
        let run = run_sandbox(Some("crucible-run-brave-otter")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = tokens(
            dir.path(),
            &format!("tok-run {}\ntok-a sb-a\n", run.as_str()),
        );
        let (_jwks, realm) = realm_broker(&file).await;
        let broker = Broker {
            run_caller: Some(run.clone()),
            ..Arc::into_inner(realm).unwrap()
        };
        assert_eq!(
            broker
                .caller(&headers(&[("authorization", "Bearer tok-run")]))
                .await,
            Err(StatusCode::FORBIDDEN)
        );
        assert!(
            broker
                .caller(&headers(&[("authorization", "Bearer tok-a")]))
                .await
                .is_ok()
        );
        let mut claims = realm_claims();
        claims["sandbox"] = serde_json::json!(run.as_str());
        let token = crucible_mcp_auth::testing::sign("a", &claims);
        let refused = broker
            .authenticate(&bearer_header(&token))
            .await
            .unwrap_err();
        assert_eq!(
            status_of(broker.respond(&bearer_header(&token), refused))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let ours = crucible_mcp_auth::testing::sign("a", &realm_claims());
        assert!(broker.authenticate(&bearer_header(&ours)).await.is_ok());
    }

    #[tokio::test]
    async fn the_socket_replaces_a_stale_one_is_private_and_never_clobbers_a_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buildit.sock");
        drop(bind_socket(&path).unwrap());
        assert!(path.exists(), "a dropped listener leaves its socket file");
        let _live = bind_socket(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        tokio::net::UnixStream::connect(&path).await.unwrap();

        let file = dir.path().join("state.json");
        std::fs::write(&file, b"{}").unwrap();
        let e = bind_socket(&file).unwrap_err();
        assert!(format!("{e:#}").contains("not a socket"), "{e:#}");
        assert_eq!(std::fs::read(&file).unwrap(), b"{}");
    }

    #[tokio::test]
    async fn a_token_for_another_run_or_another_servers_audience_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (_jwks, broker) = realm_broker(&tokens(dir.path(), "")).await;
        let broker = Broker {
            run_id: Some("crucible-run-own".into()),
            ..Arc::into_inner(broker).unwrap()
        };
        let mut own = realm_claims();
        own["run_id"] = serde_json::json!("crucible-run-own");
        let token = crucible_mcp_auth::testing::sign("a", &own);
        broker
            .authenticate(&bearer_header(&token))
            .await
            .expect("this run's token");

        for (key, value) in [
            ("run_id", Some(serde_json::json!("crucible-run-other"))),
            ("run_id", None),
        ] {
            let mut claims = own.clone();
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

        let mut kueue = own.clone();
        kueue["aud"] =
            serde_json::json!(["http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp"]);
        let token = crucible_mcp_auth::testing::sign("a", &kueue);
        let refused = broker
            .authenticate(&bearer_header(&token))
            .await
            .unwrap_err();
        assert_eq!(
            status_of(broker.respond(&bearer_header(&token), refused))
                .await
                .0,
            StatusCode::UNAUTHORIZED,
            "a token minted for crucible-kueue is not buildit's"
        );
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
        assert!(reply.contains(r#""isError":true"#), "{reply}");
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
            cluster: None,
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

    #[test]
    fn a_push_goes_only_to_the_operators_repository_under_buildits_tag() {
        for bad in [
            "smoke",
            "docker://quay.io/org/smoke",
            "quay.io/org/smoke:latest",
            "quay.io/org/smoke@sha256:00",
            "Quay.io/Org/Smoke",
        ] {
            assert!(PushTarget::new(bad.into(), "/a".into()).is_err(), "{bad}");
        }
        let target = PushTarget::new("quay.io/example/crucible-smoke".into(), "/a".into()).unwrap();
        assert!(PushTarget::new("registry.local:5000/org/smoke".into(), "/a".into()).is_ok());
        let id = BuildId::parse("buildit-7").unwrap();
        let sandbox = SandboxName::parse("ci-0123abcd").unwrap();
        assert_eq!(
            target.reference(&sandbox, &id),
            "quay.io/example/crucible-smoke:ci-0123abcd-buildit-7"
        );
    }

    #[tokio::test]
    async fn push_is_refused_by_a_server_that_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let broker = broker(offline_client(), &tokens(dir.path(), ""), None);
        let err = broker
            .push(
                &sandbox_caller("sb-a"),
                PushParams {
                    build_id: "buildit-1".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--push-repo"), "{err:#}");
    }

    /// Each validation of `deploy/builder-admission.yaml` against `pod`: the messages of those that
    /// refuse it.
    fn refused_by_admission(pod: &serde_json::Value) -> Vec<String> {
        refused_on("CREATE", &serde_json::Value::Null, pod)
    }

    /// The same, for an update of `old` to `pod`.
    fn refused_on_update(old: &serde_json::Value, pod: &serde_json::Value) -> Vec<String> {
        refused_on("UPDATE", old, pod)
    }

    fn refused_on(
        operation: &str,
        old: &serde_json::Value,
        pod: &serde_json::Value,
    ) -> Vec<String> {
        let policy: serde_json::Value =
            serde_norway::from_str(include_str!("../deploy/builder-admission.yaml")).unwrap();
        let object = crucible_mcp_auth::policy::cel_value(pod).unwrap();
        let old_object = crucible_mcp_auth::policy::cel_value(old).unwrap();
        let request =
            crucible_mcp_auth::policy::cel_value(&serde_json::json!({"operation": operation}))
                .unwrap();
        let context = |variables: &serde_json::Value| {
            let mut context = cel::Context::default();
            context.add_variable_from_value("object", object.clone());
            context.add_variable_from_value("oldObject", old_object.clone());
            context.add_variable_from_value("request", request.clone());
            context.add_variable_from_value(
                "variables",
                crucible_mcp_auth::policy::cel_value(variables).unwrap(),
            );
            context
        };
        let mut variables = serde_json::json!({});
        for v in policy["spec"]["variables"].as_array().unwrap() {
            let program = cel::Program::compile(v["expression"].as_str().unwrap()).unwrap();
            let value = program.execute(&context(&variables)).unwrap();
            variables[v["name"].as_str().unwrap()] = value.json().unwrap();
        }
        policy["spec"]["validations"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| {
                let program = cel::Program::compile(v["expression"].as_str().unwrap()).unwrap();
                match program.execute(&context(&variables)) {
                    Ok(cel::Value::Bool(true)) => None,
                    Ok(cel::Value::Bool(false)) => Some(v["message"].as_str().unwrap().to_string()),
                    other => Some(format!("{}: {other:?}", v["expression"])),
                }
            })
            .collect()
    }

    /// The pod a Job of buildit's runs, as the Job controller creates it.
    fn pod_of(job: &k8s_openapi::api::batch::v1::Job) -> serde_json::Value {
        let template = &job.spec.as_ref().unwrap().template;
        serde_json::json!({
            "metadata": template.metadata,
            "spec": template.spec,
        })
    }

    /// The builder namespace's admission policy admits the pods buildit's Jobs run, and nothing
    /// that could reach the node from there.
    #[tokio::test]
    async fn the_builder_admission_policy_admits_buildits_pods_and_no_escape() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker_value(offline_client(), &tokens(dir.path(), ""), None);
        broker.site.registry = jobs::Registry::Cluster {
            host: "image-registry.openshift-image-registry.svc:5000".to_string(),
        };
        let id = BuildId::fresh();
        let labels = broker.labels(&sandbox_caller("sb-a"), &id);
        let (run_name, push_name) = (format!("{}-run-1", id.0), format!("{}-push-1", id.0));
        let ident = |name| jobs::Ident {
            name,
            build: id.as_str(),
            sandbox: "sb-a",
            labels: &labels,
            annotations: BTreeMap::new(),
        };
        let runner_args = ["30".to_string()];
        let pod = pod_of(&jobs::build_job(
            &broker.site,
            &ident(id.as_str()),
            &jobs::BuildJob {
                context_digest: "sha256:00",
                cache_dirs: &[],
                runner_args: &runner_args,
                local_tag: &id.tag(),
                limit_secs: 60,
            },
        ));
        assert_eq!(refused_by_admission(&pod), Vec::<String>::new());
        let cmd = ["true".to_string()];
        let run = pod_of(&jobs::run_job(
            &broker.site,
            &ident(&run_name),
            &jobs::RunJob {
                n: 1,
                image: "image-registry.openshift-image-registry.svc:5000/b/buildit@sha256:00",
                cmd: &cmd,
                fetch_paths: &[],
                limit_secs: 60,
            },
        ));
        assert_eq!(refused_by_admission(&run), Vec::<String>::new());
        let push = pod_of(&jobs::push_job(
            &broker.site,
            &ident(&push_name),
            &jobs::PushJob {
                source: "image-registry.openshift-image-registry.svc:5000/b/buildit@sha256:00",
                dest: "quay.io/acme/b:sb-a-x",
            },
        ));
        assert_eq!(refused_by_admission(&push), Vec::<String>::new());
        let layer_caches = [("deps".to_string(), "/src/app/.deps".to_string())];
        let layer = pod_of(&jobs::layer_job(
            &broker.site,
            &ident(id.as_str()),
            &jobs::LayerJob {
                context_digest: "sha256:00",
                base: "docker.io/acme/base@sha256:00",
                script: "make",
                path: "/src/app",
                env: &BTreeMap::new(),
                caches: &layer_caches,
                ccache: true,
                limit_secs: 60,
            },
        ));
        assert_eq!(refused_by_admission(&layer), Vec::<String>::new());
        let script_reads_the_token = {
            let mut p = layer.clone();
            p["spec"]["containers"][0]["volumeMounts"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"name": "cluster", "mountPath": "/t"}));
            p
        };
        assert!(
            !refused_by_admission(&script_reads_the_token).is_empty(),
            "a layer build's script never mounts the account's token"
        );
        let by_tag = {
            let mut p = run.clone();
            p["spec"]["containers"][0]["image"] = serde_json::json!("docker.io/library/alpine");
            p
        };
        assert!(
            !refused_by_admission(&by_tag).is_empty(),
            "a run's image goes by digest"
        );
        let run_reads_the_token = {
            let mut p = run.clone();
            p["spec"]["containers"][0]["volumeMounts"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"name": "archive-auth", "mountPath": "/t"}));
            p
        };
        assert!(
            !refused_by_admission(&run_reads_the_token).is_empty(),
            "a run never mounts the write token"
        );
        let fetch_reads_the_account = {
            let mut p = pod.clone();
            p["spec"]["initContainers"][0]["volumeMounts"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"name": "cluster", "mountPath": "/t"}));
            p
        };
        assert!(
            !refused_by_admission(&fetch_reads_the_account).is_empty(),
            "nor does the fetch mount the account's token"
        );
        let mut injected = pod.clone();
        injected["spec"]["imagePullSecrets"] =
            serde_json::json!([{"name": "default-dockercfg-4pvtr"}]);
        assert_eq!(
            refused_by_admission(&injected),
            Vec::<String>::new(),
            "the internal registry pull Secret OpenShift adds to the default account"
        );

        let escape = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut p = pod.clone();
            edit(&mut p);
            p
        };
        let escapes: Vec<(&str, serde_json::Value)> = vec![
            (
                "privileged",
                escape(&|p| {
                    p["spec"]["containers"][0]["securityContext"]["privileged"] =
                        serde_json::json!(true)
                }),
            ),
            (
                "added capability",
                escape(&|p| {
                    p["spec"]["containers"][0]["securityContext"]["capabilities"] =
                        serde_json::json!({"add": ["SYS_ADMIN"]})
                }),
            ),
            (
                "host users",
                escape(&|p| p["spec"]["hostUsers"] = serde_json::json!(true)),
            ),
            (
                "no user namespace",
                escape(&|p| {
                    p["spec"].as_object_mut().unwrap().remove("hostUsers");
                }),
            ),
            (
                "a privileged sidecar",
                escape(&|p| {
                    p["spec"]["initContainers"][1]["securityContext"]["privileged"] =
                        serde_json::json!(true)
                }),
            ),
            (
                "an archive that is no sidecar",
                escape(&|p| {
                    p["spec"]["initContainers"][1]
                        .as_object_mut()
                        .unwrap()
                        .remove("restartPolicy");
                }),
            ),
            (
                "another Secret",
                escape(&|p| {
                    p["spec"]["volumes"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::json!({"name": "x", "secret": {"secretName": "buildit-cache-token"}}))
                }),
            ),
            (
                "another build's inputs under a made-up build",
                escape(&|p| {
                    p["metadata"]["labels"]["buildit.dev/build-id"] = serde_json::json!("default");
                }),
            ),
            (
                "a projected Secret",
                escape(&|p| {
                    p["spec"]["volumes"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::json!({"name": "x", "projected": {"sources": [{"secret": {"name": "s"}}]}}))
                }),
            ),
            (
                "a token for another audience",
                escape(&|p| {
                    p["spec"]["volumes"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::json!({"name": "x", "projected": {"sources": [{"serviceAccountToken": {"path": "t", "audience": "vault"}}]}}))
                }),
            ),
            (
                "an AppArmor field",
                escape(&|p| {
                    p["spec"]["containers"][0]["securityContext"]["appArmorProfile"] =
                        serde_json::json!({"type": "Unconfined"})
                }),
            ),
            (
                "seccomp off",
                escape(&|p| {
                    p["spec"]["containers"][0]["securityContext"]["seccompProfile"] =
                        serde_json::json!({"type": "Unconfined"})
                }),
            ),
            (
                "another image",
                escape(&|p| p["spec"]["containers"][0]["image"] = serde_json::json!("alpine")),
            ),
            (
                "a second container",
                escape(&|p| {
                    p["spec"]["containers"].as_array_mut().unwrap().push(
                        serde_json::json!({"name": "x", "image": "quay.io/buildah/stable:latest"}),
                    )
                }),
            ),
            (
                "an init container",
                escape(&|p| {
                    p["spec"]["initContainers"] =
                        serde_json::json!([{"name": "x", "image": "quay.io/buildah/stable:latest"}])
                }),
            ),
            (
                "an ephemeral container",
                escape(&|p| {
                    p["spec"]["ephemeralContainers"] =
                        serde_json::json!([{"name": "x", "image": "alpine"}])
                }),
            ),
            (
                "hostPath",
                escape(&|p| {
                    p["spec"]["volumes"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::json!({"name": "root", "hostPath": {"path": "/"}}))
                }),
            ),
            (
                "hostNetwork",
                escape(&|p| p["spec"]["hostNetwork"] = serde_json::json!(true)),
            ),
            (
                "hostPID",
                escape(&|p| p["spec"]["hostPID"] = serde_json::json!(true)),
            ),
            (
                "hostIPC",
                escape(&|p| p["spec"]["hostIPC"] = serde_json::json!(true)),
            ),
            (
                "a token",
                escape(&|p| p["spec"]["automountServiceAccountToken"] = serde_json::json!(true)),
            ),
            (
                "a Secret in env",
                escape(&|p| {
                    p["spec"]["containers"][0]["env"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::json!({"name": "T", "valueFrom": {"secretKeyRef": {"name": "s", "key": "k"}}}))
                }),
            ),
            (
                "a ConfigMap in env",
                escape(&|p| {
                    p["spec"]["containers"][0]["env"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::json!({"name": "T", "valueFrom": {"configMapKeyRef": {"name": "c", "key": "k"}}}))
                }),
            ),
            (
                "envFrom",
                escape(&|p| {
                    p["spec"]["containers"][0]["envFrom"] =
                        serde_json::json!([{"secretRef": {"name": "s"}}])
                }),
            ),
            (
                "nodeName",
                escape(&|p| p["spec"]["nodeName"] = serde_json::json!("node-1")),
            ),
            (
                "a runtime class",
                escape(&|p| p["spec"]["runtimeClassName"] = serde_json::json!("kata")),
            ),
            (
                "a system priority class",
                escape(&|p| {
                    p["spec"]["priorityClassName"] = serde_json::json!("system-node-critical")
                }),
            ),
            (
                "a toleration",
                escape(
                    &|p| p["spec"]["tolerations"] = serde_json::json!([{"key": "node-role.kubernetes.io/control-plane", "operator": "Exists", "effect": "NoSchedule"}]),
                ),
            ),
            (
                "tolerate everything",
                escape(&|p| p["spec"]["tolerations"] = serde_json::json!([{"operator": "Exists"}])),
            ),
            (
                "a sysctl",
                escape(
                    &|p| p["spec"]["securityContext"] = serde_json::json!({"sysctls": [{"name": "kernel.shm_rmid_forced", "value": "0"}]}),
                ),
            ),
            (
                "an SELinux type",
                escape(&|p| {
                    p["spec"]["containers"][0]["securityContext"]["seLinuxOptions"] =
                        serde_json::json!({"type": "spc_t"})
                }),
            ),
            (
                "a pod SELinux user",
                escape(&|p| {
                    p["spec"]["securityContext"] =
                        serde_json::json!({"seLinuxOptions": {"user": "system_u"}})
                }),
            ),
            (
                "an unmasked /proc",
                escape(&|p| {
                    p["spec"]["containers"][0]["securityContext"]["procMount"] =
                        serde_json::json!("Unmasked")
                }),
            ),
            (
                "a pull Secret",
                escape(&|p| {
                    p["spec"]["imagePullSecrets"] = serde_json::json!([{"name": "quay-pull"}])
                }),
            ),
            (
                "another account",
                escape(&|p| p["spec"]["serviceAccountName"] = serde_json::json!("crucible-spoke")),
            ),
            (
                "no label",
                escape(&|p| {
                    p["metadata"]["labels"]
                        .as_object_mut()
                        .unwrap()
                        .remove("buildit.dev/managed-by");
                }),
            ),
            (
                "apparmor elsewhere",
                escape(&|p| {
                    p["metadata"]["annotations"]["container.apparmor.security.beta.kubernetes.io/x"] =
                        serde_json::json!("unconfined")
                }),
            ),
            (
                "apparmor profile",
                escape(&|p| {
                    p["metadata"]["annotations"]["container.apparmor.security.beta.kubernetes.io/builder"] =
                        serde_json::json!("localhost/anything")
                }),
            ),
            (
                "pod apparmor field",
                escape(&|p| {
                    p["spec"]["securityContext"] =
                        serde_json::json!({"appArmorProfile": {"type": "Unconfined"}})
                }),
            ),
        ];
        for (name, escaped) in escapes {
            assert!(
                !refused_by_admission(&escaped).is_empty(),
                "{name} was admitted"
            );
        }
        let defaulted = escape(&|p| {
            p["spec"]["tolerations"] = serde_json::json!([
                {"key": "node.kubernetes.io/not-ready", "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 300},
                {"key": "node.kubernetes.io/unreachable", "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 300},
            ]);
            p["spec"]["priorityClassName"] = serde_json::json!("default-priority");
            p["spec"]["securityContext"] =
                serde_json::json!({"seLinuxOptions": {"level": "s0:c26,c5"}});
            p["spec"]["containers"][0]["securityContext"]["procMount"] =
                serde_json::json!("Default");
        });
        assert_eq!(
            refused_by_admission(&defaulted),
            Vec::<String>::new(),
            "what admission defaults every pod with"
        );
        let scheduled = escape(&|p| p["spec"]["nodeName"] = serde_json::json!("node-1"));
        assert_eq!(
            refused_on_update(&pod, &scheduled),
            Vec::<String>::new(),
            "a scheduled pod keeps its node on update"
        );
        let swapped = escape(&|p| {
            p["spec"]["nodeName"] = serde_json::json!("node-1");
            p["spec"]["containers"][0]["image"] = serde_json::json!("alpine");
        });
        assert!(
            !refused_on_update(&pod, &swapped).is_empty(),
            "an update cannot swap the image"
        );
    }

    #[test]
    fn a_subject_that_is_no_label_value_still_labels_its_builds_distinctly() {
        assert_eq!(Launcher::of("alice").as_str(), "alice");
        let email = Launcher::of("alice@example.com");
        assert!(
            crate::sandbox::is_label_value(email.as_str()),
            "{}",
            email.as_str()
        );
        assert!(
            email.as_str().starts_with("alice-example.com-"),
            "{}",
            email.as_str()
        );
        assert_ne!(email, Launcher::of("alice#example.com"));
        let long = Launcher::of(&format!("{}@example.com", "a".repeat(80)));
        assert!(
            crate::sandbox::is_label_value(long.as_str()),
            "{}",
            long.as_str()
        );
        assert!(crate::sandbox::is_label_value(Launcher::of("@@@").as_str()));
    }

    #[test]
    fn a_build_is_told_the_cache_url_and_the_token_only_when_it_may_write() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        let cache = |token_file: Option<&Path>, writers| SharedCache {
            url: url::Url::parse("http://buildit-cache.crucible-system.svc:8850").unwrap(),
            token_file: token_file.map(Path::to_path_buf),
            writers,
        };
        let doc = |c: &SharedCache, starter| -> serde_json::Value {
            serde_json::from_slice(&c.document(starter)).unwrap()
        };
        let url = "http://buildit-cache.crucible-system.svc:8850/";
        std::fs::write(&token, "  tok-1\n").unwrap();
        let run_writes = cache(Some(&token), CacheWriters::Run);
        assert_eq!(
            doc(&run_writes, Starter::Run),
            serde_json::json!({ "url": url, "token": "tok-1" })
        );
        assert_eq!(
            doc(&run_writes, Starter::Sandbox),
            serde_json::json!({ "url": url })
        );
        let all_write = cache(Some(&token), CacheWriters::All);
        assert_eq!(doc(&all_write, Starter::Sandbox)["token"], "tok-1");
        std::fs::write(&token, "tok-2").unwrap();
        assert_eq!(
            doc(&run_writes, Starter::Run)["token"],
            "tok-2",
            "re-read per build"
        );
        std::fs::write(&token, "\n").unwrap();
        assert_eq!(
            doc(&run_writes, Starter::Run),
            serde_json::json!({ "url": url })
        );
        std::fs::remove_file(&token).unwrap();
        assert_eq!(
            doc(&run_writes, Starter::Run),
            serde_json::json!({ "url": url })
        );
        assert_eq!(
            doc(&cache(None, CacheWriters::All), Starter::Run),
            serde_json::json!({ "url": url })
        );
    }

    /// A build's snapshot plan holds its cluster caches alone, each keyed from the context, and
    /// saves only what the build mounts read-write and may write to the shared cache.
    #[test]
    fn a_build_saves_the_cluster_caches_it_may_write_and_restores_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "tok-1\n").unwrap();
        let ctx = dir.path().join("ctx");
        std::fs::create_dir_all(ctx.join("vllm/cmake")).unwrap();
        std::fs::write(ctx.join("vllm/cmake/deps.cmake"), "GIT_TAG 9cd61de\n").unwrap();
        let tar = crate::context::staged_tar_bytes(&ctx).unwrap();
        let cache = |writers| SharedCache {
            url: url::Url::parse("http://cache.example.svc:8850/").unwrap(),
            token_file: Some(token.clone()),
            writers,
        };
        let spec = |name: &str, read_only, scope, key: &[&str]| CacheSpec {
            name: name.to_string(),
            path: format!("/{name}"),
            read_only,
            scope,
            key: key.iter().map(|k| k.to_string()).collect(),
        };
        let caches = [
            spec("deps", false, CacheScope::Cluster, &["vllm/cmake"]),
            spec("pinned", true, CacheScope::Cluster, &[]),
            spec("ccache", false, CacheScope::Pod, &[]),
        ];
        let deps =
            crate::context::cache_key("deps", &tar, &[RelPath::parse("vllm/cmake").unwrap()])
                .unwrap();
        let pinned = crate::context::cache_key("pinned", &tar, &[]).unwrap();
        let plan = |uses: [&str; 2]| {
            format!(
                "deps {deps} {CACHE_ROOT}/deps {}\npinned {pinned} {CACHE_ROOT}/pinned {}\n",
                uses[0], uses[1]
            )
        };
        let header = Some("Authorization: Bearer tok-1\n".to_string());
        assert_eq!(
            cache(CacheWriters::Run)
                .snapshots(&caches, &tar, Starter::Run, CACHE_ROOT)
                .unwrap(),
            Snapshots {
                plan: plan(["save", "restore"]),
                auth: header.clone(),
            }
        );
        assert_eq!(
            cache(CacheWriters::Run)
                .snapshots(&caches, &tar, Starter::Sandbox, CACHE_ROOT)
                .unwrap(),
            Snapshots {
                plan: plan(["restore", "restore"]),
                auth: None,
            }
        );
        assert_eq!(
            cache(CacheWriters::All)
                .snapshots(&caches, &tar, Starter::Sandbox, CACHE_ROOT)
                .unwrap(),
            Snapshots {
                plan: plan(["save", "restore"]),
                auth: header,
            }
        );
        assert_eq!(
            cache(CacheWriters::Run)
                .snapshots(&caches[2..], &tar, Starter::Run, CACHE_ROOT)
                .unwrap(),
            Snapshots::default()
        );
        assert_eq!(
            cache(CacheWriters::Run).base(),
            "http://cache.example.svc:8850"
        );
    }

    #[test]
    fn a_build_that_asks_for_ccache_gets_a_conf_for_the_cache_it_may_use() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        let cache = |url: &str, token_file: Option<&Path>, writers| SharedCache {
            url: url::Url::parse(url).unwrap(),
            token_file: token_file.map(Path::to_path_buf),
            writers,
        };
        let stats = "stats_log = /buildit/ccache-stats.log\n";
        assert_eq!(ccache_conf(None, Starter::Run), stats);

        let read_only = cache("http://cache.example.svc:8850/", None, CacheWriters::All);
        assert_eq!(
            ccache_conf(Some(&read_only), Starter::Run),
            format!("{stats}secondary_storage = http://cache.example.svc:8850/ccache|read-only\n")
        );

        std::fs::write(&token, "  t0k/en+1\n").unwrap();
        let shared = "http://cache.example.svc:8850/shared/";
        let run_writes = cache(shared, Some(&token), CacheWriters::Run);
        assert_eq!(
            run_writes.ccache_storage(Starter::Run),
            "http://buildit:t0k%2Fen+1@cache.example.svc:8850/shared/ccache",
            "the token is the basic-auth password, escaped"
        );
        assert_eq!(
            run_writes.ccache_storage(Starter::Sandbox),
            "http://cache.example.svc:8850/shared/ccache|read-only"
        );
        let all_write = cache(shared, Some(&token), CacheWriters::All);
        assert_eq!(
            all_write.ccache_storage(Starter::Sandbox),
            "http://buildit:t0k%2Fen+1@cache.example.svc:8850/shared/ccache"
        );
        std::fs::write(&token, "\n").unwrap();
        assert_eq!(
            run_writes.ccache_storage(Starter::Run),
            "http://cache.example.svc:8850/shared/ccache|read-only",
            "an empty token reads only"
        );

        let id = BuildId::fresh();
        let df = RelPath::parse("Dockerfile").unwrap();
        let args = BTreeMap::new();
        let conf = format!("{}/ccache.conf:/etc/ccache.conf:ro", jobs::FACTS);
        let with = bud_argv(&id, &df, None, &args, &[], true, &[]).unwrap();
        assert!(
            with.windows(2).any(|w| w == ["--volume", conf.as_str()]),
            "{with:?}"
        );
        let without = bud_argv(&id, &df, None, &args, &[], false, &[]).unwrap();
        assert!(!without.iter().any(|a| a.contains("ccache")), "{without:?}");

        let args = BTreeMap::from([("A".to_string(), "1".to_string())]);
        let extra = ["--retry".to_string(), "5".to_string()];
        let tuned = bud_argv(&id, &df, None, &args, &[], false, &extra).unwrap();
        let at = |a: &str| tuned.iter().position(|x| x == a).unwrap();
        assert!(
            at("--build-arg") < at("--retry") && at("5") + 1 == at("-f"),
            "the site's arguments follow buildit's own: {tuned:?}"
        );
    }

    #[test]
    fn declared_caches_are_plain_named_paths_buildit_does_not_own() {
        let spec = |name: &str, path: &str, read_only| CacheSpec {
            name: name.to_string(),
            path: path.to_string(),
            read_only,
            scope: CacheScope::Pod,
            key: Vec::new(),
        };
        let ok = cache_volumes(&[
            spec("ccache", "/root/.cache/ccache", false),
            spec("ext-src-2", "/var/cache/ext", true),
        ])
        .unwrap();
        assert_eq!(
            ok,
            [
                (
                    "/home/build/.local/share/containers/buildit-caches/ccache".to_string(),
                    "/home/build/.local/share/containers/buildit-caches/ccache:/root/.cache/ccache:rw"
                        .to_string()
                ),
                (
                    "/home/build/.local/share/containers/buildit-caches/ext-src-2".to_string(),
                    "/home/build/.local/share/containers/buildit-caches/ext-src-2:/var/cache/ext:ro"
                        .to_string()
                ),
            ]
        );
        for bad in [
            vec![spec("Cache", "/c", false)],
            vec![spec("-c", "/c", false)],
            vec![spec("", "/c", false)],
            vec![spec(&"a".repeat(64), "/c", false)],
            vec![spec("c", "relative", false)],
            vec![spec("c", "/", false)],
            vec![spec("c", "/a/../b", false)],
            vec![spec("c", "/a/./b", false)],
            vec![spec("c", "/a:b", false)],
            vec![spec("c", "/a,ro", false)],
            vec![spec("c", "/buildit", false)],
            vec![spec("c", "/buildit/x", false)],
            vec![spec("c", "/a", false), spec("c", "/b", false)],
            vec![spec("c", "/a", false), spec("d", "/a", false)],
            (0..9)
                .map(|i| spec(&format!("c{i}"), &format!("/c{i}"), false))
                .collect(),
        ] {
            assert!(cache_volumes(&bad).is_err(), "{bad:?}");
        }
        let keyed = |scope, key: &str| CacheSpec {
            scope,
            key: vec![key.to_string()],
            ..spec("deps", "/src/vllm/.deps", false)
        };
        assert!(cache_volumes(&[keyed(CacheScope::Cluster, "vllm/cmake")]).is_ok());
        for bad in [
            keyed(CacheScope::Pod, "vllm/cmake"),
            keyed(CacheScope::Cluster, "/vllm/cmake"),
            keyed(CacheScope::Cluster, "vllm/../cmake"),
            keyed(CacheScope::Cluster, "."),
        ] {
            assert!(
                cache_volumes(std::slice::from_ref(&bad)).is_err(),
                "{bad:?}"
            );
        }
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
            library(
                "FROM docker.io/library/busybox:latest AS base\n\
             RUN echo hello-from-build && sleep 20 && mkdir -p /out && echo artifact > /out/a.txt\n\
             FROM base AS broken\n\
             RUN exit 1\n",
            ),
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
            "cmd": ["sh", "-c", "echo out-1; mkdir -p /made && echo made > /made/new.txt; exit 5"],
            "fetch_paths": ["/out/a.txt", "/made", "/nope"],
        });
        let ran = a("run", args).await.unwrap();
        assert_eq!(ran["exit"], 5, "{ran}");
        assert_eq!(ran["log"], "run:1");
        assert!(ran["log_tail"].as_str().unwrap().contains("out-1"), "{ran}");
        let fetched = format!(".buildit/{id}/out/a.txt");
        let made = format!(".buildit/{id}/made/new.txt");
        assert_eq!(
            ran["fetched"],
            serde_json::json!([fetched, made]),
            "a path the command creates is fetched like one the image had"
        );
        let body = std::fs::read_to_string(work.join(&fetched)).unwrap();
        assert_eq!(body, "artifact\n");
        let body = std::fs::read_to_string(work.join(&made)).unwrap();
        assert_eq!(body, "made\n");

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
        assert_eq!(line["lines"], "1:out-1\n", "{line}");
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
            assert!(e.contains("no build"), "{tool}: {e}");
        }
        let cleaned = b("clean", only_id.clone()).await.unwrap();
        assert_eq!(cleaned["deleted"], serde_json::json!([]));
        let cleaned = a("clean", serde_json::json!({ "build_id": broken }))
            .await
            .unwrap();
        assert_eq!(cleaned["deleted"], serde_json::json!([broken]));
        let cleaned = a("clean", serde_json::json!({})).await.unwrap();
        assert_eq!(
            cleaned["deleted"],
            serde_json::json!([id, format!("{id}-run-1"), format!("{id}-run-2")])
        );
        let kept = a("status", only_id.clone()).await.unwrap();
        assert_eq!(
            kept["status"], "succeeded",
            "a cleaned build's record stays: {kept}"
        );
        let grep = serde_json::json!({ "build_id": id, "grep": "^hello-from-build$" });
        assert_eq!(
            a("logs", grep).await.unwrap()["matched"],
            1,
            "and so does its log"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: BUILDIT_E2E_KUBECONTEXT=kind-x cargo test -- --ignored"]
    async fn e2e_a_layer_build_runs_its_script_in_the_base_and_appends_its_path() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("BUILDIT_E2E_KUBECONTEXT").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(work.join("svc")).unwrap();
        std::fs::write(work.join("svc/hello.txt"), "from the context\n").unwrap();
        let file = tokens(&work, "tok-a sb-layer\n");
        let client = crate::client_for(Some(&context)).await.unwrap();
        let (addr, _server) = serve(broker(client, &file, Some(&work))).await;
        let a = |tool, args| call(&addr, "tok-a", tool, args);
        a("clean", serde_json::json!({})).await.unwrap();

        let base = library("docker.io/library/busybox:latest");
        let refused = a(
            "build",
            serde_json::json!({ "context": "svc", "base": base, "script": "true", "path": "/" }),
        )
        .await
        .unwrap_err();
        assert!(refused.contains("plain absolute directory"), "{refused}");
        let built = a(
            "build",
            serde_json::json!({
                "context": "svc",
                "base": base,
                "path": "/app",
                "script": "cat hello.txt > built.txt\necho \"$GREETING\" >> built.txt\n\
                           echo cached > deps/x\necho layer-script-ran",
                "build_args": { "GREETING": "hi from a build arg" },
                "caches": [{ "name": "layer-deps", "path": "/app/deps", "scope": "cluster", "key": ["hello.txt"] }],
            }),
        )
        .await
        .unwrap();
        let id = built["build_id"].as_str().unwrap().to_string();
        let only_id = serde_json::json!({ "build_id": id });
        let mut status = serde_json::Value::Null;
        for _ in 0..150 {
            status = a("status", only_id.clone()).await.unwrap();
            if status["status"] != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert_eq!(status["status"], "succeeded", "{status}");
        assert!(
            status["log_tail"]
                .as_str()
                .unwrap()
                .contains("layer-script-ran"),
            "{status}"
        );

        let ran = a(
            "run",
            serde_json::json!({
                "build_id": id,
                "cmd": ["sh", "-c", "cat /app/built.txt; ls -a /app"],
            }),
        )
        .await
        .unwrap();
        assert_eq!(ran["exit"], 0, "{ran}");
        let log = serde_json::json!({ "build_id": id, "which": "run:1" });
        let lines = a("logs", log).await.unwrap()["lines"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(lines.contains(":from the context\n"), "{lines}");
        assert!(lines.contains(":hi from a build arg\n"), "{lines}");
        assert!(lines.contains(":built.txt\n"), "{lines}");
        assert!(
            !lines.contains(":deps\n"),
            "a cache under path stays out of the layer: {lines}"
        );
        a("clean", serde_json::json!({})).await.unwrap();
    }

    /// The shared cache an e2e build uses: a deployed one (BUILDIT_E2E_CACHE_URL and
    /// BUILDIT_E2E_CACHE_TOKEN_FILE), or one served from this host, which pods reach at
    /// BUILDIT_E2E_HOST_ADDR. True when it is local, so its blobs are on disk under `work`.
    async fn e2e_cache(work: &Path, cache_token: &Path) -> (String, bool) {
        match std::env::var("BUILDIT_E2E_CACHE_URL") {
            Ok(url) => {
                let token = std::env::var("BUILDIT_E2E_CACHE_TOKEN_FILE").unwrap();
                std::fs::copy(token, cache_token).unwrap();
                (url, false)
            }
            Err(_) => {
                let host_addr = std::env::var("BUILDIT_E2E_HOST_ADDR").unwrap();
                std::fs::write(cache_token, "e2e-cache-token\n").unwrap();
                let store = crate::cache::Store::open(
                    &work.join("cache"),
                    cache_token.to_path_buf(),
                    u64::MAX,
                    1 << 30,
                )
                .unwrap();
                let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
                let port = listener.local_addr().unwrap().port();
                tokio::spawn(async move {
                    axum::serve(listener, crate::cache::router(Arc::new(store))).await
                });
                (format!("http://{host_addr}:{port}"), true)
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: BUILDIT_E2E_KUBECONTEXT=kind-x and either \
                BUILDIT_E2E_HOST_ADDR=<this host's address from a pod> for a cache on this host, or \
                BUILDIT_E2E_CACHE_URL and BUILDIT_E2E_CACHE_TOKEN_FILE for a deployed one"]
    async fn e2e_a_plain_dockerfile_reads_the_ccache_objects_the_runs_own_build_seeded() {
        use crate::client::{Call, run as on_socket};
        use crate::mcp::CleanParams;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("BUILDIT_E2E_KUBECONTEXT").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap();
        let cache_token = work.join("cache-token");
        let run = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let (cache_url, _) = e2e_cache(&work, &cache_token).await;
        std::fs::create_dir_all(work.join("svc")).unwrap();
        let source = |factor: u32| {
            std::fs::write(
                work.join("svc/a.c"),
                format!(
                    "int plain(int x) {{ return x * {factor} + {}; }}\n",
                    run % 1_000_000_007
                ),
            )
            .unwrap()
        };
        source(7);
        std::fs::write(
            work.join("svc/Dockerfile"),
            library("FROM docker.io/library/alpine:3.20\n\
             RUN apk add --no-cache ccache gcc musl-dev\n\
             COPY a.c /src/\n\
             ARG NONCE\n\
             RUN set -e; export CCACHE_DIR=/tmp/ccache; \\\n\
               ccache -z > /dev/null; ccache gcc -c /src/a.c -o /tmp/a.o; cp /tmp/a.o /objs/; \\\n\
               stats=$(ccache --print-stats); \\\n\
               hits=$(echo \"$stats\" | awk '$1 == \"remote_storage_hit\" || $1 == \"secondary_storage_hit\" { n += $2 } END { print n + 0 }'); \\\n\
               misses=$(echo \"$stats\" | awk '$1 == \"cache_miss\" { n += $2 } END { print n + 0 }'); \\\n\
               echo \"plain-ccache nonce=$NONCE remote_hits=$hits misses=$misses\"\n"),
        )
        .unwrap();
        let file = tokens(&work, "tok-a sb-plain-a\ntok-b sb-plain-b\n");
        let client = crate::client_for(Some(&context)).await.unwrap();
        let run_caller = run_sandbox(Some("buildit-plain-e2e")).unwrap();
        let broker = Arc::new(Broker {
            shared_cache: Some(SharedCache {
                url: url::Url::parse(&cache_url).unwrap(),
                token_file: Some(cache_token.clone()),
                writers: CacheWriters::Run,
            }),
            run_caller: Some(run_caller.clone()),
            ..broker_value(client, &file, Some(&work))
        });
        let (addr, _server) = serve(broker.clone()).await;
        let socket = tmp.path().join("b.sock");
        let listener = bind_socket(&socket).unwrap();
        let ours = Caller {
            sandbox: run_caller,
            workspace: Workspace::Local { dir: work.clone() },
            launched_by: None,
        };
        let app = socket_router(broker, ours, ToolPolicy::from_toml(POLICY).unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for token in ["tok-a", "tok-b"] {
            call(&addr, token, "clean", serde_json::json!({}))
                .await
                .unwrap();
        }
        on_socket(&socket, Call::Clean(CleanParams { build_id: None })).await;
        let caches = || {
            vec![CacheSpec {
                name: "plain-objs".to_string(),
                path: "/objs".to_string(),
                read_only: false,
                scope: CacheScope::Pod,
                key: Vec::new(),
            }]
        };
        let build = |token: &'static str, nonce: String| {
            let addr = addr.clone();
            async move {
                let args = serde_json::json!({
                    "context": "svc", "timeout_s": 600, "ccache": true,
                    "caches": [{ "name": "plain-objs", "path": "/objs" }],
                    "build_args": { "NONCE": nonce },
                });
                let started = call(&addr, token, "build", args).await.unwrap();
                let id = started["build_id"].as_str().unwrap().to_string();
                let args = serde_json::json!({ "build_id": id });
                for _ in 0..300 {
                    let status = call(&addr, token, "status", args.clone()).await.unwrap();
                    if status["status"] != "running" {
                        assert_eq!(status["status"], "succeeded", "{status}");
                        let grep = serde_json::json!({ "build_id": id, "grep": "^plain-ccache " });
                        let lines = call(&addr, token, "logs", grep).await.unwrap();
                        let line = lines["lines"].as_str().unwrap_or_default().trim_end();
                        return (id, line.to_string());
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                panic!("build {id} still running after 600s");
            }
        };

        let seeded = on_socket(
            &socket,
            Call::Build {
                params: BuildParams {
                    context: "svc".to_string(),
                    dockerfile: None,
                    target: None,
                    build_args: BTreeMap::from([("NONCE".to_string(), format!("{run}-seed"))]),
                    timeout_s: Some(600),
                    caches: caches(),
                    ccache: true,
                    base: None,
                    script: None,
                    path: None,
                },
                wait: true,
            },
        )
        .await;
        assert_eq!(seeded.reply["status"], "succeeded", "{}", seeded.reply);
        let seed_log = on_socket(
            &socket,
            Call::Logs(LogsParams {
                build_id: seeded.reply["build_id"].as_str().unwrap().to_string(),
                which: None,
                grep: Some("^plain-ccache ".to_string()),
                context: None,
                from_line: None,
                to_line: None,
                tail_lines: None,
                max_bytes: None,
            }),
        )
        .await;
        assert!(
            seed_log.reply["lines"]
                .as_str()
                .unwrap_or_default()
                .trim_end()
                .ends_with(&format!(
                    "plain-ccache nonce={run}-seed remote_hits=0 misses=1"
                )),
            "the run's own build compiles: {}",
            seed_log.reply
        );
        let (b, line) = build("tok-b", format!("{run}-b")).await;
        assert!(
            line.ends_with(&format!(
                "plain-ccache nonce={run}-b remote_hits=1 misses=0"
            )),
            "a sandbox's Dockerfile that never names the cache hits the run's object: {line}"
        );
        source(11);
        let (_, line) = build("tok-a", format!("{run}-a")).await;
        assert!(
            line.ends_with(&format!(
                "plain-ccache nonce={run}-a remote_hits=0 misses=1"
            )),
            "{line}"
        );
        let (_, line) = build("tok-b", format!("{run}-b2")).await;
        assert!(
            line.ends_with(&format!(
                "plain-ccache nonce={run}-b2 remote_hits=0 misses=1"
            )),
            "a sandbox's build stores nothing another sandbox could hit: {line}"
        );

        let read = serde_json::json!({
            "build_id": b,
            "cmd": ["sh", "-c", "cat /buildit/ccache-stats.log /buildit/steps.log; test -e /etc/ccache.conf || echo no-conf; ls /objs; touch /objs/x 2>/dev/null || echo caches-read-only"],
        });
        let ran = call(&addr, "tok-b", "run", read).await.unwrap();
        let id = serde_json::json!({ "build_id": b, "which": "run:1", "tail_lines": 50 });
        let out = call(&addr, "tok-b", "logs", id).await.unwrap();
        let out = out["lines"].as_str().unwrap_or_default();
        assert_eq!(ran["exit"], 0, "{ran} {out}");
        assert!(
            out.contains("/src/a.c")
                && out
                    .lines()
                    .any(|l| l.contains("storage") && l.ends_with("hit")),
            "run sees the build's per-object ccache log under /buildit: {out}"
        );
        assert!(
            out.contains("no-conf"),
            "the generated conf is mounted into RUN steps, not left in the image: {out}"
        );
        assert!(
            out.contains("a.o") && out.contains("caches-read-only"),
            "run sees the build's caches, read-only: {out}"
        );
        let stamped = |prefix: &str| {
            out.lines().any(|l| {
                let l = l.split_once(':').map_or(l, |(_, line)| line);
                l.split_once(' ').is_some_and(|(at, text)| {
                    at.parse::<u128>().is_ok() && text.starts_with(prefix)
                })
            })
        };
        assert!(
            stamped("STEP 1/") && stamped("COMMIT ") && stamped("END 0"),
            "the build's steps are stamped under /buildit: {out}"
        );

        for token in ["tok-a", "tok-b"] {
            call(&addr, token, "clean", serde_json::json!({}))
                .await
                .unwrap();
        }
        on_socket(&socket, Call::Clean(CleanParams { build_id: None })).await;
    }

    /// A cluster cache through the run's socket and sandboxes' tokens: the run's build fills it and
    /// saves it under its key, a sandbox's new pod restores that snapshot and saves nothing, and a
    /// new key starts a new pod from the newest snapshot, which the run's next build saves under
    /// the new key.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: BUILDIT_E2E_KUBECONTEXT=kind-x and either \
                BUILDIT_E2E_HOST_ADDR=<this host's address from a pod> for a cache on this host, or \
                BUILDIT_E2E_CACHE_URL and BUILDIT_E2E_CACHE_TOKEN_FILE for a deployed one"]
    async fn e2e_a_cluster_cache_the_run_saves_starts_a_sandbox_s_new_pod() {
        use crate::client::{Call, run as on_socket};
        use crate::mcp::{CleanParams, RunParams};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("BUILDIT_E2E_KUBECONTEXT").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap();
        let cache_token = work.join("cache-token");
        let run = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let (cache_url, local) = e2e_cache(&work, &cache_token).await;
        std::fs::create_dir_all(work.join("svc")).unwrap();
        let pins = |n: u32| {
            std::fs::write(work.join("svc/pins.txt"), format!("cutlass {run}-{n}\n")).unwrap()
        };
        pins(1);
        std::fs::write(
            work.join("svc/Dockerfile"),
            library("FROM docker.io/library/alpine:3.20\n\
             COPY pins.txt /pins.txt\n\
             ARG NONCE\n\
             RUN if [ -s /deps/pins.txt ]; then \\\n\
               echo \"deps-from-cache $(cat /deps/nonce) $(cat /deps/pins.txt)\"; \\\n\
             else cp /pins.txt /deps/pins.txt && echo \"$NONCE\" > /deps/nonce && echo \"deps-fetched $NONCE\"; fi\n"),
        )
        .unwrap();
        let file = tokens(&work, "tok-a sb-deps-a\ntok-b sb-deps-b\n");
        let client = crate::client_for(Some(&context)).await.unwrap();
        let run_caller = run_sandbox(Some("buildit-deps-e2e")).unwrap();
        let broker = Arc::new(Broker {
            shared_cache: Some(SharedCache {
                url: url::Url::parse(&cache_url).unwrap(),
                token_file: Some(cache_token.clone()),
                writers: CacheWriters::Run,
            }),
            run_caller: Some(run_caller.clone()),
            ..broker_value(client, &file, Some(&work))
        });
        let (addr, _server) = serve(broker.clone()).await;
        let socket = tmp.path().join("b.sock");
        let listener = bind_socket(&socket).unwrap();
        let ours = Caller {
            sandbox: run_caller,
            workspace: Workspace::Local { dir: work.clone() },
            launched_by: None,
        };
        let app = socket_router(broker, ours, ToolPolicy::from_toml(POLICY).unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for token in ["tok-a", "tok-b"] {
            call(&addr, token, "clean", serde_json::json!({}))
                .await
                .unwrap();
        }
        on_socket(&socket, Call::Clean(CleanParams { build_id: None })).await;
        let grep = "^(deps-|cache deps|CACHE )";
        let run_build = |nonce: String| {
            let socket = socket.clone();
            async move {
                let built = on_socket(
                    &socket,
                    Call::Build {
                        params: BuildParams {
                            context: "svc".to_string(),
                            dockerfile: None,
                            target: None,
                            build_args: BTreeMap::from([("NONCE".to_string(), nonce)]),
                            timeout_s: Some(600),
                            caches: vec![CacheSpec {
                                name: "deps".to_string(),
                                path: "/deps".to_string(),
                                read_only: false,
                                scope: CacheScope::Cluster,
                                key: vec!["pins.txt".to_string()],
                            }],
                            ccache: false,
                            base: None,
                            script: None,
                            path: None,
                        },
                        wait: true,
                    },
                )
                .await;
                assert_eq!(built.reply["status"], "succeeded", "{}", built.reply);
                let id = built.reply["build_id"].as_str().unwrap().to_string();
                let log = on_socket(
                    &socket,
                    Call::Logs(LogsParams {
                        build_id: id.clone(),
                        which: None,
                        grep: Some(grep.to_string()),
                        context: None,
                        from_line: None,
                        to_line: None,
                        tail_lines: None,
                        max_bytes: None,
                    }),
                )
                .await;
                (
                    id,
                    log.reply["lines"].as_str().unwrap_or_default().to_string(),
                )
            }
        };
        let sandbox_build = |token: &'static str, nonce: String| {
            let addr = addr.clone();
            async move {
                let args = serde_json::json!({
                    "context": "svc", "timeout_s": 600,
                    "caches": [{ "name": "deps", "path": "/deps", "scope": "cluster", "key": ["pins.txt"] }],
                    "build_args": { "NONCE": nonce },
                });
                let started = call(&addr, token, "build", args).await.unwrap();
                let id = started["build_id"].as_str().unwrap().to_string();
                let args = serde_json::json!({ "build_id": id });
                for _ in 0..300 {
                    let status = call(&addr, token, "status", args.clone()).await.unwrap();
                    if status["status"] != "running" {
                        assert_eq!(status["status"], "succeeded", "{status}");
                        let logs = serde_json::json!({ "build_id": id, "grep": grep });
                        let lines = call(&addr, token, "logs", logs).await.unwrap();
                        return lines["lines"].as_str().unwrap_or_default().to_string();
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                panic!("build {id} still running after 600s");
            }
        };

        if !local {
            let newest = format!("{cache_url}/dirs/deps/newest");
            let token = std::fs::read_to_string(&cache_token).unwrap();
            e2e_http(Some(&SharedCache {
                url: url::Url::parse(&cache_url).unwrap(),
                token_file: None,
                writers: CacheWriters::All,
            }))
            .delete(&newest)
            .bearer_auth(token.trim())
            .send()
            .await
            .unwrap();
        }
        let (first, log) = run_build(format!("{run}-seed")).await;
        assert!(
            log.contains("cache deps: no snapshot")
                && log.contains(&format!("deps-fetched {run}-seed"))
                && log.contains("cache deps: saved "),
            "the run's own build fills the cache and saves it: {log}"
        );
        let log = sandbox_build("tok-b", format!("{run}-b")).await;
        assert!(
            log.contains("cache deps: restored ")
                && !log.contains("restored the newest")
                && log.contains(&format!("deps-from-cache {run}-seed cutlass {run}-1")),
            "a sandbox's new pod starts from the snapshot under its key: {log}"
        );
        assert!(
            !log.contains("CACHE save"),
            "a sandbox's build saves nothing: {log}"
        );

        pins(2);
        let log = sandbox_build("tok-a", format!("{run}-a")).await;
        assert!(
            log.contains("cache deps: restored the newest snapshot")
                && log.contains(&format!("deps-from-cache {run}-seed cutlass {run}-1")),
            "a new key starts from the newest snapshot: {log}"
        );
        let (_, log) = run_build(format!("{run}-seed-2")).await;
        assert!(
            log.contains("cache deps: restored") && log.contains("cache deps: saved "),
            "the run's next build restores the cache and saves it under the new key: {log}"
        );
        let (_, log) = run_build(format!("{run}-seed-3")).await;
        assert!(
            log.contains("is saved already"),
            "a key is saved once: {log}"
        );

        let steps = on_socket(
            &socket,
            Call::Run(RunParams {
                build_id: first,
                cmd: vec!["cat".to_string(), "/buildit/steps.log".to_string()],
                timeout_s: Some(60),
                fetch_paths: Vec::new(),
            }),
        )
        .await;
        let stamped = steps.reply["log_tail"].as_str().unwrap_or_default();
        assert!(
            [
                "CACHE restore deps",
                "STEP 1/",
                "COMMIT ",
                "CACHE save deps",
                "END 0"
            ]
            .iter()
            .all(|line| stamped.contains(line)),
            "the restore and the save are stamped as steps: {}",
            steps.reply
        );

        for token in ["tok-a", "tok-b"] {
            call(&addr, token, "clean", serde_json::json!({}))
                .await
                .unwrap();
        }
        on_socket(&socket, Call::Clean(CleanParams { build_id: None })).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: BUILDIT_E2E_KUBECONTEXT=kind-x and either \
                BUILDIT_E2E_HOST_ADDR=<this host's address from a pod> for a cache on this host, or \
                BUILDIT_E2E_CACHE_URL and BUILDIT_E2E_CACHE_TOKEN_FILE for a deployed one"]
    async fn e2e_builds_of_different_sandboxes_share_ccache_objects_through_the_cache() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("BUILDIT_E2E_KUBECONTEXT").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap();
        let cache_token = work.join("cache-token");
        let run = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let (cache_url, local) = e2e_cache(&work, &cache_token).await;
        let blobs = || {
            let mut dirs = vec![work.join("cache/blobs")];
            let mut found = Vec::new();
            while let Some(d) = dirs.pop() {
                for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    match e.path().is_dir() {
                        true => dirs.push(e.path()),
                        false => found.push(e.path()),
                    }
                }
            }
            found
        };

        let bench =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/vllm-build-bench/bench");
        std::fs::create_dir_all(work.join("svc")).unwrap();
        for module in ["informer.py", "buildtrace.py"] {
            std::fs::copy(bench.join(module), work.join("svc").join(module)).unwrap();
        }
        std::fs::write(
            work.join("svc/a.c"),
            format!(
                "int shared(int x) {{ return x * 13 + {}; }}\n",
                run % 1_000_000_007
            ),
        )
        .unwrap();
        std::fs::write(
            work.join("svc/Dockerfile"),
            library("FROM docker.io/library/alpine:3.20\n\
             RUN apk add --no-cache ccache gcc musl-dev python3\n\
             COPY informer.py buildtrace.py a.c /src/\n\
             ARG NONCE\n\
             RUN set -e; \\\n\
               remote=$(python3 -c 'import pathlib, sys; sys.path.insert(0, \"/src\"); import informer; \\\n\
                 print(informer.Remote.load(pathlib.Path(\"/buildit/cache.json\")).ccache_storage())'); \\\n\
               export CCACHE_DIR=/tmp/ccache CCACHE_REMOTE_STORAGE=\"$remote\" CCACHE_SECONDARY_STORAGE=\"$remote\"; \\\n\
               ccache -z > /dev/null; ccache gcc -c /src/a.c -o /tmp/a.o; \\\n\
               stats=$(ccache --print-stats); \\\n\
               hits=$(echo \"$stats\" | awk '$1 == \"remote_storage_hit\" || $1 == \"secondary_storage_hit\" { n += $2 } END { print n + 0 }'); \\\n\
               misses=$(echo \"$stats\" | awk '$1 == \"cache_miss\" { n += $2 } END { print n + 0 }'); \\\n\
               echo \"shared-ccache nonce=$NONCE remote_hits=$hits misses=$misses\"\n"),
        )
        .unwrap();
        let file = tokens(&work, "tok-a sb-cache-a\ntok-b sb-cache-b\n");
        let client = crate::client_for(Some(&context)).await.unwrap();
        let writing = |writers| Broker {
            shared_cache: Some(SharedCache {
                url: url::Url::parse(&cache_url).unwrap(),
                token_file: Some(cache_token.clone()),
                writers,
            }),
            ..broker_value(client.clone(), &file, Some(&work))
        };
        let (addr, _server) = serve(Arc::new(writing(CacheWriters::All))).await;
        let (reader, _reader) = serve(Arc::new(writing(CacheWriters::None))).await;
        for token in ["tok-a", "tok-b"] {
            call(&addr, token, "clean", serde_json::json!({}))
                .await
                .unwrap();
        }
        let build = |addr: &String, token: &'static str, nonce: String| {
            let addr = addr.clone();
            async move {
                let args = serde_json::json!({
                    "context": "svc", "timeout_s": 600, "build_args": { "NONCE": nonce },
                });
                let started = call(&addr, token, "build", args).await.unwrap();
                let id = started["build_id"].as_str().unwrap().to_string();
                let args = serde_json::json!({ "build_id": id });
                for _ in 0..300 {
                    let status = call(&addr, token, "status", args.clone()).await.unwrap();
                    if status["status"] != "running" {
                        assert_eq!(status["status"], "succeeded", "{status}");
                        let grep = serde_json::json!({ "build_id": id, "grep": "^shared-ccache " });
                        let lines = call(&addr, token, "logs", grep).await.unwrap();
                        return (
                            id,
                            lines["lines"]
                                .as_str()
                                .unwrap_or_default()
                                .trim_end()
                                .to_string(),
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                panic!("build {id} still running after 600s");
            }
        };

        let (a, line) = build(&addr, "tok-a", format!("{run}-a")).await;
        assert!(
            line.ends_with(&format!(
                "shared-ccache nonce={run}-a remote_hits=0 misses=1"
            )),
            "{line}"
        );
        let stored = blobs();
        assert!(
            !local || !stored.is_empty(),
            "sandbox a's build wrote its objects to the cache"
        );

        let (b, line) = build(&reader, "tok-b", format!("{run}-b")).await;
        assert!(
            !b.starts_with(&a) && !a.starts_with(&b),
            "each sandbox builds in a pod of its own: {a} {b}"
        );
        assert!(
            line.ends_with(&format!(
                "shared-ccache nonce={run}-b remote_hits=1 misses=0"
            )),
            "another sandbox's build, in its own pod, hits a's object: {line}"
        );
        assert_eq!(blobs(), stored, "a read-only build stores nothing");

        for token in ["tok-a", "tok-b"] {
            call(&addr, token, "clean", serde_json::json!({}))
                .await
                .unwrap();
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
            library(
                "FROM docker.io/library/busybox:latest\nRUN echo deadline-build && sleep 600\n",
            ),
        )
        .unwrap();
        let file = tokens(&work, "tok-a sb-a\n");
        let client = crate::client_for(Some(&context)).await.unwrap();
        let deadline = 120;
        let mut broker = broker_value(client.clone(), &file, Some(&work));
        broker.site.deadline_secs = deadline;
        let (addr, _task) = serve(Arc::new(broker)).await;
        let a = |tool, args| call(&addr, "tok-a", tool, args);

        let built = a("build", serde_json::json!({ "context": "svc" }))
            .await
            .unwrap();
        let id = built["build_id"].as_str().unwrap().to_string();
        let only_id = serde_json::json!({ "build_id": id });

        let mut status = serde_json::Value::Null;
        for _ in 0..deadline * 2 {
            status = a("status", only_id.clone()).await.unwrap();
            if status["status"] != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(status["status"], "failed", "{status}");
        assert_eq!(
            status["timed_out"], true,
            "the Job's deadline ended it: {status}"
        );
        assert_eq!(status["exit"], serde_json::Value::Null, "{status}");
        let run = serde_json::json!({ "build_id": id, "cmd": ["true"] });
        let e = a("run", run).await.unwrap_err();
        assert!(e.contains("failed"), "{e}");

        let cleaned = a("clean", only_id.clone()).await.unwrap();
        assert_eq!(cleaned["deleted"], serde_json::json!([id]));
    }

    /// Waits for a build's or run's Job to end, as a cancel ends it.
    async fn wait_job_ended(client: &kube::Client, name: &str) {
        let jobs: kube::Api<k8s_openapi::api::batch::v1::Job> =
            kube::Api::namespaced(client.clone(), client.default_namespace());
        let start = std::time::Instant::now();
        loop {
            match jobs.get_opt(name).await.unwrap() {
                Some(job) if !crate::mcp::job_ended(&job) => {}
                _ => return,
            }
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "job {name} did not end"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

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

        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("BUILDIT_E2E_KUBECONTEXT").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(work.join("svc")).unwrap();
        std::fs::write(
            work.join("svc/Dockerfile"),
            library(
                "FROM docker.io/library/busybox:latest AS base\n\
             RUN echo hello-from-task && mkdir -p /out && echo artifact > /out/a.txt\n\
             FROM base AS slow\n\
             RUN echo slow-build && sleep 600\n",
            ),
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

        let start = std::time::Instant::now();
        loop {
            let args =
                serde_json::json!({ "build_id": id, "which": "run:1", "grep": "^slow-start$" });
            let seen = call(&addr, "tok-a", "logs", args).await.unwrap();
            if seen["matched"] == 1 {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(120), "{seen}");
            tokio::time::sleep(Duration::from_secs(1)).await;
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
        wait_job_ended(&client, &id).await;

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
        wait_job_ended(&client, &slow_id).await;

        let cleaned = call(&addr, "tok-a", "clean", serde_json::json!({}))
            .await
            .unwrap();
        let mut both = vec![
            id.clone(),
            format!("{id}-run-1"),
            format!("{id}-run-2"),
            format!("{id}-run-3"),
            slow_id.clone(),
        ];
        both.sort();
        assert_eq!(cleaned["deleted"], serde_json::json!(both));
        assert_eq!(
            get_task(&a, &build).await.unwrap().status(),
            TaskStatus::Completed,
            "a cleaned build's record stays"
        );
        cancel_task(&a, &fetch).await.unwrap();
        assert_eq!(
            get_task(&a, &fetch).await.unwrap().status(),
            TaskStatus::Completed,
            "cancelling a cleaned run that ended changes nothing"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: BUILDIT_E2E_KUBECONTEXT=kind-x cargo test -- --ignored"]
    async fn e2e_the_runs_socket_builds_runs_and_reports_the_context_it_built() {
        use crate::client::{Call, run};
        use crate::mcp::{CleanParams, RunParams};
        use std::os::unix::fs::PermissionsExt;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("BUILDIT_E2E_KUBECONTEXT").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap().join("ws");
        let svc = work.join("svc");
        std::fs::create_dir_all(&svc).unwrap();
        std::fs::write(
            svc.join("Dockerfile"),
            library("FROM docker.io/library/busybox:latest\nCOPY run.sh /run.sh\nRUN echo socket-build\n"),
        )
        .unwrap();
        std::fs::write(svc.join("run.sh"), "#!/bin/sh\necho ran-from-context\n").unwrap();
        std::fs::set_permissions(svc.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let client = crate::client_for(Some(&context)).await.unwrap();
        let caller = Caller {
            sandbox: run_sandbox(Some("buildit-socket-e2e")).unwrap(),
            workspace: Workspace::Local { dir: work.clone() },
            launched_by: None,
        };
        let socket = tmp.path().join("buildit.sock");
        let listener = bind_socket(&socket).unwrap();
        let app = socket_router(
            broker(client, &tokens(tmp.path(), ""), None),
            caller,
            ToolPolicy::from_toml(POLICY).unwrap(),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let build = |wait| Call::Build {
            params: BuildParams {
                context: svc.to_str().unwrap().to_string(),
                dockerfile: None,
                target: None,
                build_args: BTreeMap::new(),
                timeout_s: None,
                caches: Vec::new(),
                ccache: false,
                base: None,
                script: None,
                path: None,
            },
            wait,
        };
        let expected = crate::context::digest_dir(&svc).unwrap();

        let built = run(&socket, build(true)).await;
        assert!(built.ok, "{}", built.reply);
        assert_eq!(built.reply["status"], "succeeded", "{}", built.reply);
        assert_eq!(built.reply["context_digest"], expected.as_str());
        assert!(
            built.reply["log_tail"]
                .as_str()
                .unwrap()
                .contains("socket-build")
        );
        let id = built.reply["build_id"].as_str().unwrap().to_string();

        let exec = |cmd: &[&str]| {
            Call::Run(RunParams {
                build_id: id.clone(),
                cmd: cmd.iter().map(|s| s.to_string()).collect(),
                timeout_s: None,
                fetch_paths: Vec::new(),
            })
        };
        let ran = run(&socket, exec(&["/run.sh"])).await;
        assert!(ran.ok, "{}", ran.reply);
        assert!(
            ran.reply["log_tail"]
                .as_str()
                .unwrap()
                .contains("ran-from-context")
        );
        let failed = run(&socket, exec(&["sh", "-c", "exit 3"])).await;
        assert!(!failed.ok, "{}", failed.reply);
        assert_eq!(failed.reply["exit"], 3);

        let pushed = run(
            &socket,
            Call::Push(PushParams {
                build_id: id.clone(),
            }),
        )
        .await;
        assert!(!pushed.ok);
        assert!(
            pushed.reply["error"]
                .as_str()
                .unwrap()
                .contains("publishes no builds")
        );

        let started = run(&socket, build(false)).await;
        assert!(started.ok, "{}", started.reply);
        assert_eq!(started.reply["status"], "running");
        assert_eq!(started.reply["context_digest"], expected.as_str());
        let second = started.reply["build_id"].as_str().unwrap().to_string();

        let cleaned = run(&socket, Call::Clean(CleanParams { build_id: None })).await;
        assert!(cleaned.ok, "{}", cleaned.reply);
        let mut both = vec![format!("{id}-run-1"), format!("{id}-run-2"), id, second];
        both.sort();
        assert_eq!(
            cleaned.reply["deleted"],
            serde_json::json!(both),
            "both builds and the first's runs"
        );
    }
}
